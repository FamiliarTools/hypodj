//! On-demand now-playing RECOGNITION for raw streams (task f7vnd3i).
//!
//! Sibling jmrwr99 surfaces a stream's ICY `icy-name`/`icy-title` into MPD
//! `Name`/`Title`. But some real radio streams (the NTS mixtapes) carry NO ICY at
//! all, so the now-playing text must come from OUTSIDE the stream. This module
//! fingerprints a short SIDE-BAND capture of the SAME stream URL with `songrec`
//! (open-source Shazam) and returns the recognized artist / title / album / cover
//! art, station-agnostic and with ZERO interference to the playing libmpv
//! instance.
//!
//! Two honest subprocess steps, both async (`tokio::process`, so the child I/O
//! never blocks the reactor and a timeout can actually KILL the child):
//! 1. `ffmpeg` captures ~11s of the stream URL to a temp mono 16 kHz wav.
//! 2. `songrec recognize --json <wav>` fingerprints the wav, queries Shazam, and
//!    prints ONE line of JSON to stdout, with its OUTCOME on stderr. Both are
//!    captured and classified by [`classify_songrec`], which is what splits a content
//!    miss ("No match for this song") from a transport failure ("Network unreachable")
//!    AT THE SOURCE, so nothing downstream has to infer it from timing.
//!
//! Both tools are put on `PATH` by the nix wrapper (see `nix/package.nix`), so the
//! feature is self-contained. The temp wav is removed in EVERY branch (RAII guard),
//! and every child is `kill_on_drop` so a timeout leaves no orphan process.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Total wall-clock ceiling for one capture + recognition. Three nested guards keep
/// a hung endpoint from ever WEDGING the `identify` trigger: `ffmpeg -t 11`
/// self-terminates a healthy capture, the `ffmpeg -rw_timeout` (see
/// [`FFMPEG_RW_TIMEOUT_US`]) self-aborts a STALLED stream read well before this
/// bound, and this outer `tokio::time::timeout` is the last resort. On elapse the
/// in-flight child future is dropped, which `kill_on_drop` turns into a real
/// SIGKILL of the `ffmpeg`/`songrec` child (no orphan survives), and the temp wav is
/// cleaned by the RAII guard - only THEN does the async call return and release the
/// caller's in-flight guard, so a later `identify` still runs on a clean slate.
const RECOGNIZE_TIMEOUT: Duration = Duration::from_secs(40);

/// Per-operation I/O ceiling handed to `ffmpeg` as `-rw_timeout` (microseconds): a
/// stream whose socket read/connect stalls for this long self-aborts the capture,
/// so the common "endpoint went silent" case never has to wait for the outer
/// [`RECOGNIZE_TIMEOUT`]. Well under that bound (15s vs 40s) and comfortably above
/// the ~11s a healthy realtime capture takes.
const FFMPEG_RW_TIMEOUT_US: &str = "15000000";

/// A monotonic per-process counter mixed into the temp-file name alongside the pid,
/// so two captures can never collide on the same path (the in-flight guard already
/// serializes them within a process, but the counter is a cheap second belt).
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The fields a recognized track carries into the now-playing surface. Every field
/// is `Option` so a partial Shazam hit (title but no album, say) is honest rather
/// than fabricated. Produced by [`parse_recognize_json`] from the `songrec` output.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecognizedTrack {
    /// The performing artist (`track.subtitle` in the Shazam JSON).
    pub artist: Option<String>,
    /// The track title (`track.title`).
    pub title: Option<String>,
    /// The album name, read from the `SONG` section's `Album` metadata row.
    pub album: Option<String>,
    /// The release date, read from the `SONG` section's `Released` row. Shazam emits
    /// either a bare year ("1999") or a full date; passed through verbatim because
    /// MPD's `Date` tag accepts both.
    pub released: Option<String>,
    /// The record label, read from the `SONG` section's `Label` row.
    pub label: Option<String>,
    /// The primary genre (`track.genres.primary`).
    pub genre: Option<String>,
    /// The recording's ISRC (`track.isrc`), the stable identifier that lets a caller
    /// match the hit back to a library entry.
    pub isrc: Option<String>,
    /// The Shazam/Apple cover-art HTTPS URL (prefers the HQ variant). A remote URL,
    /// not local bytes; surfaced toward the dj-gui art pane as an extension field.
    pub cover_url: Option<String>,
}

/// Why a recognition attempt failed at the SUBPROCESS layer (as opposed to a clean
/// no-match, which is `Ok(None)`). Kept distinct so the handler can ACK an honest
/// error versus a plain "no match".
///
/// THE SPLIT THAT MATTERS: a CONTENT miss (`Ok(None)` - Shazam simply does not know
/// this music) and a TRANSPORT failure (this variant - the network, the endpoint, or a
/// stalled capture) are different facts about the world and must back off differently.
/// Before the stderr taxonomy below existed they were indistinguishable, so both decayed
/// on the same exponential and an all-miss mixtape evening produced 40 minutes of
/// deafness. Every variant here is a TRANSPORT failure; nothing content-shaped reaches
/// it. CORE-INTERNAL: `RecognizeError` appears nowhere outside this module (the handler
/// consumes it via `Display`), so adding a variant is not a cross-crate break.
#[derive(Debug)]
pub enum RecognizeError {
    /// A tool could not be spawned or exec'd (e.g. missing from `PATH`). Carries the
    /// tool name and the underlying io error.
    Spawn(&'static str, std::io::Error),
    /// `ffmpeg` ran but exited non-zero (the stream URL was unreachable / not
    /// capturable, or its `-rw_timeout` fired on a stalled read).
    Capture,
    /// `songrec` ran but could not REACH Shazam, as it said on its own stderr. Carries
    /// whether the message was specifically a rate-limit ("Your IP has been
    /// rate-limited"), which songrec 0.7.4 prints with no retry and whose limiter is
    /// IP-keyed - so it is worth naming in a log even though the backoff treats it as
    /// any other transport failure.
    Transport { rate_limited: bool },
    /// `songrec` produced something this module cannot classify (an unparseable stdout
    /// with a silent stderr, an unrecognized stderr message). Counted as a transport
    /// failure for backoff, because an UNKNOWN outcome must not be optimistically
    /// treated as "Shazam does not know this music".
    Unknown,
    /// The whole capture+recognition exceeded [`RECOGNIZE_TIMEOUT`]; the child was
    /// killed on the way out. Note that a timeout with an EMPTY stderr is the ONLY
    /// remaining 429 SUSPICION (songrec prints nothing and hangs on a genuine 429), and
    /// it is logged as suspicion, never as certainty - there is no timing inference
    /// anywhere in this module.
    Timeout,
}

impl RecognizeError {
    /// A short, stable word for the ledger `outcome` field.
    pub fn outcome_word(&self) -> &'static str {
        match self {
            RecognizeError::Transport { rate_limited: true } => "rate_limited",
            RecognizeError::Timeout => "timeout",
            _ => "transport",
        }
    }
}

impl std::fmt::Display for RecognizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecognizeError::Spawn(tool, e) => write!(f, "could not run {tool}: {e}"),
            RecognizeError::Capture => write!(f, "stream capture failed"),
            RecognizeError::Transport { rate_limited: true } => {
                write!(f, "shazam rate-limited this address")
            }
            RecognizeError::Transport { rate_limited: false } => {
                write!(f, "could not reach shazam")
            }
            RecognizeError::Unknown => write!(f, "recognition failed for an unknown reason"),
            RecognizeError::Timeout => write!(f, "recognition timed out"),
        }
    }
}

impl std::error::Error for RecognizeError {}

/// What one `songrec recognize --json` run actually said, read from its exit status,
/// its stdout AND its stderr.
///
/// The stderr was previously thrown away (`Stdio::null()`), which collapsed three
/// genuinely different outcomes into one indistinguishable "empty or unparseable
/// stdout". songrec ALREADY prints the taxonomy; capturing it removes every need for
/// timing inference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SongrecOutcome {
    /// A parseable track came back on stdout.
    Hit(String),
    /// A CONTENT miss: songrec reached Shazam and Shazam did not know this audio.
    NoMatch,
    /// A TRANSPORT failure: songrec could not reach Shazam.
    Transport { rate_limited: bool },
    /// Unclassifiable. Treated as transport, never as a content miss.
    Unknown,
}

/// stderr substrings that mean a clean CONTENT miss.
const NO_MATCH_MARKERS: &[&str] = &["no match for this song", "no match found", "no match"];

/// stderr substrings that mean songrec was RATE-LIMITED (songrec 0.7.4's own wording,
/// plus the raw status).
const RATE_LIMIT_MARKERS: &[&str] = &["rate-limited", "rate limited", "429", "too many requests"];

/// stderr substrings that mean a TRANSPORT failure of any other shape.
const TRANSPORT_MARKERS: &[&str] = &[
    "network unreachable",
    "network is unreachable",
    "error sending request",
    "connection refused",
    "connection reset",
    "connection closed",
    "dns error",
    "failed to lookup address",
    "temporary failure in name resolution",
    "operation timed out",
    "timed out",
    "certificate",
    "tls",
    "no route to host",
];

/// Classify one songrec run. PURE, so the whole taxonomy is unit-testable against real
/// songrec strings with no subprocess, no network and no clock.
///
/// STDOUT FIRST: a parseable track is a Hit regardless of any stderr noise. Only then is
/// stderr consulted, no-match before the failure markers, because a clean miss is the
/// common case and must never be mistaken for a network problem (that mistake is what
/// would put a content miss on the full exponential again).
pub fn classify_songrec(status: Option<i32>, stdout: &str, stderr: &str) -> SongrecOutcome {
    let out = stdout.trim();
    if !out.is_empty() && parse_recognize_json(out).is_some() {
        return SongrecOutcome::Hit(out.to_string());
    }
    let err = stderr.to_lowercase();
    if NO_MATCH_MARKERS.iter().any(|m| err.contains(m)) {
        return SongrecOutcome::NoMatch;
    }
    if RATE_LIMIT_MARKERS.iter().any(|m| err.contains(m)) {
        return SongrecOutcome::Transport { rate_limited: true };
    }
    if TRANSPORT_MARKERS.iter().any(|m| err.contains(m)) {
        return SongrecOutcome::Transport { rate_limited: false };
    }
    // The LEGACY clean-no-match shape: exit 0, empty stdout, silent stderr. Kept as a
    // content miss because that is what it has always meant and inflating it into a
    // transport failure would put the gentler backoff on the wrong arm.
    if status == Some(0) && out.is_empty() && err.trim().is_empty() {
        return SongrecOutcome::NoMatch;
    }
    SongrecOutcome::Unknown
}

/// Removes its temp wav on drop, in EVERY branch (ok / err / panic / timeout), so a
/// recognition never leaves litter in the temp dir. Removal is best-effort (`let _`)
/// because a missing file (ffmpeg never wrote it) is not an error worth surfacing.
struct TempFileGuard(PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A unique temp path for one capture: `hypodj-songrec-<pid>-<counter>.wav`, in the
/// system temp dir (mirrors the viz-probe temp pattern in `player.rs`).
fn temp_wav_path() -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("hypodj-songrec-{}-{}.wav", std::process::id(), n))
}

/// The subprocess half: capture the stream with `ffmpeg`, then fingerprint the wav
/// with `songrec recognize --json`, returning songrec's raw stdout. Uses
/// `tokio::process` so the child I/O rides the reactor (never blocks it) and both
/// children carry `kill_on_drop(true)` - so if the awaiting future is dropped (the
/// [`RECOGNIZE_TIMEOUT`] path in [`recognize_stream_url`]) the in-flight child is
/// SIGKILLed rather than orphaned. Every subprocess uses `Stdio::null()` for stdin
/// so it can never block waiting on input.
///
/// songrec's OWN stderr carries the outcome taxonomy - "No match for this song" versus
/// "Network unreachable" - so it is PIPED and classified by [`classify_songrec`]
/// rather than discarded. ffmpeg's stderr stays null (it is decode noise, and a failed
/// capture is already an unambiguous non-zero exit). The exit status is passed to the
/// classifier but is never on its own a hard error.
async fn capture_and_recognize(url: &str, wav: &Path) -> Result<SongrecOutcome, RecognizeError> {
    use std::process::Stdio;
    use tokio::process::Command;

    // 1. SIDE-BAND capture: re-fetch the SAME stream URL to a bounded temp wav. 11s
    // mono 16 kHz is plenty for a Shazam fingerprint and does not touch the playing
    // libmpv instance. `-nostdin` + null stdin so ffmpeg never waits on the tty;
    // `-rw_timeout` self-aborts a stalled read (see FFMPEG_RW_TIMEOUT_US).
    let capture = Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-rw_timeout", FFMPEG_RW_TIMEOUT_US, "-y", "-i"])
        .arg(url)
        .args(["-t", "11", "-ac", "1", "-ar", "16000", "-f", "wav"])
        .arg(wav)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status()
        .await
        .map_err(|e| RecognizeError::Spawn("ffmpeg", e))?;
    if !capture.success() {
        return Err(RecognizeError::Capture);
    }

    // 2. Headless recognition: songrec prints ONE line of JSON on a match, and its
    // OUTCOME on stderr. Both are captured, because the stderr IS the taxonomy that
    // splits a content miss from a transport failure at the source. Null stdin so it
    // never blocks.
    let out = Command::new("songrec")
        .args(["recognize", "--json"])
        .arg(wav)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| RecognizeError::Spawn("songrec", e))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    Ok(classify_songrec(out.status.code(), &stdout, &stderr))
}

/// Recognize the currently-playing audio at `url` via the side-band capture +
/// `songrec` pipeline. `Ok(None)` is a clean NO MATCH (the honest common case for a
/// niche stream); `Ok(Some(_))` is a hit; `Err(_)` is a subprocess/timeout failure.
///
/// ASYNC/LOCK DISCIPLINE: the caller reads the stream URL under the std state lock
/// and DROPS the lock before calling this (no lock is held across the await here).
/// The heavy work is one async subprocess pair bounded by [`RECOGNIZE_TIMEOUT`] so a
/// hung Shazam call cannot wedge the trigger; on elapse the child future is dropped
/// and `kill_on_drop` reaps the child (no orphan). The temp wav is cleaned in every
/// branch by [`TempFileGuard`].
pub async fn recognize_stream_url(url: String) -> Result<Option<RecognizedTrack>, RecognizeError> {
    let wav = temp_wav_path();
    run_bounded(wav.clone(), RECOGNIZE_TIMEOUT, capture_and_recognize(&url, &wav)).await
}

/// Bound `work` (the capture+recognize future) by `timeout`, cleaning `wav` on EVERY
/// exit via [`TempFileGuard`] - including the timeout branch, where dropping `work`
/// also `kill_on_drop`-reaps the in-flight child. Split out from
/// [`recognize_stream_url`] so the timeout + cleanup wiring is unit-testable with a
/// synthetic `work` future (no real hung stream needed).
async fn run_bounded(
    wav: PathBuf,
    timeout: Duration,
    work: impl std::future::Future<Output = Result<SongrecOutcome, RecognizeError>>,
) -> Result<Option<RecognizedTrack>, RecognizeError> {
    // RAII: removes the wav on EVERY exit path below (including the timeout branch,
    // where `work` is dropped - killing its child - but this guard still unlinks it).
    let _guard = TempFileGuard(wav);
    let outcome = match tokio::time::timeout(timeout, work).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(e)) => return Err(e),
        Err(_elapsed) => return Err(RecognizeError::Timeout),
    };
    // The classified outcome maps onto the caller's contract: `Ok(None)` is and stays
    // the CONTENT miss, every failure shape is an `Err` the backoff reads as transport.
    match outcome {
        // A classified Hit already parsed once; the second parse is the same pure call
        // and cannot disagree, so an unparseable value here is impossible by
        // construction and degrades to a content miss rather than a panic.
        SongrecOutcome::Hit(stdout) => Ok(parse_recognize_json(&stdout)),
        SongrecOutcome::NoMatch => Ok(None),
        SongrecOutcome::Transport { rate_limited } => {
            Err(RecognizeError::Transport { rate_limited })
        }
        SongrecOutcome::Unknown => Err(RecognizeError::Unknown),
    }
}

// ── the songrec JSON shape (only the fields the mapper needs) ────────────────

/// The top-level `songrec recognize --json` object. Everything is optional so a
/// reshaped or partial payload degrades to `None` fields rather than a parse error.
#[derive(serde::Deserialize)]
struct RecognizeResponse {
    track: Option<TrackJson>,
}

#[derive(serde::Deserialize)]
struct TrackJson {
    /// The track title.
    title: Option<String>,
    /// The performing artist (Shazam names this `subtitle`).
    subtitle: Option<String>,
    /// Metadata sections; the one whose `type == "SONG"` holds the Album/Label/
    /// Released rows.
    #[serde(default)]
    sections: Vec<SectionJson>,
    /// Cover-art URLs.
    images: Option<ImagesJson>,
    /// Genre block; only `primary` is a plain string worth surfacing.
    genres: Option<GenresJson>,
    /// The recording's ISRC.
    isrc: Option<String>,
}

#[derive(serde::Deserialize)]
struct GenresJson {
    primary: Option<String>,
}

#[derive(serde::Deserialize)]
struct SectionJson {
    #[serde(rename = "type")]
    section_type: Option<String>,
    #[serde(default)]
    metadata: Vec<MetaJson>,
}

#[derive(serde::Deserialize)]
struct MetaJson {
    /// The row label ("Album" / "Label" / "Released").
    title: Option<String>,
    /// The row value.
    text: Option<String>,
}

#[derive(serde::Deserialize)]
struct ImagesJson {
    coverart: Option<String>,
    coverarthq: Option<String>,
}

/// Trim a value, returning `None` for an empty/whitespace-only string so a blank
/// Shazam field never becomes a visible label.
fn non_blank(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.trim().is_empty())
}

/// Parse one line of `songrec recognize --json` stdout into a [`RecognizedTrack`].
///
/// Returns `None` for the two non-hit cases the daemon must treat identically to a
/// no-match: EMPTY/whitespace stdout (songrec's clean no-match, exit 0), and
/// GARBAGE/malformed JSON (never a panic). A hit requires at least a title or an
/// artist; a `track` object with neither is treated as no-match.
pub fn parse_recognize_json(stdout: &str) -> Option<RecognizedTrack> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        // songrec no-match: exit 0, empty stdout, "No match" on stderr.
        return None;
    }
    // Malformed / unexpected JSON degrades to no-match, never an error or panic.
    let resp: RecognizeResponse = serde_json::from_str(trimmed).ok()?;
    let track = resp.track?;

    // The SONG-typed section carries the Album / Label / Released rows as
    // title/text pairs; look each one up by its row label.
    let song_row = |label: &str| {
        track
            .sections
            .iter()
            .filter(|s| s.section_type.as_deref() == Some("SONG"))
            .flat_map(|s| &s.metadata)
            .find(|m| m.title.as_deref() == Some(label))
            .and_then(|m| non_blank(m.text.clone()))
    };
    let album = song_row("Album");
    let label = song_row("Label");
    let released = song_row("Released");

    // Prefer the HQ cover, fall back to the standard one.
    let cover_url = track
        .images
        .and_then(|i| non_blank(i.coverarthq).or_else(|| non_blank(i.coverart)));

    let genre = track.genres.and_then(|g| non_blank(g.primary));
    let isrc = non_blank(track.isrc);

    let title = non_blank(track.title);
    let artist = non_blank(track.subtitle);
    if title.is_none() && artist.is_none() {
        // A track object with no usable text is not a real hit.
        return None;
    }
    Some(RecognizedTrack { artist, title, album, released, label, genre, isrc, cover_url })
}

/// The now-playing `Title` line for a recognized track, mirroring the ICY
/// "Artist - Track" convention so it rides the exact same MPD `Title` surface as a
/// real icy-title (see `apply_stream_meta`). Falls back to whichever half is
/// present; `None` only when neither artist nor title exists.
pub fn now_playing_title(track: &RecognizedTrack) -> Option<String> {
    match (&track.artist, &track.title) {
        (Some(a), Some(t)) => Some(format!("{a} - {t}")),
        (None, Some(t)) => Some(t.clone()),
        (Some(a), None) => Some(a.clone()),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed-down but STRUCTURALLY REAL Shazam payload (the shape verified live
    /// against songrec 0.4.3 during the feasibility investigation): the fields the
    /// mapper reads, in the nesting songrec actually emits.
    const REAL_HIT: &str = r#"{
      "track": {
        "title": "Blessings",
        "subtitle": "Calvin Harris & Clementine Douglas",
        "sections": [
          {
            "type": "SONG",
            "metadata": [
              { "title": "Album", "text": "Blessings" },
              { "title": "Label", "text": "Columbia" },
              { "title": "Released", "text": "2024" }
            ]
          },
          { "type": "LYRICS", "text": ["la la"] }
        ],
        "images": {
          "coverart": "https://is1.example/400x400.jpg",
          "coverarthq": "https://is1.example/hq.jpg"
        },
        "genres": { "primary": "Dance" },
        "isrc": "GBARL2400123",
        "key": "12345",
        "share": { "subject": "Blessings - Calvin Harris & Clementine Douglas" }
      }
    }"#;

    #[test]
    fn parse_recognize_json_extracts_fields() {
        let t = parse_recognize_json(REAL_HIT).expect("a hit");
        assert_eq!(t.title.as_deref(), Some("Blessings"));
        assert_eq!(t.artist.as_deref(), Some("Calvin Harris & Clementine Douglas"));
        assert_eq!(t.album.as_deref(), Some("Blessings"));
        // Prefers the HQ cover URL over the standard one.
        assert_eq!(t.cover_url.as_deref(), Some("https://is1.example/hq.jpg"));
        // The rest of the SONG-section rows and the top-level blocks (task 5578wi6):
        // every field Shazam offers is carried, not just Album.
        assert_eq!(t.label.as_deref(), Some("Columbia"));
        assert_eq!(t.released.as_deref(), Some("2024"));
        assert_eq!(t.genre.as_deref(), Some("Dance"));
        assert_eq!(t.isrc.as_deref(), Some("GBARL2400123"));
    }

    #[test]
    fn parse_recognize_missing_extended_fields_stay_none() {
        // A minimal hit (no sections / genres / isrc) must degrade to `None` per
        // field rather than fabricating a label.
        let t = parse_recognize_json(r#"{"track":{"title":"X","subtitle":"Y"}}"#)
            .expect("title + artist is a hit");
        assert_eq!(t.album, None);
        assert_eq!(t.label, None);
        assert_eq!(t.released, None);
        assert_eq!(t.genre, None);
        assert_eq!(t.isrc, None);
    }

    /// A VERBATIM (field-for-field, only trimmed of blocks the mapper ignores) songrec
    /// 0.7.4 payload captured LIVE off SomaFM Seventies on 2026-07-29. Guards the wire
    /// contract for the extended fields (task 5578wi6) against REAL Shazam output
    /// rather than a hand-written guess: the `Album`/`Label`/`Released` row labels,
    /// `genres.primary`, and top-level `isrc` are all exactly as Shazam emits them.
    const REAL_LIVE_HIT: &str = r#"{"track":{"title":"Hold On","subtitle":"Steve Winwood",
      "isrc":"GBAAN7700019","genres":{"primary":"Rock"},
      "images":{"coverart":"https://is1-ssl.mzstatic.com/c/400x400cc.jpg",
                "coverarthq":"https://is1-ssl.mzstatic.com/hq/400x400cc.jpg"},
      "sections":[{"type":"SONG","metadata":[
        {"text":"Steve Winwood","title":"Album"},
        {"text":"UMC (Universal Music Catalogue)","title":"Label"},
        {"text":"1977","title":"Released"}]}]}}"#;

    #[test]
    fn parse_recognize_json_matches_live_shazam_payload() {
        let t = parse_recognize_json(REAL_LIVE_HIT).expect("a hit");
        assert_eq!(t.title.as_deref(), Some("Hold On"));
        assert_eq!(t.artist.as_deref(), Some("Steve Winwood"));
        assert_eq!(t.album.as_deref(), Some("Steve Winwood"));
        assert_eq!(t.label.as_deref(), Some("UMC (Universal Music Catalogue)"));
        assert_eq!(t.released.as_deref(), Some("1977"));
        assert_eq!(t.genre.as_deref(), Some("Rock"));
        assert_eq!(t.isrc.as_deref(), Some("GBAAN7700019"));
        assert_eq!(t.cover_url.as_deref(), Some("https://is1-ssl.mzstatic.com/hq/400x400cc.jpg"));
    }

    #[test]
    fn parse_recognize_json_handles_live_hit_with_empty_song_rows() {
        // ALSO REAL (SomaFM Underground 80s, same capture run): a genuine hit whose
        // SONG section carries an EMPTY metadata array and a null isrc. Shazam does
        // this for obscure tracks, so the album/label/released/isrc fields must
        // degrade to `None` while the hit itself still stands on title + artist.
        let json = r#"{"track":{"title":"First, Last for Everything (Club Version)",
          "subtitle":"Endgames","isrc":null,"genres":{"primary":"Dance"},
          "sections":[{"type":"SONG","metadata":[]}]}}"#;
        let t = parse_recognize_json(json).expect("still a hit");
        assert_eq!(t.artist.as_deref(), Some("Endgames"));
        assert_eq!(t.genre.as_deref(), Some("Dance"));
        assert_eq!(t.album, None);
        assert_eq!(t.label, None);
        assert_eq!(t.released, None);
        assert_eq!(t.isrc, None);
    }

    #[test]
    fn parse_recognize_no_match_is_none() {
        // songrec's clean no-match: exit 0 with empty stdout. Also a whitespace-only
        // line must map to no-match, never an error.
        assert_eq!(parse_recognize_json(""), None);
        assert_eq!(parse_recognize_json("   \n  "), None);
    }

    #[test]
    fn parse_recognize_malformed_is_none() {
        // Garbage stdout must degrade to no-match gracefully, never panic or error.
        assert_eq!(parse_recognize_json("not json at all"), None);
        assert_eq!(parse_recognize_json("{ \"track\": "), None);
        // A well-formed object with no `track` is no-match.
        assert_eq!(parse_recognize_json("{}"), None);
        // A `track` with neither title nor artist is not a real hit.
        assert_eq!(parse_recognize_json(r#"{"track":{"images":{}}}"#), None);
    }

    #[test]
    fn parse_recognize_blank_fields_drop_out() {
        // Whitespace-only Shazam fields must not become visible labels.
        let json = r#"{"track":{"title":"Yelle","subtitle":"   ","images":{"coverarthq":"  "}}}"#;
        let t = parse_recognize_json(json).expect("title alone is a hit");
        assert_eq!(t.title.as_deref(), Some("Yelle"));
        assert_eq!(t.artist, None);
        assert_eq!(t.cover_url, None);
        assert_eq!(t.album, None);
    }

    #[test]
    fn temp_file_guard_unlinks_on_drop() {
        // The RAII guard must remove its wav on drop, in every branch. Write a real
        // file, drop the guard, and confirm it is gone.
        let path = temp_wav_path();
        std::fs::write(&path, b"wav").unwrap();
        assert!(path.exists());
        {
            let _guard = TempFileGuard(path.clone());
        }
        assert!(!path.exists(), "guard must unlink the temp wav on drop");
    }

    #[tokio::test(start_paused = true)]
    async fn run_bounded_timeout_kills_and_cleans() {
        // On timeout, run_bounded must (a) surface RecognizeError::Timeout and (b)
        // still unlink the temp wav via the RAII guard, even though `work` never
        // resolved. A never-completing `work` stands in for a hung stream; the
        // paused clock auto-advances past the timeout without real waiting. (The
        // kill_on_drop of a real child is a tokio guarantee exercised by the live
        // proof; here we pin the wiring: elapse -> Timeout + temp cleaned.)
        let path = temp_wav_path();
        std::fs::write(&path, b"wav").unwrap();
        assert!(path.exists());
        let work = async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(SongrecOutcome::NoMatch)
        };
        let res = run_bounded(path.clone(), Duration::from_secs(40), work).await;
        assert!(matches!(res, Err(RecognizeError::Timeout)));
        assert!(!path.exists(), "temp wav must be cleaned on the timeout path");
    }

    #[tokio::test]
    async fn run_bounded_passes_hit_through() {
        // The success path: a `work` that resolves in time parses into a hit and the
        // temp wav is still cleaned afterward.
        let path = temp_wav_path();
        std::fs::write(&path, b"wav").unwrap();
        let hit = SongrecOutcome::Hit(REAL_HIT.to_string());
        let work = async move { Ok(hit) };
        let res = run_bounded(path.clone(), Duration::from_secs(40), work).await;
        let track = res.expect("no error").expect("a hit");
        assert_eq!(track.title.as_deref(), Some("Blessings"));
        assert!(!path.exists(), "temp wav must be cleaned on the success path");
    }

    #[tokio::test]
    async fn run_bounded_splits_content_miss_from_transport_failure() {
        // THE residual: a content miss stays `Ok(None)` while every transport shape
        // becomes an `Err`, so the two can back off differently. Before this split both
        // arrived as an indistinguishable empty stdout.
        let path = temp_wav_path();
        let miss = run_bounded(path.clone(), Duration::from_secs(40), async {
            Ok(SongrecOutcome::NoMatch)
        })
        .await;
        assert!(matches!(miss, Ok(None)), "a content miss is Ok(None)");

        let net = run_bounded(path.clone(), Duration::from_secs(40), async {
            Ok(SongrecOutcome::Transport { rate_limited: false })
        })
        .await;
        assert!(matches!(net, Err(RecognizeError::Transport { rate_limited: false })));

        let limited = run_bounded(path.clone(), Duration::from_secs(40), async {
            Ok(SongrecOutcome::Transport { rate_limited: true })
        })
        .await;
        assert!(matches!(limited, Err(RecognizeError::Transport { rate_limited: true })));

        let unknown =
            run_bounded(path.clone(), Duration::from_secs(40), async { Ok(SongrecOutcome::Unknown) })
                .await;
        assert!(matches!(unknown, Err(RecognizeError::Unknown)));
    }

    #[test]
    fn classify_songrec_reads_songrecs_own_stderr_taxonomy() {
        // songrec 0.7.4's REAL strings. The whole point of piping stderr is that these
        // three are different facts about the world.
        assert_eq!(
            classify_songrec(Some(0), "", "No match for this song\n"),
            SongrecOutcome::NoMatch
        );
        assert_eq!(
            classify_songrec(Some(1), "", "Error: Network unreachable (os error 101)\n"),
            SongrecOutcome::Transport { rate_limited: false }
        );
        assert_eq!(
            classify_songrec(Some(1), "", "Your IP has been rate-limited\n"),
            SongrecOutcome::Transport { rate_limited: true }
        );
        // A reqwest transport failure, as songrec surfaces it.
        assert_eq!(
            classify_songrec(Some(1), "", "error sending request for url (https://amp.shazam.com)"),
            SongrecOutcome::Transport { rate_limited: false }
        );
    }

    #[test]
    fn classify_songrec_is_stdout_first_and_never_guesses_a_content_miss() {
        // A parseable track is a Hit regardless of stderr noise.
        let out = classify_songrec(Some(0), REAL_HIT, "warning: something on stderr\n");
        assert!(matches!(out, SongrecOutcome::Hit(_)));
        // The LEGACY clean shape (exit 0, empty stdout, silent stderr) stays a content
        // miss - inflating it would put the gentle backoff on the wrong arm.
        assert_eq!(classify_songrec(Some(0), "", ""), SongrecOutcome::NoMatch);
        assert_eq!(classify_songrec(Some(0), "  \n", "   "), SongrecOutcome::NoMatch);
        // An UNCLASSIFIABLE outcome must NOT be optimistically read as "Shazam does not
        // know this music": a non-zero exit with a silent stderr, and garbage stdout,
        // are both Unknown (which the backoff treats as transport).
        assert_eq!(classify_songrec(Some(1), "", ""), SongrecOutcome::Unknown);
        assert_eq!(classify_songrec(None, "", ""), SongrecOutcome::Unknown);
        assert_eq!(classify_songrec(Some(0), "not json at all", ""), SongrecOutcome::Unknown);
    }

    #[test]
    fn the_songrec_child_really_pipes_its_stderr() {
        // The classifier is pure and fully tested above, but it is only USEFUL if the
        // child's stderr actually reaches it - and that is one literal in a builder
        // chain that no unit test can otherwise observe (spawning songrec would cost a
        // real call against an IP-keyed limiter). A structural guard on the source is
        // the honest floor: it catches a revert to `Stdio::null()`, which is exactly how
        // this residual existed for a year.
        let whole = include_str!("recognize.rs");
        let src = whole.split("#[cfg(test)]").next().expect("a production half");
        let songrec = src
            .split("Command::new(\"songrec\")")
            .nth(1)
            .expect("the songrec child");
        let chain = &songrec[..songrec.find(".output()").expect("the output call")];
        assert!(
            chain.contains(".stderr(Stdio::piped())"),
            "songrec's stderr must be PIPED; its outcome taxonomy lives there"
        );
        assert!(
            !chain.contains(".stderr(Stdio::null())"),
            "nulling songrec's stderr fuses content misses with transport failures again"
        );
        // stdin stays null, so the child can never block waiting on input.
        assert!(chain.contains(".stdin(Stdio::null())"));
    }

    #[test]
    fn recognize_error_outcome_words_are_stable() {
        assert_eq!(RecognizeError::Timeout.outcome_word(), "timeout");
        assert_eq!(
            RecognizeError::Transport { rate_limited: true }.outcome_word(),
            "rate_limited"
        );
        assert_eq!(
            RecognizeError::Transport { rate_limited: false }.outcome_word(),
            "transport"
        );
        assert_eq!(RecognizeError::Unknown.outcome_word(), "transport");
        assert_eq!(RecognizeError::Capture.outcome_word(), "transport");
    }

    #[test]
    fn now_playing_title_follows_icy_convention() {
        let full = RecognizedTrack {
            artist: Some("Yelle".into()),
            title: Some("Qui est cette fille?".into()),
            ..Default::default()
        };
        assert_eq!(now_playing_title(&full).as_deref(), Some("Yelle - Qui est cette fille?"));

        let title_only = RecognizedTrack { title: Some("Just A Title".into()), ..Default::default() };
        assert_eq!(now_playing_title(&title_only).as_deref(), Some("Just A Title"));

        let artist_only = RecognizedTrack { artist: Some("Just An Artist".into()), ..Default::default() };
        assert_eq!(now_playing_title(&artist_only).as_deref(), Some("Just An Artist"));

        assert_eq!(now_playing_title(&RecognizedTrack::default()), None);
    }
}
