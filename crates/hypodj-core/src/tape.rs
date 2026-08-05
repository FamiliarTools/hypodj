//! THE TAPE: retroactive audio capture off the demuxer cache mpv is ALREADY filling.
//!
//! There is no ring to build and nothing to arm. Under the daemon's exact current
//! options mpv keeps the whole past of the current entry in RAM (`cache=auto`,
//! `demuxer-max-back-bytes=50MiB`, `seekable-ranges` starting at 0.0 with
//! `bof-cached=true` on Icecast mp3, Icecast AAC and HLS), and
//! `dump-cache <start> <end> <file>` hands back an arbitrary PAST window as a bitstream
//! copy at ~350 MB/s with zero network. So capture is not something you start, it is
//! something you stop, and the press is allowed to be late.
//!
//! Three rules shape every function here.
//!
//! **Rule 0 - dump wide, refine on disk.** The cache is VOLATILE: the instant
//! `SwitchWarmed` lands (`playlist-play-index 1` + `playlist-remove 0`) 110 seconds of
//! retained history vanished and `dump-cache` wrote a ZERO-BYTE FILE while returning
//! `"error":"success"`. So a press dumps a generous window IMMEDIATELY and only then
//! reasons about boundaries, re-cutting the LOCAL file with `ffmpeg -c copy` - no
//! network, no re-encode, repeatable. Nothing here is ever clever in the moment.
//!
//! **Rule 1 - the success return is a measured liar.** mpv's own manual: "If no data is
//! cached at the given time range, nothing may be dumped (creating a file with no
//! packets)". So a dump is proved by EVIDENCE at two layers - a byte count taken on the
//! actor thread ([`crate::player::DumpOutcome::bytes`]) and an ffprobe duration taken
//! off it ([`probe_secs`]) - and only a file that clears both ever acquires a name.
//!
//! **Rule 2 - the filename never over-claims.** Capture is exact; boundaries are a
//! best-effort claim. A file named for a track it may have cut 40 seconds late is a lie
//! discovered on playback, so [`segment_name`] has exactly two shapes and the shape IS
//! the claim: a WINDOW asserts a station, a minute and a duration (all three certain), a
//! TRACK asserts a track and is earned only by a position-stamped ICY edge. See
//! [`TRACK_SHAPE_PROVEN`] for the one measurement the track shape waits on.
//!
//! Nothing in this module runs on the DIRECTOR SPINE. The sidecar write is the one
//! fsyncing call and it lives in [`commit`], which the handler runs in
//! `spawn_blocking`; a source-text guard test below pins that.

use std::path::{Path, PathBuf};

use crate::heard::MarkSubject;

/// The shortest window worth keeping, seconds.
///
/// A `const`, deliberately NOT a knob: it is the songrec constraint
/// ([`crate::recognize::SONGREC_EXACT_SECS`]) - anything shorter cannot be fingerprinted
/// at full strength - and a configurable copy of it would drift away from the thing it
/// mirrors.
pub const TAPE_MIN_SECS: f64 = 12.0;

/// Whether the TRACK filename shape has earned the right to ship.
///
/// Both track cuts lean on ICY boundaries landing at the PLAYHEAD rather than at the
/// read position. The basis is a changelog, not a measurement: mpv 0.29.0 shipped timed
/// ICY metadata for issue #2453, so residual error should be encoder-side plus one
/// `icy-metaint`, about 1 s at 128 kbps. If that is wrong the flip lands at the read
/// position instead, and mpv holds a steady ~16.0 s of forward cache - which on an
/// [`Cut::IcyEdge`] means the file is the previous track shifted 16 s: missing its last
/// 16 s and opening with 16 s of the one before. That is audible, and it is exactly the
/// lie this module exists to prevent.
///
/// Until that flip is MEASURED against a real ICY station (dump `[flip_pos - 25,
/// flip_pos + 10]`, locate the audible join, report the offset; inside ~2 s passes), the
/// cut still TRIMS - an [`Cut::IcyEdge`] file really is narrower and cleaner - but the
/// name stays a window and claims nothing. A gate rather than a knob: a runtime toggle
/// for honesty would be a hidden mode.
pub const TRACK_SHAPE_PROVEN: bool = false;

/// How the segment's boundaries were decided. This is both the sidecar's `cut` field and
/// the input to [`segment_name`], so the record and the filename can never disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cut {
    /// BOTH edges are position-stamped ICY changes on the same entry. The file is that
    /// track, start to end. Only reachable from [`MarkSubject::Previous`].
    IcyEdge,
    /// The START is a position-stamped ICY change on this entry; the end is the press.
    /// Everything in the file is that track, from its start, truncated at the press -
    /// incomplete is not the same as wrong, and the sidecar records the truncation.
    IcyOpen,
    /// Everything else. The file certainly contains what was heard and also contains
    /// other things, and the name says exactly that and nothing more.
    Window,
}

impl Cut {
    /// The stable sidecar word.
    pub fn as_str(self) -> &'static str {
        match self {
            Cut::IcyEdge => "icy-edge",
            Cut::IcyOpen => "icy-open",
            Cut::Window => "window",
        }
    }

    /// Whether this cut is ALLOWED to mint a track-shaped filename. Both halves must
    /// hold: the cut names a real ICY edge, AND [`TRACK_SHAPE_PROVEN`] says that edge has
    /// been measured to land at the playhead.
    pub fn may_name_a_track(self) -> bool {
        self.may_name_a_track_when(TRACK_SHAPE_PROVEN)
    }

    /// The same rule with the GATE supplied, so the track branch is reachable in a test
    /// while [`TRACK_SHAPE_PROVEN`] is still false. Without this the branch is dead code
    /// that no test can distinguish from a stub.
    fn may_name_a_track_when(self, proven: bool) -> bool {
        proven && !matches!(self, Cut::Window)
    }
}

/// Why a window could not be resolved from the cache. Distinct from a dump that ran and
/// produced nothing: none of these spends an mpv command at all.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowError {
    /// The actor has no usable `time-pos` (nothing loaded, or a non-finite reading).
    NoPosition,
    /// The cache holds less than the floor. Carries what it DOES hold, which is what
    /// lets a caller WAIT the deficit instead of refetching anything.
    TooThin { available: f64 },
}

impl std::fmt::Display for WindowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WindowError::NoPosition => write!(f, "nothing is loaded to capture from"),
            WindowError::TooThin { available } => {
                write!(f, "only {available:.0}s is buffered so far")
            }
        }
    }
}

/// Resolve the PAST/FUTURE ask into an absolute `[start, end]` on the entry's own
/// timeline, clamped into the seekable range that actually contains `pos`.
///
/// PURE and TOTAL: every input including NaN, infinities and an empty range list yields
/// a value or a typed error, never a panic. Called on the mpv actor thread with the
/// numbers it read itself, which is the only place a position and a cache state can be
/// sampled in the same instant as the dump.
///
/// `max_secs` is applied by keeping the FRESHEST span (the audio nearest the press),
/// because that is the audio the press was about.
pub fn plan_window(
    pos: f64,
    back: f64,
    fwd: f64,
    ranges: &[(f64, f64)],
    max_secs: f64,
    min_secs: f64,
) -> Result<(f64, f64), WindowError> {
    if !pos.is_finite() {
        return Err(WindowError::NoPosition);
    }
    // Sanitize the asks rather than reject them: a non-finite or negative knob must
    // degrade to "no margin on that side", never to a NaN bound handed to mpv.
    let back = if back.is_finite() { back.max(0.0) } else { 0.0 };
    let fwd = if fwd.is_finite() { fwd.max(0.0) } else { 0.0 };
    let max_secs = if max_secs.is_finite() && max_secs > 0.0 {
        max_secs
    } else {
        f64::MAX
    };
    let min_secs = if min_secs.is_finite() && min_secs >= 0.0 {
        min_secs
    } else {
        0.0
    };

    // The range containing `pos`. A seekable range start can be NEGATIVE on a live
    // stream (measured: `[{start: -0.025057, ...}]`), so nothing here floors at zero.
    let Some(&(lo, hi)) = ranges
        .iter()
        .filter(|(lo, hi)| lo.is_finite() && hi.is_finite() && hi >= lo)
        .find(|(lo, hi)| pos >= *lo && pos <= *hi)
    else {
        return Err(WindowError::TooThin { available: 0.0 });
    };

    let mut start = (pos - back).max(lo);
    let mut end = (pos + fwd).min(hi);
    if end < start {
        end = start;
    }
    if end - start > max_secs {
        start = end - max_secs;
    }
    let span = end - start;
    if span < min_secs {
        return Err(WindowError::TooThin { available: span.max(0.0) });
    }
    Ok((start, end))
}

/// The entry-relative positions of the ICY boundaries this press can see, read out of
/// the handler's state under the same lock the mark snapshot already takes.
///
/// `None` on a field means "not stamped", which degrades the cut to [`Cut::Window`]
/// rather than to an error - a missing position is a weaker claim, never a failure.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CutStamps {
    /// Where the LIVE ICY title started standing.
    pub icy_start_pos: Option<f64>,
    /// Where the RETIRED ICY title started standing.
    pub prev_start_pos: Option<f64>,
    /// Where the retired title stopped being live (== where the live one began, when a
    /// live one exists; the retirement position otherwise).
    pub prev_end_pos: Option<f64>,
}

/// A resolved cut: the honesty label plus whatever narrowing the stamps permit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CutPlan {
    pub cut: Cut,
    /// Entry-relative start to re-cut the dumped file back to, when a stamped ICY edge
    /// narrows it. `None` keeps the dump's own start.
    pub start_pos: Option<f64>,
    /// Entry-relative end, when BOTH edges are known. `None` runs to the press.
    pub end_pos: Option<f64>,
}

/// Is a dump attributable to the entry the caller snapshotted?
///
/// MPD `next` advances the handler's reported current IMMEDIATELY while the warm switch
/// lands one to two seconds later, so a press inside that window snapshots the NEW entry
/// while mpv dumps the OLD entry's audio. The station and url written into a sidecar come
/// from the caller's snapshot, so a mismatch would put a truthful label on somebody
/// else's audio - the one artifact this feature must never produce.
///
/// `None` on either side means "not knowable", which is NOT a mismatch: a headless player
/// reports no entry, and refusing every capture there would break the tests rather than
/// protect anything. Pure and total.
pub fn attributable(expect: Option<u64>, got: Option<u64>) -> bool {
    match (expect, got) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

/// Decide the cut for a resolved mark subject. PURE and total over all nine
/// [`MarkSubject`] variants.
///
/// `None` means NO DUMP AT ALL, which is a real answer and not a degenerate one:
/// - [`MarkSubject::Song`] - the audio is already his, in `store/` or one download away
///   with real provenance and a `SongId`. mpv's cache for a local file IS the file, so a
///   dump would be a byte-for-byte duplicate, and `store::commit` would refuse a radio
///   copy of it anyway (server-authoritative `song.size`, `endpoint = "download"`). This
///   mirrors `identify_inner`'s `Target::LibrarySong -> AlreadyKnown`, so both halves of
///   the gesture answer the same on the same input.
/// - [`MarkSubject::Nothing`] / [`MarkSubject::NoPrevious`] - the press named nothing, so
///   there is nothing to keep.
pub fn cut_for(subject: &MarkSubject, stamps: &CutStamps) -> Option<CutPlan> {
    let finite = |v: Option<f64>| v.filter(|p| p.is_finite());
    match subject {
        MarkSubject::Song | MarkSubject::Nothing | MarkSubject::NoPrevious => None,
        MarkSubject::Previous { .. } => {
            // BOTH edges stamped: the strongest cut in the feature, and the only one that
            // can name a whole track start to end.
            match (finite(stamps.prev_start_pos), finite(stamps.prev_end_pos)) {
                (Some(s), Some(e)) if e > s => Some(CutPlan {
                    cut: Cut::IcyEdge,
                    start_pos: Some(s),
                    end_pos: Some(e),
                }),
                _ => Some(CutPlan { cut: Cut::Window, start_pos: None, end_pos: None }),
            }
        }
        MarkSubject::Icy { .. } => match finite(stamps.icy_start_pos) {
            Some(s) => Some(CutPlan { cut: Cut::IcyOpen, start_pos: Some(s), end_pos: None }),
            // An ICY subject whose start position was never stamped degrades to a
            // window rather than erroring: the press still deserves its audio.
            None => Some(CutPlan { cut: Cut::Window, start_pos: None, end_pos: None }),
        },
        MarkSubject::Ambiguous { .. }
        | MarkSubject::Show { .. }
        | MarkSubject::FreshMatch { .. }
        | MarkSubject::Moment => {
            Some(CutPlan { cut: Cut::Window, start_pos: None, end_pos: None })
        }
    }
}

/// How the local file should actually be cut, and what the record is still allowed to
/// CLAIM after the dump came back.
///
/// The two are one answer on purpose. A [`CutPlan`] is stamped on the ENTRY timeline, but
/// the dump is whatever window mpv's cache could serve - so a plan and a file can disagree,
/// and when they do the label must move, not the arithmetic.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocalCut {
    /// The honesty label the sidecar, the ledger row and the filename all take.
    pub cut: Cut,
    /// File-relative `-ss`, or `None` for "keep the dump as it is".
    pub start: Option<f64>,
    /// File-relative `-to`, only ever set alongside a `start`.
    pub end: Option<f64>,
    /// Whether the file stops at the press rather than at a real end edge.
    pub truncated_at_press: bool,
}

/// How much extra PAST a press asks for beyond a stamped ICY start, seconds.
///
/// A window that stops exactly at the edge cannot survive [`CONTAINMENT_SLACK_SECS`] of
/// stamp error, and a couple of seconds of the previous track costs nothing on a file the
/// re-cut is about to trim anyway.
pub const EDGE_LEAD_SECS: f64 = 5.0;

/// Tolerance, seconds, on "is this stamped boundary inside the dumped window". The stamps
/// come off a lossy ~1 Hz position copy (see the handler's `TitleStamp::pos`), so an exact
/// comparison would refuse edges that really are in the file.
const CONTAINMENT_SLACK_SECS: f64 = 1.5;

/// Translate a [`CutPlan`] onto the file that actually came back, DOWNGRADING the claim
/// whenever the dumped window does not contain the boundary the plan names.
///
/// THIS IS THE LINE BETWEEN A NARROWER FILE AND A LIE. `mark previous` puts no bound on
/// how old the retired ICY line is, and a press asks the cache for a fixed `back_secs` of
/// past, so a track that ended six minutes ago can still be stamped OUTSIDE the window a
/// press could dump. Subtracting anyway and clamping at zero produces a re-cut that copies
/// the WHOLE window while `cut` stays [`Cut::IcyEdge`] - a sidecar, a ledger row and a
/// sentence all asserting "the file is that track, start to end" about a file containing
/// none of it. So containment is checked, and a boundary the bytes do not hold degrades to
/// [`Cut::Window`]: wider than asked for is never a lie, and a window claims nothing but
/// the station, the minute and the duration.
///
/// The degraded case deliberately does NOT re-cut at all. Narrowing on one surviving edge
/// would still be arithmetic on a stamp the file cannot corroborate, and a wide file that
/// certainly contains what he heard is the honest floor.
///
/// PURE and TOTAL: NaN, infinities and inverted windows all yield a value.
pub fn local_cut(plan: &CutPlan, dump_start: f64, dump_end: f64) -> LocalCut {
    let window = LocalCut { cut: Cut::Window, start: None, end: None, truncated_at_press: false };
    let Some(start_pos) = plan.start_pos.filter(|p| p.is_finite()) else {
        // No usable start. Ordinarily that is just a plain window ask, but an ICY label
        // without a start position is precisely a claim nothing supports - so the answer
        // is Window either way rather than plan.cut, and an added variant cannot slip a
        // claim through on a boundary it never gave.
        return window;
    };
    if !dump_start.is_finite() || !dump_end.is_finite() || dump_end <= dump_start {
        return window;
    }
    // CONTAINMENT, both edges. A start before the dump means the file OPENS mid-track; an
    // end after it means the file runs past the track into the next one.
    if start_pos < dump_start - CONTAINMENT_SLACK_SECS || start_pos > dump_end {
        return window;
    }
    let end_pos = plan.end_pos.filter(|e| e.is_finite());
    if let Some(e) = end_pos {
        if e <= start_pos || e > dump_end + CONTAINMENT_SLACK_SECS {
            return window;
        }
    }
    LocalCut {
        cut: plan.cut,
        start: Some((start_pos - dump_start).max(0.0)),
        end: end_pos.map(|e| (e - dump_start).max(0.0)),
        truncated_at_press: matches!(plan.cut, Cut::IcyOpen),
    }
}

/// The `YYYYMMDD-HHMM` local stamp a segment name opens with, from epoch seconds.
///
/// Formatted from a unix stamp the producer already captured, exactly as
/// `heard::encode_batch` does, so this never has to be near the spine: chrono's
/// wall-clock now-reader lstats `/etc/localtime` whenever its per-thread cache is stale,
/// and that class of syscall was removed from the spine in 97bcd61.
pub fn stamp(at_unix: u64) -> String {
    chrono::DateTime::from_timestamp(at_unix as i64, 0)
        .map(|utc| {
            utc.with_timezone(&chrono::Local)
                .format("%Y%m%d-%H%M")
                .to_string()
        })
        .unwrap_or_else(|| format!("epoch-{at_unix}"))
}

/// Longest slug, in bytes. Two of them plus the stamp keep a segment name comfortably
/// inside every filesystem's 255-byte component limit.
const SLUG_MAX_BYTES: usize = 48;

/// Filename-safe slug: lowercase, `[a-z0-9]` kept, every other byte collapsed to a
/// single hyphen, leading/trailing hyphens stripped, truncated on a CHAR boundary.
///
/// This is cosmetics, not a defence. No ICY-derived byte ever reaches
/// `mpv_command_string`: mpv is only ever handed `tmp.<pid>.<seq>.mkv`, and the slug is
/// applied by the daemon's own `rename` afterwards. The tests still cover `$`, `${`, a
/// quote, a backslash and a newline, because a slug that mangled them would be a bug
/// even though it would not be a vulnerability.
pub fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(SLUG_MAX_BYTES));
    let mut pending_hyphen = false;
    for c in s.chars() {
        let mapped: Option<char> = if c.is_ascii_alphanumeric() {
            Some(c.to_ascii_lowercase())
        } else if c.is_alphanumeric() {
            // A non-ASCII letter carries no filename-safe spelling here; collapse it
            // rather than transliterating badly.
            None
        } else {
            None
        };
        match mapped {
            Some(c) => {
                if pending_hyphen && !out.is_empty() && out.len() + 1 <= SLUG_MAX_BYTES {
                    out.push('-');
                }
                pending_hyphen = false;
                if out.len() + c.len_utf8() > SLUG_MAX_BYTES {
                    break;
                }
                out.push(c);
            }
            None => pending_hyphen = true,
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// The station slug a name falls back on when nothing better is known.
const UNKNOWN_STATION: &str = "stream";

/// Build the segment id (the filename stem shared by `<id>.mkv` and `<id>.toml`).
///
/// TWO SHAPES, and the shape IS the claim a reader six weeks later can check without
/// opening anything:
///
/// - WINDOW, `20260805-2317-nts-2-w312s`: on this station, around this minute, here are
///   312 seconds that certainly contain what you heard and also contain other things. No
///   artist. No title. EVER - not when an ICY line is standing, not after a Shazam hit
///   names it.
/// - TRACK, `20260805-2317-modular-station-kassem-mosse-untitled`: earned only by a
///   position-stamped ICY edge, and only once [`TRACK_SHAPE_PROVEN`] holds.
///
/// A SHAZAM OFFSET NEVER PRODUCES A NAME and never promotes a shape. There is no
/// confidence field, the offset reports position in the STUDIO recording (a DJ's +4%
/// fader costs 0.04 x elapsed, 9.6 s at 240 s in), and a track dropped in at its second
/// chorus makes `now - offset` land in the previous track's tail. It narrows a search
/// window; it does not authorise a cut, so it never earns a name. A recognition writes
/// artist and title into the SIDECAR, labelled a guess, and leaves the filename alone -
/// which is why this function has no parameter for one.
pub fn segment_name(
    cut: Cut,
    stamp: &str,
    station: Option<&str>,
    icy_title: Option<&str>,
    secs: f64,
) -> String {
    segment_name_when(TRACK_SHAPE_PROVEN, cut, stamp, station, icy_title, secs)
}

/// [`segment_name`] with the [`TRACK_SHAPE_PROVEN`] gate supplied.
///
/// The gate is a `const false` until an ICY flip is measured against a real station, which
/// makes the track branch unreachable - and an unreachable branch that no test can enter is
/// indistinguishable from a stub, a wrong separator or a missing fallback. Whoever flips
/// the const is entitled to a branch that was actually exercised, so this is the entry
/// point the tests use and `segment_name` is the one production takes.
fn segment_name_when(
    proven: bool,
    cut: Cut,
    stamp: &str,
    station: Option<&str>,
    icy_title: Option<&str>,
    secs: f64,
) -> String {
    let station_slug = station.map(slug).filter(|s| !s.is_empty());
    let station_slug = station_slug.as_deref().unwrap_or(UNKNOWN_STATION);

    if cut.may_name_a_track_when(proven) {
        if let Some(title_slug) = icy_title.map(slug).filter(|s| !s.is_empty()) {
            return format!("{stamp}-{station_slug}-{title_slug}");
        }
    }
    let secs = if secs.is_finite() { secs.max(0.0).round() as u64 } else { 0 };
    format!("{stamp}-{station_slug}-w{secs}s")
}

// ─────────────────────────────────────────────────────────────────────────────
// The root, the pair, and the commit
// ─────────────────────────────────────────────────────────────────────────────

/// The one container, always. MEASURED rather than assumed: `dump-cache 0 12` of a raw
/// Icecast mp3 stream into `.mkv` gave 196,592 B which ffprobe reads as
/// `matroska,webm` / `mp3` / 12.042 s, against 308,132 B for the same window as `.ts`
/// (36% larger) and 193,265 B as `.mp3`. Matroska takes mp3, AAC and the HLS elementary
/// streams, `ffmpeg -c copy` re-cuts it accurately, and it removes the `.ts` class
/// entirely - which matters, because a bare mpegts/AAC slice makes songrec emit seven
/// `symphonia_codec_aac` ERROR lines that match NONE of `classify_songrec`'s twenty
/// markers, silently converting content misses into transport misses on the full
/// exponential.
pub const SEGMENT_EXT: &str = "mkv";

/// The sidecar extension. Written LAST, which is what makes an interrupted commit an
/// orphan the next sweep removes rather than a segment the UI offers.
pub const SIDECAR_EXT: &str = "toml";

/// Is this root safe to hand mpv?
///
/// mpv's flat command syntax C-unescapes inside double quotes, so a `"` or `\` in the
/// path is mangled rather than merely risky, and a newline would split the command
/// outright. The daemon controls the LEAF (`tmp.<pid>.<seq>.mkv`, ASCII by
/// construction) but not the ROOT, which comes from `[tape].dir`. One `contains` check
/// at resolution time closes the whole class; the caller warns and runs without the tape
/// rather than handing mpv an ambiguous string.
///
/// (The mpv manual's percent-length form `%n%string`, which would sidestep quoting
/// entirely, was tried against real mpv 0.41.0 with a byte-correct length, with and
/// without the `raw` prefix: it produced no file and no error either time. `raw
/// dump-cache <start> <end> "<path>"` DID work on a path containing a space and a `$`.)
pub fn root_is_safe(root: &Path) -> bool {
    let s = root.to_string_lossy();
    !s.contains('"') && !s.contains('\\') && !s.contains('\n') && !s.contains('\r')
}

/// The ownership marker that makes a directory the tape's own, mirroring the store's.
pub const TAPE_MARKER_NAME: &str = ".hypodj-tape";

/// What the marker says when a human finds it.
const TAPE_MARKER_BODY: &str = "hypodj tape: this directory is managed by hypodj and is swept.\n";

/// Refuse to adopt a directory that is not ours, exactly as `store::claim_ownership` does
/// and for exactly the same reason.
///
/// [`sweep`] deletes by ABSENCE OF A PAIR: every `.mkv` without a sibling `.toml` and every
/// `.toml` without a sibling `.mkv` is an interrupted commit and is unlinked. That rule is
/// right for a directory the tape owns and catastrophic for one it does not - `[tape].dir =
/// "~/Videos"` would lose every video in it on the FIRST press, before anything was even
/// captured. So a non-empty directory carrying no marker is refused and the caller runs
/// without the tape; an empty one is claimed by writing the marker.
///
/// Sync `std::fs` plus the fsyncing writer: called once, at startup, off the spine.
pub fn claim_ownership(root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    let marker = root.join(TAPE_MARKER_NAME);
    match std::fs::metadata(&marker) {
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // An unreadable or erroring entry counts as PRESENT: the safe direction is always
    // "someone else's directory".
    if std::fs::read_dir(root)?.next().is_some() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{} is not empty and carries no {} marker, so it is not a hypodj tape: \
                 the tape DELETES every .{} without a matching .{} in its directory. \
                 Point [tape].dir at a dedicated directory, or empty this one, before \
                 enabling the tape",
                root.display(),
                TAPE_MARKER_NAME,
                SEGMENT_EXT,
                SIDECAR_EXT
            ),
        ));
    }
    crate::resume::atomic_write_bytes(&marker, TAPE_MARKER_BODY.as_bytes())
}

/// A unique in-flight dump path: `tmp.<pid>.<seq>.mkv`, ASCII by construction.
pub fn tmp_path(root: &Path, seq: u64) -> PathBuf {
    root.join(format!("tmp.{}.{seq}.{SEGMENT_EXT}", std::process::id()))
}

/// Unlinks its in-flight dump on drop, in EVERY branch (committed / refused / panicked),
/// so a press never leaves a nameless file in the tape root. Best-effort (`let _`),
/// because a file mpv never wrote is not an error worth surfacing.
///
/// The same RAII posture `recognize::TempFileGuard` takes, and for the same reason: the
/// discard path here is not exceptional, it is the COMMON path on an ICY station, where a
/// successful star means the rip is deleted because he owns the studio master.
pub struct TmpGuard(pub PathBuf);

impl TmpGuard {
    /// Give up ownership of the path (the file has been renamed away).
    pub fn release(mut self) {
        self.0 = PathBuf::new();
    }
}

impl Drop for TmpGuard {
    fn drop(&mut self) {
        if self.0.as_os_str().is_empty() {
            return;
        }
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The audio path for a committed segment id.
pub fn segment_path(root: &Path, id: &str) -> PathBuf {
    root.join(format!("{id}.{SEGMENT_EXT}"))
}

/// The sidecar path for a committed segment id.
pub fn sidecar_path(root: &Path, id: &str) -> PathBuf {
    root.join(format!("{id}.{SIDECAR_EXT}"))
}

/// Everything known about one segment, written beside its audio.
///
/// The gap between `requested_start`/`requested_end` and `observed_secs` is what makes an
/// over-claim detectable AFTER THE FACT instead of on playback, so `observed_secs` is
/// always the ffprobe reading and never the arithmetic the caller believed.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TapeSidecar {
    /// The filename stem, repeated inside so the file is self-describing if it is ever
    /// moved or found alone.
    #[serde(default)]
    pub id: String,
    /// RFC3339 LOCAL wall clock, and the epoch seconds for the same instant. Both, as
    /// `HeardRow` carries both, so a clock jump is diagnosable rather than invisible.
    #[serde(default)]
    pub at: String,
    #[serde(default)]
    pub at_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub station: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The window that was ASKED FOR, entry-relative seconds, echoed from the actor
    /// rather than from what a caller believed.
    #[serde(default)]
    pub requested_start: f64,
    #[serde(default)]
    pub requested_end: f64,
    /// The ffprobe duration of the committed file. The only honest duration.
    #[serde(default)]
    pub observed_secs: f64,
    /// `time-pos` as the actor read it in the same instant as the dump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pos_at_dump: Option<f64>,
    /// Whether mpv still held the beginning of the entry.
    #[serde(default)]
    pub bof_cached: bool,
    /// The live ICY line at the press, verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icy_title: Option<String>,
    /// The retired ICY line, when one stood.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_icy: Option<String>,
    /// [`Cut::as_str`].
    #[serde(default)]
    pub cut: String,
    /// Whether the file was truncated at the press rather than at a real end edge.
    #[serde(default)]
    pub truncated_at_press: bool,
    /// The join key back to the ledger: the `at_unix` of the mark row that took it.
    #[serde(default)]
    pub mark_at_unix: u64,
    /// A later recognition, LABELLED A GUESS. It never reaches the filename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guess: Option<String>,
    /// Pinned against eviction by `heard keep <n>`.
    #[serde(default)]
    pub keep: bool,
}

/// Read a segment's sidecar, or `None` if it is missing or unparseable.
///
/// Sync `std::fs`: `spawn_blocking` territory.
pub fn read_sidecar(root: &Path, id: &str) -> Option<TapeSidecar> {
    let text = std::fs::read_to_string(sidecar_path(root, id)).ok()?;
    toml::from_str(&text).ok()
}

/// Flip a segment's `keep` pin, rewriting its sidecar in place.
///
/// Sync `std::fs` plus the fsyncing writer: `spawn_blocking` territory, never the spine.
pub fn set_keep(root: &Path, id: &str, keep: bool) -> std::io::Result<()> {
    let mut side = read_sidecar(root, id).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, format!("no sidecar for {id}"))
    })?;
    side.keep = keep;
    let body = toml::to_string(&side).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
    })?;
    crate::resume::atomic_write_bytes(&sidecar_path(root, id), body.as_bytes())
}

/// Probe a local file's duration with `ffprobe`, in seconds.
///
/// LAYER 2 of the two-layer verify, and the only source of the sidecar's and the
/// filename's duration - so it can never be skipped without losing the record. Catches
/// the class the actor's byte count cannot: non-empty but structurally garbage, or
/// truncated mid-frame. `None` on a spawn failure, a non-zero exit, or unparseable
/// output; the caller deletes the temp and records the honest failure.
///
/// Sync `std::process`: `spawn_blocking` territory. `ffprobe` comes from the daemon's own
/// nix wrapper (`nix/package.nix`), not an interactive PATH.
pub fn probe_secs(path: &Path) -> Option<f64> {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "default=nw=1:nk=1"])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let secs: f64 = text.trim().parse().ok()?;
    (secs.is_finite() && secs > 0.0).then_some(secs)
}

/// Re-cut `src` into `dst` on the OUTPUT side, bitstream-copied.
///
/// `-ss`/`-to` AFTER `-i` is not a style choice: input-side `-ss` plus `-t` returned
/// 7.001 s for a 5 s ask on a real dump, while the output-side form returned 5.001 s.
/// `-c copy` means no re-encode and no network - the whole point of Rule 0 is that the
/// refinement happens on bytes already on disk, as many times as wanted.
///
/// Returns whether the re-cut produced a usable file; the caller keeps the original on
/// `false`, because a narrower file that does not exist is worse than a wide one that
/// does.
///
/// Sync `std::process`: `spawn_blocking` territory.
pub fn recut(src: &Path, dst: &Path, start: f64, end: Option<f64>) -> bool {
    if !start.is_finite() || start < 0.0 {
        return false;
    }
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args(["-nostdin", "-loglevel", "error", "-y", "-i"])
        .arg(src)
        .args(["-ss", &format!("{start:.3}")]);
    if let Some(e) = end.filter(|e| e.is_finite() && *e > start) {
        cmd.args(["-to", &format!("{e:.3}")]);
    }
    let ok = cmd
        .args(["-c", "copy"])
        .arg(dst)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        let _ = std::fs::remove_file(dst);
        return false;
    }
    // A successful exit that produced nothing usable is exactly the class this module
    // refuses to trust anywhere else, so it is not trusted here either.
    match probe_secs(dst) {
        Some(secs) if secs >= TAPE_MIN_SECS => true,
        _ => {
            let _ = std::fs::remove_file(dst);
            false
        }
    }
}

/// Commit a verified temp file as `<id>.mkv` + `<id>.toml`, returning the id actually
/// used (a same-minute collision gets a `-b`, `-c`, ... suffix rather than clobbering).
///
/// ORDER IS THE CRASH CONTRACT: rename the audio, THEN write the sidecar. A SIGKILL
/// between the two leaves an orphan `.mkv` the next [`sweep`] removes; a sidecar naming a
/// file that does not exist is not producible. The ledger row is appended after this
/// returns, so a row can never name a segment that was not committed.
///
/// Sync `std::fs` plus the fsyncing sidecar writer: `spawn_blocking` territory.
pub fn commit(root: &Path, tmp: &Path, id: &str, mut side: TapeSidecar) -> std::io::Result<String> {
    std::fs::create_dir_all(root)?;
    let id = unique_id(root, id);
    let audio = segment_path(root, &id);
    std::fs::rename(tmp, &audio)?;
    side.id = id.clone();
    let body = toml::to_string(&side)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    if let Err(e) = crate::resume::atomic_write_bytes(&sidecar_path(root, &id), body.as_bytes()) {
        // The audio is already renamed. Leaving it would be an orphan the sweep removes
        // anyway; removing it here is the same outcome one pass sooner and keeps the
        // failure local.
        let _ = std::fs::remove_file(&audio);
        return Err(e);
    }
    Ok(id)
}

/// Suffix a stem until it is free. Two presses inside the same minute on the same
/// station with the same rounded duration would otherwise collide.
fn unique_id(root: &Path, id: &str) -> String {
    if !segment_path(root, id).exists() && !sidecar_path(root, id).exists() {
        return id.to_string();
    }
    for suffix in 'b'..='z' {
        let candidate = format!("{id}-{suffix}");
        if !segment_path(root, &candidate).exists() && !sidecar_path(root, &candidate).exists() {
            return candidate;
        }
    }
    // 25 collisions in one minute is not a real case; fall back to the pid+nanos rather
    // than looping forever or clobbering.
    format!("{id}-{}", std::process::id())
}

// ─────────────────────────────────────────────────────────────────────────────
// Retention
// ─────────────────────────────────────────────────────────────────────────────

/// What one sweep did, so the caller can say something true rather than guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SweepReport {
    /// Committed pairs removed.
    pub removed: usize,
    /// Orphans removed: a `.mkv` with no sidecar, a `.toml` with no audio, a foreign
    /// `tmp.*`.
    pub orphans: usize,
    /// Audio bytes remaining after the sweep.
    pub bytes: u64,
    /// True when the PINNED pairs alone still exceed the budget. The sweep stops rather
    /// than deleting something he flagged, and the next press is refused with an honest
    /// sentence - both of those beat silently exceeding the budget or silently deleting a
    /// pin.
    pub over_budget_on_pins: bool,
}

/// One segment as the sweep sees it.
struct Pair {
    id: String,
    bytes: u64,
    keep: bool,
}

/// Evict oldest-first until the audio total fits `max_bytes`, never deleting a `keep`
/// pair and never going below `keep_min` segments.
///
/// DELIBERATELY TRIVIAL, and it does NOT reuse `store.rs`'s LRU - that is not a
/// preference, it is structural. Every input to the store's eviction is
/// server-authoritative: `plan_pass` sorts `IndexEntry`s built only from a sidecar whose
/// embedded `song.id` matches the filename, `pinned_now` needs the `getStarred2` pin set,
/// `commit` hard-requires `song.size > 0` from the server and writes
/// `endpoint = "download"`, and `is_storable_id` admits only `[A-Za-z0-9_-]+`. A radio
/// window has no SongId the server knows, no size it can report and no pin verdict.
/// Entering it would mean falsifying provenance in a sidecar.
///
/// What IS reused is the SHAPE of `heard::sweep_sessions`: name-sorted (lexical equals
/// chronological by [`stamp`]'s construction, so no `stat` per file), extension-filtered,
/// floor-clamped keep, run in `spawn_blocking` before the first write. And exactly like
/// it, any file that is not a segment, a sidecar or a foreign temp is left ALONE.
///
/// Delete order is SIDECAR FIRST then audio - commit order reversed - so a crash
/// mid-delete leaves only orphan audio the next sweep removes.
///
/// Sync `std::fs`: `spawn_blocking` territory.
pub fn sweep(root: &Path, max_bytes: u64, keep_min: u32) -> SweepReport {
    let keep_min = keep_min.max(1) as usize;
    let mut report = SweepReport::default();
    let Ok(rd) = std::fs::read_dir(root) else {
        return report;
    };

    let mut audio: Vec<(String, u64)> = Vec::new();
    let mut sidecars: Vec<String> = Vec::new();
    let mut foreign_tmp: Vec<PathBuf> = Vec::new();
    let me = format!("tmp.{}.", std::process::id());
    for entry in rd.filter_map(|e| e.ok()) {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(stem) = name.strip_suffix(&format!(".{SEGMENT_EXT}")) {
            if name.starts_with("tmp.") {
                // A temp from a DEAD daemon run. Our own in-flight temp is never swept:
                // the press holds the tape single-flight for its whole life, and its RAII
                // guard unlinks it, so only another pid's leftovers are ours to clean.
                if !name.starts_with(&me) {
                    foreign_tmp.push(entry.path());
                }
                continue;
            }
            let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
            audio.push((stem.to_string(), bytes));
        } else if let Some(stem) = name.strip_suffix(&format!(".{SIDECAR_EXT}")) {
            sidecars.push(stem.to_string());
        }
        // Anything else in the directory is not ours and is never touched.
    }

    for path in foreign_tmp {
        if std::fs::remove_file(&path).is_ok() {
            report.orphans += 1;
        }
    }
    // A sidecar with no audio: the other direction of an interrupted commit or delete.
    for stem in &sidecars {
        if !audio.iter().any(|(s, _)| s == stem)
            && std::fs::remove_file(sidecar_path(root, stem)).is_ok()
        {
            report.orphans += 1;
        }
    }

    let mut pairs: Vec<Pair> = Vec::new();
    for (id, bytes) in audio {
        if !sidecars.iter().any(|s| *s == id) {
            // Audio with no sidecar: a crash between the rename and the sidecar write, or
            // a dump that never committed. Sidecar-commits-last is what makes this
            // unambiguously incomplete rather than possibly-precious.
            if std::fs::remove_file(segment_path(root, &id)).is_ok() {
                report.orphans += 1;
            }
            continue;
        }
        let keep = read_sidecar(root, &id).map(|s| s.keep).unwrap_or(false);
        pairs.push(Pair { id, bytes, keep });
    }
    // Lexical == chronological by construction (the `YYYYMMDD-HHMM` prefix), so oldest
    // first needs no clock and no stat.
    pairs.sort_by(|a, b| a.id.cmp(&b.id));

    let mut total: u64 = pairs.iter().map(|p| p.bytes).sum();
    let mut remaining = pairs.len();
    for pair in &pairs {
        if total <= max_bytes || remaining <= keep_min {
            break;
        }
        // A pinned pair still COUNTS against the budget - a budget that excludes pins
        // lies - but is never the thing deleted.
        if pair.keep {
            continue;
        }
        let _ = std::fs::remove_file(sidecar_path(root, &pair.id));
        let _ = std::fs::remove_file(segment_path(root, &pair.id));
        total = total.saturating_sub(pair.bytes);
        remaining -= 1;
        report.removed += 1;
    }
    report.bytes = total;
    if total > max_bytes {
        report.over_budget_on_pins = pairs.iter().any(|p| p.keep);
        if report.over_budget_on_pins {
            tracing::warn!(
                bytes = total,
                budget = max_bytes,
                "the tape's pinned segments alone exceed [tape].max_bytes; nothing flagged keep was deleted"
            );
        }
    }
    report
}

/// Every committed segment id in `root`, newest LAST (the sweep's order).
///
/// Sync `std::fs`: `spawn_blocking` territory.
pub fn segment_ids(root: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("tmp.") {
                return None;
            }
            name.strip_suffix(&format!(".{SEGMENT_EXT}")).map(str::to_string)
        })
        .collect();
    ids.sort();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    // The live proof reproduced this twice, in both directions, with the codec proving
    // it: a sidecar naming an mp3 station over aac bytes, because MPD `next` moves the
    // reported current one to two seconds before the warm switch actually lands.
    #[test]
    fn a_dump_from_a_different_entry_is_not_attributable() {
        assert!(!attributable(Some(7), Some(8)), "different entries must be refused");
        assert!(attributable(Some(7), Some(7)), "the same entry is fine");
    }

    #[test]
    fn an_unknowable_entry_is_not_treated_as_a_mismatch() {
        // A headless player reports no entry. Refusing there would break every test
        // without protecting anything, because there is no label to get wrong.
        assert!(attributable(None, Some(8)));
        assert!(attributable(Some(7), None));
        assert!(attributable(None, None));
    }

    // ── plan_window ─────────────────────────────────────────────────────────

    #[test]
    fn plan_window_clamps_the_ask_into_the_real_cache() {
        // The ordinary case: 300s back asked, the range holds it all.
        let r = plan_window(400.0, 300.0, 0.0, &[(0.0, 420.0)], 1200.0, 12.0);
        assert_eq!(r, Ok((100.0, 400.0)));
        // Only 90s buffered: the start clamps to the range start rather than going
        // negative, and the forward margin clamps to what the cache actually holds.
        let r = plan_window(90.0, 300.0, 60.0, &[(0.0, 106.0)], 1200.0, 12.0);
        assert_eq!(r, Ok((0.0, 106.0)));
    }

    #[test]
    fn plan_window_refuses_a_span_below_the_floor_without_spending_a_command() {
        // A press three seconds after a load. The typed error carries the number, which
        // is what lets the recognize path WAIT the deficit instead of refetching.
        let r = plan_window(3.0, 300.0, 0.0, &[(0.0, 3.0)], 1200.0, 12.0);
        assert_eq!(r, Err(WindowError::TooThin { available: 3.0 }));
        // pos outside every range, and no ranges at all.
        assert_eq!(
            plan_window(500.0, 300.0, 0.0, &[(0.0, 100.0)], 1200.0, 12.0),
            Err(WindowError::TooThin { available: 0.0 })
        );
        assert_eq!(
            plan_window(10.0, 300.0, 0.0, &[], 1200.0, 12.0),
            Err(WindowError::TooThin { available: 0.0 })
        );
    }

    #[test]
    fn plan_window_caps_the_span_and_keeps_the_freshest_audio() {
        // max_secs bounds BOTH disk per press and blocked actor time per dump, and it is
        // applied on the actor so neither a config typo nor a caller can widen it. The
        // audio nearest the press is the audio the press was about, so the cap trims the
        // OLD end.
        let r = plan_window(4000.0, 100_000.0, 0.0, &[(0.0, 4000.0)], 1200.0, 12.0);
        assert_eq!(r, Ok((2800.0, 4000.0)));
    }

    #[test]
    fn plan_window_handles_a_negative_range_start_and_disjoint_ranges() {
        // MEASURED on a real live stream: `seekable-ranges: [{start: -0.025057, ...}]`.
        // Nothing here may floor at zero.
        let r = plan_window(50.0, 300.0, 0.0, &[(-0.025_057, 60.0)], 1200.0, 12.0);
        assert_eq!(r, Ok((-0.025_057, 50.0)));
        // Two disjoint ranges: the one CONTAINING pos wins, not the first one.
        let r = plan_window(200.0, 300.0, 0.0, &[(0.0, 50.0), (150.0, 240.0)], 1200.0, 12.0);
        assert_eq!(r, Ok((150.0, 200.0)));
    }

    #[test]
    fn plan_window_is_total_over_non_finite_input() {
        // TOTAL: NaN and the infinities yield a value or a typed error, never a panic
        // and never a NaN bound formatted into an mpv command.
        assert_eq!(plan_window(f64::NAN, 300.0, 0.0, &[(0.0, 400.0)], 1200.0, 12.0), Err(WindowError::NoPosition));
        assert_eq!(plan_window(f64::INFINITY, 300.0, 0.0, &[(0.0, 400.0)], 1200.0, 12.0), Err(WindowError::NoPosition));
        // A NaN back-ask degrades to no past margin, which is TooThin here, not a crash.
        assert!(matches!(
            plan_window(400.0, f64::NAN, 0.0, &[(0.0, 400.0)], 1200.0, 12.0),
            Err(WindowError::TooThin { .. })
        ));
        // NaN bounds inside the range list are filtered out, not propagated.
        assert_eq!(
            plan_window(400.0, 300.0, 0.0, &[(f64::NAN, 400.0), (0.0, 400.0)], 1200.0, 12.0),
            Ok((100.0, 400.0))
        );
        // A non-finite cap means "no cap", never a NaN span.
        let (s, e) = plan_window(400.0, 300.0, 0.0, &[(0.0, 400.0)], f64::NAN, 12.0).unwrap();
        assert!(s.is_finite() && e.is_finite());
    }

    // ── the naming honesty gate ─────────────────────────────────────────────

    #[test]
    fn a_window_name_never_carries_a_title() {
        // The sharpest line in the design. A window asserts a station, a minute and a
        // duration - all three certain - and nothing else, EVEN when an ICY line is
        // standing.
        let n = segment_name(Cut::Window, "20260805-2317", Some("NTS 2"), Some("Kassem Mosse - Untitled"), 312.0);
        assert_eq!(n, "20260805-2317-nts-2-w312s");
        assert!(!n.contains("kassem"), "a window name must never name a track");
    }

    #[test]
    fn the_track_shape_is_gated_on_a_measurement_nobody_has_taken() {
        // Both ICY cuts still TRIM (the file really is narrower), but until the ICY flip
        // is measured to land at the playhead rather than at the read position, the NAME
        // stays a window and claims nothing. If TRACK_SHAPE_PROVEN is ever flipped, this
        // test is what makes the change deliberate rather than incidental.
        for cut in [Cut::IcyEdge, Cut::IcyOpen] {
            let n = segment_name(cut, "20260805-2317", Some("Modular Station"), Some("Kassem Mosse - Untitled"), 240.0);
            if TRACK_SHAPE_PROVEN {
                assert_eq!(n, "20260805-2317-modular-station-kassem-mosse-untitled");
            } else {
                assert_eq!(n, "20260805-2317-modular-station-w240s");
            }
        }
    }

    #[test]
    fn the_track_shape_itself_is_real_code_and_does_what_it_claims() {
        // The gate above is about WHETHER to use the track shape; this is that the shape
        // is implemented rather than stubbed, so flipping the const is a one-line change
        // and not a re-write. It calls `segment_name_when` with the gate FORCED OPEN -
        // formatting the expected string by hand here would test `format!`, and the branch
        // could be deleted outright with every tape test still green.
        assert!(!Cut::Window.may_name_a_track());
        assert!(Cut::IcyEdge.may_name_a_track_when(true));
        assert!(Cut::IcyOpen.may_name_a_track_when(true));
        assert!(!Cut::Window.may_name_a_track_when(true), "a window NEVER names a track");

        for cut in [Cut::IcyEdge, Cut::IcyOpen] {
            assert_eq!(
                segment_name_when(
                    true,
                    cut,
                    "20260805-2317",
                    Some("Modular Station"),
                    Some("Kassem Mosse - Untitled"),
                    240.0
                ),
                "20260805-2317-modular-station-kassem-mosse-untitled",
                "stamp, station, title - in that order, hyphen-separated, no duration"
            );
        }
        // A window keeps the duration shape even with the gate open and a title standing.
        assert_eq!(
            segment_name_when(true, Cut::Window, "20260805-2317", Some("Modular Station"), Some("Kassem Mosse - Untitled"), 240.0),
            "20260805-2317-modular-station-w240s"
        );
        // The FALLBACK inside the track branch: an ICY cut with no usable title has
        // nothing to name, so it must land on the window shape rather than a dangling
        // hyphen or an empty component.
        assert_eq!(
            segment_name_when(true, Cut::IcyEdge, "20260805-2317", Some("Modular Station"), None, 240.0),
            "20260805-2317-modular-station-w240s"
        );
        assert_eq!(
            segment_name_when(true, Cut::IcyEdge, "20260805-2317", Some("Modular Station"), Some("  --- "), 240.0),
            "20260805-2317-modular-station-w240s"
        );
        // And production still takes the gated entry point, whatever this test forced.
        assert_eq!(
            segment_name(Cut::IcyEdge, "20260805-2317", Some("Modular Station"), Some("Kassem Mosse - Untitled"), 240.0),
            segment_name_when(TRACK_SHAPE_PROVEN, Cut::IcyEdge, "20260805-2317", Some("Modular Station"), Some("Kassem Mosse - Untitled"), 240.0)
        );
    }

    #[test]
    fn a_nameless_station_still_yields_a_usable_window_name() {
        let n = segment_name(Cut::Window, "20260805-2317", None, None, 42.4);
        assert_eq!(n, "20260805-2317-stream-w42s");
        // A station that slugs to nothing falls back the same way rather than producing
        // a double hyphen or an empty component.
        let n = segment_name(Cut::Window, "20260805-2317", Some("   ---  "), None, 8.0);
        assert_eq!(n, "20260805-2317-stream-w8s");
        // A non-finite duration is a zero, never a NaN in a filename.
        assert_eq!(segment_name(Cut::Window, "S", None, None, f64::NAN), "S-stream-w0s");
    }

    #[test]
    fn slug_survives_every_shell_and_mpv_hostile_byte() {
        // Cosmetics rather than a defence (no ICY byte ever reaches mpv), but a slug that
        // mangled these would still be a bug.
        assert_eq!(slug("$HOME ${x} \"q\" \\ back\nline"), "home-x-q-back-line");
        assert_eq!(slug("../../etc/passwd"), "etc-passwd");
        assert_eq!(slug("---"), "");
        assert_eq!(slug(""), "");
        // Truncation lands on a char boundary and never mid-codepoint.
        let long = "a".repeat(200);
        assert_eq!(slug(&long).len(), SLUG_MAX_BYTES);
        let accents = "é".repeat(100);
        assert!(slug(&accents).is_empty(), "non-ascii letters collapse rather than transliterate badly");
        let mixed = format!("{}{}", "z".repeat(60), "é");
        assert!(mixed.len() > SLUG_MAX_BYTES);
        assert_eq!(slug(&mixed).len(), SLUG_MAX_BYTES);
    }

    #[test]
    fn every_window_name_ends_in_its_duration() {
        for secs in [0.0, 11.9, 12.0, 312.4, 1200.0] {
            let n = segment_name(Cut::Window, "20260805-2317", Some("NTS 2"), None, secs);
            assert!(n.ends_with(&format!("-w{}s", secs.round() as u64)), "{n}");
        }
    }

    // ── cut_for ─────────────────────────────────────────────────────────────

    fn stamps(icy: Option<f64>, prev_start: Option<f64>, prev_end: Option<f64>) -> CutStamps {
        CutStamps { icy_start_pos: icy, prev_start_pos: prev_start, prev_end_pos: prev_end }
    }

    #[test]
    fn cut_for_covers_every_mark_subject() {
        let full = stamps(Some(300.0), Some(60.0), Some(300.0));
        // No dump at all: he already owns it, or the press named nothing.
        assert_eq!(cut_for(&MarkSubject::Song, &full), None);
        assert_eq!(cut_for(&MarkSubject::Nothing, &full), None);
        assert_eq!(cut_for(&MarkSubject::NoPrevious, &full), None);

        // Only Previous can yield the both-ended cut.
        let prev = MarkSubject::Previous { raw: "A - B".into(), ended_secs: 5 };
        assert_eq!(
            cut_for(&prev, &full),
            Some(CutPlan { cut: Cut::IcyEdge, start_pos: Some(60.0), end_pos: Some(300.0) })
        );
        // Only Icy WITH a stamped start can yield the open-ended one.
        let icy = MarkSubject::Icy { raw: "A - B".into(), age_secs: 60 };
        assert_eq!(
            cut_for(&icy, &full),
            Some(CutPlan { cut: Cut::IcyOpen, start_pos: Some(300.0), end_pos: None })
        );

        // Everything else is a window, permanently.
        for subject in [
            MarkSubject::Ambiguous {
                raw: "A".into(),
                age_secs: 3,
                prev_raw: "B".into(),
                prev_ended_secs: 3,
            },
            MarkSubject::Show { raw: "NTS 2 - X".into(), class: crate::heard::IcyClass::Show },
            MarkSubject::FreshMatch { names: "A - B".into(), age_secs: 20 },
            MarkSubject::Moment,
        ] {
            assert_eq!(
                cut_for(&subject, &full).map(|p| p.cut),
                Some(Cut::Window),
                "{subject:?} must never claim an ICY edge"
            );
        }
    }

    #[test]
    fn a_missing_or_absurd_position_degrades_to_a_window_not_an_error() {
        let icy = MarkSubject::Icy { raw: "A - B".into(), age_secs: 60 };
        let prev = MarkSubject::Previous { raw: "A - B".into(), ended_secs: 5 };
        // No stamps at all: the press still deserves its audio, just not the claim.
        assert_eq!(cut_for(&icy, &stamps(None, None, None)).unwrap().cut, Cut::Window);
        assert_eq!(cut_for(&prev, &stamps(None, None, None)).unwrap().cut, Cut::Window);
        // NaN is not a position.
        assert_eq!(cut_for(&icy, &stamps(Some(f64::NAN), None, None)).unwrap().cut, Cut::Window);
        // An inverted pair (end before start) is not an edge.
        assert_eq!(
            cut_for(&prev, &stamps(None, Some(300.0), Some(60.0))).unwrap().cut,
            Cut::Window
        );
        // Only one edge of a Previous: not a both-ended cut.
        assert_eq!(cut_for(&prev, &stamps(None, Some(60.0), None)).unwrap().cut, Cut::Window);
    }

    // ── local_cut: where the label meets the bytes ──────────────────────

    #[test]
    fn local_cut_translates_a_contained_edge_onto_the_file() {
        // The ordinary Modular Station press: the track started 60 s ago and the dump
        // reaches back past it, so the file really can be trimmed to the edge and the
        // claim really is about the bytes.
        let plan = CutPlan { cut: Cut::IcyOpen, start_pos: Some(760.0), end_pos: None };
        assert_eq!(
            local_cut(&plan, 700.0, 1000.0),
            LocalCut { cut: Cut::IcyOpen, start: Some(60.0), end: None, truncated_at_press: true }
        );
        // `mark previous`, both edges inside: the strongest cut in the feature.
        let plan = CutPlan { cut: Cut::IcyEdge, start_pos: Some(760.0), end_pos: Some(940.0) };
        assert_eq!(
            local_cut(&plan, 700.0, 1000.0),
            LocalCut {
                cut: Cut::IcyEdge,
                start: Some(60.0),
                end: Some(240.0),
                truncated_at_press: false,
            }
        );
        // A plain window asks for no narrowing and gets none.
        let plan = CutPlan { cut: Cut::Window, start_pos: None, end_pos: None };
        assert_eq!(
            local_cut(&plan, 700.0, 1000.0),
            LocalCut { cut: Cut::Window, start: None, end: None, truncated_at_press: false }
        );
    }

    #[test]
    fn an_edge_outside_the_dumped_window_loses_the_claim_and_never_the_audio() {
        // THE LIE THIS EXISTS TO PREVENT. `mark previous` has no age bound, so a track
        // that ended six minutes ago can be stamped OUTSIDE the window a press dumped.
        // Subtracting anyway and clamping at zero produces a re-cut that copies the WHOLE
        // window while `cut` stays icy-edge - a sidecar, a ledger row and a sentence all
        // asserting "the file is that track, start to end" about a file containing none of
        // it. Containment is what stops that, and the answer is a WIDER file with a weaker
        // claim, never a narrower file with a false one.
        let plan = CutPlan { cut: Cut::IcyEdge, start_pos: Some(100.0), end_pos: Some(640.0) };
        let got = local_cut(&plan, 700.0, 1000.0);
        assert_eq!(got.cut, Cut::Window, "the label moves");
        assert_eq!(got.start, None, "and nothing is trimmed on a boundary the bytes lack");
        assert_eq!(got.end, None);
        assert!(!got.truncated_at_press);

        // The icy-open direction: an 8-minute ICY line against a 5-minute window. Its own
        // doc says "everything in the file is that track, from its start"; the first three
        // minutes are the previous track, so the claim cannot stand.
        let plan = CutPlan { cut: Cut::IcyOpen, start_pos: Some(220.0), end_pos: None };
        assert_eq!(local_cut(&plan, 700.0, 1000.0).cut, Cut::Window);

        // An END past the window is the same failure from the other side: the file runs
        // on into the next track.
        let plan = CutPlan { cut: Cut::IcyEdge, start_pos: Some(760.0), end_pos: Some(1200.0) };
        assert_eq!(local_cut(&plan, 700.0, 1000.0).cut, Cut::Window);
    }

    #[test]
    fn local_cut_tolerates_the_stamp_error_it_was_built_with_and_is_total() {
        // The stamps are a lossy ~1 Hz copy of `time-pos`, so an exact comparison would
        // refuse edges that really are in the file. A second under the window start is
        // still contained, and the offset floors at zero rather than going negative.
        let plan = CutPlan { cut: Cut::IcyOpen, start_pos: Some(699.0), end_pos: None };
        let got = local_cut(&plan, 700.0, 1000.0);
        assert_eq!(got.cut, Cut::IcyOpen);
        assert_eq!(got.start, Some(0.0), "never a negative -ss");

        // TOTAL over garbage: NaN, the infinities and an inverted window all answer.
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let plan = CutPlan { cut: Cut::IcyOpen, start_pos: Some(bad), end_pos: None };
            assert_eq!(local_cut(&plan, 700.0, 1000.0).cut, Cut::Window);
            let plan = CutPlan { cut: Cut::IcyOpen, start_pos: Some(760.0), end_pos: None };
            assert_eq!(local_cut(&plan, bad, 1000.0).cut, Cut::Window);
            assert_eq!(local_cut(&plan, 700.0, bad).cut, Cut::Window);
        }
        let plan = CutPlan { cut: Cut::IcyEdge, start_pos: Some(760.0), end_pos: Some(f64::NAN) };
        let got = local_cut(&plan, 700.0, 1000.0);
        assert_eq!(got.cut, Cut::IcyEdge, "a NaN END is no end, not a broken start");
        assert_eq!(got.end, None);
        // An empty or inverted dump window narrows nothing.
        let plan = CutPlan { cut: Cut::IcyOpen, start_pos: Some(760.0), end_pos: None };
        assert_eq!(local_cut(&plan, 1000.0, 700.0).cut, Cut::Window);
        assert_eq!(local_cut(&plan, 900.0, 900.0).cut, Cut::Window);
        // An end at or before the start is not a span.
        let plan = CutPlan { cut: Cut::IcyEdge, start_pos: Some(760.0), end_pos: Some(760.0) };
        assert_eq!(local_cut(&plan, 700.0, 1000.0).cut, Cut::Window);
    }

    // ── roots and paths ─────────────────────────────────────────────────────

    #[test]
    fn a_root_mpv_would_mangle_is_refused() {
        assert!(root_is_safe(Path::new("/home/u/.local/state/hypodj/tape")));
        assert!(root_is_safe(Path::new("/home/u/my tape/with $dollar")));
        assert!(!root_is_safe(Path::new("/home/u/ta\"pe")));
        assert!(!root_is_safe(Path::new("/home/u/ta\\pe")));
        assert!(!root_is_safe(Path::new("/home/u/ta\npe")));
    }

    #[test]
    fn the_temp_leaf_handed_to_mpv_is_ascii_by_construction() {
        let p = tmp_path(Path::new("/tmp/x"), 7);
        let leaf = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(leaf.is_ascii(), "mpv is only ever handed an ASCII leaf: {leaf}");
        assert!(leaf.starts_with("tmp."));
        assert!(leaf.ends_with(".mkv"));
    }

    // ── the directory: sweep, commit, orphans ───────────────────────────────

    /// A fresh temp dir, removed FIRST and LAST, with no `tempfile` dependency (the
    /// `heard::test_dir` rig). A prior workflow run left eight of these behind in /tmp;
    /// every test here removes its own in both directions.
    fn test_dir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("hypodj-tape-{tag}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    /// Write a committed pair directly, bypassing `commit`, so a sweep test can build a
    /// directory without an mpv or an ffprobe.
    fn place(root: &Path, id: &str, bytes: usize, keep: bool) {
        std::fs::write(segment_path(root, id), vec![0u8; bytes]).unwrap();
        let side = TapeSidecar { id: id.to_string(), keep, ..Default::default() };
        std::fs::write(sidecar_path(root, id), toml::to_string(&side).unwrap()).unwrap();
    }

    fn names(root: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn sweep_evicts_oldest_first_and_never_touches_a_foreign_file() {
        let root = test_dir("sweep-oldest");
        place(&root, "20260801-1000-nts-2-w300s", 100, false);
        place(&root, "20260802-1000-nts-2-w300s", 100, false);
        place(&root, "20260803-1000-nts-2-w300s", 100, false);
        std::fs::write(root.join("NOTES.txt"), b"not mine").unwrap();

        let r = sweep(&root, 150, 1);
        assert_eq!(r.removed, 2, "two oldest evicted to fit 150 bytes");
        assert_eq!(r.bytes, 100);
        assert!(!r.over_budget_on_pins);
        let n = names(&root);
        assert!(n.contains(&"20260803-1000-nts-2-w300s.mkv".to_string()));
        assert!(n.contains(&"20260803-1000-nts-2-w300s.toml".to_string()));
        assert!(n.contains(&"NOTES.txt".to_string()), "a foreign file is never touched");
        assert!(!n.iter().any(|f| f.starts_with("20260801")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sweep_never_deletes_a_kept_pair_and_says_so_when_pins_alone_bust_the_budget() {
        let root = test_dir("sweep-keep");
        place(&root, "20260801-1000-a-w300s", 100, true);
        place(&root, "20260802-1000-b-w300s", 100, true);
        place(&root, "20260803-1000-c-w300s", 100, false);

        let r = sweep(&root, 50, 1);
        assert_eq!(r.removed, 1, "only the unpinned one may go");
        assert_eq!(r.bytes, 200, "pins still COUNT against the budget");
        assert!(r.over_budget_on_pins, "the caller must be able to refuse the next press honestly");
        let n = names(&root);
        assert!(n.contains(&"20260801-1000-a-w300s.mkv".to_string()));
        assert!(n.contains(&"20260802-1000-b-w300s.mkv".to_string()));
        assert!(!n.iter().any(|f| f.starts_with("20260803")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sweep_floor_clamps_keep_min_to_one_so_it_can_never_empty_the_directory() {
        let root = test_dir("sweep-floor");
        place(&root, "20260801-1000-a-w300s", 100, false);
        // keep_min = 0 with a zero budget would otherwise delete everything, including
        // the segment just committed. Same rule heard::sweep_sessions pins.
        let r = sweep(&root, 0, 0);
        assert_eq!(r.removed, 0);
        assert_eq!(segment_ids(&root), vec!["20260801-1000-a-w300s".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sweep_removes_both_directions_of_an_incomplete_pair_and_a_foreign_temp() {
        let root = test_dir("sweep-orphans");
        // A crash between the rename and the sidecar write.
        std::fs::write(segment_path(&root, "20260801-1000-a-w300s"), b"audio").unwrap();
        // A crash mid-delete, or the other direction.
        std::fs::write(sidecar_path(&root, "20260802-1000-b-w300s"), b"id = \"x\"\n").unwrap();
        // A temp from a DEAD daemon run.
        std::fs::write(root.join("tmp.999999.0.mkv"), b"half").unwrap();
        // Our OWN in-flight temp, which the sweep must NOT touch.
        let mine = tmp_path(&root, 0);
        std::fs::write(&mine, b"in flight").unwrap();

        let r = sweep(&root, u64::MAX, 1);
        assert_eq!(r.orphans, 3);
        let n = names(&root);
        assert_eq!(n, vec![mine.file_name().unwrap().to_string_lossy().into_owned()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unmarked_directory_with_anything_in_it_is_never_adopted() {
        // `sweep` deletes by ABSENCE OF A PAIR, so `[tape].dir = "~/Videos"` would take
        // every video in it on the FIRST press, before a byte was even captured. The store
        // has this guard for the same reason; the tape had none.
        let root = test_dir("claim");
        std::fs::write(root.join("holiday.mkv"), b"his, not ours").unwrap();
        let err = claim_ownership(&root).expect_err("a stranger's directory is refused");
        assert!(err.to_string().contains(TAPE_MARKER_NAME), "{err}");
        assert!(
            root.join("holiday.mkv").exists(),
            "and refusing must not itself touch anything"
        );
        // The refusal is total: nothing was claimed, so a second attempt refuses too.
        assert!(claim_ownership(&root).is_err());
        assert!(!root.join(TAPE_MARKER_NAME).exists());

        // An EMPTY directory is adopted, and the marker makes the adoption durable.
        let fresh = test_dir("claim-fresh");
        claim_ownership(&fresh).expect("an empty directory is ours to take");
        assert!(fresh.join(TAPE_MARKER_NAME).exists());
        place(&fresh, "20260801-1000-a-w300s", 10, false);
        claim_ownership(&fresh).expect("a marked directory stays ours once it fills up");
        // A directory that does not exist yet is created and claimed.
        let nested = fresh.join("deeper");
        claim_ownership(&nested).expect("a missing directory is created");
        assert!(nested.join(TAPE_MARKER_NAME).exists());

        // And the marker is not something the sweep can mistake for a segment.
        let r = sweep(&fresh, 0, 1);
        assert_eq!(r.orphans, 0, "the marker is neither audio nor a sidecar");
        assert!(fresh.join(TAPE_MARKER_NAME).exists());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&fresh);
    }

    #[test]
    fn commit_writes_the_sidecar_last_and_never_clobbers_a_same_minute_collision() {
        let root = test_dir("commit");
        let tmp = tmp_path(&root, 1);
        std::fs::write(&tmp, b"first").unwrap();
        let id = commit(&root, &tmp, "20260805-2317-nts-2-w42s", TapeSidecar::default()).unwrap();
        assert_eq!(id, "20260805-2317-nts-2-w42s");
        assert!(segment_path(&root, &id).exists());
        // The sidecar records the id it was actually committed under, so a file found
        // alone is still self-describing.
        assert_eq!(read_sidecar(&root, &id).unwrap().id, id);
        assert!(!tmp.exists(), "the temp is renamed, never copied");

        let tmp2 = tmp_path(&root, 2);
        std::fs::write(&tmp2, b"second").unwrap();
        let id2 = commit(&root, &tmp2, "20260805-2317-nts-2-w42s", TapeSidecar::default()).unwrap();
        assert_eq!(id2, "20260805-2317-nts-2-w42s-b");
        assert_eq!(std::fs::read(segment_path(&root, &id)).unwrap(), b"first");
        assert_eq!(std::fs::read(segment_path(&root, &id2)).unwrap(), b"second");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn set_keep_pins_a_segment_and_a_later_sweep_honors_it() {
        let root = test_dir("keep");
        place(&root, "20260801-1000-a-w300s", 100, false);
        place(&root, "20260802-1000-b-w300s", 100, false);
        set_keep(&root, "20260801-1000-a-w300s", true).unwrap();
        assert!(read_sidecar(&root, "20260801-1000-a-w300s").unwrap().keep);

        let r = sweep(&root, 100, 1);
        assert_eq!(r.removed, 1);
        assert_eq!(segment_ids(&root), vec!["20260801-1000-a-w300s".to_string()]);
        // Pinning something that is not there is an error, never a panic and never a
        // fabricated sidecar.
        assert!(set_keep(&root, "nope", true).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_sidecar_round_trips_through_toml_with_every_field() {
        let root = test_dir("sidecar");
        let side = TapeSidecar {
            id: "20260805-2317-nts-2-w312s".into(),
            at: "2026-08-05T23:17:00+01:00".into(),
            at_unix: 1_785_000_000,
            station: Some("NTS 2".into()),
            url: Some("https://stream.example/mixtape5".into()),
            requested_start: 100.5,
            requested_end: 412.5,
            observed_secs: 312.04,
            pos_at_dump: Some(412.5),
            bof_cached: true,
            icy_title: Some("Kassem Mosse - Untitled".into()),
            prev_icy: Some("Someone - Else".into()),
            cut: Cut::Window.as_str().to_string(),
            truncated_at_press: true,
            mark_at_unix: 1_785_000_000,
            guess: Some("a guess, never the filename".into()),
            keep: true,
        };
        std::fs::write(
            sidecar_path(&root, &side.id),
            toml::to_string(&side).unwrap(),
        )
        .unwrap();
        assert_eq!(read_sidecar(&root, &side.id).unwrap(), side);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_tmp_guard_unlinks_a_refused_dump_and_releases_a_committed_one() {
        let root = test_dir("tmpguard");
        let p = tmp_path(&root, 3);
        std::fs::write(&p, b"dump").unwrap();
        {
            let _g = TmpGuard(p.clone());
        }
        assert!(!p.exists(), "a refused dump must leave no nameless file behind");

        std::fs::write(&p, b"dump").unwrap();
        TmpGuard(p.clone()).release();
        assert!(p.exists(), "a committed dump was renamed away, not deleted");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cut_words_are_stable_because_the_sidecar_is_a_record() {
        assert_eq!(Cut::IcyEdge.as_str(), "icy-edge");
        assert_eq!(Cut::IcyOpen.as_str(), "icy-open");
        assert_eq!(Cut::Window.as_str(), "window");
    }

    #[test]
    fn stamps_sort_lexically_in_chronological_order() {
        // The whole sweep depends on this: name order IS time order, so no stat per file.
        let a = stamp(1_785_000_000);
        let b = stamp(1_785_000_000 + 3600);
        assert!(a < b, "{a} must sort before {b}");
        assert_eq!(a.len(), "YYYYMMDD-HHMM".len());
        // An absurd stamp yields something usable rather than a panic.
        assert!(!stamp(u64::MAX).is_empty());
    }

    // ── the structural guards ───────────────────────────────────────────────

    #[test]
    fn this_module_never_reaches_for_the_fsyncing_writer_off_the_commit_path() {
        // 97bcd61 removed a chrono wall-clock now-read that lstat'd /etc/localtime under
        // the State lock on the director spine, and the fsync in atomic_write_bytes is the
        // same class of thing. This module must never reintroduce either: every timestamp
        // comes from an epoch second the producer already captured (see `stamp`), and the
        // only fsyncing writes are the two sidecar commits, both of which the handler runs
        // in spawn_blocking.
        let whole = include_str!("tape.rs");
        let src = whole.split("#[cfg(test)]").next().expect("a production half");
        assert!(
            !src.contains("Local::now("),
            "no wall-clock read in this module; `stamp` formats an epoch second the caller already had"
        );
        assert_eq!(
            src.matches("atomic_write_bytes(").count(),
            3,
            "the fsyncing writer belongs to `commit`, `set_keep` and the one-off `claim_ownership` only"
        );
        assert!(!src.contains(".sync_all("), "no direct fsync outside atomic_write_bytes");
    }
}
