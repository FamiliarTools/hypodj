//! The SINGLE source-of-truth keymap. [`KEYMAP`] lists every normal-mode binding
//! once: its matcher(s), a display label, a group, a scope, the action it fires, and a
//! one-line help string. The `?` help overlay renders straight from [`grouped`], so
//! it can NEVER drift from the real bindings; [`match_key`] resolves a KeyEvent to its
//! [`Act`] so dispatch can stay in lockstep with the same table.
//!
//! NOTE on lockstep: state.rs `key_normal` DISPATCHES through this table - it calls
//! [`match_key`] to resolve a key to its [`Act`], then runs it through an EXHAUSTIVE
//! `apply_act` match. So a new row (new Act) cannot be added without a compiler error
//! until dispatch handles it, and an Act cannot be dropped from dispatch while a row
//! still advertises it: help (rendered from [`grouped`]) and behavior share one source
//! and cannot drift. The round-trip test additionally guards against matcher typos.

// The scope field + `keys`/`group`/`help` are read by the help overlay and the scope
// gating; some are not consulted by the (minimum) dispatch, so allow dead_code.
#![allow(dead_code)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::state::Screen;

/// A logical action a normal-mode key maps to. Mirrors the intents the state machine
/// already produces; the help overlay only ever needs the label + description, but the
/// Act is what makes the round-trip test meaningful.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    ScreenQueue,
    ScreenAlbums,
    ScreenPlaylists,
    ScreenDj,
    ScreenFind,
    /// Cycle the body view forward / back (Tab / Shift-Tab), the Deck's way between
    /// views; F1-F5 stay as aliases for one release.
    ViewNext,
    ViewPrev,
    SectionNext,
    SectionPrev,
    Down,
    Up,
    Top,
    Bottom,
    JumpCurrent,
    SearchStart,
    SearchNext,
    SearchPrev,
    CommandLine,
    VolumeUp,
    VolumeDown,
    Pause,
    Next,
    Prev,
    ScrubFwd,
    ScrubBack,
    Radio,
    /// Flip one playback mode, resolved against the last snapshot's flag.
    ToggleRandom,
    ToggleRepeat,
    ToggleSingle,
    ToggleConsume,
    FavSelected,
    FavCurrent,
    Heard,
    /// Open the info card on the selected row. A pure CLIENT act: it renders pairs the
    /// row already carries, so it reaches no socket and cannot fail.
    Info,
    /// Retract the newest thing the daemon can still put back.
    Undo,
    /// Show the undo ring: what ran, in the daemon's own words.
    Journal,
    PlaySel,
    Enqueue,
    /// Remove the selected QUEUE row. Removing the row you are HEARING is a transport
    /// gesture on the daemon side (it dips, loads the next track, and drops the entry
    /// at the same instant - or ramps to silence and stops when nothing follows), so
    /// this key never hard-cuts the audio.
    RemoveSel,
    /// Open the row CONTEXT MENU for the thing under the cursor. Renamed from `Open`
    /// when `o` stopped meaning "drill": drilling moved to its own key so `o` can be
    /// unconditional on every screen and every row kind, and an album row can reach its
    /// own menu (which is where "go to artist" lives).
    Menu,
    /// Open the context menu for the NOW PLAYING track, screen-independent.
    MenuCurrent,
    /// Drill into a directory - the body `o` used to run, on its own key so the vim
    /// symmetry with the existing `h` / Left / Esc back-out is restored.
    BrowseIn,
    BrowseBack,
    HelpToggle,
    Quit,
}

/// One way a binding can match a key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMatch {
    Char(char),
    Ctrl(char),
    Code(KeyCode),
}

impl KeyMatch {
    fn matches(&self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match *self {
            KeyMatch::Char(c) => !ctrl && key.code == KeyCode::Char(c),
            KeyMatch::Ctrl(c) => ctrl && key.code == KeyCode::Char(c),
            KeyMatch::Code(code) => key.code == code,
        }
    }
}

/// The help-overlay group a binding belongs to. Stable order defines the two-column
/// layout order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    View,
    Navigation,
    Playback,
    Volume,
    Search,
    Command,
    Browse,
    Favorites,
    General,
}

impl Group {
    pub fn title(&self) -> &'static str {
        match self {
            Group::View => "View",
            Group::Navigation => "Navigation",
            Group::Playback => "Playback",
            Group::Volume => "Volume",
            Group::Search => "Search",
            Group::Command => "Command",
            Group::Browse => "Browse",
            Group::Favorites => "Favorites",
            Group::General => "General",
        }
    }
}

/// The scope a binding is meaningful in (used for the help label and, in principle,
/// match gating; the dispatch bodies already guard screen-specific no-ops themselves).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Queue,
    Browse,
}

/// One keymap row.
pub struct Binding {
    pub matchers: &'static [KeyMatch],
    pub keys: &'static str,
    pub group: Group,
    pub scope: Scope,
    pub act: Act,
    pub help: &'static str,
}

/// The stable group order for the overlay.
pub const GROUP_ORDER: [Group; 9] = [
    Group::View,
    Group::Navigation,
    Group::Playback,
    Group::Volume,
    Group::Search,
    Group::Command,
    Group::Browse,
    Group::Favorites,
    Group::General,
];

use Act::*;
use Group::*;
use KeyMatch::{Char, Code, Ctrl};

/// The one table. Every current normal-mode binding lives here exactly once.
pub const KEYMAP: &[Binding] = &[
    // View. Tab / Shift-Tab cycle, 1-5 jump; the F-keys are the old names, kept for
    // one release (the first press says so once).
    Binding { matchers: &[Code(KeyCode::Tab)], keys: "tab", group: View, scope: Scope::Global, act: ViewNext, help: "next view" },
    Binding { matchers: &[Code(KeyCode::BackTab)], keys: "S-tab", group: View, scope: Scope::Global, act: ViewPrev, help: "previous view" },
    Binding { matchers: &[Char('1'), Code(KeyCode::F(1))], keys: "1 / F1", group: View, scope: Scope::Global, act: ScreenQueue, help: "Queue" },
    Binding { matchers: &[Char('2'), Code(KeyCode::F(2))], keys: "2 / F2", group: View, scope: Scope::Global, act: ScreenAlbums, help: "Albums" },
    Binding { matchers: &[Char('3'), Code(KeyCode::F(3))], keys: "3 / F3", group: View, scope: Scope::Global, act: ScreenPlaylists, help: "Lists (playlists)" },
    Binding { matchers: &[Char('4'), Code(KeyCode::F(4))], keys: "4 / F4", group: View, scope: Scope::Global, act: ScreenDj, help: "DJ (ask in words)" },
    Binding { matchers: &[Char('5'), Code(KeyCode::F(5))], keys: "5 / F5", group: View, scope: Scope::Global, act: ScreenFind, help: "Find (library query)" },
    // Navigation.
    Binding { matchers: &[Char('j'), Code(KeyCode::Down), Ctrl('n')], keys: "j / down / C-n", group: Navigation, scope: Scope::Global, act: Down, help: "move cursor down" },
    Binding { matchers: &[Char('k'), Code(KeyCode::Up), Ctrl('p')], keys: "k / up / C-p", group: Navigation, scope: Scope::Global, act: Up, help: "move cursor up" },
    Binding { matchers: &[Char('g')], keys: "g", group: Navigation, scope: Scope::Global, act: Top, help: "jump to top" },
    Binding { matchers: &[Char('G')], keys: "G", group: Navigation, scope: Scope::Global, act: Bottom, help: "jump to bottom" },
    Binding { matchers: &[Char('P')], keys: "P", group: Navigation, scope: Scope::Queue, act: JumpCurrent, help: "jump to the playing song" },
    Binding { matchers: &[Char('}')], keys: "}", group: Navigation, scope: Scope::Browse, act: SectionNext, help: "next result section (Search)" },
    Binding { matchers: &[Char('{')], keys: "{", group: Navigation, scope: Scope::Browse, act: SectionPrev, help: "previous result section (Search)" },
    // Playback.
    Binding { matchers: &[Char('p')], keys: "p", group: Playback, scope: Scope::Global, act: Pause, help: "play / pause" },
    Binding { matchers: &[Char('>')], keys: ">", group: Playback, scope: Scope::Global, act: Next, help: "next track" },
    Binding { matchers: &[Char('<')], keys: "<", group: Playback, scope: Scope::Global, act: Prev, help: "previous track" },
    Binding { matchers: &[Ctrl('f')], keys: "C-f", group: Playback, scope: Scope::Global, act: ScrubFwd, help: "scrub forward 5s" },
    Binding { matchers: &[Ctrl('b')], keys: "C-b", group: Playback, scope: Scope::Global, act: ScrubBack, help: "scrub back 5s" },
    Binding { matchers: &[Char('r')], keys: "r", group: Playback, scope: Scope::Global, act: Radio, help: "endless radio from this row" },
    // Modes. ncmpcpp's letters where they are free (z, y, R); `r` is radio here, so
    // repeat takes `e`.
    Binding { matchers: &[Char('z')], keys: "z", group: Playback, scope: Scope::Global, act: ToggleRandom, help: "toggle random order" },
    Binding { matchers: &[Char('e')], keys: "e", group: Playback, scope: Scope::Global, act: ToggleRepeat, help: "toggle repeat (the queue)" },
    Binding { matchers: &[Char('y')], keys: "y", group: Playback, scope: Scope::Global, act: ToggleSingle, help: "toggle single (stop after this track)" },
    Binding { matchers: &[Char('R')], keys: "R", group: Playback, scope: Scope::Global, act: ToggleConsume, help: "toggle consume (drop played tracks)" },
    // Volume.
    Binding { matchers: &[Char('0'), Char('+'), Char('=')], keys: "0 / + / =", group: Volume, scope: Scope::Global, act: VolumeUp, help: "turn the knob up" },
    Binding { matchers: &[Char('9'), Char('-'), Char('_')], keys: "9 / - / _", group: Volume, scope: Scope::Global, act: VolumeDown, help: "turn the knob down" },
    // Search.
    Binding { matchers: &[Char('/')], keys: "/", group: Search, scope: Scope::Global, act: SearchStart, help: "incremental search" },
    Binding { matchers: &[Char('n')], keys: "n", group: Search, scope: Scope::Global, act: SearchNext, help: "repeat search forward" },
    Binding { matchers: &[Char('N')], keys: "N", group: Search, scope: Scope::Global, act: SearchPrev, help: "repeat search backward" },
    // Command.
    Binding { matchers: &[Char(':')], keys: ":", group: Command, scope: Scope::Global, act: CommandLine, help: "command / NL line" },
    // Browse.
    Binding { matchers: &[Code(KeyCode::Enter)], keys: "enter", group: Browse, scope: Scope::Global, act: PlaySel, help: "play the selection" },
    Binding { matchers: &[Char(' ')], keys: "space", group: Browse, scope: Scope::Browse, act: Enqueue, help: "enqueue the selected row" },
    Binding { matchers: &[Char('d'), Code(KeyCode::Delete)], keys: "d / del", group: Browse, scope: Scope::Queue, act: RemoveSel, help: "remove the selected track from the queue" },
    Binding { matchers: &[Char('l'), Code(KeyCode::Right)], keys: "l / right", group: Browse, scope: Scope::Browse, act: BrowseIn, help: "drill into a directory" },
    Binding { matchers: &[Char('o')], keys: "o", group: Browse, scope: Scope::Global, act: Menu, help: "context menu for this row (open is item 1)" },
    Binding { matchers: &[Char('O')], keys: "O", group: Browse, scope: Scope::Global, act: MenuCurrent, help: "context menu for the playing track" },
    Binding { matchers: &[Char('h'), Code(KeyCode::Left), Code(KeyCode::Esc)], keys: "h / left / esc", group: Browse, scope: Scope::Browse, act: BrowseBack, help: "back out one browse level" },
    // Favorites.
    Binding { matchers: &[Char('s')], keys: "s", group: Favorites, scope: Scope::Global, act: FavSelected, help: "star the selected row" },
    Binding { matchers: &[Ctrl('s')], keys: "C-s", group: Favorites, scope: Scope::Global, act: FavCurrent, help: "star the playing track" },
    // `t` for TAPE. C-s on a stream keeps the audio of what it marked, and the segment
    // outlives the one-line banner that announced it - so the gesture needs a read-back
    // that is not "go run another program". The daemon renders it; this key asks for it.
    Binding { matchers: &[Char('t')], keys: "t", group: Favorites, scope: Scope::Global, act: Heard, help: "what you marked, and the audio kept" },
    Binding { matchers: &[Char('i')], keys: "i", group: General, scope: Scope::Global, act: Info, help: "details about the selected song" },
    // Retraction. `u` acts, `U` shows - the same shift-pair shape as `o`/`O`. In
    // General rather than Playback because it is not a transport gesture: it puts the
    // world back the way it was, whatever the last plan action did to it.
    Binding { matchers: &[Char('u')], keys: "u", group: General, scope: Scope::Global, act: Undo, help: "undo the last thing the daemon did" },
    Binding { matchers: &[Char('U')], keys: "U", group: General, scope: Scope::Global, act: Journal, help: "what ran, and what can still be undone" },
    // General.
    Binding { matchers: &[Char('?')], keys: "?", group: General, scope: Scope::Global, act: HelpToggle, help: "toggle this help" },
    Binding { matchers: &[Char('q')], keys: "q", group: General, scope: Scope::Global, act: Quit, help: "quit" },
];

/// What the `?` help is ABOUT: the view (or the info card) it was opened from. The
/// overlay leads with this context's own keys, then the global table, so the keys that
/// are special HERE are the first thing read rather than tags buried in a long list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpView {
    Queue,
    Albums,
    Lists,
    Dj,
    Find,
    Info,
}

impl HelpView {
    pub fn title(&self) -> &'static str {
        match self {
            HelpView::Queue => "Queue",
            HelpView::Albums => "Albums",
            HelpView::Lists => "Lists",
            HelpView::Dj => "DJ",
            HelpView::Find => "Find",
            HelpView::Info => "Info",
        }
    }

    /// The [`KEYMAP`] scope whose rows belong to this view's section: the queue-only
    /// rows on Queue, the browse-only rows on every browsing view.
    pub fn scope(&self) -> Option<Scope> {
        match self {
            HelpView::Queue => Some(Scope::Queue),
            HelpView::Albums | HelpView::Lists | HelpView::Find => Some(Scope::Browse),
            HelpView::Dj | HelpView::Info => None,
        }
    }
}

/// A key a view claims in its OWN dispatch, before (or instead of) [`KEYMAP`]: the DJ
/// ask line, the Find query line, the info card's scroll. Listed here so the help
/// overlay reads them from a table rather than from prose; the `view_keys_*` tests in
/// state.rs press every matcher in its view and fail if one does nothing, so a row
/// cannot outlive the dispatch arm it describes. An empty `matchers` is a gesture, not
/// a key (typing text), and is documented only.
pub struct ViewKey {
    pub view: HelpView,
    pub matchers: &'static [KeyMatch],
    pub keys: &'static str,
    pub help: &'static str,
}

pub const VIEW_KEYS: &[ViewKey] = &[
    ViewKey { view: HelpView::Dj, matchers: &[], keys: "type", help: "ask the DJ in your own words" },
    ViewKey { view: HelpView::Dj, matchers: &[Code(KeyCode::Enter)], keys: "enter", help: "send the ask" },
    ViewKey { view: HelpView::Dj, matchers: &[Code(KeyCode::Backspace)], keys: "backspace", help: "delete a character" },
    ViewKey { view: HelpView::Dj, matchers: &[Code(KeyCode::Esc)], keys: "esc", help: "leave to the Queue" },
    ViewKey { view: HelpView::Dj, matchers: &[], keys: "?", help: "this help (on an empty line)" },
    ViewKey { view: HelpView::Find, matchers: &[], keys: "type", help: "write a library query" },
    ViewKey { view: HelpView::Find, matchers: &[Code(KeyCode::Enter)], keys: "enter", help: "run the query" },
    ViewKey { view: HelpView::Find, matchers: &[Code(KeyCode::Up), Code(KeyCode::Down)], keys: "up / down", help: "walk the query history" },
    ViewKey { view: HelpView::Find, matchers: &[Code(KeyCode::Tab)], keys: "tab", help: "switch between query and results" },
    ViewKey { view: HelpView::Find, matchers: &[Code(KeyCode::Esc)], keys: "esc", help: "out of a drill, then off the query line" },
    ViewKey { view: HelpView::Info, matchers: &[Char('j'), Code(KeyCode::Down), Char('k'), Code(KeyCode::Up)], keys: "j / k", help: "scroll a line" },
    ViewKey { view: HelpView::Info, matchers: &[Code(KeyCode::PageDown), Code(KeyCode::PageUp), Char(' ')], keys: "pgdn / pgup", help: "scroll a page" },
    ViewKey { view: HelpView::Info, matchers: &[Char('i'), Code(KeyCode::Esc), Char('q')], keys: "i / esc / q", help: "close the card" },
    ViewKey { view: HelpView::Info, matchers: &[Char('?')], keys: "?", help: "this help" },
];

/// The Albums COVER GRID's 2D cursor. A table rather than a match so the dispatch
/// (`TuiState::key_grid`) and the help overlay read the same rows: what `?` says the
/// grid does is what the grid does.
pub struct GridKey {
    pub matchers: &'static [KeyMatch],
    pub keys: &'static str,
    pub step: (i32, i32),
    pub help: &'static str,
}

pub const GRID_KEYS: &[GridKey] = &[
    GridKey { matchers: &[Code(KeyCode::Left)], keys: "left", step: (-1, 0), help: "previous cover" },
    GridKey { matchers: &[Code(KeyCode::Right)], keys: "right", step: (1, 0), help: "next cover" },
    GridKey { matchers: &[Char('j'), Code(KeyCode::Down), Ctrl('n')], keys: "j / down / C-n", step: (0, 1), help: "cover row below" },
    GridKey { matchers: &[Char('k'), Code(KeyCode::Up), Ctrl('p')], keys: "k / up / C-p", step: (0, -1), help: "cover row above" },
];

/// The grid step for `key`, if it is a grid key. A `Code` matcher never fires with
/// Ctrl held, so a Ctrl-arrow keeps whatever meaning the shared table gives it.
pub fn grid_step(key: KeyEvent) -> Option<(i32, i32)> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    GRID_KEYS
        .iter()
        .find(|g| g.matchers.iter().any(|m| !(ctrl && matches!(m, Code(_))) && m.matches(key)))
        .map(|g| g.step)
}

/// Resolve a key event to its Act, honoring the readline-first ordering (a Ctrl chord
/// is matched as `Ctrl`, so a plain `p`/`n`/`s` never shadows `C-p`/`C-n`/`C-s`). The
/// `_screen` is accepted for scope gating; screen-specific no-ops stay guarded in the
/// dispatch bodies, so this returns the first matching row.
pub fn match_key(key: KeyEvent, _screen: Screen) -> Option<Act> {
    KEYMAP
        .iter()
        .find(|b| b.matchers.iter().any(|m| m.matches(key)))
        .map(|b| b.act)
}

/// The keymap grouped in stable order, for the two-column overlay. Empty groups are
/// omitted.
pub fn grouped() -> Vec<(Group, Vec<&'static Binding>)> {
    GROUP_ORDER
        .iter()
        .filter_map(|g| {
            let rows: Vec<&'static Binding> = KEYMAP.iter().filter(|b| b.group == *g).collect();
            if rows.is_empty() {
                None
            } else {
                Some((*g, rows))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn every_matcher_round_trips_to_its_own_act() {
        // A table typo (wrong char, wrong Ctrl flag) is caught here: each matcher must
        // resolve via match_key back to the Act it is declared on.
        for b in KEYMAP {
            for m in b.matchers {
                let ev = match *m {
                    KeyMatch::Char(c) => ch(c),
                    KeyMatch::Ctrl(c) => ctrl(c),
                    KeyMatch::Code(code) => key(code),
                };
                assert_eq!(
                    match_key(ev, Screen::Queue),
                    Some(b.act),
                    "matcher {m:?} for {:?} did not round-trip",
                    b.act
                );
            }
        }
    }

    #[test]
    fn ctrl_chords_do_not_collide_with_plain_letters() {
        // Plain p is Pause; C-p is Up (cursor). Plain s is FavSelected; C-s FavCurrent.
        assert_eq!(match_key(ch('p'), Screen::Queue), Some(Act::Pause));
        assert_eq!(match_key(ctrl('p'), Screen::Queue), Some(Act::Up));
        assert_eq!(match_key(ch('s'), Screen::Queue), Some(Act::FavSelected));
        assert_eq!(match_key(ctrl('s'), Screen::Queue), Some(Act::FavCurrent));
        assert_eq!(match_key(ctrl('n'), Screen::Queue), Some(Act::Down));
    }

    #[test]
    fn no_two_rows_claim_the_same_matcher() {
        // match_key returns the FIRST matching row, so a duplicated matcher would make
        // one binding permanently unreachable - silently, since the round-trip test
        // only checks that each matcher resolves to SOME row's Act. Adding a row for an
        // already-taken key (the `r` question) is caught here.
        let mut seen: Vec<(KeyMatch, Act)> = Vec::new();
        for b in KEYMAP {
            for m in b.matchers {
                if let Some((_, other)) = seen.iter().find(|(k, _)| k == m) {
                    panic!("matcher {m:?} is claimed by both {other:?} and {:?}", b.act);
                }
                seen.push((*m, b.act));
            }
        }
    }

    #[test]
    fn grouped_is_stable_and_non_empty() {
        let g = grouped();
        // Every group present is non-empty and appears in GROUP_ORDER order.
        assert!(!g.is_empty());
        let order: Vec<Group> = g.iter().map(|(grp, _)| *grp).collect();
        let mut expected: Vec<Group> = GROUP_ORDER
            .iter()
            .copied()
            .filter(|grp| KEYMAP.iter().any(|b| b.group == *grp))
            .collect();
        assert_eq!(order, expected);
        for (_, rows) in &g {
            assert!(!rows.is_empty());
        }
        expected.dedup();
        assert_eq!(expected.len(), order.len(), "no duplicate groups");
    }

    #[test]
    fn documented_bindings_all_present() {
        // The set the task calls out must each have a row.
        let want_chars = ['j', 'k', 'g', 'G', 'P', '/', 'n', 'N', ':', 'p', '<', '>', 'r', 's', 'o', 'O', 'l', 'h', 'q', '?', '0', '9', ' ', 'z', 'e', 'y', 'R'];
        for c in want_chars {
            assert!(
                KEYMAP.iter().any(|b| b.matchers.iter().any(|m| matches!(m, KeyMatch::Char(k) if *k == c))),
                "missing a keymap row for {c:?}"
            );
        }
        for f in 1..=5u8 {
            assert!(
                KEYMAP.iter().any(|b| b.matchers.iter().any(|m| matches!(m, KeyMatch::Code(KeyCode::F(n)) if *n == f))),
                "missing F{f}"
            );
        }
    }
}
