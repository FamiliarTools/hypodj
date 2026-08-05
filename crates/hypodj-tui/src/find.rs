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
//! list of matching artists, albums, saved internet radio stations and songs that
//! can be enqueued, played, starred and drilled into with the keys that already work
//! on the Albums tab.
//!
//! A STATION row is the user's own saved stream, matched by name daemon-side because
//! `search3` does not index stations at all. It is not a library object: it cannot be
//! starred (Subsonic has no star endpoint for internet radio), cannot be drilled into
//! and cannot seed a radio walk - so those keys say WHY rather than going quiet, while
//! Enter and Space enqueue it through the `station/<name>` uri that already worked.
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
/// then saved stations, then songs, so a long song list can never bury what is above
/// it.
///
/// `Station` sits BETWEEN albums and songs rather than first because an ordinary
/// library query ("coltrane") matches no station at all: the block is empty and the
/// top of the list stays byte-identical to what it always was, while a handful of
/// saved stations still sit safely above the 200-song flood - which is the whole
/// reason this order exists.
// Constructed by the wire parser in step 2; step 1 is the screen skeleton, so the
// variants exist here (and are exercised by this module's tests) before the parser
// that builds them lands.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindKind {
    Artist,
    Album,
    /// A SAVED internet radio station - a thing the user keeps and can search for.
    /// Never the algorithmic `radio/random` browse path, which is a generator.
    Station,
    Song,
}

impl FindKind {
    /// The gutter sigil. ASCII for the same reason `#`/`~` are: a terminal that
    /// cannot render a glyph must not shift the column. `)` for a station: it must
    /// clash with neither `@`/`=` here nor the `#`/`~` queue marks that share the same
    /// three-column gutter, and `*` is avoided because it would read as "starred" right
    /// next to the `s` favorite key.
    pub fn sigil(self) -> char {
        match self {
            FindKind::Artist => '@',
            FindKind::Album => '=',
            FindKind::Station => ')',
            FindKind::Song => ' ',
        }
    }
}

/// One row of the flat hit list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindRow {
    pub kind: FindKind,
    /// The primary text: an artist name, an album title, a station name, a song title.
    pub label: String,
    /// The browse/enqueue uri: `artist/<id>`, `album/<id>`, `song/<id>`, or
    /// `station/<name>` - a station is keyed by its NAME because that is what
    /// `enqueue_uri` resolves it by, and the name is carried VERBATIM (spaces, commas
    /// and all) so a row the daemon offered is a row that enqueues.
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
    /// The RAW artist credit on an album or song row, kept SEPARATE from `trailer` -
    /// which is a COMPOSED display string ("C418   4:05") and would feed garbage into
    /// the library query "go to artist" runs. Empty on an artist row (the artist IS the
    /// row, and its name is the label) and on a station (a stream has no credit).
    pub artist: Option<String>,
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

    /// Jump the cursor to the first row of the next (`+1`) or previous (`-1`) KIND.
    /// Rows are grouped by kind, so this is a section jump: it skips a kind that
    /// returned nothing (there are no rows to land on), and clamps at both ends
    /// rather than wrapping.
    pub fn jump_section(&mut self, delta: i32) {
        if self.hits.rows.is_empty() {
            return;
        }
        // The distinct kinds actually present, in row order.
        let mut kinds: Vec<FindKind> = Vec::new();
        for row in &self.hits.rows {
            if kinds.last() != Some(&row.kind) {
                kinds.push(row.kind);
            }
        }
        let cur = self.hits.rows[self.selected.min(self.hits.rows.len() - 1)].kind;
        let at = kinds.iter().position(|k| *k == cur).unwrap_or(0) as i32;
        let want = (at + delta).clamp(0, kinds.len() as i32 - 1) as usize;
        let target = kinds[want];
        if let Some(i) = self.hits.rows.iter().position(|r| r.kind == target) {
            self.selected = i;
        }
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
                    FindKind::Station => "station",
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

/// Parse a stage-two `searchall <q>` response into hits.
///
/// Three kinds share one flat frame, discriminated by the uri prefix on each
/// `directory:` / `file:` row, plus an `X-Hits` preamble that states each kind's
/// (returned, cap). The preamble is why a kind that returned NOTHING can be shown
/// as an explicit "no artists" instead of being indistinguishable from a kind the
/// query never asked about: over the wire an empty result and an empty filter are
/// both a bare `OK`.
///
/// Artist and album blocks are byte-identical in shape to what `lsinfo` already
/// emits, so this reads them with the same field names the browse path uses.
///
/// Pure: no clock, no I/O.
pub fn parse_searchall_hits(pairs: &[(String, String)]) -> FindHits {
    let mut tallies: Vec<(FindKind, usize, Option<usize>)> = Vec::new();
    let mut artists: Vec<FindRow> = Vec::new();
    let mut albums: Vec<FindRow> = Vec::new();
    let mut stations: Vec<FindRow> = Vec::new();
    let mut songs: Vec<FindRow> = Vec::new();
    let mut cur: Option<Block> = None;

    for (k, v) in pairs {
        match k.as_str() {
            "X-Hits" => {
                if let Some(t) = parse_hits_tally(v) {
                    tallies.push(t);
                }
            }
            "directory" | "file" => {
                flush_block(cur.take(), &mut artists, &mut albums, &mut stations, &mut songs);
                cur = Block::start(v);
            }
            _ => {
                if let Some(b) = cur.as_mut() {
                    b.field(k, v);
                }
            }
        }
    }
    flush_block(cur.take(), &mut artists, &mut albums, &mut stations, &mut songs);

    // Display order is artists, then albums, then saved stations, then songs, so a
    // long song list can never bury what is above it - which is also why no per-kind
    // display caps are needed.
    let mut rows = artists;
    rows.extend(albums);
    rows.extend(stations);
    rows.extend(songs);
    FindHits { rows, tallies }
}

/// `artist 3 20` -> (Artist, 3, Some(20)). A malformed line is DROPPED rather than
/// guessed at: a wrong tally silently mislabels the title.
///
/// The cap is OPTIONAL, which is what lets `station 2` (two tokens) parse: the station
/// match runs daemon-side over the FULL saved set, so there is no server cap to claim
/// and [`Find::tally_title`] correctly never appends "(server cap)" for it.
fn parse_hits_tally(v: &str) -> Option<(FindKind, usize, Option<usize>)> {
    let mut it = v.split_whitespace();
    let kind = match it.next()? {
        "artist" => FindKind::Artist,
        "album" => FindKind::Album,
        "station" => FindKind::Station,
        "song" => FindKind::Song,
        _ => return None,
    };
    let n: usize = it.next()?.parse().ok()?;
    let cap: Option<usize> = it.next().and_then(|c| c.parse().ok());
    Some((kind, n, cap))
}

/// One in-progress result block.
struct Block {
    kind: FindKind,
    uri: String,
    name: String,
    artist: String,
    count: Option<u32>,
    secs: Option<u32>,
    album_uri: Option<String>,
    host: String,
}

impl Block {
    /// Start a block from its uri, classifying by prefix. A uri whose prefix is not
    /// one we model is ignored entirely rather than rendered as an unactionable row.
    ///
    /// A `station/` uri keeps its remainder VERBATIM - never split further - because a
    /// station name is a human label carrying spaces, commas and non-ASCII (his
    /// "Moon Mission Recordings, Tokyo Deep and Electronic"), and that whole remainder
    /// is the key `add station/<name>` resolves by.
    fn start(uri: &str) -> Option<Block> {
        let kind = if uri.starts_with("artist/") {
            FindKind::Artist
        } else if uri.starts_with("album/") {
            FindKind::Album
        } else if uri.starts_with("station/") {
            FindKind::Station
        } else if uri.starts_with("song/") {
            FindKind::Song
        } else {
            return None;
        };
        Some(Block {
            kind,
            uri: uri.to_string(),
            name: String::new(),
            artist: String::new(),
            count: None,
            secs: None,
            album_uri: None,
            host: String::new(),
        })
    }

    fn field(&mut self, k: &str, v: &str) {
        match k {
            // `Artist` is the NAME on an artist block and the credit on album/song
            // blocks - the same key meaning two things, which is why kind is decided
            // by the uri prefix first.
            "Artist" if self.kind == FindKind::Artist => self.name = v.to_string(),
            "Artist" => self.artist = v.to_string(),
            "Album" if self.kind == FindKind::Album => self.name = v.to_string(),
            "Title" => self.name = v.to_string(),
            "X-AlbumCount" | "X-SongCount" => self.count = v.parse().ok(),
            "X-AlbumUri" => self.album_uri = Some(v.to_string()),
            // The stream's host, extracted daemon-side so this stays a dumb column
            // fitter. It is what distinguishes "NTS Infinite Mixtapes 1" from
            // "NTS Radio Live 1" at a glance.
            "X-StreamHost" => self.host = v.to_string(),
            "Time" => self.secs = v.parse().ok(),
            _ => {}
        }
    }
}

fn flush_block(
    b: Option<Block>,
    artists: &mut Vec<FindRow>,
    albums: &mut Vec<FindRow>,
    stations: &mut Vec<FindRow>,
    songs: &mut Vec<FindRow>,
) {
    let Some(b) = b else { return };
    // The RAW credit, captured BEFORE the trailer composes it with a track count or a
    // duration: the trailer is a display string and "go to artist" needs a query term.
    let artist = if b.artist.is_empty() { None } else { Some(b.artist.clone()) };
    let plural = |n: u32, one: &str| {
        if n == 1 {
            format!("{n} {one}")
        } else {
            format!("{n} {one}s")
        }
    };
    let (trailer, bucket): (String, &mut Vec<FindRow>) = match b.kind {
        FindKind::Artist => (b.count.map(|n| plural(n, "album")).unwrap_or_default(), artists),
        FindKind::Album => {
            let mut t = b.artist.clone();
            if let Some(n) = b.count {
                if !t.is_empty() {
                    t.push_str("   ");
                }
                t.push_str(&plural(n, "track"));
            }
            (t, albums)
        }
        // A saved station has no artist, no track count and no duration - a stream has
        // no end. Its host is the only thing that tells two similarly-named stations
        // apart, so that is the trailer.
        FindKind::Station => (b.host.clone(), stations),
        FindKind::Song => {
            let mut t = b.artist.clone();
            if let Some(secs) = b.secs {
                if !t.is_empty() {
                    t.push_str("   ");
                }
                t.push_str(&format!("{}:{:02}", secs / 60, secs % 60));
            }
            (t, songs)
        }
    };
    bucket.push(FindRow {
        kind: b.kind,
        // A block with no name falls back to its uri rather than rendering a blank
        // row the cursor can land on but the eye cannot read.
        label: if b.name.is_empty() { b.uri.clone() } else { b.name },
        uri: b.uri,
        trailer,
        // Real counts here, unlike a stage-one DERIVED album row: this is the
        // album's true track total, so album_mark may legitimately claim Full.
        song_count: if b.kind == FindKind::Album { b.count } else { None },
        album_uri: b.album_uri,
        artist,
    });
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
            artist: None,
        }
    }

    fn pairs(raw: &[(&str, &str)]) -> Vec<(String, String)> {
        raw.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn a_real_navidrome_frame_parses_into_the_screen_the_user_sees() {
        // Captured verbatim from `searchall "el waili"` against the live server on
        // 2026-08-01, trimmed to the first song. A hand-written frame proves the
        // parser matches what I THINK the daemon sends; this proves it matches what
        // the daemon ACTUALLY sends, including the fields the parser ignores.
        let p = pairs(&[
            ("X-Hits", "artist 1 20"),
            ("X-Hits", "album 3 50"),
            ("X-Hits", "song 6 200"),
            ("directory", "artist/1Rtdgeg7gXhuafNJ3koUhu"),
            ("Artist", "El Waili"),
            ("X-AlbumCount", "3"),
            ("directory", "album/3Mf8abx3MlG1V1ke2CPeY9"),
            ("Album", "Toktok El Waili"),
            ("Artist", "El Waili"),
            ("X-SongCount", "1"),
            ("directory", "album/1U83UROeRVdzlHcjtpfEod"),
            ("Album", "[Unknown Album]"),
            ("Artist", "El Waili"),
            ("X-SongCount", "1"),
            ("directory", "album/5UJPX3LK2uOaU9Evh3mzp2"),
            ("Album", "L Nor (3-3)"),
            ("Artist", "El Waili"),
            ("X-SongCount", "3"),
            ("file", "song/Y4CTEObRkzA9nrqa3WYAsm"),
            ("Title", "Manatek"),
            ("Artist", "El Waili"),
            ("Album", "Toktok El Waili"),
            ("X-AlbumUri", "album/3Mf8abx3MlG1V1ke2CPeY9"),
            ("Track", "5"),
            ("Date", "2021"),
            ("MUSICBRAINZ_TRACKID", ""),
            ("Comment", "Visit https://elwaili.bandcamp.com"),
            ("Format", "778kbps"),
            ("Time", "168"),
            ("duration", "168.000"),
        ]);
        let hits = parse_searchall_hits(&p);
        assert_eq!(hits.rows.len(), 5, "1 artist + 3 albums + 1 song");
        assert_eq!(hits.tallies.len(), 3, "every kind states its count and cap");

        let artist = &hits.rows[0];
        assert_eq!(artist.kind, FindKind::Artist);
        assert_eq!(artist.label, "El Waili");
        assert_eq!(artist.uri, "artist/1Rtdgeg7gXhuafNJ3koUhu");
        assert!(artist.trailer.contains("3 albums"));

        // An album whose NAME is bracketed metadata must still render its name, not
        // its id: "[Unknown Album]" is what the server says the album is called.
        assert_eq!(hits.rows[2].label, "[Unknown Album]");
        // A name containing parentheses and digits must survive verbatim.
        assert_eq!(hits.rows[3].label, "L Nor (3-3)");

        let song = &hits.rows[4];
        assert_eq!(song.kind, FindKind::Song);
        assert_eq!(song.label, "Manatek");
        assert!(song.trailer.contains("2:48"), "duration: {:?}", song.trailer);
        assert_eq!(
            song.album_uri.as_deref(),
            Some("album/3Mf8abx3MlG1V1ke2CPeY9"),
            "the queue gutter needs this to mark the row"
        );
        // An EMPTY field the parser does not model must not corrupt the block.
        assert_eq!(song.uri, "song/Y4CTEObRkzA9nrqa3WYAsm");
    }

    #[test]
    fn three_kinds_parse_from_one_frame_discriminated_by_uri_prefix() {
        let p = pairs(&[
            ("X-Hits", "artist 1 20"),
            ("X-Hits", "album 2 50"),
            ("X-Hits", "song 1 200"),
            ("directory", "artist/a1"), ("Artist", "El Waili"), ("X-AlbumCount", "3"),
            ("directory", "album/b1"), ("Album", "Toktok El Waili"),
            ("Artist", "El Waili"), ("X-SongCount", "9"),
            ("directory", "album/b2"), ("Album", "[Unknown Album]"),
            ("Artist", "El Waili"), ("X-SongCount", "1"),
            ("file", "song/s1"), ("Title", "Manatek"), ("Artist", "El Waili"),
            ("Album", "Toktok El Waili"), ("X-AlbumUri", "album/b1"), ("Time", "245"),
        ]);
        let hits = parse_searchall_hits(&p);
        // Artists first, then albums, then songs.
        assert_eq!(hits.rows[0].kind, FindKind::Artist);
        assert_eq!(hits.rows[0].label, "El Waili", "the artist NAME, not its raw id");
        assert_eq!(hits.rows[0].uri, "artist/a1", "a real id, so the row can drill");
        assert!(hits.rows[0].trailer.contains("3 albums"));
        assert_eq!(hits.rows[1].kind, FindKind::Album);
        assert_eq!(hits.rows[1].label, "Toktok El Waili");
        assert!(hits.rows[1].trailer.contains("El Waili"), "{:?}", hits.rows[1].trailer);
        assert!(hits.rows[1].trailer.contains("9 tracks"), "{:?}", hits.rows[1].trailer);
        assert_eq!(hits.rows[3].kind, FindKind::Song);
        assert_eq!(hits.rows[3].label, "Manatek");
        assert!(hits.rows[3].trailer.contains("4:05"), "{:?}", hits.rows[3].trailer);
        assert_eq!(hits.rows[3].album_uri.as_deref(), Some("album/b1"));
    }

    #[test]
    fn a_station_frame_parses_into_a_row_that_enqueues_verbatim() {
        // The FOURTH kind, in the exact shape the daemon emits: `file: station/<name>`
        // (a leaf), `Title:` = the label, `X-StreamHost:` = the trailer. Both halves of
        // this feature had to land together - before it, `Block::start` dropped the
        // unmodelled `station/` prefix and the row VANISHED with no error at all.
        let p = pairs(&[
            ("X-Hits", "artist 0 20"),
            ("X-Hits", "album 0 50"),
            ("X-Hits", "station 1"),
            ("X-Hits", "song 0 200"),
            ("file", "station/NTS 4 To The Floor"),
            ("Title", "NTS 4 To The Floor"),
            ("X-StreamHost", "stream-mixtape-geo.ntslive.net"),
        ]);
        let hits = parse_searchall_hits(&p);
        assert_eq!(hits.rows.len(), 1);
        let st = &hits.rows[0];
        assert_eq!(st.kind, FindKind::Station);
        assert_eq!(st.label, "NTS 4 To The Floor");
        assert_eq!(
            st.uri, "station/NTS 4 To The Floor",
            "the uri `enqueue_uri` already resolves, name and spaces intact"
        );
        assert_eq!(st.trailer, "stream-mixtape-geo.ntslive.net");
        assert_eq!(st.song_count, None, "a stream has no track total");
        assert_eq!(st.album_uri, None, "and no owning album to mark");
        assert_eq!(hits.tallies[2], (FindKind::Station, 1, None), "two tokens, no cap");
    }

    #[test]
    fn a_station_name_with_a_comma_and_spaces_round_trips_verbatim() {
        // His real collection carries "Moon Mission Recordings, Tokyo Deep and
        // Electronic". The remainder after `station/` is the WHOLE name and is never
        // split further - it is the key `add station/<name>` resolves by, so a byte
        // lost here is a row that cannot be played.
        const NAME: &str = "Moon Mission Recordings, Tokyo Deep and Electronic";
        let p = pairs(&[
            ("file", &format!("station/{NAME}")),
            ("Title", NAME),
            ("X-StreamHost", "uk5.internet-radio.com"),
        ]);
        let hits = parse_searchall_hits(&p);
        assert_eq!(hits.rows[0].uri, format!("station/{NAME}"));
        assert_eq!(hits.rows[0].label, NAME);
    }

    #[test]
    fn stations_render_after_albums_and_before_songs() {
        // The order IS the display order (nothing sorts by it - this concat is it), and
        // it is why 22 stations can never be buried under a 200-song page while an
        // ordinary query, which matches no station at all, leaves the top untouched.
        let p = pairs(&[
            ("directory", "artist/a1"), ("Artist", "NTS"),
            ("directory", "album/b1"), ("Album", "NTS Sessions"),
            // Emitted by the daemon between the albums and the songs; asserted here
            // independently of that so the CLIENT order is pinned on its own.
            ("file", "station/NTS Radio Live 1"), ("Title", "NTS Radio Live 1"),
            ("X-StreamHost", "stream-relay-geo.ntslive.net"),
            ("file", "song/s1"), ("Title", "Sweden"),
        ]);
        let hits = parse_searchall_hits(&p);
        let kinds: Vec<FindKind> = hits.rows.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![FindKind::Artist, FindKind::Album, FindKind::Station, FindKind::Song]
        );
        // The derived Ord must AGREE with that display order, or the doc lies.
        assert!(FindKind::Album < FindKind::Station && FindKind::Station < FindKind::Song);
    }

    #[test]
    fn a_station_tally_has_no_server_cap_to_claim() {
        // Two tokens, because the station match runs daemon-side over the FULL saved
        // set. Claiming a cap here would invent a ceiling that does not exist.
        let mut f = Find::default();
        f.hits.tallies = vec![
            (FindKind::Album, 0, Some(50)),
            (FindKind::Station, 2, None),
            (FindKind::Song, 200, Some(200)),
        ];
        assert_eq!(f.tally_title(), "no albums / 2 stations / 200 songs (server cap)");
        // The cap suffix belongs to the SONG line; a station alone never earns it.
        let mut f = Find::default();
        f.hits.tallies = vec![(FindKind::Station, 2, None)];
        assert_eq!(f.tally_title(), "2 stations");
        let mut f = Find::default();
        f.hits.tallies = vec![(FindKind::Station, 1, None)];
        assert_eq!(f.tally_title(), "1 station");
    }

    #[test]
    fn an_omitted_station_tally_makes_no_claim_at_all() {
        // The daemon OMITS the line (never `station 0`) when the internet-radio endpoint
        // could not be reached: absence of a claim beats a false claim. The title must
        // then simply have no station clause, rather than inventing "no stations" about
        // a set nobody saw.
        let p = pairs(&[("X-Hits", "artist 0 20"), ("X-Hits", "album 1 50"), ("X-Hits", "song 3 200")]);
        let hits = parse_searchall_hits(&p);
        let mut f = Find::default();
        f.hits = hits;
        assert_eq!(f.tally_title(), "no artists / 1 album / 3 songs");
        assert!(!f.tally_title().contains("station"));
    }

    #[test]
    fn a_real_album_row_carries_its_true_track_total() {
        // Unlike a derived row, this count is the album's ACTUAL total, so
        // album_mark may legitimately claim Full.
        let p = pairs(&[("directory", "album/b1"), ("Album", "X"), ("X-SongCount", "9")]);
        let hits = parse_searchall_hits(&p);
        assert_eq!(hits.rows[0].song_count, Some(9));
    }

    #[test]
    fn artist_key_means_the_name_on_an_artist_row_and_the_credit_elsewhere() {
        // The same wire key means two things, which is why kind is decided by the
        // uri prefix BEFORE any field is read.
        let p = pairs(&[
            ("directory", "artist/a1"), ("Artist", "C418"),
            ("file", "song/s1"), ("Title", "Sweden"), ("Artist", "C418"),
        ]);
        let hits = parse_searchall_hits(&p);
        assert_eq!(hits.rows[0].label, "C418", "artist row: Artist is the name");
        assert_eq!(hits.rows[1].label, "Sweden", "song row: Title is the name");
        assert!(hits.rows[1].trailer.contains("C418"), "song row: Artist is the credit");
    }

    #[test]
    fn the_raw_credit_is_kept_beside_the_composed_trailer_never_read_out_of_it() {
        // "go to artist" runs a real library QUERY, and the trailer is a COMPOSED
        // display string ("C418   4:05") that would feed garbage into it. So the raw
        // credit rides its own field, and the trailer keeps composing exactly as it did.
        let p = pairs(&[
            ("file", "song/s1"), ("Title", "Sweden"), ("Artist", "C418"), ("Time", "245"),
            ("directory", "album/b1"), ("Album", "Volume Alpha"), ("Artist", "C418"),
            ("X-SongCount", "24"),
            ("directory", "artist/a1"), ("Artist", "C418"),
            ("directory", "station/NTS 1"), ("X-StreamHost", "ntslive.net"),
        ]);
        let hits = parse_searchall_hits(&p);
        let by = |k: FindKind| hits.rows.iter().find(|r| r.kind == k).unwrap();
        assert_eq!(by(FindKind::Song).artist.as_deref(), Some("C418"));
        assert_eq!(by(FindKind::Song).trailer, "C418   4:05", "the trailer still composes");
        assert_eq!(by(FindKind::Album).artist.as_deref(), Some("C418"));
        assert_eq!(by(FindKind::Album).trailer, "C418   24 tracks");
        // An artist row IS the artist (its name is the label), and a station has no
        // credit at all - neither has a separate artist to go to.
        assert_eq!(by(FindKind::Artist).artist, None);
        assert_eq!(by(FindKind::Station).artist, None);
    }

    #[test]
    fn x_hits_carries_a_zero_kind_so_absence_can_be_stated() {
        let p = pairs(&[("X-Hits", "artist 0 20"), ("X-Hits", "song 5 200")]);
        let hits = parse_searchall_hits(&p);
        assert_eq!(hits.tallies[0], (FindKind::Artist, 0, Some(20)));
        assert_eq!(hits.tallies[1], (FindKind::Song, 5, Some(200)));
        assert!(hits.rows.is_empty());
    }

    #[test]
    fn a_malformed_or_unknown_hits_line_is_dropped_not_guessed_at() {
        let p = pairs(&[
            ("X-Hits", "artist notanumber 20"),
            ("X-Hits", "sandwich 3 20"),
            ("X-Hits", "song 5 200"),
        ]);
        let hits = parse_searchall_hits(&p);
        assert_eq!(hits.tallies.len(), 1, "only the well-formed line survives");
        assert_eq!(hits.tallies[0].0, FindKind::Song);
    }

    #[test]
    fn a_uri_prefix_we_do_not_model_is_ignored_entirely() {
        // Better no row than a row the cursor can land on and no verb can act on.
        let p = pairs(&[
            ("directory", "playlist/p1"), ("Album", "Some Playlist"),
            ("file", "song/s1"), ("Title", "Sweden"),
        ]);
        let hits = parse_searchall_hits(&p);
        assert_eq!(hits.rows.len(), 1);
        assert_eq!(hits.rows[0].uri, "song/s1");
    }

    #[test]
    fn a_block_with_no_name_falls_back_to_its_uri_rather_than_a_blank_row() {
        let hits = parse_searchall_hits(&pairs(&[("file", "song/s42")]));
        assert_eq!(hits.rows[0].label, "song/s42");
    }

    #[test]
    fn an_empty_frame_parses_to_nothing_without_panicking() {
        let hits = parse_searchall_hits(&[]);
        assert!(hits.rows.is_empty());
        assert!(hits.tallies.is_empty());
    }

    #[test]
    fn section_jump_moves_between_kinds_and_skips_absent_ones() {
        let mut f = Find::default();
        f.hits.rows = vec![
            row(FindKind::Album, "alb1"),
            row(FindKind::Album, "alb2"),
            row(FindKind::Song, "s1"),
            row(FindKind::Song, "s2"),
        ];
        // Artists returned nothing, so there is no artist section to land on and the
        // jump goes straight from albums to songs.
        f.jump_section(1);
        assert_eq!(f.selected, 2, "landed on the first SONG row");
        f.jump_section(1);
        assert_eq!(f.selected, 2, "clamps at the last section, never wraps");
        f.jump_section(-1);
        assert_eq!(f.selected, 0, "back to the first album row");
        f.jump_section(-1);
        assert_eq!(f.selected, 0, "clamps at the first section too");
    }

    #[test]
    fn section_jump_from_mid_section_lands_on_the_next_sections_head() {
        let mut f = Find::default();
        f.hits.rows = vec![
            row(FindKind::Album, "alb1"),
            row(FindKind::Album, "alb2"),
            row(FindKind::Song, "s1"),
        ];
        f.selected = 1;
        f.jump_section(1);
        assert_eq!(f.selected, 2);
    }

    #[test]
    fn section_jump_on_an_empty_hit_list_is_a_no_op() {
        let mut f = Find::default();
        f.jump_section(1);
        assert_eq!(f.selected, 0);
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
        assert_eq!(FindKind::Station.sigil(), ')');
        assert_eq!(FindKind::Song.sigil(), ' ');
        // All four ASCII, all four distinct, and none of them a queue mark (`#`/`~`)
        // sharing the same three-column gutter.
        let sigils = [
            FindKind::Artist.sigil(),
            FindKind::Album.sigil(),
            FindKind::Station.sigil(),
            FindKind::Song.sigil(),
        ];
        assert!(sigils.iter().all(|c| c.is_ascii()), "a non-ASCII glyph would shift the column");
        for (i, a) in sigils.iter().enumerate() {
            for b in &sigils[i + 1..] {
                assert_ne!(a, b, "two kinds sharing a sigil makes the gutter meaningless");
            }
            assert!(*a != '#' && *a != '~', "clashes with a queue mark in the same gutter");
        }
    }
}
