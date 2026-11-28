//! LISTENING ARCS: a session he already listened to, read back as its shape and kept
//! under a name so it can be played again (task brw7b4u, the generalised "Warm Room").
//!
//! There is no second recorder. The heard ledger ([`crate::heard`]) already owns the
//! durable, bounded, spine-safe write path into the state dir (one `O_APPEND` handle,
//! `keep_sessions` retention), so a library track that STARTS sounding writes one
//! `play` row there and a manual early skip writes one `skip` row. This module is the
//! pure read side over those rows plus the small saved-arc file beside them:
//!
//! 1. [`sessions`] - segment the play rows into listening sessions by SILENCE, not by
//!    daemon process: a gap of [`ARC_GAP_SECS`] between two track starts ends one.
//!    A daemon restart mid-evening is not a new mood, and a daemon left up for a week
//!    is not one session.
//! 2. [`summary`] / [`movements`] - the arc itself: time of day, the genre MOVEMENTS
//!    (runs of one genre, the "downtempo -> trip-hop -> ambient" he saw), and the
//!    energy trajectory over the feature seam ([`crate::intelligence::FeatureStore`],
//!    measured where analysed, the documented heuristic elsewhere - stamped at play
//!    time so the read-back needs no library round trip).
//! 3. [`SavedArc`] / [`load_saved`] / [`save_arc`] - a named arc on disk, bounded to
//!    [`MAX_SAVED_ARCS`] entries of at most [`crate::plan::MAX_ENQUEUE`] tracks.
//!
//! REPLAY is not here on purpose. It is one `Action::PlayNow { Selector::Exact }`
//! through `run_action_outcome`, so a replay is journaled and undoable exactly like an
//! agent's plan, and nothing in this module can write to Navidrome: a saved arc is a
//! local file, never a server playlist.

use std::path::Path;

use crate::heard::HeardRow;

/// Silence between two track STARTS that ends a listening session. 45 minutes is
/// longer than any album side or club edit he plays, and shorter than the gap between
/// an evening and the next morning, which is the split he actually described.
pub const ARC_GAP_SECS: u64 = 45 * 60;

/// How many recent sessions `arc` lists by default.
pub const ARC_DEFAULT_LIMIT: usize = 5;

/// The saved-arc file, beside the ledger sessions. Not `.jsonl`, so the ledger's
/// retention sweep can never take it.
pub const ARCS_FILE: &str = "arcs.json";

/// The most arcs kept. A save past this evicts the OLDEST save, so the file is
/// bounded by construction rather than by care.
pub const MAX_SAVED_ARCS: usize = 64;

/// The `arc` verb's forms (see `mpd::parse_arc`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArcCmd {
    /// `arc` / `arc limit <n>`: the recent sessions with their arcs.
    Recent(usize),
    /// `arc save <n> <name>`: name session n (1 = newest) and keep it.
    Save { n: usize, name: String },
    /// `arc saved`: the named arcs.
    Saved,
    /// `arc play <name>`: replay a saved arc (or recent session `<n>` when no arc has
    /// that name and it is a number) through the journaled `PlayNow` action.
    Play(String),
}

/// One track as it sounded inside a session.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArcTrack {
    /// The library id (the `<id>` of `song/<id>`).
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    /// Energy 0..=100 as the feature seam saw it at play time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy_pct: Option<u8>,
    pub at_unix: u64,
    #[serde(default)]
    pub skipped: bool,
    #[serde(default)]
    pub starred: bool,
}

/// One listening session, oldest track first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub tracks: Vec<ArcTrack>,
}

impl Session {
    pub fn start_unix(&self) -> u64 {
        self.tracks.first().map_or(0, |t| t.at_unix)
    }
    pub fn end_unix(&self) -> u64 {
        self.tracks.last().map_or(0, |t| t.at_unix)
    }
    /// What a replay plays: every track he did NOT skip, in order. A skip was a
    /// rejection inside the arc, so putting it back would replay his "no" as a "yes".
    pub fn kept_ids(&self) -> Vec<String> {
        self.tracks.iter().filter(|t| !t.skipped).map(|t| t.id.clone()).collect()
    }
}

fn song_id(uri: Option<&str>) -> Option<String> {
    uri.and_then(|u| u.strip_prefix("song/")).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Segment ledger rows into sessions, oldest first. Pure.
///
/// `play` rows are the tracks; a `skip` row flags the newest earlier play of the same
/// song; a STARRED `mark` row flags it too. Rows of any other kind are ignored, so a
/// ledger written before plays were recorded simply yields no sessions.
pub fn sessions(rows: &[HeardRow]) -> Vec<Session> {
    let mut plays: Vec<ArcTrack> = Vec::new();
    let mut sorted: Vec<&HeardRow> = rows.iter().collect();
    // Stable, so rows sharing a second keep their file order (play before its skip).
    sorted.sort_by_key(|r| r.at_unix);
    for r in sorted {
        let Some(id) = song_id(r.match_uri.as_deref()) else { continue };
        match r.ev.as_str() {
            "play" => plays.push(ArcTrack {
                id,
                artist: r.artist.clone(),
                title: r.title.clone(),
                genre: r.genre.clone(),
                energy_pct: r.energy_pct,
                at_unix: r.at_unix,
                skipped: false,
                starred: r.starred,
            }),
            "skip" | "mark" => {
                if r.ev == "mark" && !r.starred {
                    continue;
                }
                if let Some(t) = plays.iter_mut().rev().find(|t| t.id == id) {
                    if r.ev == "skip" {
                        t.skipped = true;
                    } else {
                        t.starred = true;
                    }
                }
            }
            _ => {}
        }
    }
    let mut out: Vec<Session> = Vec::new();
    for t in plays {
        match out.last_mut() {
            Some(s) if t.at_unix.saturating_sub(s.end_unix()) <= ARC_GAP_SECS => s.tracks.push(t),
            _ => out.push(Session { tracks: vec![t] }),
        }
    }
    out
}

/// The time-of-day word for a LOCAL hour.
pub fn time_of_day(hour: u32) -> &'static str {
    match hour {
        5..=11 => "morning",
        12..=16 => "afternoon",
        17..=21 => "evening",
        _ => "night",
    }
}

/// One movement: a run of consecutive tracks sharing a genre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Movement {
    pub genre: String,
    pub len: usize,
    /// Mean energy 0..=100 over the run's tracks that carried one.
    pub energy_pct: Option<u8>,
}

fn genre_key(t: &ArcTrack) -> String {
    t.genre
        .as_deref()
        .map(|g| g.trim().to_lowercase())
        .filter(|g| !g.is_empty())
        .unwrap_or_else(|| "unknown genre".to_string())
}

fn mean(vals: impl Iterator<Item = u8>) -> Option<u8> {
    let (sum, n) = vals.fold((0u32, 0u32), |(s, n), v| (s + v as u32, n + 1));
    (n > 0).then(|| ((sum + n / 2) / n) as u8)
}

/// The genre movements of a session, in order. Pure.
pub fn movements(s: &Session) -> Vec<Movement> {
    let mut out: Vec<(String, Vec<&ArcTrack>)> = Vec::new();
    for t in &s.tracks {
        let g = genre_key(t);
        match out.last_mut() {
            Some((lg, run)) if *lg == g => run.push(t),
            _ => out.push((g, vec![t])),
        }
    }
    out.into_iter()
        .map(|(genre, run)| Movement {
            genre,
            len: run.len(),
            energy_pct: mean(run.iter().filter_map(|t| t.energy_pct)),
        })
        .collect()
}

/// The energy shape over a session: the first third against the last third. Pure.
pub fn energy_shape(s: &Session) -> Option<String> {
    let e: Vec<u8> = s.tracks.iter().filter_map(|t| t.energy_pct).collect();
    if e.len() < 2 {
        return None;
    }
    let third = (e.len() / 3).max(1);
    let open = mean(e[..third].iter().copied())?;
    let close = mean(e[e.len() - third..].iter().copied())?;
    let word = match close as i16 - open as i16 {
        d if d <= -10 => "winding down",
        d if d >= 10 => "lifting",
        _ => "steady",
    };
    Some(format!("energy {:.2} -> {:.2} {word}", open as f32 / 100.0, close as f32 / 100.0))
}

fn human_span(secs: u64) -> String {
    let m = secs / 60;
    if m >= 60 {
        format!("{}h{:02}m", m / 60, m % 60)
    } else {
        format!("{m}m")
    }
}

fn local(at_unix: u64) -> chrono::DateTime<chrono::Local> {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(at_unix as i64, 0)
        .single()
        .unwrap_or_else(|| chrono::Local.timestamp_opt(0, 0).unwrap())
}

/// The one-paragraph arc of a session: when, how long, what it did, and its shape.
/// Pure over the session plus the LOCAL hour of its start (passed in, so tests do not
/// depend on the machine's timezone).
pub fn summary(s: &Session, start_hour: u32, clock: &str) -> String {
    let skipped = s.tracks.iter().filter(|t| t.skipped).count();
    let starred = s.tracks.iter().filter(|t| t.starred).count();
    let mut head = format!(
        "{} {clock}, {}, {} track{}",
        time_of_day(start_hour),
        human_span(s.end_unix().saturating_sub(s.start_unix())),
        s.tracks.len(),
        if s.tracks.len() == 1 { "" } else { "s" }
    );
    let mut extra = Vec::new();
    if skipped > 0 {
        extra.push(format!("{skipped} skipped"));
    }
    if starred > 0 {
        extra.push(format!("{starred} starred"));
    }
    if !extra.is_empty() {
        head.push_str(&format!(" ({})", extra.join(", ")));
    }
    let flow: Vec<String> = movements(s)
        .iter()
        .map(|m| match m.energy_pct {
            Some(e) => format!("{} x{} e{:.2}", m.genre, m.len, e as f32 / 100.0),
            None => format!("{} x{}", m.genre, m.len),
        })
        .collect();
    let mut out = format!("{head} | {}", flow.join(" -> "));
    if let Some(shape) = energy_shape(s) {
        out.push_str(&format!(" | {shape}"));
    }
    out
}

/// The summary with the start hour and clock string taken from the local timezone.
pub fn summary_local(s: &Session) -> String {
    use chrono::Timelike;
    let a = local(s.start_unix());
    let b = local(s.end_unix());
    let clock = format!("{} {}-{}", a.format("%Y-%m-%d"), a.format("%H:%M"), b.format("%H:%M"));
    summary(s, a.hour(), &clock)
}

/// Render the `arc` listing: newest session first, numbered as `arc save <n>` takes
/// them. Pure apart from the local-time formatting.
pub fn render_recent(all: &[Session], limit: usize) -> Vec<String> {
    if all.is_empty() {
        return vec![
            "no listening sessions recorded yet - a session is written as library tracks play".to_string(),
        ];
    }
    let mut out = Vec::new();
    for (i, s) in all.iter().rev().take(limit).enumerate() {
        out.push(format!("[{}] {}", i + 1, summary_local(s)));
    }
    out
}

/// Session `n` in the listing's numbering (1 = newest).
pub fn nth_recent(all: &[Session], n: usize) -> Option<&Session> {
    n.checked_sub(1).and_then(|i| all.iter().rev().nth(i))
}

/// A named, saved arc.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SavedArc {
    pub name: String,
    pub saved_unix: u64,
    /// The arc paragraph as it read when saved.
    pub summary: String,
    pub tracks: Vec<ArcTrack>,
}

impl SavedArc {
    pub fn from_session(name: &str, s: &Session, saved_unix: u64) -> Self {
        let max = crate::plan::MAX_ENQUEUE as usize;
        SavedArc {
            name: name.trim().to_string(),
            saved_unix,
            summary: summary_local(s),
            tracks: s.tracks.iter().take(max).cloned().collect(),
        }
    }
    /// What a replay plays: the unskipped tracks, in order.
    pub fn kept_ids(&self) -> Vec<String> {
        self.tracks.iter().filter(|t| !t.skipped).map(|t| t.id.clone()).collect()
    }
}

/// Insert `arc`, replacing a same-named arc (case-insensitive) and evicting the oldest
/// saves past [`MAX_SAVED_ARCS`]. Pure.
pub fn upsert(mut arcs: Vec<SavedArc>, arc: SavedArc) -> Vec<SavedArc> {
    arcs.retain(|a| !a.name.eq_ignore_ascii_case(&arc.name));
    arcs.push(arc);
    arcs.sort_by_key(|a| a.saved_unix);
    let over = arcs.len().saturating_sub(MAX_SAVED_ARCS);
    arcs.drain(..over);
    arcs
}

/// Find a saved arc by name, case-insensitive.
pub fn find<'a>(arcs: &'a [SavedArc], name: &str) -> Option<&'a SavedArc> {
    arcs.iter().find(|a| a.name.eq_ignore_ascii_case(name.trim()))
}

/// Read the saved arcs. A missing or unreadable file is an empty list. Sync:
/// `spawn_blocking` territory.
pub fn load_saved(dir: &Path) -> Vec<SavedArc> {
    std::fs::read(dir.join(ARCS_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Save `arc` (upsert + bound) atomically. Sync: `spawn_blocking` territory.
pub fn save_arc(dir: &Path, arc: SavedArc) -> std::io::Result<()> {
    // One process-wide lock over the read-merge-write: two concurrent saves would
    // each read the same old list and the later rename would drop the other's arc.
    // A std Mutex is right here - this runs on a blocking thread, never across an
    // await. A poisoned lock still serializes; the file itself is atomic.
    static SAVE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = SAVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    std::fs::create_dir_all(dir)?;
    let arcs = upsert(load_saved(dir), arc);
    let bytes = serde_json::to_vec_pretty(&arcs).map_err(std::io::Error::other)?;
    crate::resume::atomic_write_bytes(&dir.join(ARCS_FILE), &bytes)
}

/// Render the saved arcs, newest save first.
pub fn render_saved(arcs: &[SavedArc]) -> Vec<String> {
    if arcs.is_empty() {
        return vec!["no saved arcs - name one with: arc save <n> <name>".to_string()];
    }
    arcs.iter()
        .rev()
        .map(|a| format!("{}: {} kept of {} | {}", a.name, a.kept_ids().len(), a.tracks.len(), a.summary))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn play(at: u64, id: &str, genre: &str, e: u8) -> HeardRow {
        HeardRow {
            at_unix: at,
            ev: "play".into(),
            match_uri: Some(format!("song/{id}")),
            title: Some(id.into()),
            genre: Some(genre.into()),
            energy_pct: Some(e),
            ..Default::default()
        }
    }

    fn row(at: u64, ev: &str, id: &str, starred: bool) -> HeardRow {
        HeardRow { at_unix: at, ev: ev.into(), match_uri: Some(format!("song/{id}")), starred, ..Default::default() }
    }

    #[test]
    fn silence_splits_sessions_and_a_restart_does_not() {
        let t0 = 1_700_000_000;
        let rows = vec![
            play(t0, "a", "Downtempo", 50),
            HeardRow { at_unix: t0 + 100, ev: "session".into(), ..Default::default() },
            play(t0 + 240, "b", "Downtempo", 40),
            play(t0 + 240 + ARC_GAP_SECS + 1, "c", "Ambient", 20),
        ];
        let s = sessions(&rows);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].tracks.len(), 2, "a daemon restart inside the gap is the same session");
        assert_eq!(s[1].tracks[0].id, "c");
    }

    #[test]
    fn skips_and_stars_attach_to_the_latest_play_and_skips_are_not_replayed() {
        let rows = vec![
            play(10, "a", "x", 50),
            play(20, "b", "x", 50),
            row(25, "skip", "b", false),
            row(30, "mark", "a", true),
            row(31, "mark", "b", false),
            play(40, "c", "x", 50),
        ];
        let s = &sessions(&rows)[0];
        assert!(s.tracks[1].skipped);
        assert!(s.tracks[0].starred && !s.tracks[1].starred);
        assert_eq!(s.kept_ids(), vec!["a", "c"]);
    }

    #[test]
    fn movements_and_energy_trace_the_warm_room_shape() {
        let rows = vec![
            play(0, "1", "Downtempo", 60),
            play(1, "2", "downtempo", 56),
            play(2, "3", "Trip-Hop", 45),
            play(3, "4", "Ambient", 20),
            play(4, "5", "Ambient", 16),
            play(5, "6", "Ambient", 14),
        ];
        let s = &sessions(&rows)[0];
        let m = movements(s);
        assert_eq!(
            m.iter().map(|m| (m.genre.as_str(), m.len)).collect::<Vec<_>>(),
            vec![("downtempo", 2), ("trip-hop", 1), ("ambient", 3)]
        );
        assert_eq!(m[0].energy_pct, Some(58));
        let line = summary(s, 23, "x");
        assert!(line.starts_with("night x, 0m, 6 tracks"), "{line}");
        assert!(line.contains("downtempo x2 e0.58 -> trip-hop x1 e0.45 -> ambient x3 e0.17"), "{line}");
        assert!(line.ends_with("energy 0.58 -> 0.15 winding down"), "{line}");
    }

    #[test]
    fn time_of_day_covers_every_hour() {
        assert_eq!(time_of_day(3), "night");
        assert_eq!(time_of_day(8), "morning");
        assert_eq!(time_of_day(14), "afternoon");
        assert_eq!(time_of_day(20), "evening");
        assert_eq!(time_of_day(23), "night");
    }

    #[test]
    fn nth_recent_counts_from_the_newest() {
        let rows = vec![play(0, "old", "x", 1), play(ARC_GAP_SECS * 3, "new", "x", 1)];
        let s = sessions(&rows);
        assert_eq!(nth_recent(&s, 1).unwrap().tracks[0].id, "new");
        assert_eq!(nth_recent(&s, 2).unwrap().tracks[0].id, "old");
        assert!(nth_recent(&s, 0).is_none() && nth_recent(&s, 3).is_none());
    }

    #[test]
    fn upsert_replaces_by_name_and_stays_bounded() {
        let mk = |name: &str, at: u64| SavedArc { name: name.into(), saved_unix: at, summary: String::new(), tracks: vec![] };
        let mut arcs = Vec::new();
        for i in 0..(MAX_SAVED_ARCS as u64 + 5) {
            arcs = upsert(arcs, mk(&format!("a{i}"), i));
        }
        assert_eq!(arcs.len(), MAX_SAVED_ARCS);
        assert_eq!(arcs[0].name, "a5", "the oldest saves go first");
        let arcs = upsert(arcs, mk("A10", 999));
        assert_eq!(arcs.len(), MAX_SAVED_ARCS);
        assert_eq!(find(&arcs, "a10").unwrap().saved_unix, 999);
    }

    #[test]
    fn saved_arcs_round_trip_on_disk() {
        let dir = std::env::temp_dir().join(format!("hypodj-arc-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = &sessions(&[play(0, "a", "x", 10), play(5, "b", "x", 10)])[0];
        save_arc(&dir, SavedArc::from_session(" Warm Room ", s, 7)).unwrap();
        let back = load_saved(&dir);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].name, "Warm Room");
        assert_eq!(back[0].kept_ids(), vec!["a", "b"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_saves_of_distinct_names_all_land() {
        let dir = std::env::temp_dir().join(format!("hypodj-arc-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let threads: Vec<_> = (0..16)
            .map(|i| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    let arc = SavedArc { name: format!("arc {i}"), saved_unix: i, summary: String::new(), tracks: vec![] };
                    save_arc(&dir, arc).unwrap();
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(load_saved(&dir).len(), 16);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
