//! The OFFLINE AUDIO STORE: an on-disk mirror of starred songs and the upcoming
//! queue window, so playback works when the server is slow, flaky, or gone.
//!
//! This module is the deterministic, filesystem-only half of the feature. It holds
//! the validity model, the on-disk layout, the sidecar round trip, the startup
//! scan-and-heal, the sync play-time probe, and [`plan_pass`] - a PURE function
//! from (pin set, queue window, directory scan) to a list of [`StoreAction`]s. The
//! reconciler task that executes those actions and does the actual downloading
//! lives on top of this; nothing here touches the network.
//!
//! ## The validity model
//!
//! Only `/rest/download` ORIGINALS are ever stored, which deletes the entire
//! transcoding-settings validity class by construction: server-side transcoding
//! changes what `/rest/stream` returns, never what `download` returns. The sidecar
//! records `endpoint = "download"` so a future format-aware mode cannot silently
//! mix provenances.
//!
//! A cached entry for song id X is VALID iff:
//!
//! 1. `<store>/<X>.toml` exists, parses, and carries the current
//!    [`STORE_SCHEMA_VERSION`] with `endpoint = "download"`.
//! 2. `<store>/<X>.<suffix>` exists and its byte length equals
//!    `sidecar.fingerprint.size` EXACTLY.
//! 3. X is not marked suspect (an in-memory flag; see [`AudioStore::mark_suspect`]).
//! 4. Background only: the sidecar's identity fingerprint `(size, suffix, created)`
//!    still matches what the server currently reports.
//!
//! ## The commit point
//!
//! The SIDECAR RENAME. Audio bytes are fully written to a temp file,
//! length-verified against the server-reported size, fsynced, and renamed into
//! place BEFORE the sidecar ever appears - so a valid-looking truncation is
//! structurally impossible. An audio file with no sidecar is an ORPHAN the next
//! scan deletes; it is never offered to playback. See [`AudioStore::commit`].
//!
//! ## The directory is owned exclusively
//!
//! Convergence is by DELETION: whatever the scan cannot account for as a valid pair
//! is removed. That is only safe in a directory that is the store's, so ownership is
//! proven before anything is healed - [`AudioStore::open`] adopts an EMPTY directory
//! (dropping the [`STORE_MARKER_NAME`] claim into it) or one already marked, and
//! REFUSES any other without touching a file. A `store.dir` pointed at the state dir
//! or a music folder therefore costs a warn and a store-less run, never the files.
//!
//! ## Keep-until-replaced
//!
//! Nothing is ever deleted because it MIGHT be wrong. A drifted fingerprint marks
//! the sidecar stale and the entry KEEPS SERVING until a verified replacement is
//! renamed over it. A song that vanishes from the pin set is DEMOTED to evictable,
//! not deleted. A suspect entry is de-offered immediately but its bytes are removed
//! only by the rename of a secured replacement. An offline pass can therefore never
//! destroy what it cannot replace, which is the whole point of an offline store.
//!
//! ## The honest gap (tail rot), and its manual repair
//!
//! A locally cached file with a VALID HEADER and a ROTTED TAIL most plausibly ends
//! with mpv reason EOF, not Error. This module does NOT detect that: the fingerprint
//! still matches, the stat still matches, and the entry re-confirms valid on every
//! pass forever. The gap is deliberate - the alternative (an early-EOF heuristic
//! comparing observed position against `duration_secs`) produces false suspects on
//! VBR duration disagreement, each costing a full original re-download.
//!
//! THE REPAIR VERB IS MANUAL and falls straight out of the scan-heal design:
//!
//! ```text
//! rm <store>/<song-id>.*
//! ```
//!
//! Deleting the pair by hand makes the entry disappear; the next reconcile pass
//! re-downloads it if it is still wanted. No daemon restart, no config, no state to
//! clear - the scan converges on whatever the directory says.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::future::Future;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::clock::Clock;
use crate::config::StoreConfig;
use crate::model::{Album, AlbumId, ArtistId, Song, SongId};
use crate::resume::atomic_write_bytes;
use crate::subsonic::{Starred, SubsonicClient, SubsonicError};

/// On-disk sidecar schema version. A sidecar whose `schema_version` differs is
/// treated as CORRUPT (see [`sidecar_from_toml`]), which invalidates exactly one
/// song - the reason per-song sidecars beat a single manifest.
pub const STORE_SCHEMA_VERSION: u32 = 1;

/// The only endpoint whose bytes are ever stored. Recorded in every sidecar so a
/// future format-aware mode cannot silently serve transcoded bytes as originals.
pub const ENDPOINT_DOWNLOAD: &str = "download";

/// Fallback suffix for an audio file whose server-reported suffix is missing or
/// unusable. See [`sanitize_suffix`].
pub const FALLBACK_SUFFIX: &str = "bin";

/// The OWNERSHIP MARKER: the file whose presence says "this directory is the
/// store's to converge, and anything in it that is not a valid pair is garbage".
///
/// It exists because the scan-heal is DESTRUCTIVE by design - that is what makes
/// crash recovery, external tampering, and a cold start one code path - and
/// `store.dir` is a free-form config key. Without a marker, aiming it at the state
/// dir or a music folder would delete `resume.toml` or a library on the next start,
/// silently and before anything could object. With one, [`AudioStore::open`] adopts
/// only a directory that is EMPTY (fresh) or already marked, and refuses everything
/// else without removing a single byte.
///
/// Dot-prefixed so it can never collide with an `<id>.<suffix>` or `<id>.toml`
/// name: [`is_storable_id`] rejects the empty id, so no entry can ever be named
/// `.hypodj-store`, and [`scan_dir`] excludes it from classification so the heal
/// cannot delete the very claim it depends on.
pub const STORE_MARKER_NAME: &str = ".hypodj-store";

/// Body of the ownership marker. Human-facing only - nothing parses it; the file's
/// EXISTENCE is the whole claim. It says what the directory is so a person who
/// finds it knows why files there disappear.
const STORE_MARKER_BODY: &str = "\
hypodj offline audio store (schema 1).

This directory is owned by the hypodj daemon: it is scanned and healed on every
start, and any file in it that is not part of a valid <song-id>.toml +
<song-id>.<suffix> pair is DELETED. Do not keep anything else here.

Deleting this marker makes hypodj refuse the directory rather than converge it.
";

/// Max length of a sanitized suffix. Long enough for every real container
/// (`flac`, `opus`, `m4a`, `ogg`, `mp3`, `wav`, `aiff`, `webm`) and short enough
/// that a hostile value cannot bloat a path.
pub const MAX_SUFFIX_LEN: usize = 8;

/// How many downloads ONE pass may schedule. The reconciler re-enters immediately
/// while work remains, so a large backlog (a fresh mirror, a full re-import) drains
/// incrementally instead of pinning the task or saturating the link in one burst.
pub const DOWNLOAD_BATCH: usize = 4;

/// How long a download may go WITHOUT RECEIVING A CHUNK before it is abandoned.
/// Deliberately a per-chunk inactivity budget, not a total-request timeout: a
/// 60 MB FLAC on a thin link is slow but healthy, whereas a stalled socket is
/// dead - a total timeout cannot tell those apart and would kill exactly the
/// downloads that matter most.
const DOWNLOAD_CHUNK_TIMEOUT: Duration = Duration::from_secs(30);

/// Connect timeout for the store's own audio HTTP client. Matches the metadata
/// client's, so a blackholed host costs seconds everywhere rather than minutes of
/// kernel SYN retries.
const DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Slack over the server-reported size that the running cap allows before it
/// abandons the download. Nonzero so a server that appends a trailing byte fails
/// on the exact-length gate (an honest error) rather than on a cap that fires
/// mid-stream; tiny, so a wrong `size` can never let an unbounded body fill the
/// disk.
const DOWNLOAD_SIZE_SLACK: u64 = 64 * 1024;

/// First retry delay for an id whose download failed, doubled per consecutive
/// failure up to [`DOWNLOAD_BACKOFF_MAX`]. In memory only: the pass cadence IS the
/// retry schedule, and a restart is allowed to try again immediately.
const DOWNLOAD_BACKOFF_BASE: Duration = Duration::from_secs(30);

/// Ceiling on the per-id download backoff.
const DOWNLOAD_BACKOFF_MAX: Duration = Duration::from_secs(3600);

/// Consecutive failures after which an id is GIVEN UP on for this process.
///
/// The exponential backoff bounds the RATE of a retry; it does not bound the TOTAL,
/// so a permanently-invalid download still costs ~24 requests a day forever once it
/// reaches [`DOWNLOAD_BACKOFF_MAX`]. Observed live: a starred song whose Navidrome
/// metadata declares 3 MiB while `/rest/download` serves 29.2 MiB can NEVER satisfy
/// the exact-length check, so waiting longer changes nothing - the condition is a
/// disagreement between the server and itself, not a transient.
///
/// Eight is past any plausible transient (with the doubling that is roughly two and a
/// half hours of trying) and short of a number that would keep a genuinely flaky
/// network out of the mirror. Giving up is per-PROCESS and deliberately not
/// persisted: a restart is exactly when the server may have been fixed or the file
/// rescanned, so it earns a fresh attempt.
const DOWNLOAD_GIVE_UP_AFTER: u32 = 8;

/// Process-wide sequence for in-flight temp names, so two writers in one process
/// can never collide on a temp path.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Wall-clock epoch seconds, for the sidecar's `fetched_at` / `last_played`
/// bookkeeping.
///
/// Deliberately NOT routed through [`crate::clock::Clock`]: that seam exists for
/// SCHEDULING (a monotonic `Instant` plus absolute deadlines, so time-dependent
/// logic is fake-clockable), and it cannot express a persisted calendar
/// timestamp. Nothing branches on this value except LRU ordering, so a clock step
/// costs at worst one odd eviction order, never a wrong decision.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─────────────────────────────────────────────────────────────────────────────
// Keys, names, and path safety
// ─────────────────────────────────────────────────────────────────────────────

/// Whether `id` may be used as a store key at all.
///
/// Only `[A-Za-z0-9_-]+` is storable. This is not cosmetic: the id becomes a path
/// component and, downstream, part of an mpv `loadfile` argument, where the
/// literal-double-quote trap in the player's `quote()` helper lives. Excluding
/// everything else - dots, slashes, quotes, spaces, empty - makes a path escape
/// and a quoting escape STRUCTURALLY impossible rather than defended against. An
/// id that fails this is simply never stored; resolution falls through to
/// streaming, which is the same audible outcome as a cache miss.
pub fn is_storable_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Sanitize a server-reported file suffix into something safe to put in a path.
///
/// Lowercased, then accepted only as 1 to [`MAX_SUFFIX_LEN`] ASCII alphanumerics;
/// anything else becomes [`FALLBACK_SUFFIX`]. `toml` is REJECTED even though it
/// matches the character class - `<id>.toml` is the sidecar, so a song whose
/// suffix were literally `toml` would have its audio file collide with its own
/// commit record.
pub fn sanitize_suffix(suffix: Option<&str>) -> String {
    let Some(raw) = suffix else {
        return FALLBACK_SUFFIX.to_string();
    };
    let lower = raw.trim().to_ascii_lowercase();
    let ok = !lower.is_empty()
        && lower.len() <= MAX_SUFFIX_LEN
        && lower.bytes().all(|b| b.is_ascii_alphanumeric())
        && lower != "toml";
    if ok {
        lower
    } else {
        FALLBACK_SUFFIX.to_string()
    }
}

/// Whether `name` is one of our in-flight temp files (`tmp.<pid>.<seq>`).
///
/// Both trailing segments must be all-digits, which is what keeps a real song
/// whose id happens to be `tmp` (giving `tmp.flac`) from being swept as garbage.
fn is_tmp_name(name: &str) -> bool {
    match name.strip_prefix("tmp.").and_then(|r| r.split_once('.')) {
        Some((pid, seq)) => {
            !pid.is_empty()
                && !seq.is_empty()
                && pid.bytes().all(|b| b.is_ascii_digit())
                && seq.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The created-timestamp helper
// ─────────────────────────────────────────────────────────────────────────────

/// Parse an RFC 3339 / ISO-8601 timestamp into epoch seconds. `None` when the
/// shape is not recognized, for ANY input, including arbitrary UTF-8 - it never
/// panics.
///
/// Small and in-repo on purpose (zero new dependencies). It exists so the
/// fingerprint's `created` leg is compared as an INSTANT, not as a raw string: the
/// same moment rendered `2024-05-01T12:00:00Z` and `2024-05-01T14:00:00+02:00` is
/// equal, so a server that re-renders offsets after an upgrade cannot mass-
/// invalidate the whole mirror in one pass.
///
/// Accepts `YYYY-MM-DDTHH:MM:SS`, an optional fractional part (dropped - sub-second
/// precision is noise for a re-import verdict), and an optional `Z` or `+HH:MM` /
/// `-HHMM` offset. A naive timestamp with no offset is read as UTC.
pub fn parse_rfc3339_epoch(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |lo: usize, hi: usize| -> Option<i64> {
        let part = s.get(lo..hi)?;
        if part.bytes().all(|c| c.is_ascii_digit()) {
            part.parse::<i64>().ok()
        } else {
            None
        }
    };
    if b[4] != b'-' || b[7] != b'-' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    if !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    let hour = num(11, 13)?;
    let minute = num(14, 16)?;
    // 60 is permitted: a leap second is a real value some servers emit.
    let second = num(17, 19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    let mut rest = &s[19..];
    // Optional fractional seconds, either separator, then digits.
    if let Some(stripped) = rest.strip_prefix('.').or_else(|| rest.strip_prefix(',')) {
        let digits = stripped
            .bytes()
            .take_while(|c| c.is_ascii_digit())
            .count();
        if digits == 0 {
            return None;
        }
        rest = &stripped[digits..];
    }

    let offset_secs: i64 = if rest.is_empty() {
        0
    } else if rest.eq_ignore_ascii_case("z") {
        0
    } else {
        let (sign, body) = match rest.as_bytes()[0] {
            b'+' => (1i64, &rest[1..]),
            b'-' => (-1i64, &rest[1..]),
            _ => return None,
        };
        // The arms below slice the body by BYTE index, so a non-ASCII body is
        // rejected up front rather than sliced inside a code point. `created` is
        // server data judged inside the reconciler task that owns every store
        // mutation: one odd timestamp must cost a `None` verdict, never the task.
        if !body.is_ascii() {
            return None;
        }
        let (oh, om) = match body.len() {
            5 if body.as_bytes()[2] == b':' => (&body[0..2], &body[3..5]),
            4 => (&body[0..2], &body[2..4]),
            2 => (&body[0..2], "0"),
            _ => return None,
        };
        let oh: i64 = oh.parse().ok().filter(|h| *h <= 23)?;
        let om: i64 = om.parse().ok().filter(|m| *m <= 59)?;
        sign * (oh * 3600 + om * 60)
    };

    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second - offset_secs)
}

/// Days since 1970-01-01 for a proleptic-Gregorian date (Howard Hinnant's
/// `days_from_civil`). Exact for every year in `i64` range, no lookup tables.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // Mar = 0 .. Feb = 11
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Whether two `created` timestamps denote the same moment.
///
/// Compared as PARSED INSTANTS when both parse, so a timezone re-rendering is not
/// drift. When either side is unparseable the comparison falls back to exact string
/// equality - unknown-but-identical is still "same", and unknown-and-different is
/// honestly reported as different rather than silently confirmed.
pub fn created_matches(a: &str, b: &str) -> bool {
    match (parse_rfc3339_epoch(a), parse_rfc3339_epoch(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The sidecar
// ─────────────────────────────────────────────────────────────────────────────

/// The identity of the bytes on disk, captured from the server's `Child` at
/// download time. Not a content hash: hashing every original on every pass costs
/// gigabytes of reads to detect a case (silent same-size bit rot) the suspect path
/// already covers audibly.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    /// Exact byte length of the original. The commit gate AND the play-time stat
    /// check, so a truncated file can never look valid.
    pub size: u64,
    /// Sanitized suffix - also the audio file's extension on disk.
    pub suffix: String,
    /// The server's `created` timestamp, when it reported one. `None` simply drops
    /// this leg of the verdict rather than forcing a re-download.
    #[serde(default)]
    pub created: Option<String>,
}

/// The per-song commit record. Its atomic appearance IS the commit; see
/// [`AudioStore::commit`].
///
/// Field order matters: TOML requires plain values before tables, so every scalar
/// is declared ahead of `fingerprint` and `song`. The bookkeeping scalars carry
/// `#[serde(default)]` so a sidecar written by an older build - or one whose
/// serializer omitted a `None` - still loads without a schema bump.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Sidecar {
    /// Version gate; a mismatch means CORRUPT (see [`sidecar_from_toml`]).
    pub schema_version: u32,
    /// Always [`ENDPOINT_DOWNLOAD`]. Anything else means CORRUPT: bytes of unknown
    /// provenance must never be served as originals.
    pub endpoint: String,
    /// MIME type of the stored bytes, for provenance. Never used to build a path.
    #[serde(default)]
    pub content_type: Option<String>,
    /// Whether this entry is in the authoritative pin set (starred). Pins are never
    /// silently evicted.
    #[serde(default)]
    pub pinned: bool,
    /// Set when a background fingerprint verdict found drift. A stale entry KEEPS
    /// SERVING until a verified replacement is renamed over it.
    #[serde(default)]
    pub stale: bool,
    /// When these bytes were committed, epoch seconds.
    #[serde(default)]
    pub fetched_at_unix: u64,
    /// Last time playback resolved to these bytes, epoch seconds. The LRU key for
    /// eviction; bumped in memory at resolve time and flushed here by a full pass.
    #[serde(default)]
    pub last_played_unix: u64,
    pub fingerprint: Fingerprint,
    /// The WHOLE song. This embedded copy is what lets an offline restore and an
    /// offline `add song/<id>` carry real metadata instead of a bare id.
    pub song: Song,
}

/// Serialize a sidecar to TOML. A serializer error degrades to the empty string,
/// which [`sidecar_from_toml`] then reads back as `None` (a corrupt entry the scan
/// heals) rather than propagating - the [`crate::resume::to_toml`] posture.
pub fn sidecar_to_toml(s: &Sidecar) -> String {
    toml::to_string(s).unwrap_or_default()
}

/// Parse a sidecar. ANY of {parse failure, garbage, truncation, missing required
/// field, wrong [`STORE_SCHEMA_VERSION`], endpoint other than
/// [`ENDPOINT_DOWNLOAD`], unstorable id, zero-size fingerprint} yields `None`.
/// NEVER panics.
///
/// This is the corruption bar from [`crate::resume::from_toml`], applied per song:
/// a `None` here invalidates exactly one entry, whose two files the next pass
/// deletes and re-downloads if still wanted.
pub fn sidecar_from_toml(raw: &str) -> Option<Sidecar> {
    let s: Sidecar = toml::from_str(raw).ok()?;
    if s.schema_version != STORE_SCHEMA_VERSION {
        return None;
    }
    if s.endpoint != ENDPOINT_DOWNLOAD {
        return None;
    }
    if !is_storable_id(&s.song.id.0) {
        return None;
    }
    // A zero-size fingerprint would make an EMPTY file pass the length check and
    // be served as valid audio.
    if s.fingerprint.size == 0 {
        return None;
    }
    // Re-sanitize on read: a hand-edited `suffix = "../../etc/passwd"` must not
    // reach a path. Sanitizing is idempotent for anything we wrote ourselves, so
    // a mismatch here means the sidecar was tampered with and the entry is dropped
    // by the audio-file check (the sanitized name will not exist).
    let sane = sanitize_suffix(Some(&s.fingerprint.suffix));
    if sane != s.fingerprint.suffix {
        return None;
    }
    Some(s)
}

// ─────────────────────────────────────────────────────────────────────────────
// The in-memory index
// ─────────────────────────────────────────────────────────────────────────────

/// One entry as the running daemon knows it: the sidecar's committed truth plus
/// the two in-memory-only flags (`suspect`, `recency_dirty`).
///
/// `suspect` is deliberately NOT persisted. A restart therefore re-offers a
/// previously suspect entry - which is right: the most common cause of an errored
/// end-of-play is an audio-output hiccup on suspend, not bad bytes, and if the
/// bytes really are bad the very next play re-marks it at the cost of one stream
/// fallback. Persisting it would turn a transient environment failure into a
/// permanent de-offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub id: SongId,
    /// Sanitized suffix; with the id this names the audio file.
    pub suffix: String,
    /// Exact expected byte length.
    pub size: u64,
    /// The server `created` leg of the fingerprint, when known.
    pub created: Option<String>,
    pub pinned: bool,
    pub stale: bool,
    /// In-memory only: de-offered by [`AudioStore::lookup`], bytes kept until a
    /// verified replacement exists.
    pub suspect: bool,
    pub last_played_unix: u64,
    /// In-memory only: `last_played_unix` moved since the last sidecar flush.
    pub recency_dirty: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// The directory scan
// ─────────────────────────────────────────────────────────────────────────────

/// Why an entry (or a loose file) must go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteReason {
    /// The sidecar did not parse, or failed the version / endpoint / id gate.
    CorruptSidecar,
    /// The sidecar names an audio file that is not there.
    MissingAudio,
    /// The audio file exists but its length is not the recorded one - a truncated
    /// or replaced file, never valid.
    LengthMismatch,
}

/// What one read of the store directory says. Pure observation: [`scan_dir`]
/// deletes nothing, so the same result can drive both the startup heal and a
/// reconcile pass without two implementations of the classification.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DirScan {
    /// Entries that are VALID by rules 1 and 2 of the validity model.
    pub entries: Vec<IndexEntry>,
    /// Files that no valid sidecar names: never-committed bytes, leftovers from a
    /// suffix change, and crashed sidecar temps.
    pub orphan_audio: Vec<PathBuf>,
    /// Sidecars that must go, with the reason.
    pub orphan_sidecars: Vec<(SongId, DeleteReason)>,
    /// In-flight temps from a previous process or a crashed download.
    pub stale_tmps: Vec<PathBuf>,
}

/// Establish that `root` is the store's to converge, or refuse it outright.
///
/// The store's convergence is DESTRUCTIVE (see [`scan_dir`]: everything that is not
/// a valid pair is deleted), and `store.dir` is a free-form path a person types. So
/// ownership is proven before anything is removed:
///
/// - marker present: ours already, proceed.
/// - directory EMPTY: a fresh store - drop the marker and proceed.
/// - anything else: refuse with an error, having touched NOTHING. The daemon warns
///   and runs without a store, which costs the offline path but not the user's files.
///
/// Deliberately not overridable by config: the failure it prevents (a state dir's
/// `resume.toml`, a music library) is silent and unrecoverable, while the fix (an
/// empty directory) is one `mkdir`.
fn claim_ownership(root: &Path) -> io::Result<()> {
    let marker = root.join(STORE_MARKER_NAME);
    match std::fs::metadata(&marker) {
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // An unreadable / erroring entry counts as PRESENT: the safe direction here is
    // always "someone else's directory".
    if std::fs::read_dir(root)?.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is not empty and carries no {} marker, so it is not a hypodj store: \
                 the store DELETES everything in its directory that is not a cached song. \
                 Point [store].dir at a dedicated directory, or empty this one, before \
                 enabling the store",
                root.display(),
                STORE_MARKER_NAME
            ),
        ));
    }
    atomic_write_bytes(&marker, STORE_MARKER_BODY.as_bytes())
}

/// Read and classify the store directory. Sorted output, so a plan built from it
/// is deterministic for a given directory state.
///
/// Subdirectories are IGNORED, never removed: the store is flat, so a directory in
/// its root is not ours and deleting other people's data is not converge-by-scan.
/// The [`STORE_MARKER_NAME`] ownership file is likewise excluded from every
/// category - it is the claim this whole destructive convergence rests on.
pub fn scan_dir(root: &Path) -> io::Result<DirScan> {
    let mut names: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        // `file_type` here does not follow symlinks, so a symlinked "audio file"
        // is not treated as ours (its length is not our length to trust).
        match entry.file_type() {
            Ok(t) if t.is_file() => {}
            Ok(_) => continue,
            Err(_) => continue,
        }
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();

    let mut scan = DirScan::default();
    let mut sidecar_ids: Vec<String> = Vec::new();
    let mut loose: Vec<String> = Vec::new();
    for name in names {
        if name == STORE_MARKER_NAME {
            // Our own ownership claim, never an orphan: healing it away would make
            // the next open refuse the directory it just converged.
            continue;
        } else if is_tmp_name(&name) {
            scan.stale_tmps.push(root.join(&name));
        } else if let Some(id) = name.strip_suffix(".toml").filter(|id| is_storable_id(id)) {
            sidecar_ids.push(id.to_string());
        } else {
            loose.push(name);
        }
    }

    // Audio names a valid sidecar accounts for. Everything else in `loose` is an
    // orphan - including a `<id>.toml.tmp.<pid>.<n>` left by a crashed sidecar
    // write, which can never match a sanitized `<id>.<suffix>`.
    let mut accounted: HashSet<String> = HashSet::new();
    for id in sidecar_ids {
        let sid = SongId(id.clone());
        let raw = match std::fs::read_to_string(root.join(format!("{id}.toml"))) {
            Ok(r) => r,
            Err(_) => {
                scan.orphan_sidecars.push((sid, DeleteReason::CorruptSidecar));
                continue;
            }
        };
        let Some(sc) = sidecar_from_toml(&raw) else {
            scan.orphan_sidecars.push((sid, DeleteReason::CorruptSidecar));
            continue;
        };
        // The sidecar's own embedded song must be the song the FILE NAME claims,
        // or the entry is mis-keyed and every downstream lookup would serve the
        // wrong audio.
        if sc.song.id.0 != id {
            scan.orphan_sidecars.push((sid, DeleteReason::CorruptSidecar));
            continue;
        }
        let audio_name = format!("{id}.{}", sc.fingerprint.suffix);
        match std::fs::metadata(root.join(&audio_name)) {
            Ok(m) if m.is_file() && m.len() == sc.fingerprint.size => {
                accounted.insert(audio_name);
                scan.entries.push(IndexEntry {
                    id: sid,
                    suffix: sc.fingerprint.suffix.clone(),
                    size: sc.fingerprint.size,
                    created: sc.fingerprint.created.clone(),
                    pinned: sc.pinned,
                    stale: sc.stale,
                    suspect: false,
                    last_played_unix: sc.last_played_unix,
                    recency_dirty: false,
                });
            }
            Ok(_) => scan
                .orphan_sidecars
                .push((sid, DeleteReason::LengthMismatch)),
            Err(_) => scan.orphan_sidecars.push((sid, DeleteReason::MissingAudio)),
        }
    }
    for name in loose {
        if !accounted.contains(&name) {
            scan.orphan_audio.push(root.join(name));
        }
    }
    scan.entries.sort_by(|a, b| a.id.0.cmp(&b.id.0));
    Ok(scan)
}

// ─────────────────────────────────────────────────────────────────────────────
// The pure pass planner
// ─────────────────────────────────────────────────────────────────────────────

/// How much of the reconcile a pass is allowed to do.
///
/// Kick scoping is what keeps a track boundary from costing a full directory scan
/// plus a `getStarred2` round trip. A LIGHT pass replans against cached state only
/// and executes just the work the user is about to hear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PassMode {
    /// Cached state only: window downloads and suspect replacements. No scan
    /// housekeeping, no verdicts, no recency flush, no eviction.
    Light,
    /// The whole reconcile: sweep, orphans, fingerprint verdicts, downloads,
    /// recency flush, eviction.
    Full,
}

/// Why a download was scheduled. Also its PRIORITY, in declaration order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DownloadReason {
    /// Replaces bytes a locally resolved errored play de-offered. First, because it
    /// gates a de-offered song's return.
    Suspect,
    /// An id in the current queue window with nothing cached. Second, because it
    /// gates the next audible advance.
    Window,
    /// The fingerprint drifted; the old bytes keep serving until this lands.
    Stale,
    /// A pin with nothing cached yet, newest-starred-first.
    Backfill,
}

// ─────────────────────────────────────────────────────────────────────────────
// The PIN SET: what starring actually asks the store to keep
// ─────────────────────────────────────────────────────────────────────────────

/// Which starred GESTURE claimed a track.
///
/// DECLARATION ORDER IS NOT THE PRIORITY MODEL and must not be reordered: `tier as
/// usize` indexes the fixed per-tier arrays on the status surface, so flipping the
/// variants would silently mislabel every per-tier count while still compiling. The
/// priority lives in [`PinTier::class`] and [`PinTier::rank`], which are exhaustive
/// matches - a new variant is then a compile error rather than a wrong number.
///
/// The gesture is only the SECOND thing the frontier asks. The first is neglect (see
/// [`Frontier::build`]): "music I haven't played for a long time, especially from
/// albums I favorited" leads with the neglect and makes the album an emphasis, so
/// neglect decides and the tier breaks the tie.
///
/// What the tier still decides, and this half is load-bearing:
///
/// - A starred ARTIST is a standing subscription, and the only UNBOUNDED gesture -
///   it means everything they have and everything they release from now on. So it is
///   FLOORED by [`PinTier::class`]: however neglected, an artist's fan-out can never
///   outrank a hand-picked gesture. Otherwise starring one prolific artist with five
///   hundred never-played albums would own the top of the order forever.
/// - Between the two HAND-PICKED gestures, a starred ALBUM is a statement about a
///   work and wins the tie against a loose starred song. That is the "especially
///   from albums I favorited" clause, and it is a tie-break rather than a gate on
///   purpose: an album-first GATE would defer essentially every hand-starred song,
///   which is a visible "I starred this and it vanished" regression nobody asked for.
///   Keeping that promise takes one more thing than ordering the clause below
///   neglect, because a whole decile of groups can tie at once: it speaks only where
///   there IS neglect to emphasise (see [`emphasis_rank`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PinTier {
    Song,
    Album,
    Artist,
}

impl PinTier {
    /// Stable label for the status surface and the logs.
    pub fn label(self) -> &'static str {
        match self {
            PinTier::Song => "song",
            PinTier::Album => "album",
            PinTier::Artist => "artist",
        }
    }

    /// The frontier's FLOOR, and the first thing its comparator asks: `0` for a
    /// hand-picked gesture (a starred song or a starred album), `1` for the fan-out
    /// of a starred artist. Above neglect in the key, so an unbounded subscription
    /// can never outrank something he pointed at.
    fn class(self) -> u8 {
        match self {
            PinTier::Song | PinTier::Album => 0,
            PinTier::Artist => 1,
        }
    }

    /// The tie-break AFTER neglect has spoken, within a class: an album before a
    /// loose starred song. Artist is last for completeness - it is already alone in
    /// its class, so this arm never decides anything.
    fn rank(self) -> u8 {
        match self {
            PinTier::Album => 0,
            PinTier::Song => 1,
            PinTier::Artist => 2,
        }
    }
}

/// [`PinTier::rank`], but only WHERE THERE IS NEGLECT TO EMPHASISE.
///
/// "especially from albums I favorited" is a clause about NEGLECTED music - it says
/// which of the music he has not played for a long time to prefer. A group with no
/// neglected bytes at all is outside that clause entirely: there is nothing to
/// emphasise, and applying the album preference there anyway is what turned a
/// tie-break into the GATE the tier's own doc says it must not be.
///
/// That is measured, not feared. On one real 88-group library 65 groups score decile
/// 0 and the ceiling cuts THROUGH that block, so ranking every starred album ahead of
/// every loose starred song inside it deferred 24 of his 47 hand-starred songs (767
/// MiB) while decile-0 albums he plays weekly stayed - the exact "I starred this and
/// it vanished" outcome [`PinTier`] promises to avoid.
///
/// So: where a group has cold bytes the ask's order stands (album, then song). Where a
/// group has none, the cheapest and most precise gesture goes first (song, then
/// album), and behind every group that does have some neglect - which keeps the
/// primary key's own direction. Both halves are constant per group up to a real
/// crossing, so neither can churn.
fn emphasis_rank(tier: PinTier, has_cold: bool) -> u8 {
    if has_cold {
        tier.rank()
    } else {
        match tier {
            PinTier::Song => 3,
            PinTier::Album => 4,
            PinTier::Artist => 5,
        }
    }
}

/// How long since a play before a track counts as NEGLECTED, in whole days.
///
/// THE ONE CONSTANT in the frontier's ranking, and a named `const` rather than
/// config on purpose: a knob invites tuning and makes "why did the order change?"
/// unanswerable, while a constant makes the rule one sentence a human can recompute.
///
/// Anchored to a measured distribution rather than fitted: across one real user's
/// 347-track pin set the per-track ages are 44 with no record at all, 17 under a
/// week, 142 within a month, 71 within three, 33 within six and 40 beyond - so a
/// 60-day line puts roughly half the tracks on the cold side. Near the middle is the
/// only condition under which a ranking key discriminates at all; a line that puts
/// everything (or nothing) on one side is a key that decides nothing.
pub const STALE_PLAY_DAYS: u32 = 60;

/// The most of the mirror a HAND-PICKED star can be guaranteed, in bytes.
pub const STAR_FLOOR_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// ...and never more than this fraction of the whole ceiling (1/2).
const STAR_FLOOR_SHARE_DEN: u64 = 2;

/// The reservation [`Frontier`] admits hand-picked stars into BEFORE the neglect
/// walk sees anything.
///
/// WHY A FLOOR EXISTS AT ALL, and it is not a hedge - it repairs a real inversion the
/// neglect key produces. He stars a song WHILE IT IS PLAYING, so a hand-picked star is
/// played-today almost by definition and scores decile 0: the ranking sorts the very
/// gesture that is most deliberate to the very bottom. Measured on his real library at
/// a 3.01 GiB ceiling, pure neglect order kept 7 of 47 hand-starred songs where
/// arrival order kept all 47. The `emphasis_rank` tie-break cannot reach this, because
/// it only speaks among groups that HAVE neglect to emphasise and only once the cut
/// descends into the decile-0 block - which under real disk pressure it never does.
///
/// So the floor is not a weight to be traded off against the score; it is a slice of
/// the budget the score does not get to spend. Above it, neglect decides everything,
/// which is what he asked for. A starred song is free to win space in the main walk
/// too - the floor is a minimum, not a cap.
///
/// BOUNDED TWO WAYS so it can never become the whole policy: an absolute
/// [`STAR_FLOOR_BYTES`], and half the ceiling, whichever is smaller. The fraction is
/// what matters on a small store - a flat 2 GiB against a 1 GiB budget would reserve
/// the entire mirror for songs and silently delete the album ranking.
pub fn star_floor(ceiling: u64) -> u64 {
    STAR_FLOOR_BYTES.min(ceiling / STAR_FLOOR_SHARE_DEN)
}

/// The frontier's rule in ONE line, so the rule and its outcome are readable
/// together and a future rule change is self-documenting on the wire.
///
/// Built from [`STALE_PLAY_DAYS`] rather than restating it, so the sentence cannot
/// drift from the number the comparator actually used.
pub fn frontier_rule() -> String {
    format!(
        "not played in {STALE_PLAY_DAYS} days first, by share of the album; \
         what is already on disk keeps a tie; starred albums before loose starred \
         songs where anything is neglected, loose starred songs first where nothing \
         is; a starred artist's other albums last"
    )
}

/// Is this track NEGLECTED - no usable play record at all, or last played at least
/// [`STALE_PLAY_DAYS`] ago?
///
/// Absence counts as cold, and that is exact rather than defensive: the server OMITS
/// `played` precisely when there is no play record, so "no record" IS "not played in
/// a long time" (longer, in fact, than its whole history horizon). An UNPARSEABLE
/// stamp lands here too, via [`Song::played_days_ago`], which fails open toward
/// keeping his music offline: a parse regression can only make a group win space,
/// never starve it.
fn is_cold(s: &Song, now_unix: u64) -> bool {
    match s.played_days_ago(now_unix) {
        // No usable STAMP. A play COUNT is still a record that he played it, so the
        // missing date is a gap in the server's bookkeeping rather than evidence of
        // neglect - Navidrome carries the two fields independently and can hold one
        // without the other. Scoring such a track maximally cold prints a line that
        // contradicts itself ("10/10 neglected, 4 plays, 0 never played") on the one
        // surface whose entire job is to be checkable by eye.
        None => s.play_count.unwrap_or(0) == 0,
        Some(d) => d >= STALE_PLAY_DAYS,
    }
}

/// What kind of entity a [`PinGroup`] came from, for the browse uri the status
/// surface prints (`song/<id>`, `album/<id>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PinKind {
    Song,
    Album,
}

impl PinKind {
    pub fn label(self) -> &'static str {
        match self {
            PinKind::Song => "song",
            PinKind::Album => "album",
        }
    }
}

/// ONE indivisible unit of wanting: a starred song (a group of one), or the tracks
/// of one album.
///
/// A starred ARTIST does NOT become one group - it becomes one group PER ALBUM, at
/// [`PinTier::Artist`]. That is what lets a huge catalogue degrade album by album
/// at the frontier instead of being refused whole, and it is why an unbounded
/// gesture is safe here: unboundedness is a BYTE problem, and the frontier bounds
/// bytes structurally.
#[derive(Clone, Debug, PartialEq)]
pub struct PinGroup {
    pub kind: PinKind,
    /// The entity id (song id or album id), for the `<kind>/<id>` uri.
    pub id: String,
    /// Human name, for the deferred list the user reads.
    pub name: String,
    pub tier: PinTier,
    /// The tracks this group wants, with server-reported sizes so the group's cost
    /// is known BEFORE a byte moves.
    pub songs: Vec<Song>,
}

/// The authoritative desired set for one pass: every starred song, every track of
/// every starred album, and every track of every album of every starred artist -
/// as groups, in newest-starred-first order within each tier.
///
/// A `PinSet` is ALL OR NOTHING by policy (see [`PinSource::pins`]): a partially
/// expanded set would look authoritative and demote a whole album over one flaky
/// `getAlbum`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PinSet {
    pub groups: Vec<PinGroup>,
}

impl PinSet {
    /// The pre-expansion shape: one [`PinTier::Song`] group per song. This is what
    /// a songs-only pin set looks like, and what most planner tests want.
    pub fn of_songs(songs: Vec<Song>) -> Self {
        Self {
            groups: songs
                .into_iter()
                .map(|s| PinGroup {
                    kind: PinKind::Song,
                    id: s.id.0.clone(),
                    name: s.title.clone(),
                    tier: PinTier::Song,
                    songs: vec![s],
                })
                .collect(),
        }
    }

    /// Every song in every group, groups in order. Duplicates are possible (a track
    /// can be a starred song AND an album track); the frontier resolves them by
    /// first claim.
    pub fn songs(&self) -> impl Iterator<Item = &Song> {
        self.groups.iter().flat_map(|g| g.songs.iter())
    }

    pub fn is_empty(&self) -> bool {
        self.groups.iter().all(|g| g.songs.is_empty())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The BUDGET: why hypodj cannot fill the disk
// ─────────────────────────────────────────────────────────────────────────────

/// Percent of the store filesystem's TOTAL size held back from hypodj, no matter
/// what `store.max_bytes` says.
///
/// Not a number anyone invented: 5 % is the fraction `mke2fs` reserves by default
/// against filesystem exhaustion. hypodj applies the same setback a second time,
/// as its own. A fraction also scales with the disk without ever needing retuning.
pub const STORE_RESERVE_FRACTION_PCT: u64 = 5;

/// Absolute floor on the reserve, 20 GiB. On a small disk 5 % is less than a
/// single NixOS system closure, so the fraction alone would not be a brake.
///
/// This is the ONE constant here not derived from a measurement: it is roughly
/// twice the 11 GB that had to be deleted to rescue this laptop's disk once. That
/// is one step better than arbitrary, not several. What survives being wrong about
/// it is the SHAPE - a ceiling that is a function of observed free space, and a
/// breach regime that writes nothing.
pub const STORE_RESERVE_FLOOR: u64 = 20 * 1024 * 1024 * 1024;

/// Setback below the effective budget that PINS may not touch, so a full pin
/// frontier can never starve the queue window and stale replacements.
///
/// Sized from real data: p90 per stored original is 62 MiB and the tail holds a
/// 415 MiB track, so the window slice must fit one worst-case original plus a few
/// ordinary ones.
pub const STORE_PIN_CEILING_SETBACK: u64 = 512 * 1024 * 1024;

/// Where a pass's effective budget came from. Reported verbatim by the `store`
/// verb, because "the budget is 16 GiB" and "the budget is 16 GiB because the disk
/// is roomy" are different facts and only the second one is falsifiable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BudgetSource {
    /// `statvfs` answered: the budget is `min(config, (avail + own) - reserve)`.
    FreeSpace,
    /// `statvfs` failed or reported nonsense: fall back to the configured cap.
    /// NEVER to unlimited - an unmeasured disk is not an empty one.
    #[default]
    Config,
}

impl BudgetSource {
    pub fn label(self) -> &'static str {
        match self {
            BudgetSource::FreeSpace => "free-space",
            BudgetSource::Config => "config",
        }
    }
}

/// THE CEILING. Pure, total, and table-testable - which is the point: the one rule
/// that makes overfilling the disk impossible must be provable without a disk.
///
/// ```text
/// reserve       = max(STORE_RESERVE_FLOOR, total * 5%)
/// effective_max = min(configured, (avail + own) - reserve)
/// ```
///
/// Four properties, each checkable:
///
/// 1. The ceiling is a function of OBSERVED free space, re-measured every full
///    pass. If another process eats the disk, hypodj's ceiling FALLS and eviction
///    hands space back.
/// 2. `(avail + own)` is INVARIANT to hypodj's own eviction, so hypodj's own
///    actions cannot move hypodj's own ceiling. That is what kills the oscillation
///    a naive fraction-of-free rule has: after evicting to meet a tight budget the
///    recomputed budget is identical, so the state settles instead of looping.
/// 3. Eviction is planned LAST and download admission budgets against bytes on
///    disk RIGHT NOW, so the store never transiently exceeds the ceiling.
/// 4. A zero here means the reserve is breached, and a zero budget writes NOTHING
///    - not even the queue window - and only deletes.
///
/// `avail` larger than `total` is nonsense from the mount; the caller treats a
/// `None` as a `statvfs` failure and falls back to the configured cap.
pub fn derive_budget(avail: u64, total: u64, own: u64, configured: u64) -> Option<u64> {
    if avail > total {
        return None;
    }
    // `total / 100 * pct` rather than `total * pct / 100`: the division first
    // cannot overflow for any u64 total, and the lost sub-percent is noise against
    // a 20 GiB floor.
    let reserve = STORE_RESERVE_FLOOR.max(total / 100 * STORE_RESERVE_FRACTION_PCT);
    let pool = avail.saturating_add(own);
    Some(configured.min(pool.saturating_sub(reserve)))
}

/// The reserve `derive_budget` would apply to a filesystem of this total size.
/// Split out so the status surface can report it without recomputing the rule.
/// The floor an EVICTION will not cut below while the disk is merely tight rather
/// than genuinely critical.
///
/// Admission and deletion are different questions and the design conflated them.
/// Admission must be strict - refusing to GROW when the reserve is breached is what
/// makes "hypodj cannot fill the disk" structural, and that is untouched. But the
/// budget is derived from FREE SPACE, so it shrinks whenever anything else on the
/// machine grows, and tying deletion to it means an ordinary `cargo build` deletes
/// the offline mirror. Measured on this machine: `target/` alone swings 11 GiB, which
/// on the real numbers (54.6 GiB free, 46.4 GiB reserve) takes the budget from
/// 10.5 GiB to under 1 - so a build would evict almost the whole mirror and cleaning
/// it would re-download ten gigabytes. That is a thrash loop, not a safety measure.
///
/// So a pass will not evict below this while `avail` is above
/// [`STORE_CRITICAL_AVAIL`]. Keeping bytes we ALREADY hold cannot fill a disk; only
/// admission can, and admission stays gated. Below the critical mark the floor is
/// abandoned and the mirror is reclaimed in full, because at that point the disk
/// genuinely needs the space more than the music does.
pub const STORE_EVICT_FLOOR: u64 = 2 * 1024 * 1024 * 1024;

/// The point at which free space stops being "tight" and starts being a problem the
/// mirror should get out of the way of.
pub const STORE_CRITICAL_AVAIL: u64 = 5 * 1024 * 1024 * 1024;

/// The byte total an eviction may reclaim down to: the pass budget normally, but
/// never below [`STORE_EVICT_FLOOR`] unless free space is critical.
///
/// Pure and total so the hysteresis is table-testable without a filesystem.
pub fn evict_target(max_bytes: u64, configured: u64, avail: u64) -> u64 {
    if avail <= STORE_CRITICAL_AVAIL {
        return max_bytes;
    }
    // The floor is CAPPED BY THE CONFIGURED CAP, and that clamp is load-bearing: a
    // flat floor would override a deliberately small `store.max_bytes` and disable
    // LRU eviction entirely for any store below it. The floor exists to absorb the
    // FREE-SPACE clamp pulling the effective budget under what was configured - it is
    // not a licence to ignore what the user asked for.
    max_bytes.max(STORE_EVICT_FLOOR.min(configured))
}

pub fn budget_reserve(total: u64) -> u64 {
    STORE_RESERVE_FLOOR.max(total / 100 * STORE_RESERVE_FRACTION_PCT)
}

/// How much of an effective budget the PIN FRONTIER may fill, leaving the rest for
/// the queue window and stale replacements.
///
/// The setback is capped at a QUARTER of the budget so this stays total: a 64 MiB
/// store (the config floor) would otherwise have a zero pin ceiling and mirror
/// nothing starred at all, which is not what a small budget asks for.
pub fn pin_ceiling(effective_max: u64) -> u64 {
    effective_max.saturating_sub(STORE_PIN_CEILING_SETBACK.min(effective_max / 4))
}

/// Read `(avail, total)` bytes for the filesystem holding `path`, via one
/// `statvfs`.
///
/// `f_bavail` (blocks free to an UNPRIVILEGED writer) is the honest number: it
/// already excludes the filesystem's own reserve, so hypodj's reserve stacks on
/// top of it rather than pretending to own it.
///
/// This is the reversal of a stated project judgement - the tape declined a `libc`
/// dependency for exactly this call. The footprint half of that judgement no
/// longer holds: `libc` is already resolved in `Cargo.lock`, already a direct
/// dependency of `hypodj-tui`, and already pulled by tokio and mio, so naming it
/// here adds ZERO nodes to the build graph. The other half is a difference in
/// kind: the tape is a budget the user presses a key to grow, while this feature
/// downloads gigabytes because he starred an album, so observing the disk IS the
/// safety property.
#[cfg(unix)]
pub fn statvfs_space(path: &Path) -> Option<(u64, u64)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `buf` is a valid, correctly sized, writable `statvfs`; `c` is a
    // NUL-terminated path that outlives the call. The return code is checked
    // before any field is read.
    let stat = unsafe {
        let mut buf = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
        if libc::statvfs(c.as_ptr(), buf.as_mut_ptr()) != 0 {
            return None;
        }
        buf.assume_init()
    };
    let frsize = if stat.f_frsize > 0 { stat.f_frsize as u64 } else { stat.f_bsize as u64 };
    if frsize == 0 {
        return None;
    }
    let avail = (stat.f_bavail as u64).saturating_mul(frsize);
    let total = (stat.f_blocks as u64).saturating_mul(frsize);
    if total == 0 {
        return None;
    }
    Some((avail, total))
}

#[cfg(not(unix))]
pub fn statvfs_space(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// One thing the reconciler should do. [`plan_pass`] returns these in EXECUTION
/// ORDER; the executor walks the list front to back.
#[derive(Clone, Debug, PartialEq)]
pub enum StoreAction {
    /// Delete an in-flight temp left by a previous process or a crashed download.
    SweepTmp(PathBuf),
    /// Delete a loose file no valid sidecar accounts for.
    DeleteFile(PathBuf),
    /// Delete both files of an entry that cannot be valid.
    DeleteEntry { id: SongId, reason: DeleteReason },
    /// Rewrite the sidecar's `stale` flag. `true` on fingerprint drift (the entry
    /// KEEPS SERVING); `false` when a later verdict confirms it again, so a
    /// transient bad verdict does not schedule a re-download forever.
    SetStale { id: SongId, stale: bool },
    /// Rewrite the sidecar's `pinned` flag: `true` promotes an opportunistic entry
    /// that is now starred, `false` DEMOTES one that is not - to evictable, with its
    /// bytes kept, so an accidental unstar and re-star costs zero bytes.
    SetPinned { id: SongId, pinned: bool },
    /// Fetch this song's original and commit it.
    Download { id: SongId, reason: DownloadReason },
    /// Persist a resolve-time recency bump, so LRU eviction does not degenerate
    /// into FIFO-by-download-date across a restart.
    FlushRecency { id: SongId, last_played_unix: u64 },
    /// Reclaim an unprotected entry. Opportunistic bytes go by oldest
    /// `last_played`; pin-group members go whole groups at a time, from BELOW the
    /// frontier first and its tail last. Never an id the same pass is downloading -
    /// see [`plan_pass`] on download-evict thrash.
    Evict(SongId),
}

/// Where one pin group ended up relative to the frontier line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupStanding {
    /// Above the line: downloaded and kept.
    Resident,
    /// Below the line, with tracks nothing else holds - a real shortfall, named.
    Deferred,
    /// It holds nothing of its OWN, because every one of its tracks is already held
    /// by a RESIDENT group that reached them first - a starred song inside a starred
    /// album, which on one real library is 20 of 47 starred songs. Nothing is missing,
    /// so naming it as deferred would be a lie; but it is not carrying anything
    /// either, so calling it resident would credit it with bytes another group paid
    /// for.
    ///
    /// RESIDENT is the load-bearing word: a group whose tracks are only wanted by
    /// another group BELOW the line is not covered by anything - those bytes are on
    /// nobody's disk - and it is Deferred like any other shortfall.
    Covered,
}

impl GroupStanding {
    pub fn label(self) -> &'static str {
        match self {
            GroupStanding::Resident => "resident",
            GroupStanding::Deferred => "deferred",
            GroupStanding::Covered => "covered",
        }
    }
}

/// One pin group as the frontier ranked it, WITH the evidence it was ranked on.
///
/// Every group gets one of these, won or lost, and the numbers here are READ OFF the
/// very integers the comparator sorted on - never re-derived for display. That is
/// what makes "why is this album deferred?" answerable with an answer that cannot
/// drift from the decision: a ranking that is clever but unexplainable is worse than
/// one that is dumb and predictable, because a good decision then looks exactly like
/// a bug.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RankedGroup {
    pub kind: PinKind,
    pub id: String,
    pub name: String,
    pub tier: PinTier,
    pub standing: GroupStanding,
    /// Position in the frontier order, 0-based. The group ranked one above is
    /// literally the line before this one in the `store frontier` listing, which is
    /// why "what did it lose to?" needs no per-group lookup.
    pub rank: usize,
    /// The group's OWN unique storable tracks and their total byte cost - what it
    /// asks for, and the denominator `cold_decile` divides.
    pub tracks: usize,
    pub bytes: u64,
    /// What is actually NOT on offer because of this group's standing: its OWN tracks
    /// that no resident group holds. Zero for `Resident` and for `Covered` by
    /// construction, so no line of the shortfall list ever overstates itself.
    ///
    /// PER GROUP, not a partition: one track wanted by two deferred groups is counted
    /// by both, because each of them really is missing it. Summing the column across
    /// the list is therefore an upper bound on the shortfall, not the shortfall - the
    /// alternative (attributing each track to one group) makes some group report a
    /// shortfall of zero while its music is on nobody's disk, which is the worse lie
    /// by far.
    pub missing_tracks: usize,
    pub missing_bytes: u64,
    /// THE KEY, 0..=10: how much of the space this group asks for goes to music he
    /// has not heard in [`STALE_PLAY_DAYS`] days. Bytes-weighted, so a long neglected
    /// album outweighs a short one.
    pub cold_decile: u8,
    /// Whether any of this group's tracks were already on disk when the frontier
    /// ranked it - the INCUMBENCY clause, which decides exact ties (see
    /// [`Frontier::build`]). On the wire because it is part of the decision: an
    /// explanation missing the clause that decided is not an explanation.
    pub held: bool,
    pub cold_tracks: usize,
    pub cold_bytes: u64,
    /// Tracks with NO usable play record (the server sent none, or the stamp did not
    /// parse). A subset of `cold_tracks`.
    pub never_played: usize,
    /// Total plays across the group. EVIDENCE ONLY - never part of the key. It is
    /// here so he can check the decision, not so it can confuse it.
    pub plays: u32,
    /// Age in whole days of the FRESHEST played track, and of the STALEST one that
    /// has a record at all. `None` when nothing in the group was ever played.
    pub last_played_days: Option<u32>,
    pub oldest_played_days: Option<u32>,
    /// Bytes it missed by, at its own turn in the walk: the cost charged to it minus
    /// the headroom left under the pin ceiling when it came up. Zero for a group that
    /// fitted. Together with `blocked_by` this is the literal answer to "why not this
    /// one" - you needed N more bytes at your position, and that is what took them.
    pub over_by: u64,
    /// The group admitted immediately before this one was refused.
    pub blocked_by: Option<String>,
    /// Refused by the hand-picked RESERVATION rather than by a full mirror (see
    /// [`star_floor`]). On the wire because it is the ONE refusal the rest of the
    /// evidence cannot account for: the ceiling visibly had room, and without this the
    /// line reads as the walk contradicting its own arithmetic.
    pub held_back_by_floor: bool,
}

impl RankedGroup {
    /// The browse uri (`song/<id>`, `album/<id>`) this group names.
    pub fn uri(&self) -> String {
        format!("{}/{}", self.kind.label(), self.id)
    }
}

/// Why bulk store work is not moving right now. `None` is the answer that means
/// "it is moving"; the other two exist so a CORRECT deferral is distinguishable
/// from a stuck reconciler, which is otherwise impossible from outside.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StoreWaiting {
    #[default]
    None,
    /// The deck is playing off the network, so a bulk backfill would fight the
    /// very playback it exists to protect.
    PlaybackRemote,
    /// `store pause` (per process, never persisted).
    Paused,
    /// The free-space reserve is breached: the store writes NOTHING and only
    /// deletes until the disk recovers.
    ReserveBreached,
}

impl StoreWaiting {
    pub fn label(self) -> &'static str {
        match self {
            StoreWaiting::None => "none",
            StoreWaiting::PlaybackRemote => "playback-remote",
            StoreWaiting::Paused => "paused",
            StoreWaiting::ReserveBreached => "reserve-breached",
        }
    }

    /// The same state as a SENTENCE, for the surfaces a person reads.
    ///
    /// `label` is the slug: stable, greppable, what a log and a machine-facing field
    /// want. But `waiting (playback-remote)` on the client's badge told the user
    /// nothing - it named an internal enum where the only question was "is it stuck?".
    /// Each phrase answers that instead: it says the hold is deliberate and, where
    /// there is one, what would end it. `None` has no phrase because a store that is
    /// not waiting says nothing at all.
    pub fn phrase(self) -> Option<&'static str> {
        match self {
            StoreWaiting::None => None,
            StoreWaiting::PlaybackRemote => Some("paused while streaming"),
            StoreWaiting::Paused => Some("paused by you"),
            // No comma: the badge is comma-joined, and a clause that splits itself
            // would arrive at the client as two.
            StoreWaiting::ReserveBreached => Some("stopped - the disk is almost full"),
        }
    }
}

/// What the store knows about itself right now, recomputed by every full pass and
/// published for anyone to read.
///
/// This REPLACES the old per-pass `warn!` about pin overflow, which fired every
/// fifteen minutes forever, named only an integer shortfall, halted the backfill
/// entirely so it could never converge, and appeared in no surface the user looks
/// at. The shortfall is now a named list of albums in `store` and a moving number
/// in `status`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StoreStatus {
    /// Whether a full pass has ever published: `false` means every number below is
    /// a placeholder.
    pub known: bool,
    /// Bytes of stored audio, and how many entries that is.
    pub bytes: u64,
    pub entries: usize,
    /// `store.max_bytes` as configured, and the budget actually in force.
    pub configured_max: u64,
    pub effective_max: u64,
    pub budget_source: BudgetSource,
    /// The free-space observation the budget came from (zero when the source is
    /// `Config`).
    pub reserve: u64,
    pub avail: u64,
    pub fs_total: u64,
    /// Tracks the frontier decided to KEEP, their total byte cost, and how many of
    /// those are on disk.
    pub resident_tracks: usize,
    pub resident_bytes: u64,
    pub cached_tracks: usize,
    /// Resident tracks still to fetch, and their exact byte cost.
    pub pending_tracks: usize,
    pub pending_bytes: u64,
    /// Resident tracks per tier, in [`PinTier`] order (song, album, artist).
    pub tier_tracks: [usize; 3],
    pub tier_bytes: [u64; 3],
    /// EVERY pin group in frontier order, won or lost, with the evidence it was
    /// ranked on. ONE source: the badge count, the deferred list and the full
    /// ranking are all views of this vector, so they cannot disagree.
    pub frontier: Vec<RankedGroup>,
    /// Ids this process has given up downloading. LOAD-BEARING: without it a
    /// pending count that will NEVER reach zero is indistinguishable from one that
    /// is merely slow.
    pub given_up: usize,
    pub waiting: StoreWaiting,
}

impl StoreStatus {
    /// The mirror holds everything it decided to hold. A real predicate the pass
    /// computes, not a guess - which is what makes "it is done" answerable.
    pub fn complete(&self) -> bool {
        self.known && self.pending_tracks == 0
    }

    /// The groups below the line that a resident group does NOT already cover, in
    /// frontier order - the shortfall as a LIST OF ALBUMS rather than an integer in
    /// a log nobody reads. A filter over `frontier`, never a second list.
    pub fn deferred(&self) -> impl Iterator<Item = &RankedGroup> {
        self.frontier
            .iter()
            .filter(|g| g.standing == GroupStanding::Deferred)
    }

    /// How many groups the shortfall names. Same filter as [`StoreStatus::deferred`]
    /// by construction, so the badge count can never exceed the list under it.
    pub fn deferred_count(&self) -> usize {
        self.deferred().count()
    }

    /// The fields a one-line badge is built from. Compared between passes so the
    /// log speaks only when something actually moved.
    fn digest(&self) -> (usize, usize, usize, usize, u64, StoreWaiting) {
        (
            self.resident_tracks,
            self.cached_tracks,
            self.pending_tracks,
            self.deferred_count(),
            self.effective_max,
            self.waiting,
        )
    }
}

/// Everything one pass gets to look at. A plain data struct so [`plan_pass`] is a
/// PURE function: no clock, no filesystem, no network, table-testable.
#[derive(Clone, Debug)]
pub struct PassInput {
    pub mode: PassMode,
    /// The pin set from THIS pass's `getStarred2` and its expansions, as GROUPS,
    /// newest-starred-first within each tier.
    ///
    /// `None` means NO AUTHORITATIVE PIN SET this pass - either a light pass (which
    /// never calls the server) or a full pass whose expansion failed anywhere.
    /// Every pin verdict is then skipped: nothing is deleted, demoted, or marked
    /// stale because the server flapped. Transient-keeps-the-claim IS offline mode.
    pub pins: Option<PinSet>,
    /// Current song plus the next `queue_ahead` upcoming Song ids, in play order.
    /// Protected from eviction and the source of `Window` downloads.
    pub window: Vec<SongId>,
    /// Extra ids the handler explicitly protects - concretely the pending-skip
    /// target, which is about to be current but is not in the window yet.
    pub protected: HashSet<SongId>,
    /// The valid entries, from [`scan_dir`] on a full pass or the live index on a
    /// light one.
    pub entries: Vec<IndexEntry>,
    /// Full-pass scan findings. Ignored in [`PassMode::Light`] by construction, so
    /// a light pass can never delete anything even if handed a scan.
    pub orphan_audio: Vec<PathBuf>,
    pub orphan_sidecars: Vec<(SongId, DeleteReason)>,
    pub stale_tmps: Vec<PathBuf>,
    /// The EFFECTIVE budget for this pass: `min(store.max_bytes, (free + own) -
    /// reserve)`, already clamped by [`derive_budget`] so this function stays pure.
    /// ZERO means the free-space reserve is breached and the pass writes NOTHING.
    pub max_bytes: u64,
    /// `store.max_bytes` as configured, for the status surface only.
    pub configured_max: u64,
    /// How the effective budget was arrived at, and the free-space observation
    /// behind it. Reported, never used to decide anything.
    pub budget_source: BudgetSource,
    pub reserve: u64,
    pub avail: u64,
    pub fs_total: u64,
    /// Cap on downloads this pass ([`DOWNLOAD_BATCH`] in production).
    pub download_batch: usize,
    /// True while the current track is a remote stream or a remotely resolved song:
    /// bulk work (stale replacements and backfill) waits so initial sync cannot
    /// stall live playback on a thin link. Window and suspect work never waits.
    pub defer_bulk: bool,
    /// `store pause`: the same suspension `defer_bulk` applies, asked for by hand.
    /// Per process and deliberately NOT persisted, so a restart resumes mirroring
    /// and pausing can never become a forgotten config.
    pub paused: bool,
    /// CALENDAR NOW, in unix seconds, read ONCE at the impure boundary in `run_pass`
    /// and threaded in - which is what keeps this function pure and the frontier's
    /// neglect ranking table-testable at any chosen date.
    ///
    /// It deliberately does NOT go through [`crate::clock::Clock`]: that seam is a
    /// monotonic `tokio::time::Instant` for SCHEDULING and cannot express a persisted
    /// calendar timestamp (see [`now_unix`]). Injection is this repo's established
    /// pattern for pure wall-clock-dependent logic - the same shape as
    /// `plan::validate`'s `now_civil` - and it is what makes a 60-day threshold
    /// sweepable in a test instead of observable once.
    ///
    /// ZERO is the default for a caller that does not care, and it is NOT neutral:
    /// every stamp is then in the future, no track is [`is_cold`], and the neglect key
    /// INVERTS rather than falls silent - decile 0 for a genuinely stale album and for
    /// one played this morning alike, leaving the whole order to the tie-breaks. That
    /// is a deterministic and safe order, but it is not the ranking, so any test that
    /// asserts anything about neglect must set a real date. Only `run_pass` and those
    /// tests set it.
    pub now_unix: u64,
}

impl PassInput {
    /// A pass over nothing, for tests and for the "store just opened, no pins yet"
    /// case. Mode and the budget are the only things a caller must supply.
    pub fn new(mode: PassMode, max_bytes: u64) -> Self {
        Self {
            mode,
            pins: None,
            window: Vec::new(),
            protected: HashSet::new(),
            entries: Vec::new(),
            orphan_audio: Vec::new(),
            orphan_sidecars: Vec::new(),
            stale_tmps: Vec::new(),
            max_bytes,
            configured_max: max_bytes,
            budget_source: BudgetSource::Config,
            reserve: 0,
            avail: 0,
            fs_total: 0,
            download_batch: DOWNLOAD_BATCH,
            defer_bulk: false,
            paused: false,
            now_unix: 0,
        }
    }
}

/// The verdict on one entry's identity fingerprint against what the server now
/// reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    /// Same bytes; if the entry was marked stale, that mark is now wrong.
    Equal,
    /// The server's original changed under a stable id (a re-import, a re-tag).
    Differ,
    /// The server did not report enough to judge. NOT drift - a server that stops
    /// sending `size` must not invalidate the mirror.
    Unknown,
}

/// Judge one cached entry against the server's current `Child`.
fn fingerprint_verdict(entry: &IndexEntry, server: &Song) -> Verdict {
    let (Some(size), Some(suffix)) = (server.size, server.suffix.as_deref()) else {
        return Verdict::Unknown;
    };
    if size != entry.size || sanitize_suffix(Some(suffix)) != entry.suffix {
        return Verdict::Differ;
    }
    // The `created` leg only votes when BOTH sides have one; one-sided absence is
    // missing information, not evidence of change.
    match (entry.created.as_deref(), server.created.as_deref()) {
        (Some(a), Some(b)) if !created_matches(a, b) => Verdict::Differ,
        _ => Verdict::Equal,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// THE FRONTIER: one ordering decides what is kept, what is fetched, and what is
// refused by name
// ─────────────────────────────────────────────────────────────────────────────

/// Where a cached entry sits relative to the frontier. Declaration order IS the
/// eviction order: opportunistic bytes go first, then everything below the line,
/// then - only under real pressure - the line's own tail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Standing {
    /// Not in the pin set at all: opportunistic queue-window bytes.
    Opportunistic,
    /// In a pin group the frontier could not fit. Never downloaded; reclaimed
    /// first if it is somehow already on disk.
    Deferred,
    /// In a pin group above the line: downloaded and kept.
    Resident,
}

/// The total ordering over pin groups, walked once per pass, with the line drawn
/// where the pin ceiling runs out.
///
/// THE POINT: admission and eviction both read THIS, so they cannot contradict
/// each other. Two separate rules - a tiered victim filter plus a tiered admission
/// loop - would disagree the moment a big album is evicted to reclaim a little
/// space, because next pass that album is the newest-starred backfill candidate
/// with fresh headroom: it downloads, overruns, and is evicted again, forever.
/// That is a download loop against the server and the disk, and it is exactly the
/// shape of bug that fills a disk slowly. Here a group below the line is still
/// below the line next pass over identical input, so it is never re-admitted.
struct Frontier<'a> {
    /// Groups in frontier order - see [`Frontier::build`] for the key.
    order: Vec<&'a PinGroup>,
    /// Parallel to `order`: whether the group fitted under the ceiling.
    resident: Vec<bool>,
    /// Parallel to `order`: the neglect evidence each group was RANKED on, computed
    /// once before the walk. The explanation is read off this, so the reason a group
    /// lost is literally the integers the comparator compared.
    score: Vec<GroupScore>,
    /// Parallel to `order`: whether the group already had bytes on disk when it was
    /// ranked - the incumbency clause, kept so the explanation carries it too.
    held: Vec<bool>,
    /// Parallel to `order`: bytes the group missed by at its own turn, and the group
    /// admitted just before it. Recorded AT the refusal site - the only place that
    /// knows both numbers - so neither is reconstructed afterwards.
    over_by: Vec<u64>,
    blocked_by: Vec<Option<String>>,
    /// Per slot: refused by the hand-picked reservation (see [`star_floor`]).
    held_back_by_floor: Vec<bool>,
    /// Storable song id -> slot in `order` for the RESIDENT group that claimed it.
    /// First claim wins in frontier order, so a track that is both a starred song
    /// and an album track is claimed at the SONG tier and cannot be dragged out by
    /// an album-level decision.
    claim: HashMap<&'a str, usize>,
    /// Storable song id -> slot in `order` for the DEFERRED group that holds it,
    /// for ids no resident group claimed.
    below: HashMap<&'a str, usize>,
    /// Every storable id anywhere in the pin set. This - not the resident set - is
    /// what the `pinned` sidecar flag mirrors, so the flag keeps meaning "starred"
    /// and an unstar still demotes.
    all_ids: HashSet<&'a str>,
    /// Byte cost per storable id, first occurrence in frontier order wins. The
    /// server's reported size, else what is on disk, else zero.
    size_by_id: HashMap<&'a str, u64>,
    /// Bytes and tracks above the line.
    resident_bytes: u64,
    resident_tracks: usize,
}

impl<'a> Frontier<'a> {
    /// Build the frontier. `size_of` gives a song's byte cost - the server's
    /// reported size, falling back to what is on disk, and zero when neither knows
    /// (rare, bounded by the pin set, and settled by the commit's exact-length
    /// gate).
    ///
    /// THE ORDER: COLD SHARE FIRST. The key is a lexicographic tuple of small
    /// integers, every one of which a human can recompute from the printed evidence:
    ///
    /// ```text
    /// (class, 10 - cold_decile, not held on disk, emphasis_rank, position in the pin set, id)
    /// ```
    ///
    /// - `class` FLOORS the unbounded gesture: a starred artist's fan-out can never
    ///   outrank something hand-picked, however neglected (see [`PinTier::class`]).
    /// - `cold_decile` is the primary key, because the ask leads with "music I
    ///   haven't played for a long time" and makes the album an emphasis, not a gate.
    ///   It is measured PER TRACK and weighted by bytes: at ALBUM granularity the
    ///   server's `played` is a max over songs, so a mostly-neglected album that he
    ///   dipped into last week looks fresh and the signal hides inside it. On one
    ///   real library that is the difference between 0.62 GiB and 3.34 GiB of
    ///   90-day-stale material - between a key that decorates and a key that decides.
    /// - `held on disk` is INCUMBENCY, and it speaks ONLY on an exact tie: between two
    ///   groups the neglect key cannot separate, the one whose bytes are already here
    ///   keeps its place. Moving bytes costs a download and a delete; a tie is by
    ///   definition not worth either. See the stability paragraph below - this clause
    ///   is what makes stability a property rather than a hope.
    /// - `emphasis_rank` is the "especially from albums I favorited" clause, speaking
    ///   only on a tie - which quantizing to ten buckets deliberately manufactures -
    ///   and only among groups that HAVE neglect to emphasise (see [`emphasis_rank`]).
    ///
    /// WHY DECILES rather than a percentage or a curve: it kills boundary churn, it
    /// creates the ties the tier clause needs in order to speak at all, and it makes
    /// the number a sentence ("8/10 of this album is music you have not played in two
    /// months") instead of an unfalsifiable rank.
    ///
    /// PLAY COUNT IS NOT IN THE KEY. He did not ask for it, and a frequency term
    /// fights the primary key head on - the most-played album is also the
    /// most-recently-played. It is carried and printed as evidence, which is where it
    /// belongs: it lets him check the decision without being able to confuse it.
    ///
    /// STABILITY IS A PROPERTY HERE, NOT A HOPE, and it matters more than the
    /// ranking: an order that reshuffles between passes makes the mirror download and
    /// evict the same albums forever, which is strictly worse than the arbitrary
    /// order it replaces. Absent a new play a track's age only increases, so `cold`
    /// is a ONE-WAY LATCH and a group's decile is monotone non-decreasing, taking at
    /// most eleven values in its lifetime.
    ///
    /// PER-GROUP MONOTONICITY IS NOT PAIRWISE STABILITY, and assuming it was is a bug
    /// this design shipped once. Two groups whose tracks cross the 60-day line on
    /// interleaved days ratchet PAST each other: A leads at deciles 2 v 2, B takes it
    /// at 2 v 5, A takes it back at 5 v 5 because the tie fell through to arrival
    /// position, B at 5 v 7, and so on - a lead handed back and forth on every tick of
    /// either group's decile. Measured on the real planner with a ceiling that fits
    /// one of the two: six reversals and 28 downloads against 24 evictions in seventy
    /// days, for two four-track albums. That is the download-evict loop this whole
    /// structure exists to forbid, at album granularity.
    ///
    /// So the tie is broken by INCUMBENCY instead of by position: what is already on
    /// disk keeps its place. The lead can then change only when the challenger's
    /// decile STRICTLY exceeds the incumbent's, which makes the sequence of leader
    /// deciles strictly increasing and therefore at most ten changes long over a
    /// pair's whole life - each one a real crossing that genuinely reordered the two,
    /// not a quantization artefact. No hysteresis CONSTANT is needed for that, and
    /// none is introduced: a tie is worth zero bytes of movement, which is the whole
    /// rule.
    ///
    /// `sort_by` is stable and the key ends in the id, so identical state draws the
    /// identical line.
    fn build(
        pins: &'a PinSet,
        by_id: &HashMap<&str, &IndexEntry>,
        ceiling: u64,
        now_unix: u64,
    ) -> Frontier<'a> {
        let size_of = |s: &Song| -> u64 {
            s.size
                .or_else(|| by_id.get(s.id.0.as_str()).map(|e| e.size))
                .unwrap_or(0)
        };

        // Sizes are gathered in the PIN SET's own order, BEFORE the sort. That is
        // required rather than tidy: the score divides by these bytes, so building
        // them from the walk's output would make the key a function of the order it
        // is supposed to produce. It also removes an order-dependence in the size map
        // itself, whose first-wins entry could otherwise change with the ranking.
        let mut all_ids: HashSet<&'a str> = HashSet::new();
        let mut size_by_id: HashMap<&'a str, u64> = HashMap::new();
        for g in &pins.groups {
            for s in &g.songs {
                if is_storable_id(&s.id.0) {
                    all_ids.insert(s.id.0.as_str());
                    size_by_id.entry(s.id.0.as_str()).or_insert_with(|| size_of(s));
                }
            }
        }

        // The whole score, one pure pass over each group's own tracks.
        let scored: Vec<GroupScore> = pins
            .groups
            .iter()
            .map(|g| GroupScore::of(g, &size_by_id, now_unix))
            .collect();

        // INCUMBENCY, in the pin set's own index space like `scored`: does this group
        // already have bytes here? Any one of its own tracks counts, which covers the
        // group mid-backfill and the track he is playing right now as well as the
        // fully mirrored album - all three are bytes a reorder would throw away.
        let held: Vec<bool> = pins
            .groups
            .iter()
            .map(|g| {
                g.songs
                    .iter()
                    .any(|s| is_storable_id(&s.id.0) && by_id.contains_key(s.id.0.as_str()))
            })
            .collect();

        let mut order: Vec<(usize, &'a PinGroup)> = pins.groups.iter().enumerate().collect();
        order.sort_by(|(ia, a), (ib, b)| {
            a.tier
                .class()
                .cmp(&b.tier.class())
                // Higher decile FIRST, which is the `10 - decile` of the rule written
                // as a reversed comparison so there is no subtraction to underflow.
                .then_with(|| scored[*ib].cold_decile.cmp(&scored[*ia].cold_decile))
                // Held FIRST, and only here: a tie is not worth a download plus a
                // delete. Above the emphasis clause deliberately - what is already on
                // disk outranks a preference between gestures, because the preference
                // is free to express next time and the bytes are not.
                .then_with(|| held[*ib].cmp(&held[*ia]))
                .then_with(|| {
                    emphasis_rank(a.tier, scored[*ia].cold_bytes > 0)
                        .cmp(&emphasis_rank(b.tier, scored[*ib].cold_bytes > 0))
                })
                // Position within the pin set, as getStarred2 returned it. NOTE this
                // is not a guarantee of anything: nothing here enforces or records
                // starred-descending order, and the starred-at timestamp is dropped
                // at the mapper. If the server's order changed, this key degrades to
                // "arbitrary but consistent", never to a reshuffle - `sort_by` is
                // stable and the id below is total.
                .then_with(|| ia.cmp(ib))
                .then_with(|| a.id.cmp(&b.id))
        });
        let (positions, order): (Vec<usize>, Vec<&'a PinGroup>) = order.into_iter().unzip();
        // Re-index the scores into frontier order, so `score[slot]` is the evidence
        // for `order[slot]` and the explanation reads off the same vector the
        // comparator sorted on.
        let score: Vec<GroupScore> = positions.iter().map(|i| scored[*i].clone()).collect();
        let held: Vec<bool> = positions.into_iter().map(|i| held[i]).collect();

        let mut claim: HashMap<&'a str, usize> = HashMap::new();
        let mut resident = vec![false; order.len()];
        let mut over_by = vec![0u64; order.len()];
        let mut blocked_by: Vec<Option<String>> = vec![None; order.len()];
        // The last group to actually take space. That, and not merely the previous
        // line, is what a refused group lost to.
        let mut last_admitted: Option<String> = None;
        let mut resident_bytes = 0u64;
        let mut resident_tracks = 0usize;
        // Refused by the hand-picked RESERVATION rather than by a full mirror -
        // carried so the group can SAY so, since "N bytes short" beside a ceiling that
        // visibly had room is the one outcome the printed evidence cannot account for.
        let mut held_back_by_floor = vec![false; order.len()];

        // THE RESERVATION, computed BEFORE the walk and spent DURING it. `star_ahead`
        // is the byte cost of every hand-picked star still to come at each slot, so an
        // album is offered the ceiling MINUS whatever the stars below it still need,
        // capped at [`star_floor`]. As the walk passes each star the reservation
        // shrinks, and once the stars are behind it the albums see the full ceiling.
        //
        // A RESERVATION AND NOT A PRE-PASS, which is the whole subtlety: admitting the
        // stars first would also make them CLAIM the tracks they share with albums (20
        // of one real user's 47 starred songs are also starred-album tracks), and the
        // download walk emits a track at its claiming group's slot. The order would
        // shift for shared tracks even when everything fits. Reserving budget instead
        // touches only WHO IS ADMITTED under pressure and leaves the order, the
        // claims, and therefore stability exactly as the comparator left them. When
        // everything fits, this is a no-op by construction.
        //
        // AND ONLY FOR A STAR NOTHING ABOVE IT ALREADY WANTS. A hand-picked song that
        // is also a track of a higher-ranked starred album is not at risk from the
        // INVERSION - the album carries it, for free, at no extra byte. It is at risk
        // only from the mirror being full, which is the ordinary scarcity the floor
        // does not exist to fix and must not quietly start deciding. Exempting it
        // keeps the reservation aimed at exactly the songs with nothing else to save
        // them - 27 of one real user's 47 - instead of charging twice for the other 20
        // and refusing albums that would have covered them anyway.
        let floor = star_floor(ceiling);
        let star_ahead: Vec<u64> = {
            let mut wanted_above: HashSet<&str> = HashSet::new();
            let mut own = vec![0u64; order.len()];
            for (slot, g) in order.iter().enumerate() {
                if g.tier == PinTier::Song {
                    let ids: Vec<&str> = g
                        .songs
                        .iter()
                        .map(|s| s.id.0.as_str())
                        .filter(|id| is_storable_id(id))
                        .collect();
                    if !ids.is_empty() && !ids.iter().all(|id| wanted_above.contains(id)) {
                        own[slot] = ids
                            .iter()
                            .map(|id| size_by_id.get(id).copied().unwrap_or(0))
                            .fold(0u64, |a, b| a.saturating_add(b));
                    }
                }
                for song in &g.songs {
                    if is_storable_id(&song.id.0) {
                        wanted_above.insert(song.id.0.as_str());
                    }
                }
            }
            let mut v = vec![0u64; order.len() + 1];
            for slot in (0..order.len()).rev() {
                v[slot] = v[slot + 1].saturating_add(own[slot]);
            }
            v
        };

        for (slot, g) in order.iter().enumerate() {
            // A group's cost counts only the tracks no earlier group already holds:
            // 20 of one real user's 47 starred songs are also starred-album tracks,
            // and charging for them twice would defer albums that actually fit.
            let mut fresh: Vec<&'a str> = Vec::new();
            let mut cost = 0u64;
            for s in &g.songs {
                let id = s.id.0.as_str();
                if !is_storable_id(id) || claim.contains_key(id) || fresh.contains(&id) {
                    continue;
                }
                fresh.push(id);
                cost = cost.saturating_add(size_by_id.get(id).copied().unwrap_or(0));
            }
            // What this group may actually spend. A star spends the whole ceiling: the
            // floor exists FOR it and can never be an extra bound on it.
            let reserved = if g.tier == PinTier::Song {
                0
            } else {
                floor.min(star_ahead[slot])
            };
            let budget = ceiling.saturating_sub(reserved);
            // WHOLE OR ABSENT. A group that does not fit is refused entire and the
            // walk CONTINUES, so a smaller later album still lands - best-effort
            // fill, order-stable, no bin-packing cleverness. On real data this is
            // not theoretical: three albums are 4 GiB of a 12.3 GiB want, and
            // per-track admission would let those three eat the budget in arrival
            // order while the frontier refuses them BY NAME and fits the other 33.
            if resident_bytes.saturating_add(cost) <= budget {
                resident[slot] = true;
                resident_bytes = resident_bytes.saturating_add(cost);
                resident_tracks += fresh.len();
                for id in fresh {
                    claim.insert(id, slot);
                }
                // A zero-cost group takes no space, so it cannot be what anything
                // later lost to. Naming it would send him to look at an album that
                // was never the reason.
                if cost > 0 {
                    last_admitted = Some(g.name.clone());
                }
            } else {
                // The two numbers that ARE the answer to "why not this one", taken
                // here because this is the only point that knows both: what it was
                // charged, and what was left when it came up.
                over_by[slot] = cost.saturating_sub(budget.saturating_sub(resident_bytes));
                blocked_by[slot] = last_admitted.clone();
                // ...and WHICH bound refused it, because "3 bytes short" beside a
                // mirror that visibly has room is the reading that looks like a bug.
                held_back_by_floor[slot] = reserved > 0
                    && resident_bytes.saturating_add(cost) <= ceiling;
            }
        }

        // Below the line, resolved AFTER the walk: a track in a deferred album that
        // a later resident group claimed is RESIDENT, not deferred.
        let mut below: HashMap<&'a str, usize> = HashMap::new();
        for (slot, g) in order.iter().enumerate() {
            if resident[slot] {
                continue;
            }
            for s in &g.songs {
                let id = s.id.0.as_str();
                if is_storable_id(id) && !claim.contains_key(id) {
                    below.entry(id).or_insert(slot);
                }
            }
        }

        Frontier {
            order,
            resident,
            score,
            held,
            over_by,
            blocked_by,
            held_back_by_floor,
            claim,
            below,
            all_ids,
            size_by_id,
            resident_bytes,
            resident_tracks,
        }
    }

    /// Where a cached entry stands, plus the group slot when it is in one.
    fn standing(&self, id: &str) -> (Standing, usize) {
        if let Some(slot) = self.claim.get(id) {
            (Standing::Resident, *slot)
        } else if let Some(slot) = self.below.get(id) {
            (Standing::Deferred, *slot)
        } else {
            (Standing::Opportunistic, usize::MAX)
        }
    }

    /// A storable id's byte cost as the frontier accounted for it.
    fn size(&self, id: &str) -> u64 {
        self.size_by_id.get(id).copied().unwrap_or(0)
    }

    /// EVERY group in frontier order, with its standing and the evidence it was
    /// ranked on. The deferred list, the badge count and the full `store frontier`
    /// listing are all views of this one vector, so the decision and the answer to
    /// "why?" cannot drift apart.
    fn ranked_groups(&self) -> Vec<RankedGroup> {
        // What each group actually CARRIES: the tracks it claimed itself. Claims only
        // ever go to a resident group, so a group with none is holding nothing of its
        // own, whichever side of the line it landed on. Counted once, linearly.
        let mut claimed_here = vec![0usize; self.order.len()];
        for slot in self.claim.values() {
            claimed_here[*slot] += 1;
        }
        let mut out = Vec::with_capacity(self.order.len());
        for (slot, g) in self.order.iter().enumerate() {
            // What this group's standing actually costs him: its OWN tracks that no
            // RESIDENT group holds. Zero for a resident group by construction, and
            // zero for one whose every track a resident group already covers.
            //
            // Asked of `claim` - who actually HOLDS the track - and never of `below`,
            // which only attributes an unheld track to the FIRST deferred group that
            // wants it. Attributing is right for a byte total and wrong for a verdict:
            // a starred song sitting under a deferred album that also wants it would
            // then report nothing missing and be filed as Covered, so twelve of one
            // real user's forty-seven hand-starred songs vanished from the answer to
            // "what did not fit?" while being on nobody's disk. A track wanted by two
            // deferred groups is therefore named by both, each honestly counting its
            // own shortfall.
            let mut missing_tracks = 0usize;
            let mut missing_bytes = 0u64;
            let mut counted: HashSet<&str> = HashSet::new();
            for s in &g.songs {
                let id = s.id.0.as_str();
                if is_storable_id(id) && !self.claim.contains_key(id) && counted.insert(id) {
                    missing_tracks += 1;
                    missing_bytes = missing_bytes.saturating_add(self.size(id));
                }
            }
            let sc = &self.score[slot];
            let standing = if claimed_here[slot] > 0 {
                GroupStanding::Resident
            } else if missing_tracks > 0 {
                GroupStanding::Deferred
            } else if sc.tracks > 0 {
                // Every track it wants is held by a RESIDENT group that reached them
                // first - so nothing of it is missing, whatever else also wants them.
                GroupStanding::Covered
            } else if self.resident[slot] {
                // No storable tracks at all - an empty group above the line.
                GroupStanding::Resident
            } else {
                GroupStanding::Deferred
            };
            out.push(RankedGroup {
                kind: g.kind,
                id: g.id.clone(),
                name: g.name.clone(),
                tier: g.tier,
                standing,
                rank: slot,
                tracks: sc.tracks,
                bytes: sc.bytes,
                missing_tracks,
                missing_bytes,
                cold_decile: sc.cold_decile,
                held: self.held[slot],
                cold_tracks: sc.cold_tracks,
                cold_bytes: sc.cold_bytes,
                never_played: sc.never_played,
                plays: sc.plays,
                last_played_days: sc.last_played_days,
                oldest_played_days: sc.oldest_played_days,
                over_by: self.over_by[slot],
                blocked_by: self.blocked_by[slot].clone(),
                held_back_by_floor: self.held_back_by_floor[slot],
            });
        }
        out
    }
}

/// The neglect evidence for ONE pin group, over the group's OWN tracks.
///
/// PURE and computed once per pass BEFORE the walk, so the sort key cannot depend on
/// the walk that consumes it. Every field here is either part of the comparator's key
/// or printed beside it, and nothing else recomputes any of them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct GroupScore {
    tracks: usize,
    bytes: u64,
    cold_tracks: usize,
    cold_bytes: u64,
    never_played: usize,
    plays: u32,
    last_played_days: Option<u32>,
    oldest_played_days: Option<u32>,
    /// 0..=10, the bytes-weighted share of this group that is neglected.
    cold_decile: u8,
}

impl GroupScore {
    fn of(g: &PinGroup, size_by_id: &HashMap<&str, u64>, now_unix: u64) -> GroupScore {
        let mut s = GroupScore::default();
        let mut counted: HashSet<&str> = HashSet::new();
        for song in &g.songs {
            let id = song.id.0.as_str();
            // Deduped within the group, exactly as the admission cost is: an album
            // that listed a track twice must not weigh twice in its own share.
            if !is_storable_id(id) || !counted.insert(id) {
                continue;
            }
            let size = size_by_id.get(id).copied().unwrap_or(0);
            s.tracks += 1;
            s.bytes = s.bytes.saturating_add(size);
            s.plays = s.plays.saturating_add(song.play_count.unwrap_or(0));
            match song.played_days_ago(now_unix) {
                // No usable stamp. Counted as never played only when there is no play
                // count either, so the printed evidence agrees with [`is_cold`]'s
                // verdict instead of contradicting it.
                None if song.play_count.unwrap_or(0) == 0 => s.never_played += 1,
                None => {}
                Some(d) => {
                    s.last_played_days = Some(s.last_played_days.map_or(d, |c| c.min(d)));
                    s.oldest_played_days = Some(s.oldest_played_days.map_or(d, |c| c.max(d)));
                }
            }
            if is_cold(song, now_unix) {
                s.cold_tracks += 1;
                s.cold_bytes = s.cold_bytes.saturating_add(size);
            }
        }
        // Bytes-weighted, so a long neglected album outweighs a short one - it is the
        // SPACE the decision is about. A group of unknown size scores 0 rather than
        // dividing by zero: with no bytes to argue over there is nothing to rank.
        // u128 because a percentage of a byte count would otherwise overflow above
        // 184 exabytes, and a wrap here would be an invisible ranking bug.
        s.cold_decile = if s.bytes == 0 {
            0
        } else {
            ((s.cold_bytes as u128 * 100 / s.bytes as u128) / 10) as u8
        };
        s
    }
}

/// Plan one reconcile pass. PURE: same input, same output, no side effects.
///
/// The returned actions are in EXECUTION ORDER, and that order is load-bearing:
///
/// 1. sweep temps, then loose orphans, then invalid entries - so the byte
///    accounting below is against what will actually be on disk;
/// 2. fingerprint verdicts (stale / pin marks) - marks only, never deletes;
/// 3. downloads, in [`DownloadReason`] priority order and, within backfill, in
///    FRONTIER order by whole group;
/// 4. recency flushes;
/// 5. evictions, LAST - and never of an id step 3 admitted, so no plan ever tells
///    the executor to download bytes and then delete them.
///
/// Because evictions come last, download admission is budgeted against the bytes
/// on disk RIGHT NOW, not against a post-eviction projection. The store therefore
/// never transiently exceeds `max_bytes`: space a pass reclaims becomes the NEXT
/// pass's headroom, and the reconciler re-enters immediately while work remains, so
/// the cost is one extra pass, not a stall.
///
/// `input.max_bytes` is the EFFECTIVE budget - already clamped against free space
/// by [`derive_budget`] before it gets here, which is what keeps this function
/// pure and the one rule that bounds the disk table-testable. A ZERO effective
/// budget means the free-space reserve is breached, and this plan then contains no
/// `Download` of ANY reason - not even `Window` or `Suspect` - and evicts
/// everything unprotected. A suppressed window download costs a track that streams
/// instead of playing from disk, which nobody notices; 415 MiB written onto a disk
/// at 99 % is a machine he cannot use.
pub fn plan_pass(input: &PassInput) -> Vec<StoreAction> {
    plan_pass_with_status(input).0
}

/// [`plan_pass`], plus the self-description a full pass publishes.
///
/// The status is `Some` exactly when the pass had an AUTHORITATIVE pin set (a full
/// pass whose `getStarred2` succeeded). A light pass and a flapping server publish
/// nothing rather than a set of zeros that would read as "the mirror is empty".
pub fn plan_pass_with_status(input: &PassInput) -> (Vec<StoreAction>, Option<StoreStatus>) {
    let full = input.mode == PassMode::Full;
    let mut out: Vec<StoreAction> = Vec::new();

    // ── 1. Housekeeping (full only). A light pass ignores the scan fields even if
    // they are populated, which is what makes "a track boundary deletes nothing" a
    // structural property rather than a caller convention.
    if full {
        for p in &input.stale_tmps {
            out.push(StoreAction::SweepTmp(p.clone()));
        }
        for p in &input.orphan_audio {
            out.push(StoreAction::DeleteFile(p.clone()));
        }
        for (id, reason) in &input.orphan_sidecars {
            out.push(StoreAction::DeleteEntry { id: id.clone(), reason: *reason });
        }
    }

    let by_id: HashMap<&str, &IndexEntry> = input
        .entries
        .iter()
        .map(|e| (e.id.0.as_str(), e))
        .collect();

    // ── 2. Fingerprint verdicts. Only on a full pass WITH an authoritative pin
    // set; otherwise every verdict is skipped and every claim is kept.
    let verdicts = full && input.pins.is_some();
    // The frontier: built once, read by BOTH admission and eviction, which is the
    // whole design. Absent without an authoritative pin set.
    let frontier = input
        .pins
        .as_ref()
        .filter(|_| verdicts)
        .map(|pins| Frontier::build(pins, &by_id, pin_ceiling(input.max_bytes), input.now_unix));
    // Ids whose fingerprint drifted in THIS pass's verdicts. Collected as the
    // verdicts are formed rather than recomputed afterwards, so the pass stays
    // linear in (entries + pins) instead of quadratic.
    let mut drifted: HashSet<&str> = HashSet::new();
    // Entries needing a replacement download: freshly drifted this pass, or still
    // carrying a stale mark from an earlier one.
    let mut needs_replacement: Vec<&IndexEntry> = Vec::new();
    // The freshest server copy of every pinned song, first occurrence wins.
    let mut pin_song: HashMap<&str, &Song> = HashMap::new();
    if let Some(pins) = input.pins.as_ref().filter(|_| verdicts) {
        for p in pins.songs() {
            if is_storable_id(&p.id.0) {
                pin_song.entry(p.id.0.as_str()).or_insert(p);
            }
        }
        // Verdicts are formed per UNIQUE id, not per group membership: a track that
        // is both a starred song and an album track must not be marked stale twice.
        let mut judged: Vec<&str> = pin_song.keys().copied().collect();
        judged.sort_unstable();
        for id in judged {
            let p = pin_song[id];
            let Some(e) = by_id.get(id) else { continue };
            match fingerprint_verdict(e, p) {
                // Marked stale, and it KEEPS SERVING - the replacement below is
                // what eventually retires these bytes, by renaming over them.
                Verdict::Differ => {
                    drifted.insert(e.id.0.as_str());
                    if !e.stale {
                        out.push(StoreAction::SetStale { id: e.id.clone(), stale: true });
                    }
                }
                // Confirmed. Clearing a previous mark matters: without it a
                // transient bad verdict would schedule a replacement every pass
                // forever.
                Verdict::Equal => {
                    if e.stale {
                        out.push(StoreAction::SetStale { id: e.id.clone(), stale: false });
                    }
                }
                Verdict::Unknown => {}
            }
            if !e.pinned {
                out.push(StoreAction::SetPinned { id: e.id.clone(), pinned: true });
            }
        }
        // THE DEMOTE PATH, and it is key-agnostic on purpose: it diffs the entries
        // against whatever ids the pin set carried, never against why they were in
        // it. So unstarring an ALBUM demotes its tracks here for free, because the
        // expansion happens inside `pins()` and the returned set is a COMPLETE
        // desired set every pass. Bytes are kept - an accidental unstar and re-star
        // costs zero downloads - and the entry is evictable in THIS same pass,
        // because eviction reads the frontier, not the pre-pass sidecar flag.
        let all_pinned = frontier.as_ref().map(|f| &f.all_ids);
        for e in &input.entries {
            let still = all_pinned.is_some_and(|s| s.contains(e.id.0.as_str()));
            if e.pinned && !still {
                out.push(StoreAction::SetPinned { id: e.id.clone(), pinned: false });
            }
        }
    }
    // Both the mark set THIS pass and any mark surviving from an earlier one need a
    // replacement download: the flag persists precisely so the work is retried after
    // a restart or an offline stretch.
    for e in &input.entries {
        if e.stale || drifted.contains(e.id.0.as_str()) {
            needs_replacement.push(e);
        }
    }

    let total_bytes = input
        .entries
        .iter()
        .fold(0u64, |acc, e| acc.saturating_add(e.size));

    // ── THE EVICTION ORDER, computed ONCE and consumed twice: first by the
    // reclaim-for-pins step inside admission (step 3d), then by the over-budget
    // eviction (step 5). One definition, so "what goes first" cannot disagree
    // between the two halves that both remove bytes.
    //
    // Absolutely excluded, whatever the pressure: the queue window plus whatever
    // the handler pinned explicitly (the pending-skip target) - evicting what he is
    // about to hear is never right. Ids this pass is DOWNLOADING are excluded at
    // use time rather than here, because `seen` is still being filled below.
    //
    // Ranked by `Standing` (declaration order IS the eviction order): opportunistic
    // bytes by oldest `last_played` (real LRU, thanks to the resolve-time bump),
    // then whole groups from BELOW the line, then the line's TAIL backwards.
    // Without an authoritative pin set there is no frontier, so this degrades to
    // exactly the old rule: unpinned entries only, LRU. A server flap must never
    // cost a starred file.
    //
    // Both consumers are full-pass only, so a light pass does not pay for the sort.
    let mut victims: Vec<((Standing, usize, u64, &str), &IndexEntry)> = Vec::new();
    if full {
        let mut protected: HashSet<&str> =
            input.protected.iter().map(|i| i.0.as_str()).collect();
        for id in &input.window {
            protected.insert(id.0.as_str());
        }
        // (standing, group order key, last_played, id). The group key is the
        // frontier slot INVERTED, so the tail of the frontier is taken first.
        victims = input
            .entries
            .iter()
            .filter(|e| !protected.contains(e.id.0.as_str()))
            .filter_map(|e| {
                let id = e.id.0.as_str();
                match frontier.as_ref() {
                    Some(f) => {
                        let (st, slot) = f.standing(id);
                        let key = match st {
                            Standing::Opportunistic => 0,
                            _ => usize::MAX - slot,
                        };
                        let lru =
                            if st == Standing::Opportunistic { e.last_played_unix } else { 0 };
                        Some(((st, key, lru, id), e))
                    }
                    // No verdicts: the sidecar's own flag stands and pins are
                    // untouchable, exactly as before the frontier existed.
                    None if !e.pinned => {
                        Some(((Standing::Opportunistic, 0, e.last_played_unix, id), e))
                    }
                    None => None,
                }
            })
            .collect();
        victims.sort_by(|a, b| a.0.cmp(&b.0));
    }
    // Ids this pass has already scheduled for eviction, so the reclaim step and
    // the over-budget eviction can never emit two `Evict`s for one id.
    let mut evicted: HashSet<&str> = HashSet::new();
    // What will actually be on disk once this pass's evictions have run. Starts at
    // the observed total and falls as bytes are reclaimed, so the over-budget
    // eviction below measures against the post-reclaim store rather than
    // double-counting bytes the reclaim already took.
    let mut on_disk = total_bytes;

    // ── 3. Downloads, in priority order, deduped by id keeping the highest
    // priority reason.
    //
    // Budget admission applies to the BULK categories (`Stale`, `Backfill`).
    // `Suspect` and `Window` are not budget-gated in the ordinary regime: they are
    // bounded in count (the suspect set; queue_ahead + 1), they are precisely what
    // the user is about to hear, and refusing them because the store is full of
    // pins would defeat the feature. Their bytes are reclaimed by the next pass's
    // eviction like any others. The ONE exception is a breached free-space reserve
    // (`max_bytes == 0`), where nothing at all is written.
    let writes_allowed = input.max_bytes > 0;
    let mut headroom = input.max_bytes.saturating_sub(total_bytes);
    let mut seen: HashSet<String> = HashSet::new();
    let mut downloads: Vec<StoreAction> = Vec::new();

    /// Admit one download, or decline it. Declines an unstorable id, a duplicate
    /// (the first, highest-priority reason wins), and anything past the batch cap.
    /// Returns whether it was admitted, so a budgeted caller only spends headroom
    /// on a download that actually happened.
    fn push(
        downloads: &mut Vec<StoreAction>,
        seen: &mut HashSet<String>,
        batch: usize,
        id: &SongId,
        reason: DownloadReason,
    ) -> bool {
        if downloads.len() >= batch || !is_storable_id(&id.0) || !seen.insert(id.0.clone()) {
            return false;
        }
        downloads.push(StoreAction::Download { id: id.clone(), reason });
        true
    }

    /// Reclaim `want` bytes FOR a resident pin group, returning what was reclaimed.
    ///
    /// WHY IT HAS TO EXIST. Admission spends `max_bytes - total_bytes`, and
    /// `total_bytes` counts the opportunistic cache the queue window leaves behind -
    /// bytes that are never budget-gated on the way in. Eviction alone cannot hand
    /// them back, because it only fires ABOVE the budget and stops the instant it is
    /// at or under. So ordinary listening walks the store up to `max_bytes` and
    /// parks there, headroom settles at roughly zero, and every starred group is
    /// refused for want of space that is being held by a cache with no claim on it.
    /// Permanently, and silently: the group is above the line, so it is not in the
    /// deferred list either. "Star an album and it is simply there later" would just
    /// stop being true once the store filled up. Bytes he asked for outrank bytes
    /// hypodj kept on spec, and this is where that is enforced on the way IN, not
    /// only on the way out.
    ///
    /// Takes victims in the ONE eviction order: opportunistic entries LRU-first,
    /// then whole groups from below the line. NEVER a resident group - spending one
    /// pin's bytes on another is precisely the download-evict thrash the frontier
    /// exists to forbid - and never anything protected or already being downloaded.
    ///
    /// ALL OR NOTHING, like group admission itself: if the cold bytes do not cover
    /// `want` it evicts NOTHING and returns 0, because a partial reclaim would
    /// delete cache for a group that is still refused afterwards - a deletion that
    /// bought nothing and a re-download of the cache next time he plays it.
    fn reclaim_for_pin<'a>(
        out: &mut Vec<StoreAction>,
        victims: &[((Standing, usize, u64, &'a str), &'a IndexEntry)],
        evicted: &mut HashSet<&'a str>,
        seen: &HashSet<String>,
        want: u64,
    ) -> u64 {
        let mut take: Vec<usize> = Vec::new();
        let mut got = 0u64;
        let mut i = 0usize;
        while i < victims.len() && got < want {
            let ((st, key, _, id), e) = victims[i];
            if st == Standing::Resident {
                // Ranked last, so everything from here on is a pin above the line.
                break;
            }
            if evicted.contains(id) || seen.contains(id) {
                i += 1;
                continue;
            }
            if st == Standing::Opportunistic {
                take.push(i);
                got = got.saturating_add(e.size);
                i += 1;
                continue;
            }
            // WHOLE GROUP for anything below the line, exactly as the eviction at
            // step 5 does: half an album is not a state this planner leaves behind.
            let mut j = i;
            while j < victims.len() && victims[j].0 .0 == st && victims[j].0 .1 == key {
                let ((_, _, _, vid), ve) = victims[j];
                if !evicted.contains(vid) && !seen.contains(vid) {
                    take.push(j);
                    got = got.saturating_add(ve.size);
                }
                j += 1;
            }
            i = j;
        }
        if got < want {
            return 0;
        }
        for idx in take {
            let ((_, _, _, id), e) = victims[idx];
            evicted.insert(id);
            out.push(StoreAction::Evict(e.id.clone()));
        }
        got
    }

    let batch = input.download_batch;
    if writes_allowed {
        // (a) Suspect replacements. A de-offered song is silent until this lands.
        for e in &input.entries {
            if e.suspect {
                push(&mut downloads, &mut seen, batch, &e.id, DownloadReason::Suspect);
            }
        }
        // (b) Window ids with nothing cached at all. An id whose entry exists but is
        // stale keeps serving, so it is bulk work, not window work.
        for id in &input.window {
            if !by_id.contains_key(id.0.as_str()) {
                push(&mut downloads, &mut seen, batch, id, DownloadReason::Window);
            }
        }
    }
    // (c) and (d) are BULK work: full passes only (a light kick executes only what
    // the user is about to hear), deferred while playback is remote, and suspended
    // by `store pause`.
    if full && writes_allowed && !input.defer_bulk && !input.paused {
        // (c) Stale replacements. A same-suffix replacement renames OVER the old
        // file, so it grows the store only by the size difference.
        for e in &needs_replacement {
            let grow = pin_song
                .get(e.id.0.as_str())
                .and_then(|p| p.size)
                .map(|new| new.saturating_sub(e.size))
                .unwrap_or(0);
            if grow > headroom {
                continue;
            }
            if push(&mut downloads, &mut seen, batch, &e.id, DownloadReason::Stale) {
                headroom = headroom.saturating_sub(grow);
            }
        }
        // (d) Starred backfill, in FRONTIER order, by WHOLE GROUP, and only from
        // groups above the line. Nothing below the line is ever downloaded - that
        // is the half of the anti-thrash property admission owns.
        //
        // ATOMIC ADMISSION: a group's entire outstanding cost is reserved out of
        // headroom the moment its first track is admitted, so a later group cannot
        // steal the space mid-fill, and a group that does not fit is skipped ENTIRE
        // (the walk continues, so a smaller later group still lands). That is also
        // what makes a resident-tail eviction safe: after eviction the pass is at
        // or under budget, so either the whole group fits again - in which case
        // re-downloading it does NOT put the store back over - or it does not fit
        // and is not re-admitted. Neither branch loops.
        //
        // AND WHEN IT DOES NOT FIT, the group first asks the cache for the space
        // ([`reclaim_for_pin`]): opportunistic bytes are what hypodj kept on spec,
        // a starred group is what he asked for, so the cache yields. Only then is
        // the group skipped.
        if let Some(front) = frontier.as_ref() {
            for (slot, g) in front.order.iter().enumerate() {
                if downloads.len() >= batch {
                    // The batch is full, so nothing further can be admitted this
                    // pass. Stopping HERE rather than walking on is what keeps the
                    // reclaim honest: it must never delete cache for a group whose
                    // downloads the cap would refuse anyway.
                    break;
                }
                if !front.resident[slot] {
                    continue;
                }
                let mut missing: Vec<&Song> = Vec::new();
                let mut outstanding = 0u64;
                let mut counted: HashSet<&str> = HashSet::new();
                for s in &g.songs {
                    let id = s.id.0.as_str();
                    if front.claim.get(id) != Some(&slot) || by_id.contains_key(id) {
                        continue;
                    }
                    if !counted.insert(id) {
                        continue;
                    }
                    missing.push(s);
                    outstanding = outstanding.saturating_add(front.size(id));
                }
                if missing.is_empty() {
                    continue;
                }
                if outstanding > headroom {
                    let want = outstanding - headroom;
                    let freed =
                        reclaim_for_pin(&mut out, &victims, &mut evicted, &seen, want);
                    if freed == 0 {
                        continue;
                    }
                    on_disk = on_disk.saturating_sub(freed);
                    // Credited, then RE-DERIVED against the budget: `headroom + freed`
                    // is the same number as `max_bytes - on_disk` minus what this pass
                    // already spent, except when a nonsense entry size has saturated
                    // the totals - and there the ceiling is the honest answer, never
                    // the saturated credit.
                    headroom = headroom
                        .saturating_add(freed)
                        .min(input.max_bytes.saturating_sub(on_disk));
                }
                let mut admitted = false;
                for s in missing {
                    if push(&mut downloads, &mut seen, batch, &s.id, DownloadReason::Backfill) {
                        admitted = true;
                    }
                }
                if admitted {
                    headroom = headroom.saturating_sub(outstanding);
                }
            }
        }
    }
    out.append(&mut downloads);

    // ── 4. Recency flushes, full passes only: a light kick fires at every track
    // boundary, and one sidecar rewrite per boundary is a write storm for a value
    // whose whole job is coarse LRU ordering.
    if full {
        for e in &input.entries {
            if e.recency_dirty {
                out.push(StoreAction::FlushRecency {
                    id: e.id.clone(),
                    last_played_unix: e.last_played_unix,
                });
            }
        }
    }

    // ── 5. Eviction, LAST, and over the SAME ranked victim list admission read
    // (built above, so the two removers cannot disagree about what goes first).
    //
    // Skipped here on top of the ranking's own exclusions: any id THIS pass is
    // downloading, and anything the reclaim-for-pins step already took.
    //
    // The download exclusion is what forbids DOWNLOAD-EVICT THRASH. Suspect and
    // stale replacements are scheduled for entries that already exist on disk, so
    // without it an over-budget pass could emit `Download` and `Evict` for the same
    // id and the executor, walking the list front to back, would fetch a whole
    // original and then unlink it. The exclusion only DELAYS such an eviction: the
    // commit clears `stale` and `suspect`, so the next pass sees an ordinary entry
    // and reclaims it if it is still the coldest.
    //
    // The measure is `on_disk` - the observed total MINUS whatever the reclaim
    // already scheduled away - so the two steps cannot double-count the same bytes
    // and evict twice over for one overflow.
    // HYSTERESIS. Reclaim toward the pass budget, but not below STORE_EVICT_FLOOR
    // while free space is merely tight - see `evict_target`. Deleting music we
    // already hold never frees the disk from a growth we did not cause, and the
    // budget shrinks with every unrelated byte the machine writes, so tying deletion
    // to it makes an ordinary build evict the mirror and cleaning up re-download it.
    let evict_to = evict_target(input.max_bytes, input.configured_max, input.avail);
    if full && on_disk > evict_to {
        let mut remaining = on_disk;
        let mut i = 0usize;
        while i < victims.len() && remaining > evict_to {
            let ((st, key, _, id), e) = victims[i];
            if evicted.contains(id) || seen.contains(id) {
                i += 1;
                continue;
            }
            if st == Standing::Opportunistic {
                evicted.insert(id);
                out.push(StoreAction::Evict(e.id.clone()));
                remaining = remaining.saturating_sub(e.size);
                i += 1;
                continue;
            }
            // WHOLE GROUP, even if that overshoots the reclaim: half an album is not
            // a state this planner is willing to leave behind. Members claimed at a
            // higher tier are not in this run at all - dropping an artist album can
            // never take a starred song with it.
            let mut j = i;
            while j < victims.len() && victims[j].0 .0 == st && victims[j].0 .1 == key {
                let ((_, _, _, vid), e) = victims[j];
                if !evicted.contains(vid) && !seen.contains(vid) {
                    evicted.insert(vid);
                    out.push(StoreAction::Evict(e.id.clone()));
                    remaining = remaining.saturating_sub(e.size);
                }
                j += 1;
            }
            i = j;
        }
    }

    // ── The self-description, published only when the pin set was authoritative.
    let status = frontier.as_ref().map(|front| {
        let mut cached_tracks = 0usize;
        let mut pending_tracks = 0usize;
        let mut pending_bytes = 0u64;
        let mut tier_tracks = [0usize; 3];
        let mut tier_bytes = [0u64; 3];
        for (id, slot) in &front.claim {
            let size = front.size(id);
            let ti = front.order[*slot].tier as usize;
            tier_tracks[ti] += 1;
            tier_bytes[ti] = tier_bytes[ti].saturating_add(size);
            if by_id.contains_key(*id) {
                cached_tracks += 1;
            } else {
                pending_tracks += 1;
                pending_bytes = pending_bytes.saturating_add(size);
            }
        }
        StoreStatus {
            known: true,
            bytes: total_bytes,
            entries: input.entries.len(),
            configured_max: input.configured_max,
            effective_max: input.max_bytes,
            budget_source: input.budget_source,
            reserve: input.reserve,
            avail: input.avail,
            fs_total: input.fs_total,
            resident_tracks: front.resident_tracks,
            resident_bytes: front.resident_bytes,
            cached_tracks,
            pending_tracks,
            pending_bytes,
            tier_tracks,
            tier_bytes,
            frontier: front.ranked_groups(),
            given_up: 0,
            waiting: if !writes_allowed {
                StoreWaiting::ReserveBreached
            } else if input.paused {
                StoreWaiting::Paused
            } else if input.defer_bulk {
                StoreWaiting::PlaybackRemote
            } else {
                StoreWaiting::None
            },
        }
    });

    (out, status)
}

// ─────────────────────────────────────────────────────────────────────────────
// The store
// ─────────────────────────────────────────────────────────────────────────────

/// The on-disk audio store: a flat directory of `<id>.<suffix>` + `<id>.toml`
/// pairs, plus the in-memory index playback probes.
///
/// EXACTLY ONE DISK WRITER (the reconciler task). Playback and restore only read:
/// [`lookup`](Self::lookup) is a short-locked index probe plus one stat, with the
/// sole in-memory exceptions of the recency bump and the suspect flag - both index
/// mutations under the same short lock, never disk. The `std::sync::Mutex`es here
/// are never held across an await, and every public method on this type is sync.
pub struct AudioStore {
    root: PathBuf,
    cfg: StoreConfig,
    /// The authoritative in-memory view. Short-locked only: probe, bump, mark.
    index: Mutex<HashMap<SongId, IndexEntry>>,
    /// Current song plus the next `queue_ahead` upcoming Song ids.
    window: Mutex<Vec<SongId>>,
    /// Ids protected from eviction beyond pins and the window - the pending-skip
    /// target, which is about to be current but is not in the window yet.
    protected: Mutex<HashSet<SongId>>,
    /// The reconciler's wake signal. `notify_one` semantics: a kick delivered WHILE
    /// a pass is running leaves a permit, so the loop re-enters instead of sleeping
    /// through the work. Correctness is level-triggered regardless (the next pass
    /// replans from scratch), so a coalesced kick costs latency, never a wrong state.
    kick: Notify,
    /// Set by [`kick_full`](Self::kick_full), consumed by the loop: the next wake
    /// must be a FULL pass (scan + `getStarred2` verdicts), not a light one. Sticky,
    /// so a full kick landing during a light pass is still honored afterwards.
    full_requested: AtomicBool,
    /// Whether what is loaded on the deck comes off the NETWORK (a raw stream, or a
    /// song that resolved to a stream URL). Bulk work - stale replacements and
    /// starred backfill - waits while this is true, so an initial sync cannot starve
    /// live playback on a thin link. Window and suspect downloads NEVER wait: they
    /// are precisely what the user is about to hear.
    ///
    /// It tracks the DECK, not the last load: a stopped or drained deck plays
    /// nothing, so the handler clears it wherever playback ends (see
    /// `set_store_playback_remote`). Latching it on the last remote load and never
    /// clearing it would suspend the pin mirror for the rest of the process - the
    /// idle hours are exactly when it is supposed to fill.
    playback_remote: AtomicBool,
    /// `store pause`: bulk work suspended BY HAND. Read once per pass exactly like
    /// [`playback_remote`](Self::playback_remote), and deliberately not persisted -
    /// a restart resumes mirroring, so the safe state is the default and a pause
    /// cannot become a forgotten config.
    paused: AtomicBool,
    /// WHAT THE STORE KNOWS ABOUT ITSELF, overwritten at the end of every full pass
    /// that had an authoritative pin set. Short-locked and clone-out; read-only for
    /// everyone but the reconciler.
    ///
    /// This is the surface the old per-pass overflow `warn!` should have been: the
    /// shortfall is a NAMED LIST OF ALBUMS the `store` verb prints and a moving
    /// number on `status`, not an integer in a log line every fifteen minutes
    /// forever.
    status: Mutex<StoreStatus>,
    /// Fired by the reconciler after any pass whose `getStarred2` SUCCEEDED - the
    /// "the server is back" edge, which the daemon uses to refresh the id-only
    /// placeholders an OFFLINE restore installed.
    ///
    /// A bare `Fn()` and not an async trait on purpose: the store must not learn about
    /// the handler (the dependency runs the other way), and the one subscriber wants
    /// nothing more than "spawn my refresh now". Absent in every test and in a
    /// store-less build.
    server_back_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl AudioStore {
    /// Open (creating if needed) the store at `root`, then SCAN AND HEAL it.
    ///
    /// Crash recovery, external tampering, and a cold start are all the same code
    /// path here: the directory is the truth, and whatever does not add up to a
    /// valid pair is removed. Concretely - temps from a dead process are swept,
    /// loose files no sidecar accounts for are deleted, and a sidecar that is
    /// corrupt, version-mismatched, or names a missing / wrong-length audio file
    /// takes both files with it. What survives becomes the index.
    ///
    /// Because that heal is DESTRUCTIVE, the directory must be OURS first: `root` is
    /// adopted only when it is empty (a fresh store, which then gets the
    /// [`STORE_MARKER_NAME`] claim) or already carries the marker. A non-empty
    /// unmarked directory - `store.dir` aimed at the state dir, a music folder, a
    /// home directory - is REFUSED with an error and NOT TOUCHED: not one file is
    /// scanned away. There is no flag to override that; a directory worth mirroring
    /// into is a directory you can create empty.
    ///
    /// Returns the io error to the caller, which warns and runs WITHOUT a store -
    /// never fatal, the resume posture.
    pub fn open(root: PathBuf, cfg: StoreConfig) -> io::Result<Self> {
        std::fs::create_dir_all(&root)?;
        claim_ownership(&root)?;
        let scan = scan_dir(&root)?;
        let mut swept = 0usize;
        for p in &scan.stale_tmps {
            match std::fs::remove_file(p) {
                Ok(()) => swept += 1,
                Err(e) => tracing::warn!(path = %p.display(), error = %e, "store: sweeping temp failed"),
            }
        }
        let mut orphans = 0usize;
        for p in &scan.orphan_audio {
            match std::fs::remove_file(p) {
                Ok(()) => orphans += 1,
                Err(e) => tracing::warn!(path = %p.display(), error = %e, "store: removing orphan failed"),
            }
        }
        for (id, reason) in &scan.orphan_sidecars {
            tracing::info!(id = %id.0, ?reason, "store: healing an invalid entry");
            remove_pair(&root, id, None);
        }
        let index: HashMap<SongId, IndexEntry> = scan
            .entries
            .iter()
            .cloned()
            .map(|e| (e.id.clone(), e))
            .collect();
        let bytes: u64 = index.values().fold(0u64, |a, e| a.saturating_add(e.size));
        tracing::info!(
            root = %root.display(),
            entries = index.len(),
            bytes,
            swept,
            orphans,
            healed = scan.orphan_sidecars.len(),
            "store: opened"
        );
        Ok(Self {
            root,
            cfg,
            index: Mutex::new(index),
            window: Mutex::new(Vec::new()),
            protected: Mutex::new(HashSet::new()),
            kick: Notify::new(),
            full_requested: AtomicBool::new(false),
            // Nothing is loaded yet, so nothing is streaming: the first pass is free
            // to backfill.
            playback_remote: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            status: Mutex::new(StoreStatus::default()),
            server_back_hook: Mutex::new(None),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config(&self) -> &StoreConfig {
        &self.cfg
    }

    /// The audio file for `id` with `suffix` (already sanitized).
    pub fn audio_path(&self, id: &SongId, suffix: &str) -> PathBuf {
        self.root.join(format!("{}.{}", id.0, suffix))
    }

    /// The sidecar for `id`.
    pub fn sidecar_path(&self, id: &SongId) -> PathBuf {
        self.root.join(format!("{}.toml", id.0))
    }

    /// A fresh in-flight temp path in the store root: `tmp.<pid>.<seq>`. Same
    /// directory as its target so the committing rename is atomic; unique per
    /// (process, call) so concurrent writers cannot clobber each other.
    pub fn tmp_path(&self) -> PathBuf {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        self.root.join(format!("tmp.{}.{}", std::process::id(), seq))
    }

    /// THE PLAY-TIME PROBE. Returns the local path to play, or `None` to fall
    /// through to a stream URL.
    ///
    /// SYNC and cheap by contract: one short index lock, then ONE `fs::metadata`
    /// confirming existence and the exact recorded length. No hashing, no parsing,
    /// no network, no lock held across anything. The lock is released BEFORE the
    /// stat so a slow filesystem cannot block another connection's probe.
    ///
    /// Rejects a suspect entry outright - known-bad bytes are never re-offered,
    /// because replaying them just loops corrupt audio. De-offering costs a stream
    /// attempt that fails, the same audible outcome, while the bytes stay on disk
    /// for the online repair.
    ///
    /// NOT covered, deliberately: a file whose header is intact and whose TAIL has
    /// rotted. Its length still matches, so this returns it forever. The repair is
    /// manual - `rm <store>/<id>.*` and let the next pass re-download. See the
    /// module docs.
    pub fn lookup(&self, id: &SongId) -> Option<PathBuf> {
        let (suffix, size) = {
            let index = self.index.lock().expect("store index lock");
            let e = index.get(id)?;
            if e.suspect {
                return None;
            }
            (e.suffix.clone(), e.size)
        };
        let path = self.audio_path(id, &suffix);
        match std::fs::metadata(&path) {
            Ok(m) if m.is_file() && m.len() == size => Some(path),
            _ => None,
        }
    }

    /// Record that playback resolved to `id`'s local bytes. In-memory ONLY: the
    /// reconciler flushes dirty recency to sidecars on a full pass, so the
    /// one-disk-writer rule survives and a crash loses at most one interval of
    /// LRU ordering.
    pub fn note_played(&self, id: &SongId, now_unix: u64) {
        let mut index = self.index.lock().expect("store index lock");
        if let Some(e) = index.get_mut(id) {
            e.last_played_unix = now_unix;
            e.recency_dirty = true;
        }
    }

    /// Mark `id`'s bytes SUSPECT after a locally resolved play ended in an error.
    /// Returns whether this changed anything (so the caller can skip a pointless
    /// kick).
    ///
    /// De-offers immediately; deletes NOTHING. The bytes go only when a replacement
    /// has been downloaded, length-verified, and renamed over them, so an
    /// ao/pipewire hiccup on suspend costs at worst a stream fallback until the
    /// server returns, never the loss of a pinned file.
    pub fn mark_suspect(&self, id: &SongId) -> bool {
        let mut index = self.index.lock().expect("store index lock");
        match index.get_mut(id) {
            Some(e) if !e.suspect => {
                e.suspect = true;
                tracing::warn!(id = %id.0, "store: local bytes marked suspect; de-offered until replaced");
                true
            }
            _ => false,
        }
    }

    /// Replace the queue window. Returns whether it changed, so the caller only
    /// kicks on a real change.
    pub fn set_window(&self, ids: Vec<SongId>) -> bool {
        let mut w = self.window.lock().expect("store window lock");
        if *w == ids {
            return false;
        }
        *w = ids;
        true
    }

    pub fn window(&self) -> Vec<SongId> {
        self.window.lock().expect("store window lock").clone()
    }

    /// Protect `id` from eviction until [`unprotect`](Self::unprotect) - used for
    /// the pending-skip target, which is about to be current but is not yet in the
    /// window.
    pub fn protect(&self, id: SongId) {
        self.protected.lock().expect("store protected lock").insert(id);
    }

    pub fn unprotect(&self, id: &SongId) {
        self.protected.lock().expect("store protected lock").remove(id);
    }

    pub fn protected_ids(&self) -> HashSet<SongId> {
        self.protected.lock().expect("store protected lock").clone()
    }

    /// A snapshot of the index, id-sorted so callers (and plans built from it) are
    /// deterministic.
    pub fn entries(&self) -> Vec<IndexEntry> {
        let index = self.index.lock().expect("store index lock");
        let mut v: Vec<IndexEntry> = index.values().cloned().collect();
        v.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        v
    }

    /// Total stored audio bytes, as the index believes them.
    pub fn total_bytes(&self) -> u64 {
        self.index
            .lock()
            .expect("store index lock")
            .values()
            .fold(0u64, |a, e| a.saturating_add(e.size))
    }

    /// The sidecar-embedded [`Song`] for `id`, when the entry is valid. This is
    /// what lets an offline restore and an offline `add song/<id>` carry real
    /// metadata instead of a bare id.
    pub fn cached_song(&self, id: &SongId) -> Option<Song> {
        self.index.lock().expect("store index lock").get(id)?;
        self.read_sidecar(id).map(|sc| sc.song)
    }

    /// Read and validate `id`'s sidecar. `None` for missing, unreadable, or corrupt
    /// (the [`sidecar_from_toml`] bar).
    pub fn read_sidecar(&self, id: &SongId) -> Option<Sidecar> {
        let raw = std::fs::read_to_string(self.sidecar_path(id)).ok()?;
        sidecar_from_toml(&raw)
    }

    /// Write `id`'s sidecar through the shared atomic discipline (sibling temp,
    /// fsync, rename). THIS RENAME IS THE COMMIT POINT for a cached song.
    pub fn write_sidecar(&self, sc: &Sidecar) -> io::Result<()> {
        if !is_storable_id(&sc.song.id.0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unstorable song id: {:?}", sc.song.id.0),
            ));
        }
        atomic_write_bytes(
            &self.sidecar_path(&sc.song.id),
            sidecar_to_toml(sc).as_bytes(),
        )
    }

    /// COMMIT a downloaded original: the whole write protocol from verified temp to
    /// a valid entry, in the one order that cannot produce a valid-looking partial.
    ///
    /// 1. re-verify the temp's length against the server-reported `song.size` (the
    ///    downloader checked it at EOF; this is the single gate every path passes);
    /// 2. fsync the temp;
    /// 3. rename it onto `<id>.<suffix>`, ATOMICALLY REPLACING any previous file of
    ///    the same suffix - mpv's already-open fd keeps playing the old inode via
    ///    unlink semantics, so a mid-play replacement is inaudible;
    /// 4. fsync the directory, so the rename is on disk before anything names it;
    /// 5. WRITE THE SIDECAR - the commit. Until this instant the new bytes are an
    ///    orphan the next scan deletes, never something [`lookup`](Self::lookup)
    ///    can offer;
    /// 6. only NOW delete a previous audio file whose suffix DIFFERED, so old bytes
    ///    are never unlinked before their verified replacement exists;
    /// 7. update the index (clearing `stale` and `suspect` - these bytes are fresh).
    ///
    /// Sync, using `std::fs` throughout: the caller runs it in `spawn_blocking`.
    /// On a length mismatch the temp is removed and the PREVIOUS entry is left
    /// untouched and still serving.
    pub fn commit(
        &self,
        song: &Song,
        tmp: &Path,
        pinned: bool,
        now_unix: u64,
    ) -> io::Result<PathBuf> {
        if !is_storable_id(&song.id.0) {
            let _ = std::fs::remove_file(tmp);
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unstorable song id: {:?}", song.id.0),
            ));
        }
        // No server-reported size means no commit gate, and without the gate a
        // truncated download would commit as valid. Refuse rather than trust.
        let Some(size) = song.size.filter(|s| *s > 0) else {
            let _ = std::fs::remove_file(tmp);
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("no server-reported size for {:?}; refusing to commit", song.id.0),
            ));
        };
        let got = std::fs::metadata(tmp)?.len();
        if got != size {
            let _ = std::fs::remove_file(tmp);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{:?}: downloaded {got} bytes, server reported {size}; refusing to commit",
                    song.id.0
                ),
            ));
        }
        // fsync the bytes before the rename that publishes them.
        std::fs::OpenOptions::new().write(true).open(tmp)?.sync_all()?;

        let suffix = sanitize_suffix(song.suffix.as_deref());
        let audio = self.audio_path(&song.id, &suffix);
        let previous_suffix = {
            let index = self.index.lock().expect("store index lock");
            index.get(&song.id).map(|e| e.suffix.clone())
        };
        std::fs::rename(tmp, &audio)?;
        // Durability of the rename, not correctness of the ordering: a failure here
        // is worth a warning, not a failed commit (and on some filesystems a
        // directory fsync is a no-op anyway).
        if let Err(e) = File::open(&self.root).and_then(|d| d.sync_all()) {
            tracing::warn!(root = %self.root.display(), error = %e, "store: directory fsync failed");
        }

        let sc = Sidecar {
            schema_version: STORE_SCHEMA_VERSION,
            endpoint: ENDPOINT_DOWNLOAD.to_string(),
            content_type: song.content_type.clone(),
            pinned,
            stale: false,
            fetched_at_unix: now_unix,
            last_played_unix: now_unix,
            fingerprint: Fingerprint {
                size,
                suffix: suffix.clone(),
                created: song.created.clone(),
            },
            song: song.clone(),
        };
        self.write_sidecar(&sc)?;

        // The new pair is valid; only now may differently-suffixed old bytes go.
        if let Some(old) = previous_suffix.filter(|s| *s != suffix) {
            let old_path = self.audio_path(&song.id, &old);
            if let Err(e) = std::fs::remove_file(&old_path) {
                tracing::warn!(path = %old_path.display(), error = %e, "store: removing superseded audio failed");
            }
        }

        let mut index = self.index.lock().expect("store index lock");
        index.insert(
            song.id.clone(),
            IndexEntry {
                id: song.id.clone(),
                suffix,
                size,
                created: song.created.clone(),
                pinned,
                stale: false,
                suspect: false,
                last_played_unix: now_unix,
                recency_dirty: false,
            },
        );
        Ok(audio)
    }

    /// Drop `id` entirely: the SIDECAR first (which de-commits the entry), then out
    /// of the index, then the audio.
    ///
    /// That order is what ties the OFFER to the on-disk truth in both directions.
    /// De-committing first means an interrupted delete leaves an orphan the next
    /// scan sweeps, never a sidecar pointing at bytes that are gone. And because a
    /// REFUSED sidecar unlink - a read-only remount, an immutable file, a disk gone
    /// bad - returns before the index is touched, an entry the filesystem would not
    /// let us delete is still whole on disk and therefore KEEPS BEING OFFERED. The
    /// earlier index-first order de-offered it instead, so a still-valid pinned song
    /// streamed over the network until the next FULL pass re-adopted it from disk -
    /// up to `store.sync_interval_secs` later. Keep-until-replaced cuts both ways:
    /// what we could not remove, we keep serving, and its bytes keep counting
    /// against the budget because they are genuinely still there.
    ///
    /// Once the sidecar IS gone the entry can no longer be valid (section 2, rule 1)
    /// whatever happens to the bytes, so it leaves the index BEFORE the unlink -
    /// playback is never handed a path that is about to vanish.
    ///
    /// Returns whether the FILES actually went (see [`remove_audio`]). `false` means
    /// bytes are still on disk, so a caller counting reclaimed space must not treat
    /// it as progress. Removing an id the store never had returns `true`: nothing is
    /// there, which is the requested state.
    pub fn remove_entry(&self, id: &SongId) -> bool {
        if !remove_sidecar(&self.root, id) {
            return false;
        }
        let suffix = {
            let mut index = self.index.lock().expect("store index lock");
            index.remove(id).map(|e| e.suffix)
        };
        remove_audio(&self.root, id, suffix.as_deref())
    }

    /// Rewrite the sidecar's `pinned` flag and mirror it into the index. A demote
    /// keeps the bytes: an accidental unstar and re-star costs nothing.
    pub fn set_pinned(&self, id: &SongId, pinned: bool) -> io::Result<()> {
        self.mutate_sidecar(id, |sc| sc.pinned = pinned)?;
        let mut index = self.index.lock().expect("store index lock");
        if let Some(e) = index.get_mut(id) {
            e.pinned = pinned;
        }
        Ok(())
    }

    /// Rewrite the sidecar's `stale` flag and mirror it into the index. A stale
    /// entry KEEPS SERVING - the flag only schedules a replacement.
    pub fn set_stale(&self, id: &SongId, stale: bool) -> io::Result<()> {
        self.mutate_sidecar(id, |sc| sc.stale = stale)?;
        let mut index = self.index.lock().expect("store index lock");
        if let Some(e) = index.get_mut(id) {
            e.stale = stale;
        }
        Ok(())
    }

    /// Persist a resolve-time recency bump and clear the dirty flag, so LRU
    /// eviction survives a restart instead of degenerating into
    /// FIFO-by-download-date.
    pub fn flush_recency(&self, id: &SongId, last_played_unix: u64) -> io::Result<()> {
        self.mutate_sidecar(id, |sc| sc.last_played_unix = last_played_unix)?;
        let mut index = self.index.lock().expect("store index lock");
        if let Some(e) = index.get_mut(id) {
            if e.last_played_unix == last_played_unix {
                e.recency_dirty = false;
            }
        }
        Ok(())
    }

    /// Read-modify-write a sidecar through the atomic discipline. A missing or
    /// corrupt sidecar is `NotFound`: there is nothing to amend, and the entry is
    /// already on the next scan's heal list.
    fn mutate_sidecar(&self, id: &SongId, f: impl FnOnce(&mut Sidecar)) -> io::Result<()> {
        let mut sc = self.read_sidecar(id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no valid sidecar for {:?}", id.0),
            )
        })?;
        f(&mut sc);
        self.write_sidecar(&sc)
    }

    // ── the reconciler's wake signal ────────────────────────────────────────

    /// Wake the reconciler for a LIGHT pass: replan against CACHED state only and
    /// execute just the window / suspect downloads. No directory scan, no
    /// `getStarred2`, no deletes - which is what keeps a track boundary from costing
    /// a full reconcile.
    ///
    /// Sync and never blocking, so it is safe to call from the handler's short
    /// locked sections.
    pub fn kick_light(&self) {
        self.kick.notify_one();
    }

    /// Wake the reconciler for a FULL pass (scan, `getStarred2` verdicts, downloads,
    /// recency flush, eviction). Used where the PIN SET itself may have changed - a
    /// star or unstar - since only a full pass talks to the server.
    pub fn kick_full(&self) {
        self.full_requested.store(true, Ordering::Release);
        self.kick.notify_one();
    }

    /// Await the next kick. Only the reconciler task calls this.
    async fn kicked(&self) {
        self.kick.notified().await;
    }

    /// Take (and clear) a pending full-pass request. `true` means the next pass must
    /// be full.
    fn take_full_request(&self) -> bool {
        self.full_requested.swap(false, Ordering::AcqRel)
    }

    /// TEST-ONLY: observe (and clear) the pending full-pass request, so a handler
    /// test can prove WHICH KIND of kick a seam fires - a star flip must reach the
    /// server, a track boundary must not.
    #[cfg(test)]
    pub fn take_full_request_for_test(&self) -> bool {
        self.take_full_request()
    }

    /// Record whether the deck is playing off the NETWORK. `false` is not only "a
    /// local file is loaded" but also "nothing is loaded": a stopped deck defers
    /// nothing. See [`playback_remote`](Self::playback_remote).
    pub fn set_playback_remote(&self, remote: bool) {
        self.playback_remote.store(remote, Ordering::Relaxed);
    }

    /// Whether the deck is currently playing off the network, i.e. whether bulk
    /// store work should defer this pass.
    pub fn playback_remote(&self) -> bool {
        self.playback_remote.load(Ordering::Relaxed)
    }

    /// Suspend or resume BULK store work by hand (`store pause` / `store resume`).
    /// Returns the new state. Window and suspect downloads are unaffected: pausing
    /// the mirror must never make the next track stream.
    pub fn set_paused(&self, paused: bool) -> bool {
        self.paused.store(paused, Ordering::Relaxed);
        if !paused {
            self.kick_full();
        }
        paused
    }

    /// Whether bulk work is suspended by hand.
    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// The last published self-description, CLONED OUT under a short lock.
    /// `known == false` until the first full pass with an authoritative pin set.
    pub fn status(&self) -> StoreStatus {
        self.status.lock().expect("store status lock").clone()
    }

    /// Publish a fresh self-description. The reconciler is the only caller.
    fn publish_status(&self, status: StoreStatus) {
        *self.status.lock().expect("store status lock") = status;
    }

    /// TEST-ONLY: publish a self-description without running a pass, so a HANDLER
    /// test can prove that the badge on `status` and the detail behind the `store`
    /// verb read the SAME published status and cannot drift apart.
    #[cfg(test)]
    pub fn publish_status_for_test(&self, status: StoreStatus) {
        self.publish_status(status);
    }

    /// Register the "the server answered" callback (see
    /// [`server_back_hook`](Self::server_back_hook)). Last writer wins; the daemon
    /// registers exactly one, holding only a `Weak` to the handler so this can never
    /// keep it alive.
    pub fn set_server_back_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.server_back_hook.lock().expect("store hook lock") = Some(hook);
    }

    /// Fire the server-back hook, if one is registered. The `Arc` is CLONED OUT before
    /// the call so the lock is never held while a subscriber runs.
    fn fire_server_back(&self) {
        let hook = self.server_back_hook.lock().expect("store hook lock").clone();
        if let Some(h) = hook {
            h();
        }
    }

    /// Replace the index with what a fresh [`scan_dir`] found, CARRYING OVER the two
    /// in-memory-only flags for ids that survive.
    ///
    /// The disk is the truth about which pairs exist and what they contain, but it
    /// knows nothing about `suspect` (never persisted, deliberately) or a recency
    /// bump that has not been flushed yet. Dropping either would re-offer bytes a
    /// play just proved bad, and would lose LRU ordering on every pass. Ids the scan
    /// did not see are gone: the scan already classified them as orphans and the
    /// pass deletes them.
    pub fn resync_from_scan(&self, scanned: Vec<IndexEntry>) {
        let mut index = self.index.lock().expect("store index lock");
        let mut next: HashMap<SongId, IndexEntry> = HashMap::with_capacity(scanned.len());
        for mut e in scanned {
            if let Some(old) = index.get(&e.id) {
                e.suspect = old.suspect;
                // A bump that has not reached the sidecar yet is NEWER than what the
                // scan read back off disk, so it wins and stays dirty.
                if old.recency_dirty && old.last_played_unix >= e.last_played_unix {
                    e.last_played_unix = old.last_played_unix;
                    e.recency_dirty = true;
                }
            }
            next.insert(e.id.clone(), e);
        }
        *index = next;
    }

    // NOTE: there is deliberately no free-standing `sweep_tmps` here. Temps are
    // classified in exactly ONE place - [`scan_dir`] via [`is_tmp_name`] - and the
    // two live sweeps (the startup heal in [`open`](Self::open) and the full pass's
    // [`StoreAction::SweepTmp`]) both apply that one classification. A second
    // directory walk with its own predicate could only drift away from it.
}

/// Delete an entry's two files, SIDECAR FIRST so the entry is de-committed before
/// its bytes go. `suffix` names the audio file when known; otherwise every
/// `<id>.<something>` in the directory is removed, which is what heals an entry
/// left over from a suffix change.
///
/// Returns whether the bytes ACTUALLY went. A filesystem that refuses the unlink -
/// read-only, immutable, a full or broken disk - reclaims nothing, and a caller
/// that counted the attempt as progress would keep re-planning the same eviction
/// forever. Every refusal is warned once here, so the failure is visible rather
/// than merely silent-but-bounded.
fn remove_pair(root: &Path, id: &SongId, suffix: Option<&str>) -> bool {
    // Both halves always run - a refused sidecar must not leave the bytes behind
    // as an orphan the scan would have to sweep on some later pass.
    let sidecar_gone = remove_sidecar(root, id);
    let audio_gone = remove_audio(root, id, suffix);
    sidecar_gone && audio_gone
}

/// Delete an entry's SIDECAR - the COMMIT RECORD, so this alone de-commits the
/// entry: without it the pair is no longer valid (section 2, rule 1) whatever
/// happens to the bytes, and what is left is an orphan the next scan sweeps.
///
/// Returns whether it is gone. An absent sidecar counts as gone - that is the
/// requested state - but a REFUSED unlink does not, and a refusal means the entry
/// on disk is still whole.
fn remove_sidecar(root: &Path, id: &SongId) -> bool {
    let sidecar = root.join(format!("{}.toml", id.0));
    if let Err(e) = std::fs::remove_file(&sidecar) {
        if e.kind() != io::ErrorKind::NotFound {
            tracing::warn!(path = %sidecar.display(), error = %e, "store: removing sidecar failed");
            return false;
        }
    }
    true
}

/// Delete an entry's AUDIO bytes. `suffix` names the file when known; otherwise
/// every `<id>.<something>` left in the directory goes, which is what heals an
/// entry left over from a suffix change.
///
/// Returns whether the bytes ACTUALLY went, which is the only thing that reclaims
/// space - see [`remove_pair`].
fn remove_audio(root: &Path, id: &SongId, suffix: Option<&str>) -> bool {
    let mut gone = true;
    match suffix {
        Some(s) => {
            let audio = root.join(format!("{}.{}", id.0, s));
            if let Err(e) = std::fs::remove_file(&audio) {
                if e.kind() != io::ErrorKind::NotFound {
                    gone = false;
                    tracing::warn!(path = %audio.display(), error = %e, "store: removing audio failed");
                }
            }
        }
        None => {
            let prefix = format!("{}.", id.0);
            // An unreadable directory means nothing could be removed and nothing is
            // even known to be there: not progress.
            let Ok(rd) = std::fs::read_dir(root) else { return false };
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(&prefix) && !matches!(entry.file_type(), Ok(t) if t.is_dir()) {
                    if let Err(e) = std::fs::remove_file(entry.path()) {
                        if e.kind() != io::ErrorKind::NotFound {
                            gone = false;
                            tracing::warn!(path = %entry.path().display(), error = %e, "store: removing entry file failed");
                        }
                    }
                }
            }
        }
    }
    gone
}

// ─────────────────────────────────────────────────────────────────────────────
// The reconciler: the ONE owner of every store mutation
// ─────────────────────────────────────────────────────────────────────────────

/// Everything the reconciler needs from the SERVER, behind one small trait.
///
/// It exists so [`run`] is generic and therefore testable with no network at all:
/// a test implementation returns scripted pin sets, scripted errors, and writes
/// scripted bytes into the temp path, which is what makes cadence, kick scoping,
/// backoff, and the skip-all-verdicts-on-transient rule provable under
/// `#[tokio::test(start_paused = true)]` rather than against a live server.
///
/// Every method returns `Result<_, String>`: the reconciler's response to ANY
/// failure is the same - keep the claim, back off, retry next pass - so a richer
/// error taxonomy here would buy nothing it could act on.
pub trait PinSource: Send + Sync + 'static {
    /// The authoritative pin set: starred songs, the tracks of starred albums, and
    /// the tracks of every album of every starred artist, as GROUPS.
    ///
    /// ALL OR NOTHING. An `Err` is TRANSIENT BY POLICY: the pass then skips ALL
    /// verdicts (nothing deleted, demoted, or marked stale because the server
    /// flapped), which is what "transient keeps the claim" means and is the whole of
    /// offline mode. That is exactly why a partial expansion must not be returned -
    /// 33 of 36 albums would look authoritative and demote the other three over one
    /// flaky `getAlbum`, then evict them next pass. A DEFINITIVE `NotFound` is
    /// different: that album is gone, so it expands to nothing and its tracks demote
    /// correctly.
    fn pins(&self) -> impl Future<Output = Result<PinSet, String>> + Send;

    /// Drop any memoised expansion, because the PIN SET ITSELF just changed.
    ///
    /// Sync and default-no-op: the memos exist only in the production source. The
    /// reconciler calls this on a full pass that a KICK asked for (a star or unstar
    /// is the only thing that kicks one), never on the interval tick and never on a
    /// re-entry - which is what keeps a 290-track backfill's ~73 chained passes at
    /// ONE expansion instead of 73.
    fn invalidate(&self) {}

    /// Fresh metadata for ONE id - the fingerprint a window or suspect download
    /// commits against, since those ids need not be in the pin set.
    fn song(&self, id: &SongId) -> impl Future<Output = Result<Song, String>> + Send;

    /// Stream `song`'s ORIGINAL into `tmp`, returning the byte count written.
    /// Bounded: a per-chunk inactivity timeout and a running size cap, never a
    /// whole-file buffer in memory. The caller owns `tmp`'s cleanup on failure.
    fn fetch(&self, song: &Song, tmp: &Path)
    -> impl Future<Output = Result<u64, String>> + Send;
}

/// The production [`PinSource`]: `getStarred2` / `getSong` through the shared
/// (timeout-bounded) metadata client, and originals through its OWN HTTP client.
///
/// Two clients on purpose. The metadata client carries a total per-request
/// timeout, which is right for a JSON round trip and fatal for a 60 MB FLAC on a
/// thin link; the audio client below has a connect timeout and NO total timeout,
/// bounded instead by per-chunk inactivity. Audio never rides the metadata client
/// and metadata never rides this one.
pub struct SubsonicPinSource<C: Clock> {
    client: Arc<SubsonicClient>,
    http: reqwest::Client,
    /// Mirrors `store.pin_starred`. When false the pin set is authoritatively
    /// EMPTY (not unknown): entries demote to evictable and only the queue window
    /// is mirrored, which is exactly what the knob promises.
    pin_starred: bool,
    /// The starred-buckets-to-groups EXPANSION and its two memos, behind the
    /// [`PinCatalog`] seam so all of it is testable with no server.
    expansion: PinExpansion<C, Arc<SubsonicClient>>,
}

/// The three CATALOGUE reads the pin expansion makes, behind one small seam.
///
/// [`PinSource`] is the wrong place to fake this: a test double THERE replaces the
/// expansion wholesale instead of exercising it, which left tier assignment, the
/// artist per-album fan-out, newest-first ordering, [`ARTIST_ALBUM_CAP`]
/// truncation, `NotFound`-as-a-definitive-empty, the all-or-nothing `Err` policy
/// the entire demote-safety argument rests on, and both memos with no test at all.
/// This seam is one level lower, so those are all provable without a network.
///
/// It speaks only hypodj's own model types, so `subsonic.rs` keeps its monopoly on
/// the wire.
pub(crate) trait PinCatalog: Send + Sync {
    /// `getStarred2`, decomposed into the three buckets.
    fn starred(&self) -> impl Future<Output = Result<Starred, SubsonicError>> + Send;
    /// One album's track list (`getAlbum`).
    fn album_songs(
        &self,
        id: &AlbumId,
    ) -> impl Future<Output = Result<Vec<Song>, SubsonicError>> + Send;
    /// One artist's albums (`getArtist`), in whatever order the server gave them.
    fn artist_albums(
        &self,
        id: &ArtistId,
    ) -> impl Future<Output = Result<Vec<Album>, SubsonicError>> + Send;
}

impl PinCatalog for Arc<SubsonicClient> {
    fn starred(&self) -> impl Future<Output = Result<Starred, SubsonicError>> + Send {
        SubsonicClient::starred(self)
    }

    fn album_songs(
        &self,
        id: &AlbumId,
    ) -> impl Future<Output = Result<Vec<Song>, SubsonicError>> + Send {
        SubsonicClient::album_songs(self, id)
    }

    fn artist_albums(
        &self,
        id: &ArtistId,
    ) -> impl Future<Output = Result<Vec<Album>, SubsonicError>> + Send {
        SubsonicClient::artist_albums(self, id)
    }
}

/// The pin EXPANSION: three starred buckets to an ordered [`PinSet`], plus the two
/// memos that make it affordable to ask for on every pass.
struct PinExpansion<C: Clock, K: PinCatalog> {
    catalog: K,
    /// The scheduling clock, so both memos below expire under
    /// `#[tokio::test(start_paused = true)]` and never against the wall clock.
    clock: C,
    /// THE CHAIN MEMO. A whole expanded pin set, good for
    /// [`PIN_SET_MEMO_TTL`].
    ///
    /// Without it a cold backfill is unaffordable: `PassReport::re_enter` repeats
    /// the same mode after every drained batch of four, so 290 tracks is ~73
    /// chained FULL passes, and a naive expansion is 43 round trips (~25-90 s) each
    /// time. Thirty seconds is far shorter than the 900 s full cadence, so it
    /// changes nothing about freshness on the interval - it only collapses the
    /// chain. Freshness on the GESTURE path is exact, because a star or unstar
    /// kicks a full pass and the reconciler calls [`PinSource::invalidate`] first.
    pin_set_memo: Mutex<Option<(Instant, PinSet)>>,
    /// THE EXPANSION MEMO, keyed by album id, good for [`ALBUM_MEMO_TTL`].
    ///
    /// Re-expanding every starred album on every full pass is 36 `getAlbum` calls
    /// against a server that answers in ~0.6 s each. The memo is invalidated early
    /// when the album's `song_count` from the current `getStarred2` disagrees with
    /// what was cached, and dropped for any album that left the starred set, so it
    /// cannot grow.
    album_memo: Mutex<HashMap<String, AlbumMemo>>,
}

/// One memoised album expansion.
struct AlbumMemo {
    at: Instant,
    /// The `song_count` the starred listing reported when this was cached. A
    /// disagreement forces a refetch before the TTL.
    song_count: u32,
    songs: Vec<Song>,
}

/// How long a whole expanded [`PinSet`] may be reused. Short on purpose: its only
/// job is to collapse the re-entry chain, not to cache anything.
pub const PIN_SET_MEMO_TTL: Duration = Duration::from_secs(30);

/// How long one album's track list may be reused.
///
/// THE HONEST COST: a track ADDED to a starred album is invisible to the mirror
/// for up to this long. The `song_count` check catches an addition but not a
/// replacement, `AlbumId3` carries no version field, and re-expanding 36 albums
/// every pass is 25-90 s of round trips on a pass that chains every four
/// downloads. This is a chosen bounded staleness, not an oversight.
pub const ALBUM_MEMO_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// Ceiling on how many `getAlbum` calls ONE starred artist may trigger per
/// expansion.
///
/// A RUNAWAY BRAKE ON ROUND TRIPS, not a statement about what "starred" means: a
/// starred artist still means every album they have, and the frontier - not this -
/// is what bounds the bytes. No real personal-library artist reaches it.
pub const ARTIST_ALBUM_CAP: usize = 100;

impl<C: Clock> SubsonicPinSource<C> {
    pub fn new(client: Arc<SubsonicClient>, pin_starred: bool, clock: C) -> Self {
        Self {
            http: build_download_http_client(),
            pin_starred,
            expansion: PinExpansion::new(client.clone(), clock),
            client,
        }
    }
}

impl<C: Clock, K: PinCatalog> PinExpansion<C, K> {
    fn new(catalog: K, clock: C) -> Self {
        Self {
            catalog,
            clock,
            pin_set_memo: Mutex::new(None),
            album_memo: Mutex::new(HashMap::new()),
        }
    }

    /// The memoised expansion: [`PIN_SET_MEMO_TTL`] of reuse, then a fresh walk.
    async fn pins(&self) -> Result<PinSet, String> {
        let now = self.clock.now();
        {
            let memo = self.pin_set_memo.lock().expect("store pin memo lock");
            if let Some((at, set)) = memo.as_ref() {
                if now.saturating_duration_since(*at) < PIN_SET_MEMO_TTL {
                    return Ok(set.clone());
                }
            }
        }
        let set = self.expand().await?;
        *self.pin_set_memo.lock().expect("store pin memo lock") = Some((now, set.clone()));
        Ok(set)
    }

    /// Drop BOTH memos, because the pin set itself just changed.
    fn invalidate(&self) {
        *self.pin_set_memo.lock().expect("store pin memo lock") = None;
        self.album_memo.lock().expect("store album memo lock").clear();
    }

    /// One album's tracks, from the memo when it is fresh and agrees with the
    /// starred listing's `song_count`, else from `getAlbum`.
    ///
    /// A definitive `NotFound` expands to NOTHING (the album is gone, so its tracks
    /// must demote); every other error propagates, which aborts the whole pin set.
    async fn album_tracks(&self, album: &Album) -> Result<Vec<Song>, String> {
        let now = self.clock.now();
        {
            let memo = self.album_memo.lock().expect("store album memo lock");
            if let Some(m) = memo.get(&album.id.0) {
                let fresh = now.saturating_duration_since(m.at) < ALBUM_MEMO_TTL;
                if fresh && m.song_count == album.song_count {
                    return Ok(m.songs.clone());
                }
            }
        }
        let songs = match self.catalog.album_songs(&album.id).await {
            Ok(v) => v,
            Err(SubsonicError::NotFound(_)) => {
                tracing::info!(album = %album.id.0, "store: starred album is gone; expanding it to nothing");
                Vec::new()
            }
            Err(e) => return Err(e.to_string()),
        };
        self.album_memo.lock().expect("store album memo lock").insert(
            album.id.0.clone(),
            AlbumMemo { at: now, song_count: album.song_count, songs: songs.clone() },
        );
        Ok(songs)
    }

    /// Expand the three starred buckets into groups. Every failure that is not a
    /// definitive `NotFound` aborts.
    async fn expand(&self) -> Result<PinSet, String> {
        let starred = self.catalog.starred().await.map_err(|e| e.to_string())?;
        let mut groups: Vec<PinGroup> = Vec::new();
        // Tier SONG: the per-track gesture, most specific and smallest.
        for s in &starred.songs {
            groups.push(PinGroup {
                kind: PinKind::Song,
                id: s.id.0.clone(),
                name: s.title.clone(),
                tier: PinTier::Song,
                songs: vec![s.clone()],
            });
        }
        // Tier ALBUM: one group per album, so it is held whole or not at all.
        let mut wanted: HashSet<String> = HashSet::new();
        for a in &starred.albums {
            wanted.insert(a.id.0.clone());
            let songs = self.album_tracks(a).await?;
            groups.push(PinGroup {
                kind: PinKind::Album,
                id: a.id.0.clone(),
                name: a.name.clone(),
                tier: PinTier::Album,
                songs,
            });
        }
        // Tier ARTIST: one group PER ALBUM, newest-first, so a huge catalogue
        // degrades album by album at the frontier instead of being refused whole.
        for artist in &starred.artists {
            let mut albums = match self.catalog.artist_albums(&artist.id).await {
                Ok(v) => v,
                Err(SubsonicError::NotFound(_)) => Vec::new(),
                Err(e) => return Err(e.to_string()),
            };
            sort_albums_newest_first(&mut albums);
            if albums.len() > ARTIST_ALBUM_CAP {
                tracing::warn!(
                    artist = %artist.id.0,
                    albums = albums.len(),
                    cap = ARTIST_ALBUM_CAP,
                    "store: starred artist has more albums than the per-expansion cap; taking the newest"
                );
                albums.truncate(ARTIST_ALBUM_CAP);
            }
            for a in &albums {
                wanted.insert(a.id.0.clone());
                let songs = self.album_tracks(a).await?;
                groups.push(PinGroup {
                    kind: PinKind::Album,
                    id: a.id.0.clone(),
                    name: format!("{} - {}", artist.name, a.name),
                    tier: PinTier::Artist,
                    songs,
                });
            }
        }
        // An album nobody wants any more can never be consulted again, so the memo
        // drops it here rather than growing for the life of the process.
        self.album_memo
            .lock()
            .expect("store album memo lock")
            .retain(|id, _| wanted.contains(id));
        Ok(PinSet { groups })
    }
}

/// Order an artist's albums NEWEST FIRST: `created` (the wire timestamp, ISO-8601
/// so a lexical compare is a chronological one), then `year`, then the id for
/// determinism. So a partly-resident artist keeps their recent work - the "keep me
/// current with them" reading - and the truncation is visible in the deferred list
/// rather than assumed.
fn sort_albums_newest_first(albums: &mut [Album]) {
    albums.sort_by(|a, b| {
        b.created
            .as_deref()
            .unwrap_or("")
            .cmp(a.created.as_deref().unwrap_or(""))
            .then_with(|| b.year.unwrap_or(0).cmp(&a.year.unwrap_or(0)))
            .then_with(|| a.id.0.cmp(&b.id.0))
    });
}

/// The store's audio HTTP client. Connect-bounded, redirect-bounded, and
/// deliberately WITHOUT a total request timeout - see [`SubsonicPinSource`].
fn build_download_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(DOWNLOAD_CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(4))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

impl<C: Clock> PinSource for SubsonicPinSource<C> {
    async fn pins(&self) -> Result<PinSet, String> {
        // AUTHORITATIVELY EMPTY, not unknown. This is the one path where the
        // Vec -> struct change could have silently started returning "no
        // information" and stopped demoting anything, so it is an explicit early
        // return with its own test.
        if !self.pin_starred {
            return Ok(PinSet::default());
        }
        self.expansion.pins().await
    }

    fn invalidate(&self) {
        self.expansion.invalidate();
    }

    async fn song(&self, id: &SongId) -> Result<Song, String> {
        self.client.song(id).await.map_err(|e| e.to_string())
    }

    async fn fetch(&self, song: &Song, tmp: &Path) -> Result<u64, String> {
        let url = self
            .client
            .download_url(&song.id)
            .map_err(|e| e.to_string())?;
        // No server-reported size means no commit gate downstream, so there is no
        // point spending a download: refuse here where it is cheap.
        let size = song
            .size
            .filter(|s| *s > 0)
            .ok_or_else(|| format!("no server-reported size for {:?}", song.id.0))?;
        fetch_original(&self.http, url.as_str(), tmp, size).await
    }
}

/// Stream one original into `tmp` with the bounded chunk loop.
///
/// Bounded three ways, none of which is a total-request timeout:
///
/// - a PER-CHUNK inactivity timeout, so a stalled socket dies in seconds while a
///   slow-but-alive transfer runs to completion;
/// - a RUNNING size cap of `expected + slack`, so a lying or unbounded body can
///   never fill the disk (it dies mid-stream, not after);
/// - an EXACT-length gate at EOF, so a truncated transfer is an error here rather
///   than something [`AudioStore::commit`] has to catch (it re-checks anyway - one
///   gate every path passes).
///
/// The writes are sync `std::fs` per chunk, which is the design's deliberate
/// choice: the workspace tokio has no `fs` feature, the chunks arrive at network
/// pace, and the alternative (buffering the file) is precisely the whole-file RAM
/// spike this exists to avoid.
async fn fetch_original(
    http: &reqwest::Client,
    url: &str,
    tmp: &Path,
    expected: u64,
) -> Result<u64, String> {
    let cap = expected.saturating_add(DOWNLOAD_SIZE_SLACK);
    let mut resp = tokio::time::timeout(DOWNLOAD_CHUNK_TIMEOUT, http.get(url).send())
        .await
        .map_err(|_| "download: timed out connecting".to_string())?
        .map_err(|e| format!("download: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("download: HTTP {}", resp.status()));
    }
    // A declared length that already disagrees is a wasted transfer: refuse before
    // a single byte lands.
    if let Some(len) = resp.content_length() {
        if len != expected {
            return Err(format!(
                "download: server declared {len} bytes, expected {expected}"
            ));
        }
    }
    let mut file = File::create(tmp).map_err(|e| format!("download: creating temp: {e}"))?;
    let mut written: u64 = 0;
    loop {
        let chunk = tokio::time::timeout(DOWNLOAD_CHUNK_TIMEOUT, resp.chunk())
            .await
            .map_err(|_| "download: stalled".to_string())?
            .map_err(|e| format!("download: {e}"))?;
        let Some(chunk) = chunk else { break };
        written = written.saturating_add(chunk.len() as u64);
        if written > cap {
            return Err(format!("download: body exceeded {cap} bytes; abandoned"));
        }
        file.write_all(&chunk)
            .map_err(|e| format!("download: writing temp: {e}"))?;
    }
    file.flush().map_err(|e| format!("download: flushing temp: {e}"))?;
    if written != expected {
        return Err(format!(
            "download: got {written} bytes, server reported {expected}"
        ));
    }
    Ok(written)
}

/// Per-id retry pacing for failed downloads. In memory only, owned by the
/// reconciler task - so it needs no lock, and a restart is deliberately allowed to
/// try again at once (a fresh process is the strongest evidence that whatever
/// failed may not any more).
#[derive(Default)]
struct Backoff {
    /// id -> (consecutive failures, the instant it may be retried).
    entries: HashMap<SongId, (u32, tokio::time::Instant)>,
}

/// Why an id is not being attempted right now.
#[derive(Debug, PartialEq, Eq)]
enum Attempt {
    /// Go ahead.
    Now,
    /// Still inside its backoff window.
    Waiting,
    /// Given up on for this process - see [`DOWNLOAD_GIVE_UP_AFTER`]. Distinct from
    /// `Waiting` because it is reported ONCE and then stays silent, instead of a warn
    /// on every pass forever for a thing that will never change.
    GivenUp,
}

impl Backoff {
    /// Whether `id` may be attempted now.
    fn ready(&self, id: &SongId, now: tokio::time::Instant) -> bool {
        matches!(self.attempt(id, now), Attempt::Now)
    }

    /// Whether `id` may be attempted, and if not, WHY - so the caller can log a
    /// give-up once rather than a wait every pass.
    fn attempt(&self, id: &SongId, now: tokio::time::Instant) -> Attempt {
        match self.entries.get(id) {
            Some((failures, _)) if *failures >= DOWNLOAD_GIVE_UP_AFTER => Attempt::GivenUp,
            Some((_, not_before)) if now < *not_before => Attempt::Waiting,
            _ => Attempt::Now,
        }
    }

    /// Has `id` just crossed into given-up on this failure? True exactly once per id,
    /// which is what keeps the warn from repeating.
    fn just_gave_up(&self, id: &SongId) -> bool {
        self.entries.get(id).map(|(n, _)| *n) == Some(DOWNLOAD_GIVE_UP_AFTER)
    }

    /// Record a failure and push the next attempt out, doubling per consecutive
    /// failure up to [`DOWNLOAD_BACKOFF_MAX`].
    fn fail(&mut self, id: &SongId, now: tokio::time::Instant) {
        let failures = self.entries.get(id).map(|(n, _)| *n).unwrap_or(0) + 1;
        let delay = DOWNLOAD_BACKOFF_BASE
            .saturating_mul(1u32 << failures.min(8).saturating_sub(1))
            .min(DOWNLOAD_BACKOFF_MAX);
        self.entries.insert(id.clone(), (failures, now + delay));
    }

    /// Forget `id`'s history: it succeeded.
    fn succeed(&mut self, id: &SongId) {
        self.entries.remove(id);
    }
}

/// How many times in a row the reconciler may re-enter on eviction alone - that
/// is, without a single committed download to show for it - before it must wait
/// for the next kick or tick.
///
/// The BACKSTOP behind the progress gate, not the primary bound. A pass evicts
/// down to the budget in one go and only counts entries whose bytes actually went,
/// so a healthy store never chains more than once; this cap is what keeps a
/// filesystem that lies about reclamation from turning a full pass - a whole
/// directory scan plus a `getStarred2` round trip - into a hot loop against the
/// server. Download chains are deliberately NOT capped by it: each of those links
/// is a committed original, self-limiting because the desired set is finite, and
/// capping them would stall a cold backfill for a whole interval.
const MAX_EVICTION_CHAIN: usize = 8;

/// What one pass did, enough to decide whether to re-enter immediately.
#[derive(Default, Debug, PartialEq, Eq)]
struct PassReport {
    /// Downloads the plan scheduled (before backoff filtering).
    scheduled: usize,
    /// Downloads skipped because the id is given up on for this process. Counted so a
    /// pass that "did nothing" is distinguishable from one that did nothing BECAUSE
    /// something is permanently broken.
    given_up: usize,
    /// Downloads that committed.
    committed: usize,
    /// Entries whose bytes were ACTUALLY reclaimed - an eviction whose unlink
    /// failed is not counted, because it freed nothing and the next scan will find
    /// the very same entry over budget again.
    evicted: usize,
    /// WHAT THIS PASS ACTUALLY BUDGETED AGAINST, after the free-space clamp.
    ///
    /// Reported because it is the one number that makes "hypodj cannot fill the
    /// disk" checkable per pass: `0` means the reserve is breached and the pass
    /// wrote nothing at all, and anything below the configured cap means the clamp
    /// ran. LIGHT passes carry it too, which is the point - they are the ones that
    /// fire at every track boundary, and a light pass that skipped the measurement
    /// would keep downloading window originals onto a disk a full pass had already
    /// declared unwritable.
    budget: u64,
}

impl PassReport {
    /// Whether the reconciler should run ANOTHER pass right away rather than sleep.
    ///
    /// Gated on real PROGRESS, never on outstanding work: a batch that filled up and
    /// landed something means there is very likely more of the same waiting, while a
    /// batch that landed nothing must wait for the backoff or the interval - without
    /// that gate a permanently failing download would spin the task at full speed
    /// forever. Eviction also re-enters, so headroom it just reclaimed becomes
    /// usable immediately instead of at the next tick; it terminates because a pass
    /// evicts down to the budget in one go, because only bytes that really went are
    /// counted, and - `chain` links deep - because of [`MAX_EVICTION_CHAIN`].
    ///
    /// `chain` is how many eviction-only re-entries have already run back to back;
    /// the caller resets it whenever a pass drains a full batch or the loop sleeps.
    fn re_enter(&self, batch: usize, chain: usize) -> bool {
        self.drained_a_full_batch(batch) || (self.evicted > 0 && chain < MAX_EVICTION_CHAIN)
    }

    /// Whether this pass filled its download batch AND landed something - the
    /// "there is very likely more of the same waiting" signal, and the one kind of
    /// re-entry [`MAX_EVICTION_CHAIN`] does not count against.
    fn drained_a_full_batch(&self, batch: usize) -> bool {
        self.committed > 0 && self.scheduled >= batch
    }
}

/// THE RECONCILER: the single owner of every store mutation, for the daemon's
/// lifetime.
///
/// It diffs a DESIRED set (the starred pins plus the live queue window) against
/// sidecar-committed on-disk truth and closes the gap. Crash recovery, external
/// tampering, server drift, and cold start are therefore all one code path - the
/// next pass - rather than four.
///
/// Two cadences, which is what keeps the feature proportionate:
///
/// - a FULL pass on the interval, on a star flip, and at startup: sweep temps,
///   scan the directory, one `getStarred2`, fingerprint verdicts, downloads,
///   recency flush, eviction;
/// - a LIGHT pass on every other kick (a queue-window change, a suspect mark, a
///   skip-target pin): replan against CACHED state only and execute nothing but
///   the window / suspect downloads. A track boundary costs zero network beyond
///   the bytes it actually needs.
///
/// Generic over [`Clock`] and [`PinSource`] so the whole loop runs under
/// `#[tokio::test(start_paused = true)]` with no network and no wall clock.
/// Deadlines are ABSOLUTE (`t0 + k*interval`, the clock.rs convention), so a long
/// pass shortens the next sleep instead of drifting the schedule.
///
/// Never returns. Every failure inside is logged and retried on a later pass -
/// there is no error the reconciler could report to anybody.
pub async fn run<C: Clock, P: PinSource>(store: Arc<AudioStore>, source: Arc<P>, clock: C) {
    let interval = Duration::from_secs(store.config().sync_interval_secs.max(1));
    let batch = DOWNLOAD_BATCH;
    let mut backoff = Backoff::default();
    // Startup IS a full pass: the directory may have moved under a dead process,
    // and nothing about the pin set is known yet.
    let mut next_mode = Some(PassMode::Full);
    let mut next_full = clock.now();
    // Consecutive re-entries that reclaimed but downloaded nothing, bounded by
    // MAX_EVICTION_CHAIN.
    let mut eviction_chain = 0usize;
    tracing::info!(
        interval_secs = interval.as_secs(),
        max_bytes = store.config().max_bytes,
        "store: reconciler started"
    );
    loop {
        let mode = match next_mode.take() {
            Some(m) => m,
            None => {
                tokio::select! {
                    _ = clock.sleep_until(next_full) => PassMode::Full,
                    _ = store.kicked() => {
                        // A full kick that landed during the previous pass is still
                        // honored here: the flag is sticky until taken.
                        if store.take_full_request() {
                            // A full KICK means the pin set itself may have changed
                            // - a star or unstar is the only thing that fires one -
                            // so the memoised expansion must go. The interval tick
                            // and the re-entry chain deliberately do NOT invalidate:
                            // that is what keeps a cold backfill at one expansion
                            // instead of one per drained batch.
                            source.invalidate();
                            PassMode::Full
                        } else {
                            PassMode::Light
                        }
                    }
                }
            }
        };
        if mode == PassMode::Full {
            // Absolute deadline from NOW, so a pass that ran long does not
            // immediately trigger the next tick, and a run of light passes never
            // delays the full cadence.
            next_full = clock.now() + interval;
            // A full pass subsumes any pending full request.
            store.take_full_request();
        }
        let report = run_pass(&store, source.as_ref(), mode, &clock, &mut backoff, batch).await;
        if report.re_enter(batch, eviction_chain) {
            eviction_chain =
                if report.drained_a_full_batch(batch) { 0 } else { eviction_chain + 1 };
            next_mode = Some(mode);
        } else {
            if report.evicted > 0 && eviction_chain >= MAX_EVICTION_CHAIN {
                // Reached only when eviction keeps reporting reclaimed bytes that
                // never reduce the pass's work - a filesystem misbehaving in a way
                // the unlink itself did not report. Degrade to the ordinary cadence
                // and say so, rather than scanning and polling the server in a loop.
                tracing::warn!(
                    chain = eviction_chain,
                    "store: evictions re-entered {MAX_EVICTION_CHAIN} passes in a row without draining a download batch; waiting for the next kick or tick"
                );
            }
            eviction_chain = 0;
        }
    }
}

/// Run exactly ONE pass: observe, plan (purely), execute in the planned order.
async fn run_pass<C: Clock, P: PinSource>(
    store: &Arc<AudioStore>,
    source: &P,
    mode: PassMode,
    clock: &C,
    backoff: &mut Backoff,
    batch: usize,
) -> PassReport {
    let full = mode == PassMode::Full;
    let configured = store.config().max_bytes;
    let mut input = PassInput::new(mode, configured);
    input.download_batch = batch;
    input.defer_bulk = store.playback_remote();
    input.paused = store.paused();
    // CALENDAR NOW, read ONCE here at the impure boundary - beside the statvfs, for
    // the same reason - and threaded into the plan. `plan_pass` therefore stays pure
    // and the frontier's neglect ranking is testable at any chosen date.
    input.now_unix = now_unix();

    {
        // THE CEILING, re-measured EVERY pass, light ones included. `own` is the
        // store's own bytes, so `(avail + own)` is INVARIANT to hypodj's own
        // eviction: hypodj's actions cannot move hypodj's ceiling, only another
        // process's can. That is what makes the post-eviction budget identical to
        // the pre-eviction one and the state settle instead of oscillate.
        //
        // LIGHT PASSES MEASURE TOO, and that is the whole of "a breached reserve
        // writes nothing at all". `plan_pass` gates every write on
        // `input.max_bytes > 0`, and a light kick fires at EVERY track boundary and
        // queue edit - so leaving `max_bytes` at the configured cap here would let
        // the window arm keep writing 31-415 MiB originals onto a disk the full pass
        // has already declared unwritable, once per track, only for the next full
        // pass to delete them again. One `statvfs` on a blocking thread is the price
        // of the guarantee being true on both paths rather than one.
        let own = store
            .entries()
            .iter()
            .fold(0u64, |a, e| a.saturating_add(e.size));
        let root = store.root().to_path_buf();
        let space = tokio::task::spawn_blocking(move || statvfs_space(&root))
            .await
            .ok()
            .flatten();
        match space.and_then(|(avail, total)| {
            derive_budget(avail, total, own, configured).map(|max| (avail, total, max))
        }) {
            Some((avail, total, max)) => {
                input.max_bytes = max;
                input.budget_source = BudgetSource::FreeSpace;
                input.reserve = budget_reserve(total);
                input.avail = avail;
                input.fs_total = total;
            }
            // NEVER to unlimited: an unmeasured disk is not an empty one.
            None => {
                if full {
                    // Full only: a light kick lands at every track boundary, so
                    // warning here would turn one unmeasurable disk into a log storm
                    // saying the same thing. The fallback is identical either way.
                    tracing::warn!(
                        root = %store.root().display(),
                        "store: could not measure free space; falling back to the configured budget"
                    );
                }
                input.budget_source = BudgetSource::Config;
            }
        }
    }

    if full {
        // The directory read is the one genuinely slow observation in a pass (it
        // stats every entry), so it goes to a blocking thread rather than parking a
        // runtime worker - the daemon runs on two.
        let root = store.root().to_path_buf();
        match tokio::task::spawn_blocking(move || scan_dir(&root)).await {
            Ok(Ok(scan)) => {
                // Disk truth becomes the index, carrying the in-memory-only flags
                // across, BEFORE the plan reads entries - so the plan is built
                // against what is really there.
                store.resync_from_scan(scan.entries.clone());
                input.orphan_audio = scan.orphan_audio;
                input.orphan_sidecars = scan.orphan_sidecars;
                input.stale_tmps = scan.stale_tmps;
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "store: directory scan failed; skipping housekeeping");
            }
            Err(e) => {
                tracing::warn!(error = %e, "store: scan task failed; skipping housekeeping");
            }
        }
        // ONE getStarred2 per full pass revalidates the ENTIRE pinned mirror for
        // free. A failure leaves `pins` at None, which makes plan_pass skip every
        // verdict: nothing deleted, demoted, or marked stale because the server
        // flapped.
        match source.pins().await {
            Ok(pins) => {
                input.pins = Some(pins);
                // The server answered: this is the "back online" edge the daemon uses
                // to refresh the id-only placeholders an offline restore installed.
                // Fired before the plan so the (cosmetic) refresh overlaps the pass.
                store.fire_server_back();
            }
            Err(e) => {
                tracing::info!(error = %e, "store: pin set unavailable this pass; keeping every claim");
            }
        }
    }

    input.entries = store.entries();
    input.window = store.window();
    input.protected = store.protected_ids();

    let (actions, status) = plan_pass_with_status(&input);
    let mut report = execute(store, source, &input, actions, clock, backoff).await;
    report.budget = input.max_bytes;
    if let Some(mut status) = status {
        // The one number the plan cannot know: ids this PROCESS has stopped
        // retrying. Without it a pending count that will never reach zero is
        // indistinguishable from one that is merely slow.
        status.given_up = report.given_up;
        let before = store.status();
        if !before.known || before.digest() != status.digest() {
            tracing::info!(
                resident = status.resident_tracks,
                cached = status.cached_tracks,
                pending = status.pending_tracks,
                deferred = status.deferred_count(),
                bytes = status.bytes,
                budget = status.effective_max,
                source = status.budget_source.label(),
                waiting = status.waiting.label(),
                rule = %frontier_rule(),
                "store: mirror frontier"
            );
            // The shortfall carries its REASON into the journal too, so the mirror
            // explains itself with no client attached.
            for d in status.deferred() {
                tracing::info!(
                    uri = %d.uri(),
                    name = %d.name,
                    tier = d.tier.label(),
                    rank = d.rank,
                    tracks = d.missing_tracks,
                    bytes = d.missing_bytes,
                    cold_decile = d.cold_decile,
                    cold_tracks = d.cold_tracks,
                    never_played = d.never_played,
                    plays = d.plays,
                    last_played_days = ?d.last_played_days,
                    over_by = d.over_by,
                    blocked_by = ?d.blocked_by,
                    "store: deferred, over the pin ceiling"
                );
            }
        }
        store.publish_status(status);
    }
    report
}

/// Execute a planned pass front to back. The order is [`plan_pass`]'s, and it is
/// load-bearing (see its docs) - this walks the list, it never reorders it.
async fn execute<C: Clock, P: PinSource>(
    store: &Arc<AudioStore>,
    source: &P,
    input: &PassInput,
    actions: Vec<StoreAction>,
    clock: &C,
    backoff: &mut Backoff,
) -> PassReport {
    let mut report = PassReport::default();
    // The pin set, for the pinned flag a fresh commit records and for choosing the
    // freshest fingerprint. Absent on a light pass and on a transient failure.
    let pin_by_id: HashMap<&str, &Song> = match &input.pins {
        Some(pins) => {
            let mut m: HashMap<&str, &Song> = HashMap::new();
            for p in pins.songs() {
                m.entry(p.id.0.as_str()).or_insert(p);
            }
            m
        }
        None => HashMap::new(),
    };
    let existing_pinned: HashSet<&str> = input
        .entries
        .iter()
        .filter(|e| e.pinned)
        .map(|e| e.id.0.as_str())
        .collect();

    for action in actions {
        match action {
            StoreAction::SweepTmp(path) | StoreAction::DeleteFile(path) => {
                let p = path.clone();
                let _ = tokio::task::spawn_blocking(move || std::fs::remove_file(&p)).await;
                tracing::debug!(path = %path.display(), "store: removed a loose file");
            }
            StoreAction::DeleteEntry { id, reason } => {
                tracing::info!(id = %id.0, ?reason, "store: dropping an invalid entry");
                let (s, i) = (store.clone(), id.clone());
                let _ = tokio::task::spawn_blocking(move || s.remove_entry(&i)).await;
            }
            StoreAction::SetStale { id, stale } => {
                let (s, i) = (store.clone(), id.clone());
                match tokio::task::spawn_blocking(move || s.set_stale(&i, stale)).await {
                    Ok(Ok(())) => tracing::info!(id = %id.0, stale, "store: fingerprint verdict"),
                    Ok(Err(e)) => {
                        tracing::warn!(id = %id.0, error = %e, "store: marking stale failed")
                    }
                    Err(e) => tracing::warn!(id = %id.0, error = %e, "store: stale task failed"),
                }
            }
            StoreAction::SetPinned { id, pinned } => {
                let (s, i) = (store.clone(), id.clone());
                match tokio::task::spawn_blocking(move || s.set_pinned(&i, pinned)).await {
                    Ok(Ok(())) => tracing::info!(id = %id.0, pinned, "store: pin state changed"),
                    Ok(Err(e)) => {
                        tracing::warn!(id = %id.0, error = %e, "store: repinning failed")
                    }
                    Err(e) => tracing::warn!(id = %id.0, error = %e, "store: pin task failed"),
                }
            }
            StoreAction::Download { id, reason } => {
                // PLAYBACK STARTED MID-BATCH. `defer_bulk` is sampled once per pass,
                // so without this re-check the rest of a four-deep backfill batch
                // keeps pulling originals while he is listening on a thin link -
                // which is precisely what the deferral exists to prevent. Suspect
                // and Window continue: those ARE what he is about to hear.
                if matches!(reason, DownloadReason::Backfill | DownloadReason::Stale)
                    && (store.playback_remote() || store.paused())
                {
                    tracing::debug!(id = %id.0, ?reason, "store: bulk work yielding mid-pass");
                    continue;
                }
                report.scheduled += 1;
                match backoff.attempt(&id, clock.now()) {
                    Attempt::Waiting => {
                        tracing::debug!(id = %id.0, ?reason, "store: download still backing off");
                        continue;
                    }
                    // Already reported when it crossed the line. Staying silent here is
                    // the point: the old behaviour warned on every pass forever about a
                    // condition that cannot change, which buried everything else.
                    Attempt::GivenUp => {
                        tracing::debug!(id = %id.0, ?reason, "store: download given up on");
                        report.given_up += 1;
                        continue;
                    }
                    Attempt::Now => {}
                }
                {
                }
                // The fingerprint MUST come from this pass's own server data, never
                // a cached copy - committing against a stale size is how a wrong
                // file would pass the length gate.
                let song = match pin_by_id.get(id.0.as_str()) {
                    Some(p) => (*p).clone(),
                    None => match source.song(&id).await {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::info!(id = %id.0, error = %e, "store: metadata unavailable; deferring download");
                            backoff.fail(&id, clock.now());
                            continue;
                        }
                    },
                };
                let pinned = if input.pins.is_some() {
                    pin_by_id.contains_key(id.0.as_str())
                } else {
                    existing_pinned.contains(id.0.as_str())
                };
                match download_and_commit(store, source, &song, pinned).await {
                    Ok(path) => {
                        backoff.succeed(&id);
                        report.committed += 1;
                        // Bulk backfill is a 290-track, multi-day background job:
                        // one info line per track is a wall nobody reads. The
                        // progress signal is the frontier line and the X-Store
                        // badge, both of which move. Everything the user is about to
                        // HEAR still says so at info.
                        if reason == DownloadReason::Backfill {
                            tracing::debug!(id = %id.0, ?reason, path = %path.display(), "store: committed");
                        } else {
                            tracing::info!(id = %id.0, ?reason, path = %path.display(), "store: committed");
                        }
                    }
                    Err(e) => {
                        backoff.fail(&id, clock.now());
                        if backoff.just_gave_up(&id) {
                            // ONCE, loudly, naming the id and the reason - because this
                            // is the moment a starred song becomes permanently absent
                            // from the offline mirror, and it must not be discoverable
                            // only by noticing the mirror is one short.
                            tracing::warn!(
                                id = %id.0, ?reason, error = %e,
                                attempts = DOWNLOAD_GIVE_UP_AFTER,
                                "store: giving up on this download for now; it will be \
                                 retried after a restart"
                            );
                        } else {
                            tracing::warn!(id = %id.0, ?reason, error = %e, "store: download failed; will retry");
                        }
                    }
                }
            }
            StoreAction::FlushRecency { id, last_played_unix } => {
                let (s, i) = (store.clone(), id.clone());
                match tokio::task::spawn_blocking(move || s.flush_recency(&i, last_played_unix))
                    .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::debug!(id = %id.0, error = %e, "store: recency flush failed")
                    }
                    Err(e) => tracing::warn!(id = %id.0, error = %e, "store: recency task failed"),
                }
            }
            StoreAction::Evict(id) => {
                tracing::info!(id = %id.0, "store: evicting the coldest unpinned entry");
                let (s, i) = (store.clone(), id.clone());
                match tokio::task::spawn_blocking(move || s.remove_entry(&i)).await {
                    // ONLY a real reclamation counts. An unlink the filesystem
                    // refused freed nothing, so calling it progress would have the
                    // reconciler re-enter on the very same over-budget entry - a
                    // tight loop of full directory scans and getStarred2 round
                    // trips. `remove_pair` already warned with the io error.
                    Ok(true) => report.evicted += 1,
                    Ok(false) => tracing::warn!(
                        id = %id.0,
                        "store: eviction reclaimed nothing; leaving it to the next pass"
                    ),
                    Err(e) => tracing::warn!(id = %id.0, error = %e, "store: eviction task failed"),
                }
            }
        }
    }
    report
}

/// Fetch one original into a fresh temp and COMMIT it, or clean up and report why.
///
/// The commit is [`AudioStore::commit`] - sync on purpose, so it runs in
/// `spawn_blocking` - and it is what atomically publishes the bytes: until its
/// sidecar rename lands, the download is an orphan the next scan removes and
/// playback can never be offered it. A failed fetch removes its own temp, so a
/// dead link leaves nothing behind either.
async fn download_and_commit<P: PinSource>(
    store: &Arc<AudioStore>,
    source: &P,
    song: &Song,
    pinned: bool,
) -> Result<PathBuf, String> {
    let tmp = store.tmp_path();
    if let Err(e) = source.fetch(song, &tmp).await {
        let doomed = tmp.clone();
        let _ = tokio::task::spawn_blocking(move || std::fs::remove_file(&doomed)).await;
        return Err(e);
    }
    let (s, song, tmp2) = (store.clone(), song.clone(), tmp.clone());
    match tokio::task::spawn_blocking(move || s.commit(&song, &tmp2, pinned, now_unix())).await {
        Ok(Ok(path)) => Ok(path),
        // `commit` removes the temp itself on every refusal, so there is nothing
        // left to clean here.
        Ok(Err(e)) => Err(e.to_string()),
        Err(e) => {
            let doomed = tmp.clone();
            let _ = tokio::task::spawn_blocking(move || std::fs::remove_file(&doomed)).await;
            Err(format!("commit task failed: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ADMISSION and DELETION are different questions. Admission stays strict - that is
    // what makes "cannot fill the disk" structural. Deletion must NOT track the
    // free-space budget one-for-one, because that budget shrinks with every unrelated
    // byte the machine writes: measured on this machine, target/ alone swings 11 GiB,
    // which takes the derived budget from 10.5 GiB to under 1 and would evict almost
    // the whole mirror - then cleaning the build re-downloads ten gigabytes.
    #[test]
    fn a_tight_disk_does_not_delete_the_mirror_but_a_critical_one_does() {
        let plenty = 40 * 1024 * 1024 * 1024;
        // Budget squeezed to almost nothing by an unrelated build, disk still fine:
        // keep what we already hold rather than thrash.
        let configured = 16 * 1024 * 1024 * 1024;
        assert_eq!(
            evict_target(100 * 1024 * 1024, configured, plenty),
            STORE_EVICT_FLOOR,
            "a transient squeeze must not reclaim below the floor"
        );
        // Budget above the floor: the budget governs, as before.
        let big = 8 * 1024 * 1024 * 1024;
        assert_eq!(evict_target(big, configured, plenty), big);
        // Genuinely critical: the floor is abandoned and the mirror gets out of the way.
        assert_eq!(
            evict_target(0, configured, 1024 * 1024 * 1024),
            0,
            "below the critical mark the disk needs the space more than the music does"
        );
        assert_eq!(evict_target(0, configured, STORE_CRITICAL_AVAIL), 0, "boundary is inclusive");
    }

    // A flat floor would silently disable LRU for any store smaller than it, which is
    // how the first version of this fix broke an existing eviction test.
    #[test]
    fn the_floor_never_overrides_a_deliberately_small_configured_cap() {
        let plenty = 40 * 1024 * 1024 * 1024;
        let small = 64 * 1024 * 1024;
        assert_eq!(
            evict_target(small, small, plenty),
            small,
            "a 64 MiB store still evicts at 64 MiB, not at the 2 GiB floor"
        );
    }

    #[test]
    fn the_eviction_floor_never_lets_the_store_grow() {
        // The floor only stops DELETION. Admission is a separate gate, so a zero
        // budget still writes nothing no matter what the floor says - that is what
        // keeps the disk guarantee structural rather than a hope.
        assert_eq!(derive_budget(0, 1_000_000_000_000, 0, u64::MAX), Some(0));
    }

    // Filesystem-and-pure-logic only: no clock, no network, no libmpv. Safe in the
    // certless, network-less Nix sandbox.

    /// A fresh, uniquely named temp dir for one test. No `tempfile` dependency
    /// (this feature adds zero deps); uniqueness comes from pid + a process-wide
    /// counter so parallel tests cannot collide.
    /// An ESTABLISHED store directory: created empty and already carrying the
    /// ownership marker, which is what every test that pre-places entries with
    /// [`place`] needs (an unmarked non-empty directory is refused by `open`, on
    /// purpose - see [`unowned_tmpdir`]).
    fn tmpdir(tag: &str) -> PathBuf {
        let dir = unowned_tmpdir(tag);
        std::fs::write(dir.join(STORE_MARKER_NAME), STORE_MARKER_BODY).expect("mark test store");
        dir
    }

    /// A fresh directory with NO ownership marker: what a user's state dir or music
    /// folder looks like to `open`.
    fn unowned_tmpdir(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("hypodj-store-{tag}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn sid(id: &str) -> SongId {
        SongId(id.to_string())
    }

    /// A song carrying a full store fingerprint, as `map_song` would produce it.
    fn song(id: &str, size: u64, suffix: &str, created: Option<&str>) -> Song {
        Song {
            id: sid(id),
            title: format!("t-{id}"),
            album: Some("al".into()),
            album_id: None,
            artist: Some("ar".into()),
            track: Some(3),
            duration_secs: Some(210),
            cover_art: None,
            starred: true,
            musicbrainz_id: None,
            disc: None,
            year: Some(2019),
            genre: Some("Electronic".into()),
            bitrate: Some(1000),
            comment: None,
            user_rating: None,
            composer: None,
            performer: None,
            size: Some(size),
            suffix: Some(suffix.into()),
            content_type: Some("audio/flac".into()),
            created: created.map(|c| c.to_string()),
            play_count: Some(7),
            played: Some("2026-08-06T14:17:24+01:00".into()),
        }
    }

    fn entry(id: &str, size: u64, last_played: u64) -> IndexEntry {
        IndexEntry {
            id: sid(id),
            suffix: "flac".into(),
            size,
            created: Some("2024-05-01T12:00:00Z".into()),
            pinned: false,
            stale: false,
            suspect: false,
            last_played_unix: last_played,
            recency_dirty: false,
        }
    }

    /// Write a valid committed pair for `s` directly, bypassing `commit`, so a scan
    /// test can construct arbitrary on-disk states.
    fn place(root: &Path, s: &Song, pinned: bool, stale: bool, last_played: u64) {
        let size = s.size.expect("fixture size");
        let suffix = sanitize_suffix(s.suffix.as_deref());
        std::fs::write(root.join(format!("{}.{}", s.id.0, suffix)), vec![b'x'; size as usize])
            .expect("write audio");
        let sc = Sidecar {
            schema_version: STORE_SCHEMA_VERSION,
            endpoint: ENDPOINT_DOWNLOAD.to_string(),
            content_type: s.content_type.clone(),
            pinned,
            stale,
            fetched_at_unix: 1_700_000_000,
            last_played_unix: last_played,
            fingerprint: Fingerprint {
                size,
                suffix,
                created: s.created.clone(),
            },
            song: s.clone(),
        };
        std::fs::write(root.join(format!("{}.toml", s.id.0)), sidecar_to_toml(&sc))
            .expect("write sidecar");
    }

    /// The store artifacts in `dir`. The ownership marker is excluded because it is
    /// scaffolding present in EVERY case, not a fact about the heal being asserted;
    /// that it survives the heal is proved on its own in
    /// [`the_ownership_marker_survives_the_heal_it_authorizes`].
    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|n| n != STORE_MARKER_NAME)
            .collect();
        v.sort();
        v
    }

    fn store(dir: &Path, max_bytes: u64) -> AudioStore {
        let mut cfg = StoreConfig::default();
        cfg.max_bytes = max_bytes;
        AudioStore::open(dir.to_path_buf(), cfg).expect("open store")
    }

    // ── keys, suffixes, temp names ──────────────────────────────────────────

    #[test]
    fn is_storable_id_admits_only_path_safe_ids() {
        // Navidrome ids are hex/uuid-ish, but `_` and `-` are common enough in other
        // servers that excluding them would silently disable the store for a whole
        // installation.
        for ok in ["a", "so-1", "SO_1", "0", "abc123", "a-b_c-9", "-", "_"] {
            assert!(is_storable_id(ok), "{ok:?} must be storable");
        }
        // Every rejection is a concrete escape or collision the store then does NOT
        // have to defend against at every later layer: path traversal, a nested
        // path, the `<id>.<suffix>` filename split, and the mpv `quote()` trap.
        for bad in [
            "", "..", ".", "a/b", "/abs", "a.b", "a b", "a\"b", "a'b", "a\nb", "a\\b", "a;b",
            "cafe\u{301}", "a*", "a?", "a\0b", "$(id)", "~",
        ] {
            assert!(!is_storable_id(bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn sanitize_suffix_table_including_the_toml_collision() {
        assert_eq!(sanitize_suffix(Some("flac")), "flac");
        assert_eq!(sanitize_suffix(Some("mp3")), "mp3");
        assert_eq!(sanitize_suffix(Some("MP3")), "mp3", "lowercased");
        assert_eq!(sanitize_suffix(Some("  ogg  ")), "ogg", "trimmed");
        assert_eq!(sanitize_suffix(Some("m4a")), "m4a");
        // `<id>.toml` IS the sidecar: a song whose suffix were `toml` would have
        // its audio collide with its own commit record.
        assert_eq!(sanitize_suffix(Some("toml")), FALLBACK_SUFFIX);
        assert_eq!(sanitize_suffix(Some("TOML")), FALLBACK_SUFFIX);
        // Path escapes, separators, dots, and over-long values all fall back.
        for bad in [
            "", "  ", "../../etc/passwd", "a/b", "fl.ac", "waytoolongsuffix", "f l", "f\"",
        ] {
            assert_eq!(sanitize_suffix(Some(bad)), FALLBACK_SUFFIX, "{bad:?}");
        }
        assert_eq!(sanitize_suffix(None), FALLBACK_SUFFIX);
        // Idempotent, which is what lets the scan re-sanitize on read safely.
        for s in ["flac", "toml", "../x", ""] {
            let once = sanitize_suffix(Some(s));
            assert_eq!(sanitize_suffix(Some(&once)), once, "{s:?}");
        }
    }

    #[test]
    fn is_tmp_name_matches_our_temps_and_not_a_song_called_tmp() {
        assert!(is_tmp_name("tmp.1234.0"));
        assert!(is_tmp_name("tmp.1.99999"));
        // A real song whose id is literally `tmp` must NOT be swept as garbage.
        assert!(!is_tmp_name("tmp.flac"));
        assert!(!is_tmp_name("tmp.toml"));
        assert!(!is_tmp_name("tmp.12a.0"));
        assert!(!is_tmp_name("tmp.1234"));
        assert!(!is_tmp_name("tmp..0"));
        assert!(!is_tmp_name("so-1.flac"));
        assert!(!is_tmp_name("tmpfile"));
    }

    // ── the created-timestamp helper ────────────────────────────────────────

    #[test]
    fn parse_rfc3339_epoch_table() {
        // The epoch itself, and a known-good instant (1714566896 =
        // 2024-05-01T12:34:56Z).
        assert_eq!(parse_rfc3339_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_epoch("2024-05-01T12:34:56Z"), Some(1_714_566_896));
        // Fractional seconds are dropped (sub-second precision is noise for a
        // re-import verdict); both separators are accepted.
        assert_eq!(parse_rfc3339_epoch("2024-05-01T12:34:56.789Z"), Some(1_714_566_896));
        assert_eq!(parse_rfc3339_epoch("2024-05-01T12:34:56,7Z"), Some(1_714_566_896));
        // Offsets, in all three renderings, plus a naive timestamp read as UTC.
        assert_eq!(parse_rfc3339_epoch("2024-05-01T14:34:56+02:00"), Some(1_714_566_896));
        assert_eq!(parse_rfc3339_epoch("2024-05-01T14:34:56+0200"), Some(1_714_566_896));
        assert_eq!(parse_rfc3339_epoch("2024-05-01T10:34:56-02:00"), Some(1_714_566_896));
        assert_eq!(parse_rfc3339_epoch("2024-05-01T12:34:56"), Some(1_714_566_896));
        assert_eq!(parse_rfc3339_epoch("2024-05-01 12:34:56z"), Some(1_714_566_896));
        // A leap second is a real value some servers emit; do not reject it.
        assert_eq!(parse_rfc3339_epoch("2016-12-31T23:59:60Z"), Some(1_483_228_800));
        // Leap-year and century-boundary correctness (days_from_civil, not a table).
        assert_eq!(parse_rfc3339_epoch("2000-02-29T00:00:00Z"), Some(951_782_400));
        assert_eq!(parse_rfc3339_epoch("2100-03-01T00:00:00Z"), Some(4_107_542_400));
        assert_eq!(parse_rfc3339_epoch("1969-12-31T23:59:59Z"), Some(-1));
        // Unrecognized shapes are None, never a panic and never a wrong instant.
        for bad in [
            "", "not a date", "2024-05-01", "2024-05-01T12:34", "2024-13-01T00:00:00Z",
            "2024-05-32T00:00:00Z", "2024-05-01T24:00:00Z", "2024-05-01T00:60:00Z",
            "2024-05-01T00:00:61Z", "2024/05/01T00:00:00Z", "2024-05-01X00:00:00Z",
            "2024-05-01T12:34:56.Z", "2024-05-01T12:34:56+99:00", "2024-05-01T12:34:56*",
            "20xx-05-01T00:00:00Z", "2024-05-01T12:34:56+2",
        ] {
            assert_eq!(parse_rfc3339_epoch(bad), None, "{bad:?} must not parse");
        }
    }

    #[test]
    fn parse_rfc3339_epoch_never_panics_on_a_non_ascii_offset() {
        // The offset body is sliced by BYTE index, so a multi-byte char there once
        // sliced inside a code point and panicked. A `created` value is server data
        // (or hand-edited sidecar data) and it is judged inside the reconciler task,
        // which owns every store mutation: one odd timestamp must cost a `None`
        // verdict, never the task.
        for bad in [
            "2024-05-01T12:34:56+\u{20ac}9",
            "2024-05-01T12:34:56-\u{20ac}9",
            "2024-05-01T12:34:56+\u{e9}\u{e9}",
            "2024-05-01T12:34:56+0\u{e9}0",
            "2024-05-01T12:34:56+\u{4e00}\u{4e8c}",
            "2024-05-01T12:34:56\u{20ac}",
            "2024-05-01T12:34:56.5+\u{20ac}9",
        ] {
            assert_eq!(parse_rfc3339_epoch(bad), None, "{bad:?} must not parse");
        }
        // And the same shape reaching the verdict comparison is honest difference,
        // not a crash: unparseable falls back to exact string equality.
        assert!(created_matches("2024-05-01T12:34:56+\u{20ac}9", "2024-05-01T12:34:56+\u{20ac}9"));
        assert!(!created_matches("2024-05-01T12:34:56+\u{20ac}9", "2024-05-01T12:34:56Z"));
        assert_eq!(
            fingerprint_verdict(
                &{
                    let mut e = entry("odd", 100, 0);
                    e.created = Some("2024-05-01T12:34:56Z".into());
                    e
                },
                &song("odd", 100, "flac", Some("2024-05-01T12:34:56+\u{20ac}9")),
            ),
            Verdict::Differ,
            "an unparseable server timestamp is drift, not a panic"
        );
    }

    #[test]
    fn created_matches_compares_instants_not_strings() {
        // THE point of the helper: a timezone re-rendering after a server upgrade
        // must not read as drift and mass-invalidate the whole mirror.
        assert!(created_matches("2024-05-01T12:34:56Z", "2024-05-01T14:34:56+02:00"));
        assert!(created_matches("2024-05-01T12:34:56.000Z", "2024-05-01T12:34:56Z"));
        assert!(created_matches("2024-05-01T12:34:56Z", "2024-05-01T12:34:56"));
        // A real change in the instant IS drift.
        assert!(!created_matches("2024-05-01T12:34:56Z", "2024-05-01T12:34:57Z"));
        assert!(!created_matches("2024-05-01T12:34:56Z", "2025-01-01T00:00:00Z"));
        // Unparseable falls back to exact string equality: unknown-but-identical is
        // still "same"; unknown-and-different is honestly different, not silently
        // confirmed.
        assert!(created_matches("who knows", "who knows"));
        assert!(!created_matches("who knows", "something else"));
        assert!(!created_matches("who knows", "2024-05-01T12:34:56Z"));
    }

    // ── the sidecar round trip ──────────────────────────────────────────────

    fn sample_sidecar() -> Sidecar {
        Sidecar {
            schema_version: STORE_SCHEMA_VERSION,
            endpoint: ENDPOINT_DOWNLOAD.to_string(),
            content_type: Some("audio/flac".into()),
            pinned: true,
            stale: false,
            fetched_at_unix: 1_700_000_000,
            last_played_unix: 1_700_000_500,
            fingerprint: Fingerprint {
                size: 22_548_990,
                suffix: "flac".into(),
                created: Some("2024-05-01T12:34:56.000Z".into()),
            },
            song: song("so-1", 22_548_990, "flac", Some("2024-05-01T12:34:56.000Z")),
        }
    }

    #[test]
    fn sidecar_round_trips_through_toml_including_the_embedded_song() {
        let sc = sample_sidecar();
        let raw = sidecar_to_toml(&sc);
        let back = sidecar_from_toml(&raw).expect("round-trips");
        assert_eq!(sc, back);
        // The embedded song really is whole - this is what an offline restore and an
        // offline `add song/<id>` read instead of a bare id.
        assert_eq!(back.song.title, "t-so-1");
        assert_eq!(back.song.genre.as_deref(), Some("Electronic"));
        assert_eq!(back.song.duration_secs, Some(210));
        // The server's play stats survive the sidecar too, stamp verbatim.
        assert_eq!(back.song.play_count, Some(7));
        assert_eq!(back.song.played.as_deref(), Some("2026-08-06T14:17:24+01:00"));
    }

    #[test]
    fn sidecar_round_trips_a_song_whose_optionals_are_all_none() {
        // The TOML serializer OMITS a None field, so a song with sparse metadata is
        // the case that breaks without `#[serde(default)]` on every Option. A
        // plain-Subsonic server produces exactly this.
        let mut sparse = song("so-2", 1024, "mp3", None);
        sparse.album = None;
        sparse.artist = None;
        sparse.track = None;
        sparse.duration_secs = None;
        sparse.year = None;
        sparse.genre = None;
        sparse.bitrate = None;
        sparse.content_type = None;
        // A never-played track is exactly this case and it is the common one: the
        // server omits both keys, so both are None and the TOML serializer omits
        // them in turn. The sidecar must still parse its own output - and it must
        // read them back as None rather than inventing a zero.
        sparse.play_count = None;
        sparse.played = None;
        let sc = Sidecar {
            content_type: None,
            fingerprint: Fingerprint { size: 1024, suffix: "mp3".into(), created: None },
            song: sparse.clone(),
            ..sample_sidecar()
        };
        let back = sidecar_from_toml(&sidecar_to_toml(&sc)).expect("sparse round-trips");
        assert_eq!(back, sc);
        assert_eq!(back.song, sparse);
    }

    #[test]
    fn sidecar_from_toml_corruption_battery_is_none_never_panics() {
        // Empty, garbage, and a truncated valid document.
        assert!(sidecar_from_toml("").is_none());
        assert!(sidecar_from_toml("}{ not toml @@@").is_none());
        let raw = sidecar_to_toml(&sample_sidecar());
        assert!(sidecar_from_toml(&raw[..raw.len() / 2]).is_none());
        // Version gate, in both directions.
        for v in [0u32, 2, 999] {
            let sc = Sidecar { schema_version: v, ..sample_sidecar() };
            assert!(sidecar_from_toml(&sidecar_to_toml(&sc)).is_none(), "version {v}");
        }
        // Provenance gate: bytes of unknown origin are never served as originals.
        for ep in ["stream", "", "Download", "transcode"] {
            let sc = Sidecar { endpoint: ep.into(), ..sample_sidecar() };
            assert!(sidecar_from_toml(&sidecar_to_toml(&sc)).is_none(), "endpoint {ep:?}");
        }
        // A missing required field (drop the whole [song] table).
        let no_song = "schema_version = 1\nendpoint = \"download\"\n[fingerprint]\nsize = 1\nsuffix = \"flac\"\n";
        assert!(sidecar_from_toml(no_song).is_none());
        // A zero-size fingerprint would let an EMPTY file pass the length check.
        let sc = Sidecar {
            fingerprint: Fingerprint { size: 0, suffix: "flac".into(), created: None },
            ..sample_sidecar()
        };
        assert!(sidecar_from_toml(&sidecar_to_toml(&sc)).is_none(), "zero size");
        // A tampered suffix that would escape the store directory.
        for bad in ["../../etc/passwd", "toml", "FLAC", "a/b", ""] {
            let sc = Sidecar {
                fingerprint: Fingerprint { size: 10, suffix: bad.into(), created: None },
                ..sample_sidecar()
            };
            assert!(sidecar_from_toml(&sidecar_to_toml(&sc)).is_none(), "suffix {bad:?}");
        }
        // An unstorable embedded song id.
        let mut bad_id = sample_sidecar();
        bad_id.song.id = SongId("../evil".into());
        assert!(sidecar_from_toml(&sidecar_to_toml(&bad_id)).is_none());
    }

    #[test]
    fn sidecar_forward_compat_missing_bookkeeping_keys_default() {
        // A sidecar written by a build that had not yet grown the bookkeeping
        // scalars must still load - no schema bump, no cold-start of the mirror.
        let raw = r#"
schema_version = 1
endpoint = "download"

[fingerprint]
size = 4096
suffix = "flac"

[song]
id = "so-3"
title = "Minimal"
"#;
        let sc = sidecar_from_toml(raw).expect("minimal sidecar loads");
        assert!(!sc.pinned);
        assert!(!sc.stale);
        assert_eq!(sc.fetched_at_unix, 0);
        assert_eq!(sc.last_played_unix, 0);
        assert_eq!(sc.content_type, None);
        assert_eq!(sc.fingerprint.created, None);
        assert_eq!(sc.song.id, sid("so-3"));
        assert_eq!(sc.song.title, "Minimal");
    }

    // ── store I/O against tempdirs ──────────────────────────────────────────

    #[test]
    fn open_refuses_a_directory_it_does_not_own_without_deleting_anything() {
        // THE HAZARD: the heal converges by DELETING everything that is not a valid
        // pair, and `store.dir` is a path a person types. Aimed at the state dir,
        // `resume.toml` strips to the storable id `resume`, fails to parse as a
        // sidecar, and would be removed as an invalid entry - taking the saved queue
        // with it, before the restore that was about to read it. Aimed at a music
        // folder, every track is an unaccounted loose file. So an unmarked non-empty
        // directory is REFUSED, and refusal must be total: not one byte removed.
        let dir = unowned_tmpdir("not-ours");
        std::fs::write(dir.join("resume.toml"), "version = 3\nvolume = 70\n").expect("resume");
        std::fs::write(dir.join("Waltz.flac"), vec![b'x'; 8]).expect("track");
        std::fs::create_dir_all(dir.join("covers")).expect("subdir");

        let Err(err) = AudioStore::open(dir.clone(), StoreConfig::default()) else {
            panic!("a directory that is not ours must be refused");
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains(STORE_MARKER_NAME),
            "the error names the marker so the fix is obvious: {err}"
        );
        assert!(dir.join("resume.toml").exists(), "the saved queue survives");
        assert!(dir.join("Waltz.flac").exists(), "the music survives");
        assert!(dir.join("covers").is_dir(), "and so does everything else");
        assert!(!dir.join(STORE_MARKER_NAME).exists(), "a refusal claims nothing");

        // A HIDDEN file is enough to make a directory someone else's: emptiness is
        // the bar, not "empty of things we recognize".
        let dotted = unowned_tmpdir("not-ours-hidden");
        std::fs::write(dotted.join(".keep"), b"").expect("dotfile");
        assert!(AudioStore::open(dotted.clone(), StoreConfig::default()).is_err());
        assert!(dotted.join(".keep").exists());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dotted);
    }

    #[test]
    fn open_adopts_an_empty_directory_and_keeps_owning_it() {
        // The only way in: empty (or already ours). Adoption is what a fresh
        // `<state_dir>/store` takes, and it must stick across restarts.
        let dir = unowned_tmpdir("adopt");
        let root = dir.join("store");
        let s = store(&root, 1 << 30);
        assert!(root.join(STORE_MARKER_NAME).is_file(), "adoption drops the claim");
        assert_eq!(s.entries(), Vec::new());
        drop(s);

        // Reopening a marked directory is the ordinary path, and a pair placed in it
        // heals normally now that ownership is established.
        place(&root, &song("so-1", 32, "flac", None), true, false, 7);
        std::fs::write(root.join("stray"), b"junk").expect("stray");
        let s = store(&root, 1 << 30);
        assert_eq!(s.entries().len(), 1, "the pair survives");
        assert!(!root.join("stray").exists(), "and the heal still runs, in OUR directory");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_ownership_marker_survives_the_heal_it_authorizes() {
        // If the scan classified the marker as an orphan, the store would delete its
        // own claim and refuse the directory on the NEXT start - a store that works
        // exactly once.
        let dir = tmpdir("marker-survives");
        place(&dir, &song("so-1", 16, "flac", None), true, false, 0);
        std::fs::write(dir.join("orphan.flac"), vec![b'x'; 4]).expect("orphan");
        for _ in 0..3 {
            let s = store(&dir, 1 << 30);
            assert_eq!(s.entries().len(), 1);
            assert!(dir.join(STORE_MARKER_NAME).is_file(), "the marker is never healed away");
        }
        assert!(!dir.join("orphan.flac").exists(), "while real orphans do go");
        // And the scan itself never reports it in any category.
        let scan = scan_dir(&dir).expect("scan");
        assert!(scan.orphan_audio.is_empty(), "{:?}", scan.orphan_audio);
        assert!(scan.stale_tmps.is_empty());
        assert!(scan.orphan_sidecars.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_creates_the_root_and_indexes_a_committed_pair() {
        let dir = tmpdir("open-fresh");
        let root = dir.join("store");
        assert!(!root.exists(), "the root does not exist yet");
        let s = store(&root, 1 << 30);
        assert!(root.is_dir(), "open creates the root");
        assert_eq!(s.entries(), Vec::new());
        assert_eq!(s.total_bytes(), 0);

        // A hand-placed valid pair is picked up on the next open, with its
        // sidecar's pinned / stale / recency preserved.
        place(&root, &song("so-1", 32, "flac", Some("2024-05-01T12:00:00Z")), true, true, 42);
        let s = store(&root, 1 << 30);
        let e = s.entries();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].id, sid("so-1"));
        assert_eq!(e[0].size, 32);
        assert_eq!(e[0].suffix, "flac");
        assert!(e[0].pinned);
        assert!(e[0].stale, "a stale entry is still indexed - it keeps serving");
        assert_eq!(e[0].last_played_unix, 42);
        assert!(!e[0].suspect, "suspect is in-memory only, so a restart re-offers");
        assert!(!e[0].recency_dirty);
        assert_eq!(s.total_bytes(), 32);
        // Idempotent: opening again over a healthy store changes nothing.
        let again = store(&root, 1 << 30);
        assert_eq!(again.entries(), e);
        assert_eq!(names(&root), vec!["so-1.flac".to_string(), "so-1.toml".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lookup_verdict_table() {
        let dir = tmpdir("lookup");
        place(&dir, &song("happy", 16, "flac", None), true, false, 0);
        place(&dir, &song("short", 16, "mp3", None), false, false, 0);
        place(&dir, &song("gone", 16, "ogg", None), false, false, 0);
        // Truncate one by a byte AFTER the scan would have accepted it (the live
        // "someone touched the file while we ran" case), and delete another.
        let s = store(&dir, 1 << 30);
        assert_eq!(s.entries().len(), 3, "all three indexed at open");
        std::fs::write(dir.join("short.mp3"), vec![b'x'; 15]).expect("truncate");
        std::fs::remove_file(dir.join("gone.ogg")).expect("delete audio");

        // Happy: the path is returned.
        assert_eq!(s.lookup(&sid("happy")), Some(dir.join("happy.flac")));
        // Wrong length: rejected by the one stat, so a truncated file never plays.
        assert_eq!(s.lookup(&sid("short")), None);
        // Missing file: rejected.
        assert_eq!(s.lookup(&sid("gone")), None);
        // Unknown id: rejected without touching the filesystem.
        assert_eq!(s.lookup(&sid("never-heard-of-it")), None);
        // Suspect: de-offered even though bytes and length are both fine, and the
        // BYTES ARE STILL THERE - keep-until-replaced.
        assert!(s.mark_suspect(&sid("happy")), "first mark changes state");
        assert!(!s.mark_suspect(&sid("happy")), "second mark is a no-op");
        assert_eq!(s.lookup(&sid("happy")), None, "a suspect is never re-offered");
        assert!(dir.join("happy.flac").exists(), "suspect bytes are NOT deleted");
        // Marking an unknown id is a no-op, not a panic.
        assert!(!s.mark_suspect(&sid("nope")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lookup_never_returns_a_tmp_or_a_sidecar_less_audio_file() {
        // ATOMIC VISIBILITY. The two intermediate states of the write protocol - a
        // temp still being written, and audio renamed into place before the sidecar
        // commits - must both be invisible to playback.
        let dir = tmpdir("visibility");
        let s = store(&dir, 1 << 30);
        // (a) A fully written temp of exactly the right length.
        let tmp = s.tmp_path();
        std::fs::write(&tmp, vec![b'x'; 64]).expect("write tmp");
        assert_eq!(s.lookup(&sid("so-1")), None, "a temp is not an entry");
        // (b) The audio renamed in, correct length, NO sidecar yet.
        std::fs::rename(&tmp, dir.join("so-1.flac")).expect("rename");
        assert_eq!(
            s.lookup(&sid("so-1")),
            None,
            "audio with no sidecar is an orphan, never playable"
        );
        // A reopen agrees, and heals it: the sidecar rename IS the commit.
        let s = store(&dir, 1 << 30);
        assert_eq!(s.entries(), Vec::new());
        assert_eq!(names(&dir), Vec::<String>::new(), "the orphan is swept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_heals_every_invalid_on_disk_state() {
        let dir = tmpdir("heal");
        // A survivor.
        place(&dir, &song("keep", 24, "flac", None), true, false, 7);
        // A sidecar whose audio is MISSING.
        place(&dir, &song("noaudio", 24, "flac", None), false, false, 0);
        std::fs::remove_file(dir.join("noaudio.flac")).expect("rm");
        // A sidecar whose audio is the WRONG LENGTH (a truncated download that
        // somehow got a sidecar, or a tampered file).
        place(&dir, &song("short", 24, "flac", None), false, false, 0);
        std::fs::write(dir.join("short.flac"), vec![b'x'; 23]).expect("truncate");
        // A CORRUPT sidecar, with its audio present.
        std::fs::write(dir.join("garbage.flac"), vec![b'x'; 24]).expect("write");
        std::fs::write(dir.join("garbage.toml"), "}{ not toml").expect("write");
        // A sidecar whose embedded song id does not match its file name - mis-keyed,
        // so every lookup through it would serve the wrong audio.
        place(&dir, &song("real-id", 24, "flac", None), false, false, 0);
        std::fs::rename(dir.join("real-id.toml"), dir.join("wrong-id.toml")).expect("rename");
        // ORPHAN AUDIO with no sidecar at all, and a crashed sidecar temp.
        std::fs::write(dir.join("orphan.flac"), vec![b'x'; 24]).expect("write");
        std::fs::write(dir.join("keep.toml.tmp.999.0"), "half").expect("write");
        // STALE TEMPS from a dead process.
        std::fs::write(dir.join("tmp.999.0"), vec![b'x'; 10]).expect("write");
        std::fs::write(dir.join("tmp.999.1"), vec![b'x'; 10]).expect("write");
        // A subdirectory: not ours, never removed.
        std::fs::create_dir(dir.join("subdir")).expect("mkdir");

        let s = store(&dir, 1 << 30);
        assert_eq!(
            s.entries().iter().map(|e| e.id.0.clone()).collect::<Vec<_>>(),
            vec!["keep".to_string()],
            "only the valid pair survives"
        );
        assert_eq!(
            names(&dir),
            vec!["keep.flac".to_string(), "keep.toml".to_string(), "subdir".to_string()],
            "everything invalid is healed; the subdir is left alone"
        );
        // `real-id.flac` went too: with its sidecar renamed away it was orphan audio.
        assert!(!dir.join("real-id.flac").exists());
        assert!(s.lookup(&sid("keep")).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_writes_bytes_before_the_sidecar_and_indexes_the_entry() {
        let dir = tmpdir("commit");
        let s = store(&dir, 1 << 30);
        let sg = song("so-1", 48, "flac", Some("2024-05-01T12:00:00Z"));
        let tmp = s.tmp_path();
        std::fs::write(&tmp, vec![b'a'; 48]).expect("write tmp");
        let path = s.commit(&sg, &tmp, true, 1_700_000_000).expect("commit");

        assert_eq!(path, dir.join("so-1.flac"));
        assert!(!tmp.exists(), "the temp was renamed, not copied");
        assert_eq!(names(&dir), vec!["so-1.flac".to_string(), "so-1.toml".to_string()]);
        assert_eq!(s.lookup(&sid("so-1")), Some(path));
        // The sidecar records the fingerprint the commit was gated on, plus the
        // provenance marker and the whole song.
        let sc = s.read_sidecar(&sid("so-1")).expect("sidecar");
        assert_eq!(sc.schema_version, STORE_SCHEMA_VERSION);
        assert_eq!(sc.endpoint, ENDPOINT_DOWNLOAD);
        assert_eq!(sc.fingerprint.size, 48);
        assert_eq!(sc.fingerprint.suffix, "flac");
        assert_eq!(sc.fingerprint.created.as_deref(), Some("2024-05-01T12:00:00Z"));
        assert_eq!(sc.content_type.as_deref(), Some("audio/flac"));
        assert!(sc.pinned);
        assert!(!sc.stale);
        assert_eq!(sc.fetched_at_unix, 1_700_000_000);
        assert_eq!(sc.song, sg);
        // And a reopen rebuilds the same entry from disk truth alone.
        let reopened = store(&dir, 1 << 30);
        assert_eq!(reopened.entries(), s.entries());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_refuses_a_length_mismatch_and_leaves_the_previous_entry_serving() {
        let dir = tmpdir("commit-mismatch");
        let s = store(&dir, 1 << 30);
        let sg = song("so-1", 48, "flac", None);
        // First, a good commit so there is something to protect.
        let tmp = s.tmp_path();
        std::fs::write(&tmp, vec![b'a'; 48]).expect("write");
        s.commit(&sg, &tmp, true, 10).expect("commit");

        // A SHORT download must not commit, must delete its own temp, and must
        // leave the previous valid entry untouched and playable.
        let bad = s.tmp_path();
        std::fs::write(&bad, vec![b'b'; 47]).expect("write");
        let err = s.commit(&sg, &bad, true, 20).expect_err("short download");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(!bad.exists(), "the rejected temp is removed");
        assert_eq!(s.lookup(&sid("so-1")), Some(dir.join("so-1.flac")));
        assert_eq!(std::fs::read(dir.join("so-1.flac")).expect("read"), vec![b'a'; 48]);
        // A LONG download is refused the same way.
        let long = s.tmp_path();
        std::fs::write(&long, vec![b'c'; 49]).expect("write");
        assert!(s.commit(&sg, &long, true, 30).is_err());
        assert!(!long.exists());
        assert_eq!(std::fs::read(dir.join("so-1.flac")).expect("read"), vec![b'a'; 48]);

        // No server-reported size means no commit gate at all: refuse rather than
        // trust, or a truncated download would commit as valid.
        let mut sizeless = sg.clone();
        sizeless.size = None;
        let t = s.tmp_path();
        std::fs::write(&t, vec![b'd'; 48]).expect("write");
        let err = s.commit(&sizeless, &t, true, 40).expect_err("no size");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(!t.exists());
        // A zero size is the same refusal (an empty file must never look valid).
        let mut zero = sg.clone();
        zero.size = Some(0);
        let t = s.tmp_path();
        std::fs::write(&t, Vec::<u8>::new()).expect("write");
        assert!(s.commit(&zero, &t, true, 50).is_err());
        // An unstorable id never reaches the filesystem.
        let mut evil = sg.clone();
        evil.id = SongId("../evil".into());
        let t = s.tmp_path();
        std::fs::write(&t, vec![b'e'; 48]).expect("write");
        assert!(s.commit(&evil, &t, true, 60).is_err());
        assert_eq!(names(&dir), vec!["so-1.flac".to_string(), "so-1.toml".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_rename_over_replacement_preserves_continuous_validity() {
        use std::io::Read;
        let dir = tmpdir("commit-replace");
        let s = store(&dir, 1 << 30);
        let v1 = song("so-1", 48, "flac", Some("2024-01-01T00:00:00Z"));
        let tmp = s.tmp_path();
        std::fs::write(&tmp, vec![b'1'; 48]).expect("write");
        s.commit(&v1, &tmp, true, 10).expect("commit v1");
        s.mark_suspect(&sid("so-1"));
        assert_eq!(s.lookup(&sid("so-1")), None, "de-offered while suspect");

        // Hold an open fd on the OLD inode, the way mpv does mid-play.
        let mut held = File::open(dir.join("so-1.flac")).expect("open");

        // The replacement: same suffix, a different size and `created`.
        let v2 = song("so-1", 64, "flac", Some("2024-06-01T00:00:00Z"));
        let tmp = s.tmp_path();
        std::fs::write(&tmp, vec![b'2'; 64]).expect("write");
        s.commit(&v2, &tmp, true, 20).expect("commit v2");

        // The entry is valid again, at the NEW length, with suspect and stale
        // cleared - these bytes are fresh.
        assert_eq!(s.lookup(&sid("so-1")), Some(dir.join("so-1.flac")));
        let e = &s.entries()[0];
        assert_eq!(e.size, 64);
        assert!(!e.suspect, "a verified replacement clears the suspect mark");
        assert!(!e.stale);
        assert_eq!(e.created.as_deref(), Some("2024-06-01T00:00:00Z"));
        assert_eq!(std::fs::read(dir.join("so-1.flac")).expect("read"), vec![b'2'; 64]);
        // The already-open fd still reads the OLD bytes: unlink semantics keep the
        // playing inode alive, which is why a mid-play replacement is inaudible.
        let mut old = Vec::new();
        held.read_to_end(&mut old).expect("read held fd");
        assert_eq!(old, vec![b'1'; 48], "mpv's open fd survives the rename-over");
        // Exactly one pair on disk; no delete-first window ever existed.
        assert_eq!(names(&dir), vec!["so-1.flac".to_string(), "so-1.toml".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_with_a_suffix_change_writes_the_new_pair_before_deleting_the_old() {
        let dir = tmpdir("commit-suffix");
        let s = store(&dir, 1 << 30);
        let flac = song("so-1", 48, "flac", None);
        let tmp = s.tmp_path();
        std::fs::write(&tmp, vec![b'1'; 48]).expect("write");
        s.commit(&flac, &tmp, true, 10).expect("commit flac");

        // A re-import as mp3: the NEW pair must be valid before the old bytes go.
        let mp3 = song("so-1", 32, "mp3", None);
        let tmp = s.tmp_path();
        std::fs::write(&tmp, vec![b'2'; 32]).expect("write");
        s.commit(&mp3, &tmp, true, 20).expect("commit mp3");

        assert_eq!(s.lookup(&sid("so-1")), Some(dir.join("so-1.mp3")));
        assert!(!dir.join("so-1.flac").exists(), "the superseded audio is gone");
        assert_eq!(names(&dir), vec!["so-1.mp3".to_string(), "so-1.toml".to_string()]);
        assert_eq!(s.entries()[0].suffix, "mp3");
        // A reopen agrees with the in-memory index.
        assert_eq!(store(&dir, 1 << 30).entries(), s.entries());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_falls_back_to_bin_for_an_unusable_suffix() {
        let dir = tmpdir("commit-bin");
        let s = store(&dir, 1 << 30);
        // A suffix of `toml` would collide with the sidecar; a traversal attempt
        // would escape the store. Both land on `bin`.
        for (id, suffix) in [("so-1", "toml"), ("so-2", "../../etc/passwd")] {
            let sg = song(id, 16, suffix, None);
            let tmp = s.tmp_path();
            std::fs::write(&tmp, vec![b'x'; 16]).expect("write");
            let path = s.commit(&sg, &tmp, false, 1).expect("commit");
            assert_eq!(path, dir.join(format!("{id}.bin")));
            assert_eq!(s.lookup(&sid(id)), Some(path));
        }
        assert_eq!(
            names(&dir),
            vec![
                "so-1.bin".to_string(),
                "so-1.toml".to_string(),
                "so-2.bin".to_string(),
                "so-2.toml".to_string(),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_entry_drops_the_index_and_both_files() {
        let dir = tmpdir("remove");
        place(&dir, &song("a", 16, "flac", None), false, false, 0);
        place(&dir, &song("b", 16, "flac", None), false, false, 0);
        // A leftover from an interrupted suffix change: `remove_entry` must take it
        // too when it sweeps by prefix.
        std::fs::write(dir.join("a.mp3"), vec![b'x'; 16]).expect("write");
        let s = store(&dir, 1 << 30);
        // `a.mp3` was orphan audio at open, so it is already gone; re-create it to
        // exercise the unknown-suffix sweep path.
        std::fs::write(dir.join("a.mp3"), vec![b'x'; 16]).expect("write");
        s.remove_entry(&sid("a"));
        assert_eq!(s.lookup(&sid("a")), None);
        assert_eq!(s.entries().iter().map(|e| e.id.0.clone()).collect::<Vec<_>>(), vec!["b"]);
        assert!(!dir.join("a.flac").exists());
        assert!(!dir.join("a.toml").exists());
        assert!(dir.join("b.flac").exists(), "the neighbor is untouched");
        // Removing something unknown is a silent no-op, never a panic.
        s.remove_entry(&sid("never-existed"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A removal the FILESYSTEM REFUSES must not de-offer a song whose bytes are
    /// still right there and still valid.
    ///
    /// On a read-only remount (or an immutable file) the unlink fails, the pair
    /// survives whole, and dropping the index entry anyway would make `resolve_play`
    /// fall through to the network for a song the disk can serve - and only a FULL
    /// pass re-adopts it, so the regression lasted up to `sync_interval_secs`
    /// (900s by default). Keep-until-replaced cuts both ways.
    #[test]
    fn a_refused_removal_keeps_serving_the_entry_it_could_not_delete() {
        let dir = tmpdir("remove-denied");
        place(&dir, &song("a", 16, "flac", None), true, false, 0);
        let s = store(&dir, 1 << 30);
        assert_eq!(s.lookup(&sid("a")), Some(dir.join("a.flac")), "offered before");

        deny_unlink(&dir);
        let reclaimed = s.remove_entry(&sid("a"));
        let refused = dir.join("a.flac").exists() && dir.join("a.toml").exists();
        // ALWAYS restore before asserting, so a failure still leaves nothing behind.
        allow_unlink(&dir);
        if !refused {
            // A root build user unlinks inside a read-only directory anyway; the
            // scenario cannot be built there, so do not pretend to have tested it.
            eprintln!("skipping: this process can unlink inside a read-only directory");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        assert!(!reclaimed, "an unlink the filesystem refused reclaimed nothing");
        assert_eq!(
            s.lookup(&sid("a")),
            Some(dir.join("a.flac")),
            "the pair is still whole on disk, so it must still be OFFERED - de-offering it \
             streams a cached song over the network until the next full pass"
        );
        assert_eq!(s.total_bytes(), 16, "and its bytes still count against the budget");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half of that ordering: once the SIDECAR is gone the entry is
    /// de-committed and can never be valid again (section 2, rule 1), so it leaves
    /// the index even though the bytes could not be removed - which is also what
    /// keeps playback from being handed a path that is about to vanish.
    #[test]
    fn a_de_committed_entry_leaves_the_index_even_when_its_bytes_will_not_go() {
        let dir = tmpdir("remove-audio-stuck");
        place(&dir, &song("a", 16, "flac", None), true, false, 0);
        let s = store(&dir, 1 << 30);
        // Turn the audio file into a DIRECTORY: `remove_file` then fails with EISDIR
        // while the sidecar unlink still succeeds - the split-outcome case, with no
        // root privileges needed to build it.
        std::fs::remove_file(dir.join("a.flac")).expect("drop the audio file");
        std::fs::create_dir(dir.join("a.flac")).expect("stand a directory in its place");

        assert!(!s.remove_entry(&sid("a")), "nothing was reclaimed");
        // Asserted on the INDEX itself, not through `lookup` - `lookup`'s stat would
        // reject a directory anyway, so it could not tell the two orderings apart.
        assert!(s.entries().is_empty(), "a de-committed entry leaves the index");
        assert_eq!(s.lookup(&sid("a")), None, "and is never offered");
        assert!(!dir.join("a.toml").exists(), "and its commit record is gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_pinned_set_stale_and_flush_recency_round_trip_through_the_sidecar() {
        let dir = tmpdir("mutators");
        place(&dir, &song("a", 16, "flac", None), true, false, 100);
        let s = store(&dir, 1 << 30);

        // Demote: the flag flips in BOTH the sidecar and the index, and the BYTES
        // STAY - an accidental unstar and re-star costs zero bytes.
        s.set_pinned(&sid("a"), false).expect("demote");
        assert!(!s.entries()[0].pinned);
        assert!(!s.read_sidecar(&sid("a")).expect("sidecar").pinned);
        assert_eq!(s.lookup(&sid("a")), Some(dir.join("a.flac")));
        s.set_pinned(&sid("a"), true).expect("promote");
        assert!(s.entries()[0].pinned);

        // Stale keeps serving - the mark only schedules a replacement.
        s.set_stale(&sid("a"), true).expect("mark stale");
        assert!(s.entries()[0].stale);
        assert!(s.read_sidecar(&sid("a")).expect("sidecar").stale);
        assert_eq!(s.lookup(&sid("a")), Some(dir.join("a.flac")), "stale still plays");
        s.set_stale(&sid("a"), false).expect("clear stale");
        assert!(!s.entries()[0].stale);

        // A resolve-time bump is in-memory and dirty until a pass flushes it.
        s.note_played(&sid("a"), 555);
        let e = s.entries().remove(0);
        assert_eq!(e.last_played_unix, 555);
        assert!(e.recency_dirty, "dirty until flushed");
        assert_eq!(
            s.read_sidecar(&sid("a")).expect("sidecar").last_played_unix,
            100,
            "the bump has NOT touched disk yet - one disk writer"
        );
        s.flush_recency(&sid("a"), 555).expect("flush");
        assert_eq!(s.read_sidecar(&sid("a")).expect("sidecar").last_played_unix, 555);
        assert!(!s.entries()[0].recency_dirty);
        // A flush of a STALER value than the current one must not clear the dirty
        // flag, or the newer bump would be lost silently.
        s.note_played(&sid("a"), 900);
        s.flush_recency(&sid("a"), 555).expect("flush old value");
        assert!(s.entries()[0].recency_dirty, "a newer bump stays dirty");
        // A bump for an unknown id is a no-op; a mutation of one is NotFound.
        s.note_played(&sid("nope"), 1);
        assert_eq!(
            s.set_stale(&sid("nope"), true).expect_err("no sidecar").kind(),
            io::ErrorKind::NotFound
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_startup_heal_sweeps_temps_and_spares_a_song_called_tmp() {
        // The temp sweep has exactly TWO live paths - the startup heal here and the
        // full pass's `SweepTmp` (covered by `scan_dir_output_drives_a_full_pass_end_to_end`)
        // - and both apply `scan_dir`'s one classification. This asserts the startup
        // one on the case that would be a data-loss bug: a song whose id is literally
        // `tmp` must NOT look like an in-flight temp.
        let dir = tmpdir("sweep");
        place(&dir, &song("tmp", 16, "flac", None), false, false, 0);
        std::fs::write(dir.join("tmp.1.0"), b"x").expect("write");
        std::fs::write(dir.join("tmp.99999.7"), b"x").expect("write");
        let s = store(&dir, 1 << 30);
        assert_eq!(names(&dir), vec!["tmp.flac".to_string(), "tmp.toml".to_string()]);
        assert_eq!(s.lookup(&sid("tmp")), Some(dir.join("tmp.flac")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tmp_paths_are_unique_and_live_beside_their_target() {
        let dir = tmpdir("tmp-path");
        let s = store(&dir, 1 << 30);
        let a = s.tmp_path();
        let b = s.tmp_path();
        assert_ne!(a, b, "unique per call, so concurrent writers cannot collide");
        for p in [&a, &b] {
            assert_eq!(p.parent(), Some(dir.as_path()), "same dir => atomic rename");
            let name = p.file_name().expect("name").to_string_lossy().into_owned();
            assert!(is_tmp_name(&name), "{name:?} must be sweepable");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cached_song_returns_the_embedded_metadata_only_for_a_valid_entry() {
        let dir = tmpdir("cached-song");
        let sg = song("so-1", 16, "flac", None);
        place(&dir, &sg, true, false, 0);
        let s = store(&dir, 1 << 30);
        let got = s.cached_song(&sid("so-1")).expect("embedded song");
        assert_eq!(got, sg, "the whole song, not a bare id");
        assert_eq!(s.cached_song(&sid("unknown")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn window_and_protected_sets_are_shared_mutable_state() {
        let dir = tmpdir("window");
        let s = store(&dir, 1 << 30);
        assert!(s.set_window(vec![sid("a"), sid("b")]), "a change reports true");
        assert!(!s.set_window(vec![sid("a"), sid("b")]), "no change reports false");
        assert_eq!(s.window(), vec![sid("a"), sid("b")]);
        assert!(s.set_window(vec![sid("b"), sid("a")]), "order is part of the window");
        s.protect(sid("skip-target"));
        assert!(s.protected_ids().contains(&sid("skip-target")));
        s.unprotect(&sid("skip-target"));
        assert!(s.protected_ids().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── the budget: why hypodj cannot fill the disk ─────────────────────────
    //
    // The one rule that bounds the disk is a PURE function, which is the point:
    // "overfilling is impossible" has to be provable without a disk, without a
    // server and without a clock. These are that proof.

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn derive_budget_lets_free_space_beat_the_configured_cap_and_never_the_reverse() {
        // A 2000 GiB filesystem so the 5 % fraction is a round 100 GiB and every
        // expectation below is a hand-computable number rather than a restatement of
        // the formula.
        assert_eq!(budget_reserve(2000 * GIB), 100 * GIB, "5 % beats the floor on a big disk");

        // ROOMY: the configured cap binds. The reserve is a BRAKE, not a governor.
        assert_eq!(derive_budget(500 * GIB, 2000 * GIB, 10 * GIB, 16 * GIB), Some(16 * GIB));
        // TIGHTENING: free space binds and the ceiling falls BELOW the cap. Pool is
        // 105 + 10 = 115 GiB, less the 100 GiB reserve.
        assert_eq!(derive_budget(105 * GIB, 2000 * GIB, 10 * GIB, 16 * GIB), Some(15 * GIB));
        // EXACTLY AT THE RESERVE: zero, not a negative wrapped into something huge.
        assert_eq!(derive_budget(100 * GIB, 2000 * GIB, 0, 16 * GIB), Some(0));
        // BREACHED: the pool is smaller than the reserve. Zero means the store writes
        // NOTHING at all and only deletes.
        assert_eq!(derive_budget(60 * GIB, 2000 * GIB, 10 * GIB, 16 * GIB), Some(0));
        // A SMALLER CONFIGURED CAP always wins - the knob can only ever lower.
        assert_eq!(derive_budget(500 * GIB, 2000 * GIB, 10 * GIB, GIB), Some(GIB));

        // THE REAL MACHINE this was sized against: 929 GiB total, 113 GiB free, a
        // 2 GiB store. The reserve (46 GiB) is not binding, so the 16 GiB cap is.
        assert_eq!(
            derive_budget(113 * GIB, 929 * GIB, 2 * GIB, crate::config::DEFAULT_STORE_MAX_BYTES),
            Some(crate::config::DEFAULT_STORE_MAX_BYTES),
            "a roomy laptop disk gets the full configured cap"
        );
    }

    #[test]
    fn derive_budget_floors_the_reserve_so_a_small_disk_is_not_cheaply_filled() {
        // On a small disk 5 % is less than a single NixOS system closure, so the
        // fraction alone would not be a brake. 5 % of 100 GiB is 5 GiB; the floor
        // wins.
        assert_eq!(budget_reserve(100 * GIB), STORE_RESERVE_FLOOR);
        assert_eq!(derive_budget(50 * GIB, 100 * GIB, 0, 16 * GIB), Some(16 * GIB));
        assert_eq!(derive_budget(25 * GIB, 100 * GIB, 0, 16 * GIB), Some(5 * GIB));

        // A filesystem SMALLER than the reserve itself - the 6 GiB tmpfs the live
        // proof points a daemon at - can never be written to at all. This is the
        // guarantee in its sharpest form: no configuration, on any mount, lets the
        // store take the last of a small volume.
        assert_eq!(budget_reserve(6 * GIB), STORE_RESERVE_FLOOR);
        for own in [0, GIB, 5 * GIB] {
            assert_eq!(
                derive_budget(6 * GIB, 6 * GIB, own, 16 * GIB),
                Some(0),
                "a filesystem smaller than the reserve yields a zero budget"
            );
        }
    }

    #[test]
    fn derive_budget_is_invariant_to_the_stores_own_eviction() {
        // THE ANTI-OSCILLATION PROPERTY, and the reason the ceiling is written
        // `(avail + own) - reserve` rather than a fraction of free space.
        //
        // Evicting moves bytes from `own` into `avail`, and the sum is what the rule
        // reads - so hypodj's own actions cannot move hypodj's own ceiling. Only
        // another process can. Without this a tight budget would be met by evicting,
        // which would RAISE the recomputed budget, which would re-admit, which would
        // evict again: a download loop against the server and the disk.
        let cfg = 16 * GIB;
        let total = 2000 * GIB;
        let baseline = derive_budget(105 * GIB, total, 10 * GIB, cfg);
        assert_eq!(baseline, Some(15 * GIB), "the pool binds, so the case is a real test");
        // Every split of the same pool - including evicting the store to nothing -
        // yields the IDENTICAL ceiling.
        for (avail, own) in [
            (105 * GIB, 10 * GIB),
            (110 * GIB, 5 * GIB),
            (114 * GIB, GIB),
            (115 * GIB, 0),
        ] {
            assert_eq!(
                derive_budget(avail, total, own, cfg),
                baseline,
                "freeing {own} of our own bytes must not move our own ceiling"
            );
        }
    }

    #[test]
    fn derive_budget_never_trusts_an_unmeasured_or_nonsense_mount() {
        // `avail` larger than `total` is nonsense from the mount. `None` sends the
        // caller to the CONFIGURED cap - never to unlimited, because an unmeasured
        // disk is not an empty one.
        assert_eq!(derive_budget(101 * GIB, 100 * GIB, 0, 16 * GIB), None);
        assert_eq!(derive_budget(1, 0, 0, 16 * GIB), None);
        // A zero-sized filesystem is measurable and simply yields nothing.
        assert_eq!(derive_budget(0, 0, 0, 16 * GIB), Some(0));
        // Degenerate extremes are TOTAL: saturating throughout, no panic, no wrap.
        assert_eq!(derive_budget(u64::MAX, u64::MAX, u64::MAX, 16 * GIB), Some(16 * GIB));
        assert_eq!(derive_budget(0, u64::MAX, 0, u64::MAX), Some(0), "a huge disk with nothing free");
        // Even with no configured cap at all the reserve still holds some back, so
        // there is no input for which the store may claim the whole filesystem.
        let unlimited = derive_budget(u64::MAX, u64::MAX, u64::MAX, u64::MAX).expect("total");
        assert!(unlimited < u64::MAX, "the reserve applies even at the extreme");
        assert!(
            u64::MAX - unlimited >= u64::MAX / 100 * STORE_RESERVE_FRACTION_PCT,
            "and it holds back the full fraction"
        );
    }

    #[test]
    fn the_derived_budget_may_fall_below_the_config_floor_while_the_knob_may_not() {
        // Deliberate asymmetry. `normalize` clamps the CONFIGURED value up to 64 MiB
        // because a budget that small would thrash eviction against every download.
        // The DERIVED value has no such floor: "the disk is nearly full" must beat
        // "the store would like at least 64 MiB".
        let derived = derive_budget(
            STORE_RESERVE_FLOOR + 1024 * 1024,
            100 * GIB,
            0,
            crate::config::DEFAULT_STORE_MAX_BYTES,
        );
        assert_eq!(derived, Some(1024 * 1024), "one megabyte of ceiling");
        assert!(
            derived.unwrap() < crate::config::STORE_MIN_MAX_BYTES,
            "and it is allowed below the config floor"
        );

        let mut cfg = StoreConfig { max_bytes: 1, ..StoreConfig::default() };
        cfg.normalize();
        assert_eq!(
            cfg.max_bytes,
            crate::config::STORE_MIN_MAX_BYTES,
            "while the configured knob is still clamped up at load"
        );
    }

    #[test]
    fn pin_ceiling_is_total_and_never_starves_a_small_budget_to_nothing() {
        // The setback exists so a full pin frontier cannot starve the queue window and
        // stale replacements. It must not, on a small budget, become the whole budget.
        assert_eq!(pin_ceiling(0), 0);
        assert_eq!(pin_ceiling(16 * GIB), 16 * GIB - STORE_PIN_CEILING_SETBACK);
        // At the 64 MiB config floor the flat setback would leave nothing, so it is
        // capped at a quarter.
        assert_eq!(pin_ceiling(64 * 1024 * 1024), 48 * 1024 * 1024);
        for max in [1u64, 4, 999, 1 << 20, 1 << 30, u64::MAX] {
            let c = pin_ceiling(max);
            assert!(c <= max, "the pin ceiling never exceeds the budget ({max})");
            assert!(c > 0, "a non-zero budget always leaves room for pins ({max})");
        }
    }

    #[test]
    fn statvfs_measures_a_real_filesystem_and_the_ceiling_follows_from_it() {
        // The one part of the rule that is an OBSERVATION rather than arithmetic. A
        // real directory on a real mount, so a broken syscall binding is caught here
        // rather than by a daemon silently falling back to the configured cap.
        let dir = tmpdir("statvfs");
        let (avail, total) = statvfs_space(&dir).expect("statvfs answers for a real directory");
        assert!(total > 0, "a mounted filesystem has a size");
        assert!(avail <= total, "free space never exceeds the filesystem: {avail} of {total}");
        // And the measurement feeds the rule without panicking, bounded by the cap.
        let budget = derive_budget(avail, total, 0, crate::config::DEFAULT_STORE_MAX_BYTES)
            .expect("a real mount is never nonsense");
        assert!(budget <= crate::config::DEFAULT_STORE_MAX_BYTES);
        // A path that does not exist is a failure, not a zero-sized filesystem - the
        // caller must fall back to the config rather than conclude the disk is full.
        assert_eq!(statvfs_space(&dir.join("no-such-child")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── plan_pass: the full divergence matrix ───────────────────────────────

    /// Just the downloads of a plan, as (id, reason) - the shape most cases assert.
    fn dls(plan: &[StoreAction]) -> Vec<(String, DownloadReason)> {
        plan.iter()
            .filter_map(|a| match a {
                StoreAction::Download { id, reason } => Some((id.0.clone(), *reason)),
                _ => None,
            })
            .collect()
    }

    fn evictions(plan: &[StoreAction]) -> Vec<String> {
        plan.iter()
            .filter_map(|a| match a {
                StoreAction::Evict(id) => Some(id.0.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn plan_pass_over_an_empty_store_with_nothing_wanted_is_empty() {
        let plan = plan_pass(&PassInput::new(PassMode::Full, 1 << 30));
        assert_eq!(plan, Vec::new(), "no divergence, no actions");
        assert_eq!(plan_pass(&PassInput::new(PassMode::Light, 1 << 30)), Vec::new());
    }

    #[test]
    fn plan_pass_full_housekeeping_comes_before_everything_else() {
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.stale_tmps = vec![PathBuf::from("/s/tmp.9.0")];
        input.orphan_audio = vec![PathBuf::from("/s/orphan.flac")];
        input.orphan_sidecars = vec![(sid("bad"), DeleteReason::CorruptSidecar)];
        input.window = vec![sid("want")];
        let plan = plan_pass(&input);
        assert_eq!(
            plan,
            vec![
                StoreAction::SweepTmp(PathBuf::from("/s/tmp.9.0")),
                StoreAction::DeleteFile(PathBuf::from("/s/orphan.flac")),
                StoreAction::DeleteEntry { id: sid("bad"), reason: DeleteReason::CorruptSidecar },
                StoreAction::Download { id: sid("want"), reason: DownloadReason::Window },
            ],
            "sweep, then orphans, then invalid entries, then work"
        );
    }

    #[test]
    fn plan_pass_missing_pins_backfill_newest_starred_first() {
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        // getStarred2 order IS newest-first, and the plan preserves it.
        input.pins = Some(PinSet::of_songs(vec![
            song("newest", 10, "flac", None),
            song("middle", 10, "flac", None),
            song("oldest", 10, "flac", None),
        ]));
        input.download_batch = 2;
        let plan = plan_pass(&input);
        assert_eq!(
            dls(&plan),
            vec![
                ("newest".to_string(), DownloadReason::Backfill),
                ("middle".to_string(), DownloadReason::Backfill),
            ],
            "newest-first, bounded by the batch"
        );
        // Nothing is deleted or marked: a missing pin is only ever a download.
        assert!(plan.iter().all(|a| matches!(a, StoreAction::Download { .. })));
    }

    #[test]
    fn plan_pass_fingerprint_drift_marks_stale_and_keeps_serving() {
        // Each leg of the fingerprint independently constitutes drift, and NONE of
        // them ever produces a delete: the old bytes keep playing until a verified
        // replacement renames over them.
        for (label, server) in [
            ("size", song("a", 999, "flac", Some("2024-05-01T12:00:00Z"))),
            ("suffix", song("a", 100, "mp3", Some("2024-05-01T12:00:00Z"))),
            ("created", song("a", 100, "flac", Some("2025-01-01T00:00:00Z"))),
        ] {
            let mut e = entry("a", 100, 0);
            e.pinned = true;
            let mut input = PassInput::new(PassMode::Full, 1 << 30);
            input.entries = vec![e];
            input.pins = Some(PinSet::of_songs(vec![server]));
            let plan = plan_pass(&input);
            assert!(
                plan.contains(&StoreAction::SetStale { id: sid("a"), stale: true }),
                "{label} drift must mark stale"
            );
            assert_eq!(
                dls(&plan),
                vec![("a".to_string(), DownloadReason::Stale)],
                "{label} drift schedules a replacement"
            );
            assert!(
                !plan.iter().any(|a| matches!(
                    a,
                    StoreAction::DeleteEntry { .. } | StoreAction::Evict(_) | StoreAction::DeleteFile(_)
                )),
                "{label} drift must NEVER delete - keep-until-replaced"
            );
        }
    }

    #[test]
    fn plan_pass_confirmed_fingerprint_clears_a_previous_stale_mark() {
        // Without this, one transient bad verdict would schedule a replacement
        // download on every pass forever.
        let mut e = entry("a", 100, 0);
        e.pinned = true;
        e.stale = true;
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.entries = vec![e];
        input.pins = Some(PinSet::of_songs(vec![song("a", 100, "flac", Some("2024-05-01T12:00:00Z"))]));
        let plan = plan_pass(&input);
        assert!(plan.contains(&StoreAction::SetStale { id: sid("a"), stale: false }));
        // It is still scheduled for replacement THIS pass (the mark was live when
        // the pass began); the cleared mark is what stops the next one.
        assert_eq!(dls(&plan), vec![("a".to_string(), DownloadReason::Stale)]);
        // A timezone re-rendering of the SAME instant is confirmation, not drift -
        // this is what keeps a server upgrade from invalidating the whole mirror.
        let mut e = entry("a", 100, 0);
        e.pinned = true;
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.entries = vec![e];
        input.pins = Some(PinSet::of_songs(vec![song("a", 100, "flac", Some("2024-05-01T14:00:00+02:00"))]));
        let plan = plan_pass(&input);
        assert_eq!(plan, Vec::new(), "same instant, different rendering => no work");
    }

    #[test]
    fn plan_pass_unknown_server_fingerprint_is_not_drift() {
        // A server that stops reporting `size` (or `suffix`, or `created`) must not
        // invalidate anything: missing information is not evidence of change.
        let mut e = entry("a", 100, 0);
        e.pinned = true;
        for mut server in [
            song("a", 100, "flac", None),
            song("a", 100, "flac", Some("2024-05-01T12:00:00Z")),
        ] {
            for drop_field in 0..3 {
                match drop_field {
                    0 => server.size = None,
                    1 => server.suffix = None,
                    _ => {}
                }
                let mut input = PassInput::new(PassMode::Full, 1 << 30);
                input.entries = vec![e.clone()];
                input.pins = Some(PinSet::of_songs(vec![server.clone()]));
                let plan = plan_pass(&input);
                assert!(
                    !plan.iter().any(|a| matches!(a, StoreAction::SetStale { stale: true, .. })),
                    "an unknown fingerprint must not read as drift"
                );
            }
        }
    }

    #[test]
    fn plan_pass_demotes_what_left_the_pin_set_and_promotes_what_joined() {
        let mut pinned = entry("was-starred", 100, 0);
        pinned.pinned = true;
        let unpinned = entry("now-starred", 100, 0);
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.entries = vec![pinned, unpinned];
        input.pins = Some(PinSet::of_songs(vec![song("now-starred", 100, "flac", Some("2024-05-01T12:00:00Z"))]));
        let plan = plan_pass(&input);
        // Demote to evictable, bytes KEPT - not an eager delete.
        assert!(plan.contains(&StoreAction::SetPinned { id: sid("was-starred"), pinned: false }));
        // Promote the opportunistic entry that is now starred.
        assert!(plan.contains(&StoreAction::SetPinned { id: sid("now-starred"), pinned: true }));
        assert!(
            !plan.iter().any(|a| matches!(
                a,
                StoreAction::DeleteEntry { .. } | StoreAction::Evict(_)
            )),
            "a demote never deletes; only budget pressure reclaims"
        );
        assert_eq!(dls(&plan), Vec::new(), "both are already cached");
    }

    #[test]
    fn plan_pass_transient_pin_failure_skips_all_verdicts() {
        // `pins: None` means the server flapped. NOTHING may be deleted, demoted,
        // or marked stale on that basis - transient-keeps-the-claim IS offline mode.
        let mut a = entry("pinned-and-gone-from-server", 100, 0);
        a.pinned = true;
        let mut b = entry("stale-already", 100, 0);
        b.pinned = true;
        b.stale = true;
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.entries = vec![a, b];
        input.pins = None;
        input.window = vec![sid("wanted")];
        let plan = plan_pass(&input);
        assert!(
            !plan.iter().any(|a| matches!(
                a,
                StoreAction::SetStale { .. } | StoreAction::SetPinned { .. }
                    | StoreAction::DeleteEntry { .. } | StoreAction::Evict(_)
            )),
            "no verdicts without an authoritative pin set: {plan:?}"
        );
        // The window still gets served, and a pre-existing stale mark still drives
        // its replacement attempt - work already decided is not undone by the flap.
        assert_eq!(
            dls(&plan),
            vec![
                ("wanted".to_string(), DownloadReason::Window),
                ("stale-already".to_string(), DownloadReason::Stale),
            ]
        );
    }

    #[test]
    fn plan_pass_suspect_replacement_is_first_and_never_a_delete() {
        let mut suspect = entry("suspect", 100, 0);
        suspect.suspect = true;
        suspect.pinned = true;
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.entries = vec![suspect];
        input.window = vec![sid("window-want")];
        input.pins = Some(PinSet::of_songs(vec![
            song("backfill-want", 100, "flac", None),
            song("suspect", 100, "flac", Some("2024-05-01T12:00:00Z")),
        ]));
        let plan = plan_pass(&input);
        assert_eq!(
            dls(&plan),
            vec![
                ("suspect".to_string(), DownloadReason::Suspect),
                ("window-want".to_string(), DownloadReason::Window),
                ("backfill-want".to_string(), DownloadReason::Backfill),
            ],
            "suspect gates a de-offered song's return, so it goes first"
        );
        // Its bytes are NOT scheduled for deletion: an offline pass can never
        // destroy what it cannot replace.
        assert!(!plan.iter().any(|a| matches!(
            a,
            StoreAction::DeleteEntry { .. } | StoreAction::Evict(_)
        )));
    }

    #[test]
    fn plan_pass_light_mode_scopes_to_window_and_suspect_only() {
        // A LIGHT kick (a track boundary, a suspect mark, a skip pin) must cost no
        // scan, no verdicts, no recency writes, and no eviction - even when handed a
        // full scan and a pin set, which is what makes the scoping structural.
        let mut suspect = entry("suspect", 100, 0);
        suspect.suspect = true;
        let mut stale = entry("stale", 100, 0);
        stale.stale = true;
        let mut dirty = entry("dirty", 100, 5);
        dirty.recency_dirty = true;
        let mut input = PassInput::new(PassMode::Light, 150);
        input.entries = vec![suspect, stale, dirty];
        input.window = vec![sid("wanted")];
        input.pins = Some(PinSet::of_songs(vec![song("unseen-pin", 100, "flac", None)]));
        input.stale_tmps = vec![PathBuf::from("/s/tmp.9.0")];
        input.orphan_audio = vec![PathBuf::from("/s/orphan.flac")];
        input.orphan_sidecars = vec![(sid("bad"), DeleteReason::CorruptSidecar)];
        let plan = plan_pass(&input);
        assert_eq!(
            plan,
            vec![
                StoreAction::Download { id: sid("suspect"), reason: DownloadReason::Suspect },
                StoreAction::Download { id: sid("wanted"), reason: DownloadReason::Window },
            ],
            "a light pass executes only what the user is about to hear"
        );
    }

    #[test]
    fn plan_pass_defer_bulk_keeps_window_and_suspect_work_moving() {
        // While the current track is a remote stream, bulk work waits so initial
        // sync cannot stall live playback on a thin link. The two latency-critical
        // categories never wait.
        let mut suspect = entry("suspect", 100, 0);
        suspect.suspect = true;
        let mut stale = entry("stale", 100, 0);
        stale.stale = true;
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.entries = vec![suspect, stale];
        input.window = vec![sid("wanted")];
        input.pins = Some(PinSet::of_songs(vec![song("backfill", 100, "flac", None)]));
        input.defer_bulk = true;
        assert_eq!(
            dls(&plan_pass(&input)),
            vec![
                ("suspect".to_string(), DownloadReason::Suspect),
                ("wanted".to_string(), DownloadReason::Window),
            ]
        );
        input.defer_bulk = false;
        assert_eq!(
            dls(&plan_pass(&input)),
            vec![
                ("suspect".to_string(), DownloadReason::Suspect),
                ("wanted".to_string(), DownloadReason::Window),
                ("stale".to_string(), DownloadReason::Stale),
                ("backfill".to_string(), DownloadReason::Backfill),
            ],
            "and the full priority order once playback is local"
        );
    }

    #[test]
    fn plan_pass_dedupes_by_id_keeping_the_highest_priority_reason() {
        // An id that is simultaneously suspect, in the window, stale, and a pin must
        // be downloaded ONCE, under the reason that gates the soonest audio.
        let mut e = entry("everything", 100, 0);
        e.suspect = true;
        e.stale = true;
        e.pinned = true;
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.entries = vec![e];
        input.window = vec![sid("everything")];
        input.pins = Some(PinSet::of_songs(vec![song("everything", 999, "flac", None)]));
        assert_eq!(
            dls(&plan_pass(&input)),
            vec![("everything".to_string(), DownloadReason::Suspect)]
        );
        // And a window id that is also an uncached pin is Window, not Backfill.
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.window = vec![sid("both")];
        input.pins = Some(PinSet::of_songs(vec![song("both", 100, "flac", None)]));
        assert_eq!(dls(&plan_pass(&input)), vec![("both".to_string(), DownloadReason::Window)]);
    }

    #[test]
    fn plan_pass_never_plans_work_for_an_unstorable_id() {
        // An id that cannot be a path component is excluded from the store entirely;
        // resolution falls through to streaming.
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.window = vec![SongId("../evil".into()), SongId("".into()), sid("ok")];
        input.pins = Some(PinSet::of_songs(vec![song("a/b", 100, "flac", None)]));
        assert_eq!(dls(&plan_pass(&input)), vec![("ok".to_string(), DownloadReason::Window)]);
    }

    #[test]
    fn plan_pass_batch_bound_caps_a_huge_backlog() {
        // A cold mirror must drain incrementally rather than pinning the task or
        // saturating the link in one burst.
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.pins = Some(PinSet::of_songs(
            (0..50).map(|i| song(&format!("p{i:02}"), 10, "flac", None)).collect(),
        ));
        input.download_batch = 4;
        assert_eq!(dls(&plan_pass(&input)).len(), 4);
        // The bound covers the latency-critical categories too, so no pass is
        // unbounded regardless of which divergence dominates.
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.entries = (0..10)
            .map(|i| {
                let mut e = entry(&format!("s{i}"), 10, 0);
                e.suspect = true;
                e
            })
            .collect();
        input.download_batch = 3;
        assert_eq!(dls(&plan_pass(&input)).len(), 3);
        // A zero batch plans no downloads at all but still does housekeeping.
        input.download_batch = 0;
        input.stale_tmps = vec![PathBuf::from("/s/tmp.1.0")];
        let plan = plan_pass(&input);
        assert_eq!(dls(&plan), Vec::new());
        assert_eq!(plan, vec![StoreAction::SweepTmp(PathBuf::from("/s/tmp.1.0"))]);
    }

    // ── eviction ordering and the protected set ─────────────────────────────

    #[test]
    fn plan_pass_under_the_cap_evicts_nothing() {
        let mut input = PassInput::new(PassMode::Full, 300);
        input.entries = vec![entry("a", 100, 1), entry("b", 100, 2)];
        assert_eq!(evictions(&plan_pass(&input)), Vec::<String>::new());
        // Exactly AT the cap is not over it.
        input.max_bytes = 200;
        assert_eq!(evictions(&plan_pass(&input)), Vec::<String>::new());
    }

    #[test]
    fn plan_pass_evicts_oldest_last_played_first_and_only_as_far_as_needed() {
        let mut input = PassInput::new(PassMode::Full, 250);
        input.entries = vec![
            entry("newest", 100, 300),
            entry("oldest", 100, 100),
            entry("middle", 100, 200),
        ];
        // 300 bytes over a 250 cap: evicting the single oldest is enough.
        assert_eq!(evictions(&plan_pass(&input)), vec!["oldest".to_string()]);
        // Tighter cap: keep going in LRU order, and stop the moment it fits.
        input.max_bytes = 150;
        assert_eq!(evictions(&plan_pass(&input)), vec!["oldest".to_string(), "middle".to_string()]);
        // Tighter still: everything evictable goes, and no more than that.
        input.max_bytes = 64;
        assert_eq!(
            evictions(&plan_pass(&input)),
            vec!["oldest".to_string(), "middle".to_string(), "newest".to_string()]
        );
    }

    #[test]
    fn plan_pass_eviction_order_is_deterministic_under_a_recency_tie() {
        // Two passes over identical state must pick identical victims, or the plan
        // is untestable and the log unreadable. The id is the tie-break.
        let mut input = PassInput::new(PassMode::Full, 100);
        input.entries = vec![entry("zeta", 100, 5), entry("alpha", 100, 5), entry("mid", 100, 5)];
        let first = evictions(&plan_pass(&input));
        assert_eq!(first, vec!["alpha".to_string(), "mid".to_string()]);
        input.entries.reverse();
        assert_eq!(evictions(&plan_pass(&input)), first, "order of input does not matter");
    }

    #[test]
    fn plan_pass_eviction_never_touches_the_window_or_the_skip_target() {
        // Every ABSOLUTE protection, all with the OLDEST possible recency so plain
        // LRU would pick them first. A RESIDENT pin outranks opportunistic bytes but
        // is no longer categorically exempt - the window and the skip target are.
        let mut pinned = entry("pinned", 100, 0);
        pinned.pinned = true;
        let mut input = PassInput::new(PassMode::Full, 400);
        input.entries = vec![
            pinned,
            entry("current", 100, 0),
            entry("upcoming", 100, 0),
            entry("skip-target", 100, 0),
            entry("evictable", 100, 999),
        ];
        // The window is current plus the queue-ahead upcoming entries; the skip
        // target is the handler's explicit pin, set when `pending_skip` is armed.
        input.window = vec![sid("current"), sid("upcoming")];
        input.protected = HashSet::from([sid("skip-target")]);
        input.pins = Some(PinSet::of_songs(vec![song("pinned", 100, "flac", Some("2024-05-01T12:00:00Z"))]));
        let plan = plan_pass(&input);
        assert_eq!(
            evictions(&plan),
            vec!["evictable".to_string()],
            "the opportunistic entry goes first, despite being the NEWEST"
        );
        // Squeeze the budget until the pin no longer fits under the pin ceiling. It
        // is then DEFERRED, and a deferred group is reclaimed after opportunistic
        // bytes and before nothing else - which is the whole point of dropping the
        // old blanket exemption: the store converges instead of settling forever
        // over budget while a warn repeats every fifteen minutes.
        input.max_bytes = 50;
        let plan = plan_pass(&input);
        assert_eq!(
            evictions(&plan),
            vec!["evictable".to_string(), "pinned".to_string()],
            "opportunistic first, then the deferred pin - and never the window"
        );
    }

    #[test]
    fn plan_pass_never_downloads_and_evicts_the_same_id() {
        // NO DOWNLOAD-EVICT THRASH. Suspect and stale replacements are scheduled for
        // entries that already exist on disk, and neither reason is protected from
        // eviction by pinning or the window - so an over-budget pass once emitted
        // Download and Evict for the SAME id, making the executor fetch a whole
        // original and then unlink it in the same walk (and leaving a de-offered song
        // permanently gone).
        for (tag, mut e) in [
            ("suspect", {
                let mut e = entry("s", 100, 0);
                e.suspect = true;
                e
            }),
            ("stale", {
                let mut e = entry("s", 100, 0);
                e.stale = true;
                e
            }),
        ] {
            e.pinned = false;
            let mut input = PassInput::new(PassMode::Full, 50);
            input.entries = vec![e];
            let plan = plan_pass(&input);
            assert!(
                !dls(&plan).is_empty(),
                "{tag}: the replacement is still scheduled"
            );
            assert_eq!(
                evictions(&plan),
                Vec::<String>::new(),
                "{tag}: an id this pass is downloading must not also be evicted"
            );
        }
        // The exclusion is scoped to ids the pass is ACTUALLY downloading: a cold
        // unpinned neighbour is still reclaimed in the same pass, so the budget is
        // not held hostage by one in-flight replacement.
        let mut suspect = entry("s", 100, 0);
        suspect.suspect = true;
        let mut input = PassInput::new(PassMode::Full, 50);
        input.entries = vec![suspect, entry("cold", 100, 1)];
        let plan = plan_pass(&input);
        assert_eq!(dls(&plan), vec![("s".to_string(), DownloadReason::Suspect)]);
        assert_eq!(evictions(&plan), vec!["cold".to_string()]);
        // Once the replacement has landed the entry is no longer suspect or stale, so
        // the next pass may reclaim it like anything else - the exclusion delays an
        // eviction by a pass, it does not grant immunity.
        let mut settled = PassInput::new(PassMode::Full, 50);
        settled.entries = vec![entry("s", 100, 0)];
        assert_eq!(evictions(&plan_pass(&settled)), vec!["s".to_string()]);
    }

    #[test]
    fn plan_pass_demoted_entries_become_evictable_in_the_same_pass() {
        // A song unstarred since the last pass must be reclaimable NOW, not one pass
        // later: the verdict's `pinned` flag is what eviction consults, not the
        // sidecar's pre-pass value.
        let mut was_pinned = entry("was-starred", 200, 0);
        was_pinned.pinned = true;
        let mut still_pinned = entry("still-starred", 100, 0);
        still_pinned.pinned = true;
        let mut input = PassInput::new(PassMode::Full, 150);
        input.entries = vec![was_pinned, still_pinned];
        input.pins = Some(PinSet::of_songs(vec![song("still-starred", 100, "flac", Some("2024-05-01T12:00:00Z"))]));
        let plan = plan_pass(&input);
        assert!(plan.contains(&StoreAction::SetPinned { id: sid("was-starred"), pinned: false }));
        assert_eq!(evictions(&plan), vec!["was-starred".to_string()]);
        // Without an authoritative pin set the sidecar's own flag stands, so nothing
        // is reclaimable and nothing is broken.
        input.pins = None;
        assert_eq!(evictions(&plan_pass(&input)), Vec::<String>::new());
    }

    #[test]
    fn plan_pass_pins_exceeding_the_ceiling_are_deferred_by_name_not_halted() {
        // THE REPLACEMENT FOR THE OLD OVERFLOW WARN. A pin set larger than the pin
        // ceiling used to halt the backfill ENTIRELY and warn an integer shortfall
        // every fifteen minutes forever, so it could never converge and the user
        // could never see which albums were missing. Now the frontier fits what it
        // can, refuses the rest BY NAME, and keeps serving the window.
        let mut a = entry("pin-a", 100, 0);
        a.pinned = true;
        let mut input = PassInput::new(PassMode::Full, 150);
        input.entries = vec![a];
        input.pins = Some(PinSet::of_songs(vec![
            song("pin-a", 100, "flac", Some("2024-05-01T12:00:00Z")),
            song("pin-b", 100, "flac", Some("2024-05-01T12:00:00Z")),
        ]));
        input.window = vec![sid("must-have")];
        let (plan, status) = plan_pass_with_status(&input);
        let status = status.expect("a full pass with pins publishes a status");
        // Ceiling is 150 - min(512 MiB, 37) = 113, so exactly one 100-byte group
        // fits and the second is refused whole.
        assert_eq!(status.resident_tracks, 1);
        let deferred: Vec<&RankedGroup> = status.deferred().collect();
        assert_eq!(
            deferred.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            vec!["pin-b"],
            "the shortfall is a NAMED group, not an integer"
        );
        assert_eq!(deferred[0].missing_tracks, 1);
        assert_eq!(deferred[0].missing_bytes, 100);
        // Both groups are ranked, won or lost, from the ONE vector - so the badge
        // count, the list and the full order are views of the same decision.
        assert_eq!(status.frontier.len(), 2);
        assert_eq!(status.deferred_count(), 1);
        // Nothing below the line is ever downloaded; the queue window still is.
        assert_eq!(
            dls(&plan),
            vec![("must-have".to_string(), DownloadReason::Window)],
            "a deferred group is never fetched, but the window always is"
        );
        // Not over budget yet (100 on disk against 150), so nothing is reclaimed.
        assert_eq!(evictions(&plan), Vec::<String>::new());
    }

    // ── the frontier: ONE ordering decides what is kept and what is fetched ──
    //
    // Everything in this block is about the single property the design exists for:
    // admission and eviction read the SAME ordering, so they cannot contradict each
    // other and a download-evict loop is structurally impossible rather than merely
    // unlikely.

    /// One pin group of `tracks` at `tier`. `kind` follows the tier the way the
    /// expansion produces it: a starred song is a group of one, everything else is
    /// an album.
    fn grp(tier: PinTier, id: &str, tracks: &[(&str, u64)]) -> PinGroup {
        PinGroup {
            kind: if tier == PinTier::Song { PinKind::Song } else { PinKind::Album },
            id: id.to_string(),
            name: format!("name-{id}"),
            tier,
            songs: tracks
                .iter()
                .map(|(t, size)| song(t, *size, "flac", Some("2024-05-01T12:00:00Z")))
                .collect(),
        }
    }

    /// The fixed calendar instant every neglect test measures back from:
    /// 2026-08-10T00:00:00Z. INJECTED through `PassInput::now_unix`, never read from
    /// the wall clock - which is what lets a 60-day threshold be swept
    /// deterministically instead of observed once and then rotting silently.
    const NOW_TEST: u64 = 1_786_320_000;

    /// One song last played exactly `days_ago` whole days before [`NOW_TEST`].
    /// `None` means the server has NO play record - the never-played case, which is
    /// both the common one and the one that counts as cold.
    fn aged_song(id: &str, size: u64, days_ago: Option<u32>) -> Song {
        let mut s = song(id, size, "flac", Some("2024-05-01T12:00:00Z"));
        match days_ago {
            None => {
                s.play_count = None;
                s.played = None;
            }
            Some(d) => {
                s.play_count = Some(1);
                let at = NOW_TEST.saturating_sub(d as u64 * 86_400);
                s.played = Some(
                    chrono::DateTime::from_timestamp(at as i64, 0)
                        .expect("a representable test instant")
                        .to_rfc3339(),
                );
            }
        }
        s
    }

    /// A pin group whose tracks carry EXPLICIT play ages, one per track, in whole
    /// days back from [`NOW_TEST`].
    fn aged_grp(
        tier: PinTier,
        id: &str,
        tracks: &[(&str, u64)],
        ages: &[Option<u32>],
    ) -> PinGroup {
        assert_eq!(tracks.len(), ages.len(), "one age per track");
        PinGroup {
            kind: if tier == PinTier::Song { PinKind::Song } else { PinKind::Album },
            id: id.to_string(),
            name: format!("name-{id}"),
            tier,
            songs: tracks
                .iter()
                .zip(ages)
                .map(|((t, size), age)| aged_song(t, *size, *age))
                .collect(),
        }
    }

    /// A pin group played TODAY throughout: maximally fresh, decile 0.
    fn hot_grp(tier: PinTier, id: &str, tracks: &[(&str, u64)]) -> PinGroup {
        let ages: Vec<Option<u32>> = tracks.iter().map(|_| Some(0)).collect();
        aged_grp(tier, id, tracks, &ages)
    }

    /// A pinned, on-disk entry - what a track that has already been mirrored looks
    /// like to a later pass.
    fn pinned_entry(id: &str, size: u64, last_played: u64) -> IndexEntry {
        let mut e = entry(id, size, last_played);
        e.pinned = true;
        e
    }

    /// Just the download ids of a plan, in plan order.
    fn dl_ids(plan: &[StoreAction]) -> Vec<String> {
        dls(plan).into_iter().map(|(id, _)| id).collect()
    }

    #[test]
    fn the_hand_picked_floor_keeps_starred_songs_the_neglect_order_would_drop() {
        // THE INVERSION THIS REPAIRS, and it is not hypothetical - it is what the
        // neglect key does to a real library. He stars a song WHILE LISTENING to it,
        // so a hand-picked star is played-today by construction and scores decile 0:
        // the most deliberate gesture sorts to the very bottom, beneath every album he
        // has not opened in months. Measured on his library at its real 3.01 GiB
        // ceiling, pure neglect order kept 7 of 47 hand-starred songs.
        //
        // Here: one 60-byte album nobody has played in 200 days, and three fresh
        // hand-starred songs of 10 bytes each. The ceiling fits the album OR the songs,
        // not both, and neglect order alone would spend all of it on the album.
        let mut input = PassInput::new(PassMode::Full, 100);
        input.now_unix = NOW_TEST;
        input.download_batch = 16;
        let pins = PinSet {
            groups: vec![
                aged_grp(PinTier::Album, "al-1", &[("a1", 30), ("a2", 30)], &[Some(200), Some(200)]),
                hot_grp(PinTier::Song, "so-1", &[("sng-a", 10)]),
                hot_grp(PinTier::Song, "so-2", &[("sng-b", 10)]),
                hot_grp(PinTier::Song, "so-3", &[("sng-c", 10)]),
            ],
        };
        input.pins = Some(pins.clone());
        let got = dl_ids(&plan_pass(&input));
        for id in ["sng-a", "sng-b", "sng-c"] {
            assert!(got.iter().any(|d| d == id), "{id} kept by the floor, got {got:?}");
        }

        // AND THE FLOOR IS A MINIMUM, NOT A CAP. Revert it and the songs vanish: this
        // is the assertion that bites, because everything above still passes under a
        // single-round walk that simply admits the album first.
        let ceiling = pin_ceiling(input.max_bytes);
        assert!(star_floor(ceiling) >= 30, "the floor must fit all three songs here");
        let by_id = HashMap::new();
        let f = Frontier::build(&pins, &by_id, ceiling, NOW_TEST);
        let ranked = f.ranked_groups();
        assert!(
            ranked
                .iter()
                .any(|g| g.tier != PinTier::Song && g.held_back_by_floor),
            "and whatever the reservation refused SAYS the reservation refused it, \
             rather than printing a shortfall against a ceiling that had room"
        );
    }

    #[test]
    fn the_floor_never_becomes_the_whole_policy_on_a_small_store() {
        // A FLAT reservation would quietly delete the album ranking on any store below
        // it: 2 GiB of floor against a 1 GiB budget reserves everything, and every
        // album is deferred forever with no line saying so. The fraction is what makes
        // the rule total rather than merely well-behaved at his size.
        for ceiling in [0u64, 1, 999, 1 << 20, 3 * GIB, 16 * GIB, u64::MAX] {
            let f = star_floor(ceiling);
            assert!(f <= ceiling / 2, "never more than half the mirror ({ceiling})");
            assert!(f <= STAR_FLOOR_BYTES, "never more than the absolute cap ({ceiling})");
        }
        assert_eq!(star_floor(0), 0, "no budget reserves nothing");
        // Large enough for the cap to bind rather than the fraction.
        assert_eq!(star_floor(16 * GIB), STAR_FLOOR_BYTES);
    }

    #[test]
    fn a_play_count_with_no_stamp_is_a_gap_in_the_records_not_neglect() {
        // Navidrome carries `playCount` and `played` independently and can hold one
        // without the other. Reading a missing STAMP as never-played then prints a line
        // that contradicts itself - "10/10 neglected, 4 plays, 0 never played" - on the
        // one surface whose whole job is to be checkable by eye.
        let mut s = aged_song("x", 10, None);
        assert!(is_cold(&s, NOW_TEST), "no stamp AND no plays really is neglect");
        s.play_count = Some(4);
        assert!(!is_cold(&s, NOW_TEST), "but a play count is a record that he played it");

        let g = PinGroup { songs: vec![s], ..grp(PinTier::Album, "al", &[]) };
        let sc = GroupScore::of(&g, &HashMap::from([("x", 10u64)]), NOW_TEST);
        assert_eq!(sc.never_played, 0, "and the printed evidence agrees with the verdict");
        assert_eq!(sc.cold_decile, 0);
        assert_eq!(sc.plays, 4);
    }

    #[test]
    fn at_equal_neglect_an_album_leads_a_loose_song_and_the_artist_fan_out_is_last() {
        // THE TIER'S REMAINING JOB, once neglect has spoken. This test REPLACES an
        // older one asserting song-then-album-then-artist: that was a preference, and
        // it is the exact preference the ask contradicted ("especially from albums I
        // favorited"). The ARTIST half of the old rule is untouched and is asserted
        // below, because it is structural rather than taste - a starred artist is the
        // only UNBOUNDED gesture.
        //
        // Every group here has identical neglect (one track, one stamp, all of it
        // COLD), so the decile is a tie by construction and the emphasis clause is the
        // only thing left to speak. The groups are declared in DELIBERATELY scrambled
        // order, so an implementation that merely preserved input order would fail.
        //
        // COLD is load-bearing, not decoration: the clause is about which NEGLECTED
        // music to prefer, so it speaks here and deliberately does not speak over
        // groups with no neglect at all - the second half of this test.
        let cold = |tier, id, track| aged_grp(tier, id, &[(track, 10)], &[Some(200)]);
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.now_unix = NOW_TEST;
        input.download_batch = 16;
        input.pins = Some(PinSet {
            groups: vec![
                cold(PinTier::Artist, "ar-1", "art-a"),
                cold(PinTier::Album, "al-1", "alb-a"),
                cold(PinTier::Song, "so-1", "sng-a"),
                cold(PinTier::Artist, "ar-2", "art-b"),
                cold(PinTier::Album, "al-2", "alb-b"),
                cold(PinTier::Song, "so-2", "sng-b"),
            ],
        });
        assert_eq!(
            dl_ids(&plan_pass(&input)),
            vec!["alb-a", "alb-b", "sng-a", "sng-b", "art-a", "art-b"],
            "albums, then loose starred songs, then the artist fan-out; within a tier, starred order"
        );

        // THE NARROWING, and it is the half that keeps the tier's promise. Played
        // today throughout: nothing is neglected, so the "especially from albums"
        // clause has nothing to emphasise and the cheapest, most precise gesture leads
        // instead. Without this the clause is a GATE over every group that ties at
        // decile 0 - 65 of 88 groups on one real library, with the ceiling cutting
        // straight through them - which defers essentially every hand-starred song.
        // The artist floor is untouched by the narrowing.
        input.pins = Some(PinSet {
            groups: vec![
                hot_grp(PinTier::Artist, "ar-1", &[("art-a", 10)]),
                hot_grp(PinTier::Album, "al-1", &[("alb-a", 10)]),
                hot_grp(PinTier::Song, "so-1", &[("sng-a", 10)]),
                hot_grp(PinTier::Album, "al-2", &[("alb-b", 10)]),
                hot_grp(PinTier::Song, "so-2", &[("sng-b", 10)]),
            ],
        });
        assert_eq!(
            dl_ids(&plan_pass(&input)),
            vec!["sng-a", "sng-b", "alb-a", "alb-b", "art-a"],
            "with NOTHING neglected the loose starred songs lead - and the unbounded \
             artist fan-out is still last"
        );

        // And the narrowing is per GROUP, not per pass: one cold track is enough to
        // put a group back under the ask's own clause, so the two halves coexist in
        // one order.
        input.pins = Some(PinSet {
            groups: vec![
                hot_grp(PinTier::Song, "so-1", &[("sng-a", 10)]),
                aged_grp(PinTier::Album, "al-1", &[("alb-a", 10)], &[Some(200)]),
                aged_grp(PinTier::Song, "so-2", &[("sng-b", 10)], &[Some(200)]),
                hot_grp(PinTier::Album, "al-2", &[("alb-b", 10)]),
            ],
        });
        assert_eq!(
            dl_ids(&plan_pass(&input)),
            vec!["alb-a", "sng-b", "sng-a", "alb-b"],
            "the two COLD groups rank by the ask (album, then song) and both lead the \
             two fresh ones, which rank song-first among themselves"
        );
    }

    #[test]
    fn the_artist_fan_out_is_floored_however_neglected_it_is() {
        // THE SINGLE MOST IMPORTANT ORDERING ASSERTION. A starred artist is the only
        // UNBOUNDED gesture - everything they have and everything they release - so
        // its fan-out must never be able to outrank a hand-picked one, whatever the
        // neglect signal says. Without the class floor, starring one prolific artist
        // with five hundred never-played albums would own the top of the order
        // forever and quietly starve every album and song he chose by hand.
        //
        // Here the artist album is MAXIMALLY neglected (never played at all, decile
        // 10) and the two hand-picked groups are maximally fresh (played today,
        // decile 0) - the strongest case the neglect key can make - and it still
        // loses to both.
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.now_unix = NOW_TEST;
        input.download_batch = 16;
        input.pins = Some(PinSet {
            groups: vec![
                aged_grp(PinTier::Artist, "ar", &[("art-a", 10)], &[None]),
                hot_grp(PinTier::Song, "so", &[("sng-a", 10)]),
                hot_grp(PinTier::Album, "al", &[("alb-a", 10)]),
            ],
        });
        let (plan, status) = plan_pass_with_status(&input);
        let status = status.expect("status");
        assert_eq!(
            dl_ids(&plan),
            vec!["sng-a", "alb-a", "art-a"],
            "the never-played artist album is still LAST, behind two tracks played today"
        );
        // The two hand-picked groups are both wholly fresh, so the ask's album clause
        // has nothing to emphasise between them and the loose song leads - which is
        // beside the point being tested here and asserted properly in
        // `at_equal_neglect_an_album_leads_a_loose_song_and_the_artist_fan_out_is_last`.
        // The FLOOR is what this test pins, and no neglect score can lift the artist
        // through it.
        //
        // And the evidence says exactly that, so the outcome is checkable by eye.
        let by = |id: &str| status.frontier.iter().find(|g| g.id == id).unwrap().clone();
        assert_eq!(by("ar").cold_decile, 10, "maximally neglected");
        assert_eq!(by("al").cold_decile, 0, "and it still loses to a decile 0 album");
        assert_eq!(by("ar").rank, 2);
    }

    #[test]
    fn neglect_outranks_the_tier_within_the_hand_picked_class() {
        // NEGLECT IS THE PRIMARY KEY, not a tie-break below the tier: the ask leads
        // with "music I haven't played for a long time" and makes the album clause
        // the "especially". So a wholly neglected starred SONG must beat a
        // freshly-played starred ALBUM, even though the album wins on a tie.
        //
        // This is the assertion that separates this design from one where the tier
        // gates and neglect merely decorates - flip the two key positions and it
        // fails.
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.now_unix = NOW_TEST;
        input.download_batch = 16;
        input.pins = Some(PinSet {
            groups: vec![
                hot_grp(PinTier::Album, "fresh-album", &[("fa-1", 10)]),
                aged_grp(PinTier::Song, "stale-song", &[("ss-1", 10)], &[Some(200)]),
            ],
        });
        assert_eq!(
            dl_ids(&plan_pass(&input)),
            vec!["ss-1", "fa-1"],
            "a song untouched for 200 days leads an album played today"
        );
        // And at EQUAL neglect the album takes it back - the emphasis clause speaking
        // exactly where it was meant to, on the tie the deciles manufacture.
        input.pins = Some(PinSet {
            groups: vec![
                aged_grp(PinTier::Song, "stale-song", &[("ss-1", 10)], &[Some(200)]),
                aged_grp(PinTier::Album, "stale-album", &[("sa-1", 10)], &[Some(70)]),
            ],
        });
        assert_eq!(
            dl_ids(&plan_pass(&input)),
            vec!["sa-1", "ss-1"],
            "both fully cold, so the ALBUM leads - and the 130-day gap between them does not speak"
        );
    }

    #[test]
    fn the_cold_share_is_bytes_weighted_over_the_groups_own_tracks() {
        // The decile is a share of the SPACE, not of the track count, because space is
        // what the decision is about. Here both albums have two of four tracks cold,
        // but one is cold in its BIG tracks - so it asks for more of its bytes on
        // behalf of music he has not heard, and it goes first.
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.now_unix = NOW_TEST;
        input.download_batch = 16;
        input.pins = Some(PinSet {
            groups: vec![
                // Cold in the SMALL tracks: 20 of 220 bytes = decile 0.
                aged_grp(
                    PinTier::Album,
                    "cold-in-the-small",
                    &[("s1", 10), ("s2", 10), ("s3", 100), ("s4", 100)],
                    &[Some(200), Some(200), Some(1), Some(1)],
                ),
                // Cold in the BIG tracks: 200 of 220 bytes = decile 9.
                aged_grp(
                    PinTier::Album,
                    "cold-in-the-large",
                    &[("l1", 10), ("l2", 10), ("l3", 100), ("l4", 100)],
                    &[Some(1), Some(1), Some(200), Some(200)],
                ),
            ],
        });
        let (plan, status) = plan_pass_with_status(&input);
        let status = status.expect("status");
        let by = |id: &str| status.frontier.iter().find(|g| g.id == id).unwrap().clone();
        assert_eq!(by("cold-in-the-large").cold_decile, 9);
        assert_eq!(by("cold-in-the-small").cold_decile, 0);
        assert_eq!(
            by("cold-in-the-large").cold_tracks,
            2,
            "the same TRACK count as the other - only the bytes differ"
        );
        assert_eq!(by("cold-in-the-small").cold_tracks, 2);
        assert_eq!(
            dl_ids(&plan)[0],
            "l1",
            "the album whose neglected half is the BIG half goes first"
        );
    }

    #[test]
    fn a_server_with_no_play_history_at_all_degrades_to_a_clean_tier_walk() {
        // A plain-Subsonic server, a fresh user, or Navidrome after a history wipe:
        // every track has no record, so every group is 100 % cold and every decile is
        // 10. The key must then COLLAPSE to (class, tier_rank, position, id) - a clean
        // tier-ordered walk - rather than hitting a cliff, an empty pin set or a
        // divide by zero. Asserted rather than hoped for.
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.now_unix = NOW_TEST;
        input.download_batch = 16;
        input.pins = Some(PinSet {
            groups: vec![
                aged_grp(PinTier::Artist, "ar-1", &[("art-a", 10)], &[None]),
                aged_grp(PinTier::Song, "so-1", &[("sng-a", 10)], &[None]),
                aged_grp(PinTier::Album, "al-1", &[("alb-a", 10)], &[None]),
                aged_grp(PinTier::Song, "so-2", &[("sng-b", 10)], &[None]),
                aged_grp(PinTier::Album, "al-2", &[("alb-b", 10)], &[None]),
            ],
        });
        let (plan, status) = plan_pass_with_status(&input);
        let status = status.expect("status");
        assert!(
            status.frontier.iter().all(|g| g.cold_decile == 10),
            "with no history anywhere, every group is wholly neglected"
        );
        assert_eq!(
            dl_ids(&plan),
            vec!["alb-a", "alb-b", "sng-a", "sng-b", "art-a"],
            "and the order is the tier walk, in pin-set position"
        );
        // The evidence is honest about WHY, so the surface does not claim a play it
        // never saw.
        let g = status.frontier.iter().find(|g| g.id == "al-1").unwrap();
        assert_eq!(g.never_played, 1);
        assert_eq!(g.plays, 0);
        assert_eq!(g.last_played_days, None);
        assert_eq!(g.oldest_played_days, None);
    }

    #[test]
    fn a_group_of_unknown_size_scores_zero_rather_than_dividing_by_zero() {
        // Neither the server nor the disk knows the size. There are no bytes to argue
        // over, so there is nothing to rank - and the arithmetic must not panic.
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.now_unix = NOW_TEST;
        input.download_batch = 16;
        let mut sizeless = aged_grp(PinTier::Album, "sizeless", &[("z1", 0), ("z2", 0)], &[None, None]);
        for s in &mut sizeless.songs {
            s.size = None;
        }
        input.pins = Some(PinSet { groups: vec![sizeless] });
        let (_, status) = plan_pass_with_status(&input);
        let status = status.expect("status");
        let g = &status.frontier[0];
        assert_eq!(g.bytes, 0);
        assert_eq!(g.cold_decile, 0, "no bytes, no share - and no divide by zero");
        assert_eq!(g.cold_tracks, 2, "the tracks are still honestly counted as cold");
    }

    /// A pin set with real spread in every dimension the key reads - three classes,
    /// mixed neglect, mixed sizes, some never-played - so a stability claim about it
    /// is a claim about a ranking that is actually deciding something.
    fn mixed_pin_set() -> PinSet {
        PinSet {
            groups: vec![
                aged_grp(
                    PinTier::Album,
                    "al-fresh",
                    &[("af1", 100), ("af2", 120), ("af3", 90)],
                    &[Some(2), Some(5), Some(1)],
                ),
                aged_grp(
                    PinTier::Album,
                    "al-half",
                    &[("ah1", 200), ("ah2", 30), ("ah3", 70)],
                    &[Some(120), Some(3), Some(9)],
                ),
                aged_grp(
                    PinTier::Album,
                    "al-neglected",
                    &[("an1", 150), ("an2", 150)],
                    &[None, Some(300)],
                ),
                aged_grp(PinTier::Song, "so-old", &[("s1", 80)], &[Some(95)]),
                aged_grp(PinTier::Song, "so-new", &[("s2", 60)], &[Some(4)]),
                aged_grp(
                    PinTier::Artist,
                    "ar-a",
                    &[("r1", 110), ("r2", 40)],
                    &[None, Some(58)],
                ),
                aged_grp(PinTier::Artist, "ar-b", &[("r3", 55)], &[Some(61)]),
            ],
        }
    }

    fn ranked_ids(status: &StoreStatus) -> Vec<&str> {
        status.frontier.iter().map(|g| g.id.as_str()).collect()
    }

    /// THE ACCEPTANCE ARTIFACT, replayed OFFLINE against a captured dump of one real
    /// library - and it downloads nothing, calls nothing and writes nothing.
    ///
    /// It runs the REAL [`plan_pass_with_status`] over a pin set rebuilt from a live
    /// `getStarred2` + `getAlbum` + `getArtist` capture, at the measured pin ceiling
    /// and with `now_unix` pinned to the capture date, and prints three things:
    ///
    /// 1. the resident/deferred split BY NAME with byte deltas, which is what a human
    ///    decides the DIRECTION on - the bet that "not played in a long time" should
    ///    win is his to confirm, not this code's to assume;
    /// 2. the DECILE HISTOGRAM, which is the falsifiability check on the one
    ///    constant: if it is degenerate (everything at 0 or 10) then 60 days is the
    ///    wrong line and gets re-derived FROM the histogram, not nudged until the
    ///    outcome looks nice;
    /// 3. never-played tracks left inside RESIDENT albums, which is the number that
    ///    would later reopen the partial-admission question.
    ///
    /// Ignored by default because it needs the capture. Point it at one with
    /// `HYPODJ_STATS_DUMP=<dir> cargo test -p hypodj-core -- --ignored replay`.
    #[test]
    #[ignore = "needs a captured live dump; see HYPODJ_STATS_DUMP"]
    fn replay_the_frontier_over_a_captured_real_library() {
        let Ok(dir) = std::env::var("HYPODJ_STATS_DUMP") else {
            eprintln!("set HYPODJ_STATS_DUMP to a directory holding starred_raw.json + expanded_raw.json");
            return;
        };
        let read = |name: &str| -> serde_json::Value {
            serde_json::from_slice(
                &std::fs::read(std::path::Path::new(&dir).join(name)).expect("dump file"),
            )
            .expect("dump json")
        };
        let starred = read("starred_raw.json");
        let expanded = read("expanded_raw.json");

        // The capture instant, so the day arithmetic is the one the capture saw.
        let now_unix = expanded["albums"]
            .as_object()
            .and_then(|m| m.values().find_map(|a| a["created"].as_str()))
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp() as u64)
            .expect("a created stamp to date the capture from");

        let to_song = |c: &serde_json::Value| Song {
            id: SongId(c["id"].as_str().unwrap_or_default().to_string()),
            title: c["title"].as_str().unwrap_or_default().to_string(),
            album: None,
            album_id: None,
            artist: None,
            track: None,
            duration_secs: None,
            cover_art: None,
            starred: c.get("starred").is_some(),
            musicbrainz_id: None,
            disc: None,
            year: None,
            genre: None,
            bitrate: None,
            comment: None,
            user_rating: None,
            composer: None,
            performer: None,
            size: c["size"].as_u64(),
            suffix: None,
            content_type: None,
            created: c["created"].as_str().map(str::to_string),
            play_count: c["playCount"].as_u64().map(|n| n as u32),
            played: c["played"].as_str().map(str::to_string),
        };
        let album_group = |id: &str, tier: PinTier, name: String| -> Option<PinGroup> {
            let a = expanded["albums"].get(id)?;
            Some(PinGroup {
                kind: PinKind::Album,
                id: id.to_string(),
                name,
                tier,
                songs: a["song"].as_array()?.iter().map(to_song).collect(),
            })
        };

        // The expansion's own shape: tier SONG per starred song, tier ALBUM per
        // starred album, tier ARTIST one group PER ALBUM of each starred artist.
        let mut groups: Vec<PinGroup> = Vec::new();
        for s in starred["song"].as_array().into_iter().flatten() {
            let song = to_song(s);
            groups.push(PinGroup {
                kind: PinKind::Song,
                id: song.id.0.clone(),
                name: song.title.clone(),
                tier: PinTier::Song,
                songs: vec![song],
            });
        }
        for a in starred["album"].as_array().into_iter().flatten() {
            let id = a["id"].as_str().unwrap_or_default();
            if let Some(g) = album_group(id, PinTier::Album, a["name"].as_str().unwrap_or(id).into())
            {
                groups.push(g);
            }
        }
        for ar in starred["artist"].as_array().into_iter().flatten() {
            let aid = ar["id"].as_str().unwrap_or_default();
            let aname = ar["name"].as_str().unwrap_or(aid);
            for al in expanded["artists"]
                .get(aid)
                .and_then(|v| v["album"].as_array())
                .into_iter()
                .flatten()
            {
                let id = al["id"].as_str().unwrap_or_default();
                let name = format!("{aname} - {}", al["name"].as_str().unwrap_or(id));
                if let Some(g) = album_group(id, PinTier::Artist, name) {
                    groups.push(g);
                }
            }
        }
        assert!(!groups.is_empty(), "the dump produced no pin groups");

        // His measured effective budget, and the ceiling the frontier actually walks.
        let mut input = PassInput::new(PassMode::Full, 10_468_982_784);
        input.now_unix = now_unix;
        input.pins = Some(PinSet { groups });
        let status = plan_pass_with_status(&input).1.expect("a full pass publishes");

        let mut hist = [0usize; 11];
        for g in &status.frontier {
            hist[g.cold_decile as usize] += 1;
        }
        let want: u64 = status.frontier.iter().map(|g| g.bytes).sum();
        println!("\nCOLD SHARE FIRST, replayed over the captured library");
        println!("rule: {}", frontier_rule());
        println!(
            "{} groups, {:.2} GiB wanted, ceiling {:.2} GiB",
            status.frontier.len(),
            want as f64 / 1024.0_f64.powi(3),
            pin_ceiling(input.max_bytes) as f64 / 1024.0_f64.powi(3),
        );
        println!("\ndecile histogram (degenerate = the 60-day line is wrong):");
        for (d, n) in hist.iter().enumerate() {
            println!("  {d:>2}/10  {n:>3}  {}", "#".repeat(*n));
        }
        println!("\nthe cut, by name:");
        for g in &status.frontier {
            println!(
                "  {:>3} {:<9} {:>2}/10 {:>10} B  {:<7} {}",
                g.rank + 1,
                g.standing.label(),
                g.cold_decile,
                g.bytes,
                g.tier.label(),
                g.name,
            );
        }
        // An UPPER BOUND, said out loud: a track two deferred groups both want is
        // counted by both, because each of them really is missing it (see
        // `RankedGroup::missing_bytes`). Stated rather than quietly conflated.
        let short: u64 = status.deferred().map(|g| g.missing_bytes).sum();
        println!(
            "\ndeferred: {} groups, at most {:.2} GiB (a track two deferred groups \
             both want is counted by both)",
            status.deferred_count(),
            short as f64 / 1024.0_f64.powi(3)
        );
        // The number that would REOPEN the partial-admission question: never-played
        // tracks sitting inside albums the frontier decided to keep whole. The BYTE
        // figure is deliberately the cold bytes, which is an UPPER BOUND on the
        // never-played ones - `RankedGroup` carries no never-played byte total, and
        // adding one to the wire for a number only this replay prints would not earn
        // its keep. Stated rather than quietly conflated.
        let (nt, nb) = status
            .frontier
            .iter()
            .filter(|g| g.standing == GroupStanding::Resident)
            .fold((0usize, 0u64), |(t, b), g| (t + g.never_played, b + g.cold_bytes));
        println!(
            "never-played tracks inside RESIDENT groups: {nt}, holding at most {:.2} GiB \
             (cold bytes, an upper bound on the never-played ones)",
            nb as f64 / 1024.0_f64.powi(3)
        );

        // The one assertion, and it is the falsifiability check rather than a
        // rubber stamp: a histogram piled entirely into one bucket means the constant
        // discriminates nothing and must be re-derived.
        let occupied = hist.iter().filter(|n| **n > 0).count();
        assert!(
            occupied >= 3,
            "the decile histogram is degenerate ({occupied} buckets used) - \
             STALE_PLAY_DAYS must be re-derived FROM this distribution"
        );
    }

    #[test]
    fn the_order_is_identical_between_passes_over_identical_input() {
        // THE PROPERTY THAT MATTERS MOST, more than the ranking itself: an order that
        // reshuffles makes the mirror download and evict the same albums forever,
        // which is strictly WORSE than the arbitrary order it replaces. A ceiling
        // that actually bites, so the resident/deferred split is a real cut and not a
        // vacuous "everything fits".
        let mut input = PassInput::new(PassMode::Full, 900);
        input.now_unix = NOW_TEST;
        input.download_batch = 16;
        input.pins = Some(mixed_pin_set());

        let (plan_a, status_a) = plan_pass_with_status(&input);
        let status_a = status_a.expect("status");
        let (plan_b, status_b) = plan_pass_with_status(&input);
        let status_b = status_b.expect("status");
        assert_eq!(status_a, status_b, "same input, byte-identical self-description");
        assert_eq!(plan_a, plan_b, "and the identical plan");
        assert!(
            status_a.deferred_count() > 0 && status_a.resident_tracks > 0,
            "the ceiling must actually cut, or this proves nothing"
        );

        // And it is a function of the STATE, not of the order the pin set arrived in.
        // The server is free to hand back the same starred set in a different
        // sequence; that must not move the line, or the mirror churns for nothing.
        let mut shuffled = input.clone();
        let mut groups = mixed_pin_set().groups;
        groups.reverse();
        shuffled.pins = Some(PinSet { groups });
        let (_, status_c) = plan_pass_with_status(&shuffled);
        let status_c = status_c.expect("status");
        assert_eq!(
            ranked_ids(&status_c)
                .iter()
                .filter(|id| {
                    status_c
                        .frontier
                        .iter()
                        .any(|g| &g.id == *id && g.standing == GroupStanding::Resident)
                })
                .count(),
            status_a
                .frontier
                .iter()
                .filter(|g| g.standing == GroupStanding::Resident)
                .count(),
            "the same groups are resident whatever order they arrived in"
        );
        let mut a_deferred: Vec<&str> = status_a.deferred().map(|g| g.id.as_str()).collect();
        let mut c_deferred: Vec<&str> = status_c.deferred().map(|g| g.id.as_str()).collect();
        a_deferred.sort_unstable();
        c_deferred.sort_unstable();
        assert_eq!(a_deferred, c_deferred, "and the same groups are refused");
    }

    #[test]
    fn a_groups_neglect_only_ever_grows_and_never_falls_back_on_its_own() {
        // THE STABILITY ARGUMENT, swept rather than asserted by hand. Absent a new
        // play a track's age only increases, so `cold` is a ONE-WAY LATCH: every
        // group's decile is monotone NON-DECREASING over time and takes at most
        // eleven values ever.
        //
        // THAT ALONE IS NOT STABILITY, and this test used to pretend it was. Per-group
        // monotonicity says nothing about a PAIR: two groups whose deciles ratchet on
        // different days hand the lead back and forth, and the old bound here (a
        // whole-order change count of at most seven over 400 days) could not see it -
        // the real counterexample produced six reversals and a full download-evict
        // cycle while passing. What this test still owns is the LATCH; the pairwise
        // property is owned by
        // `a_pair_that_ratchets_past_each_other_hands_over_once_not_forever`, which
        // runs the counterexample against the store state itself.
        //
        // 400 days at one-day steps, over a fixture whose ages straddle the line.
        let mut input = PassInput::new(PassMode::Full, 900);
        input.download_batch = 16;
        input.pins = Some(mixed_pin_set());

        let mut last_decile: std::collections::HashMap<String, u8> = Default::default();
        let mut moved = 0usize;
        for day in 0..400u64 {
            input.now_unix = NOW_TEST + day * 86_400;
            let (_, status) = plan_pass_with_status(&input);
            let status = status.expect("status");
            for g in &status.frontier {
                if let Some(prev) = last_decile.get(&g.id) {
                    assert!(
                        g.cold_decile >= *prev,
                        "day {day}: {} fell from decile {prev} to {} - neglect is a \
                         one-way latch and must never reverse without a play",
                        g.id,
                        g.cold_decile
                    );
                    if g.cold_decile > *prev {
                        moved += 1;
                    }
                }
                last_decile.insert(g.id.clone(), g.cold_decile);
            }
        }
        assert!(moved > 0, "the sweep must actually cross the line, or it proves nothing");
        // Eleven values per group, seven groups: the latch bounds the number of
        // ratchets over ANY span, not just this one.
        assert!(moved <= 7 * 10, "a decile moved {moved} times - the latch is not latching");
    }

    /// Sweep `days` days over one pin set, feeding each pass's plan back into the
    /// entries so the next pass sees the store the previous one actually produced.
    /// Returns (downloads, evictions, order reversals per pair, the day-by-day log).
    ///
    /// THE POINT: a churn claim is about the STORE, not about a printed order, so it
    /// can only be measured by closing the loop. A sweep that never sets
    /// `input.entries` cannot observe a single download or eviction and therefore
    /// cannot fail on churn - which is exactly how the leapfrog defect shipped green.
    fn sweep_days(pins: &PinSet, max_bytes: u64, days: u64) -> (usize, usize, usize, Vec<String>) {
        let mut input = PassInput::new(PassMode::Full, max_bytes);
        input.download_batch = 64;
        input.pins = Some(pins.clone());
        let mut on_disk: Vec<IndexEntry> = Vec::new();
        let (mut downloads, mut evictions, mut reversals) = (0usize, 0usize, 0usize);
        let mut last_order: Vec<String> = Vec::new();
        let mut log: Vec<String> = Vec::new();
        for day in 0..days {
            input.now_unix = NOW_TEST + day * 86_400;
            input.entries = on_disk.clone();
            let (plan, status) = plan_pass_with_status(&input);
            let status = status.expect("status");
            let (mut got, mut gone) = (Vec::new(), Vec::new());
            for a in &plan {
                match a {
                    StoreAction::Download { id, .. } => got.push(id.0.clone()),
                    StoreAction::Evict(id) => gone.push(id.0.clone()),
                    _ => {}
                }
            }
            downloads += got.len();
            evictions += gone.len();
            if !got.is_empty() || !gone.is_empty() {
                log.push(format!("day {day}: downloaded {got:?}, evicted {gone:?}"));
            }
            on_disk.retain(|e| !gone.contains(&e.id.0));
            for id in &got {
                let size = pins
                    .songs()
                    .find(|s| &s.id.0 == id)
                    .and_then(|s| s.size)
                    .unwrap_or(0);
                on_disk.push(pinned_entry(id, size, input.now_unix));
            }
            let order: Vec<String> = status.frontier.iter().map(|g| g.id.clone()).collect();
            if !last_order.is_empty() && order != last_order {
                reversals += 1;
                log.push(format!("day {day}: order {last_order:?} -> {order:?}"));
            }
            last_order = order;
        }
        (downloads, evictions, reversals, log)
    }

    #[test]
    fn a_pair_that_ratchets_past_each_other_hands_over_once_not_forever() {
        // THE COUNTEREXAMPLE THAT KILLED "no pair can oscillate". Two four-track
        // albums of identical class, tier and size whose tracks cross the 60-day line
        // on INTERLEAVED days. Both deciles are monotone non-decreasing throughout -
        // the property the old argument rested on - and yet they ratchet past each
        // other: A leads at 2 v 2, B takes it at 2 v 5, A takes it BACK at 5 v 5
        // because the tie fell through to arrival position, B at 5 v 7, A at 7 v 7,
        // and so on.
        //
        // With a ceiling that fits exactly one of them, every one of those reversals
        // is a whole album deleted and the other one re-fetched. Measured on this
        // planner before the fix: SIX reversals, 28 downloads and 24 evictions in
        // seventy days, for eight tracks - on real data that is GiB of re-fetching an
        // album he already had, against a disk that has hit 100 % once.
        //
        // The incumbency clause makes a tie worth zero bytes of movement, so the lead
        // can change only when the challenger is STRICTLY more neglected. Here that
        // happens exactly once, at day 5, and never again.
        let pins = PinSet {
            groups: vec![
                aged_grp(
                    PinTier::Album,
                    "A",
                    &[("a1", 25), ("a2", 25), ("a3", 25), ("a4", 25)],
                    &[Some(61), Some(50), Some(20), Some(10)],
                ),
                aged_grp(
                    PinTier::Album,
                    "B",
                    &[("b1", 25), ("b2", 25), ("b3", 25), ("b4", 25)],
                    &[Some(61), Some(55), Some(45), Some(15)],
                ),
            ],
        };
        // 134 puts the pin ceiling at 101: one whole four-track album fits, never two.
        let (downloads, evictions, reversals, log) = sweep_days(&pins, 134, 400);
        assert_eq!(
            reversals, 1,
            "the lead changed {reversals} times - it may change only on a STRICT \
             takeover, never on a tie the quantizer manufactured: {log:#?}"
        );
        assert_eq!(
            (downloads, evictions),
            (8, 4),
            "eight tracks, fetched once each, with one handover - not a cycle: {log:#?}"
        );
        // And the handover really is the strict crossing, not an accident of the
        // ceiling: B leads from day 5 with a higher decile than A, and holds it.
        let mut input = PassInput::new(PassMode::Full, 134);
        input.pins = Some(pins.clone());
        input.now_unix = NOW_TEST + 5 * 86_400;
        let status = plan_pass_with_status(&input).1.expect("status");
        assert_eq!(ranked_ids(&status), vec!["B", "A"]);
        assert!(status.frontier[0].cold_decile > status.frontier[1].cold_decile);
    }

    #[test]
    fn nothing_the_mirror_holds_is_re_fetched_by_the_passage_of_time_alone() {
        // The same closed loop over the spread fixture: 400 days of NOTHING HAPPENING
        // but the calendar moving. Every group's neglect ratchets, the order settles
        // repeatedly, and the store must still move each byte at most once - a mirror
        // that re-fetches what it already has because a decile ticked is the failure
        // mode the frontier exists to make structurally impossible.
        let pins = mixed_pin_set();
        let (downloads, evictions, _, log) = sweep_days(&pins, 900, 400);
        let unique: usize = {
            let mut ids: Vec<&str> = pins.songs().map(|s| s.id.0.as_str()).collect();
            ids.sort_unstable();
            ids.dedup();
            ids.len()
        };
        assert!(
            downloads <= unique,
            "{downloads} downloads for {unique} distinct tracks over 400 days - \
             something was fetched twice: {log:#?}"
        );
        assert_eq!(evictions, 0, "and nothing it fetched was ever taken back: {log:#?}");
        assert!(downloads > 0, "the sweep must actually fetch something, or it proves nothing");
    }

    #[test]
    fn playing_a_track_is_the_one_thing_that_moves_a_group_down() {
        // The mirror answers to HIM, not to a timer. A play lowers a group's cold
        // share, which is the one direction that can demote a group - and it is
        // exactly the responsiveness wanted, because what he plays he gets on demand
        // anyway (the window arm is not budget-gated), while what he never plays is
        // the only thing the mirror can give him that playing cannot.
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.now_unix = NOW_TEST;
        input.download_batch = 16;
        let neglected = |ages: &[Option<u32>]| {
            aged_grp(PinTier::Album, "al", &[("t1", 100), ("t2", 100)], ages)
        };
        input.pins = Some(PinSet {
            groups: vec![
                neglected(&[Some(300), Some(300)]),
                aged_grp(PinTier::Album, "other", &[("o1", 100), ("o2", 100)], &[Some(70), Some(1)]),
            ],
        });
        let (_, before) = plan_pass_with_status(&input);
        let before = before.expect("status");
        assert_eq!(ranked_ids(&before), vec!["al", "other"]);
        assert_eq!(before.frontier[0].cold_decile, 10);

        // He plays both of its tracks today. Same pass, same everything else.
        input.pins = Some(PinSet {
            groups: vec![
                neglected(&[Some(0), Some(0)]),
                aged_grp(PinTier::Album, "other", &[("o1", 100), ("o2", 100)], &[Some(70), Some(1)]),
            ],
        });
        let (_, after) = plan_pass_with_status(&input);
        let after = after.expect("status");
        assert_eq!(
            ranked_ids(&after),
            vec!["other", "al"],
            "playing it moved it DOWN, which is the only direction a play can move anything"
        );
        assert_eq!(after.frontier[1].cold_decile, 0);
    }

    #[test]
    fn no_group_is_ever_half_admitted_at_any_score() {
        // WHOLE-ALBUM ADMISSION SURVIVES THE RANKING. The play signal decides ORDER
        // and nothing else: half an album, with no protocol-level way to say which
        // half, is worse than none. Swept across the whole budget range so it holds
        // at every possible position of the line rather than at one convenient one.
        let pins = mixed_pin_set();
        let mut any_cut = false;
        for max_bytes in (100u64..1400).step_by(37) {
            let mut input = PassInput::new(PassMode::Full, max_bytes);
            input.now_unix = NOW_TEST;
            input.download_batch = 64;
            input.pins = Some(pins.clone());
            let (plan, status) = plan_pass_with_status(&input);
            let status = status.expect("status");
            let fetched: HashSet<String> = dl_ids(&plan).into_iter().collect();
            for g in &pins.groups {
                let got = g.songs.iter().filter(|s| fetched.contains(&s.id.0)).count();
                assert!(
                    got == 0 || got == g.songs.len(),
                    "budget {max_bytes}: group {} is HALF fetched ({got} of {})",
                    g.id,
                    g.songs.len()
                );
            }
            // A named shortfall must name real bytes, at every position of the line.
            for d in status.deferred() {
                assert!(d.missing_bytes > 0, "budget {max_bytes}: {} names nothing", d.id);
                any_cut = true;
            }
        }
        assert!(any_cut, "the sweep must actually refuse something somewhere");
    }

    #[test]
    fn the_reported_reason_is_the_key_the_comparator_sorted_on() {
        // EXPLAIN FIDELITY, as a property rather than a hope. The explanation is read
        // off the same vector the walk used, so re-deriving the key from the reported
        // evidence must reproduce the reported order exactly. If a display path ever
        // recomputed a score of its own, this is where the two would part.
        let mut input = PassInput::new(PassMode::Full, 900);
        input.now_unix = NOW_TEST;
        input.download_batch = 16;
        input.pins = Some(mixed_pin_set());
        // Some bytes already here, so the INCUMBENCY clause is live rather than
        // uniformly false - a fidelity check over a key with one clause switched off
        // proves nothing about that clause.
        input.entries = vec![pinned_entry("ah1", 200, 10), pinned_entry("s1", 80, 20)];
        let (_, status) = plan_pass_with_status(&input);
        let status = status.expect("status");

        // The key, rebuilt from ONLY what a reader can see on the wire - INCLUDING the
        // incumbency clause and the emphasis narrowing, both of which are readable
        // off `held` and `cold_bytes`. If a clause ever decided without being
        // reported, this is where the reconstruction stops matching.
        let key = |g: &RankedGroup| {
            (
                g.tier.class(),
                10 - g.cold_decile,
                u8::from(!g.held),
                emphasis_rank(g.tier, g.cold_bytes > 0),
            )
        };
        for pair in status.frontier.windows(2) {
            assert!(
                key(&pair[0]) <= key(&pair[1]),
                "{} (rank {}) reports a WORSE key than {} below it: {:?} vs {:?}",
                pair[0].id,
                pair[0].rank,
                pair[1].id,
                key(&pair[0]),
                key(&pair[1])
            );
            assert_eq!(pair[1].rank, pair[0].rank + 1, "ranks are the positions, densely");
        }
        // And each group's own numbers are internally consistent - a cold track count
        // that exceeded the track count, or a decile that did not follow from the
        // bytes, would mean the printed reason was assembled rather than read.
        for g in &status.frontier {
            assert!(g.cold_tracks <= g.tracks);
            assert!(g.never_played <= g.cold_tracks);
            assert!(g.cold_bytes <= g.bytes);
            assert!(g.cold_decile <= 10);
            let expected = if g.bytes == 0 { 0 } else { (g.cold_bytes * 100 / g.bytes / 10) as u8 };
            assert_eq!(g.cold_decile, expected, "{}: the decile follows from the bytes", g.id);
            if let (Some(last), Some(oldest)) = (g.last_played_days, g.oldest_played_days) {
                assert!(last <= oldest, "{}: freshest cannot be older than stalest", g.id);
            }
        }
    }

    #[test]
    fn a_refused_group_names_what_it_lost_to_and_by_how_much() {
        // The literal answer to "why not this one": you needed N more bytes at your
        // position, and THIS is what took the space. Both taken at the refusal site,
        // and asserted BY VALUE - a plausible-looking blame string that named the
        // wrong album would be worse than none, because he would go and look at it.
        let mut input = PassInput::new(PassMode::Full, 400);
        input.now_unix = NOW_TEST;
        input.download_batch = 16;
        input.pins = Some(PinSet {
            groups: vec![
                // Wholly neglected, so it leads and takes 200 of the 300 ceiling.
                aged_grp(PinTier::Album, "winner", &[("w1", 200)], &[Some(300)]),
                // Also cold but shorter, so it comes second - and 150 does not fit in
                // the 100 that is left.
                aged_grp(PinTier::Album, "loser", &[("l1", 75), ("l2", 75)], &[Some(300), Some(300)]),
            ],
        });
        let (_, status) = plan_pass_with_status(&input);
        let status = status.expect("status");
        let loser = status.frontier.iter().find(|g| g.id == "loser").unwrap();
        assert_eq!(loser.standing, GroupStanding::Deferred);
        assert_eq!(loser.over_by, 50, "150 wanted against the 100 left under the ceiling");
        assert_eq!(
            loser.blocked_by.as_deref(),
            Some("name-winner"),
            "and it names the group that actually took the space"
        );
        // The winner fitted, so it missed by nothing and blames nobody.
        let winner = status.frontier.iter().find(|g| g.id == "winner").unwrap();
        assert_eq!((winner.over_by, winner.blocked_by.clone()), (0, None));
    }

    #[test]
    fn the_frontier_refuses_a_group_whole_and_keeps_walking_to_a_smaller_one() {
        // WHOLE OR ABSENT, and best-effort fill. On real data this is not theoretical:
        // three albums are 4 GiB of a 12.3 GiB want, and per-track admission would let
        // those three eat the budget in arrival order. The frontier refuses them BY
        // NAME and fits the rest.
        let mut input = PassInput::new(PassMode::Full, 200);
        input.download_batch = 16;
        input.pins = Some(PinSet {
            groups: vec![
                // First in line and far too big: 3 tracks of 100 against a ceiling of 150.
                grp(PinTier::Album, "huge", &[("h1", 100), ("h2", 100), ("h3", 100)]),
                // Smaller, later, and it still lands.
                grp(PinTier::Album, "small", &[("s1", 50), ("s2", 50)]),
            ],
        });
        let (plan, status) = plan_pass_with_status(&input);
        assert_eq!(
            dl_ids(&plan),
            vec!["s1", "s2"],
            "the big group is refused ENTIRE - never a partial album - and the walk continues"
        );
        let status = status.expect("a full pass with pins publishes a status");
        assert_eq!(status.resident_tracks, 2);
        assert_eq!(
            status
                .deferred()
                .map(|d| (d.id.as_str(), d.missing_tracks, d.missing_bytes))
                .collect::<Vec<_>>(),
            vec![("huge", 3, 300)],
            "and the shortfall is one NAMED album, not an integer"
        );
        // WHY it lost, taken at its own turn: it was charged 300 against a 150
        // ceiling with nothing yet admitted, so it was 150 short and there was no
        // earlier group to blame. Read off the same integers the walk used.
        let huge = status.frontier.iter().find(|g| g.id == "huge").unwrap();
        assert_eq!(huge.over_by, 150);
        assert_eq!(huge.blocked_by, None, "nothing was admitted before it to lose to");
        let small = status.frontier.iter().find(|g| g.id == "small").unwrap();
        assert_eq!(small.standing, GroupStanding::Resident);
        assert_eq!(small.over_by, 0, "a group that fitted missed by nothing");
    }

    #[test]
    fn a_shared_track_is_charged_once_to_whichever_group_reaches_it_first() {
        // 20 of one real user's 47 starred songs are also starred-album tracks, so
        // this case is common rather than a corner. FIRST CLAIM WINS in frontier
        // order, and the invariant that matters is the one that never moved: it is
        // charged and downloaded exactly ONCE, never once per claiming group, or
        // albums that actually fit would be deferred for bytes nobody spends twice.
        //
        // WHAT DID MOVE, deliberately: at equal neglect the ALBUM now runs first, so
        // it is the album that charges the shared track, and the starred-song group
        // is then COVERED - every one of its tracks is already held BY A RESIDENT
        // group, so nothing is missing and it is not named as a shortfall. This test
        // was rewritten with the policy rather than patched around it; the numbers
        // below are the new policy stated out loud, not an accident of it.
        //
        // Both groups are wholly COLD, which is what puts them under the ask's album
        // clause at all - see `emphasis_rank`.
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.now_unix = NOW_TEST;
        input.download_batch = 16;
        input.pins = Some(PinSet {
            groups: vec![
                aged_grp(
                    PinTier::Album,
                    "al",
                    &[("shared", 100), ("only-album", 100)],
                    &[Some(200), Some(200)],
                ),
                aged_grp(PinTier::Song, "shared", &[("shared", 100)], &[Some(200)]),
            ],
        });
        let (plan, status) = plan_pass_with_status(&input);
        let status = status.expect("status");
        assert_eq!(
            status.tier_tracks[PinTier::Album as usize],
            2,
            "the album reached the shared track first and is charged for it"
        );
        assert_eq!(status.tier_tracks[PinTier::Song as usize], 0);
        assert_eq!(status.resident_tracks, 2, "and it is counted ONCE, not twice");
        assert_eq!(status.tier_bytes[PinTier::Album as usize], 200);
        assert_eq!(status.tier_bytes[PinTier::Song as usize], 0);
        // Downloaded once, at the album's position - not once per claiming group.
        assert_eq!(dl_ids(&plan), vec!["shared", "only-album"]);
        // The song group is COVERED, not deferred: naming it as a shortfall would
        // claim something is missing when the album holds every byte of it.
        let sng = status.frontier.iter().find(|g| g.tier == PinTier::Song).unwrap();
        assert_eq!(sng.standing, GroupStanding::Covered);
        assert_eq!(sng.missing_tracks, 0);
        assert_eq!(status.deferred_count(), 0, "nothing is actually missing");
    }

    #[test]
    fn a_track_only_a_deferred_group_wants_leaves_its_song_group_deferred_too() {
        // COVERED MEANS HELD, and by a RESIDENT group - never merely "some other group
        // listed it first". The distinction is the whole answer to "what did not fit?":
        // a starred song sitting under a starred ALBUM that itself lost is on NOBODY's
        // disk, so filing it as Covered drops it out of `deferred()` and therefore out
        // of the badge count, the `Deferred:` lines, `dj store` and the journal. On one
        // real library that hid twelve of his forty-seven hand-starred songs - by name:
        // "Interlude", "Survival", "Sleeper Car" - behind deferred albums.
        //
        // The ceiling here fits ONE 100-byte group. The album that wants the shared
        // track is refused, so the shared track is unheld, so the song group that also
        // wants it is a real shortfall and must say so.
        let mut input = PassInput::new(PassMode::Full, 134);
        input.now_unix = NOW_TEST;
        input.download_batch = 16;
        input.pins = Some(PinSet {
            groups: vec![
                aged_grp(PinTier::Album, "winner", &[("w1", 100)], &[None]),
                aged_grp(PinTier::Album, "big", &[("shared", 100), ("other", 100)], &[None, None]),
                aged_grp(PinTier::Song, "shared-song", &[("shared", 100)], &[None]),
            ],
        });
        let (plan, status) = plan_pass_with_status(&input);
        let status = status.expect("status");
        assert_eq!(dl_ids(&plan), vec!["w1"], "only the one group fits");
        let by = |id: &str| status.frontier.iter().find(|g| g.id == id).unwrap().clone();
        assert_eq!(by("big").standing, GroupStanding::Deferred);
        assert_eq!(
            by("shared-song").standing,
            GroupStanding::Deferred,
            "its track is wanted by a group that lost too - that is missing, not covered"
        );
        assert_eq!(
            (by("shared-song").missing_tracks, by("shared-song").missing_bytes),
            (1, 100),
            "and it names its own shortfall rather than reporting zero"
        );
        let named: Vec<&str> = status.deferred().map(|g| g.id.as_str()).collect();
        assert_eq!(named, vec!["big", "shared-song"], "BOTH are named, in frontier order");
        assert_eq!(status.deferred_count(), 2);
    }

    #[test]
    fn a_group_evicted_from_below_the_line_is_never_re_admitted() {
        // THE ANTI-THRASH PROPERTY, and the reason this design has ONE mechanism
        // rather than two. With a separate tiered victim filter and a separate tiered
        // admission loop, evicting a big album to reclaim a little space makes that
        // album the newest-starred backfill candidate next pass, with fresh headroom:
        // it downloads, overruns, and is evicted again - forever. A download loop
        // against the server and the disk, and exactly the shape of bug that fills a
        // disk slowly.
        //
        // Here the deferred group is BELOW THE LINE, so eviction takes it and
        // admission refuses it, from the same ordering.
        //
        // The budget is 400, so the pin ceiling is 400 - min(512 MiB, 100) = 300 and
        // the 100-byte band between them belongs to the queue window and stale
        // replacements. `keep` (200) is resident; `doomed` (150) would take the
        // frontier to 350 and is refused. Those numbers are chosen so the FRONTIER is
        // the only thing refusing it: there is ample raw headroom for `doomed`, so a
        // planner that consulted the budget alone would happily fetch it.
        //
        // `doomed` is the ARTIST fan-out, which is what puts it below the line under
        // the current rule whatever its neglect - so the property under test here is
        // the anti-thrash one, uncoupled from which ordering opinion happens to draw
        // the line.
        let mut input = PassInput::new(PassMode::Full, 400);
        input.now_unix = NOW_TEST;
        input.download_batch = 16;
        input.pins = Some(PinSet {
            groups: vec![
                hot_grp(PinTier::Album, "keep", &[("keep", 200)]),
                hot_grp(PinTier::Artist, "doomed", &[("d1", 75), ("d2", 75)]),
            ],
        });

        // (1) ADMISSION, from an empty store. 200 bytes of headroom remain after
        // `keep` and the deferred album needs only 150 - and it is still not fetched.
        assert_eq!(
            dl_ids(&plan_pass(&input)),
            vec!["keep"],
            "the band below the ceiling is reserved for the window, not for pins that \
             happen to fit in it"
        );

        // (2) EVICTION. Everything is on disk now and a protected window entry has
        // pushed the store over the budget, so the pressure must come out of the pin
        // groups - and it comes out of the DEFERRED one first, whole.
        input.entries = vec![
            pinned_entry("keep", 200, 500),
            pinned_entry("d1", 75, 900),
            pinned_entry("d2", 75, 900),
            entry("window-bytes", 60, 5),
        ];
        input.window = vec![sid("window-bytes")];
        let plan = plan_pass(&input);
        let mut evicted = evictions(&plan);
        evicted.sort();
        assert_eq!(
            evicted,
            ["d1", "d2"],
            "the whole deferred group goes, and the resident song is untouched"
        );
        assert_eq!(dl_ids(&plan), Vec::<String>::new(), "nothing below the line is ever fetched");

        // (3) THE NEXT PASS over exactly the state eviction produced. The group is
        // still below the line, so it is still not admitted - the state has SETTLED
        // rather than begun a download-evict cycle.
        input.entries = vec![pinned_entry("keep", 200, 500), entry("window-bytes", 60, 5)];
        let plan = plan_pass(&input);
        assert_eq!(
            dl_ids(&plan),
            Vec::<String>::new(),
            "the loop is structurally impossible, not merely unlikely"
        );
        assert_eq!(evictions(&plan), Vec::<String>::new(), "and nothing further is reclaimed");
    }

    #[test]
    fn eviction_takes_a_whole_group_and_never_a_member_a_higher_tier_claimed() {
        // An album is held whole or not at all in BOTH directions: half an album is
        // not a state this planner is willing to leave behind. And dropping an
        // artist's album can never take a starred SONG with it, because that track was
        // claimed at the song tier and is not in the artist group's run at all.
        let mut input = PassInput::new(PassMode::Full, 200);
        input.pins = Some(PinSet {
            groups: vec![
                grp(PinTier::Song, "loved", &[("loved", 50)]),
                grp(PinTier::Artist, "ar", &[("loved", 50), ("a1", 50), ("a2", 50)]),
            ],
        });
        input.entries = vec![
            pinned_entry("loved", 50, 100),
            pinned_entry("a1", 50, 900),
            pinned_entry("a2", 50, 900),
        ];
        // 150 on disk against a 200 budget is not over, so nothing moves yet.
        assert_eq!(evictions(&plan_pass(&input)), Vec::<String>::new());

        // Now push over the line with an opportunistic entry the window is holding
        // onto, so the pressure has to come out of the pin groups. The overshoot is
        // 50 bytes - ONE member's worth - so a per-track rule would stop after `a1`
        // and leave half an album behind. Whole-group eviction takes both anyway.
        input.entries.push(entry("window-bytes", 100, 5));
        input.window = vec![sid("window-bytes")];
        let evicted = evictions(&plan_pass(&input));
        assert_eq!(
            evicted,
            vec!["a1".to_string(), "a2".to_string()],
            "the artist album goes WHOLE even though half of it would have sufficed, \
             and the starred song it shares is untouched"
        );
    }

    #[test]
    fn eviction_walks_the_same_one_ordering_backwards() {
        // THE POINT OF HAVING ONE ORDERING: eviction is the frontier read from the
        // tail, so what admission refuses last is what removal takes first, and the
        // two halves cannot contradict each other.
        //
        // This test REPLACES one that asserted a starred song outlives a starred
        // album. That clause was a preference and it is the one he contradicted;
        // what survives untouched, and is asserted first, is that opportunistic bytes
        // go before ANY pin and an unbounded artist subscription goes before any
        // hand-picked gesture.
        let mut input = PassInput::new(PassMode::Full, 100);
        input.now_unix = NOW_TEST;
        // Wholly COLD throughout, so all three tie on the neglect key and the ask's
        // own clause is the thing being walked backwards here (see `emphasis_rank`:
        // the clause speaks over neglected groups, which these are).
        input.pins = Some(PinSet {
            groups: vec![
                aged_grp(PinTier::Song, "sng", &[("sng", 100)], &[Some(200)]),
                aged_grp(PinTier::Album, "al", &[("alb", 100)], &[Some(200)]),
                aged_grp(PinTier::Artist, "ar", &[("art", 100)], &[Some(200)]),
            ],
        });
        input.entries = vec![
            // The opportunistic one is the MOST recently played, so a pure LRU rule
            // would keep it and take a pin instead. Standing beats recency.
            entry("cold-but-recent", 100, 10_000),
            pinned_entry("sng", 100, 1),
            pinned_entry("alb", 100, 1),
            pinned_entry("art", 100, 1),
        ];
        assert_eq!(
            evictions(&plan_pass(&input)),
            vec![
                "cold-but-recent".to_string(),
                "art".to_string(),
                "sng".to_string(),
            ],
            "opportunistic, then the unbounded artist subscription, then - at equal \
             neglect - the loose starred song before the starred album"
        );

        // And the SAME walk with the neglect signal turned on: the album is the fresh
        // one now, so it goes before the song. Removal reads the neglect key exactly
        // as admission does - if it did not, the two would disagree the moment a
        // group crossed the line, which is the download-evict loop this design exists
        // to make structurally impossible.
        input.pins = Some(PinSet {
            groups: vec![
                aged_grp(PinTier::Song, "sng", &[("sng", 100)], &[Some(200)]),
                hot_grp(PinTier::Album, "al", &[("alb", 100)]),
                hot_grp(PinTier::Artist, "ar", &[("art", 100)]),
            ],
        });
        assert_eq!(
            evictions(&plan_pass(&input)),
            vec![
                "cold-but-recent".to_string(),
                "art".to_string(),
                "alb".to_string(),
            ],
            "the 200-day-cold song now outranks the album, and removal agrees"
        );
    }

    #[test]
    fn store_pause_stops_the_planner_from_scheduling_bulk_work() {
        // The planner half of the interrupt path (the executor re-checks too, so a
        // pause landing mid-batch is also honored - that leg is proved in the
        // reconciler section). Pausing suspends exactly the BULK categories and no
        // more: pausing the mirror must never make the next track stream.
        let mut stale = pinned_entry("stale", 100, 1);
        stale.stale = true;
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.entries = vec![stale];
        input.window = vec![sid("about-to-play")];
        input.pins = Some(PinSet::of_songs(vec![
            song("stale", 100, "flac", Some("2024-05-01T12:00:00Z")),
            song("bulk", 100, "flac", Some("2024-05-01T12:00:00Z")),
        ]));
        assert_eq!(
            dls(&plan_pass(&input)),
            vec![
                ("about-to-play".to_string(), DownloadReason::Window),
                ("stale".to_string(), DownloadReason::Stale),
                ("bulk".to_string(), DownloadReason::Backfill),
            ],
            "unpaused, every category runs"
        );

        input.paused = true;
        assert_eq!(
            dls(&plan_pass(&input)),
            vec![("about-to-play".to_string(), DownloadReason::Window)],
            "paused, the bulk categories stop and the audible one does not"
        );
    }

    #[test]
    fn a_breached_reserve_writes_nothing_at_all_and_only_deletes() {
        // THE GUARANTEE IN ITS SHARPEST FORM. A zero effective budget means the
        // free-space reserve is breached, and then even Window and Suspect - which are
        // deliberately never budget-gated anywhere else - are suppressed. A suppressed
        // window download costs a track that streams instead of playing from disk,
        // which nobody notices; 415 MiB written onto a disk at 99 % is a machine he
        // cannot use.
        let mut suspect = pinned_entry("suspect", 100, 5);
        suspect.suspect = true;
        let mut input = PassInput::new(PassMode::Full, 0);
        input.entries = vec![suspect, pinned_entry("pin", 100, 5), entry("cold", 100, 1)];
        input.window = vec![sid("suspect"), sid("not-yet-cached")];
        input.pins = Some(PinSet::of_songs(vec![
            song("suspect", 100, "flac", Some("2024-05-01T12:00:00Z")),
            song("pin", 100, "flac", Some("2024-05-01T12:00:00Z")),
            song("wanted", 100, "flac", None),
        ]));
        let (plan, status) = plan_pass_with_status(&input);
        assert_eq!(
            dls(&plan),
            Vec::new(),
            "no download of ANY reason - not backfill, not the window, not a suspect"
        );
        // ...and it only deletes. The window is still protected, because evicting what
        // he is about to hear is never right even here.
        assert_eq!(
            evictions(&plan),
            vec!["cold".to_string(), "pin".to_string()],
            "everything unprotected is reclaimed, coldest-standing first"
        );
        let status = status.expect("status");
        assert_eq!(status.waiting, StoreWaiting::ReserveBreached, "and it says why");
        assert_eq!(status.effective_max, 0);
    }

    #[test]
    fn a_breached_reserve_writes_nothing_on_a_light_pass_either() {
        // THE OTHER HALF of the guarantee above, and the half that actually runs most
        // often: a light kick fires at EVERY track boundary and queue edit, so if the
        // suppression held only for `Full` the store would keep writing window
        // originals onto a breached disk ten times per full-pass gap, each one deleted
        // again by the next full pass. `writes_allowed` must be mode-blind.
        //
        // The other half of THAT is `run_pass` actually measuring the disk on a light
        // pass, which is pinned by
        // `a_light_pass_measures_the_disk_exactly_like_a_full_one` below - this test
        // alone would pass even with the measurement skipped.
        let mut suspect = entry("suspect", 100, 5);
        suspect.suspect = true;
        let mut input = PassInput::new(PassMode::Light, 0);
        input.entries = vec![suspect];
        input.window = vec![sid("suspect"), sid("not-yet-cached")];
        let plan = plan_pass(&input);
        assert_eq!(
            dls(&plan),
            Vec::new(),
            "a light pass on a breached reserve writes nothing either"
        );
        // And a light pass never evicts: eviction reads a fresh scan, which only a full
        // pass takes. The deleting half of the breach regime stays the full pass's job.
        assert_eq!(evictions(&plan), Vec::<String>::new());
    }

    #[test]
    fn opportunistic_bytes_are_reclaimed_to_admit_a_resident_pin_group() {
        // THE STARVATION THIS EXISTS TO PREVENT. Window downloads are deliberately
        // never budget-gated, so ordinary listening walks the store up to `max_bytes`
        // in opportunistic cache. Eviction only fires ABOVE the budget and stops the
        // instant it is at or under, so the steady state is total == max_bytes with ~0
        // headroom - and admission, which spends `max_bytes - total`, then refuses
        // every starred group FOREVER while reporting `waiting: none`. "Star an album
        // and it is simply there later" would quietly stop being true once the store
        // filled up.
        //
        // Bytes he ASKED FOR outrank bytes hypodj kept on spec, so the group takes the
        // space back from the coldest cache - and the evictions must be planned BEFORE
        // the downloads, because the executor walks the list front to back.
        let album = grp(PinTier::Album, "al", &[("t1", 150), ("t2", 150)]);
        let mut input = PassInput::new(PassMode::Full, 1000);
        // Ten cold, unpinned, un-windowed entries filling the budget exactly.
        input.entries = (0..10).map(|i| entry(&format!("c{i}"), 100, i as u64)).collect();
        input.pins = Some(PinSet { groups: vec![album] });
        let (plan, status) = plan_pass_with_status(&input);

        assert_eq!(
            dl_ids(&plan),
            vec!["t1".to_string(), "t2".to_string()],
            "the starred group is admitted, not starved"
        );
        assert_eq!(
            evictions(&plan),
            vec!["c0".to_string(), "c1".to_string(), "c2".to_string()],
            "exactly enough of the COLDEST cache to cover 300 bytes, LRU first"
        );
        let first_evict = plan
            .iter()
            .position(|a| matches!(a, StoreAction::Evict(_)))
            .expect("an eviction");
        let first_dl = plan
            .iter()
            .position(|a| matches!(a, StoreAction::Download { .. }))
            .expect("a download");
        assert!(
            first_evict < first_dl,
            "the reclaim must precede the download it paid for - the executor walks the list in order:\n{plan:#?}"
        );
        let status = status.expect("status");
        assert_eq!(status.pending_tracks, 2, "still pending, because the plan has not run yet");
        assert_eq!(status.waiting, StoreWaiting::None);
    }

    #[test]
    fn the_reclaim_is_all_or_nothing_and_never_takes_the_window_or_a_pin() {
        // Three refusals the reclaim has to make, or it becomes the thrash it replaced.
        //
        // The queue window is off limits at any pressure (evicting what he is about to
        // hear is never right), a group above the line is off limits (spending one
        // pin's bytes on another is precisely the download-evict loop the frontier
        // exists to forbid), and what is left over does not cover the group - so it
        // evicts NOTHING. A PARTIAL reclaim would delete cache for a group that is
        // still refused afterwards: a deletion that bought nothing and a re-download
        // the next time he plays it.
        //
        // Budget 1000, so the pin ceiling is 750: `held` (50) plus `big` (700) is
        // exactly resident, and both are above the line.
        let held = grp(PinTier::Song, "s1", &[("held", 50)]);
        let wanted = grp(PinTier::Album, "al", &[("big", 700)]);
        let mut input = PassInput::new(PassMode::Full, 1000);
        input.entries =
            vec![pinned_entry("held", 50, 0), entry("win", 500, 1), entry("c0", 100, 2)];
        input.window = vec![sid("win")];
        input.pins = Some(PinSet { groups: vec![held, wanted] });
        let plan = plan_pass(&input);
        assert_eq!(
            dl_ids(&plan),
            Vec::<String>::new(),
            "350 wanted, only the 100-byte cold entry may be taken: refused"
        );
        assert_eq!(
            evictions(&plan),
            Vec::<String>::new(),
            "and NOTHING was deleted for it - not the window, not the held pin, not even the cold entry"
        );
    }

    #[test]
    fn the_reclaim_never_takes_an_id_the_same_pass_is_downloading() {
        // EVICT-THEN-FETCH IS NOT A RECLAIM. A suspect entry's bytes are on disk and
        // look like the coldest thing in the store, but this pass has already scheduled
        // its replacement - taking them would spend a whole original to unlink it, and
        // the space would be back to where it started the moment the download landed.
        // With it excluded there is no donor at all, so the group waits instead.
        let album = grp(PinTier::Album, "al", &[("t1", 350)]);
        let mut input = PassInput::new(PassMode::Full, 1000);
        let mut suspect = entry("sus", 700, 1);
        suspect.suspect = true;
        input.entries = vec![suspect];
        input.pins = Some(PinSet { groups: vec![album] });
        let plan = plan_pass(&input);
        assert_eq!(
            dl_ids(&plan),
            vec!["sus".to_string()],
            "only the suspect replacement - the backfill found no donor it was allowed to take"
        );
        assert_eq!(evictions(&plan), Vec::<String>::new());
    }

    #[test]
    fn an_album_leaving_the_pin_set_demotes_exactly_its_tracks_and_they_evict_at_once() {
        // UNSTARRING AN ALBUM. The demote path is key-agnostic - it diffs the entries
        // against whatever ids the pin set carried, never against WHY they were in it -
        // so this works for an album for free, PROVIDED the expansion happens inside
        // pins() and the returned set is a COMPLETE desired set every pass.
        let album = grp(PinTier::Album, "al", &[("t1", 100), ("t2", 100), ("t3", 100)]);
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.entries = vec![
            pinned_entry("t1", 100, 1),
            pinned_entry("t2", 100, 1),
            pinned_entry("t3", 100, 1),
            pinned_entry("kept", 100, 1),
        ];
        input.pins = Some(PinSet {
            groups: vec![grp(PinTier::Song, "kept", &[("kept", 100)])],
        });
        let demoted: Vec<String> = plan_pass(&input)
            .into_iter()
            .filter_map(|a| match a {
                StoreAction::SetPinned { id, pinned: false } => Some(id.0),
                _ => None,
            })
            .collect();
        assert_eq!(
            demoted,
            vec!["t1".to_string(), "t2".to_string(), "t3".to_string()],
            "exactly the unstarred album's tracks demote, and the starred song does not"
        );

        // The bytes are KEPT (an accidental unstar and re-star costs zero downloads),
        // but the entries are evictable in THIS SAME pass - eviction reads the pass's
        // verdict, not the pre-pass sidecar flag.
        input.max_bytes = 100;
        let evicted = evictions(&plan_pass(&input));
        assert_eq!(
            evicted,
            vec!["t1".to_string(), "t2".to_string(), "t3".to_string()],
            "demoted bytes are ordinary LRU candidates at once; the pin is still spared"
        );

        // And re-starring the album costs nothing: the tracks are already on disk, so
        // the next pass re-promotes them and schedules no download at all.
        let mut restar = PassInput::new(PassMode::Full, 1 << 30);
        restar.entries = vec![entry("t1", 100, 1), entry("t2", 100, 1), entry("t3", 100, 1)];
        restar.pins = Some(PinSet { groups: vec![album] });
        let plan = plan_pass(&restar);
        assert_eq!(dls(&plan), Vec::new(), "re-starring re-downloads nothing");
        assert_eq!(
            plan.iter().filter(|a| matches!(a, StoreAction::SetPinned { pinned: true, .. })).count(),
            3,
            "it just re-promotes the bytes that never left"
        );
    }

    #[test]
    fn a_server_flap_never_costs_a_starred_file_even_over_budget() {
        // Without an authoritative pin set there is NO frontier, so eviction degrades
        // to exactly the rule that existed before it: unpinned entries only, by LRU.
        // A transient getStarred2 failure must never be the reason a starred album is
        // deleted.
        let mut input = PassInput::new(PassMode::Full, 100);
        input.pins = None;
        input.entries = vec![
            pinned_entry("starred", 100, 1),
            entry("opportunistic", 100, 2),
        ];
        assert_eq!(
            evictions(&plan_pass(&input)),
            vec!["opportunistic".to_string()],
            "the pin is untouchable while the pin set is unknown, even over budget"
        );
    }

    #[test]
    fn sort_albums_newest_first_prefers_created_then_year_then_id() {
        // How a partly-resident artist chooses what to keep: their RECENT work, which
        // is the "keep me current with them" reading of a standing subscription.
        let al = |id: &str, created: Option<&str>, year: Option<u32>| Album {
            id: crate::model::AlbumId(id.to_string()),
            name: format!("al-{id}"),
            artist: "ar".into(),
            artist_id: None,
            year,
            genre: None,
            cover_art: None,
            song_count: 1,
            created: created.map(|c| c.to_string()),
        };
        let mut albums = vec![
            al("no-info", None, None),
            al("old", Some("2019-01-01T00:00:00Z"), Some(2019)),
            al("newest", Some("2024-06-01T00:00:00Z"), Some(2001)),
            al("year-only-b", None, Some(2020)),
            al("year-only-a", None, Some(2020)),
        ];
        sort_albums_newest_first(&mut albums);
        assert_eq!(
            albums.iter().map(|a| a.id.0.as_str()).collect::<Vec<_>>(),
            // `created` dominates (even when `year` disagrees, as for "newest"), then
            // `year`, then the id - so the order is TOTAL and two passes over the same
            // catalogue draw the identical line.
            vec!["newest", "old", "year-only-a", "year-only-b", "no-info"],
        );
    }

    #[test]
    fn plan_pass_budgets_bulk_downloads_against_the_bytes_on_disk_right_now() {
        // The over-budget eviction is emitted LAST, so a download admitted this pass
        // must fit in the space that exists BEFORE it runs - otherwise the store would
        // transiently exceed max_bytes. The 200 bytes already on disk are the QUEUE
        // WINDOW here, so they are off limits to the reclaim too (see
        // `the_reclaim_is_all_or_nothing_and_never_takes_the_window_or_a_pin`) and the
        // headroom really is only 50.
        let mut input = PassInput::new(PassMode::Full, 250);
        input.entries = vec![entry("old", 200, 1)];
        input.window = vec![sid("old")];
        input.pins = Some(PinSet::of_songs(vec![song("big", 100, "flac", None), song("small", 40, "flac", None)]));
        let plan = plan_pass(&input);
        // 50 bytes of headroom: `big` (100) does not fit, `small` (40) does.
        assert_eq!(dls(&plan), vec![("small".to_string(), DownloadReason::Backfill)]);
        // `old` is not evicted (200 + 0 is under the cap), so the space really was
        // only 50 bytes.
        assert_eq!(evictions(&plan), Vec::<String>::new());
        // Once eviction HAS reclaimed the space, the next pass admits the big one.
        let mut next = PassInput::new(PassMode::Full, 250);
        next.pins = input.pins.clone();
        assert_eq!(
            dls(&plan_pass(&next)),
            vec![("big".to_string(), DownloadReason::Backfill), ("small".to_string(), DownloadReason::Backfill)]
        );
    }

    #[test]
    fn plan_pass_window_and_suspect_downloads_are_not_budget_gated() {
        // These are bounded in count (queue_ahead + 1; the suspect set) and are
        // exactly what the user is about to hear. Refusing them because the store is
        // full of pins would defeat the feature; their bytes are reclaimed by the
        // next pass's eviction like any others.
        //
        // Zero headroom AND nothing the reclaim may take: the 300 bytes on disk that
        // are not the suspect entry itself are the queue WINDOW, which is off limits
        // at any pressure. So the budgeted backfill is genuinely halted while the two
        // audible categories go through anyway.
        let mut suspect = entry("suspect", 100, 0);
        suspect.suspect = true;
        suspect.pinned = true;
        let mut input = PassInput::new(PassMode::Full, 400);
        input.entries = vec![suspect, entry("in-window", 300, 0)];
        input.window = vec![sid("in-window"), sid("about-to-play")];
        input.pins = Some(PinSet::of_songs(vec![
            song("suspect", 100, "flac", Some("2024-05-01T12:00:00Z")),
            song("nice-to-have", 150, "flac", None),
        ]));
        let plan = plan_pass(&input);
        assert_eq!(
            dls(&plan),
            vec![
                ("suspect".to_string(), DownloadReason::Suspect),
                ("about-to-play".to_string(), DownloadReason::Window),
            ],
            "the audible work happens; the nice-to-have backfill is halted"
        );
        assert_eq!(evictions(&plan), Vec::<String>::new(), "and nothing was deleted for it");
    }

    #[test]
    fn plan_pass_flushes_dirty_recency_on_full_passes_only() {
        let mut dirty = entry("bumped", 100, 777);
        dirty.recency_dirty = true;
        let clean = entry("untouched", 100, 5);
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.entries = vec![dirty, clean];
        assert_eq!(
            plan_pass(&input),
            vec![StoreAction::FlushRecency { id: sid("bumped"), last_played_unix: 777 }],
            "only the dirty entry is written, and only its recency"
        );
        // A light kick fires at every track boundary; one sidecar rewrite per
        // boundary is a write storm for a coarse LRU key.
        input.mode = PassMode::Light;
        assert_eq!(plan_pass(&input), Vec::new());
    }

    #[test]
    fn plan_pass_is_pure_and_repeatable() {
        // Same input, same output, no interior mutation - the property the whole
        // table-testing approach rests on.
        let mut suspect = entry("suspect", 100, 1);
        suspect.suspect = true;
        let mut dirty = entry("dirty", 100, 2);
        dirty.recency_dirty = true;
        let mut input = PassInput::new(PassMode::Full, 150);
        input.entries = vec![suspect, dirty, entry("cold", 100, 0)];
        input.window = vec![sid("want")];
        input.protected = HashSet::from([sid("skip")]);
        input.pins = Some(PinSet::of_songs(vec![song("newpin", 10, "flac", None)]));
        input.stale_tmps = vec![PathBuf::from("/s/tmp.1.0")];
        let first = plan_pass(&input);
        assert_eq!(plan_pass(&input), first);
        assert_eq!(plan_pass(&input), first);
        assert!(!first.is_empty(), "the fixture must actually exercise something");
    }

    #[test]
    fn plan_pass_handles_saturating_byte_totals_without_overflow() {
        // Two u64::MAX-sized entries would overflow a naive sum, panicking in debug
        // and wrapping in release. The accounting SATURATES instead, so a nonsense
        // fingerprint degrades to "definitely over cap" and still plans real work.
        //
        // The honest consequence of saturating, documented by this assertion: once
        // the total has saturated, subtracting one victim's size brings the running
        // figure straight to 0, so the pass stops after ONE eviction. The next pass
        // re-reads real bytes and continues - convergence by re-entry, not by
        // pretending the arithmetic was exact.
        let mut input = PassInput::new(PassMode::Full, 64);
        input.entries = vec![entry("a", u64::MAX, 1), entry("b", u64::MAX, 2)];
        assert_eq!(evictions(&plan_pass(&input)), vec!["a".to_string()]);
        // A saturating total cannot manufacture headroom BEYOND THE BUDGET either: the
        // reclaim credits what it freed and then re-derives against `max_bytes`, so the
        // 10-byte pin lands behind ONE dropped nonsense entry rather than behind a
        // credit of u64::MAX. The store is 64 bytes, and this pass plans to put 10 in
        // it.
        input.pins = Some(PinSet::of_songs(vec![song("want", 10, "flac", None)]));
        let plan = plan_pass(&input);
        assert_eq!(dls(&plan), vec![("want".to_string(), DownloadReason::Backfill)]);
        assert_eq!(evictions(&plan), vec!["a".to_string()], "and it paid for it, once");
        // And with realistic sizes summing past u64::MAX, the same holds with no
        // panic in either profile.
        input.pins = None;
        input.entries = vec![
            entry("x", u64::MAX / 2 + 1, 1),
            entry("y", u64::MAX / 2 + 1, 2),
            entry("z", 100, 3),
        ];
        assert!(!evictions(&plan_pass(&input)).is_empty());
    }

    // ── the scan feeds the planner ──────────────────────────────────────────

    #[test]
    fn scan_dir_output_drives_a_full_pass_end_to_end() {
        // The seam that matters: a real directory in an arbitrary state, scanned,
        // planned, and the plan applied - converging to a healthy store.
        let dir = tmpdir("scan-to-plan");
        place(&dir, &dir_song("keep", 32), true, false, 50);
        place(&dir, &dir_song("drop-me", 32), true, false, 10);
        std::fs::write(dir.join("orphan.flac"), vec![b'x'; 32]).expect("write");
        std::fs::write(dir.join("tmp.999.3"), b"half").expect("write");
        std::fs::write(dir.join("broken.toml"), "}{").expect("write");

        let scan = scan_dir(&dir).expect("scan");
        assert_eq!(
            scan.entries.iter().map(|e| e.id.0.clone()).collect::<Vec<_>>(),
            vec!["drop-me".to_string(), "keep".to_string()],
            "entries are id-sorted, so a plan built from them is deterministic"
        );
        assert_eq!(scan.orphan_audio, vec![dir.join("orphan.flac")]);
        assert_eq!(scan.orphan_sidecars, vec![(sid("broken"), DeleteReason::CorruptSidecar)]);
        assert_eq!(scan.stale_tmps, vec![dir.join("tmp.999.3")]);

        let mut input = PassInput::new(PassMode::Full, 40);
        input.entries = scan.entries.clone();
        input.orphan_audio = scan.orphan_audio.clone();
        input.orphan_sidecars = scan.orphan_sidecars.clone();
        input.stale_tmps = scan.stale_tmps.clone();
        // `keep` is starred; `drop-me` is not, so it demotes and then - 64 bytes
        // against a 40 cap - it is the one thing reclaimable.
        input.pins = Some(PinSet::of_songs(vec![dir_song("keep", 32)]));
        let plan = plan_pass(&input);
        assert_eq!(
            plan,
            vec![
                StoreAction::SweepTmp(dir.join("tmp.999.3")),
                StoreAction::DeleteFile(dir.join("orphan.flac")),
                StoreAction::DeleteEntry { id: sid("broken"), reason: DeleteReason::CorruptSidecar },
                StoreAction::SetPinned { id: sid("drop-me"), pinned: false },
                StoreAction::Evict(sid("drop-me")),
            ]
        );

        // Apply it with the store's own primitives and confirm convergence: a
        // re-scan finds a clean store and a re-plan finds nothing left to do.
        let s = AudioStore::open(dir.clone(), StoreConfig::default()).expect("open");
        for action in &plan {
            match action {
                StoreAction::SweepTmp(p) | StoreAction::DeleteFile(p) => {
                    let _ = std::fs::remove_file(p);
                }
                StoreAction::DeleteEntry { id, .. } | StoreAction::Evict(id) => {
                    assert!(s.remove_entry(id), "a writable tempdir always reclaims");
                }
                StoreAction::SetPinned { id, pinned } => {
                    let _ = s.set_pinned(id, *pinned);
                }
                other => panic!("unexpected action {other:?}"),
            }
        }
        let after = scan_dir(&dir).expect("re-scan");
        assert_eq!(after.entries.iter().map(|e| e.id.0.clone()).collect::<Vec<_>>(), vec!["keep"]);
        assert_eq!(after.orphan_audio, Vec::<PathBuf>::new());
        assert_eq!(after.orphan_sidecars, Vec::new());
        assert_eq!(after.stale_tmps, Vec::<PathBuf>::new());
        let mut settled = PassInput::new(PassMode::Full, 40);
        settled.entries = after.entries;
        settled.pins = Some(PinSet::of_songs(vec![dir_song("keep", 32)]));
        assert_eq!(plan_pass(&settled), Vec::new(), "the pass converges");
        assert_eq!(names(&dir), vec!["keep.flac".to_string(), "keep.toml".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fixture song with the `created` value [`entry`] uses, so a scanned entry
    /// and a pin built from the same id agree on every fingerprint leg.
    fn dir_song(id: &str, size: u64) -> Song {
        song(id, size, "flac", Some("2024-05-01T12:00:00Z"))
    }

    #[test]
    fn scan_dir_on_a_missing_root_is_an_error_not_a_panic() {
        let missing = std::env::temp_dir().join(format!("hypodj-store-nope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        assert!(scan_dir(&missing).is_err());
        // But `open` creates it, so a first run is never an error.
        let s = AudioStore::open(missing.clone(), StoreConfig::default()).expect("open creates");
        assert_eq!(s.entries(), Vec::new());
        let _ = std::fs::remove_dir_all(&missing);
    }

    // ── the reconciler loop ─────────────────────────────────────────────────
    //
    // Generic over Clock + PinSource precisely so this whole section runs with NO
    // network and NO wall clock: `#[tokio::test(start_paused = true)]` plus a
    // scripted source. Every assertion below is about the loop's own decisions -
    // cadence, kick scoping, verdict skipping, backoff - not about HTTP.

    /// What the scripted source was asked for, so a test can assert that a LIGHT
    /// pass never touched the pin set.
    #[derive(Default, Debug)]
    struct SourceLog {
        pins_calls: usize,
        song_calls: usize,
        invalidations: usize,
        fetches: Vec<String>,
    }

    struct FakeInner {
        log: Mutex<SourceLog>,
        /// `None` scripts a TRANSIENT pin-set failure.
        pins: Mutex<Option<PinSet>>,
        /// Everything `song(id)` can resolve, for ids outside the pin set.
        catalog: Mutex<HashMap<String, Song>>,
        /// When false every fetch fails, which is how the backoff is exercised.
        fetch_ok: AtomicBool,
        /// Fired at the START of every fetch. A test uses it to change the world
        /// MID-PASS - the deck goes remote, the user pauses - which is the only way
        /// to exercise the executor's re-check rather than the planner's once-per-pass
        /// sample of the same facts.
        #[allow(clippy::type_complexity)]
        on_fetch: Mutex<Option<Box<dyn Fn(&str) + Send + Sync>>>,
    }

    /// A scripted [`PinSource`]: no server, no sockets. `fetch` writes exactly
    /// `song.size` bytes so a successful commit is byte-for-byte what the real
    /// exact-length gate would accept.
    #[derive(Clone)]
    struct FakeSource(Arc<FakeInner>);

    impl FakeSource {
        fn new(pins: Option<Vec<Song>>) -> Self {
            Self::with_pin_set(pins.map(PinSet::of_songs))
        }

        fn with_pin_set(pins: Option<PinSet>) -> Self {
            let catalog = pins
                .iter()
                .flat_map(|p| p.songs())
                .map(|s| (s.id.0.clone(), s.clone()))
                .collect();
            Self(Arc::new(FakeInner {
                log: Mutex::new(SourceLog::default()),
                pins: Mutex::new(pins),
                catalog: Mutex::new(catalog),
                fetch_ok: AtomicBool::new(true),
                on_fetch: Mutex::new(None),
            }))
        }

        /// Run `f` at the start of every fetch, so a test can move the world under a
        /// pass that is already executing.
        fn on_fetch(&self, f: impl Fn(&str) + Send + Sync + 'static) {
            *self.0.on_fetch.lock().unwrap() = Some(Box::new(f));
        }

        fn set_pins(&self, pins: Option<Vec<Song>>) {
            *self.0.pins.lock().unwrap() = pins.map(PinSet::of_songs);
        }

        fn set_pin_set(&self, pins: Option<PinSet>) {
            *self.0.pins.lock().unwrap() = pins;
        }

        fn invalidations(&self) -> usize {
            self.0.log.lock().unwrap().invalidations
        }

        fn add_to_catalog(&self, s: Song) {
            self.0.catalog.lock().unwrap().insert(s.id.0.clone(), s);
        }

        fn set_fetch_ok(&self, ok: bool) {
            self.0.fetch_ok.store(ok, Ordering::Relaxed);
        }

        fn pins_calls(&self) -> usize {
            self.0.log.lock().unwrap().pins_calls
        }

        fn song_calls(&self) -> usize {
            self.0.log.lock().unwrap().song_calls
        }

        fn fetches(&self) -> Vec<String> {
            self.0.log.lock().unwrap().fetches.clone()
        }
    }

    impl PinSource for FakeSource {
        async fn pins(&self) -> Result<PinSet, String> {
            // Scoped so no std Mutex is ever held across an await.
            let scripted = {
                self.0.log.lock().unwrap().pins_calls += 1;
                self.0.pins.lock().unwrap().clone()
            };
            scripted.ok_or_else(|| "scripted transient failure".to_string())
        }

        fn invalidate(&self) {
            self.0.log.lock().unwrap().invalidations += 1;
        }

        async fn song(&self, id: &SongId) -> Result<Song, String> {
            let found = {
                self.0.log.lock().unwrap().song_calls += 1;
                self.0.catalog.lock().unwrap().get(&id.0).cloned()
            };
            found.ok_or_else(|| format!("no such song: {}", id.0))
        }

        async fn fetch(&self, song: &Song, tmp: &Path) -> Result<u64, String> {
            // Scoped, and there is no await inside: no std Mutex is ever held across
            // one, here or anywhere else in this file.
            {
                let hook = self.0.on_fetch.lock().unwrap();
                if let Some(f) = hook.as_ref() {
                    f(&song.id.0);
                }
            }
            let ok = self.0.fetch_ok.load(Ordering::Relaxed);
            self.0.log.lock().unwrap().fetches.push(song.id.0.clone());
            if !ok {
                return Err("scripted fetch failure".into());
            }
            let size = song.size.ok_or_else(|| "no size".to_string())?;
            std::fs::write(tmp, vec![b'x'; size as usize]).map_err(|e| e.to_string())?;
            Ok(size)
        }
    }

    fn loop_store(dir: &Path, max_bytes: u64, interval: u64) -> Arc<AudioStore> {
        let mut cfg = StoreConfig::default();
        cfg.max_bytes = max_bytes;
        cfg.sync_interval_secs = interval;
        cfg.queue_ahead = 3;
        Arc::new(AudioStore::open(dir.to_path_buf(), cfg).expect("open store"))
    }

    /// Let the spawned reconciler run to a quiet point WITHOUT advancing the
    /// virtual clock, so the cadence assertions stay exact.
    ///
    /// A pass hops through `spawn_blocking`, and a blocking pool thread's
    /// completion is a real-thread event that no amount of virtual time can
    /// produce - hence the tiny parks between yields. This is scheduling slack for
    /// the test harness, NOT time-based logic: every deadline the loop itself
    /// observes still comes from the paused [`crate::clock::Clock`].
    async fn settle() {
        for _ in 0..400 {
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_micros(200));
        }
    }

    /// Like [`settle`] but stops as soon as `done` holds, REPORTING whether it ever
    /// did. Callers that must restore state before they can fail (a tempdir left
    /// read-only on purpose) use this and assert afterwards.
    async fn settle_reached(mut done: impl FnMut() -> bool) -> bool {
        for _ in 0..2000 {
            if done() {
                return true;
            }
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_micros(200));
        }
        false
    }

    /// Like [`settle`] but stops as soon as `done` holds, and FAILS LOUDLY if it
    /// never does - so a broken loop is a clear failure rather than a later
    /// confusing assertion.
    async fn settle_until(tag: &str, done: impl FnMut() -> bool) {
        if !settle_reached(done).await {
            panic!("the reconciler never reached: {tag}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn startup_runs_a_full_pass_and_backfills_the_pin_set() {
        let dir = tmpdir("loop-backfill");
        let store = loop_store(&dir, 1_000_000, 900);
        let source = Arc::new(FakeSource::new(Some(vec![
            song("aa", 12, "flac", Some("2024-05-01T12:00:00Z")),
            song("bb", 20, "mp3", Some("2024-05-01T12:00:00Z")),
        ])));
        let task = tokio::spawn(run(store.clone(), source.clone(), TokioClockForTest));
        settle_until("both pins committed", || store.entries().len() == 2).await;

        let ids: Vec<String> = store.entries().into_iter().map(|e| e.id.0).collect();
        assert_eq!(ids, vec!["aa".to_string(), "bb".to_string()], "startup is a FULL pass, so the pin set backfills");
        assert!(store.entries().iter().all(|e| e.pinned), "a starred song commits PINNED");
        assert_eq!(store.total_bytes(), 32);
        // The commit is the sidecar, and only a committed pair is offerable.
        assert_eq!(store.lookup(&sid("aa")), Some(dir.join("aa.flac")));
        assert_eq!(store.lookup(&sid("bb")), Some(dir.join("bb.mp3")));
        assert_eq!(source.pins_calls(), 1, "one getStarred2 per full pass");
        assert_eq!(
            source.song_calls(),
            0,
            "a pin carries its own fingerprint; no per-id metadata round trip"
        );
        task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn a_light_kick_fetches_the_window_without_touching_the_pin_set() {
        let dir = tmpdir("loop-light");
        let store = loop_store(&dir, 1_000_000, 900);
        let source = Arc::new(FakeSource::new(Some(vec![song(
            "pinned",
            12,
            "flac",
            Some("2024-05-01T12:00:00Z"),
        )])));
        // A queue song that is NOT starred: only `song(id)` can resolve it.
        source.add_to_catalog(song("qwin", 8, "opus", Some("2024-05-01T12:00:00Z")));
        let task = tokio::spawn(run(store.clone(), source.clone(), TokioClockForTest));
        settle_until("the pin committed", || store.lookup(&sid("pinned")).is_some()).await;
        let pins_after_startup = source.pins_calls();
        assert_eq!(pins_after_startup, 1);

        // A track boundary: the window moves, the handler kicks LIGHT.
        assert!(store.set_window(vec![sid("qwin")]));
        store.kick_light();
        settle_until("the window entry committed", || store.lookup(&sid("qwin")).is_some()).await;

        assert_eq!(
            source.pins_calls(),
            pins_after_startup,
            "a light pass must NOT spend a getStarred2 round trip"
        );
        assert_eq!(source.song_calls(), 1, "the window id resolved its own fingerprint");
        assert!(store.lookup(&sid("qwin")).is_some(), "the window entry is cached");
        let win = store
            .entries()
            .into_iter()
            .find(|e| e.id.0 == "qwin")
            .expect("window entry");
        assert!(!win.pinned, "an opportunistic window entry is not a pin");
        task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn a_light_pass_never_deletes_even_with_orphans_on_disk() {
        let dir = tmpdir("loop-light-nodelete");
        let store = loop_store(&dir, 1_000_000, 900);
        // A loose file no sidecar accounts for, dropped in AFTER the startup heal so
        // only a pass can be responsible for it. A FULL pass would remove it.
        std::fs::write(dir.join("loose.flac"), b"xxxx").expect("write orphan");
        // No pins, nothing to do - so the only thing a pass could do is delete.
        let source = Arc::new(FakeSource::new(Some(Vec::new())));
        let light_only = tokio::spawn({
            let store = store.clone();
            let source = source.clone();
            async move {
                let mut backoff = Backoff::default();
                run_pass(
                    &store,
                    source.as_ref(),
                    PassMode::Light,
                    &TokioClockForTest,
                    &mut backoff,
                    DOWNLOAD_BATCH,
                )
                .await
            }
        });
        let report = light_only.await.expect("light pass");
        assert_eq!(
            report,
            // A light pass DOES measure the disk (see
            // `a_light_pass_measures_the_disk_exactly_like_a_full_one`), so it reports
            // a budget; here the roomy test filesystem leaves the configured cap
            // standing. Everything it could DO is still zero.
            PassReport { budget: 1_000_000, ..PassReport::default() },
            "a light pass with no window does nothing"
        );
        assert!(dir.join("loose.flac").exists(), "a light pass deletes NOTHING");
        assert_eq!(source.pins_calls(), 0, "and never calls the server");

        // The full pass IS what heals it.
        let mut backoff = Backoff::default();
        run_pass(
            &store,
            source.as_ref(),
            PassMode::Full,
            &TokioClockForTest,
            &mut backoff,
            DOWNLOAD_BATCH,
        )
        .await;
        assert!(!dir.join("loose.flac").exists(), "the full pass sweeps the orphan");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn a_transient_pin_failure_skips_every_verdict() {
        let dir = tmpdir("loop-transient");
        // A committed, pinned entry whose fingerprint the server would now disagree
        // with - if only the server answered.
        let cached = song("keep", 12, "flac", Some("2024-05-01T12:00:00Z"));
        place(&dir, &cached, true, false, 100);
        let store = loop_store(&dir, 1_000_000, 900);
        let source = Arc::new(FakeSource::new(None)); // pins() errors
        let mut backoff = Backoff::default();
        run_pass(
            &store,
            source.as_ref(),
            PassMode::Full,
            &TokioClockForTest,
            &mut backoff,
            DOWNLOAD_BATCH,
        )
        .await;

        assert_eq!(source.pins_calls(), 1, "the pass tried");
        let e = store.entries().into_iter().next().expect("entry survives");
        assert!(e.pinned, "a flapping server must NOT demote the mirror");
        assert!(!e.stale, "nor mark it stale");
        assert!(store.lookup(&sid("keep")).is_some(), "and it keeps serving");
        assert_eq!(source.fetches(), Vec::<String>::new(), "nothing was re-fetched");

        // Contrast: an AUTHORITATIVE empty pin set is a real verdict, and it demotes
        // - keeping the bytes, because an unstar is not a delete.
        source.set_pins(Some(Vec::new()));
        run_pass(
            &store,
            source.as_ref(),
            PassMode::Full,
            &TokioClockForTest,
            &mut backoff,
            DOWNLOAD_BATCH,
        )
        .await;
        let e = store.entries().into_iter().next().expect("entry still there");
        assert!(!e.pinned, "an authoritative absence demotes");
        assert!(store.lookup(&sid("keep")).is_some(), "demoted, never deleted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The SERVER-BACK EDGE the offline seams hang off: a pass whose pin fetch
    // SUCCEEDED fires the hook (the daemon refreshes the id-only placeholders an
    // offline restore installed); a pass that could not reach the server must NOT -
    // "the server answered" is the whole content of the signal, and firing it while
    // still offline would spend one bounded request per placeholder for nothing.
    #[tokio::test(start_paused = true)]
    async fn only_a_pass_that_reached_the_server_fires_the_server_back_hook() {
        let dir = tmpdir("loop-server-back");
        let store = loop_store(&dir, 1_000_000, 900);
        let fired = Arc::new(AtomicU64::new(0));
        {
            let fired = fired.clone();
            store.set_server_back_hook(Arc::new(move || {
                fired.fetch_add(1, Ordering::Relaxed);
            }));
        }
        // Offline: the pin fetch fails.
        let source = Arc::new(FakeSource::new(None));
        let mut backoff = Backoff::default();
        run_pass(&store, source.as_ref(), PassMode::Full, &TokioClockForTest, &mut backoff, DOWNLOAD_BATCH).await;
        assert_eq!(fired.load(Ordering::Relaxed), 0, "a transient pass is not a server-back edge");

        // A LIGHT pass never talks to the server at all, so it cannot be one either.
        source.set_pins(Some(Vec::new()));
        run_pass(&store, source.as_ref(), PassMode::Light, &TokioClockForTest, &mut backoff, DOWNLOAD_BATCH).await;
        assert_eq!(fired.load(Ordering::Relaxed), 0, "a light pass never contacts the server");

        // The server answers: the edge fires.
        run_pass(&store, source.as_ref(), PassMode::Full, &TokioClockForTest, &mut backoff, DOWNLOAD_BATCH).await;
        assert_eq!(fired.load(Ordering::Relaxed), 1, "a successful pin fetch IS the server-back edge");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The suspect path end to end on the reconciler side (the other half of the
    // handler's suspect hook): a marked entry is de-offered at once, a LIGHT kick is
    // enough to schedule its replacement, and the bytes are only ever RENAMED OVER -
    // there is no window in which the entry exists neither old nor new.
    #[tokio::test(start_paused = true)]
    async fn a_suspect_entry_is_replaced_by_a_light_pass_and_never_deleted_first() {
        let dir = tmpdir("loop-suspect");
        let cached = song("sus", 12, "flac", Some("2024-05-01T12:00:00Z"));
        place(&dir, &cached, true, false, 100);
        let store = loop_store(&dir, 1_000_000, 900);
        assert!(store.lookup(&sid("sus")).is_some(), "offerable to begin with");

        // What the handler's Eof hook does when local bytes fail to play.
        assert!(store.mark_suspect(&sid("sus")));
        assert_eq!(store.lookup(&sid("sus")), None, "de-offered immediately");
        assert!(dir.join("sus.flac").exists(), "but NOT deleted - an offline pass can never destroy what it cannot replace");

        // A light pass (what kick_light drives) is enough: suspect work is never
        // deferred behind bulk backfill and needs no getStarred2.
        let source = Arc::new(FakeSource::new(Some(vec![cached.clone()])));
        store.set_playback_remote(true);
        let mut backoff = Backoff::default();
        run_pass(&store, source.as_ref(), PassMode::Light, &TokioClockForTest, &mut backoff, DOWNLOAD_BATCH).await;
        assert_eq!(source.fetches(), vec!["sus".to_string()], "the replacement is the light pass's first job");
        assert_eq!(
            store.entries().into_iter().next().map(|e| e.suspect),
            Some(false),
            "a successful COMMIT is what clears suspect - there is no clear_suspect"
        );
        assert!(store.lookup(&sid("sus")).is_some(), "and the entry is offerable again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // THE FREE-SPACE CLAMP IS NOT A FULL-PASS FEATURE. "hypodj cannot fill the disk"
    // rests on `plan_pass` refusing every write at `max_bytes == 0`, and `max_bytes`
    // only ever becomes 0 because `run_pass` measured the filesystem. A light kick
    // fires at EVERY track boundary and queue edit, so measuring only on a full pass
    // left the busiest path budgeting against the raw configured cap: window originals
    // (31 MiB median, 415 MiB tail on his library) written onto a disk the previous
    // full pass had already declared unwritable, roughly once per track, each one
    // deleted again by the next full pass.
    //
    // Asserted without depending on this machine's free space: the configured cap is
    // `u64::MAX`, and the reserve is at least `STORE_RESERVE_FLOOR`, so ANY successful
    // measurement must come back strictly below the cap. A pass that skipped the
    // measurement reports the cap verbatim.
    #[tokio::test(start_paused = true)]
    async fn a_light_pass_measures_the_disk_exactly_like_a_full_one() {
        let dir = tmpdir("loop-light-budget");
        let store = loop_store(&dir, u64::MAX, 900);
        let source = Arc::new(FakeSource::new(Some(Vec::new())));
        let mut backoff = Backoff::default();

        let full =
            run_pass(&store, source.as_ref(), PassMode::Full, &TokioClockForTest, &mut backoff, DOWNLOAD_BATCH)
                .await;
        assert!(full.budget < u64::MAX, "a full pass clamps to observed free space");

        let light =
            run_pass(&store, source.as_ref(), PassMode::Light, &TokioClockForTest, &mut backoff, DOWNLOAD_BATCH)
                .await;
        assert!(
            light.budget < u64::MAX,
            "and so does a LIGHT one - otherwise the breach regime is a full-pass-only promise"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn a_drifted_fingerprint_marks_stale_and_the_old_bytes_keep_serving() {
        let dir = tmpdir("loop-stale");
        let cached = song("drift", 12, "flac", Some("2024-05-01T12:00:00Z"));
        place(&dir, &cached, true, false, 100);
        let store = loop_store(&dir, 1_000_000, 900);
        // The server now reports a re-imported original: same id, bigger file.
        let reimported = song("drift", 20, "flac", Some("2025-01-09T08:00:00Z"));
        let source = Arc::new(FakeSource::new(Some(vec![reimported])));
        // Playback is remote, so BULK work (the stale replacement) defers - and the
        // old bytes must still be offered throughout.
        store.set_playback_remote(true);
        let mut backoff = Backoff::default();
        run_pass(
            &store,
            source.as_ref(),
            PassMode::Full,
            &TokioClockForTest,
            &mut backoff,
            DOWNLOAD_BATCH,
        )
        .await;
        let e = store.entries().into_iter().next().expect("entry");
        assert!(e.stale, "drift is marked");
        assert_eq!(e.size, 12, "but the OLD bytes are still what is on disk");
        assert!(store.lookup(&sid("drift")).is_some(), "and still served");
        assert_eq!(source.fetches(), Vec::<String>::new(), "bulk work deferred while streaming");

        // Playback goes local: the replacement lands and renames OVER the old file.
        store.set_playback_remote(false);
        run_pass(
            &store,
            source.as_ref(),
            PassMode::Full,
            &TokioClockForTest,
            &mut backoff,
            DOWNLOAD_BATCH,
        )
        .await;
        let e = store.entries().into_iter().next().expect("entry");
        assert!(!e.stale, "the commit clears the mark");
        assert_eq!(e.size, 20, "the replacement is what is on disk now");
        assert_eq!(
            std::fs::metadata(dir.join("drift.flac")).expect("audio").len(),
            20
        );
        assert_eq!(source.fetches(), vec!["drift".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn the_full_pass_recurs_on_the_configured_interval() {
        let dir = tmpdir("loop-cadence");
        let store = loop_store(&dir, 1_000_000, 600);
        // An empty pin set: every pass is pure bookkeeping, so the ONLY observable
        // is the getStarred2 round trip a full pass makes.
        let source = Arc::new(FakeSource::new(Some(Vec::new())));
        let task = tokio::spawn(run(store.clone(), source.clone(), TokioClockForTest));
        settle().await;
        assert_eq!(source.pins_calls(), 1, "startup is a full pass");

        // Just short of the deadline: nothing yet.
        tokio::time::advance(Duration::from_secs(599)).await;
        settle().await;
        assert_eq!(source.pins_calls(), 1, "the interval has not elapsed");

        tokio::time::advance(Duration::from_secs(2)).await;
        settle().await;
        assert_eq!(source.pins_calls(), 2, "the interval tick runs a full pass");

        // And a LIGHT kick in between does not consume the full cadence.
        assert!(store.set_window(vec![sid("nope")]));
        store.kick_light();
        settle().await;
        assert_eq!(source.pins_calls(), 2, "a light kick is not a full pass");
        tokio::time::advance(Duration::from_secs(601)).await;
        settle().await;
        assert_eq!(source.pins_calls(), 3, "the cadence survived the light kick");
        task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_kick_upgrades_the_next_wake_to_a_full_pass() {
        let dir = tmpdir("loop-fullkick");
        let store = loop_store(&dir, 1_000_000, 3600);
        let source = Arc::new(FakeSource::new(Some(Vec::new())));
        let task = tokio::spawn(run(store.clone(), source.clone(), TokioClockForTest));
        settle().await;
        assert_eq!(source.pins_calls(), 1);

        // A star flip: the pin set itself may have changed, so this must reach the
        // server rather than wait out an hour of interval.
        store.kick_full();
        settle().await;
        assert_eq!(source.pins_calls(), 2, "a full kick runs a full pass at once");
        task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn only_a_kicked_full_pass_drops_the_memoised_expansion() {
        // THE COST CONTROL for album and artist pins. A full pass expands every
        // starred album, which on a real server is ~43 round trips and up to 90 s;
        // and `re_enter` repeats the same mode after every drained batch of four, so
        // a 290-track cold backfill is ~73 chained FULL passes. Invalidating on every
        // one of them would multiply that by 73.
        //
        // A KICK is the one thing that means the PIN SET ITSELF may have changed - a
        // star or unstar is the only thing that fires one - so it alone invalidates.
        // The interval tick and the re-entry chain deliberately do not, and freshness
        // on the gesture path is exact because the kick and the invalidation are the
        // same event.
        let dir = tmpdir("loop-invalidate");
        let store = loop_store(&dir, 1_000_000, 60);
        // Four pins of the same size: draining a full batch of four is what makes the
        // loop re-enter, which is the chain this memo exists for.
        let pins: Vec<Song> = (0..8)
            .map(|i| song(&format!("p{i}"), 12, "flac", Some("2024-05-01T12:00:00Z")))
            .collect();
        let source = Arc::new(FakeSource::new(Some(pins)));
        let task = tokio::spawn(run(store.clone(), source.clone(), TokioClockForTest));

        settle_until("all eight committed", || store.entries().len() == 8).await;
        // The backfill chained at least one re-entry (8 pins, batch of 4)...
        assert!(source.pins_calls() >= 2, "the chain re-entered: {}", source.pins_calls());
        // ...and not one of those passes threw the expansion away.
        assert_eq!(
            source.invalidations(),
            0,
            "a re-entry chain must never re-expand: that is 73 expansions on a cold mirror"
        );

        // An interval tick is likewise not a reason to re-expand.
        tokio::time::advance(Duration::from_secs(120)).await;
        settle().await;
        assert_eq!(source.invalidations(), 0, "nor does the ordinary cadence");

        // A star or unstar does.
        store.kick_full();
        settle_until("the kicked pass invalidated", || source.invalidations() == 1).await;
        assert_eq!(source.invalidations(), 1, "a star flip re-expands, exactly once");
        task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn playback_starting_mid_pass_abandons_bulk_work_but_never_the_window() {
        // `defer_bulk` is sampled ONCE per pass, so without a re-check inside the
        // executor the rest of a four-deep backfill batch keeps pulling originals
        // after he presses play - which is precisely what the deferral exists to
        // prevent, and a 12 GiB backfill is exactly the case it exists for.
        let dir = tmpdir("loop-yield");
        let store = loop_store(&dir, 1_000_000, 3600);
        let pins: Vec<Song> = (0..4)
            .map(|i| song(&format!("b{i}"), 12, "flac", Some("2024-05-01T12:00:00Z")))
            .collect();
        let source = Arc::new(FakeSource::with_pin_set(Some(PinSet::of_songs(pins))));
        // A window id nobody has cached: the work he is about to HEAR.
        let wanted = song("window-id", 12, "flac", Some("2024-05-01T12:00:00Z"));
        source.add_to_catalog(wanted.clone());
        store.set_window(vec![sid("window-id")]);

        // THE DECK IS QUIET WHEN THE PASS IS PLANNED, so the planner admits the
        // window download AND all four backfills. He presses play during the first
        // fetch. Only a re-check inside the executor can catch that - the planner
        // sampled `defer_bulk` once, before any of this happened.
        let deck = store.clone();
        source.on_fetch(move |id| {
            if id == "window-id" {
                deck.set_playback_remote(true);
            }
        });

        let mut backoff = Backoff::default();
        run_pass(&store, source.as_ref(), PassMode::Full, &TokioClockForTest, &mut backoff, 8)
            .await;
        assert_eq!(
            source.fetches(),
            vec!["window-id".to_string()],
            "the window download he is about to hear completes, and the four backfills \
             the same plan admitted all yield the moment the deck goes remote"
        );

        // Stopped, the same work proceeds - the deferral is a wait, not a refusal.
        source.on_fetch(|_| {});
        store.set_playback_remote(false);
        run_pass(&store, source.as_ref(), PassMode::Full, &TokioClockForTest, &mut backoff, 8)
            .await;
        assert_eq!(store.entries().len(), 5, "a quiet deck mirrors everything");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn store_pause_suspends_bulk_work_without_ever_silencing_the_next_track() {
        // The interrupt path the store had none of. `store pause` suspends the same
        // bulk categories `defer_bulk` does - and no more: pausing the MIRROR must
        // never make the next track stream.
        let dir = tmpdir("loop-paused");
        let store = loop_store(&dir, 1_000_000, 3600);
        let source = Arc::new(FakeSource::new(Some(vec![song(
            "bulk",
            12,
            "flac",
            Some("2024-05-01T12:00:00Z"),
        )])));
        source.add_to_catalog(song("window-id", 12, "flac", Some("2024-05-01T12:00:00Z")));
        store.set_window(vec![sid("window-id")]);
        store.set_paused(true);

        let mut backoff = Backoff::default();
        run_pass(&store, source.as_ref(), PassMode::Full, &TokioClockForTest, &mut backoff, 8)
            .await;
        assert_eq!(
            source.fetches(),
            vec!["window-id".to_string()],
            "paused, the mirror still fetches what he is about to hear"
        );
        assert_eq!(store.status().waiting, StoreWaiting::Paused, "and it says why");

        // Resuming both clears the suspension and asks for a pass at once, so
        // "resume" is not indistinguishable from "still paused" for fifteen minutes.
        assert!(!store.set_paused(false));
        assert!(store.take_full_request_for_test(), "resume kicks a full pass");
        run_pass(&store, source.as_ref(), PassMode::Full, &TokioClockForTest, &mut backoff, 8)
            .await;
        assert!(
            source.fetches().contains(&"bulk".to_string()),
            "and the bulk work resumes: {:?}",
            source.fetches()
        );

        // NEVER PERSISTED: the flag lives in the process, so a restart resumes
        // mirroring and pausing can never become a forgotten config.
        store.set_paused(true);
        drop(store);
        let reopened = loop_store(&dir, 1_000_000, 3600);
        assert!(!reopened.paused(), "a restart always resumes the mirror");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_download_backs_off_instead_of_hammering() {
        let dir = tmpdir("loop-backoff");
        let store = loop_store(&dir, 1_000_000, 3600);
        let source = Arc::new(FakeSource::new(Some(vec![song(
            "bad",
            12,
            "flac",
            Some("2024-05-01T12:00:00Z"),
        )])));
        source.set_fetch_ok(false);
        let mut backoff = Backoff::default();
        for _ in 0..3 {
            run_pass(
                &store,
                source.as_ref(),
                PassMode::Full,
                &TokioClockForTest,
                &mut backoff,
                DOWNLOAD_BATCH,
            )
            .await;
        }
        assert_eq!(
            source.fetches(),
            vec!["bad".to_string()],
            "three passes, ONE attempt: the backoff holds it off"
        );
        assert!(store.entries().is_empty(), "and nothing half-written was committed");
        assert_eq!(
            names(&dir),
            Vec::<String>::new(),
            "the failed temp is cleaned up, not left behind"
        );

        // Past the first backoff step the retry is allowed again - and now succeeds.
        source.set_fetch_ok(true);
        tokio::time::advance(DOWNLOAD_BACKOFF_BASE + Duration::from_secs(1)).await;
        run_pass(
            &store,
            source.as_ref(),
            PassMode::Full,
            &TokioClockForTest,
            &mut backoff,
            DOWNLOAD_BATCH,
        )
        .await;
        assert_eq!(source.fetches(), vec!["bad".to_string(), "bad".to_string()]);
        assert!(store.lookup(&sid("bad")).is_some(), "the retry committed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backoff_doubles_per_failure_and_clears_on_success() {
        let mut b = Backoff::default();
        let id = sid("x");
        let t0 = tokio::time::Instant::now();
        assert!(b.ready(&id, t0), "an id with no history is always ready");
        b.fail(&id, t0);
        assert!(!b.ready(&id, t0 + DOWNLOAD_BACKOFF_BASE - Duration::from_secs(1)));
        assert!(b.ready(&id, t0 + DOWNLOAD_BACKOFF_BASE));
        // Second consecutive failure: twice the wait.
        b.fail(&id, t0);
        assert!(!b.ready(&id, t0 + DOWNLOAD_BACKOFF_BASE));
        assert!(b.ready(&id, t0 + DOWNLOAD_BACKOFF_BASE * 2));
        // The ceiling holds however many times it fails, and never overflows - but
        // past DOWNLOAD_GIVE_UP_AFTER the id is GIVEN UP on rather than merely slowed,
        // so no amount of waiting makes it ready again.
        for _ in 0..64 {
            b.fail(&id, t0);
        }
        assert!(
            !b.ready(&id, t0 + DOWNLOAD_BACKOFF_MAX),
            "a permanently-invalid download must stop being attempted, not just slow \
             down: the backoff bounds the RATE, this bounds the TOTAL"
        );
        assert_eq!(b.attempt(&id, t0 + DOWNLOAD_BACKOFF_MAX), Attempt::GivenUp);
        b.succeed(&id);
        assert!(b.ready(&id, t0), "a success forgets the history entirely");
    }

    // The rate bound and the total bound are different guarantees, and the second is
    // the one that was missing: observed live, a starred song whose server metadata
    // declares 3 MiB while the download serves 29.2 MiB retried ~24 times a day
    // forever, because waiting cannot fix a server disagreeing with itself.
    #[test]
    fn an_id_is_given_up_on_only_after_the_full_run_of_failures() {
        let mut b = Backoff::default();
        let id = sid("x");
        let t0 = tokio::time::Instant::now();
        for i in 1..DOWNLOAD_GIVE_UP_AFTER {
            b.fail(&id, t0);
            assert_ne!(
                b.attempt(&id, t0 + DOWNLOAD_BACKOFF_MAX),
                Attempt::GivenUp,
                "failure {i} is still inside the transient budget"
            );
            assert!(!b.just_gave_up(&id));
        }
        b.fail(&id, t0);
        assert_eq!(b.attempt(&id, t0 + DOWNLOAD_BACKOFF_MAX), Attempt::GivenUp);
        assert!(b.just_gave_up(&id), "the warn fires exactly on the crossing");
        // And exactly once: a further failure must not re-announce it.
        b.fail(&id, t0);
        assert!(!b.just_gave_up(&id), "the give-up is reported ONCE, never every pass");
        assert_eq!(b.attempt(&id, t0 + DOWNLOAD_BACKOFF_MAX), Attempt::GivenUp);
    }

    #[test]
    fn a_waiting_id_is_distinguishable_from_a_given_up_one() {
        // The caller logs them differently - a wait is debug, a give-up is a one-time
        // warn - so conflating them would either bury the give-up or restore the spam.
        let mut b = Backoff::default();
        let id = sid("x");
        let t0 = tokio::time::Instant::now();
        b.fail(&id, t0);
        assert_eq!(b.attempt(&id, t0), Attempt::Waiting);
        assert_eq!(b.attempt(&id, t0 + DOWNLOAD_BACKOFF_BASE), Attempt::Now);
    }

    #[test]
    fn re_enter_requires_progress_not_merely_outstanding_work() {
        // A full batch that landed something: keep draining.
        assert!(PassReport { scheduled: 4, given_up: 0, committed: 1, evicted: 0, ..PassReport::default() }.re_enter(4, 0));
        // A full batch that landed NOTHING must sleep, or a permanently failing
        // download would spin the reconciler at full speed forever.
        assert!(!PassReport { scheduled: 4, given_up: 0, committed: 0, evicted: 0, ..PassReport::default() }.re_enter(4, 0));
        // A partial batch is all there was: nothing more to drain.
        assert!(!PassReport { scheduled: 2, given_up: 0, committed: 2, evicted: 0, ..PassReport::default() }.re_enter(4, 0));
        // An eviction re-enters so the reclaimed headroom is usable now.
        assert!(PassReport { scheduled: 0, given_up: 0, committed: 0, evicted: 1, ..PassReport::default() }.re_enter(4, 0));
    }

    #[test]
    fn eviction_only_re_entry_is_capped_but_a_draining_download_chain_is_not() {
        // The backstop behind the progress gate. An eviction chain is bounded: a
        // filesystem that reports reclamation it did not perform gets at most
        // MAX_EVICTION_CHAIN passes, not an endless run of directory scans and
        // getStarred2 round trips.
        let evicting = PassReport { scheduled: 0, given_up: 0, committed: 0, evicted: 1, ..PassReport::default() };
        assert!(evicting.re_enter(4, MAX_EVICTION_CHAIN - 1), "the last link is allowed");
        assert!(!evicting.re_enter(4, MAX_EVICTION_CHAIN), "and then the loop must wait");
        assert!(!evicting.re_enter(4, MAX_EVICTION_CHAIN + 9));

        // A chain of COMMITTED downloads is not capped: every link cost a real
        // original, the desired set is finite, and capping it would stall a cold
        // backfill for a whole interval.
        let draining = PassReport { scheduled: 4, given_up: 0, committed: 4, evicted: 0, ..PassReport::default() };
        assert!(draining.re_enter(4, MAX_EVICTION_CHAIN * 100));
        assert!(draining.drained_a_full_batch(4), "which is what resets the chain");
        assert!(!evicting.drained_a_full_batch(4));
        // A pass that both drained a batch and evicted still counts as draining.
        let both = PassReport { scheduled: 4, given_up: 0, committed: 1, evicted: 2, ..PassReport::default() };
        assert!(both.re_enter(4, MAX_EVICTION_CHAIN));
    }

    #[tokio::test(start_paused = true)]
    async fn resync_carries_the_in_memory_only_flags_across_a_scan() {
        let dir = tmpdir("loop-resync");
        let s = song("m", 12, "flac", Some("2024-05-01T12:00:00Z"));
        place(&dir, &s, true, false, 100);
        let store = loop_store(&dir, 1_000_000, 900);
        assert!(store.mark_suspect(&sid("m")));
        store.note_played(&sid("m"), 999);

        let scan = scan_dir(&dir).expect("scan");
        store.resync_from_scan(scan.entries);
        let e = store.entries().into_iter().next().expect("entry");
        assert!(e.suspect, "suspect is in-memory only; a scan must not clear it");
        assert_eq!(e.last_played_unix, 999, "an unflushed bump outranks the sidecar");
        assert!(e.recency_dirty, "and stays dirty until the flush");
        assert!(store.lookup(&sid("m")).is_none(), "a suspect entry is de-offered");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_pass_flushes_recency_and_evicts_the_coldest_unpinned_entry() {
        let dir = tmpdir("loop-evict");
        let cold = song("cold", 40, "flac", Some("2024-05-01T12:00:00Z"));
        let warm = song("warm", 40, "flac", Some("2024-05-01T12:00:00Z"));
        place(&dir, &cold, false, false, 10);
        place(&dir, &warm, false, false, 20);
        // 80 bytes on disk against a 50 byte budget: exactly one entry must go.
        let small = loop_store(&dir, 50, 900);
        // Play the entry the SIDECARS say is colder, so its recency overtakes the
        // other one: real LRU, which is the whole point of the resolve-time bump.
        small.note_played(&sid("cold"), 5_000);
        let source = Arc::new(FakeSource::new(Some(Vec::new())));
        let mut backoff = Backoff::default();
        run_pass(
            &small,
            source.as_ref(),
            PassMode::Full,
            &TokioClockForTest,
            &mut backoff,
            DOWNLOAD_BATCH,
        )
        .await;
        let ids: Vec<String> = small.entries().into_iter().map(|e| e.id.0).collect();
        assert_eq!(ids, vec!["cold".to_string()], "the bumped entry survives; the colder one goes");
        assert!(!dir.join("warm.flac").exists(), "its bytes are reclaimed");
        assert!(!dir.join("warm.toml").exists());
        // The surviving entry's bump reached its sidecar.
        let sc = small.read_sidecar(&sid("cold")).expect("sidecar");
        assert_eq!(sc.last_played_unix, 5_000, "the recency flush persisted the bump");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── an eviction the filesystem refuses ──────────────────────────────────
    //
    // The one place the pass used to ASSUME its work landed. On a store root that
    // will not unlink - a read-only remount, an immutable file, a disk gone bad -
    // an eviction reclaims nothing, and reporting it as progress is exactly what
    // makes the reconciler re-enter at once: a tight loop of whole directory scans
    // and getStarred2 round trips, forever, on a store that is still over budget.

    /// Make every unlink in `dir` fail (EACCES) without touching the files in it.
    fn deny_unlink(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(dir).expect("stat").permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(dir, perms).expect("chmod read-only");
    }

    /// Undo [`deny_unlink`], so the tempdir can be cleaned up. ALWAYS called before
    /// the assertions, so a failing test still leaves nothing behind.
    fn allow_unlink(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(dir).expect("stat").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(dir, perms).expect("chmod writable");
    }

    #[tokio::test(start_paused = true)]
    async fn an_eviction_that_reclaimed_nothing_is_not_counted_as_progress() {
        let dir = tmpdir("loop-evict-denied");
        place(&dir, &song("cold", 40, "flac", Some("2024-05-01T12:00:00Z")), false, false, 10);
        place(&dir, &song("warm", 40, "flac", Some("2024-05-01T12:00:00Z")), false, false, 20);
        // 80 bytes against a 50 byte budget: the planner schedules exactly one
        // eviction, which the filesystem then refuses.
        let small = loop_store(&dir, 50, 900);
        deny_unlink(&dir);
        let source = Arc::new(FakeSource::new(Some(Vec::new())));
        let mut backoff = Backoff::default();
        let report = run_pass(
            &small,
            source.as_ref(),
            PassMode::Full,
            &TokioClockForTest,
            &mut backoff,
            DOWNLOAD_BATCH,
        )
        .await;
        let refused = dir.join("cold.flac").exists() && dir.join("cold.toml").exists();
        allow_unlink(&dir);
        if !refused {
            // Some environments (a root build user) delete inside a read-only
            // directory anyway; the scenario cannot be built there, so do not
            // pretend to have tested it.
            eprintln!("skipping: this process can unlink inside a read-only directory");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        assert_eq!(
            report.evicted, 0,
            "an unlink the filesystem refused freed nothing and must not read as progress"
        );
        assert!(!report.re_enter(DOWNLOAD_BATCH, 0), "so the pass must not re-enter");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_unlink_degrades_to_a_warn_instead_of_hot_looping_the_server() {
        let dir = tmpdir("loop-evict-hotloop");
        place(&dir, &song("cold", 40, "flac", Some("2024-05-01T12:00:00Z")), false, false, 10);
        place(&dir, &song("warm", 40, "flac", Some("2024-05-01T12:00:00Z")), false, false, 20);
        let small = loop_store(&dir, 50, 900);
        deny_unlink(&dir);
        // An AUTHORITATIVE empty pin set, so both entries are evictable and every
        // full pass finds the same 80-bytes-over-50 situation it just "fixed".
        let source = Arc::new(FakeSource::new(Some(Vec::new())));
        let task = tokio::spawn(run(small.clone(), source.clone(), TokioClockForTest));
        // WAIT FOR THE STARTUP PASS FIRST. Without that floor the ceiling below is
        // satisfied by `calls == 0`, so a loaded machine that simply never got the
        // reconciler going would leave this test asserting nothing at all - it could
        // only ever fail vacuously, never catch the hot loop it exists for.
        let started = settle_reached(|| source.pins_calls() >= 1).await;
        // No virtual time passes here, so the 900s interval never elapses: EVERY
        // pass beyond the startup one is an immediate re-entry.
        settle().await;
        task.abort();
        let refused = dir.join("cold.flac").exists() && dir.join("cold.toml").exists();
        allow_unlink(&dir);
        let calls = source.pins_calls();
        if !refused {
            eprintln!("skipping: this process can unlink inside a read-only directory");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        assert!(started, "the reconciler never ran its startup pass, so nothing below was tested");
        assert!(
            (1..=2).contains(&calls),
            "a store that cannot reclaim must fall back to the interval; instead {calls} full passes (each a scan plus a getStarred2) ran back to back"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn pin_starred_false_is_an_authoritative_empty_set_not_an_unknown_one() {
        // THE ONE PATH where widening `pins()` from `Vec<Song>` to a `PinSet` could
        // have silently started returning "no information" instead of "nothing is
        // starred". An `Err` here would KEEP every claim - transient-keeps-the-claim
        // is the whole of offline mode - so the knob would quietly stop demoting
        // anything and the mirror would freeze exactly as it was, forever.
        //
        // It is also the one path that must answer WITHOUT the network: the port
        // below refuses connections, so a `pins()` that reached the server at all
        // would fail this rather than pass it.
        let cfg = crate::config::ServerConfig {
            url: "http://127.0.0.1:1/never-called".to_string(),
            username: "u".to_string(),
            password: "p".to_string(),
            client_name: "test".to_string(),
        };
        let client = match std::panic::catch_unwind(|| SubsonicClient::connect(&cfg)) {
            Ok(Ok(c)) => Arc::new(c),
            _ => {
                eprintln!("skipping: no CA certs (sandbox); connect() not exercisable here");
                return;
            }
        };
        let source = SubsonicPinSource::new(client, false, TokioClockForTest);
        let pins = source
            .pins()
            .await
            .expect("pin_starred = false is a VERDICT, never a transient failure");
        assert!(pins.is_empty(), "and the verdict is EMPTY, which demotes every tier");
        assert_eq!(pins.groups, Vec::new());

        // Fed to the planner it does exactly what the knob promises: every pin
        // demotes, the bytes stay, and only the queue window is still mirrored.
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.entries = vec![pinned_entry("was-song", 100, 1), pinned_entry("was-album", 100, 1)];
        input.pins = Some(pins);
        let demoted: Vec<String> = plan_pass(&input)
            .into_iter()
            .filter_map(|a| match a {
                StoreAction::SetPinned { id, pinned: false } => Some(id.0),
                _ => None,
            })
            .collect();
        assert_eq!(demoted, vec!["was-song".to_string(), "was-album".to_string()]);
    }

    /// The production [`TokioClock`], re-exported under a local name so the loop
    /// tests read as "the same clock production uses, merely paused".
    use crate::clock::TokioClock as TokioClockForTest;

    // ── The pin EXPANSION, over the catalogue seam ───────────────────────────
    //
    // Everything below exercises `PinExpansion` itself - the code that turns a
    // starred album or artist into tracks. It is the actual feature, and a
    // `PinSource` fake cannot reach it: a double there REPLACES the expansion.

    /// What the fake catalogue answers for one album or artist.
    enum Reply {
        Songs(Vec<Song>),
        Albums(Vec<Album>),
        /// The server said, authoritatively, that it is gone (API code 70).
        Gone,
        /// A transport wobble - the case the all-or-nothing policy exists for.
        Flaky,
    }

    #[derive(Default)]
    struct FakeCatalog {
        starred: Mutex<Option<Starred>>,
        albums: Mutex<HashMap<String, Reply>>,
        artists: Mutex<HashMap<String, Reply>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeCatalog {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn set_starred(&self, s: Starred) {
            *self.starred.lock().unwrap() = Some(s);
        }

        fn set_album(&self, id: &str, r: Reply) {
            self.albums.lock().unwrap().insert(id.to_string(), r);
        }

        fn set_artist(&self, id: &str, r: Reply) {
            self.artists.lock().unwrap().insert(id.to_string(), r);
        }

        /// Every catalogue call this fake has served, in order.
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn album_calls(&self, id: &str) -> usize {
            let want = format!("album:{id}");
            self.calls.lock().unwrap().iter().filter(|c| **c == want).count()
        }
    }

    impl PinCatalog for Arc<FakeCatalog> {
        async fn starred(&self) -> Result<Starred, SubsonicError> {
            self.calls.lock().unwrap().push("starred".to_string());
            match self.starred.lock().unwrap().take() {
                Some(s) => Ok(s),
                None => Ok(Starred { songs: Vec::new(), albums: Vec::new(), artists: Vec::new() }),
            }
        }

        async fn album_songs(&self, id: &AlbumId) -> Result<Vec<Song>, SubsonicError> {
            self.calls.lock().unwrap().push(format!("album:{}", id.0));
            match self.albums.lock().unwrap().get(&id.0) {
                Some(Reply::Songs(v)) => Ok(v.clone()),
                Some(Reply::Gone) => Err(SubsonicError::NotFound(id.0.clone())),
                Some(Reply::Flaky) => Err(SubsonicError::Request("wobble".into())),
                _ => Ok(Vec::new()),
            }
        }

        async fn artist_albums(&self, id: &ArtistId) -> Result<Vec<Album>, SubsonicError> {
            self.calls.lock().unwrap().push(format!("artist:{}", id.0));
            match self.artists.lock().unwrap().get(&id.0) {
                Some(Reply::Albums(v)) => Ok(v.clone()),
                Some(Reply::Gone) => Err(SubsonicError::NotFound(id.0.clone())),
                Some(Reply::Flaky) => Err(SubsonicError::Request("wobble".into())),
                _ => Ok(Vec::new()),
            }
        }
    }

    fn album(id: &str, song_count: u32, created: Option<&str>) -> Album {
        Album {
            id: AlbumId(id.to_string()),
            name: format!("al-{id}"),
            artist: "ar".into(),
            artist_id: None,
            year: None,
            genre: None,
            cover_art: None,
            song_count,
            created: created.map(str::to_string),
        }
    }

    fn artist(id: &str) -> crate::model::Artist {
        crate::model::Artist {
            id: ArtistId(id.to_string()),
            name: format!("ar-{id}"),
            album_count: 0,
            starred: true,
            cover_art: None,
        }
    }

    /// The frontier-facing shape of an expansion: (tier, group id, name, track ids).
    fn shape(set: &PinSet) -> Vec<(PinTier, String, String, Vec<String>)> {
        set.groups
            .iter()
            .map(|g| {
                (
                    g.tier,
                    g.id.clone(),
                    g.name.clone(),
                    g.songs.iter().map(|s| s.id.0.clone()).collect(),
                )
            })
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn the_expansion_walks_songs_then_albums_then_one_group_per_artist_album() {
        // THE THREE BUCKETS TO GROUPS, in the order the frontier then walks. A starred
        // ARTIST fans out to ONE GROUP PER ALBUM, newest first, so a huge catalogue
        // degrades album by album at the ceiling instead of being refused whole - and
        // each group is NAMED "artist - album" so a deferral is legible in the `store`
        // listing.
        let cat = FakeCatalog::new();
        cat.set_starred(Starred {
            songs: vec![song("s1", 10, "flac", None)],
            albums: vec![album("a1", 2, None)],
            artists: vec![artist("ar1")],
        });
        cat.set_album("a1", Reply::Songs(vec![song("t1", 10, "flac", None), song("t2", 10, "flac", None)]));
        cat.set_artist(
            "ar1",
            Reply::Albums(vec![
                album("old", 1, Some("2019-01-01T00:00:00Z")),
                album("new", 1, Some("2024-01-01T00:00:00Z")),
            ]),
        );
        cat.set_album("old", Reply::Songs(vec![song("o1", 10, "flac", None)]));
        cat.set_album("new", Reply::Songs(vec![song("n1", 10, "flac", None)]));

        let exp = PinExpansion::new(cat.clone(), TokioClockForTest);
        let set = exp.pins().await.expect("expansion");
        assert_eq!(
            shape(&set),
            vec![
                (PinTier::Song, "s1".to_string(), "t-s1".to_string(), vec!["s1".to_string()]),
                (PinTier::Album, "a1".to_string(), "al-a1".to_string(), vec!["t1".to_string(), "t2".to_string()]),
                (PinTier::Artist, "new".to_string(), "ar-ar1 - al-new".to_string(), vec!["n1".to_string()]),
                (PinTier::Artist, "old".to_string(), "ar-ar1 - al-old".to_string(), vec!["o1".to_string()]),
            ],
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_gone_album_expands_to_nothing_while_a_flaky_one_aborts_the_whole_set() {
        // THE POLICY THE ENTIRE DEMOTE-SAFETY ARGUMENT RESTS ON. A definitive NotFound
        // is a VERDICT - that album is gone, so it expands to nothing and its tracks
        // demote correctly. Any other error is TRANSIENT, and returning a partial set
        // would look authoritative: 33 of 36 albums present would demote the other
        // three over one wobbly `getAlbum` and evict them next pass. Getting these two
        // backwards is silent and expensive in both directions, and nothing tested it.
        let cat = FakeCatalog::new();
        cat.set_starred(Starred {
            songs: Vec::new(),
            albums: vec![album("gone", 1, None), album("fine", 1, None)],
            artists: Vec::new(),
        });
        cat.set_album("gone", Reply::Gone);
        cat.set_album("fine", Reply::Songs(vec![song("f1", 10, "flac", None)]));
        let exp = PinExpansion::new(cat.clone(), TokioClockForTest);
        let set = exp.pins().await.expect("a gone album is not a failure");
        assert_eq!(
            shape(&set),
            vec![
                (PinTier::Album, "gone".to_string(), "al-gone".to_string(), Vec::new()),
                (PinTier::Album, "fine".to_string(), "al-fine".to_string(), vec!["f1".to_string()]),
            ],
            "gone expands to an EMPTY group, and the rest of the walk continues"
        );

        // A transport wobble on ONE album takes the whole expansion with it.
        let cat = FakeCatalog::new();
        cat.set_starred(Starred {
            songs: Vec::new(),
            albums: vec![album("flaky", 1, None), album("fine", 1, None)],
            artists: Vec::new(),
        });
        cat.set_album("flaky", Reply::Flaky);
        cat.set_album("fine", Reply::Songs(vec![song("f1", 10, "flac", None)]));
        let exp = PinExpansion::new(cat.clone(), TokioClockForTest);
        assert!(exp.pins().await.is_err(), "a transient failure is ALL or nothing");

        // Same rule one level up: a flaky getArtist aborts, a gone one expands to
        // nothing.
        let cat = FakeCatalog::new();
        cat.set_starred(Starred {
            songs: Vec::new(),
            albums: Vec::new(),
            artists: vec![artist("flaky")],
        });
        cat.set_artist("flaky", Reply::Flaky);
        let exp = PinExpansion::new(cat.clone(), TokioClockForTest);
        assert!(exp.pins().await.is_err());

        let cat = FakeCatalog::new();
        cat.set_starred(Starred { songs: Vec::new(), albums: Vec::new(), artists: vec![artist("gone")] });
        cat.set_artist("gone", Reply::Gone);
        let exp = PinExpansion::new(cat.clone(), TokioClockForTest);
        assert_eq!(exp.pins().await.expect("gone artist").groups.len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_artist_over_the_album_cap_keeps_the_newest_and_no_more() {
        // A RUNAWAY BRAKE ON ROUND TRIPS, not a statement about what "starred" means.
        // The cap truncates AFTER the newest-first sort, so what survives is their
        // recent work rather than an arbitrary slice of whatever order the server used.
        let cat = FakeCatalog::new();
        cat.set_starred(Starred { songs: Vec::new(), albums: Vec::new(), artists: vec![artist("ar1")] });
        // Oldest first on the wire, so an unsorted truncation would keep the wrong end.
        let albums: Vec<Album> = (0..ARTIST_ALBUM_CAP + 5)
            .map(|i| album(&format!("al{i:03}"), 1, Some(&format!("20{:02}-01-01T00:00:00Z", i % 100))))
            .collect();
        for a in &albums {
            cat.set_album(&a.id.0, Reply::Songs(vec![song(&format!("s-{}", a.id.0), 10, "flac", None)]));
        }
        cat.set_artist("ar1", Reply::Albums(albums));
        let exp = PinExpansion::new(cat.clone(), TokioClockForTest);
        let set = exp.pins().await.expect("expansion");
        assert_eq!(set.groups.len(), ARTIST_ALBUM_CAP, "capped");
        assert_eq!(
            set.groups[0].id, "al099",
            "and the survivors are the NEWEST, not the first the server listed"
        );
        assert_eq!(
            cat.calls().iter().filter(|c| c.starts_with("album:")).count(),
            ARTIST_ALBUM_CAP,
            "the cap is a brake on ROUND TRIPS - the truncated albums are never fetched"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_album_memo_expires_yields_to_a_song_count_change_and_drops_what_left() {
        // The memo is what makes asking for the pin set on every pass affordable (36
        // `getAlbum` calls at ~0.6 s each, otherwise). Its three escapes, each of which
        // would ship green if it silently stopped working: the TTL, the `song_count`
        // disagreement that catches a track ADDED before the TTL is up, and the drop of
        // any album that left the starred set so the map cannot grow for the life of
        // the process.
        let cat = FakeCatalog::new();
        let tracks = vec![song("t1", 10, "flac", None)];
        cat.set_album("a1", Reply::Songs(tracks.clone()));
        let exp = PinExpansion::new(cat.clone(), TokioClockForTest);

        cat.set_starred(Starred { songs: Vec::new(), albums: vec![album("a1", 1, None)], artists: Vec::new() });
        exp.pins().await.expect("first");
        assert_eq!(cat.album_calls("a1"), 1);

        // Past the pin-set memo but inside the album memo: no second getAlbum.
        tokio::time::advance(PIN_SET_MEMO_TTL + Duration::from_secs(1)).await;
        cat.set_starred(Starred { songs: Vec::new(), albums: vec![album("a1", 1, None)], artists: Vec::new() });
        exp.pins().await.expect("memoised");
        assert_eq!(cat.album_calls("a1"), 1, "the album memo served it");

        // A song_count disagreement forces a refetch BEFORE the TTL: a track added to a
        // starred album would otherwise be invisible for six hours.
        tokio::time::advance(PIN_SET_MEMO_TTL + Duration::from_secs(1)).await;
        cat.set_starred(Starred { songs: Vec::new(), albums: vec![album("a1", 2, None)], artists: Vec::new() });
        exp.pins().await.expect("count changed");
        assert_eq!(cat.album_calls("a1"), 2, "a song_count disagreement outranks the TTL");

        // And the TTL itself.
        tokio::time::advance(ALBUM_MEMO_TTL + Duration::from_secs(1)).await;
        cat.set_starred(Starred { songs: Vec::new(), albums: vec![album("a1", 2, None)], artists: Vec::new() });
        exp.pins().await.expect("expired");
        assert_eq!(cat.album_calls("a1"), 3, "and the TTL expires it");

        // Unstarred: the memo entry is dropped, so a re-star pays a fresh fetch rather
        // than serving a six-hour-old list.
        tokio::time::advance(PIN_SET_MEMO_TTL + Duration::from_secs(1)).await;
        cat.set_starred(Starred { songs: Vec::new(), albums: Vec::new(), artists: Vec::new() });
        exp.pins().await.expect("unstarred");
        assert_eq!(exp.album_memo.lock().unwrap().len(), 0, "an album nobody wants cannot be consulted again");
    }

    #[tokio::test(start_paused = true)]
    async fn the_pin_set_memo_collapses_the_chain_and_a_star_invalidates_it() {
        // A cold backfill re-enters the same pass after every drained batch of four -
        // ~73 chained full passes for 290 tracks - and a naive expansion is 43 round
        // trips each time. The memo collapses the chain; `invalidate` (called only on a
        // KICKED full pass, i.e. a star or unstar) is what keeps the GESTURE path exact.
        let cat = FakeCatalog::new();
        let exp = PinExpansion::new(cat.clone(), TokioClockForTest);
        cat.set_starred(Starred { songs: vec![song("s1", 10, "flac", None)], albums: Vec::new(), artists: Vec::new() });
        exp.pins().await.expect("first");
        exp.pins().await.expect("memoised");
        assert_eq!(cat.calls(), vec!["starred".to_string()], "one getStarred2 for the chain");

        // A star or unstar: the memoised set must go at once, not in 30 seconds.
        cat.set_starred(Starred {
            songs: vec![song("s1", 10, "flac", None), song("s2", 10, "flac", None)],
            albums: Vec::new(),
            artists: Vec::new(),
        });
        exp.invalidate();
        let set = exp.pins().await.expect("after the gesture");
        assert_eq!(set.groups.len(), 2, "the gesture path is exact, never memo-stale");

        // And the TTL expires it on its own.
        cat.set_starred(Starred { songs: Vec::new(), albums: Vec::new(), artists: Vec::new() });
        tokio::time::advance(PIN_SET_MEMO_TTL + Duration::from_secs(1)).await;
        assert_eq!(exp.pins().await.expect("expired").groups.len(), 0);
    }
}
