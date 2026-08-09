//! The pure, testable core of the jukebox TUI: state, the key -> intent mapping,
//! the command-vs-NL routing reused from hypodj-client, and the confirm state
//! machine. NO terminal, NO network - crossterm KeyEvents come in, Intents go out,
//! and the event loop in main.rs does all the IO.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use hypodj_client::model::{NowPlaying, QueueItem};
use hypodj_client::nl::not_understood_hint;
use hypodj_client::route::{route, Action};

use crate::find::{Find, Focus};
use crate::keymap;
use crate::menu::{classify, ArtistRef, Avail, Menu, MenuAction, Origin, PlayHow, Target, TargetKind};

/// Vim-style scrolloff: keep this many rows of context above/below the cursor.
const SCROLLOFF: usize = 3;

/// Scrub step in seconds for ctrl+f (forward) / ctrl+b (back).
const SCRUB_STEP: i32 = 5;

/// Incremental jump-to-match search over an active list. Case-insensitive
/// `contains`, scanning from `origin` forward and wrapping once through the whole
/// list. Pure and testable - no self, no IO.
///
/// - empty query -> `Some(origin)` (cursor stays put)
/// - no match -> `None`
/// - empty list -> `None`
pub fn search_jump(labels: &[&str], query: &str, origin: usize) -> Option<usize> {
    if query.is_empty() {
        return Some(origin);
    }
    search_step(labels, query, origin, true)
}

/// Scan for the next case-insensitive `contains` match starting AT `origin`,
/// stepping forward (`forward`) or backward, wrapping once through the whole list.
/// Pure and testable - the shared engine behind `search_jump` (forward from the
/// origin) and the `n`/`N` repeat-search jumps (which step off the current match).
///
/// - empty query -> `None`
/// - empty list -> `None`
/// - no match -> `None`
pub fn search_step(labels: &[&str], query: &str, origin: usize, forward: bool) -> Option<usize> {
    if query.is_empty() {
        return None;
    }
    let n = labels.len();
    if n == 0 {
        return None;
    }
    let q = query.to_lowercase();
    let start = origin % n;
    for step in 0..n {
        let i = if forward {
            (start + step) % n
        } else {
            (start + n - (step % n)) % n
        };
        if labels[i].to_lowercase().contains(&q) {
            return Some(i);
        }
    }
    None
}

/// Derive the top visible row for a scrolloff viewport. Pure and testable: given
/// the selected row `sel`, the queue length `n`, the viewport height `h`, and the
/// previous offset `prev`, return the new top row.
///
/// - Top-edge exception: when `sel < so` the cursor reaches literal row 0 with no
///   top buffer (falls out of the saturating_sub).
/// - Bottom reachable: the offset is clamped to `n - h`, so the cursor advances
///   into the bottom margin to reach the last row.
/// - Mid-list the cursor pins (at `h-1-so` going down, `so` going up) while the
///   list scrolls underneath.
/// - In a tiny viewport `so` shrinks so the top/bottom margins never overlap.
pub fn scroll_offset(sel: usize, n: usize, h: usize, prev: usize) -> usize {
    if n == 0 || h == 0 {
        return 0;
    }
    let so = SCROLLOFF.min(h.saturating_sub(1) / 2);
    let max_off = n.saturating_sub(h);
    let mut off = prev;
    if sel < off + so {
        off = sel.saturating_sub(so);
    }
    if sel + so >= off + h {
        off = (sel + so + 1).saturating_sub(h);
    }
    off.min(max_off)
}

/// Group a server browse pair list into rows. A `directory:` pair starts a dir row
/// (label refined by a following `Album`/`Artist`/`Genre` value, else the path
/// tail); a `file:` pair starts a song row (label from `Title`, with ` - <artist>`
/// appended); a `playlist:` pair becomes a name row for the Playlists screen. Pure
/// and testable - mirrors the boundary logic of client model.rs::group_blocks.
pub fn parse_browse(pairs: &[(String, String)]) -> Vec<BrowseRow> {
    let mut rows: Vec<BrowseRow> = Vec::new();
    for (k, v) in pairs {
        match k.as_str() {
            "directory" => rows.push(BrowseRow {
                label: path_tail(v).to_string(),
                uri: v.clone(),
                is_dir: true,
                song_count: None,
                artist: None,
                album_uri: None,
            }),
            "file" => rows.push(BrowseRow {
                label: path_tail(v).to_string(),
                uri: v.clone(),
                is_dir: false,
                song_count: None,
                artist: None,
                album_uri: None,
            }),
            "playlist" => rows.push(BrowseRow {
                label: v.clone(),
                uri: v.clone(),
                is_dir: false,
                song_count: None,
                artist: None,
                album_uri: None,
            }),
            "Album" | "Genre" => {
                if let Some(last) = rows.last_mut() {
                    if last.is_dir {
                        last.label = v.clone();
                    }
                }
            }
            "X-SongCount" => {
                if let Some(last) = rows.last_mut() {
                    if last.is_dir {
                        last.song_count = v.parse().ok();
                    }
                }
            }
            "Title" => {
                if let Some(last) = rows.last_mut() {
                    if !last.is_dir {
                        last.label = v.clone();
                    }
                }
            }
            // The credit is folded into the label the eye reads AND kept raw, because
            // the composed `"<title> - <artist>"` is a display string: querying with it
            // would find nothing, so "go to artist" needs the name on its own.
            "Artist" => {
                if let Some(last) = rows.last_mut() {
                    if !last.is_dir {
                        last.label = format!("{} - {}", last.label, v);
                        last.artist = Some(v.clone());
                    }
                }
            }
            // The owning album of a song row. `lsinfo` already carries it (the daemon's
            // `push_song_tags` emits it beside `Album`), so an opened album's rows can
            // reach their album exactly as a queue row can.
            "X-AlbumUri" => {
                if let Some(last) = rows.last_mut() {
                    if !last.is_dir {
                        last.album_uri = Some(v.clone());
                    }
                }
            }
            _ => {}
        }
    }
    rows
}

/// How much of an album currently sits in the queue, for the browse gutter marker.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum QueueMark {
    None,
    Partial,
    Full,
}

/// Classify an album's queue presence from the count of its DISTINCT queued songs
/// and its total track count. Pure and testable.
///
/// - `queued_count == 0` -> [`QueueMark::None`]
/// - a known `song_count > 0` with `queued_count >= song_count` -> [`QueueMark::Full`]
/// - otherwise (some queued, but fewer than the count OR the count is unknown/0)
///   -> [`QueueMark::Partial`]
///
/// The unknown/`0` songCount case degrades to Partial for any queued track - never
/// a false Full. Because the caller counts DISTINCT queued song ids, a duplicated
/// queued track cannot inflate the count past the album size.
pub fn album_mark(queued_count: usize, song_count: Option<u32>) -> QueueMark {
    if queued_count == 0 {
        return QueueMark::None;
    }
    match song_count {
        Some(n) if n > 0 && queued_count >= n as usize => QueueMark::Full,
        _ => QueueMark::Partial,
    }
}

/// The single ASCII gutter glyph for a browse row's queue state (`#` full, `~`
/// partial, ` ` none). ASCII so terminals without good unicode still render it.
pub fn queue_mark_glyph(mark: QueueMark) -> char {
    match mark {
        QueueMark::Full => '#',
        QueueMark::Partial => '~',
        QueueMark::None => ' ',
    }
}

/// Drop a present-but-BLANK credit. An empty `Artist:` pair is on the wire as often
/// as a missing one, and "go to artist" turns its value into a real library query - so
/// a blank has to read as ABSENT ("this listing carries no artist") rather than build a
/// live row that submits nothing and closes the menu with no feedback.
fn named(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

/// The last `/`-separated segment of a browse path, used as a fallback row label.
fn path_tail(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Which input surface has focus.
#[derive(Debug, PartialEq, Eq)]
pub enum Mode {
    /// Keybindings + queue navigation.
    Normal,
    /// The bottom command line (bare verb OR natural-language phrase).
    Command,
    /// Incremental jump-to-match search over the active list (`/`).
    Search,
    /// The y/N confirm popup for an armed plan (NL echo) or a destructive verb.
    Confirm,
}

/// A plan awaiting confirmation. Either an owner-scoped NL `token` (confirm via
/// `nl confirm <token>`) OR a direct `command` (e.g. destructive `clear`).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pending {
    pub token: Option<String>,
    pub command: Option<String>,
    pub steps: Vec<String>,
    pub note: Option<String>,
    /// The "via rules" / "via local model" trust footnote from the nl echo.
    pub trust: Option<String>,
}

/// Which main view is showing. Queue is the live-refreshed default; Albums and
/// Playlists are lazily-fetched browse screens; Dj is the Claude Code intelligence
/// pane (right of Queue) that translates a typed NL query into a plan to confirm.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Screen {
    Queue,
    Albums,
    Playlists,
    Dj,
    /// The library query surface. Named Find in code because every `Search`
    /// identifier is already the `/` cursor jump; the tab strip says Search.
    Find,
}

/// One row in a browse list. `uri` is the server browse path (`album/<id>`,
/// `song/<id>`, `list/<name>`) for Albums, or the playlist NAME for Playlists.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct BrowseRow {
    pub label: String,
    pub uri: String,
    pub is_dir: bool,
    /// Total track count for an album dir row, from the daemon's non-standard
    /// `X-SongCount` pair. Drives the full-vs-partial queue marker; `None` when the
    /// listing does not carry it (song rows, playlists, missing count).
    pub song_count: Option<u32>,
    /// The artist credit on a song row (`Artist`), kept alongside the label the same
    /// pair is folded into, so "go to artist" has a NAME to query with instead of the
    /// composed `"<title> - <artist>"` display string.
    pub artist: Option<String>,
    /// The owning album on a song row (`X-AlbumUri`), so "go to album" works from a
    /// drilled listing exactly as it does from the queue.
    pub album_uri: Option<String>,
}

/// A self-contained browse list with its own cursor, scroll offset, nav stack, and
/// lazy-fetch guard. One per browse screen so cursors are independent.
#[derive(Debug)]
pub struct Browse {
    pub rows: Vec<BrowseRow>,
    pub selected: usize,
    /// The lsinfo path this list currently shows (root default per screen).
    pub path: String,
    /// Display title for the pane header.
    pub title: String,
    /// (path, cursor) of each ancestor level, for BrowseBack.
    pub stack: Vec<(String, usize)>,
    /// Lazy-fetch guard: false until the first ShowScreen fetch lands.
    pub loaded: bool,
    /// Top visible row for the scrolloff viewport (see [`scroll_offset`]).
    pub offset: Cell<usize>,
}

impl Browse {
    pub(crate) fn new(path: &str, title: &str) -> Self {
        Browse {
            rows: Vec::new(),
            selected: 0,
            path: path.to_string(),
            title: title.to_string(),
            stack: Vec::new(),
            loaded: false,
            offset: Cell::new(0),
        }
    }

    /// Replace the rows for a freshly-fetched level, resetting cursor + scroll.
    pub fn apply(&mut self, rows: Vec<BrowseRow>, path: String, title: String) {
        self.rows = rows;
        self.selected = 0;
        self.offset.set(0);
        self.path = path;
        self.title = title;
        self.loaded = true;
    }

    fn move_selection(&mut self, delta: i32) {
        if self.rows.is_empty() {
            self.selected = 0;
            return;
        }
        let last = self.rows.len() - 1;
        let next = self.selected as i32 + delta;
        self.selected = next.clamp(0, last as i32) as usize;
    }
}

/// The side-effecting request handle_key emits for the event loop to run. IO lives
/// entirely in the loop; the state machine only ever returns one of these.
#[derive(Debug, PartialEq, Eq)]
pub enum Intent {
    /// Run one MPD command line, then refresh.
    Command(String),
    /// Send the phrase through the NL handshake, then enter_confirm on the echo.
    Nl(String),
    /// Re-read status + currentsong + playlistinfo.
    Refresh,
    /// Confirm the pending plan (arm it).
    ConfirmArm,
    /// Cancel the pending plan.
    ConfirmCancel,
    /// Switch the main view; main.rs lazily fetches the screen if not loaded.
    ShowScreen(Screen),
    /// Drill into a browse directory (fetch its children via lsinfo <uri>).
    BrowseInto(String),
    /// Pop one browse level and re-fetch the parent.
    BrowseBack,
    /// Enqueue a browse uri (`add <uri>`), optionally play the new tail.
    Enqueue { uri: String, play: bool },
    /// Load a playlist by name (`load <name>`), appending to the queue.
    LoadPlaylist(String),
    /// Run a library query on the dedicated find socket. NOT a mutation, so it must
    /// never set `sent_mutation` and never trail a `request_refresh`.
    Find(String),
    /// Translate a DJ View NL query via the Claude Code backend (on the dedicated CC
    /// worker thread, never the command socket), ending in a Confirm popup.
    Cc(String),
    /// Leave the session.
    Quit,
}

/// The relative-seek delta of a `seekcur +N` / `seekcur -N` command line, or None
/// if it is not a relative scrub. Used to coalesce a held-scrub burst.
fn seekcur_delta(line: &str) -> Option<i32> {
    let rest = line.strip_prefix("seekcur ")?;
    if rest.starts_with('+') || rest.starts_with('-') {
        rest.parse::<i32>().ok()
    } else {
        None
    }
}

/// Coalesce a frame's drained intents so a burst of held-key autorepeat collapses
/// into ONE real action instead of a backlog. Consecutive relative scrubs
/// (`seekcur +/-N`) SUM into a single seek; a run of IDENTICAL `radio` lines
/// collapses to one (holding `r` is one gesture, not forty - and unlike `>`/next,
/// repeating it says nothing new); everything else passes through in order. This is
/// what makes holding a key track the finger and stop the instant it is released -
/// the loop then applies the REAL summed effect (no faked UI preview, no queued
/// backlog draining after release). Pure and testable.
pub fn coalesce_intents(intents: Vec<Intent>) -> Vec<Intent> {
    let mut out: Vec<Intent> = Vec::new();
    let mut scrub: i32 = 0;
    for it in intents {
        if let Intent::Command(line) = &it {
            if let Some(d) = seekcur_delta(line) {
                scrub = scrub.saturating_add(d);
                continue;
            }
            // A repeat of the SAME radio gesture in one frame is the same press.
            if line.starts_with("radio") && out.last() == Some(&it) {
                continue;
            }
        }
        if scrub != 0 {
            out.push(scrub_intent(scrub));
            scrub = 0;
        }
        out.push(it);
    }
    if scrub != 0 {
        out.push(scrub_intent(scrub));
    }
    out
}

/// Decide whether an inbound `idle` wake should enqueue a `Refresh`, given whether
/// a refresh is already in flight. A wake with nothing in flight starts one (returns
/// true, caller sets the in-flight bool); a wake while one is in flight is dropped
/// (returns false) so a wake-storm - e.g. a fade ramp firing `changed` on every
/// volume step - collapses to a SINGLE version-gated refresh. Pure and testable.
pub fn wake_wants_refresh(refresh_in_flight: bool) -> bool {
    !refresh_in_flight
}

/// Whether an inbound worker response tagged with `resp_epoch` is stale and must be
/// dropped, given the render thread's current `epoch`. The epoch bumps on every
/// reconnect, so a response computed against a since-dead socket (epoch strictly
/// less than current) is discarded rather than folded into a fresh connection's
/// state. Pure and testable.
pub fn resp_is_stale(resp_epoch: u64, current_epoch: u64) -> bool {
    resp_epoch < current_epoch
}

/// Build a single coalesced relative-seek intent (`seekcur +N` keeps the sign).
fn scrub_intent(secs: i32) -> Intent {
    let arg = if secs >= 0 {
        format!("+{secs}")
    } else {
        secs.to_string()
    };
    Intent::Command(format!("seekcur {arg}"))
}

pub struct TuiState {
    pub now: NowPlaying,
    pub queue: Vec<QueueItem>,
    pub selected: usize,
    /// The active main view.
    pub screen: Screen,
    /// The Albums browse screen (the flat A-Z index; the smart lists are a row in it).
    pub albums: Browse,
    /// The Playlists browse screen (server currently exposes only `Starred`).
    pub playlists: Browse,
    /// The Find (library query) screen.
    pub find: Find,
    /// Top visible queue row, derived in render (where the viewport height is
    /// known) via [`scroll_offset`] and persisted here so scroll state survives
    /// across frames. Interior-mutable so the render (which holds `&TuiState`)
    /// can write the freshly computed offset back.
    pub offset: Cell<usize>,
    pub mode: Mode,
    pub input: String,
    pub pending: Option<Pending>,
    pub status_msg: Option<String>,
    pub connected: bool,
    /// The MPD queue version (`playlist:` in `status`) of the currently-held
    /// `queue`. A refresh re-fetches the (expensive) full `playlistinfo` ONLY when
    /// this changes, so the common actions that never touch the queue (fav, volume,
    /// pause, seek) cost two cheap commands instead of a whole-queue round-trip.
    pub queue_version: Option<u64>,
    /// Decoded cover art for the current track, cached by its uri (fetched on a
    /// dedicated connection when the track changes). `None` for a stream, missing
    /// art, or a fetch/decode failure - the art panel then shows a placeholder.
    pub art: Option<crate::art::AlbumArt>,
    /// The active cursor saved when `/` is pressed, so Esc can restore it after a
    /// non-destructive jump-to-match search.
    pub search_origin: usize,
    /// The last ACCEPTED search query (set on Enter, cleared on a new `/` and on a
    /// screen change). Drives `n`/`N` repeat jumps and the standing substring
    /// highlight while in Normal mode; empty means no standing search.
    pub last_search: String,
    /// True while a `Req::Refresh` is outstanding on the worker (set when one is
    /// sent, cleared when its `Snapshot` lands). Gates wake-driven refreshes so a
    /// wake-storm collapses to one refresh (see [`wake_wants_refresh`]).
    pub refresh_in_flight: bool,
    /// Set when a wake (or a mutation-driven refresh request) is suppressed because a
    /// refresh is already in flight. The outstanding refresh may have read the server
    /// state BEFORE the suppressed change landed, so when its Snapshot (or a Banner
    /// that clears the gate) arrives we re-arm exactly one more refresh to catch up.
    /// Without this a lost wake is not reflected until the 5s safety-net refresh.
    pub refresh_dirty: bool,
    /// The connection epoch, bumped on every worker reconnect. A response tagged
    /// with an older epoch is stale and dropped (see [`resp_is_stale`]).
    pub epoch: u64,
    /// The art-request KEY the art thread was last asked to fetch: `(file uri,
    /// recognized cover url)`. The render thread sends one art request per KEY
    /// change, so a stream gaining a cover (`None` -> `Some(url)`) or a re-identify
    /// swapping the cover on the same uri each fires exactly one fetch, never per
    /// frame (task kmrhj8m).
    pub art_req_key: Option<(String, Option<String>)>,
    /// The ambient-visualizer clock, in seconds. The render loop advances this by
    /// the wall-clock frame delta ONLY while playback is `play` (so it freezes when
    /// paused/stopped) and writes it here before each draw; the idle bottom-bar wave
    /// reads it as its animation phase. Pure display state - no key/logic meaning.
    pub anim_secs: f64,
    /// Free-running animation clock, advanced EVERY frame regardless of play state
    /// (unlike `anim_secs`, which freezes when paused). Drives the DJ "thinking..."
    /// spinner so it keeps rotating while a CC call is in flight even on a paused
    /// or stopped deck. Pure display state - no key/logic meaning.
    pub spin_secs: f64,
    /// The DJ View "ask>" input line (the NL query being typed on Screen::Dj).
    pub dj_input: String,
    /// The DJ View scrollback: coarse CC progress + result lines, newest at the
    /// bottom. Bounded so a long session never grows without limit.
    pub dj_log: Vec<String>,
    /// The current CC phase line (e.g. "thinking..."), shown next to a spinner while
    /// a call is in flight; `None` when idle.
    pub dj_phase: Option<String>,
    /// Whether the REAL post-gain level wave is live this frame (the viz socket is
    /// connected and a frame has landed). `false` => the render draws the decorative
    /// fallback wave. Set by the render loop from the viz worker's slot.
    pub viz_active: bool,
    /// The smoothed normalized level A in `[0, 1]` (the ballistics envelope output),
    /// persisted across frames so the one-pole attack/release integrates over time.
    pub viz_env: f32,
    /// Whether the daemon reports audio is playing (from the latest viz frame); gates
    /// the level wave between the live field and the resting hairline.
    pub viz_playing: bool,
    /// The open row CONTEXT MENU, if any. An OVERLAY like `help_open`, never a fifth
    /// [`Mode`]: `Mode` is the text-routing discriminant (a typed buffer plus a caret)
    /// and the menu has neither, so a variant there would force a dead arm into every
    /// `match self.mode`. Intercepted at the very top of `key_normal` (above the help
    /// intercept, which `open_menu` closes), so while it is open it is a true modal and
    /// its rows can never describe a row the cursor has since left.
    pub menu: Option<Menu>,
    /// Whether the `?` help overlay is open. Normal-mode-only modal: while open, only
    /// `?`/Esc/q resolve (toggle-close), everything else is swallowed.
    pub help_open: bool,
    /// The help overlay's vertical scroll offset (rows). Nonzero only when the overlay
    /// is taller than the terminal; nav keys scroll it and the renderer clamps it to the
    /// real max so a short terminal can still reach every binding. Reset when help opens.
    pub help_scroll: u16,
    /// The daemon's `heard` read-back, one entry per rendered line, or empty when the
    /// overlay has never been asked for. THE TAPE'S ONLY WINDOW in this process: `mark`
    /// keeps audio, and the segment outlives by weeks the one-line banner that announced
    /// it, so the read-back needs a surface with more than one row. Rendered daemon-side
    /// (it joins ledger text to the audio actually on disk), so these are printed
    /// verbatim and in order and this process interprets nothing.
    pub heard_lines: Vec<String>,
    /// Whether the heard/tape overlay is open. A normal-mode modal in the exact shape of
    /// [`Self::help_open`]: `t`/Esc/q close it, j/k/PgUp/PgDn scroll, everything else is
    /// swallowed. Modal rather than a transient banner because reading a ledger takes
    /// longer than the next keypress.
    pub heard_open: bool,
    /// The overlay's vertical scroll offset (rows), clamped by the renderer against the
    /// real content so a short terminal can still reach the last row. Reset on open.
    pub heard_scroll: u16,
    /// The detected terminal background (OSC 11 at startup / on resize), seeded to the
    /// guaranteed dark default so the visual system always has a bg to contrast against.
    pub term_bg: crate::album_color::TermBg,
    /// The detected inline-image protocol; `None` => the album sigil is drawn in the
    /// album-art slot's image-less path.
    pub image_protocol: crate::album_color::ImageProtocol,

    /// Cell size in pixels, set only when the terminal both ADVERTISED sixel and
    /// reported a usable pixel geometry.
    ///
    /// `None` is the default and the fallback, so every terminal that does not answer,
    /// answers without parameter 4, or reports a zero-sized cell keeps the sextant
    /// renderer without any special casing. It is also why the existing TestBackend
    /// tests still exercise the cell path unchanged.
    pub sixel_cell_px: Option<(u16, u16)>,
    /// Whether an overlay (menu, help, heard, confirm) was painted on the PREVIOUS
    /// frame. A sixel image is cell-anchored: a popup drawn over the cover prints text
    /// into cells the terminal was showing image pixels in, and those pixels are gone.
    /// Closing the popup does not bring them back on its own - the cells revert to the
    /// blank-and-skipped state the image painter left them in, and the backend never
    /// draws a skipped cell, so the cover keeps the popup's silhouette punched out of
    /// it until the track changes. See [`sixel_gen`].
    pub sixel_covered: Cell<bool>,
    /// Flips whenever a covered image needs to be re-sent, and is mixed into the payload
    /// cell's symbol so it DIFFERS from the one already on screen.
    ///
    /// This is the whole repair: ratatui re-sends a cell only when it changes, and the
    /// image painter writes a byte-identical payload every frame, so the diff would
    /// otherwise conclude there is nothing to do and leave the holes. Flipping the
    /// symbol makes the diff re-send the image, and a sixel payload paints its ENTIRE
    /// pixel rect, so one re-send fills every hole at once. A single frame, with no
    /// blank flash in between, which a clear-then-repaint would cost.
    pub sixel_gen: Cell<bool>,
    /// Whether the terminal advertises truecolor (else colors quantize to xterm-256).
    pub truecolor: bool,
    /// The cached album sigil, rebuilt only when the album identity changes (static -
    /// never regenerated per frame).
    pub sigil: Option<crate::sigil::Sigil>,
}

/// Perceptual floor / ceiling (dBFS) for the level normalize. Below the floor is
/// the resting hairline; the loudest music tops out at the ceiling.
pub const VIZ_FLOOR_DB: f32 = -54.0;
pub const VIZ_CEIL_DB: f32 = -6.0;

/// Normalize an audible post-gain level (dBFS) into `[0, 1]` in the perceptual dB
/// domain, with a gentle gamma expand of the quiet range so verses do not flatline.
/// Pure and testable.
pub fn normalize_level(post_gain_db: f32) -> f32 {
    let a = ((post_gain_db - VIZ_FLOOR_DB) / (VIZ_CEIL_DB - VIZ_FLOOR_DB)).clamp(0.0, 1.0);
    a.powf(0.8)
}

/// One asymmetric one-pole envelope step on the normalized level, computed at
/// render `dt` (seconds): quick attack (~60 ms) so a swell feels causal, slow
/// release (~350 ms) so it falls like a needle with gravity and never snaps. A fade
/// (falling target) rides the release tau, so the field settles with the audible
/// sound. Pure and testable (deterministic in `dt`, no wall clock).
pub fn envelope_step(prev: f32, target: f32, dt: f32) -> f32 {
    let tau = if target >= prev { 0.060 } else { 0.350 };
    // alpha = 1 - exp(-dt/tau); guard a zero/negative dt.
    let alpha = if dt <= 0.0 { 0.0 } else { 1.0 - (-dt / tau).exp() };
    prev + (target - prev) * alpha.clamp(0.0, 1.0)
}

impl Default for TuiState {
    fn default() -> Self {
        TuiState {
            now: NowPlaying::default(),
            queue: Vec::new(),
            selected: 0,
            screen: Screen::Queue,
            albums: Browse::new("albums/all", "Albums"),
            playlists: Browse::new("", "Playlists"),
            find: Find::default(),
            offset: Cell::new(0),
            mode: Mode::Normal,
            input: String::new(),
            pending: None,
            status_msg: None,
            connected: true,
            queue_version: None,
            art: None,
            search_origin: 0,
            last_search: String::new(),
            refresh_in_flight: false,
            refresh_dirty: false,
            epoch: 0,
            art_req_key: None,
            anim_secs: 0.0,
            spin_secs: 0.0,
            dj_input: String::new(),
            dj_log: Vec::new(),
            dj_phase: None,
            viz_active: false,
            viz_env: 0.0,
            viz_playing: false,
            menu: None,
            help_open: false,
            help_scroll: 0,
            heard_lines: Vec::new(),
            heard_open: false,
            heard_scroll: 0,
            term_bg: crate::album_color::TermBg::dark_default(),
            image_protocol: crate::album_color::ImageProtocol::None,
            sixel_cell_px: None,
            sixel_covered: Cell::new(false),
            sixel_gen: Cell::new(false),
            truecolor: false,
            sigil: None,
        }
    }
}

impl TuiState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a fresh (now-playing, queue) snapshot. Clamps `selected` down when the
    /// queue shrinks so it never dangles past the end.
    pub fn apply_snapshot(&mut self, now: NowPlaying, queue: Vec<QueueItem>) {
        self.now = now;
        self.queue = queue;
        if self.queue.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.queue.len() {
            self.selected = self.queue.len() - 1;
        }
    }

    /// Update only the now-playing card, leaving the queue (and cursor) untouched.
    /// Used by the fast refresh path when the queue version is unchanged.
    pub fn apply_now(&mut self, now: NowPlaying) {
        self.now = now;
    }

    /// Enter the confirm for a pending plan. On the DJ (chat) screen the echo +
    /// y/N prompt is pushed INLINE into the chat scrollback so it reads as part of
    /// the conversation (ui.rs skips the centered popup for Screen::Dj); on the
    /// other screens the popup carries it.
    ///
    /// A plan lands ASYNCHRONOUSLY, so it can arrive with the row menu open (the user
    /// submitted a phrase, then went on browsing). `Mode::Confirm` routes keys to
    /// `key_confirm`, which is not where the menu's modal intercept lives, so a menu
    /// left standing would be drawn over the y/N prompt with every one of its keys
    /// dead. The confirm takes the screen: close it, exactly as `open_menu` closes
    /// help in the other direction.
    pub fn enter_confirm(&mut self, pending: Pending) {
        self.take_screen();
        if self.screen == Screen::Dj {
            if let Some(trust) = &pending.trust {
                self.push_dj_log(trust.clone());
            }
            for step in &pending.steps {
                self.push_dj_log(step.clone());
            }
            if let Some(note) = &pending.note {
                self.push_dj_log(format!("! {note}"));
            }
            self.push_dj_log("confirm? [y/N]".to_string());
        }
        self.pending = Some(pending);
        self.mode = Mode::Confirm;
        self.input.clear();
    }

    /// Connection dropped: the token is owner-scoped to the dead socket, so any
    /// pending confirm is void. Fall back to Normal and show the reconnect banner.
    pub fn mark_disconnected(&mut self) {
        self.connected = false;
        self.pending = None;
        self.mode = Mode::Normal;
        // A refresh outstanding on the dead socket will never land a Snapshot (a
        // Disconnected arrives instead); clear the gate so a post-reconnect wake can
        // drive a fresh refresh.
        self.refresh_in_flight = false;
        // The command worker pushes a catch-up Snapshot on reconnect, so a suppressed
        // wake from the dead socket needs no re-arm; drop the dirty bit.
        self.refresh_dirty = false;
        // Force a full queue re-fetch on reconnect: the queue may have changed while
        // we were away, and the fresh socket's version numbering may differ.
        self.queue_version = None;
        // Browse caches were fetched on the dead socket; drop them so a reconnect
        // re-fetches on the next screen visit.
        self.albums.loaded = false;
        self.playlists.loaded = false;
        self.find.drill.loaded = false;
        // A query outstanding on the dead socket will never land, so the spinner
        // would turn forever. Fall back to Done and KEEP the hits: they are a
        // truthful snapshot of a question the user asked, and re-running is one
        // Enter away.
        if matches!(self.find.phase, crate::find::Phase::Loading(_)) {
            self.find.phase = crate::find::Phase::Done;
        }
        self.find.drill_loading = false;
        self.status_msg = Some("connection lost - reconnecting...".to_string());
    }

    /// Reconnected on a fresh socket: any old plan is gone, ask for a re-run.
    pub fn mark_connected(&mut self) {
        self.connected = true;
        self.status_msg = Some("reconnected - re-run the phrase".to_string());
    }

    /// Park the daemon's `heard` read-back and open the overlay on it.
    ///
    /// Called when the reply LANDS, so the panel is never painted around an answer that
    /// has not arrived. An empty reply cannot open it: the daemon always renders at
    /// least a coverage line or a reason, so nothing at all means an older daemon, and
    /// the worker turns that into a sentence on the bar rather than an empty box.
    /// Close every normal-mode overlay, so the caller can take the screen alone.
    ///
    /// The ONE place the "at most one overlay is live" invariant is enforced. It exists
    /// because the pairwise version of this rule kept being incomplete: `open_menu`
    /// closed help, then the heard panel arrived and closed the menu, and a confirm
    /// landing over help or over the panel still stranded them - a full-frame overlay
    /// drawn with every key dead, because `handle_key` dispatches on MODE first and
    /// `Mode::Confirm` never reaches an overlay intercept at all. Each new overlay
    /// multiplied the pairs someone had to remember.
    ///
    /// So openers call this and then set their own flag, and the invariant holds by
    /// construction for an overlay nobody has written yet.
    fn take_screen(&mut self) {
        self.menu = None;
        self.help_open = false;
        self.help_scroll = 0;
        self.heard_open = false;
        self.heard_scroll = 0;
    }

    pub fn open_heard(&mut self, lines: Vec<String>) {
        if lines.is_empty() {
            return;
        }
        self.take_screen();
        self.heard_lines = lines;
        self.heard_open = true;
        self.heard_scroll = 0;
    }

    /// Map a key to an Intent (or pure state change). The dispatch is per-mode.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Intent> {
        // Any keypress dismisses a stale banner; the action below may set a new one.
        self.status_msg = None;
        match self.mode {
            Mode::Normal => self.key_normal(key),
            Mode::Command => self.key_command(key),
            Mode::Search => self.key_search(key),
            Mode::Confirm => self.key_confirm(key),
        }
    }

    fn key_normal(&mut self, key: KeyEvent) -> Option<Intent> {
        // The row context menu is the OUTERMOST modal - above the help intercept, and
        // `open_menu` closes help, so the two are mutually exclusive by construction.
        // It must be first because it is a SNAPSHOT of one row: letting a nav key
        // through would move the cursor out from under rows that still describe the old
        // one, and letting `>`/`p` through would act on something the popup does not
        // even name.
        if self.menu.is_some() {
            return self.key_menu(key);
        }
        // The help overlay is a true modal: while open, ONLY `?`/Esc/q toggle it
        // closed and every other key is swallowed (never leaks to nav/transport).
        if self.help_open {
            // A true modal: `?`/Esc/q close it; j/k/arrows/PgUp/PgDn scroll it (so a
            // short terminal that cannot show the whole table can still reach every
            // binding); everything else is swallowed. The offset is clamped against the
            // real content/viewport in the renderer, so an over-scroll just pins to the
            // last page.
            match key.code {
                KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                    self.help_open = false;
                    self.help_scroll = 0;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.help_scroll = self.help_scroll.saturating_add(1);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    self.help_scroll = self.help_scroll.saturating_add(10);
                }
                KeyCode::PageUp => {
                    self.help_scroll = self.help_scroll.saturating_sub(10);
                }
                _ => {}
            }
            return None;
        }
        // The heard/tape overlay is the same true modal, and deliberately the same keys:
        // its own letter, Esc or q close it, j/k/PgUp/PgDn scroll. A ledger takes longer
        // to read than the next keypress, so it must NOT be dismissed by any key the way
        // a one-line banner is.
        if self.heard_open {
            match key.code {
                KeyCode::Char('t') | KeyCode::Esc | KeyCode::Char('q') => {
                    self.heard_open = false;
                    self.heard_scroll = 0;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.heard_scroll = self.heard_scroll.saturating_add(1);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.heard_scroll = self.heard_scroll.saturating_sub(1);
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    self.heard_scroll = self.heard_scroll.saturating_add(10);
                }
                KeyCode::PageUp => {
                    self.heard_scroll = self.heard_scroll.saturating_sub(10);
                }
                _ => {}
            }
            return None;
        }
        // The DJ View captures typing into its own "ask>" line (a DJ query is always
        // NL), so nav/verb keys never shadow the input. `1`/`2`/`3` still tab away is
        // NOT wanted here - Esc leaves the pane. Handled before the shared bindings.
        if self.screen == Screen::Dj {
            return self.key_dj(key);
        }
        // Find captures typing into its own `find>` line while the QUERY half has
        // focus, so nav/verb keys never shadow the input. With focus on the RESULTS
        // half it falls straight through to the shared bindings, so every global key
        // behaves exactly as it does on the other screens.
        if self.screen == Screen::Find {
            if let Some(intent) = self.key_find(key) {
                return Some(intent);
            }
            if self.find.focus == Focus::Query {
                return None;
            }
        }
        // Dispatch is DERIVED from the single-source KEYMAP: resolve the key to its
        // Act via `match_key` (which already encodes the readline-first ordering - a
        // Ctrl chord is a `Ctrl` matcher, so a plain `p`/`n`/`s` never shadows
        // `C-p`/`C-n`/`C-s`) and run it through `apply_act`. Because `apply_act` is an
        // EXHAUSTIVE match on `Act`, a new KEYMAP row (help + dispatch) cannot be added
        // without a compiler error until it is handled here, and an Act cannot be
        // removed from dispatch while a row still advertises it - so help and behavior
        // can never drift. Keys with no row (Backspace, a freed `f`) fall through to a
        // safe no-op.
        if let Some(act) = keymap::match_key(key, self.screen) {
            return self.apply_act(act);
        }
        None
    }

    /// Execute a resolved [`keymap::Act`]. The ONE place a normal-mode binding turns
    /// into an Intent or state change; [`key_normal`] routes every table key here, so
    /// this exhaustive match is the dispatch half of the single-source keymap.
    fn apply_act(&mut self, act: keymap::Act) -> Option<Intent> {
        use keymap::Act;
        match act {
            // Screen switch: main.rs lazily fetches the target view.
            Act::ScreenFind => self.switch_screen(Screen::Find),
            // Section jumps are Find-only: elsewhere they are a deliberate no-op
            // rather than a surprise, since no other screen has kinds to jump between.
            Act::SectionNext => {
                if self.screen == Screen::Find && !self.find.drilling {
                    self.find.jump_section(1);
                }
                None
            }
            Act::SectionPrev => {
                if self.screen == Screen::Find && !self.find.drilling {
                    self.find.jump_section(-1);
                }
                None
            }
            Act::ScreenQueue => self.switch_screen(Screen::Queue),
            Act::ScreenAlbums => self.switch_screen(Screen::Albums),
            Act::ScreenPlaylists => self.switch_screen(Screen::Playlists),
            Act::ScreenDj => self.switch_screen(Screen::Dj),
            Act::Down => {
                self.move_selection(1);
                None
            }
            Act::Up => {
                self.move_selection(-1);
                None
            }
            Act::Top => {
                self.go_top();
                None
            }
            Act::Bottom => {
                self.go_bottom();
                None
            }
            // Shift+P jumps the Queue cursor to the currently-playing song (browse
            // screens have no now-playing row, so it no-ops there).
            Act::JumpCurrent => {
                self.go_current();
                None
            }
            Act::SearchStart => {
                self.last_search.clear();
                self.search_origin = self.active_cursor();
                self.input.clear();
                self.mode = Mode::Search;
                None
            }
            // `n`/`N` repeat the last accepted search over the active list, stepping
            // OFF the current match (origin +/- 1); no standing search -> no-op.
            Act::SearchNext => {
                self.repeat_search(true);
                None
            }
            Act::SearchPrev => {
                self.repeat_search(false);
                None
            }
            Act::CommandLine => {
                self.mode = Mode::Command;
                self.input.clear();
                None
            }
            // Volume is a physical-potentiometer KNOB: each press is one equal-
            // loudness (dB) detent, computed server-side.
            Act::VolumeUp => Some(Intent::Command("knob up".into())),
            Act::VolumeDown => Some(Intent::Command("knob down".into())),
            Act::Pause => Some(Intent::Command("pause".into())),
            Act::Next => Some(Intent::Command("next".into())),
            Act::Prev => Some(Intent::Command("previous".into())),
            // Scrub the current track (relative seekcur).
            Act::ScrubFwd => Some(Intent::Command(format!("seekcur +{SCRUB_STEP}"))),
            Act::ScrubBack => Some(Intent::Command(format!("seekcur -{SCRUB_STEP}"))),
            // `r` starts an ENDLESS radio from the selected row (the daemon resolves a
            // bare `radio` from what is playing, so an unseedable screen still has a
            // meaning - but never a silent one; see `radio_selected`).
            Act::Radio => self.radio_selected(),
            // `s` stars the SELECTED row; C-s stars the CURRENT playing track.
            Act::FavSelected => self.favorite_selected(),
            Act::FavCurrent => self.favorite_current(),
            // `t` reads the tape back. `marks` rather than the default view because
            // THIS key exists for the presses and the audio they kept, and a marked row
            // is the only row that can carry a segment. The overlay opens when the reply
            // lands (see `open_heard`), never optimistically: a key that painted an
            // empty panel and then filled it would be claiming an answer it does not
            // have yet.
            Act::Heard => Some(Intent::Command("heard marks".into())),
            Act::PlaySel => self.enter_action(),
            // Space ADDS the selected browse row to the queue (Queue: no-op).
            Act::Enqueue => self.enqueue_selected(),
            // `l` / Right DRILLS into the selected browse directory - the body `o` ran
            // before the menu took that key, moved verbatim.
            Act::BrowseIn => self.open_selected(),
            // `o` opens the context menu for the row under the cursor, on EVERY screen
            // and every row kind. An empty list has no row to describe, so it says so
            // rather than flashing an empty popup.
            Act::Menu => {
                match self.cursor_target() {
                    Some(t) => self.open_menu(t),
                    None => self.status_msg = Some("nothing here".into()),
                }
                None
            }
            // `O` opens the same menu for what is PLAYING, from anywhere - the one row
            // the cursor may not be able to reach at all (it is not on this screen, or
            // the queue is scrolled away).
            Act::MenuCurrent => {
                match self.now_target() {
                    Some(t) => self.open_menu(t),
                    None => self.status_msg = Some("nothing is playing".into()),
                }
                None
            }
            // Back out of a browse drill-down (Queue / a browse root: no-op).
            Act::BrowseBack => self.browse_back(),
            // `?` opens the help overlay (a normal-mode modal); the modal intercept at
            // the top of key_normal then handles every key until it is toggled closed.
            Act::HelpToggle => {
                self.take_screen();
                self.help_open = true;
                None
            }
            Act::Quit => Some(Intent::Quit),
        }
    }

    /// Switch to `screen` (clearing any standing search); main.rs lazily fetches it.
    fn switch_screen(&mut self, screen: Screen) -> Option<Intent> {
        // Idempotent: re-pressing the tab key you are already on must not wipe a
        // standing `/` query or re-fetch. Without this, F2-while-on-Albums silently
        // clears the search the user is stepping through with `n`.
        if self.screen == screen {
            return None;
        }
        self.last_search.clear();
        self.screen = screen;
        Some(Intent::ShowScreen(screen))
    }

    /// The browse list for a specific target screen, if it is a browse screen. Used
    /// to fold a worker `Browse` response into the right list even if the user has
    /// since switched screens while the fetch was in flight.
    pub fn browse_for(&mut self, target: Screen) -> Option<&mut Browse> {
        match target {
            Screen::Queue | Screen::Dj => None,
            Screen::Albums => Some(&mut self.albums),
            Screen::Playlists => Some(&mut self.playlists),
            // The drill is the ONLY browse list Find owns, and only while it is the
            // visible one. Returning it off-drill would let a drill response that
            // landed after the user backed out flash under the next drill's title.
            Screen::Find => self.find.drilling.then(|| &mut self.find.drill),
        }
    }

    /// The active screen's browse list, if the active screen is a browse screen.
    pub fn active_browse(&mut self) -> Option<&mut Browse> {
        match self.screen {
            Screen::Queue | Screen::Dj => None,
            Screen::Albums => Some(&mut self.albums),
            Screen::Playlists => Some(&mut self.playlists),
            Screen::Find => self.find.drilling.then(|| &mut self.find.drill),
        }
    }

    /// Jump the selection to the top of the active list (no-op when empty).
    fn go_top(&mut self) {
        // The Find HIT list is not a `Browse`, so `active_browse()` returns None for
        // it off-drill and the queue fallback below would silently move the QUEUE
        // cursor while the visible list sat still. Claim it before that fallback.
        if self.screen == Screen::Find && !self.find.drilling {
            self.find.selected = 0;
            return;
        }
        match self.active_browse() {
            Some(b) if !b.rows.is_empty() => b.selected = 0,
            Some(_) => {}
            None => {
                if !self.queue.is_empty() {
                    self.selected = 0;
                }
            }
        }
    }

    /// Jump the selection to the last row of the active list (no-op when empty).
    fn go_bottom(&mut self) {
        // The Find HIT list is not a `Browse`, so `active_browse()` returns None for
        // it off-drill and the queue fallback below would silently move the QUEUE
        // cursor while the visible list sat still. Claim it before that fallback.
        if self.screen == Screen::Find && !self.find.drilling {
            self.find.selected = self.find.hits.rows.len().saturating_sub(1);
            return;
        }
        match self.active_browse() {
            Some(b) if !b.rows.is_empty() => b.selected = b.rows.len() - 1,
            Some(_) => {}
            None => {
                if !self.queue.is_empty() {
                    self.selected = self.queue.len() - 1;
                }
            }
        }
    }

    /// Jump the Queue cursor to the currently-playing song. Queue only (browse
    /// screens have no now-playing row); no-op when nothing is playing or the
    /// current index is out of range. `now.song` is the 0-based queue index of the
    /// current track; the queue is pos-ordered so it normally equals the row index,
    /// but we match on `pos` and fall back to the index directly to be safe.
    fn go_current(&mut self) {
        if self.screen != Screen::Queue || self.queue.is_empty() {
            return;
        }
        if let Some(song) = self.now.song {
            let idx = self
                .queue
                .iter()
                .position(|it| it.pos == song)
                .unwrap_or(song);
            if idx < self.queue.len() {
                self.selected = idx;
            }
        }
    }

    /// Enter always PLAYS the selection: Queue plays the selected row; an album/dir
    /// row enqueues the whole album and plays its first track; a song row enqueues
    /// and plays; Playlists loads the selected playlist. Drilling-in is `l` / Right.
    fn enter_action(&mut self) -> Option<Intent> {
        match self.screen {
            // Dj Enter is handled in key_dj (submit the query), never here.
            Screen::Dj => None,
            // Enter ALWAYS plays the selection, on every screen (drilling-in is `l`).
            // On an album row that enqueues the whole album and plays its first
            // track, exactly as the Albums tab does.
            Screen::Find if !self.find.drilling => {
                let uri = self.find.current_row()?.uri.clone();
                Some(Intent::Enqueue { uri, play: true })
            }
            Screen::Find => {
                let b = &self.find.drill;
                let uri = b.rows.get(b.selected).map(|r| r.uri.clone())?;
                Some(Intent::Enqueue { uri, play: true })
            }
            Screen::Queue => self
                .queue
                .get(self.selected)
                .map(|it| Intent::Command(format!("play {}", it.pos))),
            Screen::Albums => {
                let row = self.albums.rows.get(self.albums.selected)?;
                Some(Intent::Enqueue { uri: row.uri.clone(), play: true })
            }
            Screen::Playlists => {
                let row = self.playlists.rows.get(self.playlists.selected)?;
                Some(Intent::LoadPlaylist(row.uri.clone()))
            }
        }
    }

    /// Back out one browse level; only on a browse screen with a non-empty stack.
    fn browse_back(&mut self) -> Option<Intent> {
        // Backing out of a Find drill returns to the hits, which are still in memory
        // and were never overwritten - so it costs NO round trip and the query is
        // preserved by construction. Sending a BrowseBack here would re-fetch
        // `lsinfo ""`, the whole artist root, because the drill starts at depth 0
        // with an empty stack.
        if self.screen == Screen::Find && self.find.drilling && self.find.drill.stack.is_empty() {
            self.find.drilling = false;
            self.find.drill_loading = false;
            return None;
        }
        match self.active_browse() {
            Some(b) if !b.stack.is_empty() => Some(Intent::BrowseBack),
            _ => None,
        }
    }

    /// `s`: favorite (star) the SELECTED row - the thing under the cursor, mirroring
    /// Enter, so any track can be starred without playing it.
    ///
    /// EVERY screen answers, and the row it acts on is always the one the EYE is on.
    /// It used to fall through to `self.queue[self.selected]` on every non-Find screen,
    /// which is the QUEUE cursor: pressing `s` on an Albums row starred whatever library
    /// song happened to sit at that queue index - invisible, wrong, and a write into the
    /// user's library. That is the exact failure the mark work exists to prevent, so the
    /// dispatch is now per-screen like `radio_selected`, and an unstarrable row says WHY.
    fn favorite_selected(&mut self) -> Option<Intent> {
        // The Find HIT list is not a `Browse`, so it must be claimed BEFORE any
        // `active_browse()` consultation (which returns None off-drill and would fall
        // through to the QUEUE row while the visible list sat still).
        if self.screen == Screen::Find && !self.find.drilling {
            let Some(row) = self.find.current_row() else {
                self.status_msg = Some("nothing here to star".into());
                return None;
            };
            let (uri, label) = (row.uri.clone(), row.label.clone());
            return self.favorite_from_uri(&uri, &label);
        }
        match self.screen {
            // The DJ ask line owns every printable key, so `s` is text there and never
            // reaches dispatch; unreachable rather than meaningful.
            Screen::Dj => None,
            // A Playlists row is a NAME, not a uri (see `BrowseRow::uri`), so there is
            // no id to star - the same reason `r` cannot seed a radio from one.
            Screen::Playlists => {
                self.status_msg = Some("a playlist is a name, not a track - can't star it".into());
                None
            }
            // Albums, and a Find drill, are real browse lists keyed by uri.
            Screen::Albums | Screen::Find => {
                let Some(b) = self.active_browse() else { return None };
                let Some(row) = b.rows.get(b.selected) else {
                    self.status_msg = Some("nothing here to star".into());
                    return None;
                };
                let (uri, label) = (row.uri.clone(), row.label.clone());
                self.favorite_from_uri(&uri, &label)
            }
            Screen::Queue => {
                let Some(it) = self.queue.get(self.selected) else {
                    self.status_msg = Some("nothing here to star".into());
                    return None;
                };
                let (uri, label, is_current) = (
                    it.uri.clone(),
                    it.title.clone(),
                    self.now.song == Some(it.pos),
                );
                match uri {
                    Some(uri) if uri.starts_with("song/") => {
                        Some(Intent::Command(format!("playlistadd Starred {uri}")))
                    }
                    // A stream row: "can't favorite" became FALSE the moment `mark`
                    // shipped, and a false reason is worse than a dead key. The subject
                    // of a mark is what is ON AIR, though, not what the cursor is on, so
                    // only the PLAYING stream row marks; another one says what to do.
                    Some(_) if is_current => Some(Intent::Command("mark".into())),
                    Some(_) => {
                        self.status_msg =
                            Some("that row is a stream - play it, then s marks what is on".into());
                        None
                    }
                    None => {
                        self.status_msg = Some(format!("{label} has no track to star"));
                        None
                    }
                }
            }
        }
    }

    /// The ONE uri gate behind `s`: `playlistadd Starred` accepts a library `song/<id>`
    /// or `album/<id>` (`Favorite::from_uri` handles exactly those two), so the client
    /// whitelists the same two shapes and turns every other row into a REASON rather
    /// than an ACK banner. Mirrors [`radio_from_uri`].
    fn favorite_from_uri(&mut self, uri: &str, label: &str) -> Option<Intent> {
        if uri.starts_with("song/") || uri.starts_with("album/") {
            return Some(Intent::Command(format!("playlistadd Starred {uri}")));
        }
        self.status_msg = Some(if uri.starts_with("artist/") {
            "can't star an artist - open it and star an album".into()
        } else if uri.starts_with("list/") {
            "can't star a smart list".into()
        } else if uri.starts_with("station/") {
            // Subsonic has no star endpoint for internet radio at all, but the thing
            // PLAYING on it is markable - so the refusal points at the verb that works
            // instead of stopping at "can't".
            "a saved station is a stream - play it, then C-s marks what is on".into()
        } else if uri.contains("://") {
            "that row is a stream - play it, then C-s marks what is on".into()
        } else {
            format!("can't star {label}")
        });
        None
    }

    /// `r`: start an ENDLESS radio from the SELECTED row - the thing under the cursor
    /// plays, and the daemon's continuation walk owns the end of the queue from then
    /// on. Mirrors `favorite_selected` in acting on the cursor rather than on the
    /// current track, so a radio can be started from anything the eye is on.
    ///
    /// Every screen answers: a seedable row emits `radio <uri>`, an unseedable one
    /// (a playlist name, a smart-list dir, a stream) says WHY in the status line, and
    /// an empty list says there is nothing here - the key is never silently dead.
    fn radio_selected(&mut self) -> Option<Intent> {
        // The Find HIT list is not a `Browse`, so it must be claimed BEFORE any
        // `active_browse()` consultation (which returns None off-drill and would fall
        // through to the QUEUE row while the visible list sat still). All three hit
        // kinds are valid seeds - this is also the artist row's first working action,
        // since `enqueue_uri` rejects `artist/<id>` outright.
        if self.screen == Screen::Find && !self.find.drilling {
            let Some(row) = self.find.current_row() else {
                self.status_msg = Some("nothing here to start a radio from".into());
                return None;
            };
            let (uri, label) = (row.uri.clone(), row.label.clone());
            return self.radio_from_uri(&uri, &label);
        }
        match self.screen {
            // The DJ ask line owns every printable key, so `r` is text there and never
            // reaches dispatch; unreachable rather than meaningful.
            Screen::Dj => None,
            // A Playlists row is a NAME, not a uri (see `BrowseRow::uri`), so there is
            // no id to seed a radio from.
            Screen::Playlists => {
                self.status_msg = Some("can't start a radio from a playlist".into());
                None
            }
            // Albums, and a Find drill, are real browse lists keyed by uri.
            Screen::Albums | Screen::Find => {
                let Some(b) = self.active_browse() else { return None };
                let Some(row) = b.rows.get(b.selected) else {
                    self.status_msg = Some("nothing here to start a radio from".into());
                    return None;
                };
                let (uri, label) = (row.uri.clone(), row.label.clone());
                self.radio_from_uri(&uri, &label)
            }
            Screen::Queue => {
                let Some(it) = self.queue.get(self.selected) else {
                    self.status_msg = Some("nothing here to start a radio from".into());
                    return None;
                };
                let (uri, label) = (it.uri.clone(), it.title.clone());
                match uri {
                    Some(uri) => self.radio_from_uri(&uri, &label),
                    // A queue row with no uri is a raw stream (an internet radio
                    // station), which has no library id to seed a walk from. Every
                    // sibling branch sets a reason first; returning bare here made `r`
                    // a SILENTLY dead key - it did nothing and said nothing, which
                    // reads as a broken binding rather than an inapplicable one.
                    None => {
                        self.status_msg =
                            Some("that row is a stream, not a library track".into());
                        None
                    }
                }
            }
        }
    }

    /// The ONE uri gate behind `r`: the daemon's `radio` verb seeds from `song/`,
    /// `album/` and `artist/` and parse-ACKs everything else, so the client whitelists
    /// the same three shapes and turns every other row into a reason rather than an
    /// ACK banner. The status line is the instant feedback; the standing confirmation
    /// is the `then: more like <seed>` queue-tail hint the armed walk emits.
    fn radio_from_uri(&mut self, uri: &str, label: &str) -> Option<Intent> {
        if uri.starts_with("song/") || uri.starts_with("album/") || uri.starts_with("artist/") {
            self.status_msg = Some(format!("radio from {label}"));
            return Some(Intent::Command(format!("radio {uri}")));
        }
        self.status_msg = Some(if uri.starts_with("list/") {
            "can't start a radio from a list".into()
        } else if uri.starts_with("station/") {
            // A saved station is a STREAM, not a library object, so there is no id for
            // the continuation walk to seed from - and `radio` here would be confused
            // with the algorithmic `radio/random` generator, which is a different thing
            // entirely. Enter is the verb that works on this row.
            "a saved station is a stream, not a library seed - enter plays it".into()
        } else if uri.contains("://") {
            "that row is a stream, can't start a radio".into()
        } else {
            "can't start a radio from that row".into()
        });
        None
    }

    /// Space: enqueue the selected browse row (no play) and advance the cursor one
    /// row for rapid multi-add. On Playlists a row name is not a file URI, so it
    /// loads via `LoadPlaylist` (mirrors Enter's semantics without playing); an
    /// Albums/dir/song row enqueues with `add <uri>`. Queue has nothing to add ->
    /// no-op.
    fn enqueue_selected(&mut self) -> Option<Intent> {
        // The Find HIT list is not a `Browse`, so it must be claimed BEFORE the
        // active_browse() consultation below (which returns None off-drill).
        if self.screen == Screen::Find && !self.find.drilling {
            let uri = self.find.current_row()?.uri.clone();
            self.find.move_selection(1);
            return Some(Intent::Enqueue { uri, play: false });
        }
        let intent = match self.active_browse() {
            Some(b) => {
                let uri = b.rows.get(b.selected).map(|r| r.uri.clone())?;
                match self.screen {
                    Screen::Playlists => Intent::LoadPlaylist(uri),
                    _ => Intent::Enqueue { uri, play: false },
                }
            }
            None => return None,
        };
        self.move_selection(1);
        Some(intent)
    }

    /// `l` / Right: DRILL into the selected browse directory. A song row or the Queue
    /// screen is a no-op (Enter is the play verb there).
    fn open_selected(&mut self) -> Option<Intent> {
        // A hit row is not a `Browse` row, so claim it before active_browse(). Album
        // and artist hits drill; a song has nothing to open.
        if self.screen == Screen::Find && !self.find.drilling {
            let row = self.find.current_row()?;
            return match row.kind {
                crate::find::FindKind::Album | crate::find::FindKind::Artist => {
                    Some(Intent::BrowseInto(row.uri.clone()))
                }
                // A saved station is a LEAF: `lsinfo station/<name>` falls into the
                // daemon's catch-all and returns a well-formed EMPTY listing, while
                // `browse_into` sets `drilling` unconditionally - so drilling would
                // paint an empty bordered box over the hits. Say why instead of going
                // quiet, since the cursor can land here and `o` works on the rows above.
                crate::find::FindKind::Station => {
                    self.status_msg =
                        Some("a station has nothing to open - enter plays it".into());
                    None
                }
                crate::find::FindKind::Song => None,
            };
        }
        let b = self.active_browse()?;
        let row = b.rows.get(b.selected)?;
        if row.is_dir {
            Some(Intent::BrowseInto(row.uri.clone()))
        } else {
            None
        }
    }

    /// The thing under the cursor, as a [`Target`]. THE resolver: the per-screen "what
    /// is selected" match, written ONCE here instead of a seventh time per action.
    ///
    /// The Find HIT list is not a `Browse`, so it is claimed BEFORE `active_browse()`,
    /// which returns None off-drill and would otherwise fall through to the QUEUE row
    /// while the visible list sat still - the bug every hand-rolled copy of this match
    /// has to remember. An empty list yields `None`, so the caller can say "nothing
    /// here" rather than opening a popup over nothing.
    fn cursor_target(&self) -> Option<Target> {
        if self.screen == Screen::Find && !self.find.drilling {
            let row = self.find.current_row()?;
            return Some(Target {
                // The hit's KIND is already decided by the uri prefix daemon-side
                // (`Block::start`), so classifying the uri here reproduces it exactly
                // rather than translating one enum into another.
                kind: classify(Some(&row.uri), false),
                origin: Origin::FindHit,
                label: row.label.clone(),
                uri: Some(row.uri.clone()),
                album_uri: row.album_uri.clone(),
                artist: named(row.artist.clone()),
                artist_uri: None,
                match_uri: None,
            });
        }
        match self.screen {
            // The DJ ask line owns every printable key, so `o` is text there and never
            // reaches dispatch; unreachable rather than meaningful.
            Screen::Dj => None,
            Screen::Queue => {
                let it = self.queue.get(self.selected)?;
                Some(Target {
                    kind: classify(it.uri.as_deref(), false),
                    // The MPD `Pos`, not the row index: `play`/`delete` address by
                    // position and the two only coincide while the queue is dense.
                    origin: Origin::Queue { pos: it.pos },
                    label: it.title.clone(),
                    uri: it.uri.clone(),
                    album_uri: it.album_uri.clone(),
                    artist: named(it.artist.clone()),
                    artist_uri: None,
                    match_uri: None,
                })
            }
            Screen::Albums | Screen::Playlists | Screen::Find => {
                // A Playlists row is a NAME, not a browse path (see `BrowseRow::uri`),
                // which no uri prefix can reveal - the SCREEN is the only thing that
                // knows, so it is the only place that can say so.
                let playlists = self.screen == Screen::Playlists;
                let b = match self.screen {
                    Screen::Albums => &self.albums,
                    Screen::Playlists => &self.playlists,
                    _ => &self.find.drill,
                };
                let row = b.rows.get(b.selected)?;
                Some(Target {
                    kind: if playlists {
                        TargetKind::Playlist
                    } else {
                        classify(Some(&row.uri), row.is_dir)
                    },
                    origin: Origin::Browse,
                    label: row.label.clone(),
                    uri: Some(row.uri.clone()),
                    album_uri: row.album_uri.clone(),
                    artist: named(row.artist.clone()),
                    artist_uri: None,
                    match_uri: None,
                })
            }
        }
    }

    /// The playing track as a [`Target`] (`O`). A `song/` file is a LibrarySong whose
    /// `album_uri` comes from `currentsong`'s `X-AlbumUri`; anything else is a Stream
    /// carrying `match_uri`, so a RECOGNIZED radio track keeps every library action
    /// exactly as `C-s` already does for starring - the stream url stays `uri` because
    /// the playing entry is never rewritten.
    fn now_target(&self) -> Option<Target> {
        let file = self.now.file.clone()?;
        // What the eye sees on the now-playing card: the track title, else the station
        // name, else the bare url - never a blank popup heading.
        let label = self
            .now
            .title
            .clone()
            .or_else(|| self.now.name.clone())
            .unwrap_or_else(|| file.clone());
        Some(Target {
            kind: classify(Some(&file), false),
            origin: Origin::NowPlaying,
            label,
            uri: Some(file),
            album_uri: self.now.album_uri.clone(),
            artist: named(self.now.artist.clone()),
            artist_uri: None,
            match_uri: self.now.match_uri.clone(),
        })
    }

    /// Open the menu on a resolved target. Closes the help and heard overlays first so
    /// the modals are mutually exclusive by construction rather than by an ordering the
    /// intercepts have to agree on.
    fn open_menu(&mut self, target: Target) {
        self.take_screen();
        self.menu = Some(Menu::new(target, self.queue.len()));
    }

    /// The menu's own key handling while it is open. A true modal: motion moves the
    /// popup, Enter (or `l`/Right, matching the drill key) runs the highlighted row,
    /// any [`crate::menu::MenuItem::hotkey`] runs its row directly, `q`/Esc close, and
    /// everything else is SWALLOWED - never leaked to nav or transport.
    fn key_menu(&mut self, key: KeyEvent) -> Option<Intent> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // The readline chords first: crossterm delivers C-n as `Char('n') + CONTROL`,
        // so the plain-char arms below would otherwise claim them.
        if ctrl {
            if let KeyCode::Char(c) = key.code {
                if let Some(m) = self.menu.as_mut() {
                    match c {
                        'n' => m.move_selection(1),
                        'p' => m.move_selection(-1),
                        _ => {}
                    }
                }
            }
            return None;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.menu = None;
                None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(m) = self.menu.as_mut() {
                    m.move_selection(1);
                }
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(m) = self.menu.as_mut() {
                    m.move_selection(-1);
                }
                None
            }
            KeyCode::Char('g') => {
                if let Some(m) = self.menu.as_mut() {
                    m.selected = 0;
                }
                None
            }
            KeyCode::Char('G') => {
                if let Some(m) = self.menu.as_mut() {
                    m.selected = m.rows.len().saturating_sub(1);
                }
                None
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                let i = self.menu.as_ref()?.selected;
                self.run_menu_row(i)
            }
            // A direct pick. An unclaimed letter is swallowed, not leaked: the popup
            // names its whole vocabulary, so a key it does not list means nothing here.
            KeyCode::Char(c) => {
                let i = self.menu.as_ref().and_then(|m| m.pick(c))?;
                self.run_menu_row(i)
            }
            _ => None,
        }
    }

    /// Run the row at `i`. A BLOCKED row states its reason and leaves the menu OPEN
    /// (a refusal is information, and the next row is one keystroke away); a live row
    /// dispatches and closes. Dispatch runs BEFORE the close so it can still read the
    /// snapshot target (its label, and the uri `guard_pos` re-checks).
    fn run_menu_row(&mut self, i: usize) -> Option<Intent> {
        let picked = self.menu.as_mut().and_then(|m| {
            let avail = m.rows.get(i)?.avail.clone();
            m.selected = i;
            Some(avail)
        })?;
        match picked {
            Avail::No(why) => {
                self.status_msg = Some(why.to_string());
                None
            }
            Avail::Yes(action) => {
                let intent = self.run_menu_action(action);
                self.menu = None;
                intent
            }
        }
    }

    /// Run one resolved [`MenuAction`]. EXHAUSTIVE, so a new action cannot be built in
    /// `rows_for` without a compiler error here - the same lockstep `apply_act` gives
    /// `Act`. Every arm is the EXISTING verb: no new `Intent` variant ships with the
    /// menu, and the radio row routes through `radio_from_uri` so the popup and the
    /// bare `r` key share one gate and one set of reason strings.
    fn run_menu_action(&mut self, a: MenuAction) -> Option<Intent> {
        match a {
            MenuAction::OpenContents(u) => Some(Intent::BrowseInto(u)),
            MenuAction::Play(PlayHow::QueuePos(p)) => {
                self.guard_pos(p).map(|p| Intent::Command(format!("play {p}")))
            }
            MenuAction::Play(PlayHow::Enqueue(u)) => Some(Intent::Enqueue { uri: u, play: true }),
            MenuAction::Play(PlayHow::LoadPlaylist(n)) => Some(Intent::LoadPlaylist(n)),
            MenuAction::Enqueue(u) => Some(Intent::Enqueue { uri: u, play: false }),
            MenuAction::GoToAlbum(u) => {
                let label = self.menu_label();
                self.reveal(u, &label)
            }
            MenuAction::GoToArtist(ArtistRef::Uri(u)) => {
                let label = self.menu_label();
                self.reveal(u, &label)
            }
            // No artist uri exists on the wire yet, so this is a real library QUERY -
            // it lands on the Find hits, whose first section is Artists. The query line
            // is filled with the name so the screen says what it asked.
            MenuAction::GoToArtist(ArtistRef::Name(n)) => {
                self.screen = Screen::Find;
                self.find.drilling = false;
                self.find.drill_loading = false;
                self.find.focus = Focus::Results;
                self.find.query = n.clone();
                self.drop_stale_hits();
                self.submit_find(n)
            }
            MenuAction::Radio(u) => {
                let label = self.menu_label();
                self.radio_from_uri(&u, &label)
            }
            MenuAction::Favorite(u) => Some(Intent::Command(format!("playlistadd Starred {u}"))),
            MenuAction::RemoveFromQueue(p) => {
                self.guard_pos(p).map(|p| Intent::Command(format!("delete {p}")))
            }
        }
    }

    /// The open menu's target label, for status wording.
    fn menu_label(&self) -> String {
        self.menu.as_ref().map(|m| m.target.label.clone()).unwrap_or_default()
    }

    /// Re-check a snapshot queue position against the row the menu was built from.
    /// The event loop already closes the menu when a refresh changes `queue.len()`;
    /// this catches the SAME-LENGTH reorder that length alone misses, so `play`/`delete`
    /// can never act on a row that slid under the popup.
    fn guard_pos(&mut self, pos: usize) -> Option<usize> {
        let want = self.menu.as_ref().and_then(|m| m.target.uri.clone());
        match self.queue.iter().find(|it| it.pos == pos) {
            Some(it) if it.uri == want => Some(pos),
            _ => {
                self.status_msg = Some("the queue moved - reopen the menu".into());
                None
            }
        }
    }

    /// Reveal a browse uri in the FIND drill: the one pane that shows an arbitrary path
    /// without disturbing another screen's cursor or nav stack (it starts at depth 0
    /// with a deliberately empty stack, and `h`/Esc returns to the hits at no round
    /// trip). Revealing into the Albums tab instead would clobber THAT tab's cursor and
    /// stack. No new Intent: the screen flip is local state and `browse_into` already
    /// handles the Find-off-drill case.
    ///
    /// The hits underneath are kept ONLY when the menu was opened on one of them - the
    /// case where "back out to the hits is free" means anything, and where a key that
    /// beats the drill in still acts on the row the popup just named. Revealed from a
    /// queue row, an album or the playing track, they answer a question from some
    /// earlier session of use and are dropped (see [`drop_stale_hits`]).
    ///
    /// [`drop_stale_hits`]: Self::drop_stale_hits
    fn reveal(&mut self, uri: String, label: &str) -> Option<Intent> {
        let from_hits = self.menu.as_ref().is_some_and(|m| m.target.origin == Origin::FindHit);
        self.screen = Screen::Find;
        self.find.drilling = false;
        self.find.drill_loading = false;
        self.find.focus = Focus::Results;
        if !from_hits {
            self.drop_stale_hits();
        }
        self.last_search.clear();
        self.status_msg = Some(format!("showing {label}"));
        Some(Intent::BrowseInto(uri))
    }

    /// Drop the visible hits, because the screen is about to answer a DIFFERENT
    /// question than the one they answered.
    ///
    /// The query line can leave a stale answer standing while the next one flies - the
    /// renderer keeps it deliberately legible - because focus stays on the TEXT there
    /// and every key is swallowed as input. A menu jump cannot: it lands the cursor on
    /// the RESULTS half at once, and `find.hits`/`find.selected` are only replaced when
    /// the response lands. For that whole round trip Enter would enqueue and PLAY, `s`
    /// would star and `r` would seed a radio from a row belonging to a search nobody is
    /// looking at - invisible, wrong, and in the `s` case a write into the user's
    /// library. The rows go with the question.
    fn drop_stale_hits(&mut self) {
        self.find.hits = crate::find::FindHits::default();
        self.find.selected = 0;
        self.find.offset.set(0);
    }

    /// Submit a library query. Factored out of `key_find`'s Enter arm so the menu's
    /// "go to artist (search)" and the query line are ONE submit path and cannot drift
    /// on history, the in-flight phase, or the echoed query. An empty query sends
    /// nothing and records nothing.
    fn submit_find(&mut self, q: String) -> Option<Intent> {
        if q.is_empty() {
            return None;
        }
        self.find.push_history(&q);
        self.find.phase = crate::find::Phase::Loading(q.clone());
        self.find.submitted = q.clone();
        Some(Intent::Find(q))
    }

    /// The labels of the active list, as the eye sees them, for search matching.
    fn active_labels(&self) -> Vec<String> {
        match self.screen {
            Screen::Queue => self
                .queue
                .iter()
                .map(|it| match &it.artist {
                    Some(a) => format!("{} - {}", it.title, a),
                    None => it.title.clone(),
                })
                .collect(),
            Screen::Albums => self.albums.rows.iter().map(|r| r.label.clone()).collect(),
            Screen::Playlists => self.playlists.rows.iter().map(|r| r.label.clone()).collect(),
            // The DJ pane has no navigable list to search.
            Screen::Dj => Vec::new(),
            // `/` inside a drill steps the DRILL rows; over the hit list it steps
            // the hits. Both are real lists the eye can see, so both are searchable.
            Screen::Find if self.find.drilling => {
                self.find.drill.rows.iter().map(|r| r.label.clone()).collect()
            }
            Screen::Find => self.find.hits.rows.iter().map(|r| r.label.clone()).collect(),
        }
    }

    /// The active list's current cursor index (Queue or the active browse).
    fn active_cursor(&self) -> usize {
        match self.screen {
            Screen::Queue | Screen::Dj => self.selected,
            Screen::Albums => self.albums.selected,
            Screen::Playlists => self.playlists.selected,
            // Never `self.selected` here: that is the QUEUE cursor, and returning it
            // would silently move the queue while the visible list sat still.
            Screen::Find if self.find.drilling => self.find.drill.selected,
            Screen::Find => self.find.selected,
        }
    }

    /// Set the active list's cursor index (Queue or the active browse).
    fn set_active_cursor(&mut self, i: usize) {
        match self.screen {
            Screen::Queue | Screen::Dj => self.selected = i,
            Screen::Albums => self.albums.selected = i,
            Screen::Playlists => self.playlists.selected = i,
            Screen::Find if self.find.drilling => self.find.drill.selected = i,
            Screen::Find => self.find.selected = i,
        }
    }

    /// Re-run the incremental search from `search_origin` against the current input,
    /// moving the active cursor to the match (or sticking at the origin on no match).
    fn run_search(&mut self) {
        let labels = self.active_labels();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let i = search_jump(&refs, &self.input, self.search_origin).unwrap_or(self.search_origin);
        self.set_active_cursor(i);
    }

    /// Repeat the last accepted search over the active list, stepping OFF the
    /// current match: forward (`n`) from cursor+1, backward (`N`) from cursor-1,
    /// wrapping once. A no-match or empty standing search leaves the cursor put.
    fn repeat_search(&mut self, forward: bool) {
        if self.last_search.is_empty() {
            return;
        }
        let labels = self.active_labels();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let cur = self.active_cursor();
        let origin = if forward { cur + 1 } else { cur.saturating_sub(1) };
        if let Some(i) = search_step(&refs, &self.last_search, origin, forward) {
            self.set_active_cursor(i);
        }
    }

    /// The query currently driving the substring highlight: the live input while in
    /// Search mode, else the standing `last_search` in Normal mode, else empty.
    /// Used by the renderer to underline every matching row.
    pub fn highlight_query(&self) -> &str {
        match self.mode {
            Mode::Search => &self.input,
            // On Find, the query that produced the visible hits is what the eye is
            // looking for, so it underlines even with no standing `/` search. A live
            // `/` still wins: it is the more recent intent, and `/` inside a drill
            // must highlight what it is stepping through.
            Mode::Normal if self.screen == Screen::Find && self.last_search.is_empty() => {
                &self.find.submitted
            }
            Mode::Normal => &self.last_search,
            _ => "",
        }
    }

    /// Map of `album/<id>` -> the set of DISTINCT queued `song/<id>` uris for that
    /// album, folded from the current queue. A set (not a count) so a duplicated
    /// queued track cannot inflate an album past Full. Drives the browse markers.
    pub fn queued_by_album(&self) -> HashMap<String, HashSet<String>> {
        let mut map: HashMap<String, HashSet<String>> = HashMap::new();
        for it in &self.queue {
            if let (Some(al), Some(uri)) = (&it.album_uri, &it.uri) {
                map.entry(al.clone()).or_default().insert(uri.clone());
            }
        }
        map
    }

    /// The set of DISTINCT `song/<id>` uris currently in the queue, so an opened
    /// album's song rows can be marked when they are already queued.
    pub fn queued_uris(&self) -> HashSet<String> {
        self.queue.iter().filter_map(|it| it.uri.clone()).collect()
    }

    /// Incremental jump-to-match search: Char/Backspace re-run the jump, Enter
    /// accepts in place, Esc restores the pre-search cursor. Non-destructive.
    fn key_search(&mut self, key: KeyEvent) -> Option<Intent> {
        match key.code {
            KeyCode::Esc => {
                self.set_active_cursor(self.search_origin);
                self.mode = Mode::Normal;
                self.input.clear();
                None
            }
            KeyCode::Enter => {
                // Keep the accepted query as the standing search for n/N + the
                // post-accept highlight; an empty query (bare Enter) leaves any
                // prior standing search untouched (Esc-like no-op).
                if !self.input.is_empty() {
                    self.last_search = self.input.clone();
                }
                self.mode = Mode::Normal;
                self.input.clear();
                None
            }
            KeyCode::Backspace => {
                self.input.pop();
                self.run_search();
                None
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                self.run_search();
                None
            }
            _ => None,
        }
    }

    /// Move the ACTIVE screen's selection with clamping (no wrap). Queue moves
    /// `self.selected`; browse screens move their own cursor.
    fn move_selection(&mut self, delta: i32) {
        // The Find HIT list is not a `Browse`, so `active_browse()` returns None for
        // it off-drill and the queue fallback below would silently move the QUEUE
        // cursor while the visible list sat still. Claim it before that fallback.
        if self.screen == Screen::Find && !self.find.drilling {
            self.find.move_selection(delta);
            return;
        }
        if let Some(b) = self.active_browse() {
            b.move_selection(delta);
            return;
        }
        if self.queue.is_empty() {
            self.selected = 0;
            return;
        }
        let last = self.queue.len() - 1;
        let next = self.selected as i32 + delta;
        self.selected = next.clamp(0, last as i32) as usize;
    }

    fn key_command(&mut self, key: KeyEvent) -> Option<Intent> {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.input.clear();
                None
            }
            KeyCode::Backspace => {
                self.input.pop();
                None
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Char(c) => {
                self.input.push(c);
                None
            }
            _ => None,
        }
    }

    /// Route the typed line through the SAME client route() the CLI uses, so a bare
    /// verb goes to Command and a phrase goes to NL - one routing source.
    fn submit(&mut self) -> Option<Intent> {
        let words: Vec<String> = self.input.split_whitespace().map(str::to_string).collect();
        let action = route(&words);
        self.mode = Mode::Normal;
        self.input.clear();
        match action {
            Action::NowPlaying | Action::Queue => Some(Intent::Refresh),
            Action::Command(line) => Some(Intent::Command(line)),
            Action::Help => {
                self.status_msg = Some(not_understood_hint());
                None
            }
            Action::ClearConfirm => {
                self.enter_confirm(Pending {
                    command: Some("clear".to_string()),
                    token: None,
                    steps: vec!["clear the whole queue".to_string()],
                    note: None,
                    trust: None,
                });
                None
            }
            Action::FavoriteCurrent => self.favorite_current(),
            Action::Nl(phrase) => Some(Intent::Nl(phrase)),
        }
    }

    /// Favorite (star) the current track - Ctrl-s, and the typed `fav`/`favorite`
    /// phrase. TOTAL: every playing thing answers, and none of the answers is silence.
    ///
    /// A library `song/<id>` stars itself. A raw stream goes to the daemon's `mark`
    /// verb, which is the only place that holds what the decision needs - the previous
    /// ICY title, the subject ages, the provenance-stamped library match and the heard
    /// ledger - so it stars the local copy when the subject is owned and unambiguous,
    /// records a pointer row when it is not, refuses to pick between two plausible
    /// subjects, and ALWAYS answers with a sentence the worker turns into a banner.
    ///
    /// Deliberately NOT the client-side `match_uri` shortcut this replaced: that starred
    /// silently (nothing on screen changed at all), recorded nothing, and read a match
    /// the client cannot age-check. Nothing playing is still answered here rather than
    /// on a round trip, because the client already knows.
    fn favorite_current(&mut self) -> Option<Intent> {
        match self.now.file.as_deref() {
            Some(uri) if uri.starts_with("song/") => {
                Some(Intent::Command(format!("playlistadd Starred {uri}")))
            }
            Some(_) => Some(Intent::Command("mark".into())),
            None => {
                self.status_msg = Some("nothing is playing to favorite".into());
                None
            }
        }
    }

    fn key_confirm(&mut self, key: KeyEvent) -> Option<Intent> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(Intent::ConfirmArm),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(Intent::ConfirmCancel),
            _ => None,
        }
    }

    /// DJ View input: printable chars build the "ask>" query, Enter submits it,
    /// Esc leaves back to the Queue screen. A blank Enter is a no-op. Enter routes
    /// the phrase through the SAME client route() the ':' command line uses, so a
    /// bare-favorite phrase ("favorite this song") stars the current track here too
    /// instead of falling to the CC translator that has no favorite capability;
    /// anything else stays a CC translation (a DJ query is otherwise never a bare
    /// verb).
    /// The Find screen's key handling. Returns `Some(intent)` when it acted, and
    /// `None` when the caller should fall through to the shared keymap - which only
    /// happens with focus on the results half.
    ///
    /// This deliberately does NOT introduce a fifth `Mode`. `state.mode` stays
    /// `Mode::Normal` throughout, exactly as the Dj screen's `ask>` line does, so
    /// `render_command`'s per-mode caret match and `handle_key`'s mode dispatch need
    /// no new arm.
    fn key_find(&mut self, key: KeyEvent) -> Option<Intent> {
        // The screen-switch keys and help must work here too, resolved through the
        // SINGLE-SOURCE keymap so this screen can never drift from KEYMAP. F-keys are
        // never part of a query, so they switch outright. `?` opens help ONLY on an
        // empty query line, so a literal `?` can still be typed mid-query.
        if let Some(act) = keymap::match_key(key, self.screen) {
            use keymap::Act;
            match act {
                Act::ScreenQueue
                | Act::ScreenAlbums
                | Act::ScreenPlaylists
                | Act::ScreenDj
                | Act::ScreenFind => return self.apply_act(act),
                Act::HelpToggle if self.find.query.is_empty() => return self.apply_act(act),
                _ => {}
            }
        }
        if self.find.focus == Focus::Results {
            // The results half is an ordinary list: everything falls through to the
            // shared bindings. Only Tab (back to the query) is claimed here.
            return match key.code {
                KeyCode::Tab => {
                    self.find.focus = Focus::Query;
                    None
                }
                // Esc in the results half steps UP the ladder to the query line
                // (out of a drill first). Falling through to the shared BrowseBack
                // would be a silent no-op here, leaving the key feeling dead.
                KeyCode::Esc if !self.find.drilling => {
                    self.find.focus = Focus::Query;
                    None
                }
                _ => None,
            };
        }
        // A Ctrl chord is NEVER query text. crossterm delivers Ctrl-s as
        // `Char('s') + CONTROL`, and the `KeyCode::Char(c)` arm below inspects only the
        // code - so without this the mark gesture silently TYPED an `s` into the query
        // the user was in the middle of writing. A key that corrupts input is worse than
        // a dead one, and the gesture cannot be called total while a screen eats it.
        // Resolved through the SINGLE-SOURCE keymap, so this can never drift from KEYMAP.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return keymap::match_key(key, self.screen).and_then(|act| self.apply_act(act));
        }
        match key.code {
            // The Esc ladder: out of a drill, then off the query line to the results,
            // then off the screen. Each press makes progress rather than ping-ponging.
            KeyCode::Esc => {
                // The ladder: out of a drill, then off the screen. Each press makes
                // progress rather than ping-ponging between two states.
                if self.find.drilling {
                    self.find.drilling = false;
                    self.find.drill_loading = false;
                    return None;
                }
                self.screen = Screen::Queue;
                Some(Intent::ShowScreen(Screen::Queue))
            }
            KeyCode::Tab => {
                if !self.find.hits.rows.is_empty() {
                    self.find.focus = Focus::Results;
                }
                None
            }
            KeyCode::Backspace => {
                self.find.query.pop();
                None
            }
            // Up/Down walk the query history rather than moving a cursor, because a
            // submit-driven screen has no type-then-arrow-into-results gesture: Enter
            // already lands the cursor on row 0.
            KeyCode::Up => {
                self.find.walk_history(1);
                None
            }
            KeyCode::Down => {
                self.find.walk_history(-1);
                None
            }
            KeyCode::Enter => {
                // The ONE submit path, shared with the menu's "go to artist (search)"
                // so the two cannot drift on history, the in-flight phase or the
                // echoed query.
                let q = self.find.query.trim().to_string();
                self.submit_find(q)
            }
            KeyCode::Char(c) => {
                self.find.query.push(c);
                self.find.history_pos = None;
                None
            }
            _ => None,
        }
    }

    fn key_dj(&mut self, key: KeyEvent) -> Option<Intent> {
        // The Scope::Global view + help bindings must work here too, and they are
        // resolved through the SINGLE-SOURCE keymap (match_key) - NOT hand-written -
        // so the DJ screen can never drift from KEYMAP. Only the four screen-switch
        // Acts and HelpToggle are honored here; every other Global matcher (j/k/p/vol
        // etc.) falls through to be captured as ask-line input, since a DJ query is
        // always typed text. F-keys are never part of an NL query, so switch outright;
        // `?` opens help ONLY on an empty ask line, so a literal `?` can still be typed
        // mid-phrase ("what should I play?").
        if let Some(act) = keymap::match_key(key, self.screen) {
            use keymap::Act;
            match act {
                Act::ScreenQueue
                | Act::ScreenAlbums
                | Act::ScreenPlaylists
                | Act::ScreenDj
                | Act::ScreenFind => {
                    return self.apply_act(act);
                }
                Act::HelpToggle if self.dj_input.is_empty() => {
                    return self.apply_act(act);
                }
                _ => {}
            }
        }
        // A Ctrl chord is NEVER ask-line text - same reason as the Find query line:
        // crossterm delivers Ctrl-s as `Char('s') + CONTROL` and the `KeyCode::Char(c)`
        // arm below inspects only the code, so the mark gesture silently typed an `s`
        // into the phrase being written. Resolved through the SINGLE-SOURCE keymap.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return keymap::match_key(key, self.screen).and_then(|act| self.apply_act(act));
        }
        match key.code {
            KeyCode::Esc => {
                self.screen = Screen::Queue;
                self.dj_input.clear();
                Some(Intent::ShowScreen(Screen::Queue))
            }
            KeyCode::Backspace => {
                self.dj_input.pop();
                None
            }
            KeyCode::Enter => {
                let phrase = self.dj_input.trim().to_string();
                self.dj_input.clear();
                if phrase.is_empty() {
                    return None;
                }
                self.push_dj_log(format!("> {phrase}"));
                let words: Vec<String> =
                    phrase.split_whitespace().map(str::to_string).collect();
                // Route bare control verbs (favorite/star, play/pause/stop/next/prev,
                // clear) to the DETERMINISTIC client verb path BEFORE Claude - the
                // same spirit as the favorite fix. A queue-manipulation ask that is
                // NOT a bare verb (a fuzzy phrase) still goes to CC. This is the
                // hybrid split: bare verbs never reach the translator (which cannot
                // express favorite/clear and would degrade them to a no-op enqueue).
                match route(&words) {
                    Action::FavoriteCurrent => self.favorite_current(),
                    Action::Command(line) => {
                        self.push_dj_log(format!("ok: {line}"));
                        Some(Intent::Command(line))
                    }
                    Action::ClearConfirm => {
                        self.enter_confirm(Pending {
                            command: Some("clear".to_string()),
                            token: None,
                            steps: vec!["clear the whole queue".to_string()],
                            note: None,
                            trust: None,
                        });
                        None
                    }
                    Action::NowPlaying | Action::Queue => Some(Intent::Refresh),
                    Action::Help => {
                        self.push_dj_log(not_understood_hint());
                        None
                    }
                    // A fuzzy phrase (queue-edit ask, fade, enqueue, ...) -> Claude.
                    Action::Nl(phrase) => {
                        self.dj_phase = Some("thinking...".to_string());
                        Some(Intent::Cc(phrase))
                    }
                }
            }
            KeyCode::Char(c) => {
                self.dj_input.push(c);
                None
            }
            _ => None,
        }
    }

    /// Append one line to the DJ scrollback, bounding it so a long session never
    /// grows without limit. Pure (no IO) - folded by the render thread on a CC frame.
    pub fn push_dj_log(&mut self, line: String) {
        const MAX_DJ_LOG: usize = 200;
        self.dj_log.push(line);
        if self.dj_log.len() > MAX_DJ_LOG {
            let drop = self.dj_log.len() - MAX_DJ_LOG;
            self.dj_log.drain(0..drop);
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    #[test]
    fn favorite_current_marks_a_stream_and_never_goes_silent() {
        // THE gesture. A stream has no star surface of its own, and the client holds
        // none of what the subject decision needs (the previous ICY title, the ages, the
        // provenance-stamped match, the ledger) - so every stream shape goes to the
        // daemon's `mark`, which always answers with a sentence.
        let mut s = TuiState::default();
        s.now.file = Some("https://stream.example/live".into());
        assert_eq!(
            s.favorite_current(),
            Some(Intent::Command("mark".into())),
            "a stream marks rather than refusing - this WAS the silent no-op"
        );
        assert!(s.status_msg.is_none(), "no client-side refusal is invented");

        // A stream with a recognized library match goes the SAME way. The old
        // client-side `match_uri` shortcut starred it silently, recorded nothing, and
        // could not age-check the match; `mark` stars the owned copy AND says so.
        let mut s = TuiState::default();
        s.now.file = Some("https://stream.example/live".into());
        s.now.match_uri = Some("song/s7".into());
        assert_eq!(s.favorite_current(), Some(Intent::Command("mark".into())));

        // A library song still stars itself, ignoring any match uri.
        let mut s = TuiState::default();
        s.now.file = Some("song/lib1".into());
        s.now.match_uri = Some("song/other".into());
        assert_eq!(
            s.favorite_current(),
            Some(Intent::Command("playlistadd Starred song/lib1".into()))
        );

        // Nothing playing is answered HERE rather than on a round trip - the client
        // already knows - but it is still answered, never silence.
        let mut s = TuiState::default();
        assert_eq!(s.favorite_current(), None);
        assert_eq!(s.status_msg.as_deref(), Some("nothing is playing to favorite"));
    }

    fn item(pos: usize) -> QueueItem {
        QueueItem {
            pos,
            title: format!("t{pos}"),
            artist: None,
            uri: Some(format!("song/{pos}")),
            album_uri: None,
        }
    }

    fn cmd(s: &str) -> Intent {
        Intent::Command(s.to_string())
    }

    #[test]
    fn coalesce_sums_a_held_scrub_burst_into_one_seek() {
        // A held Space burst: five +5 scrubs collapse to a single +25 seek, so the
        // player jumps once instead of draining five queued seeks after release.
        let batch = (0..5).map(|_| cmd("seekcur +5")).collect();
        assert_eq!(coalesce_intents(batch), vec![cmd("seekcur +25")]);
        // Mixed directions net out (held back then forward).
        let batch = vec![cmd("seekcur -5"), cmd("seekcur -5"), cmd("seekcur +5")];
        assert_eq!(coalesce_intents(batch), vec![cmd("seekcur -5")]);
        // A net-zero burst emits nothing (no spurious seek).
        assert_eq!(coalesce_intents(vec![cmd("seekcur +5"), cmd("seekcur -5")]), vec![]);
    }

    #[test]
    fn coalesce_collapses_a_held_radio_burst_to_one_gesture() {
        // `r` is a GLOBAL auto-repeating binding: resting a finger on it delivers a
        // Repeat stream, and every one of those would otherwise be a separate seed
        // resolve on the daemon. Holding it is ONE press.
        let batch = (0..8).map(|_| cmd("radio song/abc")).collect();
        assert_eq!(coalesce_intents(batch), vec![cmd("radio song/abc")]);
        // Two DIFFERENT radio gestures are two gestures, and a repeat that resumes
        // after another action is a new press, not a duplicate to swallow.
        let batch = vec![cmd("radio song/a"), cmd("radio song/b"), cmd("next"), cmd("radio song/b")];
        assert_eq!(
            coalesce_intents(batch),
            vec![cmd("radio song/a"), cmd("radio song/b"), cmd("next"), cmd("radio song/b")],
        );
        // And a held `>` still means N skips - only radio is idempotent.
        let batch = vec![cmd("next"), cmd("next")];
        assert_eq!(coalesce_intents(batch), vec![cmd("next"), cmd("next")]);
    }

    #[test]
    fn wake_gate_collapses_a_storm_to_one_refresh() {
        // First wake with nothing in flight -> start a refresh.
        assert!(wake_wants_refresh(false));
        // A wake while a refresh is already in flight -> dropped (storm collapses).
        assert!(!wake_wants_refresh(true));
    }

    #[test]
    fn stale_resp_dropped_below_current_epoch() {
        // A response from before a reconnect (older epoch) is stale.
        assert!(resp_is_stale(0, 1));
        // Same-epoch and future-epoch responses are live.
        assert!(!resp_is_stale(1, 1));
        assert!(!resp_is_stale(2, 1));
    }

    #[test]
    fn coalesce_preserves_order_around_non_scrub() {
        // Non-scrub intents pass through in order; a scrub run flushes as one seek
        // before the next distinct action. Absolute setvol is NOT a relative scrub.
        let batch = vec![
            cmd("seekcur +5"),
            cmd("seekcur +5"),
            cmd("knob down"),
            cmd("seekcur -5"),
            cmd("setvol 40"),
        ];
        assert_eq!(
            coalesce_intents(batch),
            vec![cmd("seekcur +10"), cmd("knob down"), cmd("seekcur -5"), cmd("setvol 40")],
        );
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn apply_snapshot_sets_and_clamps_selected() {
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(0), item(1), item(2)]);
        s.selected = 2;
        // Queue shrinks to 1 item -> selected clamps down to 0.
        s.apply_snapshot(NowPlaying::default(), vec![item(0)]);
        assert_eq!(s.queue.len(), 1);
        assert_eq!(s.selected, 0);
        // Empty queue -> selected 0.
        s.apply_snapshot(NowPlaying::default(), vec![]);
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn dj_screen_confirm_is_inline_in_chat_not_popup() {
        let mut s = TuiState::new();
        s.screen = Screen::Dj;
        s.enter_confirm(Pending {
            steps: vec!["[1] add 5 calmer tracks".into()],
            note: Some("append-only".into()),
            ..Default::default()
        });
        assert_eq!(s.mode, Mode::Confirm);
        // The echo + y/N prompt landed inline in the chat scrollback, not only a popup.
        assert!(s.dj_log.iter().any(|l| l.contains("add 5 calmer tracks")));
        assert!(s.dj_log.iter().any(|l| l == "! append-only"));
        assert!(s.dj_log.iter().any(|l| l == "confirm? [y/N]"));
        // On a non-DJ screen the confirm does NOT touch the chat log (popup carries it).
        let mut q = TuiState::new();
        q.enter_confirm(Pending { steps: vec!["clear".into()], ..Default::default() });
        assert!(q.dj_log.is_empty());
    }

    #[test]
    fn normal_transport_keys() {
        let mut s = TuiState::new();
        // Space is enqueue-selected (Queue: no-op); Backspace is a safe no-op.
        assert_eq!(s.handle_key(ch(' ')), None);
        assert_eq!(s.handle_key(key(KeyCode::Backspace)), None);
        // Pause on bare `p`.
        assert_eq!(s.handle_key(ch('p')), Some(Intent::Command("pause".into())));
        // `<`/`>` are prev/next.
        assert_eq!(s.handle_key(ch('<')), Some(Intent::Command("previous".into())));
        assert_eq!(s.handle_key(ch('>')), Some(Intent::Command("next".into())));
        assert_eq!(s.handle_key(ch('q')), Some(Intent::Quit));
        // Bare b/f are freed - no transport. Bare n/N are the repeat-search jumps:
        // with no standing search they are inert no-ops (return None, cursor put).
        assert_eq!(s.handle_key(ch('n')), None);
        assert_eq!(s.handle_key(ch('N')), None);
        assert_eq!(s.handle_key(ch('b')), None);
        assert_eq!(s.handle_key(ch('f')), None);
    }

    #[test]
    fn dj_view_honors_global_view_and_help_bindings() {
        let mut s = TuiState::new();
        s.screen = Screen::Dj;
        // F-keys switch views even from the DJ screen (Scope::Global, never NL text).
        assert_eq!(s.handle_key(key(KeyCode::F(2))), Some(Intent::ShowScreen(Screen::Albums)));
        assert_eq!(s.screen, Screen::Albums);
        // Back into DJ; `?` on an EMPTY ask line opens help.
        s.screen = Screen::Dj;
        assert!(s.dj_input.is_empty());
        assert_eq!(s.handle_key(ch('?')), None);
        assert!(s.help_open);
        // Close help, then a `?` typed mid-phrase is captured as input, not help.
        s.help_open = false;
        s.screen = Screen::Dj;
        s.handle_key(ch('w'));
        s.handle_key(ch('?'));
        assert!(!s.help_open, "? mid-phrase is text, not a help toggle");
        assert_eq!(s.dj_input, "w?");
    }

    #[test]
    fn dj_screen_all_f_keys_dispatch_via_match_key() {
        // Every F1-F5 switch resolves through the SINGLE-SOURCE keymap (match_key) even
        // from the DJ screen, so the Global view keys are alive on every screen and can
        // never drift from KEYMAP.
        for (fk, want) in [
            (KeyCode::F(1), Screen::Queue),
            (KeyCode::F(2), Screen::Albums),
            (KeyCode::F(3), Screen::Playlists),
            (KeyCode::F(5), Screen::Find),
        ] {
            let mut s = TuiState::new();
            s.screen = Screen::Dj;
            assert_eq!(s.handle_key(key(fk)), Some(Intent::ShowScreen(want)));
            assert_eq!(s.screen, want);
        }
    }




    #[test]
    fn radio_starts_from_every_row_kind_that_has_an_id() {
        use crate::find::{FindKind, FindRow, Focus};
        let hit = |kind: FindKind, label: &str, uri: &str| FindRow {
            kind,
            label: label.into(),
            uri: uri.into(),
            trailer: String::new(),
            song_count: None,
            album_uri: None,
            artist: None,
        };
        // The Find HIT list: all three kinds seed. The ARTIST row's first working
        // action - Enter emits an Enqueue the daemon rejects outright.
        let mut s = TuiState::new();
        s.screen = Screen::Find;
        s.find.focus = Focus::Results;
        s.find.hits.rows = vec![
            hit(FindKind::Artist, "El Waili", "artist/a1"),
            hit(FindKind::Album, "Volume Alpha", "album/b1"),
            hit(FindKind::Song, "Sweden", "song/s1"),
        ];
        assert_eq!(s.handle_key(ch('r')), Some(cmd("radio artist/a1")));
        assert_eq!(s.status_msg.as_deref(), Some("radio from El Waili"));
        s.find.selected = 1;
        assert_eq!(s.handle_key(ch('r')), Some(cmd("radio album/b1")));
        s.find.selected = 2;
        assert_eq!(s.handle_key(ch('r')), Some(cmd("radio song/s1")));

        // A Find DRILL row is an ordinary browse row and seeds the same way.
        s.find.drilling = true;
        s.find.drill.rows = vec![brow("Sweden", "song/s9", false)];
        assert_eq!(s.handle_key(ch('r')), Some(cmd("radio song/s9")));

        // Albums: an album dir row and a song row both seed.
        let mut s = TuiState::new();
        s.screen = Screen::Albums;
        s.albums.rows = vec![brow("Volume Alpha", "album/b1", true), brow("Sweden", "song/s1", false)];
        assert_eq!(s.handle_key(ch('r')), Some(cmd("radio album/b1")));
        s.albums.selected = 1;
        assert_eq!(s.handle_key(ch('r')), Some(cmd("radio song/s1")));

        // Queue: the row under the cursor, never the playing track.
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(0), item(1)]);
        s.selected = 1;
        assert_eq!(s.handle_key(ch('r')), Some(cmd("radio song/1")));
    }

    #[test]
    fn radio_on_an_unseedable_row_says_why_instead_of_acting() {
        // A row with no library id must never emit a command the daemon would ACK, and
        // must never be a dead key either: each case states its own reason.
        let mut s = TuiState::new();
        // Albums: a smart-list dir row has no id to seed from.
        s.screen = Screen::Albums;
        s.albums.rows = vec![brow("newest", "list/newest", true)];
        assert_eq!(s.handle_key(ch('r')), None);
        assert_eq!(s.status_msg.as_deref(), Some("can't start a radio from a list"));
        // Queue: a stream row.
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(0)]);
        s.queue[0].uri = Some("http://stream.example/live".into());
        assert_eq!(s.handle_key(ch('r')), None);
        assert_eq!(s.status_msg.as_deref(), Some("that row is a stream, can't start a radio"));
        // Playlists: the row is a NAME, not a uri.
        let mut s = TuiState::new();
        s.screen = Screen::Playlists;
        s.playlists.rows = vec![brow("Starred", "Starred", false)];
        assert_eq!(s.handle_key(ch('r')), None);
        assert_eq!(s.status_msg.as_deref(), Some("can't start a radio from a playlist"));
        // An empty list still answers rather than feeling dead.
        let mut s = TuiState::new();
        assert_eq!(s.handle_key(ch('r')), None);
        assert_eq!(s.status_msg.as_deref(), Some("nothing here to start a radio from"));
    }

    #[test]
    fn radio_over_the_find_hits_never_moves_the_queue_cursor() {
        // THE per-screen-helper trap: the hit list is not a `Browse`, so a resolver
        // that consulted active_browse() first would fall through to the QUEUE row and
        // start a radio from something the user is not even looking at.
        use crate::find::{FindKind, FindRow, Focus};
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(0), item(1)]);
        s.screen = Screen::Find;
        s.find.focus = Focus::Results;
        s.find.hits.rows = vec![FindRow {
            kind: FindKind::Song,
            label: "Sweden".into(),
            uri: "song/s1".into(),
            trailer: String::new(),
            song_count: None,
            album_uri: None,
            artist: None,
        }];
        assert_eq!(s.handle_key(ch('r')), Some(cmd("radio song/s1")), "the HIT row, not song/0");
        assert_eq!(s.selected, 0, "and the queue cursor never moved");
    }

    #[test]
    fn r_is_a_letter_on_an_input_line_never_a_gesture() {
        // On the Find query line and the DJ ask line `r` is text a user is typing; the
        // route to the verb from there is the `:`/ask line itself.
        let mut s = TuiState::new();
        s.screen = Screen::Find;
        assert_eq!(s.handle_key(ch('r')), None);
        assert_eq!(s.find.query, "r");
        let mut s = TuiState::new();
        s.screen = Screen::Dj;
        assert_eq!(s.handle_key(ch('r')), None);
        assert_eq!(s.dj_input, "r");
        // And a confirm popup still only answers y/N (never a radio behind the modal).
        let mut s = TuiState::new();
        s.enter_confirm(Pending { command: Some("clear".into()), ..Default::default() });
        assert_eq!(s.handle_key(ch('r')), None);
        assert_eq!(s.mode, Mode::Confirm);
    }

    #[test]
    fn the_colon_line_routes_radio_through_the_shared_router() {
        // One routing source with the CLI: `:radio` is the bare gesture, and the
        // keywords pass through verbatim.
        let mut s = TuiState::new();
        s.mode = Mode::Command;
        s.input = "radio".into();
        assert_eq!(s.handle_key(key(KeyCode::Enter)), Some(cmd("radio")));
        s.mode = Mode::Command;
        s.input = "radio off".into();
        assert_eq!(s.handle_key(key(KeyCode::Enter)), Some(cmd("radio off")));
    }

    #[test]
    fn a_lost_connection_stops_the_find_spinner_but_keeps_the_hits() {
        use crate::find::{FindKind, FindRow};
        let mut s = TuiState::new();
        s.screen = Screen::Find;
        s.find.phase = crate::find::Phase::Loading("c418".into());
        s.find.hits.rows = vec![FindRow {
            kind: FindKind::Song, label: "Sweden".into(), uri: "song/1".into(),
            trailer: String::new(), song_count: None, album_uri: None,
            artist: None,
        }];
        s.mark_disconnected();
        // The outstanding query can never land on the dead socket, so the spinner
        // would otherwise turn forever.
        assert!(matches!(s.find.phase, crate::find::Phase::Done), "spinner stopped");
        assert_eq!(s.find.hits.rows.len(), 1, "the hits are a truthful snapshot - kept");
        assert!(!s.find.drill.loaded, "the drill was fetched on the dead socket");
    }

    #[test]
    fn the_find_query_underlines_its_own_hits_without_a_standing_slash_search() {
        let mut s = TuiState::new();
        s.screen = Screen::Find;
        s.find.submitted = "c418".into();
        assert_eq!(s.highlight_query(), "c418");
        // A live `/` is the more recent intent and still wins.
        s.last_search = "sweden".into();
        assert_eq!(s.highlight_query(), "sweden");
    }

    #[test]
    fn section_jumps_are_a_no_op_off_the_find_screen() {
        let mut s = TuiState::new();
        s.screen = Screen::Queue;
        assert_eq!(s.handle_key(ch('}')), None);
        assert_eq!(s.selected, 0, "the queue cursor must not move");
    }

    #[test]
    fn open_drills_an_album_hit_but_a_song_hit_has_nothing_to_open() {
        use crate::find::{FindKind, FindRow, Focus};
        let mut s = TuiState::new();
        s.screen = Screen::Find;
        s.find.focus = Focus::Results;
        s.find.hits.rows = vec![
            FindRow { kind: FindKind::Album, label: "Volume Alpha".into(), uri: "album/9".into(), trailer: String::new(), song_count: None, album_uri: None, artist: None, },
            FindRow { kind: FindKind::Song, label: "Sweden".into(), uri: "song/1".into(), trailer: String::new(), song_count: None, album_uri: None, artist: None, },
        ];
        assert_eq!(s.handle_key(ch('l')), Some(Intent::BrowseInto("album/9".into())));
        s.find.selected = 1;
        assert_eq!(s.handle_key(ch('l')), None, "a song row has nothing to drill into");
    }

    #[test]
    fn every_key_answers_on_a_station_hit_row() {
        // A row the cursor can land on with no working verb is a defect (the artist row
        // had exactly this). So: Enter and Space ENQUEUE through the `station/<name>`
        // uri `enqueue_uri` already resolves, and `s`, `o` and `r` each SAY why they do
        // not apply rather than reading as a broken binding.
        use crate::find::{FindKind, FindRow, Focus};
        const URI: &str = "station/Moon Mission Recordings, Tokyo Deep and Electronic";
        let station = || FindRow {
            kind: FindKind::Station,
            label: "Moon Mission Recordings, Tokyo Deep and Electronic".into(),
            uri: URI.into(),
            trailer: "uk5.internet-radio.com".into(),
            song_count: None,
            album_uri: None,
            artist: None,
        };
        let fresh = || {
            let mut s = TuiState::new();
            // A populated queue is the trap: every per-screen helper that consults
            // active_browse() first falls through to the QUEUE cursor, silently acting
            // on a row the user is not even looking at.
            s.apply_snapshot(NowPlaying::default(), vec![item(0), item(1)]);
            s.screen = Screen::Find;
            s.find.focus = Focus::Results;
            s.find.hits.rows = vec![station()];
            s
        };

        // Enter: enqueue AND play.
        let mut s = fresh();
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            Some(Intent::Enqueue { uri: URI.into(), play: true }),
            "the name rides the uri whole, comma and spaces included"
        );
        assert_eq!(s.selected, 0, "the queue cursor never moved");

        // Space: enqueue without playing, and advance for rapid multi-add.
        let mut s = fresh();
        s.find.hits.rows.push(station());
        assert_eq!(
            s.handle_key(ch(' ')),
            Some(Intent::Enqueue { uri: URI.into(), play: false })
        );
        assert_eq!(s.find.selected, 1, "the HIT cursor advanced");
        assert_eq!(s.selected, 0, "and the queue cursor did not");

        // `s`: Subsonic has no star endpoint for internet radio at all, so this is a
        // refusal with a reason, never a silent no-op - and now the reason points at the
        // verb that DOES work on a station, which is marking what it is playing.
        let mut s = fresh();
        assert_eq!(s.handle_key(ch('s')), None);
        assert_eq!(
            s.status_msg.as_deref(),
            Some("a saved station is a stream - play it, then C-s marks what is on")
        );

        // `l`: a station is a leaf. Drilling would paint an empty bordered box over the
        // hits, so it says so instead. (This was `o` until the context menu took that
        // key; the drill body is unchanged, only the key that reaches it.)
        let mut s = fresh();
        assert_eq!(s.handle_key(ch('l')), None);
        assert_eq!(
            s.status_msg.as_deref(),
            Some("a station has nothing to open - enter plays it")
        );

        // `r`: a stream has no library id for the continuation walk to seed from, and
        // the message is station-specific rather than the generic fallback.
        let mut s = fresh();
        assert_eq!(s.handle_key(ch('r')), None);
        assert_eq!(
            s.status_msg.as_deref(),
            Some("a saved station is a stream, not a library seed - enter plays it")
        );
    }

    #[test]
    fn backing_out_of_a_drill_costs_no_round_trip_and_keeps_the_hits() {
        use crate::find::{FindKind, FindRow, Focus};
        let mut s = TuiState::new();
        s.screen = Screen::Find;
        s.find.focus = Focus::Results;
        s.find.submitted = "c418".into();
        s.find.hits.rows = vec![FindRow {
            kind: FindKind::Album, label: "Volume Alpha".into(), uri: "album/9".into(),
            trailer: String::new(), song_count: None, album_uri: None,
            artist: None,
        }];
        s.find.drilling = true;
        s.find.drill.rows = vec![crate::state::BrowseRow {
            label: "Sweden".into(), uri: "song/1".into(), is_dir: false, song_count: None,
            artist: None,
            album_uri: None,
        }];
        // `h` must NOT emit a BrowseBack: the drill sits at depth 0 with an empty
        // stack, so that would re-fetch `lsinfo ""` - the whole artist root.
        assert_eq!(s.handle_key(ch('h')), None, "no round trip on the way back");
        assert!(!s.find.drilling);
        assert_eq!(s.find.hits.rows.len(), 1, "the hits were never overwritten");
        assert_eq!(s.find.submitted, "c418", "and the query that produced them survives");
    }

    #[test]
    fn a_drill_response_that_lands_after_backing_out_is_dropped() {
        // Req::Browse carries no request identity and Browse::apply latches `loaded`,
        // so without the `drilling` gate a late response would silently win and sit
        // there waiting to flash under the NEXT drill's title.
        let mut s = TuiState::new();
        s.screen = Screen::Find;
        s.find.drilling = false;
        assert!(s.browse_for(Screen::Find).is_none(), "no drill target while not drilling");
        s.find.drilling = true;
        assert!(s.browse_for(Screen::Find).is_some());
    }

    #[test]
    fn cursor_keys_move_the_drill_while_drilling_and_the_hits_otherwise() {
        use crate::find::{FindKind, FindRow, Focus};
        let mut s = TuiState::new();
        s.screen = Screen::Find;
        s.find.focus = Focus::Results;
        s.find.hits.rows = vec![
            FindRow { kind: FindKind::Song, label: "a".into(), uri: "song/1".into(), trailer: String::new(), song_count: None, album_uri: None, artist: None, },
            FindRow { kind: FindKind::Song, label: "b".into(), uri: "song/2".into(), trailer: String::new(), song_count: None, album_uri: None, artist: None, },
        ];
        s.find.drill.rows = vec![
            crate::state::BrowseRow { label: "x".into(), uri: "song/8".into(), is_dir: false, song_count: None, artist: None, album_uri: None, },
            crate::state::BrowseRow { label: "y".into(), uri: "song/9".into(), is_dir: false, song_count: None, artist: None, album_uri: None, },
        ];
        s.find.drilling = true;
        s.handle_key(ch('j'));
        assert_eq!(s.find.drill.selected, 1, "the DRILL cursor moved");
        assert_eq!(s.find.selected, 0, "the hit cursor stayed put");
        assert_eq!(s.selected, 0, "and the queue cursor never moved");
        s.find.drilling = false;
        s.handle_key(ch('j'));
        assert_eq!(s.find.selected, 1, "off-drill, `j` moves the hits again");
    }

    #[test]
    fn slash_inside_a_drill_searches_the_drill_rows() {
        let mut s = TuiState::new();
        s.screen = Screen::Find;
        s.find.hits.rows = vec![];
        s.find.drill.rows = vec![
            crate::state::BrowseRow { label: "Sweden".into(), uri: "song/1".into(), is_dir: false, song_count: None, artist: None, album_uri: None, },
        ];
        s.find.drilling = true;
        assert_eq!(s.active_labels(), vec!["Sweden".to_string()]);
    }

    #[test]
    fn typing_in_the_find_query_produces_no_intent_at_all() {
        // This is the test that proves there is NO per-keystroke request path: the
        // screen is submit-driven, so a query is one round trip, not one per letter.
        let mut s = TuiState::new();
        s.screen = Screen::Find;
        for c in "c418".chars() {
            assert_eq!(s.handle_key(ch(c)), None, "typing {c:?} must not hit the network");
        }
        assert_eq!(s.find.query, "c418");
    }

    #[test]
    fn nav_keys_type_literally_in_the_query_but_move_the_cursor_in_the_results() {
        use crate::find::{FindKind, FindRow, Focus};
        let mut s = TuiState::new();
        s.screen = Screen::Find;
        // Query focus: `j` is a letter, not a movement.
        assert_eq!(s.handle_key(ch('j')), None);
        assert_eq!(s.find.query, "j");
        // Results focus: `j` moves the HIT cursor - and crucially NOT self.selected,
        // which is the queue cursor the active_browse() fallback would have moved.
        s.find.query.clear();
        s.find.focus = Focus::Results;
        s.find.hits.rows = vec![
            FindRow { kind: FindKind::Song, label: "a".into(), uri: "song/1".into(), trailer: String::new(), song_count: None, album_uri: None, artist: None, },
            FindRow { kind: FindKind::Song, label: "b".into(), uri: "song/2".into(), trailer: String::new(), song_count: None, album_uri: None, artist: None, },
        ];
        s.handle_key(ch('j'));
        assert_eq!(s.find.selected, 1, "the hit cursor moved");
        assert_eq!(s.selected, 0, "the QUEUE cursor must not have moved");
        assert_eq!(s.find.query, "", "and nothing was typed");
    }

    #[test]
    fn enter_submits_the_query_and_marks_it_in_flight() {
        let mut s = TuiState::new();
        s.screen = Screen::Find;
        s.find.query = "  c418  ".into();
        let intent = s.handle_key(key(KeyCode::Enter));
        assert_eq!(intent, Some(Intent::Find("c418".into())), "trimmed and dispatched");
        assert!(matches!(&s.find.phase, crate::find::Phase::Loading(q) if q == "c418"));
        assert_eq!(s.find.history[0], "c418", "recorded for the ^v ring");
    }

    #[test]
    fn an_empty_query_submits_nothing() {
        let mut s = TuiState::new();
        s.screen = Screen::Find;
        s.find.query = "   ".into();
        assert_eq!(s.handle_key(key(KeyCode::Enter)), None);
        assert!(matches!(s.find.phase, crate::find::Phase::Cold));
    }

    #[test]
    fn enter_on_a_result_plays_it_and_space_enqueues_without_playing() {
        use crate::find::{FindKind, FindRow, Focus};
        let mut s = TuiState::new();
        s.screen = Screen::Find;
        s.find.focus = Focus::Results;
        s.find.hits.rows = vec![FindRow {
            kind: FindKind::Album, label: "Volume Alpha".into(), uri: "album/9".into(),
            trailer: String::new(), song_count: None, album_uri: None,
            artist: None,
        }];
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            Some(Intent::Enqueue { uri: "album/9".into(), play: true }),
            "Enter ALWAYS plays, on every screen - drilling is `o`"
        );
        assert_eq!(
            s.handle_key(ch(' ')),
            Some(Intent::Enqueue { uri: "album/9".into(), play: false })
        );
    }

    #[test]
    fn favoriting_a_find_row_stars_the_row_under_the_cursor() {
        use crate::find::{FindKind, FindRow, Focus};
        let mut s = TuiState::new();
        s.screen = Screen::Find;
        s.find.focus = Focus::Results;
        s.find.hits.rows = vec![FindRow {
            kind: FindKind::Song, label: "Sweden".into(), uri: "song/1".into(),
            trailer: String::new(), song_count: None, album_uri: None,
            artist: None,
        }];
        assert_eq!(
            s.handle_key(ch('s')),
            Some(Intent::Command("playlistadd Starred song/1".into()))
        );
    }

    #[test]
    fn esc_leaves_the_find_screen_rather_than_ping_ponging() {
        let mut s = TuiState::new();
        s.screen = Screen::Find;
        assert_eq!(s.handle_key(key(KeyCode::Esc)), Some(Intent::ShowScreen(Screen::Queue)));
        assert_eq!(s.screen, Screen::Queue);
    }

    #[test]
    fn help_opens_on_an_empty_query_but_types_a_literal_question_mark_mid_query() {
        let mut s = TuiState::new();
        s.screen = Screen::Find;
        assert_eq!(s.handle_key(ch('?')), None);
        assert!(s.help_open, "? on an EMPTY query line opens help");
        s.handle_key(ch('?'));
        s.find.query = "who".into();
        s.handle_key(ch('?'));
        assert_eq!(s.find.query, "who?", "? mid-query is a literal character");
    }

    #[test]
    fn tab_toggles_focus_only_when_there_are_results_to_move_to() {
        use crate::find::{FindKind, FindRow, Focus};
        let mut s = TuiState::new();
        s.screen = Screen::Find;
        s.handle_key(key(KeyCode::Tab));
        assert_eq!(s.find.focus, Focus::Query, "no results yet: focus stays put");
        s.find.hits.rows = vec![FindRow {
            kind: FindKind::Song, label: "a".into(), uri: "song/1".into(),
            trailer: String::new(), song_count: None, album_uri: None,
            artist: None,
        }];
        s.handle_key(key(KeyCode::Tab));
        assert_eq!(s.find.focus, Focus::Results);
        s.handle_key(key(KeyCode::Tab));
        assert_eq!(s.find.focus, Focus::Query, "and back");
    }

    #[test]
    fn pressing_the_tab_key_you_are_already_on_is_a_no_op() {
        // switch_screen is IDEMPOTENT: re-pressing your current tab must not emit a
        // ShowScreen (which re-fetches) and must not clear a standing `/` query the
        // user is stepping through with `n`.
        for (fk, screen) in [
            (KeyCode::F(1), Screen::Queue),
            (KeyCode::F(2), Screen::Albums),
            (KeyCode::F(4), Screen::Dj),
            (KeyCode::F(5), Screen::Find),
        ] {
            let mut s = TuiState::new();
            s.screen = screen;
            s.last_search = "sweden".to_string();
            assert_eq!(s.handle_key(key(fk)), None, "{screen:?} re-press must not re-fetch");
            assert_eq!(s.screen, screen);
            assert_eq!(s.last_search, "sweden", "{screen:?} re-press must not wipe the / query");
        }
    }

    #[test]
    fn help_overlay_scrolls_and_resets() {
        let mut s = TuiState::new();
        // `?` opens help at the top.
        assert_eq!(s.handle_key(ch('?')), None);
        assert!(s.help_open);
        assert_eq!(s.help_scroll, 0);
        // j / Down scroll down; k / Up scroll up (clamped at 0). PageDown jumps.
        s.handle_key(ch('j'));
        s.handle_key(key(KeyCode::Down));
        assert_eq!(s.help_scroll, 2);
        s.handle_key(ch('k'));
        assert_eq!(s.help_scroll, 1);
        s.handle_key(key(KeyCode::PageDown));
        assert_eq!(s.help_scroll, 11);
        // Up never underflows.
        s.help_scroll = 0;
        s.handle_key(ch('k'));
        assert_eq!(s.help_scroll, 0);
        // Every other key is swallowed while the modal is open (no transport leak).
        assert_eq!(s.handle_key(ch('p')), None);
        assert!(s.help_open);
        // Closing resets the offset so the next open starts at the top.
        s.help_scroll = 5;
        s.handle_key(key(KeyCode::Esc));
        assert!(!s.help_open);
        assert_eq!(s.help_scroll, 0);
    }

    #[test]
    fn every_keymap_row_is_dispatched() {
        // Drift guard: each KEYMAP matcher must resolve through key_normal's table
        // dispatch (apply_act) to a real effect - never silently fall through. We only
        // assert it does not panic and that the Act round-trips (match_key), which with
        // apply_act's exhaustive match is the single-source contract.
        for b in keymap::KEYMAP {
            let mut s = TuiState::new();
            let ev = match b.matchers[0] {
                keymap::KeyMatch::Char(c) => ch(c),
                keymap::KeyMatch::Ctrl(c) => KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL),
                keymap::KeyMatch::Code(code) => key(code),
            };
            assert_eq!(
                keymap::match_key(ev, Screen::Queue),
                Some(b.act),
                "row {:?} does not resolve to its Act",
                b.keys
            );
            // Exercising dispatch must not panic for any table key.
            let _ = s.handle_key(ev);
        }
    }

    #[test]
    fn shift_p_jumps_queue_cursor_to_current_song() {
        let mut s = TuiState::new();
        let now = NowPlaying {
            song: Some(2),
            ..NowPlaying::default()
        };
        s.apply_snapshot(now, vec![item(0), item(1), item(2), item(3), item(4)]);
        s.selected = 4;
        // Shift+P (Char 'P') moves the cursor to the playing row (index 2).
        assert_eq!(s.handle_key(ch('P')), None);
        assert_eq!(s.selected, 2);
        // Idempotent.
        s.handle_key(ch('P'));
        assert_eq!(s.selected, 2);
        // Nothing playing -> cursor unchanged.
        s.apply_snapshot(NowPlaying::default(), vec![item(0), item(1)]);
        s.selected = 1;
        s.handle_key(ch('P'));
        assert_eq!(s.selected, 1);
        // On a browse screen it no-ops (no now-playing row there).
        s.screen = Screen::Albums;
        s.albums.rows = vec![brow("A", "album/1", true), brow("B", "album/2", true)];
        s.albums.selected = 1;
        s.handle_key(ch('P'));
        assert_eq!(s.albums.selected, 1);
    }

    #[test]
    fn n_and_shift_n_repeat_the_accepted_search() {
        let mut s = TuiState::new();
        s.albums.rows = vec![
            brow("Alpha", "album/1", true),
            brow("Beta", "album/2", true),
            brow("Gamma", "album/3", true),
            brow("beta two", "album/4", true),
        ];
        s.screen = Screen::Albums;
        // No standing search -> n/N are inert.
        s.handle_key(ch('n'));
        assert_eq!(s.albums.selected, 0);
        // Accept a `/beta` search: it jumps to the first match (index 1).
        s.handle_key(ch('/'));
        for c in "beta".chars() {
            s.handle_key(ch(c));
        }
        s.handle_key(key(KeyCode::Enter));
        assert_eq!(s.albums.selected, 1);
        assert_eq!(s.last_search, "beta");
        // n steps OFF the current match to the next one (index 3).
        s.handle_key(ch('n'));
        assert_eq!(s.albums.selected, 3);
        // n again wraps back to index 1.
        s.handle_key(ch('n'));
        assert_eq!(s.albums.selected, 1);
        // N steps backward, wrapping to index 3.
        s.handle_key(ch('N'));
        assert_eq!(s.albums.selected, 3);
        // A screen switch clears the standing search (no stale highlight/jump).
        s.handle_key(key(KeyCode::F(1)));
        assert_eq!(s.last_search, "");
    }

    #[test]
    fn search_step_directions_wrap_and_case() {
        let labels = ["Alpha", "Beta", "Gamma", "beta two"];
        // Forward from origin 2 finds index 3 (wrapping would reach 1).
        assert_eq!(search_step(&labels, "beta", 2, true), Some(3));
        // Backward from origin 2 finds index 1.
        assert_eq!(search_step(&labels, "beta", 2, false), Some(1));
        // Case-insensitive.
        assert_eq!(search_step(&labels, "GAMMA", 0, true), Some(2));
        // Forward wrap from a late origin.
        assert_eq!(search_step(&labels, "alpha", 3, true), Some(0));
        // Empty query / empty list / no match -> None.
        assert_eq!(search_step(&labels, "", 0, true), None);
        assert_eq!(search_step(&[], "x", 0, true), None);
        assert_eq!(search_step(&labels, "zzz", 0, true), None);
    }

    #[test]
    fn album_mark_none_partial_full() {
        // Nothing queued -> None.
        assert_eq!(album_mark(0, Some(10)), QueueMark::None);
        // All tracks queued (>= count) -> Full.
        assert_eq!(album_mark(10, Some(10)), QueueMark::Full);
        assert_eq!(album_mark(11, Some(10)), QueueMark::Full);
        // Some but not all -> Partial.
        assert_eq!(album_mark(3, Some(10)), QueueMark::Partial);
        // Unknown or zero songCount with queued tracks -> Partial (never false Full).
        assert_eq!(album_mark(3, None), QueueMark::Partial);
        assert_eq!(album_mark(3, Some(0)), QueueMark::Partial);
    }

    #[test]
    fn queued_by_album_dedups_and_groups() {
        let mut s = TuiState::new();
        let it = |pos: usize, uri: &str, al: &str| QueueItem {
            pos,
            title: format!("t{pos}"),
            artist: None,
            uri: Some(uri.into()),
            album_uri: Some(al.into()),
        };
        // Two distinct songs of album/1, plus a DUPLICATE of song/1 (must not
        // double-count), plus one song of album/2.
        s.queue = vec![
            it(0, "song/1", "album/1"),
            it(1, "song/2", "album/1"),
            it(2, "song/1", "album/1"),
            it(3, "song/9", "album/2"),
        ];
        let map = s.queued_by_album();
        assert_eq!(map.get("album/1").map(|s| s.len()), Some(2));
        assert_eq!(map.get("album/2").map(|s| s.len()), Some(1));
        // A full album/1 (count 2) marks Full despite the duplicate row.
        assert_eq!(album_mark(map["album/1"].len(), Some(2)), QueueMark::Full);
        assert!(s.queued_uris().contains("song/9"));
    }

    #[test]
    fn parse_browse_captures_song_count_on_dir() {
        let pairs: Vec<(String, String)> = vec![
            ("directory".into(), "album/1".into()),
            ("Album".into(), "X".into()),
            ("X-SongCount".into(), "12".into()),
            ("file".into(), "song/9".into()),
            ("Title".into(), "T".into()),
            // A stray count on a file row is ignored (not a dir).
            ("X-SongCount".into(), "99".into()),
        ];
        let rows = parse_browse(&pairs);
        assert_eq!(rows[0].song_count, Some(12));
        assert_eq!(rows[1].song_count, None);
    }

    #[test]
    fn ctrl_f_and_b_scrub() {
        let mut s = TuiState::new();
        assert_eq!(s.handle_key(ctrl('f')), Some(Intent::Command("seekcur +5".into())));
        assert_eq!(s.handle_key(ctrl('b')), Some(Intent::Command("seekcur -5".into())));
    }

    #[test]
    fn ctrl_s_favorites_current() {
        let mut s = TuiState::new();
        // Current song row -> star it.
        s.now.file = Some("song/3".into());
        assert_eq!(
            s.handle_key(ctrl('s')),
            Some(Intent::Command("playlistadd Starred song/3".into()))
        );
        // A stream -> the mark gesture, NOT a refusal. This key was a silent no-op on
        // 92% of his listening; the whole point is that it now always does something.
        s.now.file = Some("http://stream.example/live".into());
        assert_eq!(s.handle_key(ctrl('s')), Some(Intent::Command("mark".into())));
        // Nothing playing -> friendly status.
        s.now.file = None;
        assert_eq!(s.handle_key(ctrl('s')), None);
        assert!(s.status_msg.is_some());
    }

    #[test]
    fn ctrl_s_on_the_find_query_line_marks_and_never_types_an_s() {
        // crossterm delivers Ctrl-s as `Char('s') + CONTROL`, and key_find's Char arm
        // inspected only the CODE - so the gesture silently TYPED an `s` into the query
        // the user was writing. A key that corrupts input is worse than a dead one.
        let mut s = TuiState::new();
        s.screen = Screen::Find;
        s.find.query = "takuya".into();
        s.now.file = Some("http://stream.example/live".into());
        assert_eq!(s.handle_key(ctrl('s')), Some(Intent::Command("mark".into())));
        assert_eq!(s.find.query, "takuya", "the query is UNCHANGED");
        // A plain `s` is still ordinary query text on this line.
        assert_eq!(s.handle_key(ch('s')), None);
        assert_eq!(s.find.query, "takuyas");
    }

    #[test]
    fn ctrl_s_on_the_dj_ask_line_marks_and_never_types_an_s() {
        let mut s = TuiState::new();
        s.screen = Screen::Dj;
        s.dj_input = "something like".into();
        s.now.file = Some("http://stream.example/live".into());
        assert_eq!(s.handle_key(ctrl('s')), Some(Intent::Command("mark".into())));
        assert_eq!(s.dj_input, "something like", "the ask line is UNCHANGED");
        // A plain `s` is still ordinary ask-line text.
        assert_eq!(s.handle_key(ch('s')), None);
        assert_eq!(s.dj_input, "something likes");
    }

    #[test]
    fn ctrl_np_move_cursor() {
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(0), item(1), item(2)]);
        assert_eq!(s.handle_key(ctrl('n')), None);
        assert_eq!(s.selected, 1);
        assert_eq!(s.handle_key(ctrl('n')), None);
        assert_eq!(s.selected, 2);
        assert_eq!(s.handle_key(ctrl('p')), None);
        assert_eq!(s.selected, 1);
    }

    #[test]
    fn g_and_shift_g_jump_and_empty_noop() {
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(0), item(1), item(2), item(3)]);
        s.selected = 2;
        s.handle_key(ch('g'));
        assert_eq!(s.selected, 0);
        s.handle_key(ch('G'));
        assert_eq!(s.selected, 3);
        // Empty queue -> both no-op.
        s.apply_snapshot(NowPlaying::default(), vec![]);
        s.handle_key(ch('G'));
        assert_eq!(s.selected, 0);
        s.handle_key(ch('g'));
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn s_favorites_selected_row() {
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(6), item(7)]);
        s.selected = 1;
        assert_eq!(
            s.handle_key(ch('s')),
            Some(Intent::Command("playlistadd Starred song/7".into()))
        );
        // A stream row that is NOT the one playing: the subject of a mark is what is on
        // AIR, not what the cursor is on, so it says what to do instead of guessing.
        s.queue[1].uri = Some("http://stream.example/live".into());
        assert_eq!(s.handle_key(ch('s')), None);
        assert_eq!(
            s.status_msg.as_deref(),
            Some("that row is a stream - play it, then s marks what is on")
        );
        // No uri at all -> a reason, not the old silent no-op.
        s.queue[1].uri = None;
        assert_eq!(s.handle_key(ch('s')), None);
        assert!(s.status_msg.is_some(), "a row the cursor can land on always answers");
        // Empty queue -> no command, and it says there is nothing here.
        s.apply_snapshot(NowPlaying::default(), vec![]);
        assert_eq!(s.handle_key(ch('s')), None);
        assert_eq!(s.status_msg.as_deref(), Some("nothing here to star"));
    }

    #[test]
    fn s_on_the_playing_stream_row_marks_it() {
        // "that row is a stream, can't favorite" became FALSE the moment `mark` shipped,
        // and a false reason is worse than a dead key.
        let mut s = TuiState::new();
        let mut now = NowPlaying::default();
        now.song = Some(1);
        let mut stream = item(1);
        stream.uri = Some("http://stream.example/live".into());
        s.apply_snapshot(now, vec![item(0), stream]);
        s.selected = 1;
        assert_eq!(s.handle_key(ch('s')), Some(Intent::Command("mark".into())));
    }

    #[test]
    fn t_asks_the_daemon_for_the_marks_and_their_audio() {
        // `mark` keeps the audio of what it marked, and the segment outlives by weeks the
        // one-line banner that announced it. Without a key for the read-back the only
        // route was `:heard` typed from memory or leaving for `dj heard` at a shell - a
        // gesture whose result you cannot look at again.
        //
        // `marks` rather than the default view: a marked row is the ONLY row that can
        // carry a segment, so this key asks for exactly what it is about.
        let mut s = TuiState::new();
        assert_eq!(s.handle_key(ch('t')), Some(Intent::Command("heard marks".into())));
        // And it does NOT open optimistically: a panel painted around an answer still in
        // flight would be claiming to show something it does not have.
        assert!(!s.heard_open, "the overlay waits for the reply");
    }

    #[test]
    fn the_tape_overlay_is_a_modal_that_scrolls_and_swallows_everything_else() {
        // A ledger takes longer to read than the next keypress, so this must NOT be
        // dismissed the way a one-line banner is (any key). Same contract as help.
        let mut s = TuiState::new();
        s.open_heard(vec!["3 marks, oldest first".into(), "23:17  * NTS 2  [tape 2: 5m, window]".into()]);
        assert!(s.heard_open);
        assert_eq!(s.heard_scroll, 0);
        // Scrolls.
        assert_eq!(s.handle_key(ch('j')), None);
        assert_eq!(s.heard_scroll, 1);
        assert_eq!(s.handle_key(ch('k')), None);
        assert_eq!(s.heard_scroll, 0);
        // Under-scroll saturates rather than wrapping to the last page.
        assert_eq!(s.handle_key(ch('k')), None);
        assert_eq!(s.heard_scroll, 0);
        // Transport and nav keys are SWALLOWED: `p` must not pause the music under a
        // panel the human is reading, and `>` must not skip the very track being read
        // about.
        assert_eq!(s.handle_key(ch('p')), None);
        assert_eq!(s.handle_key(ch('>')), None);
        assert_eq!(s.handle_key(ctrl('s')), None, "and a mark cannot fire from under it");
        assert!(s.heard_open, "none of that closed it either");
        // Its own letter closes it, and the scroll resets so the next open starts at the
        // top rather than wherever the last read ended.
        s.heard_scroll = 4;
        assert_eq!(s.handle_key(ch('t')), None);
        assert!(!s.heard_open);
        assert_eq!(s.heard_scroll, 0);
        // Esc and q close it too, exactly as they close help.
        s.open_heard(vec!["a".into(), "b".into()]);
        assert_eq!(s.handle_key(key(KeyCode::Esc)), None);
        assert!(!s.heard_open);
        s.open_heard(vec!["a".into(), "b".into()]);
        assert_eq!(s.handle_key(ch('q')), None);
        assert!(!s.heard_open, "q closes the panel rather than quitting the program");
    }

    #[test]
    fn an_empty_read_back_never_opens_an_empty_panel() {
        // The daemon always renders at least a coverage line or a reason, so nothing at
        // all means an OLDER daemon - which the worker turns into a sentence on the bar.
        // An empty box would be a key that answered with a blank stare.
        let mut s = TuiState::new();
        s.open_heard(Vec::new());
        assert!(!s.heard_open);
        assert!(s.heard_lines.is_empty());
    }

    #[test]
    fn help_wins_over_the_tape_panel_so_the_modals_cannot_stack() {
        // Both are normal-mode modals and `?` is checked first; with help open, `t` must
        // scroll nothing and close nothing - it is swallowed like every other key.
        let mut s = TuiState::new();
        s.open_heard(vec!["a".into(), "b".into()]);
        s.help_open = true;
        assert_eq!(s.handle_key(ch('t')), None);
        assert!(s.help_open, "help still owns the keyboard");
        assert!(s.heard_open, "and the panel under it is untouched");
    }

    #[test]
    fn s_stars_the_visible_album_row_not_the_queue_cursor() {
        // THE wrong-target star. `favorite_selected` fell through to
        // `self.queue[self.selected]` on every non-Find screen, which is the QUEUE
        // cursor - so `s` on an Albums row starred whatever library song happened to sit
        // at that queue index. Invisible, wrong, and a WRITE into his library.
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(11), item(22), item(33)]);
        s.selected = 2; // the queue cursor sits on song/33 and must not be touched
        s.screen = Screen::Albums;
        s.albums.rows = vec![
            BrowseRow { label: "Sun Ra".into(), uri: "album/a1".into(), is_dir: true, song_count: None, artist: None, album_uri: None, },
            BrowseRow { label: "Takuya Nakamura".into(), uri: "album/a2".into(), is_dir: true, song_count: None, artist: None, album_uri: None, },
        ];
        s.albums.selected = 1;
        assert_eq!(
            s.handle_key(ch('s')),
            Some(Intent::Command("playlistadd Starred album/a2".into())),
            "the VISIBLE album is starred, never queue[selected]"
        );
        assert_eq!(s.selected, 2, "the queue cursor never moved");

        // A smart-list row on the same screen is not starrable, and says so rather than
        // reaching for the queue.
        s.albums.rows.push(BrowseRow {
            label: "Recently Added".into(),
            uri: "list/recent".into(),
            is_dir: true,
            song_count: None,
            artist: None,
            album_uri: None,
        });
        s.albums.selected = 2;
        assert_eq!(s.handle_key(ch('s')), None);
        assert_eq!(s.status_msg.as_deref(), Some("can't star a smart list"));
    }

    #[test]
    fn s_on_a_playlist_row_says_why_instead_of_starring_a_queue_song() {
        // A Playlists row is a NAME, not a uri - the same reason `r` cannot seed a radio
        // from one. Before the per-screen dispatch this starred queue[selected].
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(5)]);
        s.screen = Screen::Playlists;
        s.playlists.rows = vec![BrowseRow {
            label: "Starred".into(),
            uri: "Starred".into(),
            is_dir: false,
            song_count: None,
            artist: None,
            album_uri: None,
        }];
        assert_eq!(s.handle_key(ch('s')), None);
        assert_eq!(
            s.status_msg.as_deref(),
            Some("a playlist is a name, not a track - can't star it")
        );
    }

    #[test]
    fn space_enqueues_and_advances_on_browse() {
        let mut s = TuiState::new();
        // Queue: space is a no-op and leaves the cursor put.
        s.apply_snapshot(NowPlaying::default(), vec![item(0), item(1)]);
        assert_eq!(s.handle_key(ch(' ')), None);
        assert_eq!(s.selected, 0);
        // Browse: space enqueues the selected uri (no play) and advances the cursor.
        s.albums.rows = vec![brow("a", "song/1", false), brow("b", "song/2", false)];
        s.screen = Screen::Albums;
        assert_eq!(
            s.handle_key(ch(' ')),
            Some(Intent::Enqueue { uri: "song/1".into(), play: false })
        );
        assert_eq!(s.albums.selected, 1);
        // Playlists: space loads the playlist (a name is not a file uri) and advances.
        s.playlists.rows = vec![brow("Starred", "Starred", false), brow("Chill", "Chill", false)];
        s.screen = Screen::Playlists;
        assert_eq!(s.handle_key(ch(' ')), Some(Intent::LoadPlaylist("Starred".into())));
        assert_eq!(s.playlists.selected, 1);
    }

    #[test]
    fn l_drills_the_selected_dir_exactly_as_o_used_to() {
        // The no-regression test for the rebind: `o` now opens the context menu, and the
        // drill it used to be moved VERBATIM onto `l` / Right. Every assertion here is
        // the one `o` carried before, so the behavior is provably unchanged.
        let mut s = TuiState::new();
        s.albums.rows = vec![brow("X", "album/9", true), brow("song", "song/7", false)];
        s.screen = Screen::Albums;
        // A dir row -> BrowseInto.
        assert_eq!(s.handle_key(ch('l')), Some(Intent::BrowseInto("album/9".into())));
        // Right is the same binding, so it must reach the same Act.
        assert_eq!(
            s.handle_key(key(KeyCode::Right)),
            Some(Intent::BrowseInto("album/9".into()))
        );
        // A song row -> no-op.
        s.albums.selected = 1;
        assert_eq!(s.handle_key(ch('l')), None);
        // Queue -> no-op.
        s.screen = Screen::Queue;
        assert_eq!(s.handle_key(ch('l')), None);
    }

    #[test]
    fn esc_in_normal_backs_out_browse() {
        let mut s = TuiState::new();
        // Queue: Esc is a no-op.
        assert_eq!(s.handle_key(key(KeyCode::Esc)), None);
        s.screen = Screen::Albums;
        // Browse root (empty stack) -> no-op.
        assert_eq!(s.handle_key(key(KeyCode::Esc)), None);
        // Browse sub-level (non-empty stack) -> BrowseBack.
        s.albums.stack.push(("list/newest".into(), 0));
        assert_eq!(s.handle_key(key(KeyCode::Esc)), Some(Intent::BrowseBack));
    }

    #[test]
    fn colon_opens_command_slash_opens_search() {
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(0), item(1)]);
        s.selected = 1;
        // `:` -> Command mode.
        assert_eq!(s.handle_key(ch(':')), None);
        assert_eq!(s.mode, Mode::Command);
        s.handle_key(key(KeyCode::Esc));
        // `/` -> Search mode, seeding search_origin from the active cursor.
        assert_eq!(s.handle_key(ch('/')), None);
        assert_eq!(s.mode, Mode::Search);
        assert_eq!(s.search_origin, 1);
    }

    #[test]
    fn search_jump_matches_wraps_and_cases() {
        let labels = ["Alpha", "Beta", "Gamma", "beta two"];
        // Forward match from origin 0.
        assert_eq!(search_jump(&labels, "Beta", 0), Some(1));
        // Case-insensitive.
        assert_eq!(search_jump(&labels, "gamma", 0), Some(2));
        // Wrap-around from a late origin: from 3, "alpha" wraps to index 0.
        assert_eq!(search_jump(&labels, "alpha", 3), Some(0));
        // From origin 2, "beta" finds index 3 first (forward), not 1.
        assert_eq!(search_jump(&labels, "beta", 2), Some(3));
        // Empty query keeps the cursor at origin.
        assert_eq!(search_jump(&labels, "", 2), Some(2));
        // No match -> None.
        assert_eq!(search_jump(&labels, "zzz", 0), None);
        // Empty list -> None.
        assert_eq!(search_jump(&[], "x", 0), None);
    }

    #[test]
    fn search_mode_transitions() {
        let mut s = TuiState::new();
        s.albums.rows = vec![
            brow("Alpha", "song/1", false),
            brow("Beta", "song/2", false),
            brow("Gamma", "song/3", false),
        ];
        s.screen = Screen::Albums;
        s.albums.selected = 0;
        // Enter search, seeding origin.
        s.handle_key(ch('/'));
        assert_eq!(s.search_origin, 0);
        // Typing a matching char jumps the active cursor.
        s.handle_key(ch('g'));
        assert_eq!(s.albums.selected, 2);
        // Enter accepts in place and returns to Normal.
        s.handle_key(key(KeyCode::Enter));
        assert_eq!(s.mode, Mode::Normal);
        assert_eq!(s.albums.selected, 2);
        // A no-match char leaves the cursor at origin (never jumps to 0 blindly).
        s.albums.selected = 1;
        s.handle_key(ch('/'));
        assert_eq!(s.search_origin, 1);
        s.handle_key(ch('z'));
        assert_eq!(s.albums.selected, 1);
        // Esc restores the pre-search cursor (origin 1), even after it moved.
        s.albums.selected = 2;
        s.handle_key(key(KeyCode::Esc));
        assert_eq!(s.mode, Mode::Normal);
        assert_eq!(s.albums.selected, 1);
    }

    #[test]
    fn keys_9_and_0_turn_the_knob() {
        let mut s = TuiState::new();
        // The knob is a server-side relative control: the keys emit `knob up|down`
        // regardless of the current (client-side, possibly stale) volume, and the
        // server owns the dB step + the off-click pause. 0/+/= up, 9/-/_ down.
        assert_eq!(s.handle_key(ch('0')), Some(Intent::Command("knob up".into())));
        assert_eq!(s.handle_key(ch('9')), Some(Intent::Command("knob down".into())));
        assert_eq!(s.handle_key(ch('+')), Some(Intent::Command("knob up".into())));
        assert_eq!(s.handle_key(ch('-')), Some(Intent::Command("knob down".into())));
        // No dependence on knowing the current volume.
        s.now.volume = None;
        assert_eq!(s.handle_key(ch('0')), Some(Intent::Command("knob up".into())));
    }

    #[test]
    fn scroll_offset_top_edge_and_bottom_and_tiny() {
        // Top-edge exception: cursor within the top margin -> offset 0 (literal top).
        assert_eq!(scroll_offset(1, 100, 10, 0), 0);
        assert_eq!(scroll_offset(0, 100, 10, 0), 0);
        // Moving down past the bottom margin scrolls: sel 6, h 10, so 3 -> pins at
        // 6 + 3 + 1 - 10 = 0 still (6+3 < 10). sel 7 -> 7+3+1-10 = 1.
        assert_eq!(scroll_offset(7, 100, 10, 0), 1);
        // Mid-list the cursor pins at h-1-so while scrolling: sel 50 -> 50+3+1-10=44.
        assert_eq!(scroll_offset(50, 100, 10, 40), 44);
        // Bottom: last row reachable, offset clamps to n-h = 90.
        assert_eq!(scroll_offset(99, 100, 10, 80), 90);
        // Tiny viewport: so shrinks to (h-1)/2 so margins never overlap.
        // h=2 -> so=0; sel 5 -> off = 5+0+1-2 = 4, clamped to n-h=8 -> 4.
        assert_eq!(scroll_offset(5, 10, 2, 0), 4);
        // Empty queue / zero height -> 0.
        assert_eq!(scroll_offset(0, 0, 10, 5), 0);
        assert_eq!(scroll_offset(3, 100, 0, 5), 0);
    }

    #[test]
    fn enter_plays_selected_and_arrows_move() {
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(10), item(11), item(12)]);
        // j/Down and k/Up move within bounds, no wrap.
        s.handle_key(ch('j'));
        assert_eq!(s.selected, 1);
        s.handle_key(key(KeyCode::Down));
        assert_eq!(s.selected, 2);
        s.handle_key(key(KeyCode::Down)); // clamp at last
        assert_eq!(s.selected, 2);
        // Enter plays the SELECTED item's pos (12), not the index.
        assert_eq!(s.handle_key(key(KeyCode::Enter)), Some(Intent::Command("play 12".into())));
        s.handle_key(ch('k'));
        s.handle_key(key(KeyCode::Up));
        assert_eq!(s.selected, 0);
        s.handle_key(key(KeyCode::Up)); // clamp at 0
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn command_mode_edit() {
        let mut s = TuiState::new();
        s.handle_key(ch(':'));
        assert_eq!(s.mode, Mode::Command);
        s.handle_key(ch('a'));
        s.handle_key(ch('b'));
        s.handle_key(key(KeyCode::Backspace));
        assert_eq!(s.input, "a");
        s.handle_key(key(KeyCode::Esc));
        assert_eq!(s.mode, Mode::Normal);
        assert_eq!(s.input, "");
    }

    #[test]
    fn submit_routes_verb_vs_nl() {
        let mut s = TuiState::new();
        s.mode = Mode::Command;
        s.input = "pause".into();
        assert_eq!(s.handle_key(key(KeyCode::Enter)), Some(Intent::Command("pause".into())));
        assert_eq!(s.mode, Mode::Normal);
        s.mode = Mode::Command;
        s.input = "fade out".into();
        assert_eq!(s.handle_key(key(KeyCode::Enter)), Some(Intent::Nl("fade out".into())));
    }

    #[test]
    fn confirm_flow() {
        let mut s = TuiState::new();
        s.enter_confirm(Pending {
            token: Some("nl-1".into()),
            command: None,
            steps: vec!["[1] fade out".into()],
            note: Some("NOTE: caveat".into()),
            trust: None,
        });
        assert_eq!(s.mode, Mode::Confirm);
        assert_eq!(s.pending.as_ref().unwrap().steps, vec!["[1] fade out".to_string()]);
        assert_eq!(s.handle_key(ch('x')), None); // ignored
        assert_eq!(s.handle_key(ch('y')), Some(Intent::ConfirmArm));
        assert_eq!(s.handle_key(ch('n')), Some(Intent::ConfirmCancel));
        assert_eq!(s.handle_key(key(KeyCode::Esc)), Some(Intent::ConfirmCancel));
    }

    #[test]
    fn clear_confirm_path() {
        let mut s = TuiState::new();
        s.mode = Mode::Command;
        s.input = "clear".into();
        assert_eq!(s.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(s.mode, Mode::Confirm);
        let p = s.pending.as_ref().unwrap();
        assert_eq!(p.command, Some("clear".to_string()));
        assert_eq!(p.token, None);
    }

    fn brow(label: &str, uri: &str, is_dir: bool) -> BrowseRow {
        BrowseRow { label: label.into(), uri: uri.into(), is_dir, song_count: None, artist: None, album_uri: None, }
    }

    #[test]
    fn screen_switch_keys_set_screen_and_intent() {
        let mut s = TuiState::new();
        assert_eq!(s.handle_key(key(KeyCode::F(2))), Some(Intent::ShowScreen(Screen::Albums)));
        assert_eq!(s.screen, Screen::Albums);
        assert_eq!(s.handle_key(key(KeyCode::F(3))), Some(Intent::ShowScreen(Screen::Playlists)));
        assert_eq!(s.screen, Screen::Playlists);
        assert_eq!(s.handle_key(key(KeyCode::F(1))), Some(Intent::ShowScreen(Screen::Queue)));
        assert_eq!(s.screen, Screen::Queue);
    }

    #[test]
    fn nav_moves_active_screen_cursor() {
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(0), item(1), item(2)]);
        s.albums.rows = vec![brow("a", "album/1", true), brow("b", "album/2", true), brow("c", "album/3", true)];
        s.screen = Screen::Albums;
        s.handle_key(ch('j'));
        assert_eq!(s.albums.selected, 1);
        s.handle_key(ch('j'));
        s.handle_key(ch('j')); // clamp, no wrap
        assert_eq!(s.albums.selected, 2);
        s.handle_key(ch('k'));
        assert_eq!(s.albums.selected, 1);
        // Queue cursor untouched.
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn enter_is_contextual_per_screen() {
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(4), item(5)]);
        s.selected = 1;
        // Queue: play selected pos.
        assert_eq!(s.handle_key(key(KeyCode::Enter)), Some(Intent::Command("play 5".into())));
        // Albums: Enter now PLAYS both a dir (enqueue album + play first) and a song.
        s.albums.rows = vec![brow("X", "album/9", true), brow("song", "song/7", false)];
        s.screen = Screen::Albums;
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            Some(Intent::Enqueue { uri: "album/9".into(), play: true })
        );
        s.albums.selected = 1;
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            Some(Intent::Enqueue { uri: "song/7".into(), play: true })
        );
        // Playlists -> LoadPlaylist(name).
        s.playlists.rows = vec![brow("Starred", "Starred", false)];
        s.screen = Screen::Playlists;
        assert_eq!(s.handle_key(key(KeyCode::Enter)), Some(Intent::LoadPlaylist("Starred".into())));
    }

    #[test]
    fn browse_back_needs_stack_and_screen() {
        let mut s = TuiState::new();
        // Queue: h is a no-op.
        assert_eq!(s.handle_key(ch('h')), None);
        s.screen = Screen::Albums;
        // No stack yet -> no-op.
        assert_eq!(s.handle_key(ch('h')), None);
        s.albums.stack.push(("list/newest".into(), 0));
        assert_eq!(s.handle_key(ch('h')), Some(Intent::BrowseBack));
        assert_eq!(s.handle_key(key(KeyCode::Left)), Some(Intent::BrowseBack));
    }

    #[test]
    fn transport_keys_work_on_every_screen() {
        let mut s = TuiState::new();
        s.screen = Screen::Albums;
        assert_eq!(s.handle_key(ctrl('f')), Some(Intent::Command("seekcur +5".into())));
        assert_eq!(s.handle_key(ch('p')), Some(Intent::Command("pause".into())));
        assert_eq!(s.handle_key(ch('>')), Some(Intent::Command("next".into())));
        assert_eq!(s.handle_key(ch('0')), Some(Intent::Command("knob up".into())));
    }

    #[test]
    fn apply_now_keeps_queue_and_cursor() {
        // The fast refresh path (queue version unchanged) updates only now-playing
        // and must NOT touch the held queue or the cursor.
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(0), item(1), item(2)]);
        s.selected = 2;
        let mut np = NowPlaying::default();
        np.title = Some("New Track".into());
        s.apply_now(np);
        assert_eq!(s.now.title.as_deref(), Some("New Track"), "now-playing updated");
        assert_eq!(s.queue.len(), 3, "queue untouched");
        assert_eq!(s.selected, 2, "cursor untouched");
    }

    #[test]
    fn empty_browse_enter_and_move_noop() {
        let mut s = TuiState::new();
        s.screen = Screen::Albums;
        assert_eq!(s.handle_key(key(KeyCode::Enter)), None);
        s.handle_key(ch('j'));
        assert_eq!(s.albums.selected, 0);
    }

    #[test]
    fn parse_browse_groups_dirs_songs_playlists() {
        let pairs: Vec<(String, String)> = vec![
            ("directory".into(), "album/1".into()),
            ("Album".into(), "X".into()),
            ("file".into(), "song/9".into()),
            ("Title".into(), "T".into()),
            ("Artist".into(), "A".into()),
            ("playlist".into(), "Starred".into()),
        ];
        let rows = parse_browse(&pairs);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            BrowseRow { label: "X".into(), uri: "album/1".into(), is_dir: true, song_count: None, artist: None, album_uri: None, }
        );
        assert_eq!(
            rows[1],
            // The credit is folded into the label the eye reads AND kept raw: the
            // composed "T - A" is a display string, so "go to artist" needs "A" alone.
            BrowseRow { label: "T - A".into(), uri: "song/9".into(), is_dir: false, song_count: None, artist: Some("A".into()), album_uri: None, }
        );
        assert_eq!(
            rows[2],
            BrowseRow {
                label: "Starred".into(),
                uri: "Starred".into(),
                is_dir: false,
                song_count: None,
                artist: None,
                album_uri: None,
            }
        );
    }

    #[test]
    fn key_f4_opens_dj_view() {
        let mut s = TuiState::new();
        assert_eq!(s.handle_key(key(KeyCode::F(4))), Some(Intent::ShowScreen(Screen::Dj)));
        assert_eq!(s.screen, Screen::Dj);
    }

    #[test]
    fn dj_input_builds_and_submits_as_cc() {
        let mut s = TuiState::new();
        s.handle_key(key(KeyCode::F(4)));
        assert_eq!(s.screen, Screen::Dj);
        // Printable chars build the ask> line (never shadowed by nav/verb keys like
        // `p`/`j`/`q`, which would be transport/nav on other screens).
        for c in "pause the 3rd".chars() {
            assert_eq!(s.handle_key(ch(c)), None);
        }
        assert_eq!(s.dj_input, "pause the 3rd");
        // Backspace edits.
        s.handle_key(key(KeyCode::Backspace));
        assert_eq!(s.dj_input, "pause the 3r");
        // Enter submits the whole line as a CC translation (always NL), logs the
        // query, sets the thinking phase, and clears the input.
        s.dj_input = "fade out".into();
        assert_eq!(s.handle_key(key(KeyCode::Enter)), Some(Intent::Cc("fade out".into())));
        assert_eq!(s.dj_input, "");
        assert_eq!(s.dj_phase.as_deref(), Some("thinking..."));
        assert!(s.dj_log.iter().any(|l| l == "> fade out"));
        // A blank Enter is a no-op (no spurious CC call).
        let mut s2 = TuiState::new();
        s2.handle_key(key(KeyCode::F(4)));
        assert_eq!(s2.handle_key(key(KeyCode::Enter)), None);
    }

    #[test]
    fn dj_bare_favorite_phrase_stars_current_track() {
        // A bare-favorite phrase typed in the DJ view routes through the SAME
        // route() the ':' line uses, so it stars the current track instead of
        // falling to the CC translator (which has no favorite capability).
        let mut s = TuiState::new();
        s.now.file = Some("song/7".into());
        s.handle_key(key(KeyCode::F(4)));
        s.dj_input = "favorite this song".into();
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            Some(Intent::Command("playlistadd Starred song/7".into()))
        );
        // No spurious CC thinking phase on the favorite path.
        assert_eq!(s.dj_phase, None);
        assert_eq!(s.dj_input, "");
    }

    #[test]
    fn dj_bare_queue_verb_routes_to_command_not_cc() {
        // A bare control verb typed in the DJ pane must run the DETERMINISTIC verb
        // path (never Claude, which cannot express clear/next and would no-op).
        let mut s = TuiState::new();
        s.handle_key(key(KeyCode::F(4)));
        s.dj_input = "next".into();
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            Some(Intent::Command("next".into()))
        );
        // No CC thinking phase on the verb path.
        assert_eq!(s.dj_phase, None);
        // Feedback is surfaced in the DJ pane scrollback.
        assert!(s.dj_log.iter().any(|l| l == "ok: next"));

        // `clear` opens the destructive default-No confirm, NOT a silent run.
        let mut s = TuiState::new();
        s.handle_key(key(KeyCode::F(4)));
        s.dj_input = "clear".into();
        assert_eq!(s.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(s.mode, Mode::Confirm);
        assert_eq!(s.dj_phase, None);

        // A fuzzy phrase still goes to CC (the translator path).
        let mut s = TuiState::new();
        s.handle_key(key(KeyCode::F(4)));
        s.dj_input = "fade out slowly".into();
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            Some(Intent::Cc("fade out slowly".into()))
        );
        assert_eq!(s.dj_phase.as_deref(), Some("thinking..."));
    }

    #[test]
    fn dj_esc_returns_to_queue() {
        let mut s = TuiState::new();
        s.handle_key(key(KeyCode::F(4)));
        s.dj_input = "half typed".into();
        assert_eq!(s.handle_key(key(KeyCode::Esc)), Some(Intent::ShowScreen(Screen::Queue)));
        assert_eq!(s.screen, Screen::Queue);
        assert_eq!(s.dj_input, "");
    }

    #[test]
    fn dj_log_folds_and_bounds() {
        let mut s = TuiState::new();
        for i in 0..250 {
            s.push_dj_log(format!("line {i}"));
        }
        // Bounded at 200, newest kept.
        assert_eq!(s.dj_log.len(), 200);
        assert_eq!(s.dj_log.last().unwrap(), "line 249");
        assert_eq!(s.dj_log.first().unwrap(), "line 50");
    }

    #[test]
    fn normalize_level_maps_floor_ceiling_and_gamma() {
        // At/below the floor -> 0; at/above the ceiling -> 1.
        assert_eq!(normalize_level(-54.0), 0.0);
        assert_eq!(normalize_level(-90.0), 0.0);
        assert!((normalize_level(-6.0) - 1.0).abs() < 1e-6);
        assert!((normalize_level(0.0) - 1.0).abs() < 1e-6);
        // Midpoint sits above the linear 0.5 because of the <1 gamma (quiet expand).
        let mid = normalize_level(-30.0);
        assert!(mid > 0.5 && mid < 1.0, "gamma lifts the mid: {mid}");
        // Monotone increasing.
        assert!(normalize_level(-40.0) < normalize_level(-20.0));
    }

    #[test]
    fn envelope_attack_faster_than_release() {
        // From rest, a step UP rises quickly; a step DOWN of the same size falls
        // slower - the asymmetric ballistics (attack 60ms vs release 350ms).
        let dt = 0.05; // one ~20fps frame
        let up = envelope_step(0.0, 1.0, dt);
        let down = 1.0 - envelope_step(1.0, 0.0, dt);
        assert!(up > down, "attack ({up}) outpaces release ({down})");
        // A zero dt makes no move; the envelope converges toward the target over
        // repeated steps and never overshoots.
        assert_eq!(envelope_step(0.3, 0.9, 0.0), 0.3);
        let mut a = 0.0f32;
        for _ in 0..200 {
            a = envelope_step(a, 1.0, dt);
        }
        assert!(a > 0.99 && a <= 1.0, "converges to the target without overshoot: {a}");
    }

    #[test]
    fn disconnect_clears_pending_reconnect_banner() {
        let mut s = TuiState::new();
        s.enter_confirm(Pending { token: Some("nl-1".into()), ..Default::default() });
        s.mark_disconnected();
        assert!(!s.connected);
        assert!(s.pending.is_none());
        assert_eq!(s.mode, Mode::Normal);
        assert!(s.status_msg.as_ref().unwrap().contains("reconnecting"));
        s.mark_connected();
        assert!(s.connected);
        assert!(s.status_msg.as_ref().unwrap().contains("re-run"));
    }
    // The row context menu.

    /// A Find hit row, with only the fields a menu fixture actually states.
    fn hit_row(
        kind: crate::find::FindKind,
        label: &str,
        uri: &str,
        artist: Option<&str>,
    ) -> crate::find::FindRow {
        crate::find::FindRow {
            kind,
            label: label.into(),
            uri: uri.into(),
            trailer: String::new(),
            song_count: None,
            album_uri: None,
            artist: artist.map(str::to_string),
        }
    }

    #[test]
    fn cursor_target_names_the_row_the_eye_is_on_for_every_screen() {
        use crate::find::{FindKind, Focus};
        use crate::menu::{Origin, TargetKind};
        // The trap this resolver exists to avoid: a POPULATED queue under every other
        // screen. A per-screen match that forgets to claim the Find hits (or the
        // playlists) falls through to `self.queue[self.selected]` and describes a row
        // the user is not even looking at.
        let fresh = || {
            let mut s = TuiState::new();
            let mut it = item(0);
            it.album_uri = Some("album/queue".into());
            it.artist = Some("Queue Artist".into());
            s.apply_snapshot(NowPlaying::default(), vec![it, item(1)]);
            s.albums.rows = vec![brow("Volume Alpha", "album/9", true)];
            s.playlists.rows = vec![brow("Starred", "Starred", false)];
            s.find.hits.rows = vec![hit_row(FindKind::Album, "Hit", "album/hit", Some("C418"))];
            s.find.drill.rows = vec![BrowseRow {
                label: "Sweden - C418".into(),
                uri: "song/drill".into(),
                is_dir: false,
                song_count: None,
                artist: Some("C418".into()),
                album_uri: Some("album/drill".into()),
            }];
            s
        };

        let s = fresh();
        let t = s.cursor_target().expect("the queue row");
        assert_eq!(t.origin, Origin::Queue { pos: 0 });
        assert_eq!(t.kind, TargetKind::LibrarySong);
        assert_eq!(t.uri.as_deref(), Some("song/0"));
        assert_eq!(t.album_uri.as_deref(), Some("album/queue"));
        assert_eq!(t.artist.as_deref(), Some("Queue Artist"));

        let mut s = fresh();
        s.screen = Screen::Albums;
        let t = s.cursor_target().expect("the album row");
        assert_eq!((t.kind, t.origin), (TargetKind::Album, Origin::Browse));
        assert_eq!(t.uri.as_deref(), Some("album/9"));

        // A playlist row is a NAME; no uri prefix can reveal that, so the SCREEN says it.
        let mut s = fresh();
        s.screen = Screen::Playlists;
        let t = s.cursor_target().expect("the playlist row");
        assert_eq!(t.kind, TargetKind::Playlist);
        assert_eq!(t.uri.as_deref(), Some("Starred"));

        // Off-drill, Find shows the HITS - which are not a `Browse`, so a resolver that
        // consulted active_browse() first would silently return the QUEUE row here.
        let mut s = fresh();
        s.screen = Screen::Find;
        s.find.focus = Focus::Results;
        let t = s.cursor_target().expect("the hit row");
        assert_eq!((t.kind, t.origin), (TargetKind::Album, Origin::FindHit));
        assert_eq!(t.uri.as_deref(), Some("album/hit"));
        assert_eq!(t.artist.as_deref(), Some("C418"));

        // Drilling, the same screen shows the drill rows instead.
        s.find.drilling = true;
        let t = s.cursor_target().expect("the drilled row");
        assert_eq!((t.kind, t.origin), (TargetKind::LibrarySong, Origin::Browse));
        assert_eq!(t.uri.as_deref(), Some("song/drill"));
        assert_eq!(t.album_uri.as_deref(), Some("album/drill"));
        assert_eq!(t.artist.as_deref(), Some("C418"));

        // An empty list has no row to describe - never a popup over nothing.
        let mut s = TuiState::new();
        assert!(s.cursor_target().is_none(), "an empty queue has no target");
        s.screen = Screen::Albums;
        assert!(s.cursor_target().is_none(), "an empty browse has no target");
        s.screen = Screen::Find;
        assert!(s.cursor_target().is_none(), "an empty hit list has no target");
        // The DJ ask line owns every printable key, so `o` never reaches dispatch there.
        s.screen = Screen::Dj;
        assert!(s.cursor_target().is_none());
    }

    #[test]
    fn a_queue_target_carries_the_mpd_pos_not_the_row_index() {
        // `play` / `delete` address by POSITION, and the two only coincide while the
        // queue is dense. A resolver that handed the row index over would delete the
        // wrong track on any sparse listing.
        let mut s = TuiState::new();
        let mut a = item(0);
        a.pos = 7;
        let mut b = item(1);
        b.pos = 11;
        s.apply_snapshot(NowPlaying::default(), vec![a, b]);
        s.selected = 1;
        assert_eq!(
            s.cursor_target().unwrap().origin,
            crate::menu::Origin::Queue { pos: 11 }
        );
    }

    #[test]
    fn o_opens_the_menu_on_every_screen_and_esc_closes_it() {
        use crate::find::{FindKind, Focus};
        let screens = [Screen::Queue, Screen::Albums, Screen::Playlists, Screen::Find];
        for screen in screens {
            let mut s = TuiState::new();
            s.apply_snapshot(NowPlaying::default(), vec![item(0)]);
            s.albums.rows = vec![brow("Volume Alpha", "album/9", true)];
            s.playlists.rows = vec![brow("Starred", "Starred", false)];
            s.find.hits.rows = vec![hit_row(FindKind::Song, "Sweden", "song/1", None)];
            s.find.focus = Focus::Results;
            s.screen = screen;
            assert_eq!(s.handle_key(ch('o')), None, "{screen:?}: opening emits no intent");
            let menu = s.menu.as_ref().unwrap_or_else(|| panic!("{screen:?} has no menu"));
            assert!(!menu.rows.is_empty(), "{screen:?}: an empty menu is a dead popup");
            // The cursor parks on the first LIVE row, so Enter is never a refusal.
            assert!(
                matches!(menu.rows[menu.selected].avail, crate::menu::Avail::Yes(_)),
                "{screen:?}: the preselected row is blocked"
            );
            assert_eq!(s.handle_key(key(KeyCode::Esc)), None);
            assert!(s.menu.is_none(), "{screen:?}: Esc closes the menu");
        }
    }

    #[test]
    fn o_on_an_empty_list_says_so_instead_of_flashing_an_empty_popup() {
        let mut s = TuiState::new();
        assert_eq!(s.handle_key(ch('o')), None);
        assert!(s.menu.is_none());
        assert_eq!(s.status_msg.as_deref(), Some("nothing here"));
    }

    #[test]
    fn shift_o_opens_the_playing_track_and_says_so_when_nothing_is() {
        use crate::menu::{Origin, TargetKind};
        // Nothing playing: a reason, never a silently dead key.
        let mut s = TuiState::new();
        assert_eq!(s.handle_key(ch('O')), None);
        assert!(s.menu.is_none());
        assert_eq!(s.status_msg.as_deref(), Some("nothing is playing"));

        // A library song, reached from a screen whose cursor is somewhere else entirely
        // - which is the whole point of `O`.
        let now = NowPlaying {
            file: Some("song/5".into()),
            title: Some("Sweden".into()),
            album_uri: Some("album/7".into()),
            artist: Some("C418".into()),
            ..NowPlaying::default()
        };
        s.apply_snapshot(now, vec![item(0)]);
        s.screen = Screen::Albums;
        s.albums.rows = vec![brow("Elsewhere", "album/99", true)];
        assert_eq!(s.handle_key(ch('O')), None);
        let m = s.menu.as_ref().expect("the playing track has a menu");
        assert_eq!(m.target.origin, Origin::NowPlaying);
        assert_eq!(m.target.kind, TargetKind::LibrarySong);
        assert_eq!(m.target.label, "Sweden");
        assert_eq!(m.target.album_uri.as_deref(), Some("album/7"));
    }

    #[test]
    fn shift_o_on_a_recognized_stream_keeps_the_library_actions() {
        use crate::menu::{MenuAction, TargetKind};
        let mut s = TuiState::new();
        let now = NowPlaying {
            file: Some("https://stream-relay.ntslive.net/1".into()),
            name: Some("NTS 1".into()),
            match_uri: Some("song/42".into()),
            ..NowPlaying::default()
        };
        s.apply_now(now);
        s.handle_key(ch('O'));
        let m = s.menu.as_ref().unwrap();
        assert_eq!(m.target.kind, TargetKind::Stream);
        assert_eq!(m.target.label, "NTS 1", "the station name, not the bare url");
        // The star and the radio seed both come off the MATCHED track, exactly as C-s
        // already does - the stream url stays the target's own uri.
        assert!(m.rows.iter().any(|r| r.avail
            == crate::menu::Avail::Yes(MenuAction::Favorite("song/42".into()))));
        assert!(m.rows.iter().any(|r| r.avail
            == crate::menu::Avail::Yes(MenuAction::Radio("song/42".into()))));
    }

    #[test]
    fn the_open_menu_swallows_every_key_it_does_not_claim() {
        // A true modal: a transport key must not act on something the popup does not
        // even name, and a nav key must not move the cursor out from under a snapshot.
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(0), item(1), item(2)]);
        s.handle_key(ch('o'));
        assert!(s.menu.is_some());
        assert_eq!(s.handle_key(ch('>')), None, "next track is swallowed");
        assert_eq!(s.handle_key(ch(':')), None, "the command line is swallowed");
        assert_eq!(s.handle_key(ch('s')), Some(cmd("playlistadd Starred song/0")),
            "a hotkey runs its OWN row - `s` is the menu's star, not the global one");
        s.handle_key(ch('o'));
        assert_eq!(s.mode, Mode::Normal, "and no mode change leaked through");
        assert_eq!(s.selected, 0, "the queue cursor never moved");
        assert!(s.menu.is_some(), "and the menu is still open");
        // j/k move the POPUP, not the list underneath.
        let before = s.menu.as_ref().unwrap().selected;
        s.handle_key(ch('j'));
        assert_eq!(s.menu.as_ref().unwrap().selected, before + 1);
        assert_eq!(s.selected, 0);
        s.handle_key(ch('G'));
        let last = s.menu.as_ref().unwrap().rows.len() - 1;
        assert_eq!(s.menu.as_ref().unwrap().selected, last);
        s.handle_key(ch('g'));
        assert_eq!(s.menu.as_ref().unwrap().selected, 0);
        // `p` is the menu's OWN "play now" hotkey, so it jumps to the row the popup
        // names - the global pause never sees it, which is the property that matters.
        assert_eq!(s.handle_key(ch('p')), Some(cmd("play 0")));
        assert!(s.menu.is_none());
    }

    #[test]
    fn the_menu_and_the_help_overlay_are_never_both_open() {
        // Mutually exclusive BY CONSTRUCTION rather than by an ordering the two
        // intercepts would each have to remember.
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(0)]);
        s.handle_key(ch('?'));
        assert!(s.help_open);
        // With help open, `o` is swallowed by the help modal (it is the inner one).
        s.handle_key(ch('o'));
        assert!(s.menu.is_none(), "the help modal swallows it");
        s.handle_key(key(KeyCode::Esc));
        s.handle_key(ch('o'));
        assert!(s.menu.is_some());
        s.help_open = true;
        s.open_menu(s.cursor_target().unwrap());
        assert!(!s.help_open, "opening the menu closes help");
    }

    #[test]
    fn go_to_album_reveals_the_album_in_the_find_drill() {
        // The one pane that can show an arbitrary path without clobbering another
        // screen's cursor or nav stack. `drilling` stays false so main.rs takes the
        // Find-ENTRY branch of browse_into (depth 0, empty stack, free back-out).
        let mut s = TuiState::new();
        let mut it = item(0);
        it.album_uri = Some("album/7".into());
        s.apply_snapshot(NowPlaying::default(), vec![it]);
        s.handle_key(ch('o'));
        assert_eq!(s.handle_key(ch('b')), Some(Intent::BrowseInto("album/7".into())));
        assert_eq!(s.screen, Screen::Find);
        assert!(!s.find.drilling);
        assert_eq!(s.find.focus, crate::find::Focus::Results);
        assert!(s.menu.is_none(), "a run row closes the menu");
        assert_eq!(s.status_msg.as_deref(), Some("showing t0"));
    }

    #[test]
    fn go_to_artist_runs_a_real_library_query_and_marks_it_in_flight() {
        // There is no artist uri on the wire yet, so this is a SEARCH - and the row
        // says so. It must go through the same submit path Enter on the query line
        // uses, or the two would drift on history / phase / the echoed query.
        let mut s = TuiState::new();
        let mut it = item(0);
        it.artist = Some("Alice Coltrane".into());
        s.apply_snapshot(NowPlaying::default(), vec![it]);
        s.handle_key(ch('o'));
        let row = s
            .menu
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .find(|r| r.item == crate::menu::MenuItem::GoToArtist)
            .unwrap();
        assert_eq!(row.label, "go to artist (search)", "honest about being a search");
        assert_eq!(s.handle_key(ch('t')), Some(Intent::Find("Alice Coltrane".into())));
        assert_eq!(s.screen, Screen::Find);
        assert_eq!(s.find.query, "Alice Coltrane", "the screen says what it asked");
        assert_eq!(s.find.submitted, "Alice Coltrane");
        assert_eq!(
            s.find.phase,
            crate::find::Phase::Loading("Alice Coltrane".into()),
            "the spinner turns while it is in flight"
        );
        assert_eq!(s.find.history.first().map(String::as_str), Some("Alice Coltrane"));
    }

    #[test]
    fn a_blocked_pick_says_why_and_leaves_the_menu_open() {
        // A refusal is information, and the next row is one keystroke away - so the
        // popup must NOT close under the user for a key that did nothing.
        let mut s = TuiState::new();
        // A queue song with no album uri: the family applies, this listing lacks it.
        s.apply_snapshot(NowPlaying::default(), vec![item(0)]);
        s.handle_key(ch('o'));
        assert_eq!(s.handle_key(ch('b')), None);
        assert_eq!(s.status_msg.as_deref(), Some("this listing carries no album uri"));
        assert!(s.menu.is_some(), "the menu stays open on a refusal");
        // And picking a live row from the still-open menu works.
        assert_eq!(s.handle_key(ch('p')), Some(cmd("play 0")));
        assert!(s.menu.is_none());
    }

    #[test]
    fn a_letter_the_menu_does_not_list_is_swallowed_not_leaked() {
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(0)]);
        s.handle_key(ch('o'));
        // `a` (add to queue) is absent on a queue row - it is already queued.
        assert_eq!(s.handle_key(ch('a')), None);
        assert!(s.menu.is_some());
        assert_eq!(s.status_msg, None, "an unlisted key means nothing here, silently");
    }

    #[test]
    fn enter_runs_the_highlighted_row_and_l_is_the_same_gesture() {
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(0), item(1)]);
        s.selected = 1;
        s.handle_key(ch('o'));
        assert_eq!(s.handle_key(key(KeyCode::Enter)), Some(cmd("play 1")));
        s.handle_key(ch('o'));
        assert_eq!(s.handle_key(ch('l')), Some(cmd("play 1")), "l runs it too");
    }

    #[test]
    fn a_same_length_reorder_under_the_popup_refuses_rather_than_acting() {
        // The event loop closes the menu when the queue LENGTH changes; this is the
        // reorder that length alone cannot see, so dispatch re-checks the uri at the
        // snapshot pos before playing or deleting.
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(0), item(1)]);
        s.selected = 1;
        s.handle_key(ch('o'));
        // The rows swap places, keeping both the length and the positions.
        let mut a = item(1);
        a.pos = 0;
        let mut b = item(0);
        b.pos = 1;
        s.apply_snapshot(NowPlaying::default(), vec![a, b]);
        assert_eq!(s.handle_key(ch('x')), None, "remove refuses on a moved row");
        assert_eq!(s.status_msg.as_deref(), Some("the queue moved - reopen the menu"));
        // And the same guard covers the jump.
        s.handle_key(ch('o'));
        s.selected = 1;
        assert_eq!(s.handle_key(ch('p')), Some(cmd("play 1")), "an unmoved row still runs");
    }

    #[test]
    fn remove_from_queue_deletes_the_snapshot_position() {
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(0), item(1), item(2)]);
        s.selected = 2;
        s.handle_key(ch('o'));
        assert_eq!(s.handle_key(ch('x')), Some(cmd("delete 2")));
        assert!(s.menu.is_none());
    }

    #[test]
    fn the_menu_radio_row_shares_the_bare_r_keys_gate_and_its_reasons() {
        use crate::find::{FindKind, Focus};
        // One gate, one set of reason strings: the popup row and `r` cannot drift.
        let mut s = TuiState::new();
        s.screen = Screen::Find;
        s.find.focus = Focus::Results;
        s.find.hits.rows = vec![hit_row(FindKind::Album, "Volume Alpha", "album/9", None)];
        s.handle_key(ch('o'));
        assert_eq!(s.handle_key(ch('r')), Some(cmd("radio album/9")));
        assert_eq!(s.status_msg.as_deref(), Some("radio from Volume Alpha"));
    }

    #[test]
    fn a_playlist_row_loads_from_the_menu_exactly_as_enter_does() {
        let mut s = TuiState::new();
        s.screen = Screen::Playlists;
        s.playlists.rows = vec![brow("Starred", "Starred", false)];
        s.handle_key(ch('o'));
        // Row 0 is the blocked "open contents"; the cursor parks on the load below it.
        assert_eq!(s.handle_key(key(KeyCode::Enter)), Some(Intent::LoadPlaylist("Starred".into())));
    }

    #[test]
    fn an_album_row_opens_its_contents_from_the_menu_just_as_l_drills_it() {
        let mut s = TuiState::new();
        s.screen = Screen::Albums;
        s.albums.rows = vec![brow("Volume Alpha", "album/9", true)];
        s.handle_key(ch('o'));
        // OpenContents is row 0 and preselected, so `o` then Enter still drills.
        assert_eq!(
            s.handle_key(key(KeyCode::Enter)),
            Some(Intent::BrowseInto("album/9".into()))
        );
    }

    #[test]
    fn ctrl_n_and_ctrl_p_move_the_menu_cursor() {
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(0)]);
        s.handle_key(ch('o'));
        s.handle_key(ctrl('n'));
        assert_eq!(s.menu.as_ref().unwrap().selected, 1);
        s.handle_key(ctrl('p'));
        assert_eq!(s.menu.as_ref().unwrap().selected, 0);
        assert_eq!(s.selected, 0, "and never the queue underneath");
    }

    #[test]
    fn parse_browse_keeps_the_raw_artist_and_the_album_uri_off_a_song_row() {
        // Both are pairs `lsinfo` ALREADY carries; the client simply never read them.
        let pairs: Vec<(String, String)> = vec![
            ("file".into(), "song/9".into()),
            ("Title".into(), "Sweden".into()),
            ("Artist".into(), "C418".into()),
            ("X-AlbumUri".into(), "album/7".into()),
            // A dir row must take neither: the pair means something else there.
            ("directory".into(), "album/1".into()),
            ("Album".into(), "Volume Alpha".into()),
        ];
        let rows = parse_browse(&pairs);
        assert_eq!(rows[0].label, "Sweden - C418", "the eye still reads the credit");
        assert_eq!(rows[0].artist.as_deref(), Some("C418"), "and the query gets it raw");
        assert_eq!(rows[0].album_uri.as_deref(), Some("album/7"));
        assert_eq!(rows[1].artist, None);
        assert_eq!(rows[1].album_uri, None);
    }

    #[test]
    fn a_menu_jump_to_find_never_leaves_the_previous_querys_hits_actionable() {
        use crate::find::{FindKind, Focus};
        // THE hazard of landing the cursor on the results half: `find.hits` and
        // `find.selected` are only replaced when the response lands, so for the whole
        // round trip the PREVIOUS query's rows are the visible AND actionable list,
        // with the old cursor index intact. Enter would enqueue and PLAY an unrelated
        // track over what is playing, and `s` would write a star into the user's
        // library. The query line does not have this hole because it keeps focus on the
        // text, where those keys are captured as input.
        let stale = |s: &mut TuiState| {
            s.find.hits.rows = vec![
                hit_row(FindKind::Song, "Sweden", "song/9", None),
                hit_row(FindKind::Song, "Wet Hands", "song/1", None),
            ];
            s.find.selected = 1;
        };
        let queue_row = || {
            let mut it = item(0);
            it.artist = Some("Alice Coltrane".into());
            it.album_uri = Some("album/7".into());
            it
        };

        // "go to artist (search)": the window is the whole searchall round trip.
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![queue_row()]);
        stale(&mut s);
        s.handle_key(ch('o'));
        assert_eq!(s.handle_key(ch('t')), Some(Intent::Find("Alice Coltrane".into())));
        assert_eq!(s.find.focus, Focus::Results);
        assert!(s.find.hits.rows.is_empty(), "the rows go with the question");
        assert_eq!(s.find.selected, 0);
        assert_eq!(s.handle_key(key(KeyCode::Enter)), None, "nothing to play but the old answer");
        assert_eq!(s.handle_key(ch('s')), None, "and nothing to star into the library");

        // "go to album" from a queue row: the window is one key-drain frame, because
        // main.rs drains every pending key before dispatch runs `browse_into` (which is
        // what sets `drilling`). Same rows, same keys, same wrong track.
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![queue_row()]);
        stale(&mut s);
        s.handle_key(ch('o'));
        assert_eq!(s.handle_key(ch('b')), Some(Intent::BrowseInto("album/7".into())));
        assert!(!s.find.drilling, "main.rs still takes the Find-ENTRY branch");
        assert!(s.find.hits.rows.is_empty());
        assert_eq!(s.handle_key(key(KeyCode::Enter)), None);

        // But revealing FROM a hit keeps them: that is the one case where backing out
        // to the hits means something, and a key that beats the drill in still acts on
        // the row the popup itself named - not on a stranger.
        let mut s = TuiState::new();
        s.screen = Screen::Find;
        s.find.focus = Focus::Results;
        s.find.hits.rows = vec![crate::find::FindRow {
            album_uri: Some("album/7".into()),
            ..hit_row(FindKind::Song, "Sweden", "song/9", None)
        }];
        s.handle_key(ch('o'));
        assert_eq!(s.handle_key(ch('b')), Some(Intent::BrowseInto("album/7".into())));
        assert_eq!(s.find.hits.rows.len(), 1, "backing out of the drill is still free");
    }

    #[test]
    fn a_confirm_arriving_over_the_menu_closes_it_instead_of_stranding_it() {
        // A plan lands ASYNCHRONOUSLY, so it can arrive while the popup is open (the
        // user submitted a phrase, then went on browsing). `Mode::Confirm` routes keys
        // to `key_confirm`, and the menu's modal intercept lives in `key_normal` - so a
        // menu left standing is drawn over the y/N prompt with every one of its keys
        // dead, and the first Esc silently cancels the plan the user asked for while the
        // popup sits there unchanged.
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(0)]);
        s.handle_key(ch('o'));
        assert!(s.menu.is_some());
        s.enter_confirm(Pending { steps: vec!["[1] fade out".into()], ..Default::default() });
        assert!(s.menu.is_none(), "the confirm takes the screen");
        assert_eq!(s.mode, Mode::Confirm);
        // And the prompt answers for itself, on the FIRST press - not after an Esc
        // spent closing a popup the user could not act on anyway.
        assert_eq!(s.handle_key(key(KeyCode::Esc)), Some(Intent::ConfirmCancel));
    }

    #[test]
    fn at_most_one_overlay_is_ever_live_whatever_lands_over_whatever() {
        // The pairwise version of this rule kept shipping incomplete: the menu axis was
        // closed while a confirm landing over help or over the heard panel still left a
        // full-frame overlay with every key dead, because `handle_key` dispatches on
        // MODE first and `Mode::Confirm` never reaches an overlay intercept. This asserts
        // the invariant itself over the whole cross product, so the next overlay someone
        // adds cannot quietly reintroduce one arm of it.
        fn live(s: &TuiState) -> Vec<&'static str> {
            let mut v = Vec::new();
            if s.menu.is_some() { v.push("menu"); }
            if s.help_open { v.push("help"); }
            if s.heard_open { v.push("heard"); }
            if s.mode == Mode::Confirm { v.push("confirm"); }
            v
        }
        let openers: Vec<(&str, fn(&mut TuiState))> = vec![
            ("menu", |s: &mut TuiState| {
                let t = s.cursor_target().expect("a queue row resolves");
                s.open_menu(t);
            }),
            ("help", |s: &mut TuiState| { s.apply_act(keymap::Act::HelpToggle); }),
            ("heard", |s: &mut TuiState| s.open_heard(vec!["12:04  a mark".to_string()])),
            ("confirm", |s: &mut TuiState| {
                s.enter_confirm(Pending { steps: vec!["[1] fade out".into()], ..Default::default() })
            }),
        ];
        // Every ORDERED pair and triple: whatever is already up, the next thing to land
        // must take the screen alone.
        for (na, a) in &openers {
            for (nb, b) in &openers {
                for (nc, c) in &openers {
                    let mut s = TuiState::new();
                    s.apply_snapshot(NowPlaying::default(), vec![item(0)]);
                    let mut last = "";
                    for (name, open) in [(na, a), (nb, b), (nc, c)] {
                        // The three OVERLAYS are opened from normal-mode dispatch, which
                        // `Mode::Confirm` never reaches - so an overlay landing on a live
                        // confirm is not a state the app can be driven into, and forcing
                        // it here would be testing fiction. The confirm itself DOES land
                        // asynchronously over anything, which is the arm that shipped
                        // broken, so it is never skipped.
                        if s.mode == Mode::Confirm && *name != "confirm" {
                            continue;
                        }
                        open(&mut s);
                        last = name;
                        let l = live(&s);
                        assert_eq!(
                            l.len(),
                            1,
                            "{na} then {nb} then {nc}: after {name} the live set was {l:?}; \
                             exactly one thing must own the screen"
                        );
                        assert_eq!(l[0], *name, "the LAST thing to land owns the screen");
                    }
                    assert!(!last.is_empty(), "every sequence lands at least one overlay");
                }
            }
        }
    }

    #[test]
    fn the_menu_and_the_heard_panel_are_never_open_at_the_same_time() {
        // The same async shape as the confirm above, and the reason `open_menu` closes
        // help: the heard reply LANDS after `t` is pressed, so the menu can be opened in
        // the window between. `key_normal` intercepts the menu ABOVE the heard panel, so
        // both open means the panel is painted over by a modal that swallows the j/k/q
        // the panel needs - a ledger the user asked for, unreadable and unscrollable.
        let mut s = TuiState::new();
        s.apply_snapshot(NowPlaying::default(), vec![item(0)]);
        // Reply lands over an open menu: the panel takes the screen.
        s.handle_key(ch('o'));
        assert!(s.menu.is_some());
        s.open_heard(vec!["12:04  Future Proof  taped 0:30".to_string()]);
        assert!(s.heard_open, "the panel opens");
        assert!(s.menu.is_none(), "and the menu does not survive under it");
        // And the other direction: the panel is up, so `o` must not leave it behind.
        // (The panel swallows `o` while it is open, so drive `open_menu` directly - the
        // guarantee belongs to the opener, not to one key's intercept order.)
        s.heard_open = true;
        let target = s.cursor_target().expect("a queue row resolves");
        s.open_menu(target);
        assert!(s.menu.is_some());
        assert!(!s.heard_open, "the panel closes rather than sitting under the menu");
    }

    #[test]
    fn a_blank_artist_pair_reads_as_absent_not_as_an_empty_query() {
        // An empty `Artist:` is on the wire as often as a missing one. Left live, the
        // row would submit nothing and close the menu with no feedback at all.
        let mut s = TuiState::new();
        let mut it = item(0);
        it.artist = Some("   ".into());
        s.apply_snapshot(NowPlaying::default(), vec![it]);
        assert_eq!(s.cursor_target().unwrap().artist, None);
        s.handle_key(ch('o'));
        assert_eq!(s.handle_key(ch('t')), None);
        assert_eq!(s.status_msg.as_deref(), Some("this listing carries no artist"));
        assert!(s.menu.is_some(), "and it is a stated refusal, not a silent close");
    }
}

