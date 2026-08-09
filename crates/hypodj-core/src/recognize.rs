//! On-demand now-playing RECOGNITION for raw streams (task f7vnd3i).
//!
//! Sibling jmrwr99 surfaces a stream's ICY `icy-name`/`icy-title` into MPD
//! `Name`/`Title`. But some real radio streams (the NTS mixtapes) carry NO ICY at
//! all, so the now-playing text must come from OUTSIDE the stream. This module
//! fingerprints a short sample with `songrec` (open-source Shazam) and returns the
//! recognized artist / title / album / cover art.
//!
//! THE SAMPLE IS NO LONGER DOWNLOADED. It used to be: `ffmpeg -i <stream url> -t 11`
//! re-fetched a stream the daemon was ALREADY downloading, at 0.17 s CPU / 9.73 s wall /
//! 401 KB per call, roughly 65 bytes pulled per byte of fingerprint sent. mpv is holding
//! that audio in RAM, so the caller now hands this module a LOCAL file dumped off the
//! demuxer cache in ~6 ms with zero network (see [`crate::tape`]), and the timing inverts:
//! identify now names the 12 s BEFORE the press rather than the 11 s after it.
//!
//! Two honest subprocess steps, both async (`tokio::process`, so the child I/O
//! never blocks the reactor and a timeout can actually KILL the child):
//! 1. `ffmpeg` cuts EXACTLY [`SONGREC_EXACT_SECS`] out of the local dump to a temp mono
//!    16 kHz wav - see that constant for why the exactness is load-bearing.
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

/// Total wall-clock ceiling for one capture + recognition.
///
/// Since the sample stopped being downloaded this is very nearly a PURE SHAZAM BUDGET:
/// the local ffprobe + ffmpeg cut costs milliseconds, so a slow attempt is a slow
/// endpoint. On elapse the in-flight child future is dropped, which `kill_on_drop` turns
/// into a real SIGKILL of the `ffmpeg`/`songrec` child (no orphan survives), and the temp
/// wav is cleaned by the RAII guard - only THEN does the async call return and release the
/// caller's in-flight guard, so a later `identify` still runs on a clean slate.
pub const RECOGNIZE_TIMEOUT: Duration = Duration::from_secs(40);

/// The EXACT slice length songrec must be handed, seconds. Get this wrong and every
/// reported offset is silently, permanently wrong.
///
/// songrec's `SignatureGenerator` PADS anything under 12 s and CENTRE-CROPS anything over
/// it. Verified locally, byte-exactly, on a time-varying 20 s mono/16k source with the
/// network-free `audio-file-to-fingerprint` subcommand: the 20 s file and its centre 12 s
/// fingerprint IDENTICALLY, while its first 12 s and its first 13 s each differ. And in
/// the other direction an 11 s file fingerprints identically to itself plus 1 s of
/// appended silence, but differs from the same silence PREPENDED - so sub-12 s input is
/// TAIL-padded.
///
/// The precise rule: any slice at or below 12.000 s anchors at FILE START, exactly
/// 12.000 s maximises the fingerprint, and anything above 12.000 s re-anchors to the
/// MIDPOINT with an error of `(duration - 12) / 2` seconds. The old code sat on the safe
/// side of that line only by accident of a bare `-t 11` literal that was named nowhere.
pub const SONGREC_EXACT_SECS: f64 = 12.0;

/// The EXACT byte size of a correct sample wav: a 44-byte canonical RIFF header plus
/// `12 x 16000 x 2` bytes of mono 16-bit 16 kHz PCM.
///
/// ENFORCEMENT IS A BYTE COUNT, NOT A COMMENT. The wav is stat'd against this BEFORE
/// songrec is spawned, so a slice that would silently re-anchor every future offset
/// becomes a loud local failure costing zero calls against an IP-keyed limiter.
///
/// Both `-fflags +bitexact` and `-flags +bitexact` are required to hit it: verified
/// locally that without them ffmpeg writes an extra 34-byte LIST/INFO chunk and the file
/// is 384,078. The flags are load-bearing for the assertion, not cosmetic.
pub const SONGREC_WAV_BYTES: u64 = 44 + 2 * 16_000 * 12;

/// Slack the cut leaves at the END of the dump, seconds - the difference between a
/// container's DECLARED duration and the audio a decoder actually emits from it.
///
/// MEASURED, and it is why this constant exists rather than a comment. A cut taken flush
/// against `ffprobe`'s `format=duration` on an mp3-in-matroska window - the exact shape
/// `dump-cache` produces on his Icecast mp3 stations - comes back SHORT every time:
/// 383,220 B of the required 384,044 on a 25 s window, 383,724 on a 13 s one, 383,436 on a
/// 90 s one. mp3 carries encoder delay plus whole-frame granularity (26.12 ms per frame at
/// 44.1 kHz), so the declared duration over-reports the decodable audio by tens of
/// milliseconds, and [`SONGREC_WAV_BYTES`] (correctly) rejects the result. Backing the cut
/// off by a quarter second - ten times the largest shortfall observed - lands it INSIDE the
/// stream, where the same command is byte-exact, and costs 0.25 s of freshness on a window
/// the design already calls generous.
pub const SONGREC_TAIL_MARGIN_SECS: f64 = 0.25;

/// The minimum LOCAL DUMP length the cut needs, seconds. Deliberately above
/// [`SONGREC_EXACT_SECS`] so a container's own rounding can never leave the cut a few
/// milliseconds short of an exact slice, which the byte check would (correctly, but
/// uselessly) reject.
///
/// The margin is on BOTH sides, which is what sets this number. The tail needs
/// [`SONGREC_TAIL_MARGIN_SECS`]; the head needs its own slack because an output-side `-ss`
/// within roughly the first 0.7 s of an mp3-in-matroska window measured 384,046 B - two
/// samples LONG, the priming region reading back as one extra frame. 12.0 + 0.25 + 1.25
/// keeps the smallest admissible dump's cut clear of both edges, and the extra half second
/// of waiting on a cache that has not filled yet is invisible beside the 9.73 s refetch
/// this path replaced.
pub const RECOGNIZE_MIN_DUMP_SECS: f64 = 13.5;

/// How much PAST to ask the cache for when sampling. Wider than needed, per Rule 0:
/// deciding the exact slice happens afterwards on a local file, never in the moment.
pub const RECOGNIZE_BACK_SECS: f64 = 20.0;

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
    /// The SAMPLE could not be turned into a fingerprintable wav: the local cache dump was
    /// missing, unprobeable, or `ffmpeg` exited non-zero cutting it - or the cut came back
    /// at the wrong size, which means the slice is not exactly [`SONGREC_EXACT_SECS`] and
    /// must never be fingerprinted.
    ///
    /// Its MEANING changed with the source: it used to be "the stream URL was
    /// unreachable", and it is now "nothing usable came out of the cache". That is the
    /// class the design study's zero-byte-file-with-success lands in, and catching it here
    /// is mandatory rather than defensive.
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
            RecognizeError::Capture => write!(f, "no usable audio in the buffer"),
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

/// A unique temp path for one cache DUMP: `hypodj-dump-<pid>-<counter>.mkv`.
///
/// ASCII by construction and in the system temp dir, because mpv writes this path itself
/// from its own thread: mpv's flat command syntax C-unescapes inside double quotes and
/// runs property expansion on every string argument, so the only safe leaf is one the
/// daemon minted. Matroska for the reason `tape::SEGMENT_EXT` records - it takes mp3, AAC
/// and the HLS elementary streams, and it removes the bare-mpegts class that makes songrec
/// emit `symphonia_codec_aac` noise no marker in this file can classify.
pub fn temp_dump_path() -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "hypodj-dump-{}-{}.{}",
        std::process::id(),
        n,
        crate::tape::SEGMENT_EXT
    ))
}

/// Where in a local dump the exact slice starts: the freshest
/// [`SONGREC_EXACT_SECS`] that still ends [`SONGREC_TAIL_MARGIN_SECS`] INSIDE the file.
///
/// NOT flush against the end, and that is the whole point of the margin - a container's
/// declared duration is not its decodable duration, so a flush cut on any compressed
/// stream comes back short of [`SONGREC_WAV_BYTES`] and the enforcement below rejects every
/// attempt. See [`SONGREC_TAIL_MARGIN_SECS`] for the measurements.
///
/// TOTAL by construction, and the property that matters is that the resulting slice is
/// never LONGER than [`SONGREC_EXACT_SECS`] in either direction - above it songrec
/// centre-crops and re-anchors every reported offset by `(duration - 12) / 2`.
pub fn sample_offset(dump_secs: f64) -> f64 {
    if !dump_secs.is_finite() {
        return 0.0;
    }
    (dump_secs - SONGREC_EXACT_SECS - SONGREC_TAIL_MARGIN_SECS).max(0.0)
}

/// Probe a local file's duration with `ffprobe`, asynchronously.
///
/// Async rather than the sync `tape::probe_secs` because this one sits INSIDE the
/// [`RECOGNIZE_TIMEOUT`] bound: its child must ride the reactor and be reapable by
/// `kill_on_drop` if the whole attempt is abandoned.
async fn probe_secs(path: &Path) -> Option<f64> {
    use std::process::Stdio;
    use tokio::process::Command;
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "default=nw=1:nk=1"])
        .arg(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let secs: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    (secs.is_finite() && secs > 0.0).then_some(secs)
}

/// Cut EXACTLY [`SONGREC_EXACT_SECS`] out of the LOCAL `sample` into `wav`, and PROVE it
/// by byte count before returning.
///
/// Split out of [`capture_and_recognize`] deliberately: this is the half that can be
/// exercised end to end against a real container with real ffmpeg and ZERO Shazam calls,
/// which is the only way the exactness contract can be regression-tested at all. The
/// failure it exists to catch is silent and format-dependent - a flush-to-the-end cut is
/// byte-exact on PCM and short on mp3 - so a test whose source is lossless proves nothing
/// about the streams he listens to.
async fn cut_exact_sample(sample: &Path, wav: &Path) -> Result<(), RecognizeError> {
    use std::process::Stdio;
    use tokio::process::Command;

    // 0. How long the local dump actually is, so the cut can site its 12.000 s. An
    // unprobeable dump is a Capture failure here and not a guess: mpv's cache dump returns
    // success even when it wrote nothing at all.
    let dump_secs = probe_secs(sample).await.ok_or(RecognizeError::Capture)?;
    let offset = sample_offset(dump_secs);

    // 1. The cut itself. Output-side `-ss` (after `-i`) plus `-t`, and both bitexact flags
    // so the wav is byte-predictable - see SONGREC_WAV_BYTES.
    let capture = Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
        .arg(sample)
        .args([
            "-ss",
            &format!("{offset:.3}"),
            "-t",
            &format!("{SONGREC_EXACT_SECS:.3}"),
            "-ac",
            "1",
            "-ar",
            "16000",
            "-fflags",
            "+bitexact",
            "-flags",
            "+bitexact",
            "-f",
            "wav",
        ])
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

    // 2. THE ENFORCEMENT. Any other size means the slice is not 12.000 s, which would
    // silently re-anchor every offset from here on.
    let bytes = tokio::fs::metadata(wav).await.map(|m| m.len()).unwrap_or(0);
    if bytes != SONGREC_WAV_BYTES {
        tracing::warn!(
            bytes,
            expected = SONGREC_WAV_BYTES,
            dump_secs,
            offset,
            "the recognition sample is not exactly 12.000s; refusing to fingerprint it"
        );
        return Err(RecognizeError::Capture);
    }
    Ok(())
}

/// The subprocess half: cut EXACTLY [`SONGREC_EXACT_SECS`] out of the LOCAL `sample`
/// (already dumped off mpv's cache by the caller - no network, no second fetch), then
/// fingerprint that wav with `songrec recognize --json`, returning songrec's raw stdout.
///
/// Uses `tokio::process` so the child I/O rides the reactor (never blocks it) and every
/// child carries `kill_on_drop(true)` - so if the awaiting future is dropped (the
/// [`RECOGNIZE_TIMEOUT`] path) the in-flight child is SIGKILLed rather than orphaned.
/// Every subprocess uses `Stdio::null()` for stdin so it can never block waiting on input.
///
/// WHY FFMPEG SURVIVES, given songrec eats wav, mp3 and bare ADTS directly and needs no
/// resampling or downmix. Three reasons, none of them fetching:
/// 1. CUTTING EXACTLY 12.000 s. This is the load-bearing one, and it is why the step is a
///    `-f wav -ar 16000` RE-ENCODE rather than a `-c copy`: a copy lands on frame
///    boundaries (an mp3 frame at 44.1 kHz is 26.12 ms), so 12.0 plus or minus a frame -
///    and "plus" silently crosses the centre-crop line.
/// 2. NORMALISING a container songrec decodes noisily. A bare mpegts/AAC slice makes it
///    emit seven `symphonia_codec_aac` ERROR lines that match NONE of
///    [`classify_songrec`]'s twenty markers, which would convert content misses into
///    transport misses on the full exponential.
/// 3. BEING THE STEP THAT CAN FAIL LOUDLY on a corrupt or empty dump - the design study's
///    zero-byte-file-with-success is exactly the class this catches.
///
/// songrec's OWN stderr carries the outcome taxonomy - "No match for this song" versus
/// "Network unreachable" - so it is PIPED and classified by [`classify_songrec`]
/// rather than discarded. ffmpeg's stderr stays null (it is decode noise, and a failed
/// cut is already an unambiguous non-zero exit). The exit status is passed to the
/// classifier but is never on its own a hard error.
async fn capture_and_recognize(sample: &Path, wav: &Path) -> Result<SongrecOutcome, RecognizeError> {
    use std::process::Stdio;
    use tokio::process::Command;

    // 1. The EXACT cut, from bytes already on disk, verified before a single call is
    // spent against an IP-keyed limiter.
    cut_exact_sample(sample, wav).await?;

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

/// Recognize the audio in a LOCAL `sample` (a bounded window the caller already dumped
/// off mpv's demuxer cache). `Ok(None)` is a clean NO MATCH (the honest common case for a
/// niche stream); `Ok(Some(_))` is a hit; `Err(_)` is a subprocess/timeout failure.
///
/// ASYNC/LOCK DISCIPLINE unchanged from the URL-fetching version it replaces: the caller
/// reads the stream URL under the std state lock and DROPS the lock before calling this.
/// The heavy work is bounded by [`RECOGNIZE_TIMEOUT`] so a hung Shazam call cannot wedge
/// the trigger; on elapse the child future is dropped and `kill_on_drop` reaps the child.
/// The temp wav is cleaned in every branch by [`TempFileGuard`]; the SAMPLE belongs to the
/// caller, who owns its own guard.
pub async fn recognize_local_sample(
    sample: &Path,
) -> Result<Option<Recognition>, RecognizeError> {
    let wav = temp_wav_path();
    run_bounded(wav.clone(), RECOGNIZE_TIMEOUT, capture_and_recognize(sample, &wav)).await
}

/// Bound `work` (the capture+recognize future) by `timeout`, cleaning `wav` on EVERY
/// exit via [`TempFileGuard`] - including the timeout branch, where dropping `work`
/// also `kill_on_drop`-reaps the in-flight child. Split out from
/// [`recognize_local_sample`] so the timeout + cleanup wiring is unit-testable with a
/// synthetic `work` future (no real hung stream needed).
async fn run_bounded(
    wav: PathBuf,
    timeout: Duration,
    work: impl std::future::Future<Output = Result<SongrecOutcome, RecognizeError>>,
) -> Result<Option<Recognition>, RecognizeError> {
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
        SongrecOutcome::Hit(stdout) => Ok(parse_recognize_json(&stdout).map(|track| Recognition {
            track,
            offset_secs: parse_recognize_offset(&stdout),
        })),
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
    /// Shazam's match array. It has been arriving on stdout all along and serde was
    /// silently dropping it, because this struct declared only `track`. Only `offset` is
    /// read; `id`, `timeskew` and `frequencyskew` stay dropped deliberately.
    #[serde(default)]
    matches: Vec<MatchJson>,
}

#[derive(serde::Deserialize)]
struct MatchJson {
    /// Position of the sample within the STUDIO recording, seconds. Can be NEGATIVE,
    /// which is the strong case: the track began INSIDE our own capture.
    offset: Option<f64>,
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

/// Shazam's reported `matches[0].offset`, when one came back.
///
/// A SEARCH WINDOW, NEVER A CUT POINT, and nothing downstream may promote it. There is no
/// confidence field anywhere in the envelope; the offset reports position in the STUDIO
/// recording, so a DJ's +4% pitch fader costs `0.04 x elapsed` (9.6 s at 240 s in); and a
/// track dropped in at its second chorus makes `now - offset` land in the PREVIOUS
/// track's tail. It narrows an unbounded search to a seconds-to-tens-of-seconds window.
/// It is recorded as a labelled guess and it never touches a filename.
///
/// It is also only meaningful because the slice handed to songrec is exactly
/// [`SONGREC_EXACT_SECS`] - see that constant.
pub fn parse_recognize_offset(stdout: &str) -> Option<f64> {
    let resp: RecognizeResponse = serde_json::from_str(stdout.trim()).ok()?;
    resp.matches
        .into_iter()
        .find_map(|m| m.offset)
        .filter(|o| o.is_finite())
}

/// A hit: the track, plus Shazam's own position reading if it sent one.
///
/// Two fields rather than one struct field on [`RecognizedTrack`], because that type
/// derives `Eq` and an `f64` cannot live on it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Recognition {
    pub track: RecognizedTrack,
    /// See [`parse_recognize_offset`]. Never authorises a cut and never earns a name.
    pub offset_secs: Option<f64>,
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
        let hit = res.expect("no error").expect("a hit");
        assert_eq!(hit.track.title.as_deref(), Some("Blessings"));
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
    fn the_songrec_wav_size_is_the_arithmetic_it_claims_to_be() {
        // 44-byte canonical RIFF header + 12 s of mono 16-bit 16 kHz PCM. If someone
        // changes the sample rate, the channel count or the slice length, this is what
        // says so out loud instead of letting the byte check silently reject every sample.
        assert_eq!(SONGREC_WAV_BYTES, 44 + 2 * 16_000 * 12);
        assert_eq!(SONGREC_WAV_BYTES, 384_044);
        assert_eq!(SONGREC_EXACT_SECS, 12.0);
        // The dump floor must leave room for an exact cut plus BOTH container margins.
        assert!(RECOGNIZE_MIN_DUMP_SECS > SONGREC_EXACT_SECS + SONGREC_TAIL_MARGIN_SECS);
    }

    #[test]
    fn the_sample_slice_is_never_longer_than_twelve_seconds_in_either_direction() {
        // THE trap: anything ABOVE 12.000 s re-anchors to the file midpoint with an error
        // of (duration - 12) / 2, silently and forever. Anything at or below anchors at
        // file start. So the resulting slice length must never exceed 12.000 s, whatever
        // the dump turned out to be. A property sweep, plus the degenerate inputs.
        let mut d = 0.0f64;
        while d <= 600.0 {
            let off = sample_offset(d);
            assert!(off >= 0.0, "offset must never be negative at {d}");
            assert!(off <= d.max(0.0) + f64::EPSILON, "offset must sit inside the dump at {d}");
            let slice = (d - off).min(SONGREC_EXACT_SECS);
            assert!(
                slice <= SONGREC_EXACT_SECS + 1e-9,
                "a {d}s dump would hand songrec {slice}s and re-anchor every offset"
            );
            // Above the exact length the cut takes the freshest audio that still ENDS
            // inside the file - never flush against a declared duration a decoder does
            // not reach (see SONGREC_TAIL_MARGIN_SECS).
            if d > SONGREC_EXACT_SECS + SONGREC_TAIL_MARGIN_SECS {
                assert!((off - (d - SONGREC_EXACT_SECS - SONGREC_TAIL_MARGIN_SECS)).abs() < 1e-9);
                assert!(
                    d - (off + SONGREC_EXACT_SECS) >= SONGREC_TAIL_MARGIN_SECS - 1e-9,
                    "a {d}s dump leaves no tail margin, so a compressed stream cuts short"
                );
            } else {
                assert_eq!(off, 0.0, "a short dump anchors at file start (songrec tail-pads)");
            }
            d += 0.25;
        }
        // AND the floor the dump path enforces must leave room for both margins at once,
        // which is the arithmetic that keeps the smallest admissible dump cuttable.
        assert!(
            sample_offset(RECOGNIZE_MIN_DUMP_SECS) >= 1.0,
            "the smallest admissible dump must still cut clear of the container's head"
        );
        // TOTAL over garbage.
        assert_eq!(sample_offset(f64::NAN), 0.0);
        assert_eq!(sample_offset(f64::INFINITY), 0.0);
        assert_eq!(sample_offset(-5.0), 0.0);
    }

    /// Is a real ffmpeg toolchain reachable? Both bins come from the daemon's own nix
    /// wrapper in production and from the devshell / check inputs here; a machine without
    /// them skips rather than failing, the same posture `handler_with_null_player` takes.
    fn ffmpeg_available() -> bool {
        ["ffmpeg", "ffprobe"].iter().all(|tool| {
            std::process::Command::new(tool)
                .arg("-version")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        })
    }

    /// A fresh temp dir, removed FIRST and LAST, with no `tempfile` dependency.
    fn media_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("hypodj-cut-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    /// `secs` of tone encoded with `codec`, then muxed into matroska with `-c copy` -
    /// exactly the container shape `dump-cache` writes, and exactly the reason the codec
    /// matters: the copy preserves the encoder delay and frame granularity that make a
    /// declared duration over-report the decodable audio.
    fn tone_in_mkv(dir: &Path, name: &str, codec: &str, ext: &str, secs: f64) -> PathBuf {
        let raw = dir.join(format!("{name}.{ext}"));
        let mkv = dir.join(format!("{name}.mkv"));
        let run = |args: Vec<String>| {
            let ok = std::process::Command::new("ffmpeg")
                .args(&args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "ffmpeg failed for {args:?}");
        };
        run(vec![
            "-nostdin".into(), "-loglevel".into(), "error".into(), "-y".into(),
            "-f".into(), "lavfi".into(),
            "-i".into(), format!("sine=frequency=440:sample_rate=44100:duration={secs}"),
            "-c:a".into(), codec.into(),
            raw.display().to_string(),
        ]);
        run(vec![
            "-nostdin".into(), "-loglevel".into(), "error".into(), "-y".into(),
            "-i".into(), raw.display().to_string(),
            "-c".into(), "copy".into(),
            mkv.display().to_string(),
        ]);
        mkv
    }

    #[tokio::test]
    async fn the_exact_cut_is_byte_exact_on_a_real_compressed_dump() {
        // THE REGRESSION, and it is only visible against a real compressed container. A
        // cut taken flush against ffprobe's declared duration is byte-exact on PCM and
        // SHORT on mp3 - 383,220 B of 384,044 was measured on a 25 s mp3-in-matroska
        // window - so `SONGREC_WAV_BYTES` rejected every sample and every identify on his
        // main stations was logged as a transport miss on the full exponential, without a
        // single Shazam call ever being spent. The live proof used a PCM source, which is
        // structurally incapable of showing it. This runs the SHIPPED cut over all three.
        if !ffmpeg_available() {
            return;
        }
        let dir = media_dir("exact");
        let wav = dir.join("sample.wav");
        for (codec, ext) in [("libmp3lame", "mp3"), ("aac", "m4a"), ("pcm_s16le", "wav")] {
            for secs in [RECOGNIZE_MIN_DUMP_SECS, RECOGNIZE_BACK_SECS, 90.0] {
                let src = tone_in_mkv(&dir, "dump", codec, ext, secs);
                let got = cut_exact_sample(&src, &wav).await;
                assert!(
                    got.is_ok(),
                    "a {secs}s {codec} dump must yield an exact slice, got {got:?} ({} B)",
                    std::fs::metadata(&wav).map(|m| m.len()).unwrap_or(0)
                );
                assert_eq!(
                    std::fs::metadata(&wav).unwrap().len(),
                    SONGREC_WAV_BYTES,
                    "{codec} at {secs}s"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_unprobeable_dump_fails_locally_and_never_reaches_songrec() {
        // The zero-byte-file-with-success class the study measured after a warm switch.
        if !ffmpeg_available() {
            return;
        }
        let dir = media_dir("garbage");
        let src = dir.join("dump.mkv");
        std::fs::write(&src, b"not a container at all").unwrap();
        let wav = dir.join("sample.wav");
        assert!(matches!(cut_exact_sample(&src, &wav).await, Err(RecognizeError::Capture)));
        assert!(!wav.exists(), "nothing usable was written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_shazam_offset_survives_serde_now_and_is_never_promoted() {
        // It has been arriving on stdout all along and this struct was dropping it.
        let json = r#"{"matches":[{"id":"1","offset":-6.500591796,"timeskew":0.0001,
          "frequencyskew":0.0}],"track":{"title":"X","subtitle":"Y"}}"#;
        assert_eq!(parse_recognize_offset(json), Some(-6.500591796));
        // A NEGATIVE offset is the strong case (the track began inside our own capture),
        // so it must NOT be filtered out as nonsense.
        assert!(parse_recognize_offset(json).unwrap() < 0.0);
        // No matches block, a null offset, and garbage all degrade to None rather than
        // fabricating a position.
        assert_eq!(parse_recognize_offset(REAL_HIT), None);
        assert_eq!(parse_recognize_offset(r#"{"matches":[{"offset":null}]}"#), None);
        assert_eq!(parse_recognize_offset("not json"), None);
        // A non-finite offset is not a position.
        assert_eq!(parse_recognize_offset(r#"{"matches":[{"offset":1e999}]}"#), None);
    }

    #[tokio::test]
    async fn a_hit_carries_its_offset_through_run_bounded() {
        let path = temp_wav_path();
        let json = r#"{"matches":[{"offset":42.5}],"track":{"title":"X","subtitle":"Y"}}"#;
        let work = async move { Ok(SongrecOutcome::Hit(json.to_string())) };
        let hit = run_bounded(path.clone(), Duration::from_secs(40), work)
            .await
            .expect("no error")
            .expect("a hit");
        assert_eq!(hit.track.title.as_deref(), Some("X"));
        assert_eq!(hit.offset_secs, Some(42.5));
        assert!(!path.exists());
    }

    #[test]
    fn this_module_can_no_longer_download_the_stream_it_is_already_playing() {
        // The second HTTP fetch is DELETED, not merely unused. A future edit that
        // reintroduces `-rw_timeout` or feeds a URL to ffmpeg would restore a 9.73 s /
        // 401 KB side-band download of a stream the daemon already has in RAM, and would
        // do it on the least-exercised path in the module. A structural guard is the
        // honest floor, exactly as it is for songrec's stderr below.
        let whole = include_str!("recognize.rs");
        let src = whole.split("#[cfg(test)]").next().expect("a production half");
        assert!(
            !src.contains("rw_timeout"),
            "nothing here reads a socket any more; an rw_timeout means a fetch came back"
        );
        assert!(
            !src.contains("recognize_stream_url"),
            "the URL-fetching entry point is gone; the sample comes from the local cache dump"
        );
        // The ffmpeg child's input is a local sample path, never a url.
        let ffmpeg = src
            .split("Command::new(\"ffmpeg\")")
            .nth(1)
            .expect("the ffmpeg child");
        let chain = &ffmpeg[..ffmpeg.find(".status()").expect("the status call")];
        assert!(chain.contains(".arg(sample)"), "ffmpeg reads the LOCAL dump");
        assert!(!chain.contains("url"), "ffmpeg must never be handed a url again");
        // And both bitexact flags stay, because SONGREC_WAV_BYTES depends on them.
        assert!(chain.contains("\"-fflags\"") && chain.contains("\"+bitexact\""));
        assert!(chain.contains("\"-flags\""));
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
