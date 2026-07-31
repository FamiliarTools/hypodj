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

use crate::clock::Clock;
use crate::config::StoreConfig;
use crate::model::{Song, SongId};
use crate::resume::atomic_write_bytes;
use crate::subsonic::SubsonicClient;

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
    /// The pinned mirror alone exceeds the budget. Warn naming the shortfall and
    /// halt new pin downloads at the cap: never silently evict a pin, never exceed
    /// the budget, no download-evict thrash.
    WarnPinOverflow { pinned_bytes: u64, max_bytes: u64 },
    /// Fetch this song's original and commit it.
    Download { id: SongId, reason: DownloadReason },
    /// Persist a resolve-time recency bump, so LRU eviction does not degenerate
    /// into FIFO-by-download-date across a restart.
    FlushRecency { id: SongId, last_played_unix: u64 },
    /// Reclaim an unpinned, unprotected entry, oldest `last_played` first. Never an
    /// id the same pass is downloading - see [`plan_pass`] on download-evict thrash.
    Evict(SongId),
}

/// Everything one pass gets to look at. A plain data struct so [`plan_pass`] is a
/// PURE function: no clock, no filesystem, no network, table-testable.
#[derive(Clone, Debug)]
pub struct PassInput {
    pub mode: PassMode,
    /// The pin set from THIS pass's `getStarred2`, newest-starred-first.
    ///
    /// `None` means NO AUTHORITATIVE PIN SET this pass - either a light pass (which
    /// never calls the server) or a full pass whose `getStarred2` failed. Every pin
    /// verdict is then skipped: nothing is deleted, demoted, or marked stale
    /// because the server flapped. Transient-keeps-the-claim IS offline mode.
    pub pins: Option<Vec<Song>>,
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
    pub max_bytes: u64,
    /// Cap on downloads this pass ([`DOWNLOAD_BATCH`] in production).
    pub download_batch: usize,
    /// True while the current track is a remote stream or a remotely resolved song:
    /// bulk work (stale replacements and backfill) waits so initial sync cannot
    /// stall live playback on a thin link. Window and suspect work never waits.
    pub defer_bulk: bool,
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
            download_batch: DOWNLOAD_BATCH,
            defer_bulk: false,
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

/// Plan one reconcile pass. PURE: same input, same output, no side effects.
///
/// The returned actions are in EXECUTION ORDER, and that order is load-bearing:
///
/// 1. sweep temps, then loose orphans, then invalid entries - so the byte
///    accounting below is against what will actually be on disk;
/// 2. fingerprint verdicts (stale / pin marks) - marks only, never deletes;
/// 3. the pin-overflow warning, if the pinned mirror alone will not fit;
/// 4. downloads, in [`DownloadReason`] priority order;
/// 5. recency flushes;
/// 6. evictions, LAST - and never of an id step 4 admitted, so no plan ever tells
///    the executor to download bytes and then delete them.
///
/// Because evictions come last, download admission is budgeted against the bytes
/// on disk RIGHT NOW, not against a post-eviction projection. The store therefore
/// never transiently exceeds `max_bytes`: space a pass reclaims becomes the NEXT
/// pass's headroom, and the reconciler re-enters immediately while work remains, so
/// the cost is one extra pass, not a stall.
pub fn plan_pass(input: &PassInput) -> Vec<StoreAction> {
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
    let mut pin_ids: HashSet<&str> = HashSet::new();
    // Ids whose fingerprint drifted in THIS pass's verdicts. Collected as the
    // verdicts are formed rather than recomputed afterwards, so the pass stays
    // linear in (entries + pins) instead of quadratic.
    let mut drifted: HashSet<&str> = HashSet::new();
    // Entries needing a replacement download: freshly drifted this pass, or still
    // carrying a stale mark from an earlier one.
    let mut needs_replacement: Vec<&IndexEntry> = Vec::new();
    if verdicts {
        let pins = input.pins.as_ref().expect("verdicts implies pins");
        for p in pins {
            if is_storable_id(&p.id.0) {
                pin_ids.insert(p.id.0.as_str());
            }
        }
        for p in pins {
            let Some(e) = by_id.get(p.id.0.as_str()) else {
                continue;
            };
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
        for e in &input.entries {
            if e.pinned && !pin_ids.contains(e.id.0.as_str()) {
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

    // Whether an entry is pinned AFTER this pass's verdicts. With no authoritative
    // pin set the sidecar's own flag stands.
    let pinned_now = |e: &IndexEntry| -> bool {
        if verdicts {
            pin_ids.contains(e.id.0.as_str())
        } else {
            e.pinned
        }
    };

    let total_bytes = input
        .entries
        .iter()
        .fold(0u64, |acc, e| acc.saturating_add(e.size));
    let pinned_bytes = input
        .entries
        .iter()
        .filter(|e| pinned_now(e))
        .fold(0u64, |acc, e| acc.saturating_add(e.size));

    // ── 3. Pin overflow. Reported before the downloads so the log reads in the
    // order the decisions were made.
    let pin_overflow = pinned_bytes > input.max_bytes;
    if full && pin_overflow {
        out.push(StoreAction::WarnPinOverflow {
            pinned_bytes,
            max_bytes: input.max_bytes,
        });
    }

    // ── 4. Downloads, in priority order, deduped by id keeping the highest
    // priority reason.
    //
    // Budget admission applies to the BULK categories only (`Stale`, `Backfill`) -
    // which is exactly where "halt pin downloads at the cap" lives. `Suspect` and
    // `Window` are NOT budget-gated: they are bounded in count (the suspect set;
    // queue_ahead + 1), they are precisely what the user is about to hear, and
    // refusing them because the store is full of pins would defeat the feature.
    // Their bytes are reclaimed by the next pass's eviction like any others.
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
    let batch = input.download_batch;

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
    // (c) and (d) are BULK work: full passes only (a light kick executes only what
    // the user is about to hear), and deferred while playback is remote.
    if full && !input.defer_bulk {
        // (c) Stale replacements. A same-suffix replacement renames OVER the old
        // file, so it grows the store only by the size difference - which is why
        // the pin-overflow halt does not apply to it: keeping the mirror correct
        // costs (almost) nothing.
        for e in &needs_replacement {
            let grow = input
                .pins
                .as_ref()
                .and_then(|pins| pins.iter().find(|p| p.id == e.id))
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
        // (d) Starred backfill, newest-starred-first (the `getStarred2` order).
        // Halted entirely while the pinned mirror alone overflows the budget.
        if !pin_overflow {
            if let Some(pins) = &input.pins {
                for p in pins {
                    if by_id.contains_key(p.id.0.as_str()) {
                        continue;
                    }
                    // An unknown size cannot be budgeted; admit it and let the
                    // commit's exact-length gate be the authority. A pin the
                    // server will not size is rare and bounded by the pin set.
                    let grow = p.size.unwrap_or(0);
                    if grow > headroom {
                        continue;
                    }
                    if push(&mut downloads, &mut seen, batch, &p.id, DownloadReason::Backfill) {
                        headroom = headroom.saturating_sub(grow);
                    }
                }
            }
        }
    }
    out.append(&mut downloads);

    // ── 5. Recency flushes, full passes only: a light kick fires at every track
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

    // ── 6. Eviction, last. Protected: pins, the queue window (current plus
    // `queue_ahead`), whatever the handler pinned explicitly (the pending-skip
    // target), and any id THIS pass is downloading. Everything else is fair game,
    // oldest `last_played` first - real LRU, thanks to the resolve-time recency bump.
    //
    // The download exclusion is what forbids DOWNLOAD-EVICT THRASH. Suspect and
    // stale replacements are scheduled for entries that already exist on disk, and
    // neither reason is protected by pinning or the window - so without it an
    // over-budget pass could emit `Download` and `Evict` for the same id and the
    // executor, walking the list front to back, would fetch a whole original and
    // then unlink it, leaving a de-offered song permanently gone. The exclusion only
    // DELAYS such an eviction: the commit clears `stale` and `suspect`, so the next
    // pass sees an ordinary entry and reclaims it if it is still the coldest. It is
    // also bounded - at most `download_batch` ids are excluded - so even a
    // replacement that keeps failing holds back a few entries' worth of bytes, not
    // the budget, and evicting them instead would delete bytes that are still
    // serving (stale) or still wanted (suspect) to buy nothing back.
    if full && total_bytes > input.max_bytes {
        let mut protected: HashSet<&str> =
            input.protected.iter().map(|i| i.0.as_str()).collect();
        for id in &input.window {
            protected.insert(id.0.as_str());
        }
        let mut victims: Vec<&IndexEntry> = input
            .entries
            .iter()
            .filter(|e| {
                !pinned_now(e)
                    && !protected.contains(e.id.0.as_str())
                    && !seen.contains(e.id.0.as_str())
            })
            .collect();
        // The id is the tie-break, so two passes over identical state pick
        // identical victims - a plan that reshuffles under ties is untestable.
        victims.sort_by(|a, b| {
            a.last_played_unix
                .cmp(&b.last_played_unix)
                .then_with(|| a.id.0.cmp(&b.id.0))
        });
        let mut remaining = total_bytes;
        for v in victims {
            if remaining <= input.max_bytes {
                break;
            }
            out.push(StoreAction::Evict(v.id.clone()));
            remaining = remaining.saturating_sub(v.size);
        }
    }

    out
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

    /// Drop `id` entirely: out of the index first (so playback stops being offered
    /// it), then the SIDECAR (which de-commits the entry), then the audio.
    ///
    /// That order is deliberate - an interrupted delete leaves an orphan the next
    /// scan sweeps, never a sidecar pointing at bytes that are gone.
    ///
    /// Returns whether the FILES actually went (see [`remove_pair`]). `false` means
    /// the bytes are still on disk and the next full scan will re-adopt them, so a
    /// caller counting reclaimed space must not treat it as progress. Removing an
    /// id the store never had returns `true`: nothing is there, which is the
    /// requested state.
    pub fn remove_entry(&self, id: &SongId) -> bool {
        let suffix = {
            let mut index = self.index.lock().expect("store index lock");
            index.remove(id).map(|e| e.suffix)
        };
        remove_pair(&self.root, id, suffix.as_deref())
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
    let mut gone = true;
    let sidecar = root.join(format!("{}.toml", id.0));
    if let Err(e) = std::fs::remove_file(&sidecar) {
        if e.kind() != io::ErrorKind::NotFound {
            gone = false;
            tracing::warn!(path = %sidecar.display(), error = %e, "store: removing sidecar failed");
        }
    }
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
    /// The authoritative pin set, newest-starred-first.
    ///
    /// An `Err` is TRANSIENT BY POLICY: the pass then skips ALL verdicts (nothing
    /// deleted, demoted, or marked stale because the server flapped), which is what
    /// "transient keeps the claim" means and is the whole of offline mode.
    fn pins(&self) -> impl Future<Output = Result<Vec<Song>, String>> + Send;

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
pub struct SubsonicPinSource {
    client: Arc<SubsonicClient>,
    http: reqwest::Client,
    /// Mirrors `store.pin_starred`. When false the pin set is authoritatively
    /// EMPTY (not unknown): entries demote to evictable and only the queue window
    /// is mirrored, which is exactly what the knob promises.
    pin_starred: bool,
}

impl SubsonicPinSource {
    pub fn new(client: Arc<SubsonicClient>, pin_starred: bool) -> Self {
        Self {
            client,
            http: build_download_http_client(),
            pin_starred,
        }
    }
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

impl PinSource for SubsonicPinSource {
    async fn pins(&self) -> Result<Vec<Song>, String> {
        if !self.pin_starred {
            return Ok(Vec::new());
        }
        self.client.starred_songs().await.map_err(|e| e.to_string())
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

impl Backoff {
    /// Whether `id` may be attempted now.
    fn ready(&self, id: &SongId, now: tokio::time::Instant) -> bool {
        match self.entries.get(id) {
            Some((_, not_before)) => now >= *not_before,
            None => true,
        }
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
    /// Downloads that committed.
    committed: usize,
    /// Entries whose bytes were ACTUALLY reclaimed - an eviction whose unlink
    /// failed is not counted, because it freed nothing and the next scan will find
    /// the very same entry over budget again.
    evicted: usize,
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
                        if store.take_full_request() { PassMode::Full } else { PassMode::Light }
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
    let mut input = PassInput::new(mode, store.config().max_bytes);
    input.download_batch = batch;
    input.defer_bulk = store.playback_remote();

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

    let actions = plan_pass(&input);
    execute(store, source, &input, actions, clock, backoff).await
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
        Some(pins) => pins.iter().map(|p| (p.id.0.as_str(), p)).collect(),
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
            StoreAction::WarnPinOverflow { pinned_bytes, max_bytes } => {
                tracing::warn!(
                    pinned_bytes,
                    max_bytes,
                    shortfall = pinned_bytes.saturating_sub(max_bytes),
                    "store: the pinned set alone exceeds store.max_bytes; halting pin downloads at the cap (nothing pinned is ever silently evicted)"
                );
            }
            StoreAction::Download { id, reason } => {
                report.scheduled += 1;
                if !backoff.ready(&id, clock.now()) {
                    tracing::debug!(id = %id.0, ?reason, "store: download still backing off");
                    continue;
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
                        tracing::info!(id = %id.0, ?reason, path = %path.display(), "store: committed");
                    }
                    Err(e) => {
                        backoff.fail(&id, clock.now());
                        tracing::warn!(id = %id.0, ?reason, error = %e, "store: download failed; will retry");
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
        input.pins = Some(vec![
            song("newest", 10, "flac", None),
            song("middle", 10, "flac", None),
            song("oldest", 10, "flac", None),
        ]);
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
            input.pins = Some(vec![server]);
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
        input.pins = Some(vec![song("a", 100, "flac", Some("2024-05-01T12:00:00Z"))]);
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
        input.pins = Some(vec![song("a", 100, "flac", Some("2024-05-01T14:00:00+02:00"))]);
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
                input.pins = Some(vec![server.clone()]);
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
        input.pins = Some(vec![song("now-starred", 100, "flac", Some("2024-05-01T12:00:00Z"))]);
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
        input.pins = Some(vec![
            song("backfill-want", 100, "flac", None),
            song("suspect", 100, "flac", Some("2024-05-01T12:00:00Z")),
        ]);
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
        input.pins = Some(vec![song("unseen-pin", 100, "flac", None)]);
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
        input.pins = Some(vec![song("backfill", 100, "flac", None)]);
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
        input.pins = Some(vec![song("everything", 999, "flac", None)]);
        assert_eq!(
            dls(&plan_pass(&input)),
            vec![("everything".to_string(), DownloadReason::Suspect)]
        );
        // And a window id that is also an uncached pin is Window, not Backfill.
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.window = vec![sid("both")];
        input.pins = Some(vec![song("both", 100, "flac", None)]);
        assert_eq!(dls(&plan_pass(&input)), vec![("both".to_string(), DownloadReason::Window)]);
    }

    #[test]
    fn plan_pass_never_plans_work_for_an_unstorable_id() {
        // An id that cannot be a path component is excluded from the store entirely;
        // resolution falls through to streaming.
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.window = vec![SongId("../evil".into()), SongId("".into()), sid("ok")];
        input.pins = Some(vec![song("a/b", 100, "flac", None)]);
        assert_eq!(dls(&plan_pass(&input)), vec![("ok".to_string(), DownloadReason::Window)]);
    }

    #[test]
    fn plan_pass_batch_bound_caps_a_huge_backlog() {
        // A cold mirror must drain incrementally rather than pinning the task or
        // saturating the link in one burst.
        let mut input = PassInput::new(PassMode::Full, 1 << 30);
        input.pins = Some((0..50).map(|i| song(&format!("p{i:02}"), 10, "flac", None)).collect());
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
    fn plan_pass_eviction_never_touches_pins_the_window_or_the_skip_target() {
        // Every protected class, all with the OLDEST possible recency so plain LRU
        // would pick them first.
        let mut pinned = entry("pinned", 100, 0);
        pinned.pinned = true;
        let mut input = PassInput::new(PassMode::Full, 100);
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
        input.pins = Some(vec![song("pinned", 100, "flac", Some("2024-05-01T12:00:00Z"))]);
        let plan = plan_pass(&input);
        assert_eq!(
            evictions(&plan),
            vec!["evictable".to_string()],
            "only the unprotected entry is reclaimable, despite being the NEWEST"
        );
        // 500 bytes against a 100 cap with only one victim available: the plan
        // reclaims what it may and STOPS, still over budget, rather than breaking a
        // protection promise to balance the books.
        assert!(
            !plan.iter().any(|a| matches!(a, StoreAction::WarnPinOverflow { .. })),
            "pinned bytes (100) do not exceed the cap (100); only protection does"
        );
        // With the pin alone over the cap, the overflow IS reported - and still no
        // pin is evicted.
        input.max_bytes = 50;
        let plan = plan_pass(&input);
        assert!(plan.contains(&StoreAction::WarnPinOverflow { pinned_bytes: 100, max_bytes: 50 }));
        assert_eq!(evictions(&plan), vec!["evictable".to_string()]);
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
        input.pins = Some(vec![song("still-starred", 100, "flac", Some("2024-05-01T12:00:00Z"))]);
        let plan = plan_pass(&input);
        assert!(plan.contains(&StoreAction::SetPinned { id: sid("was-starred"), pinned: false }));
        assert_eq!(evictions(&plan), vec!["was-starred".to_string()]);
        // Without an authoritative pin set the sidecar's own flag stands, so nothing
        // is reclaimable and nothing is broken.
        input.pins = None;
        assert_eq!(evictions(&plan_pass(&input)), Vec::<String>::new());
    }

    #[test]
    fn plan_pass_pins_exceeding_the_budget_warn_and_halt_pin_downloads() {
        let mut a = entry("pin-a", 100, 0);
        a.pinned = true;
        let mut b = entry("pin-b", 100, 0);
        b.pinned = true;
        let mut input = PassInput::new(PassMode::Full, 150);
        input.entries = vec![a, b];
        input.pins = Some(vec![
            song("pin-a", 100, "flac", Some("2024-05-01T12:00:00Z")),
            song("pin-b", 100, "flac", Some("2024-05-01T12:00:00Z")),
            song("pin-c", 100, "flac", None),
        ]);
        input.window = vec![sid("must-have")];
        let plan = plan_pass(&input);
        // Warned once, naming the shortfall.
        assert_eq!(
            plan.iter()
                .filter(|a| matches!(a, StoreAction::WarnPinOverflow { .. }))
                .collect::<Vec<_>>(),
            vec![&StoreAction::WarnPinOverflow { pinned_bytes: 200, max_bytes: 150 }]
        );
        // No new pin download, and NO pin evicted to make room - the promise holds
        // and the budget holds; the operator is told to pick one.
        assert_eq!(
            dls(&plan),
            vec![("must-have".to_string(), DownloadReason::Window)],
            "backfill halts at the cap, but the queue window still gets served"
        );
        assert_eq!(evictions(&plan), Vec::<String>::new(), "a pin is never silently evicted");
    }

    #[test]
    fn plan_pass_budgets_bulk_downloads_against_the_bytes_on_disk_right_now() {
        // Evictions are emitted LAST, so a download admitted this pass must fit in
        // the space that exists BEFORE they run - otherwise the store would
        // transiently exceed max_bytes.
        let mut input = PassInput::new(PassMode::Full, 250);
        input.entries = vec![entry("old", 200, 1)];
        input.pins = Some(vec![song("big", 100, "flac", None), song("small", 40, "flac", None)]);
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
        let mut suspect = entry("suspect", 100, 0);
        suspect.suspect = true;
        suspect.pinned = true;
        let mut pin = entry("pin", 400, 0);
        pin.pinned = true;
        let mut input = PassInput::new(PassMode::Full, 200);
        input.entries = vec![suspect, pin];
        input.window = vec![sid("about-to-play")];
        input.pins = Some(vec![
            song("suspect", 100, "flac", Some("2024-05-01T12:00:00Z")),
            song("pin", 400, "flac", Some("2024-05-01T12:00:00Z")),
            song("nice-to-have", 10, "flac", None),
        ]);
        let plan = plan_pass(&input);
        assert_eq!(
            dls(&plan),
            vec![
                ("suspect".to_string(), DownloadReason::Suspect),
                ("about-to-play".to_string(), DownloadReason::Window),
            ],
            "the audible work happens; the nice-to-have backfill is halted"
        );
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
        input.pins = Some(vec![song("newpin", 10, "flac", None)]);
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
        // A saturating total also cannot manufacture download headroom.
        input.pins = Some(vec![song("want", 10, "flac", None)]);
        assert_eq!(dls(&plan_pass(&input)), Vec::new());
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
        input.pins = Some(vec![dir_song("keep", 32)]);
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
        settled.pins = Some(vec![dir_song("keep", 32)]);
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
        fetches: Vec<String>,
    }

    struct FakeInner {
        log: Mutex<SourceLog>,
        /// `None` scripts a TRANSIENT pin-set failure.
        pins: Mutex<Option<Vec<Song>>>,
        /// Everything `song(id)` can resolve, for ids outside the pin set.
        catalog: Mutex<HashMap<String, Song>>,
        /// When false every fetch fails, which is how the backoff is exercised.
        fetch_ok: AtomicBool,
    }

    /// A scripted [`PinSource`]: no server, no sockets. `fetch` writes exactly
    /// `song.size` bytes so a successful commit is byte-for-byte what the real
    /// exact-length gate would accept.
    #[derive(Clone)]
    struct FakeSource(Arc<FakeInner>);

    impl FakeSource {
        fn new(pins: Option<Vec<Song>>) -> Self {
            let catalog = pins
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|s| (s.id.0.clone(), s))
                .collect();
            Self(Arc::new(FakeInner {
                log: Mutex::new(SourceLog::default()),
                pins: Mutex::new(pins),
                catalog: Mutex::new(catalog),
                fetch_ok: AtomicBool::new(true),
            }))
        }

        fn set_pins(&self, pins: Option<Vec<Song>>) {
            *self.0.pins.lock().unwrap() = pins;
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
        async fn pins(&self) -> Result<Vec<Song>, String> {
            // Scoped so no std Mutex is ever held across an await.
            let scripted = {
                self.0.log.lock().unwrap().pins_calls += 1;
                self.0.pins.lock().unwrap().clone()
            };
            scripted.ok_or_else(|| "scripted transient failure".to_string())
        }

        async fn song(&self, id: &SongId) -> Result<Song, String> {
            let found = {
                self.0.log.lock().unwrap().song_calls += 1;
                self.0.catalog.lock().unwrap().get(&id.0).cloned()
            };
            found.ok_or_else(|| format!("no such song: {}", id.0))
        }

        async fn fetch(&self, song: &Song, tmp: &Path) -> Result<u64, String> {
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

    /// Like [`settle`] but stops as soon as `done` holds, and FAILS LOUDLY if it
    /// never does - so a broken loop is a clear failure rather than a later
    /// confusing assertion.
    async fn settle_until(tag: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..2000 {
            if done() {
                return;
            }
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_micros(200));
        }
        panic!("the reconciler never reached: {tag}");
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
        assert_eq!(report, PassReport::default(), "a light pass with no window does nothing");
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
        // The ceiling holds however many times it fails, and never overflows.
        for _ in 0..64 {
            b.fail(&id, t0);
        }
        assert!(b.ready(&id, t0 + DOWNLOAD_BACKOFF_MAX));
        b.succeed(&id);
        assert!(b.ready(&id, t0), "a success forgets the history entirely");
    }

    #[test]
    fn re_enter_requires_progress_not_merely_outstanding_work() {
        // A full batch that landed something: keep draining.
        assert!(PassReport { scheduled: 4, committed: 1, evicted: 0 }.re_enter(4, 0));
        // A full batch that landed NOTHING must sleep, or a permanently failing
        // download would spin the reconciler at full speed forever.
        assert!(!PassReport { scheduled: 4, committed: 0, evicted: 0 }.re_enter(4, 0));
        // A partial batch is all there was: nothing more to drain.
        assert!(!PassReport { scheduled: 2, committed: 2, evicted: 0 }.re_enter(4, 0));
        // An eviction re-enters so the reclaimed headroom is usable now.
        assert!(PassReport { scheduled: 0, committed: 0, evicted: 1 }.re_enter(4, 0));
    }

    #[test]
    fn eviction_only_re_entry_is_capped_but_a_draining_download_chain_is_not() {
        // The backstop behind the progress gate. An eviction chain is bounded: a
        // filesystem that reports reclamation it did not perform gets at most
        // MAX_EVICTION_CHAIN passes, not an endless run of directory scans and
        // getStarred2 round trips.
        let evicting = PassReport { scheduled: 0, committed: 0, evicted: 1 };
        assert!(evicting.re_enter(4, MAX_EVICTION_CHAIN - 1), "the last link is allowed");
        assert!(!evicting.re_enter(4, MAX_EVICTION_CHAIN), "and then the loop must wait");
        assert!(!evicting.re_enter(4, MAX_EVICTION_CHAIN + 9));

        // A chain of COMMITTED downloads is not capped: every link cost a real
        // original, the desired set is finite, and capping it would stall a cold
        // backfill for a whole interval.
        let draining = PassReport { scheduled: 4, committed: 4, evicted: 0 };
        assert!(draining.re_enter(4, MAX_EVICTION_CHAIN * 100));
        assert!(draining.drained_a_full_batch(4), "which is what resets the chain");
        assert!(!evicting.drained_a_full_batch(4));
        // A pass that both drained a batch and evicted still counts as draining.
        let both = PassReport { scheduled: 4, committed: 1, evicted: 2 };
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
        assert!(
            calls <= 2,
            "a store that cannot reclaim must fall back to the interval; instead {calls} full passes (each a scan plus a getStarred2) ran back to back"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The production [`TokioClock`], re-exported under a local name so the loop
    /// tests read as "the same clock production uses, merely paused".
    use crate::clock::TokioClock as TokioClockForTest;
}
