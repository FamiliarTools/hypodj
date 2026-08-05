//! The HEARD LEDGER: an append-only, session-scoped record of what the radio
//! played, plus the pure decision logic behind the MARK gesture that reads it back.
//!
//! The always-on half of continuous identification (ICY titles, the auto-identify
//! cadence) already ran and kept NOTHING. This module is the durable substrate under
//! it, and the honest-subject rules over that substrate:
//!
//! 1. [`HeardLedger`] / [`spawn_heard_ledger`] - the WRITE path. An unbounded mpsc to
//!    a DEDICATED task holding one `O_APPEND` handle, with every syscall inside
//!    `spawn_blocking` (the contract `store.rs` documents). This shape is not a
//!    preference, it is the invariant: `director.rs` handles
//!    `PlayerEvent::StreamMetadata` SYNCHRONOUSLY on the same task that drives EOF and
//!    queue advance, so a whole-file rewrite plus `sync_all` there (what
//!    [`crate::resume::atomic_write_bytes`] does, deliberately NOT reached for here)
//!    would delay queue advance and become an AUDIBLE defect. [`HeardLedger::append`]
//!    is a NON-async fn returning `()`, so awaiting a write from the spine is
//!    type-impossible.
//! 2. [`icy_class`] / [`mark_decision`] - the pure SUBJECT rules. The one thing the
//!    mark gesture must never do is name the wrong track, because a wrong row is a
//!    lead chased tomorrow and a wrong star is a write into the library. So the daemon
//!    NEVER picks between two plausible subjects: when the ICY title is settled the
//!    subject is unambiguous; when it flipped moments ago BOTH candidates are recorded
//!    in one row and NEITHER is starred.
//! 3. [`render`] - the pure READ-BACK, including the mandatory COVERAGE line, because
//!    a thin file must read as a thin SAMPLE and never as a quiet evening.
//!
//! Everything except the task itself is pure over fixtures, so the whole module is
//! unit-testable in the certless, network-less nix sandbox.

use std::io::Write;
use std::path::{Path, PathBuf};

use tokio::sync::mpsc;

/// How long an ICY title must have been standing before the mark gesture treats it as
/// the UNAMBIGUOUS subject of the press.
///
/// This is deliberately NOT a decision threshold. A threshold ("under 15s means he
/// meant the previous track") is wrong on BOTH of its sides: too small and the press
/// confidently stars the current track when the previous one was meant, too large and
/// the reverse, and either error writes a wrong entry. An AMBIGUITY window is wrong on
/// only ONE side: enlarging it costs strictly less automatic starring and can never
/// cost a wrong entry. So it is set generously and can be tightened later from real
/// data, because every mark row records `subject_age_secs` - the measurement of actual
/// press latency that does not exist today.
pub const MARK_SETTLE_SECS: u64 = 45;

/// How long a prior recognition may stand as the subject of a mark on a stream that
/// carries NO ICY at all.
///
/// Unlike [`MARK_SETTLE_SECS`] this bounds a PHYSICAL quantity rather than guessing at
/// a human: 180s is shorter than a club edit, so a hit that recent is very likely still
/// the track playing. It also gates every read of the library-match slot, which is what
/// closes the stale-match wrong-star (a single hit used to leave `X-MatchUri` standing
/// for the whole rest of a mixtape entry).
pub const MATCH_SUBJECT_FRESH_SECS: u64 = 180;

/// The assumed track length, seconds, used ONLY by the coverage line's never-sampled
/// estimate. Four minutes is the club-edit figure the study measured against.
const COVERAGE_TRACK_SECS: f64 = 240.0;

/// Default number of ledger lines the compact view shows - the size the hand-kept
/// files actually were.
pub const HEARD_DEFAULT_LIMIT: usize = 20;

// ─────────────────────────────────────────────────────────────────────────────
// The row
// ─────────────────────────────────────────────────────────────────────────────

/// One line of the ledger: a single JSON object, newline-terminated.
///
/// EVERY field is `#[serde(default)]` and skipped when empty, so a field added later
/// still loads an old file (the upgrade-compat posture `resume.rs` proves) and a row
/// stays short. `raw` is the AUTHORITATIVE text for an ICY row - `artist`/`title` are
/// parse HINTS and are never displayed in its place, because an `Artist - Title` split
/// can be wrong while the verbatim line never is.
///
/// APPEND-ONLY CONSEQUENCE: "was it marked" is a SEPARATE `mark` row, never a flag
/// flipped on an existing row - flipping a flag means rewriting the file, which is the
/// fsync this whole design bans. The renderer joins marks to heard rows at read time.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HeardRow {
    /// RFC3339 LOCAL wall clock, the human-facing timestamp.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub at: String,
    /// Epoch seconds for the same instant. Both are stored so a clock jump between two
    /// rows is diagnosable rather than invisible.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub at_unix: u64,
    /// `session` | `heard` | `miss` | `mark`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ev: String,
    /// `icy` | `recognize` | `mark`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src: Option<String>,
    /// The resolved station label (ICY `icy-name`, a resolved station identity, or the
    /// saved-station name), never the raw URL when a real name is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub station: Option<String>,
    /// The stream URL. A URL reconstructs the listen; an ISRC is one more lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `track` | `show` | `junk` | `moment`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The VERBATIM source text (the ICY line, or the recognized "Artist - Title").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    /// Parse hint only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    /// Parse hint only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Whether a confident LIBRARY counterpart resolved (see [`crate::library_match`]).
    #[serde(default, skip_serializing_if = "is_false")]
    pub owned: bool,
    /// The owned copy's `song/<id>` uri, when one resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_uri: Option<String>,
    /// Whether THIS press actually starred something. An automatic row never stars.
    #[serde(default, skip_serializing_if = "is_false")]
    pub starred: bool,
    /// Whether the press landed inside the settle window with a previous title still
    /// standing - both candidates recorded, neither starred.
    #[serde(default, skip_serializing_if = "is_false")]
    pub ambiguous: bool,
    /// The retired ICY line, on an ambiguous or `mark previous` row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_raw: Option<String>,
    /// How long ago the retired line ended, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_ended_secs: Option<u64>,
    /// How long the marked subject had been standing when the press landed. THE
    /// measurement of real press latency, which is what lets [`MARK_SETTLE_SECS`] be
    /// tightened later from data instead of from another guess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_age_secs: Option<u64>,
    /// `no_match` | `transport` | `rate_limited` | `timeout` | `busy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Epoch seconds of the mark this recognition was kicked by, when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_mark: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub year: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isrc: Option<String>,
    /// The configured recognize interval at session open, so the coverage line can name
    /// the clock it is modelling even on a session with too few samples to derive one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<u64>,
    /// The TAPE segment id this press kept, when it kept one ([`crate::tape`]). The join
    /// is one-way and lossy BY DESIGN: the ledger's retention is about text
    /// (`keep_sessions`) and the tape's is about audio (`max_bytes`), the two are
    /// independent, and a row whose segment has been swept renders as the moment it always
    /// was, annotated that the audio is gone. A sidecar is self-contained, so the other
    /// direction reads fine too.
    ///
    /// Absent on a press that took no audio at all - a library song (he owns it), or a
    /// press whose star succeeded on a resolved counterpart (a radio rip of a track he now
    /// has starred is a worse copy with worse provenance).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tape: Option<String>,
    /// The segment's OBSERVED duration, whole seconds - the ffprobe reading, never the
    /// window that was asked for. `u64` rather than a float because [`HeardRow`] derives
    /// `Eq`, and because the filename carries the same rounded number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tape_secs: Option<u64>,
    /// How the segment's boundaries were decided (`crate::tape::Cut::as_str`): `window`,
    /// `icy-open` or `icy-edge`. The honesty label, recorded beside the pointer so the row
    /// claims exactly what the filename claims.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cut: Option<String>,
    /// Why no audio was kept, when the press tried and could not. An honest sentence
    /// beats a silently absent field: the two look identical in a file otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tape_outcome: Option<String>,
    /// Shazam's own `matches[0].offset`, MILLISECONDS, when it sent one. It has been
    /// arriving on stdout all along and serde was silently dropping it.
    ///
    /// A SEARCH WINDOW, NEVER A CUT POINT. There is no confidence field in the envelope;
    /// the number reports position in the STUDIO recording, so a DJ's pitch fader skews it
    /// linearly with elapsed time; and a track dropped in at its second chorus makes
    /// `now - offset` land in the previous track's tail. It narrows a search. It never
    /// authorises a cut and it never reaches a filename. Milliseconds as an `i64` (and
    /// signed - a NEGATIVE offset is the strong case, the track began inside our own
    /// capture) because [`HeardRow`] derives `Eq` and an `f64` cannot live on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shazam_offset_ms: Option<i64>,
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

fn is_false(v: &bool) -> bool {
    !*v
}

impl HeardRow {
    /// A row stamped NOW on both clocks, with `ev` set and everything else empty.
    /// Wall-clock, deliberately: this is a persisted CALENDAR timestamp, which the
    /// scheduling [`crate::clock::Clock`] seam cannot express (the same reasoning
    /// `store::now_unix` records). Nothing branches on it but rendering.
    pub fn now(ev: &str) -> Self {
        HeardRow {
            // `at` is deliberately left EMPTY here and formatted in the writer's
            // spawn_blocking hop (see `encode_batch`). This constructor is reachable
            // from the DIRECTOR SPINE via the ICY path, and chrono's Local::now()
            // re-reads TZ and lstats /etc/localtime whenever its per-thread cache is
            // over a second old - a syscall under the State lock, ahead of the next
            // player_events.recv(). `now_unix` is a vDSO clock_gettime, which is what
            // the spine can afford; the human-readable string costs nothing off it.
            at: String::new(),
            at_unix: crate::store::now_unix(),
            ev: ev.to_string(),
            ..Default::default()
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The write path: unbounded mpsc -> dedicated task -> O_APPEND -> spawn_blocking
// ─────────────────────────────────────────────────────────────────────────────

/// The cloneable ledger handle. Modelled 1:1 on [`crate::timer::TimerHandle`]: an
/// unbounded control channel whose send error is DISCARDED, so a dead task degrades to
/// "the row is simply not written" - never a stall, never backpressure, never a panic.
#[derive(Clone)]
pub struct HeardLedger {
    tx: mpsc::UnboundedSender<HeardRow>,
}

impl HeardLedger {
    /// Enqueue one row. NON-ASYNC and infallible by construction, which is the whole
    /// point: the director spine calls this, and a `()`-returning sync fn cannot be
    /// awaited, so no future edit can accidentally put file I/O on the spine. The cost
    /// is one lock-free channel send, the same class as the viz publish next to it.
    pub fn append(&self, row: HeardRow) {
        let _ = self.tx.send(row);
    }
}

/// Spawn the dedicated ledger task over `dir` and return its handle.
///
/// The task owns ONE `O_APPEND` file handle for the whole daemon run and exits when the
/// channel closes (all handles dropped) - the repo's only shutdown convention. Its FIRST
/// blocking hop creates the directory, sweeps retention down to `keep_sessions`, opens
/// the file and writes the `session` row; every later hop is one `write_all` of a
/// newline-terminated batch. There is NO `sync_all` anywhere, deliberately: an append of
/// one short line is not a whole-file rewrite, and the fsync in
/// [`crate::resume::atomic_write_bytes`] is exactly what must never happen near the
/// spine.
///
/// `interval_secs` is recorded on the session row purely so the coverage line can name
/// the sampling clock it models.
pub fn spawn_heard_ledger(dir: PathBuf, keep_sessions: u32, interval_secs: u64) -> HeardLedger {
    let (tx, mut rx) = mpsc::unbounded_channel::<HeardRow>();
    tokio::spawn(async move {
        let open_dir = dir.clone();
        let opened = tokio::task::spawn_blocking(move || {
            open_session(&open_dir, keep_sessions, interval_secs)
        })
        .await;
        let mut file = match opened {
            Ok(Ok(f)) => Some(f),
            Ok(Err(e)) => {
                tracing::warn!(dir = %dir.display(), error = %e, "heard ledger unavailable; listening is not recorded this session");
                None
            }
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "heard ledger open task failed; listening is not recorded this session");
                None
            }
        };
        let mut batch: Vec<HeardRow> = Vec::new();
        while let Some(first) = rx.recv().await {
            // No handle: drain and drop. The open failure was already warned once, and
            // a missing ledger must degrade the RECORD, never anything else.
            if file.is_none() {
                continue;
            }
            batch.clear();
            batch.push(first);
            // ONE blocking hop covers a burst: drain everything already queued.
            while let Ok(row) = rx.try_recv() {
                batch.push(row);
            }
            let buf = encode_batch(&batch);
            if buf.is_empty() {
                continue;
            }
            let f = match file.take() {
                Some(f) => f,
                None => continue,
            };
            match tokio::task::spawn_blocking(move || {
                let mut f = f;
                let r = f.write_all(buf.as_bytes());
                (f, r)
            })
            .await
            {
                // The handle is KEPT on a write error: a transient ENOSPC can recover,
                // and dropping it would silently end the session's recording.
                Ok((f, Ok(()))) => file = Some(f),
                Ok((f, Err(e))) => {
                    tracing::warn!(error = %e, "heard ledger append failed; the row is lost");
                    file = Some(f);
                }
                Err(e) => tracing::warn!(error = %e, "heard ledger write task failed"),
            }
        }
    });
    HeardLedger { tx }
}


/// Format unix seconds as a local RFC3339 stamp, falling back to the raw epoch.
///
/// Deliberately a free function rather than inline: it must be callable from the tape
/// commit path too, and it must NEVER be called from the director spine - chrono re-reads
/// TZ and lstats /etc/localtime when its per-thread cache is stale, which is a syscall
/// under the State lock (see `HeardRow::now`).
pub fn format_local(at_unix: u64) -> String {
    chrono::DateTime::from_timestamp(at_unix as i64, 0)
        .map(|utc| {
            utc.with_timezone(&chrono::Local)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
        })
        .unwrap_or_else(|| at_unix.to_string())
}

/// Serialize a batch into one newline-terminated buffer. A row that fails to serialize
/// is DROPPED with a warn rather than written partially, so one row is always one line
/// and the line-oriented reader cannot desynchronize. JSON escaping is what makes an ICY
/// string containing newlines or control characters safe here.
fn encode_batch(rows: &[HeardRow]) -> String {
    let mut buf = String::new();
    for row in rows {
        // Format the human-readable stamp HERE, off the spine, from the unix seconds
        // the producer captured. Falls back to the raw epoch rather than dropping the
        // row: a row with an ugly timestamp is still a row he can act on.
        let row = &{
            let mut r = row.clone();
            if r.at.is_empty() {
                r.at = format_local(r.at_unix);
            }
            r
        };
        match serde_json::to_string(row) {
            Ok(line) => {
                buf.push_str(&line);
                buf.push('\n');
            }
            Err(e) => tracing::warn!(error = %e, "heard ledger row would not serialize; dropped"),
        }
    }
    buf
}

/// The session file name: `YYYY-MM-DD-HHMM-<pid>.jsonl`.
///
/// The pid suffix makes two daemons structurally unable to interleave even if a
/// directory is shared by accident, which is also what makes an isolated test daemon
/// safe beside a real one. The STATION is deliberately not in the name: at file-open
/// time it is not reliably known (a `QueueEntry::Stream` carries only a url and a
/// display title), so a station in the filename would lie.
fn session_file_name(now: chrono::DateTime<chrono::Local>, pid: u32) -> String {
    format!("{}-{}.jsonl", now.format("%Y-%m-%d-%H%M"), pid)
}

/// Sync, using `std::fs` throughout: the caller runs it in `spawn_blocking`.
fn open_session(dir: &Path, keep_sessions: u32, interval_secs: u64) -> std::io::Result<std::fs::File> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(session_file_name(chrono::Local::now(), std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    // Retention runs BEFORE the first write, so the ledger can never grow without bound
    // in the user's home. This is the by-product the process must clean up itself.
    sweep_sessions(dir, keep_sessions);
    let mut session = HeardRow::now("session");
    session.interval_secs = Some(interval_secs);
    if let Ok(line) = serde_json::to_string(&session) {
        let _ = writeln!(f, "{line}");
    }
    Ok(f)
}

/// Keep the newest `keep` session files, remove the rest. Returns how many were
/// removed. Names sort chronologically by construction, so a lexical sort is a
/// chronological one and no `stat` per file is needed.
///
/// Sync, using `std::fs` throughout: the caller runs it in `spawn_blocking`.
pub fn sweep_sessions(dir: &Path, keep: u32) -> usize {
    let keep = keep.max(1) as usize;
    let mut names = session_files(dir);
    if names.len() <= keep {
        return 0;
    }
    let drop_count = names.len() - keep;
    names.truncate(drop_count);
    let mut removed = 0usize;
    for name in names {
        if std::fs::remove_file(dir.join(&name)).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Every ledger file name in `dir`, oldest first. Sync: `spawn_blocking` territory.
fn session_files(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".jsonl"))
        .collect();
    names.sort();
    names
}

/// The newest session file in `dir`, or `None`. Sync: `spawn_blocking` territory.
pub fn newest_session(dir: &Path) -> Option<PathBuf> {
    session_files(dir).pop().map(|n| dir.join(n))
}

/// Read every parseable row of `path`, plus the count of lines that would not parse.
///
/// UNREADABLE lines are counted, never fatal: the daemon can be killed mid-write, so a
/// torn trailing line is possible, and it must surface in the coverage line as
/// "1 unreadable row" rather than as a panic or a silently truncated render.
///
/// Sync, using `std::fs` throughout: the caller runs it in `spawn_blocking`.
pub fn read_session(path: &Path) -> (Vec<HeardRow>, usize) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (Vec::new(), 0);
    };
    parse_rows(&text)
}

/// Read the rows a `view` needs: EVERY retained session for [`HeardView::Marks`] (the
/// two-week question is a cross-session one), the newest session for the rest.
///
/// Sync, using `std::fs` throughout: the caller runs it in `spawn_blocking`.
pub fn read_for(dir: &Path, view: HeardView) -> (Vec<HeardRow>, usize) {
    match view {
        HeardView::Marks => {
            let (mut rows, mut unreadable) = (Vec::new(), 0usize);
            for name in session_files(dir) {
                let (r, u) = read_session(&dir.join(name));
                rows.extend(r);
                unreadable += u;
            }
            (rows, unreadable)
        }
        _ => match newest_session(dir) {
            Some(p) => read_session(&p),
            None => (Vec::new(), 0),
        },
    }
}

/// Parse a ledger file's text. Pure, so the whole read-back is testable on fixtures.
pub fn parse_rows(text: &str) -> (Vec<HeardRow>, usize) {
    let mut rows = Vec::new();
    let mut unreadable = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<HeardRow>(line) {
            Ok(r) => rows.push(r),
            Err(_) => unreadable += 1,
        }
    }
    (rows, unreadable)
}

// ─────────────────────────────────────────────────────────────────────────────
// ICY classification
// ─────────────────────────────────────────────────────────────────────────────

/// What an ICY `icy-title` line actually names.
///
/// CONSERVATIVE BY CONSTRUCTION: the default is [`IcyClass::Track`] and only a POSITIVE
/// signal demotes. That direction is load-bearing - a misclassification then degrades
/// toward today's behaviour (a title treated as a track, which is what happens now)
/// rather than toward hammering the recognition endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IcyClass {
    /// A real track line: "Artist - Title".
    Track,
    /// A SHOW or programme heading, not a track ("NTS 2 - KIM LANA (R)",
    /// "Ken Sekiguchi - Moon Mission Recordings Show Vol.34").
    Show,
    /// A placeholder that names nothing ("Airtime - offline", a bare station id).
    Junk,
}

impl IcyClass {
    /// The ledger `kind` word.
    pub fn as_str(self) -> &'static str {
        match self {
            IcyClass::Track => "track",
            IcyClass::Show => "show",
            IcyClass::Junk => "junk",
        }
    }
}

/// Placeholder substrings that name nothing at all. Matched case-insensitively on the
/// whole line.
const JUNK_MARKERS: &[&str] = &[
    "offline",
    "unknown",
    "no title",
    "nothing playing",
    "advertisement",
    "station id",
];

/// Classify one ICY line against the station it came from. PURE.
pub fn icy_class(title: &str, station: Option<&str>) -> IcyClass {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return IcyClass::Junk;
    }
    let lower = trimmed.to_lowercase();
    if JUNK_MARKERS.iter().any(|m| lower.contains(m)) {
        return IcyClass::Junk;
    }
    if let Some(station) = station.map(str::trim).filter(|s| !s.is_empty()) {
        let s_lower = station.to_lowercase();
        // The line IS the station name: a bare station id, nothing heard.
        if lower == s_lower {
            return IcyClass::Junk;
        }
        // The station name PREFIXES the line ("NTS 2 - KIM LANA (R)"): a programme
        // heading the station stamped, never a track credit.
        if let Some(rest) = lower.strip_prefix(&s_lower) {
            if rest.trim_start().starts_with('-') || rest.trim_start().starts_with(':') {
                return IcyClass::Show;
            }
        }
    }
    if has_volume_marker(&lower) || lower.contains("radio show") || lower.contains("recordings show")
    {
        return IcyClass::Show;
    }
    // A standalone "(R)" repeat marker, as NTS stamps on a rebroadcast.
    if lower.contains("(r)") {
        return IcyClass::Show;
    }
    IcyClass::Track
}

/// Does `lower` carry a `vol.<n>` / `vol <n>` episode marker? Requires the DIGIT, so an
/// ordinary word starting with "vol" (a title containing "volume of noise") does not
/// demote a real track.
fn has_volume_marker(lower: &str) -> bool {
    for (idx, _) in lower.match_indices("vol") {
        let rest = &lower[idx + 3..];
        let rest = rest.strip_prefix('.').unwrap_or(rest);
        let rest = rest.strip_prefix("ume").unwrap_or(rest);
        let rest = rest.trim_start();
        if rest.starts_with(|c: char| c.is_ascii_digit()) {
            return true;
        }
    }
    false
}

/// Split an ICY "Artist - Title" line into its halves on the FIRST " - ". Both are
/// hints: a hyphen inside a title, or a "Label - Artist - Title" shape, yields a wrong
/// split, which is exactly why the verbatim line stays authoritative in the ledger and
/// why the matcher's exact-title bar is what protects the star (a bad split yields NO
/// match, never a wrong one).
/// The separator is the SPACED " - ", never a bare hyphen: a bare hyphen would cut
/// "Jean-Michel Jarre" in half and hand the matcher a credit that names nobody.
pub fn split_icy_title(raw: &str) -> (Option<String>, Option<String>) {
    let raw = raw.trim();
    match raw.split_once(" - ") {
        Some((a, t)) => {
            let (a, t) = (a.trim(), t.trim());
            // Neither half may be blank; a degenerate split yields the verbatim line as
            // the title rather than an artist that is the empty string.
            if a.is_empty() || t.is_empty() {
                (None, Some(raw.to_string()))
            } else {
                (Some(a.to_string()), Some(t.to_string()))
            }
        }
        // No separator at all: the whole line is the title half, nothing is invented.
        None => (None, Some(raw.to_string())),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The mark decision
// ─────────────────────────────────────────────────────────────────────────────

/// Which subject the press explicitly asked for. `Auto` is the bare `mark`; the other
/// two are the explicit words that resolve an ambiguous press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkTarget {
    /// Decide from the stream shape, refusing to guess when it is genuinely undecidable.
    Auto,
    /// The CURRENT title, whatever its age.
    This,
    /// The RETIRED title.
    Previous,
}

/// What is on the deck, as far as the mark gesture cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkEntry {
    /// A library song: it already names itself, so the press stars it outright.
    Song,
    /// A raw stream.
    Stream,
}

/// The snapshot the pure decision runs over, read under ONE state lock and then
/// released - the decision itself touches no lock, no clock and no network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkInput {
    pub target: MarkTarget,
    /// `None` when nothing is playing.
    pub entry: Option<MarkEntry>,
    /// The resolved station label, when one is known.
    pub station: Option<String>,
    /// The live STATION-announced title for this entry, when non-blank. A line the
    /// daemon recognized itself is NOT this - it arrives as [`Self::fresh_match`],
    /// because its age means the opposite thing (see the handler's `TitleSource`).
    pub icy_title: Option<String>,
    /// How long that title has been standing.
    pub icy_age_secs: u64,
    /// The retired title and how long ago it ended.
    pub prev_icy: Option<(String, u64)>,
    /// A prior RECOGNITION (its title, its age). Bounded by [`Self::fresh_secs`] here
    /// rather than trusted: nothing re-confirms a recognized name, so it decays.
    pub fresh_match: Option<(String, u64)>,
    /// [`MARK_SETTLE_SECS`], injected so the boundary is testable without a clock.
    pub settle_secs: u64,
    /// [`MATCH_SUBJECT_FRESH_SECS`], likewise.
    pub fresh_secs: u64,
}

/// What the press resolved to. Exactly one of these carries a starrable subject on a
/// stream, and no input can produce a star without an unambiguous one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkSubject {
    /// Nothing on the deck.
    Nothing,
    /// A library song: star it.
    Song,
    /// The settled ICY title names the subject.
    Icy { raw: String, age_secs: u64 },
    /// The title flipped inside the settle window: BOTH candidates recorded, NEITHER
    /// starred, and the reply offers the two explicit follow-up words.
    Ambiguous {
        raw: String,
        age_secs: u64,
        prev_raw: String,
        prev_ended_secs: u64,
    },
    /// The retired title, named explicitly by `mark previous`.
    Previous { raw: String, ended_secs: u64 },
    /// The ICY line is a show heading or a placeholder: never a track subject. With a
    /// timestamp and a station this still reconstructs a tracklist tomorrow, which is
    /// the mechanism that actually converted.
    Show { raw: String, class: IcyClass },
    /// No ICY, but a recognition young enough to still be what is playing.
    FreshMatch { names: String, age_secs: u64 },
    /// No ICY and nothing fresh: a timestamped, archive-recoverable pointer, and one
    /// kicked recognition. This is the case that stays honestly UNSOLVED.
    Moment,
    /// `mark previous` with nothing retired.
    NoPrevious,
}

/// Resolve the subject of a press. PURE, total, and the one rule it enforces
/// structurally is that no input yields an automatic star without an unambiguous
/// subject.
pub fn mark_decision(input: &MarkInput) -> MarkSubject {
    let Some(entry) = &input.entry else {
        return MarkSubject::Nothing;
    };
    if matches!(entry, MarkEntry::Song) {
        return MarkSubject::Song;
    }
    // `mark previous` is explicit: it names the retired line or honestly says there is
    // none. It never falls back to the current title.
    if input.target == MarkTarget::Previous {
        return match &input.prev_icy {
            Some((raw, ended)) => MarkSubject::Previous {
                raw: raw.clone(),
                ended_secs: *ended,
            },
            None => MarkSubject::NoPrevious,
        };
    }

    let live = input
        .icy_title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());

    if let Some(raw) = live {
        let class = icy_class(raw, input.station.as_deref());
        if class != IcyClass::Track {
            return MarkSubject::Show {
                raw: raw.to_string(),
                class,
            };
        }
        // `mark this` forces the current title even inside the window: the human
        // resolved the ambiguity, so the daemon stops hedging.
        if input.target == MarkTarget::This || input.icy_age_secs >= input.settle_secs {
            return MarkSubject::Icy {
                raw: raw.to_string(),
                age_secs: input.icy_age_secs,
            };
        }
        // Inside the window WITH a retired candidate: genuinely undecidable, so record
        // both and star neither. An ambiguous row is not a wrong row; a coin-flip is.
        if let Some((prev_raw, prev_ended)) = &input.prev_icy {
            return MarkSubject::Ambiguous {
                raw: raw.to_string(),
                age_secs: input.icy_age_secs,
                prev_raw: prev_raw.clone(),
                prev_ended_secs: *prev_ended,
            };
        }
        // Inside the window with NOTHING retired: there is only one candidate, so it is
        // not ambiguous - the stream simply started.
        return MarkSubject::Icy {
            raw: raw.to_string(),
            age_secs: input.icy_age_secs,
        };
    }

    // A RECOGNITION names what was on air when it resolved, and unlike a station line
    // nothing ever re-asserts or retracts it - so it is a subject only while it is young.
    // Past the bound it is not "no station line plus a name", it is simply no name, and
    // the press stays honestly unsolved rather than starring a track that already ended.
    match &input.fresh_match {
        Some((names, age)) if *age < input.fresh_secs => MarkSubject::FreshMatch {
            names: names.clone(),
            age_secs: *age,
        },
        _ => MarkSubject::Moment,
    }
}

/// Render a duration as a compact human span ("2m", "4h12m").
pub fn human_secs(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let (h, m) = (mins / 60, mins % 60);
    if m == 0 {
        format!("{h}h")
    } else {
        format!("{h}h{m}m")
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Read-back
// ─────────────────────────────────────────────────────────────────────────────

/// Which view of the ledger to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeardView {
    /// Last session, marks at the top, unowned only, deduped, capped.
    Recent,
    /// Every row, owned included and tagged, no cap, no dedupe.
    All,
    /// Mark rows only, oldest first - the two-week answer to whether the CHOOSING is
    /// the conversion step.
    Marks,
}

/// What a `heard` request ASKS FOR, as opposed to which view it renders.
///
/// Two tokens on the EXISTING verb rather than a new one, which is what keeps
/// `MpdCommand` at zero new variants and `ADVERTISED_MPD_VERSION` untouched by
/// construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeardAction {
    /// Read the ledger back (every view).
    Render,
    /// Pin tape segment `n` (1-based, in the tape's own chronological order - the same
    /// number the render prints beside a taped row) against eviction, or unpin it.
    Keep { n: usize, on: bool },
}

/// A parsed `heard` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeardQuery {
    pub view: HeardView,
    /// Row cap for [`HeardView::Recent`]; ignored by the other views.
    pub limit: usize,
    /// The window over which repeated titles collapse, seconds. Adjacency is NOT
    /// enough: real duplicates are interleaved A,B,A,B, which consecutive-dedupe
    /// suppresses none of.
    pub dedupe_window_secs: u64,
    /// What the request asks for; [`HeardAction::Render`] for every read-back form.
    pub action: HeardAction,
}

impl Default for HeardQuery {
    fn default() -> Self {
        HeardQuery {
            view: HeardView::Recent,
            limit: HEARD_DEFAULT_LIMIT,
            dedupe_window_secs: 1800,
            action: HeardAction::Render,
        }
    }
}

/// The `HH:MM` of a row, from its RFC3339 local stamp. A missing or reshaped stamp
/// yields a visible placeholder rather than a panic or a fabricated time.
fn hhmm(row: &HeardRow) -> String {
    let b = row.at.as_bytes();
    if b.len() >= 16 && b[10] == b'T' && b[13] == b':' {
        return row.at[11..16].to_string();
    }
    "??:??".to_string()
}

/// The collapse key for the render-time dedupe: normalized artist + title, falling back
/// to the verbatim line. Lowercase, punctuation flattened, whitespace collapsed.
fn dedupe_key(row: &HeardRow) -> String {
    let text = match (&row.artist, &row.title) {
        (Some(a), Some(t)) => format!("{a} {t}"),
        _ => row
            .raw
            .clone()
            .or_else(|| row.title.clone())
            .unwrap_or_default(),
    };
    let mut out = String::with_capacity(text.len());
    let mut space = true;
    for c in text.chars() {
        if c.is_alphanumeric() {
            for l in c.to_lowercase() {
                out.push(l);
            }
            space = false;
        } else if !space {
            out.push(' ');
            space = true;
        }
    }
    out.trim_end().to_string()
}

/// The display text of a row: the VERBATIM line wins for an ICY row, because an
/// `Artist - Title` split can be wrong while the raw line never is.
fn row_text(row: &HeardRow) -> String {
    row.raw
        .clone()
        .or_else(|| match (&row.artist, &row.title) {
            (Some(a), Some(t)) => Some(format!("{a} - {t}")),
            (None, Some(t)) => Some(t.clone()),
            (Some(a), None) => Some(a.clone()),
            (None, None) => None,
        })
        .unwrap_or_else(|| "(no title)".to_string())
}

/// The COVERAGE line, and it is mandatory rather than decorative.
///
/// A blind periodic clock never samples a large share of tracks at all, and those
/// produce no hit line and no miss line - no trace whatsoever. So a short file next
/// morning is indistinguishable from a quiet evening unless the render says out loud
/// how thin the sample is, with its model and inputs printed so the claim cannot
/// silently rot.
///
/// THE MODEL, corrected. The often-quoted ~45% is the MEMORYLESS model,
/// `e^(-L/I) = e^(-0.8) = 0.449`. The daemon does not run a memoryless clock: the
/// cadence is PERIODIC (a fixed rearm, doubling on misses), whose never-sampled
/// fraction is `1 - L/I` for `I > L` - 20% at I=300 and 60% at I=600. The periodic
/// figure is therefore the correct one, and note that the flat 600s content cap makes
/// the true never-sampled fraction WORSE, not better, which strengthens rather than
/// weakens the case that a thin file must read as thin.
///
/// It is NEVER a hit rate: with no boundary signal the fire count is uncorrelated with
/// the track count, so it measures SAMPLER YIELD, and the wording says so.
fn coverage_line(rows: &[HeardRow], unreadable: usize) -> String {
    let recognize: Vec<&HeardRow> = rows
        .iter()
        .filter(|r| r.src.as_deref() == Some("recognize"))
        .collect();
    let icy_rows = rows.iter().filter(|r| r.src.as_deref() == Some("icy")).count();
    let stations: Vec<String> = {
        let mut v: Vec<String> = rows
            .iter()
            .filter_map(|r| r.station.clone())
            .filter(|s| !s.trim().is_empty())
            .collect();
        v.sort();
        v.dedup();
        v
    };
    let station_txt = match stations.len() {
        0 => String::new(),
        1 => format!(" on {}", stations[0]),
        n => format!(" across {n} stations"),
    };
    let span = {
        let stamps: Vec<u64> = rows.iter().map(|r| r.at_unix).filter(|u| *u > 0).collect();
        match (stamps.iter().min(), stamps.iter().max()) {
            (Some(lo), Some(hi)) => hi.saturating_sub(*lo),
            _ => 0,
        }
    };
    let unreadable_txt = if unreadable == 0 {
        String::new()
    } else if unreadable == 1 {
        ", 1 unreadable row".to_string()
    } else {
        format!(", {unreadable} unreadable rows")
    };

    if recognize.is_empty() {
        if icy_rows > 0 {
            return format!(
                "coverage: ICY named this station directly over {}{station_txt}; the recognizer never ran{unreadable_txt}",
                human_secs(span)
            );
        }
        return format!("coverage: nothing sampled yet{station_txt}{unreadable_txt}");
    }

    let named = recognize.iter().filter(|r| r.ev == "heard").count();
    let count = |o: &str| {
        recognize
            .iter()
            .filter(|r| r.outcome.as_deref() == Some(o))
            .count()
    };
    let (no_match, transport, timeout, busy) = (
        count("no_match"),
        count("transport") + count("rate_limited"),
        count("timeout"),
        count("busy"),
    );

    // The EFFECTIVE interval, derived from the file's own stamps when there are enough
    // samples to derive one, else the configured value from the session row. Printed
    // either way so the estimate can be checked rather than trusted.
    let configured = rows.iter().find_map(|r| r.interval_secs);
    let interval = if recognize.len() >= 2 {
        let lo = recognize.iter().map(|r| r.at_unix).min().unwrap_or(0);
        let hi = recognize.iter().map(|r| r.at_unix).max().unwrap_or(0);
        let gaps = (recognize.len() - 1) as u64;
        let derived = hi.saturating_sub(lo) / gaps.max(1);
        if derived > 0 {
            derived
        } else {
            configured.unwrap_or(crate::config::DEFAULT_RECOGNIZE_INTERVAL_SECS)
        }
    } else {
        configured.unwrap_or(crate::config::DEFAULT_RECOGNIZE_INTERVAL_SECS)
    };
    let missed_pct = never_sampled_pct(interval, COVERAGE_TRACK_SECS);

    format!(
        "coverage: sampled {} times over {}{station_txt} - {named} named, {no_match} no-match, {transport} network, {timeout} timeout, {busy} skipped{unreadable_txt}; this is SAMPLER YIELD, not a hit rate. A periodic {interval}s clock never samples about {missed_pct}% of {}s tracks (periodic model 1 - L/I, L={} assumed)",
        recognize.len(),
        human_secs(span),
        COVERAGE_TRACK_SECS as u64,
        COVERAGE_TRACK_SECS as u64,
    )
}

/// The PERIODIC never-sampled fraction, `1 - L/I` for `I > L`, as a whole percent.
/// Zero when the clock is at least as fast as the track length (every track is caught).
fn never_sampled_pct(interval_secs: u64, track_secs: f64) -> u64 {
    if interval_secs == 0 || track_secs <= 0.0 {
        return 0;
    }
    let i = interval_secs as f64;
    if i <= track_secs {
        return 0;
    }
    (((1.0 - track_secs / i) * 100.0).round()).max(0.0) as u64
}

/// Render the ledger for a human. PURE over the rows, so every view is unit-testable
/// on fixtures and runs unconditionally in the sandbox.
///
/// `tape_ids` is every segment currently ON DISK, oldest first (`tape::segment_ids`). It
/// is the SINGLE source of the `[n]` numbering a taped row prints and `heard keep <n>`
/// resolves against, so the two can never disagree - and a row whose segment has since
/// been swept simply finds no index and says the audio is gone, which is the honest half
/// of a rolling cache rather than a defect.
pub fn render(
    rows: &[HeardRow],
    unreadable: usize,
    q: &HeardQuery,
    tape_ids: &[String],
) -> Vec<String> {
    match q.view {
        HeardView::Marks => render_marks(rows, tape_ids),
        HeardView::All => render_all(rows, unreadable, tape_ids),
        HeardView::Recent => render_recent(rows, unreadable, q, tape_ids),
    }
}

fn render_marks(rows: &[HeardRow], tape_ids: &[String]) -> Vec<String> {
    let marks: Vec<&HeardRow> = rows.iter().filter(|r| r.ev == "mark").collect();
    if marks.is_empty() {
        return vec!["no marks recorded yet".to_string()];
    }
    let mut out = vec![format!("{} marks, oldest first", marks.len())];
    for r in marks {
        out.push(mark_line(r, tape_ids));
    }
    out
}

fn render_all(rows: &[HeardRow], unreadable: usize, tape_ids: &[String]) -> Vec<String> {
    let mut out = vec![coverage_line(rows, unreadable)];
    for r in rows.iter().filter(|r| r.ev != "session") {
        out.push(match r.ev.as_str() {
            "mark" => mark_line(r, tape_ids),
            "miss" => format!(
                "{}  [{}] {}",
                hhmm(r),
                r.outcome.as_deref().unwrap_or("miss"),
                r.station.as_deref().unwrap_or("")
            ),
            _ => heard_line(r, true),
        });
    }
    out
}

fn render_recent(
    rows: &[HeardRow],
    unreadable: usize,
    q: &HeardQuery,
    tape_ids: &[String],
) -> Vec<String> {
    let mut out = vec![coverage_line(rows, unreadable)];

    // MARKS first and visually separated: a press is an event and is NEVER deduped.
    let mut marks: Vec<&HeardRow> = rows.iter().filter(|r| r.ev == "mark").collect();
    marks.reverse();
    if !marks.is_empty() {
        out.push(format!("marked ({})", marks.len()));
        for r in &marks {
            out.push(mark_line(r, tape_ids));
        }
        out.push(String::new());
    }

    // Then what was heard: unowned first (the owned ones collapse to a count), newest
    // first, collapsed over the dedupe WINDOW rather than by adjacency.
    let heard: Vec<&HeardRow> = rows
        .iter()
        .filter(|r| r.ev == "heard" && r.kind.as_deref() != Some("junk"))
        .collect();
    let owned = heard.iter().filter(|r| r.owned).count();
    let mut unowned: Vec<&HeardRow> = heard.into_iter().filter(|r| !r.owned).collect();
    unowned.reverse();

    let mut seen: Vec<(String, u64)> = Vec::new();
    let mut shown = 0usize;
    let mut collapsed = 0usize;
    for r in unowned {
        let key = dedupe_key(r);
        if !key.is_empty() {
            if let Some((_, at)) = seen
                .iter()
                .find(|(k, at)| k == &key && at.abs_diff(r.at_unix) <= q.dedupe_window_secs)
            {
                let _ = at;
                collapsed += 1;
                continue;
            }
            seen.push((key, r.at_unix));
        }
        if shown >= q.limit {
            collapsed += 1;
            continue;
        }
        out.push(heard_line(r, false));
        shown += 1;
    }
    if shown == 0 {
        out.push("nothing unowned recorded this session".to_string());
    }
    if collapsed > 0 {
        out.push(format!("+{collapsed} more (repeats and beyond the cap)"));
    }
    if owned > 0 {
        out.push(format!("+{owned} you already own"));
    }
    out
}

fn heard_line(row: &HeardRow, tag_owned: bool) -> String {
    let mut line = format!("{}  {}", hhmm(row), row_text(row));
    if let Some(station) = row.station.as_deref().filter(|s| !s.trim().is_empty()) {
        line.push_str(&format!("  ({station})"));
    }
    if tag_owned && row.owned {
        line.push_str("  [owned]");
    }
    if let Some(url) = row.url.as_deref().filter(|u| !u.trim().is_empty()) {
        line.push_str(&format!("  {url}"));
    }
    line
}

/// The 1-based position of `id` in the tape's own chronological order, or `None` when the
/// segment is no longer on disk. The one place the `[n]` numbering is derived, shared by
/// the render and by `heard keep <n>`.
pub fn tape_index(tape_ids: &[String], id: &str) -> Option<usize> {
    tape_ids.iter().position(|s| s == id).map(|i| i + 1)
}

fn mark_line(row: &HeardRow, tape_ids: &[String]) -> String {
    let mut line = format!("{}  * {}", hhmm(row), row_text(row));
    if row.ambiguous {
        if let Some(prev) = &row.prev_raw {
            line.push_str(&format!("  OR  {prev}"));
        }
        line.push_str("  [unresolved]");
    }
    if row.starred {
        line.push_str("  [starred]");
    } else if row.owned {
        line.push_str("  [owned]");
    }
    if let Some(station) = row.station.as_deref().filter(|s| !s.trim().is_empty()) {
        line.push_str(&format!("  ({station})"));
    }
    if let Some(url) = row.url.as_deref().filter(|u| !u.trim().is_empty()) {
        line.push_str(&format!("  {url}"));
    }
    // The audio, last, and only ever as much as is TRUE. A segment still on disk gets its
    // number, its observed duration and its cut label; one that has been swept says so
    // rather than printing a number that resolves to nothing.
    if let Some(id) = row.tape.as_deref().filter(|t| !t.trim().is_empty()) {
        let secs = row.tape_secs.map(human_secs).unwrap_or_else(|| "?".to_string());
        let cut = row.cut.as_deref().unwrap_or("window");
        match tape_index(tape_ids, id) {
            Some(n) => line.push_str(&format!("  [tape {n}: {secs}, {cut}]")),
            None => line.push_str("  [tape swept]"),
        }
    } else if let Some(why) = row.tape_outcome.as_deref().filter(|w| !w.trim().is_empty()) {
        line.push_str(&format!("  [no tape: {why}]"));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    // The spine constructor must do NO timezone work. chrono's Local::now() re-reads TZ
    // and lstats /etc/localtime whenever its per-thread cache is over a second old, and
    // HeardRow::now is reachable from director.rs's synchronous StreamMetadata handler
    // with the State lock held. The cheap unix stamp is taken there; the human string is
    // formatted in the writer's spawn_blocking hop.
    #[test]
    fn the_spine_constructor_takes_no_wall_clock_string() {
        let row = HeardRow::now("heard");
        assert!(
            row.at.is_empty(),
            "HeardRow::now must not format a local timestamp - that is an lstat on the \
             director spine under the State lock; got {:?}",
            row.at
        );
        assert!(row.at_unix > 1_000_000_000, "but it does stamp the cheap clock");
    }

    #[test]
    fn the_writer_fills_in_the_timestamp_it_was_handed() {
        let mut row = HeardRow::now("heard");
        row.at_unix = 1_754_000_000;
        let line = encode_batch(std::slice::from_ref(&row));
        assert!(line.ends_with('\n'), "one row is one line");
        let parsed: serde_json::Value =
            serde_json::from_str(line.trim()).expect("valid json");
        let at = parsed["at"].as_str().unwrap_or_default();
        assert!(!at.is_empty(), "the writer formats what the spine left empty: {line}");
        assert!(at.starts_with("2025-") || at.starts_with("2026-"), "a real date: {at}");
    }

    /// A fresh, uniquely named temp dir for one test. No `tempfile` dependency
    /// (uniqueness from pid + a process-wide counter), mirroring `resume.rs`.
    fn test_dir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "hypodj-heard-{tag}-{}-{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    /// Every file name in `dir`, sorted - so a test can assert that NOTHING but the
    /// ledger files exist (the no-temp-litter bar that proves `atomic_write_bytes`,
    /// which writes a sibling temp, was not reached for).
    fn dir_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Yield until `pred` holds or the budget runs out. A ledger write hops through
    /// `spawn_blocking`, and a blocking-pool thread's completion is a REAL-thread event
    /// that no amount of virtual time can produce - this is scheduling slack for the
    /// harness, NOT time-based logic (the same rationale `store.rs` records). The
    /// ledger task itself has no cadence: it is purely event-driven, so there is no
    /// timer here to fake-clock.
    async fn settle_until(tag: &str, mut pred: impl FnMut() -> bool) {
        for _ in 0..2000 {
            if pred() {
                return;
            }
            tokio::task::yield_now().await;
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
        panic!("settle_until({tag}) never converged");
    }

    fn row(at_unix: u64, ev: &str, raw: &str) -> HeardRow {
        HeardRow {
            at: format!(
                "2026-08-04T{:02}:{:02}:00+01:00",
                (at_unix / 3600) % 24,
                (at_unix / 60) % 60
            ),
            at_unix,
            ev: ev.to_string(),
            src: Some("icy".to_string()),
            station: Some("NTS 2".to_string()),
            kind: Some("track".to_string()),
            raw: Some(raw.to_string()),
            ..Default::default()
        }
    }

    // ── icy_class ────────────────────────────────────────────────────────────

    #[test]
    fn icy_class_demotes_only_on_a_positive_signal() {
        // His own recorded strings, verbatim.
        assert_eq!(icy_class("Airtime - offline", Some("NTS 2")), IcyClass::Junk);
        assert_eq!(icy_class("NTS 2 - KIM LANA (R)", Some("NTS 2")), IcyClass::Show);
        assert_eq!(
            icy_class("Ken Sekiguchi - Moon Mission Recordings Show Vol.34", None),
            IcyClass::Show
        );
        assert_eq!(
            icy_class("Yelle - Qui est cette fille?", Some("Modular Station")),
            IcyClass::Track
        );
        // A bare station id names nothing.
        assert_eq!(icy_class("NTS 2", Some("NTS 2")), IcyClass::Junk);
        assert_eq!(icy_class("   ", None), IcyClass::Junk);
    }

    #[test]
    fn icy_class_defaults_to_track_for_anything_unmarked() {
        // THE DIRECTION PROPERTY: with no positive junk/show signal a line is a Track
        // even when it contains hyphens, parentheses or the letters "vol". A
        // misclassification must degrade toward today's behaviour, never toward
        // treating real tracks as headings.
        for t in [
            "Aphex Twin - Xtal",
            "Company - Second Hand (Original Mix)",
            "Volumes - Erased",
            "The Show - Someone",
            "A - B - C",
        ] {
            assert_eq!(icy_class(t, Some("Modular Station")), IcyClass::Track, "{t}");
        }
    }

    #[test]
    fn split_icy_title_is_a_hint_not_an_authority() {
        assert_eq!(
            split_icy_title("Yelle - Qui est cette fille?"),
            (Some("Yelle".into()), Some("Qui est cette fille?".into()))
        );
        // No separator: everything is the title half, nothing is invented.
        assert_eq!(split_icy_title("Untitled"), (None, Some("Untitled".into())));
        // THE SPACED separator only: a bare hyphen would cut this artist in half and
        // hand the matcher a credit that names nobody.
        assert_eq!(
            split_icy_title("Jean-Michel Jarre - Oxygene Part 4"),
            (Some("Jean-Michel Jarre".into()), Some("Oxygene Part 4".into()))
        );
        // The FIRST separator wins, so a "Label - Artist - Title" shape keeps the tail
        // whole rather than dropping it.
        assert_eq!(
            split_icy_title("Warp - Aphex Twin - Xtal"),
            (Some("Warp".into()), Some("Aphex Twin - Xtal".into()))
        );
    }

    // ── mark_decision ────────────────────────────────────────────────────────

    fn stream_input(target: MarkTarget) -> MarkInput {
        MarkInput {
            target,
            entry: Some(MarkEntry::Stream),
            station: Some("Modular Station".to_string()),
            icy_title: None,
            icy_age_secs: 0,
            prev_icy: None,
            fresh_match: None,
            settle_secs: MARK_SETTLE_SECS,
            fresh_secs: MATCH_SUBJECT_FRESH_SECS,
        }
    }

    #[test]
    fn mark_never_stars_without_an_unambiguous_subject() {
        // THE invariant of the whole design, as a truth table: no combination of
        // (prev present/absent) x (age below/at/above settle) x class produces a
        // starrable subject unless the subject is unambiguous.
        for prev in [None, Some(("Old - Track".to_string(), 4u64))] {
            for age in [0u64, MARK_SETTLE_SECS - 1, MARK_SETTLE_SECS, 600] {
                for title in [
                    "New - Track",
                    "Airtime - offline",
                    "Modular Station - Show Vol.2",
                ] {
                    let mut i = stream_input(MarkTarget::Auto);
                    i.icy_title = Some(title.to_string());
                    i.icy_age_secs = age;
                    i.prev_icy = prev.clone();
                    let got = mark_decision(&i);
                    let starrable = matches!(got, MarkSubject::Icy { .. });
                    let settled = age >= MARK_SETTLE_SECS || prev.is_none();
                    let is_track = icy_class(title, i.station.as_deref()) == IcyClass::Track;
                    assert_eq!(
                        starrable,
                        settled && is_track,
                        "title={title} age={age} prev={prev:?} got={got:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn mark_inside_the_window_with_a_retired_title_records_both_and_stars_neither() {
        let mut i = stream_input(MarkTarget::Auto);
        i.icy_title = Some("New - Track".to_string());
        i.icy_age_secs = 6;
        i.prev_icy = Some(("Old - Track".to_string(), 4));
        assert_eq!(
            mark_decision(&i),
            MarkSubject::Ambiguous {
                raw: "New - Track".into(),
                age_secs: 6,
                prev_raw: "Old - Track".into(),
                prev_ended_secs: 4,
            }
        );
        // EXACTLY at the boundary the subject is settled, not ambiguous.
        i.icy_age_secs = MARK_SETTLE_SECS;
        assert!(matches!(mark_decision(&i), MarkSubject::Icy { .. }));
        // One below it is still ambiguous.
        i.icy_age_secs = MARK_SETTLE_SECS - 1;
        assert!(matches!(mark_decision(&i), MarkSubject::Ambiguous { .. }));
    }

    #[test]
    fn a_recognition_is_a_subject_only_while_it_is_young() {
        // A station line is RE-ASSERTED by the station, so age is evidence it settled. A
        // recognition is asserted once and never again, so age is evidence it decayed.
        // Without this bound a hit at 20:00 was still a starrable subject at 21:30.
        let mut i = stream_input(MarkTarget::Auto);
        i.fresh_match = Some(("Takuya Nakamura - Ancient Reflection".to_string(), 0));
        assert!(matches!(mark_decision(&i), MarkSubject::FreshMatch { .. }));

        // One below the bound it still stands; AT the bound it is gone, and the press
        // stays honestly unsolved rather than naming a track that already ended.
        i.fresh_match = Some(("Takuya Nakamura - Ancient Reflection".to_string(), MATCH_SUBJECT_FRESH_SECS - 1));
        assert!(matches!(mark_decision(&i), MarkSubject::FreshMatch { .. }));
        i.fresh_match = Some(("Takuya Nakamura - Ancient Reflection".to_string(), MATCH_SUBJECT_FRESH_SECS));
        assert_eq!(mark_decision(&i), MarkSubject::Moment);
        i.fresh_match = Some(("Takuya Nakamura - Ancient Reflection".to_string(), 3600));
        assert_eq!(mark_decision(&i), MarkSubject::Moment);

        // And an explicit `mark this` cannot re-open it: it resolves an ambiguity between
        // two live candidates, it does not make a decayed name current again.
        i.target = MarkTarget::This;
        assert_eq!(mark_decision(&i), MarkSubject::Moment);
    }

    #[test]
    fn mark_this_and_previous_resolve_an_ambiguous_press_explicitly() {
        let mut i = stream_input(MarkTarget::This);
        i.icy_title = Some("New - Track".to_string());
        i.icy_age_secs = 2;
        i.prev_icy = Some(("Old - Track".to_string(), 3));
        assert!(matches!(mark_decision(&i), MarkSubject::Icy { .. }));

        i.target = MarkTarget::Previous;
        assert_eq!(
            mark_decision(&i),
            MarkSubject::Previous { raw: "Old - Track".into(), ended_secs: 3 }
        );

        // `mark previous` with nothing retired says so; it never falls back to the
        // current title, which would be the wrong subject on purpose.
        i.prev_icy = None;
        assert_eq!(mark_decision(&i), MarkSubject::NoPrevious);
    }

    #[test]
    fn mark_without_icy_prefers_a_fresh_match_then_an_honest_moment() {
        let mut i = stream_input(MarkTarget::Auto);
        i.fresh_match = Some(("Artist - Title".to_string(), 120));
        assert_eq!(
            mark_decision(&i),
            MarkSubject::FreshMatch { names: "Artist - Title".into(), age_secs: 120 }
        );
        i.fresh_match = None;
        assert_eq!(mark_decision(&i), MarkSubject::Moment);
    }

    #[test]
    fn mark_on_a_song_and_on_nothing() {
        let mut i = stream_input(MarkTarget::Auto);
        i.entry = Some(MarkEntry::Song);
        assert_eq!(mark_decision(&i), MarkSubject::Song);
        i.entry = None;
        assert_eq!(mark_decision(&i), MarkSubject::Nothing);
    }

    // ── coverage + render ────────────────────────────────────────────────────

    #[test]
    fn never_sampled_uses_the_periodic_model_not_the_memoryless_one() {
        // The memoryless model gives e^(-240/300) = 45%, which is the figure that does
        // not reproduce. The daemon's cadence is PERIODIC, so 1 - L/I is the right one.
        assert_eq!(never_sampled_pct(300, 240.0), 20);
        assert_eq!(never_sampled_pct(600, 240.0), 60);
        // A clock at least as fast as the track length misses nothing, and never goes
        // negative.
        assert_eq!(never_sampled_pct(240, 240.0), 0);
        assert_eq!(never_sampled_pct(60, 240.0), 0);
        assert_eq!(never_sampled_pct(0, 240.0), 0);
    }

    #[test]
    fn coverage_says_icy_named_it_when_the_recognizer_never_ran() {
        // A shape-A success must NOT render as a failure.
        let rows = vec![row(1000, "heard", "A - B"), row(1300, "heard", "C - D")];
        let line = coverage_line(&rows, 0);
        assert!(line.contains("ICY named this station directly"), "{line}");
        assert!(line.contains("the recognizer never ran"), "{line}");
    }

    #[test]
    fn coverage_prints_its_own_inputs_and_counts_unreadable_rows() {
        let mut rows = vec![HeardRow {
            at_unix: 0,
            ev: "session".into(),
            interval_secs: Some(600),
            ..Default::default()
        }];
        for (i, outcome) in ["no_match", "no_match", "transport", "timeout"].iter().enumerate() {
            rows.push(HeardRow {
                at_unix: 600 * (i as u64 + 1),
                ev: "miss".into(),
                src: Some("recognize".into()),
                station: Some("NTS 2".into()),
                outcome: Some((*outcome).to_string()),
                ..Default::default()
            });
        }
        let line = coverage_line(&rows, 1);
        assert!(line.contains("sampled 4 times"), "{line}");
        assert!(line.contains("on NTS 2"), "{line}");
        assert!(line.contains("2 no-match"), "{line}");
        assert!(line.contains("1 network"), "{line}");
        assert!(line.contains("1 timeout"), "{line}");
        assert!(line.contains("1 unreadable row"), "{line}");
        assert!(line.contains("SAMPLER YIELD, not a hit rate"), "{line}");
        // The derived clock is 600s here, and the printed model and inputs travel with
        // the number so the claim can be checked rather than trusted.
        assert!(line.contains("periodic 600s clock"), "{line}");
        assert!(line.contains("about 60% of 240s tracks"), "{line}");
        assert!(line.contains("periodic model 1 - L/I, L=240"), "{line}");
    }

    #[test]
    fn render_recent_puts_marks_first_collapses_repeats_and_counts_owned() {
        // His own duplicate shape is INTERLEAVED A,B,A,B, which consecutive-dedupe
        // suppresses none of - so the window, not adjacency, is what collapses them.
        let mut rows = vec![
            row(1000, "heard", "A - One"),
            row(1100, "heard", "B - Two"),
            row(1200, "heard", "A - One"),
            row(1300, "heard", "B - Two"),
        ];
        rows.push(HeardRow { owned: true, ..row(1400, "heard", "C - Owned") });
        rows.push(HeardRow {
            ev: "mark".into(),
            src: Some("mark".into()),
            starred: true,
            owned: true,
            ..row(1500, "mark", "D - Marked")
        });
        let out = render(&rows, 0, &HeardQuery::default(), &[]);
        assert!(out[0].starts_with("coverage:"), "{:?}", out);
        let joined = out.join("\n");
        let mark_at = joined.find("D - Marked").expect("the mark is rendered");
        let heard_at = joined.find("A - One").expect("the heard rows are rendered");
        assert!(mark_at < heard_at, "marks must come first:\n{joined}");
        assert!(joined.contains("[starred]"), "{joined}");
        // Two distinct unowned titles survive, the two repeats collapse.
        assert_eq!(joined.matches("A - One").count(), 1, "{joined}");
        assert_eq!(joined.matches("B - Two").count(), 1, "{joined}");
        assert!(joined.contains("+2 more"), "{joined}");
        // Owned rows collapse to a count and never take a line in the compact view.
        assert!(joined.contains("+1 you already own"), "{joined}");
        assert!(!joined.contains("C - Owned"), "{joined}");
    }

    #[test]
    fn render_all_keeps_owned_rows_tagged_and_uncapped() {
        let mut rows: Vec<HeardRow> = (0..40)
            .map(|i| row(1000 + i * 10, "heard", &format!("A{i} - T{i}")))
            .collect();
        rows.push(HeardRow { owned: true, ..row(9000, "heard", "C - Owned") });
        let q = HeardQuery { view: HeardView::All, ..Default::default() };
        let out = render(&rows, 0, &q, &[]);
        // 41 rows plus the coverage line, nothing capped away.
        assert_eq!(out.len(), 42, "{out:?}");
        assert!(out.iter().any(|l| l.contains("C - Owned") && l.contains("[owned]")), "{out:?}");
    }

    #[test]
    fn render_marks_is_oldest_first_and_honest_when_empty() {
        let out = render(&[row(1000, "heard", "A - B")], 0, &HeardQuery { view: HeardView::Marks, ..Default::default() }, &[]);
        assert_eq!(out, vec!["no marks recorded yet".to_string()]);

        let rows = vec![
            HeardRow { ev: "mark".into(), ..row(1000, "mark", "First - Mark") },
            HeardRow { ev: "mark".into(), ..row(2000, "mark", "Second - Mark") },
        ];
        let out = render(&rows, 0, &HeardQuery { view: HeardView::Marks, ..Default::default() }, &[]);
        assert_eq!(out.len(), 3);
        assert!(out[1].contains("First - Mark"), "{out:?}");
        assert!(out[2].contains("Second - Mark"), "{out:?}");
    }

    #[test]
    fn a_marked_row_shows_its_audio_and_says_when_the_audio_is_gone() {
        // The tape is a ROLLING CACHE, not an archive: `keep_sessions` (text) and
        // `max_bytes` (audio) are independent by construction, so a row outliving its
        // segment is DESIGNED FOR. It must read as the moment it always was, annotated
        // that the sound was swept - never as a number that resolves to nothing.
        let ids = vec![
            "20260801-1000-a-w300s".to_string(),
            "20260802-1000-b-w300s".to_string(),
        ];
        let here = HeardRow {
            ev: "mark".into(),
            tape: Some("20260802-1000-b-w300s".into()),
            tape_secs: Some(312),
            cut: Some("window".into()),
            ..row(1000, "mark", "Something - Heard")
        };
        let gone = HeardRow {
            ev: "mark".into(),
            tape: Some("20250101-0000-old-w300s".into()),
            tape_secs: Some(300),
            cut: Some("window".into()),
            ..row(2000, "mark", "Older - Moment")
        };
        let refused = HeardRow {
            ev: "mark".into(),
            tape_outcome: Some("only 3s is buffered so far".into()),
            ..row(3000, "mark", "Too - Early")
        };
        let q = HeardQuery { view: HeardView::Marks, ..Default::default() };
        let out = render(&[here, gone, refused], 0, &q, &ids);
        assert!(out[1].contains("[tape 2: 5m, window]"), "{out:?}");
        assert!(out[2].contains("[tape swept]"), "{out:?}");
        assert!(!out[2].contains("tape 0"), "a swept segment gets no number: {out:?}");
        assert!(out[3].contains("[no tape: only 3s is buffered so far]"), "{out:?}");
        // The numbering has ONE source, shared with `heard keep <n>`.
        assert_eq!(tape_index(&ids, "20260801-1000-a-w300s"), Some(1));
        assert_eq!(tape_index(&ids, "nope"), None);
    }

    #[test]
    fn a_marked_row_never_prints_a_tape_it_does_not_have() {
        // The common ICY path: the star succeeded, so the rip was deleted. The row must
        // read exactly as it did before the tape existed, plus nothing.
        let plain = HeardRow {
            ev: "mark".into(),
            starred: true,
            owned: true,
            ..row(1000, "mark", "Owned - Track")
        };
        let q = HeardQuery { view: HeardView::Marks, ..Default::default() };
        let out = render(&[plain], 0, &q, &["20260801-1000-a-w300s".to_string()]);
        assert!(!out[1].contains("tape"), "{out:?}");
        assert!(out[1].contains("[starred]"), "{out:?}");
    }

    #[test]
    fn parse_rows_skips_a_torn_line_and_counts_it() {
        // The daemon can be killed mid-write, so a torn trailing line must be counted,
        // never a panic and never a silently truncated render.
        let text = "{\"ev\":\"session\"}\n{\"ev\":\"heard\",\"raw\":\"A - B\"}\n{\"ev\":\"hea";
        let (rows, unreadable) = parse_rows(text);
        assert_eq!(rows.len(), 2);
        assert_eq!(unreadable, 1);
        assert_eq!(rows[1].raw.as_deref(), Some("A - B"));
        // An empty file is empty, not an error.
        assert_eq!(parse_rows(""), (Vec::new(), 0));
    }

    #[test]
    fn a_row_with_control_characters_stays_one_line() {
        let mut r = row(1000, "heard", "A\nB\tC");
        r.title = Some("x\u{7}y".to_string());
        let buf = encode_batch(&[r.clone()]);
        assert_eq!(buf.matches('\n').count(), 1, "one row must be exactly one line");
        let (back, unreadable) = parse_rows(&buf);
        assert_eq!(unreadable, 0);
        assert_eq!(back[0], r);
    }

    #[test]
    fn an_old_file_still_loads_after_a_field_is_added() {
        // Upgrade compat: every field is `#[serde(default)]`, so a row written before a
        // field existed still parses.
        let (rows, unreadable) = parse_rows("{\"ev\":\"heard\",\"raw\":\"A - B\"}");
        assert_eq!(unreadable, 0);
        assert_eq!(rows[0].ev, "heard");
        assert_eq!(rows[0].at_unix, 0);
        assert!(!rows[0].owned);
    }

    // ── the ledger task ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn ledger_appends_every_row_in_order_and_leaves_no_temp_file() {
        let dir = test_dir("append");
        let ledger = spawn_heard_ledger(dir.clone(), 30, 300);
        for i in 0..25u64 {
            ledger.append(row(1000 + i, "heard", &format!("A{i} - T{i}")));
        }
        let d = dir.clone();
        settle_until("25 rows written", || {
            newest_session(&d)
                .map(|p| read_session(&p).0.len() >= 26)
                .unwrap_or(false)
        })
        .await;
        let path = newest_session(&dir).expect("a session file");
        let (rows, unreadable) = read_session(&path);
        assert_eq!(unreadable, 0);
        // One session row plus the 25 appended, IN ORDER.
        assert_eq!(rows.len(), 26);
        assert_eq!(rows[0].ev, "session");
        assert_eq!(rows[0].interval_secs, Some(300));
        for i in 0..25usize {
            assert_eq!(rows[i + 1].raw.as_deref(), Some(format!("A{i} - T{i}").as_str()));
        }
        // NO sibling temp file was ever created: the ledger appends, it does not do the
        // whole-file rewrite `atomic_write_bytes` does.
        assert_eq!(dir_names(&dir).len(), 1, "{:?}", dir_names(&dir));
        drop(ledger);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ledger_appends_rather_than_rewrites_across_sessions() {
        // The direct inverse of `atomic_write_bytes_overwrite_is_whole_or_nothing_and_
        // never_appends`: a second handle on the SAME path must EXTEND the file.
        let dir = test_dir("o-append");
        let ledger = spawn_heard_ledger(dir.clone(), 30, 300);
        ledger.append(row(1000, "heard", "first"));
        let d = dir.clone();
        settle_until("first row", || {
            newest_session(&d).map(|p| read_session(&p).0.len() >= 2).unwrap_or(false)
        })
        .await;
        let path = newest_session(&dir).expect("a session file");
        drop(ledger);

        // Re-open the SAME file with the same O_APPEND discipline and write again.
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            let buf = encode_batch(&[row(2000, "heard", "second")]);
            f.write_all(buf.as_bytes()).unwrap();
        }
        let (rows, _) = read_session(&path);
        assert_eq!(rows.len(), 3, "the first rows must SURVIVE the second write");
        assert_eq!(rows[1].raw.as_deref(), Some("first"));
        assert_eq!(rows[2].raw.as_deref(), Some("second"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ledger_task_exits_when_the_last_handle_drops() {
        let dir = test_dir("exit");
        let ledger = spawn_heard_ledger(dir.clone(), 30, 300);
        ledger.append(row(1000, "heard", "x"));
        let d = dir.clone();
        settle_until("row written", || {
            newest_session(&d).map(|p| read_session(&p).0.len() >= 2).unwrap_or(false)
        })
        .await;
        drop(ledger);
        // Nothing to assert but that the drop does not hang or panic; the channel close
        // IS the shutdown signal, matching the repo's only convention.
        tokio::task::yield_now().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_on_a_dead_ledger_never_blocks_or_panics() {
        // A dead task must degrade to "the row is simply not written". Build a handle
        // whose receiver is already gone and hammer it: this is what makes the spine
        // call safe under every failure.
        let (tx, rx) = mpsc::unbounded_channel::<HeardRow>();
        drop(rx);
        let ledger = HeardLedger { tx };
        for i in 0..10_000u64 {
            ledger.append(row(i, "heard", "x"));
        }
    }

    #[test]
    fn sweep_keeps_the_newest_sessions_and_never_removes_everything() {
        let dir = test_dir("sweep");
        for n in 0..5 {
            std::fs::write(dir.join(format!("2026-08-0{n}-1200-1.jsonl")), b"{}\n").unwrap();
        }
        // A non-ledger file is never touched.
        std::fs::write(dir.join("notes.txt"), b"keep me").unwrap();
        assert_eq!(sweep_sessions(&dir, 2), 3);
        let names = dir_names(&dir);
        assert_eq!(
            names,
            vec![
                "2026-08-03-1200-1.jsonl".to_string(),
                "2026-08-04-1200-1.jsonl".to_string(),
                "notes.txt".to_string(),
            ]
        );
        // A zero keep is clamped to 1 rather than emptying the directory.
        assert_eq!(sweep_sessions(&dir, 0), 1);
        assert_eq!(dir_names(&dir).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn this_module_never_reaches_for_the_fsyncing_writer() {
        // A STRUCTURAL guard on the invariant this whole module exists to keep: the
        // ledger must never call the whole-file-rewrite-plus-fsync writer, because the
        // ICY row producer sits on the director spine and CLAUDE.md records a skip-EOF
        // audible bleed from exactly that class. Checked against the source itself, so
        // a future edit that reaches for it fails here rather than in the user's room.
        // Only the production half: the assertions below name the very strings they
        // forbid, so the test module itself must be cut off first.
        let whole = include_str!("heard.rs");
        let src = whole.split("#[cfg(test)]").next().expect("a production half");
        assert!(
            !src.contains("atomic_write_bytes("),
            "heard.rs must never CALL atomic_write_bytes (naming it in a doc is fine)"
        );
        assert!(
            !src.contains(".sync_all("),
            "heard.rs must never fsync: an O_APPEND line write is the whole point"
        );
        assert!(
            src.contains("spawn_blocking"),
            "every filesystem syscall here belongs in spawn_blocking"
        );
    }

    #[test]
    fn append_is_a_sync_fn_so_a_spine_write_cannot_compile() {
        // The type IS the invariant: `append` returns `()` rather than a future, so no
        // future edit can `.await` a ledger write from `set_stream_meta` (which the
        // director calls synchronously ahead of the EOF arm). This test pins the
        // signature; changing it to async breaks the assignment below.
        let f: fn(&HeardLedger, HeardRow) = HeardLedger::append;
        let _ = f;
    }

    #[test]
    fn session_file_name_sorts_chronologically_and_carries_the_pid() {
        use chrono::TimeZone;
        let a = chrono::Local.with_ymd_and_hms(2026, 8, 4, 9, 5, 0).unwrap();
        let b = chrono::Local.with_ymd_and_hms(2026, 8, 4, 21, 14, 0).unwrap();
        let (a, b) = (session_file_name(a, 7), session_file_name(b, 7));
        assert_eq!(a, "2026-08-04-0905-7.jsonl");
        assert!(a < b, "lexical order must be chronological order");
    }
}
