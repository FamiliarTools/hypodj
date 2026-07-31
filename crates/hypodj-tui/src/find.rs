//! The Find screen: a library query surface, distinct from the `/` cursor jump.
//!
//! ## Why "Find" and not "Search"
//!
//! `Mode::Search`, `Group::Search`, `Act::SearchStart/SearchNext/SearchPrev`,
//! `run_search`, `search_jump`, `search_step`, `search_origin`, `last_search` and
//! `highlight_query` are ALL already the vim-style `/` cursor jump over rows that
//! are already loaded. Two things called "search" in one file would be unreadable,
//! so the code says Find throughout. The tab strip says `[F5]Search` because that
//! is the word a user reaches for; the divergence is deliberate and is one line in
//! [`crate::ui`].
//!
//! ## What it is
//!
//! The `/` jump can only find what is already on screen, so an artist whose albums
//! never entered the newest-100 list is unreachable from the interface. Find asks
//! the LIBRARY instead: type a query, press Enter, get back a single flat ranked
//! list of matching songs and albums (and, once the daemon grows a `searchall`
//! verb, artists) that can be enqueued, played, starred and drilled into with the
//! keys that already work on the Albums tab.
//!
//! ## Structure
//!
//! Everything here is PURE - no clock, no I/O, no network, no lock. The hit list is
//! a flat `Vec<FindRow>` with ONE cursor, because the screen band is `height - 16`
//! rows (a `Length` constraint outranks a `Min` in ratatui) and that leaves five
//! content rows at the 60x24 the test harness uses. Per-kind header rows would have
//! spent three of those five on chrome, so kind lives in a one-character gutter
//! sigil and the tallies live in the block title, at zero row cost.

use std::cell::Cell;

use crate::state::Browse;

/// What a hit row refers to. Ordering is the display order: artists, then albums,
/// then songs, so a long song list can never bury the artists above it.
// Constructed by the wire parser in step 2; step 1 is the screen skeleton, so the
// variants exist here (and are exercised by this module's tests) before the parser
// that builds them lands.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindKind {
    Artist,
    Album,
    Song,
}

impl FindKind {
    /// The gutter sigil. ASCII for the same reason `#`/`~` are: a terminal that
    /// cannot render a glyph must not shift the column.
    pub fn sigil(self) -> char {
        match self {
            FindKind::Artist => '@',
            FindKind::Album => '=',
            FindKind::Song => ' ',
        }
    }
}

/// One row of the flat hit list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindRow {
    pub kind: FindKind,
    /// The primary text: an artist name, an album title, a song title.
    pub label: String,
    /// The browse/enqueue uri: `artist/<id>`, `album/<id>`, `song/<id>`.
    pub uri: String,
    /// Right-aligned secondary text, precomputed at parse time so the renderer
    /// stays a dumb column fitter that truncates the label first.
    pub trailer: String,
    /// Total track count for an album row, driving the full-vs-partial queue
    /// marker. `None` on a DERIVED album row (one grouped from song hits), where
    /// `album_mark` correctly degrades to Partial rather than claiming a false Full.
    pub song_count: Option<u32>,
    /// The owning album uri for a song row, so the queue gutter can mark it.
    pub album_uri: Option<String>,
}

/// A parsed result set, plus what the server said about its own caps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FindHits {
    pub rows: Vec<FindRow>,
    /// Per-kind (returned, cap). A kind that returned nothing is still present, so
    /// the title can state the absence: the protocol otherwise makes an empty
    /// result and an empty filter byte-identical (both a bare `OK`).
    pub tallies: Vec<(FindKind, usize, Option<usize>)>,
}

/// Which half of the screen owns the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Query,
    Results,
}

/// Where the hit list is in its lifecycle. `Loading` carries the query it is
/// waiting on, which IS the staleness gate: a response folds only when its echoed
/// query matches, which gives the same guarantee as a generation counter with no
/// new plumbing.
// Set by the find worker in step 2; the renderer already draws every arm.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Nothing asked yet. Distinct from a query that returned nothing.
    Cold,
    Loading(String),
    Done,
    /// The daemon ACKed. The previous hits stay visible underneath.
    Failed(String),
}

/// How many queries the history ring remembers. Slot 0 is reserved for a
/// half-typed line stashed on the first Up, so a walk never loses what was typed.
const HISTORY_CAP: usize = 20;

/// The Find screen's own state.
#[derive(Debug)]
pub struct Find {
    /// The live query line being edited.
    pub query: String,
    /// The query the visible hits actually answer, echoed in the block title so a
    /// stale-but-legible result set says what question it answered.
    pub submitted: String,
    pub focus: Focus,
    pub phase: Phase,
    pub hits: FindHits,
    pub selected: usize,
    pub offset: Cell<usize>,
    /// Most-recent-first ring of submitted queries.
    pub history: Vec<String>,
    /// Cursor into `history` while walking it with Up/Down; `None` when not walking.
    pub history_pos: Option<usize>,
    /// The half-typed line stashed when a history walk starts.
    pub history_stash: String,
    /// A drill into an artist or album hit. A SEPARATE `Browse` layered over the
    /// frozen hits rather than a reuse of them, so the whole existing
    /// `browse_into` / `Req::Browse` / `Browse::apply` / `render_browse` path works
    /// unchanged and backing out cannot lose the query.
    pub drill: Browse,
    /// True while the drill is the visible list. Every per-screen helper branches
    /// on this; it is also the drill's staleness gate, so a drill response that
    /// lands after the user backed out is dropped rather than left to flash.
    pub drilling: bool,
    /// A drill fetch is in flight - drives the spinner in the drill title. Without
    /// it the drill renders an empty bordered box for a whole round trip.
    // Read by the drill renderer in step 3.
    #[allow(dead_code)]
    pub drill_loading: bool,
}

impl Default for Find {
    fn default() -> Self {
        Find {
            query: String::new(),
            submitted: String::new(),
            focus: Focus::Query,
            phase: Phase::Cold,
            hits: FindHits::default(),
            selected: 0,
            offset: Cell::new(0),
            history: Vec::new(),
            history_pos: None,
            history_stash: String::new(),
            drill: Browse::new("", "find"),
            drilling: false,
            drill_loading: false,
        }
    }
}

impl Find {
    /// The row under the cursor, if any.
    // Used by enqueue/open/favorite in step 2.
    #[allow(dead_code)]
    pub fn current_row(&self) -> Option<&FindRow> {
        self.hits.rows.get(self.selected)
    }

    /// Move the hit cursor, clamped at both ends (never a wrap: a wrap at the top
    /// of a 200-row result set is disorienting when the list is not a loop).
    pub fn move_selection(&mut self, delta: i32) {
        if self.hits.rows.is_empty() {
            self.selected = 0;
            return;
        }
        let last = self.hits.rows.len() - 1;
        let next = self.selected as i32 + delta;
        self.selected = next.clamp(0, last as i32) as usize;
    }

    /// Record a submitted query at the front of the ring, deduped, and end any walk.
    pub fn push_history(&mut self, query: &str) {
        if query.is_empty() {
            return;
        }
        self.history.retain(|q| q != query);
        self.history.insert(0, query.to_string());
        self.history.truncate(HISTORY_CAP);
        self.history_pos = None;
        self.history_stash.clear();
    }

    /// Walk the history ring. `delta` is +1 for older (Up) and -1 for newer (Down).
    /// Walking past the newest end restores the stashed half-typed line rather than
    /// wrapping, so the text the user was writing is never unreachable.
    pub fn walk_history(&mut self, delta: i32) {
        if self.history.is_empty() {
            return;
        }
        let last = self.history.len() as i32 - 1;
        match self.history_pos {
            None => {
                if delta <= 0 {
                    return;
                }
                self.history_stash = std::mem::take(&mut self.query);
                self.history_pos = Some(0);
                self.query = self.history[0].clone();
            }
            Some(pos) => {
                let next = pos as i32 + delta;
                if next < 0 {
                    self.history_pos = None;
                    self.query = std::mem::take(&mut self.history_stash);
                } else {
                    let next = next.min(last) as usize;
                    self.history_pos = Some(next);
                    self.query = self.history[next].clone();
                }
            }
        }
    }

    /// `history 3/7` for the hint row, so a replaced half-typed line is visibly
    /// recoverable rather than apparently lost.
    pub fn history_indicator(&self) -> Option<String> {
        self.history_pos
            .map(|pos| format!("history {}/{}", pos + 1, self.history.len()))
    }

    /// The block title: the per-kind tallies, including kinds that returned
    /// nothing, plus a `(server cap)` suffix when a kind hit its ceiling. Never
    /// `200+` - `search3` is called with a fixed song cap and no over-request, so a
    /// full page is not evidence that more exist.
    pub fn tally_title(&self) -> String {
        if self.hits.tallies.is_empty() {
            return "find".to_string();
        }
        let mut capped = false;
        let parts: Vec<String> = self
            .hits
            .tallies
            .iter()
            .map(|(kind, n, cap)| {
                if Some(*n) == *cap {
                    capped = true;
                }
                let noun = match kind {
                    FindKind::Artist => "artist",
                    FindKind::Album => "album",
                    FindKind::Song => "song",
                };
                let plural = if *n == 1 { "" } else { "s" };
                if *n == 0 {
                    format!("no {noun}{}", if *n == 1 { "" } else { "s" })
                } else {
                    format!("{n} {noun}{plural}")
                }
            })
            .collect();
        let mut title = parts.join(" / ");
        if capped {
            title.push_str(" (server cap)");
        }
        title
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: FindKind, label: &str) -> FindRow {
        FindRow {
            kind,
            label: label.to_string(),
            uri: format!("song/{label}"),
            trailer: String::new(),
            song_count: None,
            album_uri: None,
        }
    }

    #[test]
    fn cursor_clamps_at_both_ends_and_never_wraps() {
        let mut f = Find::default();
        f.hits.rows = vec![row(FindKind::Song, "a"), row(FindKind::Song, "b")];
        f.move_selection(-1);
        assert_eq!(f.selected, 0, "up at the top must clamp, not wrap to the end");
        f.move_selection(5);
        assert_eq!(f.selected, 1, "down past the end must clamp to the last row");
    }

    #[test]
    fn cursor_on_an_empty_hit_list_stays_at_zero() {
        let mut f = Find::default();
        f.move_selection(3);
        assert_eq!(f.selected, 0);
    }

    #[test]
    fn history_dedupes_and_caps() {
        let mut f = Find::default();
        for i in 0..(HISTORY_CAP + 5) {
            f.push_history(&format!("q{i}"));
        }
        assert_eq!(f.history.len(), HISTORY_CAP);
        assert_eq!(f.history[0], format!("q{}", HISTORY_CAP + 4), "newest first");
        f.push_history("q1");
        assert_eq!(f.history.iter().filter(|q| *q == "q1").count(), 1, "deduped");
        assert_eq!(f.history[0], "q1", "a re-submitted query moves to the front");
    }

    #[test]
    fn an_empty_query_is_never_recorded() {
        let mut f = Find::default();
        f.push_history("");
        assert!(f.history.is_empty());
    }

    #[test]
    fn walking_history_stashes_and_restores_a_half_typed_line() {
        let mut f = Find::default();
        f.push_history("c418");
        f.push_history("boards of canada");
        f.query = "half typ".to_string();
        f.walk_history(1);
        assert_eq!(f.query, "boards of canada");
        assert_eq!(f.history_indicator().as_deref(), Some("history 1/2"));
        f.walk_history(1);
        assert_eq!(f.query, "c418");
        f.walk_history(1);
        assert_eq!(f.query, "c418", "walking past the oldest clamps, never wraps");
        f.walk_history(-1);
        f.walk_history(-1);
        assert_eq!(f.query, "half typ", "the stashed line comes back, not an empty line");
        assert_eq!(f.history_indicator(), None);
    }

    #[test]
    fn walking_down_before_any_walk_does_nothing() {
        let mut f = Find::default();
        f.push_history("c418");
        f.query = "typed".to_string();
        f.walk_history(-1);
        assert_eq!(f.query, "typed");
        assert_eq!(f.history_pos, None);
    }

    #[test]
    fn a_kind_that_returned_nothing_is_stated_in_the_title() {
        let mut f = Find::default();
        f.hits.tallies = vec![
            (FindKind::Artist, 0, Some(20)),
            (FindKind::Album, 12, Some(50)),
            (FindKind::Song, 47, Some(200)),
        ];
        assert_eq!(f.tally_title(), "no artists / 12 albums / 47 songs");
    }

    #[test]
    fn a_kind_at_its_ceiling_says_server_cap_not_a_guess() {
        let mut f = Find::default();
        f.hits.tallies = vec![(FindKind::Song, 200, Some(200))];
        assert_eq!(
            f.tally_title(),
            "200 songs (server cap)",
            "a full page is not evidence that more exist, so never render 200+"
        );
    }

    #[test]
    fn a_stage_one_tally_with_no_known_cap_claims_none() {
        let mut f = Find::default();
        f.hits.tallies = vec![(FindKind::Album, 12, None), (FindKind::Song, 47, None)];
        assert_eq!(f.tally_title(), "12 albums / 47 songs");
    }

    #[test]
    fn the_cold_title_is_not_a_tally() {
        assert_eq!(Find::default().tally_title(), "find");
    }

    #[test]
    fn gutter_sigils_are_ascii_and_distinct_per_kind() {
        assert_eq!(FindKind::Artist.sigil(), '@');
        assert_eq!(FindKind::Album.sigil(), '=');
        assert_eq!(FindKind::Song.sigil(), ' ');
    }
}
