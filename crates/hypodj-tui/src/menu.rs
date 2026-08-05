//! The row CONTEXT MENU: what the thing under the cursor (`o`) or the playing track
//! (`O`) can do, as data.
//!
//! [`Target`] is the normalization state.rs never had - a queue row, a browse row, a
//! Find hit and the now-playing track collapse into one KIND plus one ORIGIN, so the
//! per-screen "what is selected" match is written once here instead of a seventh time
//! per action. [`MenuItem`] is the vocabulary (identity, label, hotkey, order) and
//! [`MenuAction`] is the resolved command that owns its data: the same Act/Intent
//! split state.rs already runs on, bridged by [`MenuAction::item`] so the two halves
//! cannot disagree. [`rows_for`] is the ONLY builder.
//!
//! Pure: no `State`, no socket, no screen - the whole "what can this row do" question
//! is unit-testable exactly as `keymap.rs` and `find.rs` are.

/// What the target IS. Decides which items are MEANINGFUL at all; missing DATA is a
/// separate axis and only decides live-vs-blocked (see [`Avail`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    /// A library track (`song/<id>`).
    LibrarySong,
    /// A raw stream URL, or a queue row with no uri at all. The FAIL-CLOSED default of
    /// [`classify`]: a uri-less queue row is a stream, never a library song whose
    /// actions would all dead-end on a missing uri.
    Stream,
    /// An `album/<id>` directory.
    Album,
    /// An `artist/<id>` directory. Only a Find artist hit produces one today.
    Artist,
    /// A SAVED station (`station/<name>`, name VERBATIM). A LEAF: `lsinfo` on it hits
    /// the daemon catch-all and returns a well-formed EMPTY listing.
    Station,
    /// A stored playlist. Its "uri" is a NAME, not a browse path - which is why
    /// [`classify`] can never produce this from a uri prefix; only the Playlists
    /// screen knows its rows are names, and it says so at the resolver.
    Playlist,
    /// Any other browse directory (`list/...`, `Lists`, `Starred`, a genre).
    Dir,
}

/// Where the target came from. Orthogonal to the kind: two identical songs behave
/// differently by origin - only a QUEUE row has a position to jump to or delete, and
/// only the playing track carries a songrec `match_uri`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The selected queue row, at this 0-based MPD `Pos`.
    Queue { pos: usize },
    /// The selected row of a browse list (Albums, Playlists, or a Find drill).
    Browse,
    /// The selected Find HIT row - not a `Browse`, which is why every per-action copy
    /// of the old cursor match had to claim it first.
    FindHit,
    /// The currently PLAYING track, reached with `O` rather than the cursor.
    NowPlaying,
}

impl Origin {
    /// The short word the popup shows next to the label, so the eye can tell a menu
    /// over the queue row from one over the same song's Find hit.
    pub fn word(self) -> &'static str {
        match self {
            Origin::Queue { .. } => "queue row",
            Origin::Browse => "browse row",
            Origin::FindHit => "find hit",
            Origin::NowPlaying => "playing",
        }
    }
}

/// The resolved subject of the menu. Owned: the menu outlives the keypress that opened
/// it and `State` methods are `&mut self`, so a borrow would fight the borrow checker
/// for no gain at human key rates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub kind: TargetKind,
    pub origin: Origin,
    /// What the eye sees on the row. Popup title and status wording.
    pub label: String,
    /// The target's OWN uri: `song/<id>`, `album/<id>`, `artist/<id>`,
    /// `station/<name>`, a stream URL, or a playlist NAME. `None` only when the source
    /// row genuinely carried no `file` key (a raw stream queue row).
    pub uri: Option<String>,
    /// The OWNING album (`X-AlbumUri`). Live today for a queue row, a Find song hit, an
    /// lsinfo song row and `currentsong` - the daemon already emits it on all four.
    pub album_uri: Option<String>,
    /// The artist NAME. No artist uri exists on the wire yet, so this is what
    /// "go to artist" navigates with (see [`ArtistRef`]).
    pub artist: Option<String>,
    /// The artist's browse uri (`artist/<id>`). Nothing in the workspace emits one
    /// today, so this is always `None` in this slice; the daemon slice fills it and
    /// nothing else in this module changes.
    pub artist_uri: Option<String>,
    /// The library counterpart of a RECOGNIZED radio track (`X-MatchUri`), so the
    /// now-playing menu can star and seed from what the radio is playing even though
    /// `uri` is the stream URL.
    pub match_uri: Option<String>,
}

/// How "play" is expressed. The queue case is a JUMP, not an enqueue: re-adding a row
/// that is already in the queue is a different action with a different result, and the
/// distinction lives in the type rather than in a runtime re-check of the origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayHow {
    /// Jump to an existing queue position (`play <pos>`).
    QueuePos(usize),
    /// Add the uri and play the new tail.
    Enqueue(String),
    /// Append a stored playlist by NAME (`load <name>`) - exactly what Enter on a
    /// Playlists row has always done.
    LoadPlaylist(String),
}

/// What "go to artist" has to navigate with. There is NO artist uri on the wire today:
/// core `Song` has no `artist_id`, `map_song` drops `Child.artist_id`, and nothing
/// emits `X-ArtistUri`. So the NAME path is the primary path, not a bolted-on
/// fallback, and `Uri` is the door the daemon slice walks through with no change here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtistRef {
    /// A real browse path (`artist/<id>`): navigates.
    Uri(String),
    /// An artist NAME: runs the library query and lands on the Find hits, where the
    /// artist section is first. One keystroke short of a jump, and honest about being
    /// a search - the label says "(search)".
    Name(String),
}

/// The menu's vocabulary: what an entry IS. Copy and payload-free so a BLOCKED row can
/// still name itself. Declaration order is render order; [`MenuItem::ord`] is the
/// exhaustive match that pins it, with no parallel `ALL` array to silently disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
    OpenContents,
    Play,
    Enqueue,
    GoToAlbum,
    GoToArtist,
    Radio,
    Favorite,
    RemoveFromQueue,
}

impl MenuItem {
    /// Position in canonical order, so the eye learns one shape across kinds.
    pub fn ord(self) -> usize {
        match self {
            MenuItem::OpenContents => 0,
            MenuItem::Play => 1,
            MenuItem::Enqueue => 2,
            MenuItem::GoToAlbum => 3,
            MenuItem::GoToArtist => 4,
            MenuItem::Radio => 5,
            MenuItem::Favorite => 6,
            MenuItem::RemoveFromQueue => 7,
        }
    }

    /// The default rendered verb (a live row may sharpen it, see [`MenuAction::label`]).
    pub fn label(self) -> &'static str {
        match self {
            MenuItem::OpenContents => "open contents",
            MenuItem::Play => "play now",
            MenuItem::Enqueue => "add to queue",
            MenuItem::GoToAlbum => "go to album",
            MenuItem::GoToArtist => "go to artist",
            MenuItem::Radio => "start a radio from here",
            MenuItem::Favorite => "star",
            MenuItem::RemoveFromQueue => "remove from queue",
        }
    }

    /// The direct-pick letter shown in the gutter. Unique across items and disjoint
    /// from the menu's own control keys (j k g G q l enter esc) - both are tests.
    pub fn hotkey(self) -> char {
        match self {
            MenuItem::OpenContents => 'o',
            MenuItem::Play => 'p',
            MenuItem::Enqueue => 'a',
            MenuItem::GoToAlbum => 'b',
            MenuItem::GoToArtist => 't',
            MenuItem::Radio => 'r',
            MenuItem::Favorite => 's',
            MenuItem::RemoveFromQueue => 'x',
        }
    }
}

/// A RESOLVED menu command: the data is already in hand, so dispatch is a pure
/// function of the action and the same `GoToAlbum` built from a queue row, a Find song
/// hit and the playing track is provably the same behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuAction {
    /// Drill into the target's children (`lsinfo <uri>`).
    OpenContents(String),
    Play(PlayHow),
    /// Append without playing (`add <uri>`).
    Enqueue(String),
    /// Reveal the owning album's track list.
    GoToAlbum(String),
    GoToArtist(ArtistRef),
    /// Seed the endless radio (`radio <uri>`). Dispatch routes through the EXISTING
    /// `radio_from_uri` gate, so this row and the bare `r` key cannot disagree.
    Radio(String),
    /// `playlistadd Starred <uri>`.
    Favorite(String),
    /// `delete <pos>`, guarded against a stale position (see [`Menu`]).
    RemoveFromQueue(usize),
}

impl MenuAction {
    /// The bridge between the two halves. EXHAUSTIVE, so a new action cannot exist
    /// without an item to render it, and `MenuRow::live` derives the item from the
    /// action rather than trusting a caller to pair them.
    pub fn item(&self) -> MenuItem {
        match self {
            MenuAction::OpenContents(_) => MenuItem::OpenContents,
            MenuAction::Play(_) => MenuItem::Play,
            MenuAction::Enqueue(_) => MenuItem::Enqueue,
            MenuAction::GoToAlbum(_) => MenuItem::GoToAlbum,
            MenuAction::GoToArtist(_) => MenuItem::GoToArtist,
            MenuAction::Radio(_) => MenuItem::Radio,
            MenuAction::Favorite(_) => MenuItem::Favorite,
            MenuAction::RemoveFromQueue(_) => MenuItem::RemoveFromQueue,
        }
    }

    /// The rendered verb. Sharpened where the resolved data makes the generic label
    /// wrong: loading a playlist is not "play now", and a search is not a jump.
    pub fn label(&self) -> String {
        match self {
            MenuAction::Play(PlayHow::LoadPlaylist(_)) => "load into queue".to_string(),
            MenuAction::GoToArtist(ArtistRef::Name(_)) => "go to artist (search)".to_string(),
            other => other.item().label().to_string(),
        }
    }
}

/// Whether a listed item can run here. The rule, once (the seed, not a case list): an
/// item is ABSENT when its family does not apply to the target's family (no album on a
/// playlist, no star on an artist), and PRESENT-but-blocked with a REASON when the
/// family applies but this instance lacks the datum. Availability carries the resolved
/// payload, so a live row cannot be built without the data it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Avail {
    Yes(MenuAction),
    No(&'static str),
}

/// One rendered menu line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuRow {
    pub item: MenuItem,
    pub label: String,
    pub avail: Avail,
}

impl MenuRow {
    fn live(action: MenuAction) -> Self {
        MenuRow { item: action.item(), label: action.label(), avail: Avail::Yes(action) }
    }
    fn blocked(item: MenuItem, why: &'static str) -> Self {
        MenuRow { item, label: item.label().to_string(), avail: Avail::No(why) }
    }
}

/// The open menu. A snapshot: while it is open the modal intercept swallows nav keys,
/// so the rows can never describe a row the cursor has since left.
#[derive(Debug)]
pub struct Menu {
    pub target: Target,
    pub rows: Vec<MenuRow>,
    pub selected: usize,
    /// Queue length at open time. A refresh that changes it invalidates a
    /// pos-addressed target, so the event loop closes the menu on a mismatch; dispatch
    /// ADDITIONALLY re-checks the uri at the snapshot pos, which catches a same-length
    /// reorder that the length alone would miss.
    pub queue_len: usize,
}

impl Menu {
    /// Build for a target, parking the cursor on the first LIVE row so Enter is never
    /// a refusal.
    pub fn new(target: Target, queue_len: usize) -> Menu {
        let rows = rows_for(&target);
        let selected = rows
            .iter()
            .position(|r| matches!(r.avail, Avail::Yes(_)))
            .unwrap_or(0);
        Menu { target, rows, selected, queue_len }
    }

    /// Clamped, never wrapping - the same motion contract as every other list here.
    pub fn move_selection(&mut self, delta: i32) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as i32 - 1;
        self.selected = (self.selected as i32 + delta).clamp(0, last) as usize;
    }

    /// Resolve a direct hotkey press to its row index.
    pub fn pick(&self, c: char) -> Option<usize> {
        self.rows.iter().position(|r| r.item.hotkey() == c)
    }
}

/// Classify a uri by PREFIX, exactly as the daemon's find blocks do (`Block::start`).
/// FAILS CLOSED: an absent uri is a Stream, never a LibrarySong - a raw-stream queue
/// row commonly has no `file` key, and calling it a song would hand it the whole
/// library menu whose every action then dead-ends.
pub fn classify(uri: Option<&str>, is_dir: bool) -> TargetKind {
    match uri {
        Some(u) if u.starts_with("song/") => TargetKind::LibrarySong,
        Some(u) if u.starts_with("album/") => TargetKind::Album,
        Some(u) if u.starts_with("artist/") => TargetKind::Artist,
        Some(u) if u.starts_with("station/") => TargetKind::Station,
        Some(u) if u.contains("://") => TargetKind::Stream,
        Some(_) if is_dir => TargetKind::Dir,
        Some(_) => TargetKind::LibrarySong,
        None => TargetKind::Stream,
    }
}

/// The rows for a target, in canonical order by construction. The ONE place
/// applicability lives, and the ONE builder of a [`MenuRow`].
pub fn rows_for(t: &Target) -> Vec<MenuRow> {
    use MenuItem as I;
    let mut v: Vec<MenuRow> = Vec::new();
    let uri = t.uri.clone();
    let playing = t.origin == Origin::NowPlaying;

    // Open: containers only. A Station LOOKS like a path but is a leaf, and a playlist
    // has no browse level at all - both are fair asks, so both say why.
    match t.kind {
        TargetKind::Album | TargetKind::Artist | TargetKind::Dir => {
            if let Some(u) = &uri {
                v.push(MenuRow::live(MenuAction::OpenContents(u.clone())));
            }
        }
        TargetKind::Station => v.push(MenuRow::blocked(
            I::OpenContents,
            "a station has nothing to open - play it instead",
        )),
        TargetKind::Playlist => v.push(MenuRow::blocked(
            I::OpenContents,
            "a playlist has no browse level - load it instead",
        )),
        TargetKind::LibrarySong | TargetKind::Stream => {}
    }

    // Play / add. Already-playing and already-queued targets do not offer an add.
    if !playing {
        match (t.origin, t.kind) {
            (Origin::Queue { pos }, _) => {
                v.push(MenuRow::live(MenuAction::Play(PlayHow::QueuePos(pos))))
            }
            (_, TargetKind::Playlist) => {
                if let Some(u) = &uri {
                    v.push(MenuRow::live(MenuAction::Play(PlayHow::LoadPlaylist(u.clone()))));
                }
            }
            // `enqueue_uri` rejects `artist/<id>` outright, so play/add are refusals
            // here, not silence - opening the artist is the working verb.
            (_, TargetKind::Artist) => {
                v.push(MenuRow::blocked(I::Play, "an artist can't be enqueued - open it"));
                v.push(MenuRow::blocked(I::Enqueue, "an artist can't be enqueued - open it"));
            }
            _ => {
                if let Some(u) = &uri {
                    v.push(MenuRow::live(MenuAction::Play(PlayHow::Enqueue(u.clone()))));
                    v.push(MenuRow::live(MenuAction::Enqueue(u.clone())));
                }
            }
        }
    }

    // Go to album: absent when the target IS an album or has no album concept.
    match t.kind {
        TargetKind::LibrarySong => v.push(match &t.album_uri {
            Some(a) => MenuRow::live(MenuAction::GoToAlbum(a.clone())),
            None => MenuRow::blocked(I::GoToAlbum, "this listing carries no album uri"),
        }),
        TargetKind::Stream => v.push(match (&t.album_uri, &t.match_uri) {
            (Some(a), _) => MenuRow::live(MenuAction::GoToAlbum(a.clone())),
            (None, Some(_)) => MenuRow::blocked(
                I::GoToAlbum,
                "the matched track's album is not on the wire yet",
            ),
            _ => MenuRow::blocked(I::GoToAlbum, "a stream has no library album"),
        }),
        _ => {}
    }

    // Go to artist: absent when the target IS the artist or has no artist concept.
    match t.kind {
        TargetKind::LibrarySong | TargetKind::Album | TargetKind::Stream => {
            v.push(match (&t.artist_uri, &t.artist) {
                (Some(a), _) => MenuRow::live(MenuAction::GoToArtist(ArtistRef::Uri(a.clone()))),
                (None, Some(n)) => {
                    MenuRow::live(MenuAction::GoToArtist(ArtistRef::Name(n.clone())))
                }
                (None, None) => {
                    MenuRow::blocked(I::GoToArtist, "this listing carries no artist")
                }
            })
        }
        _ => {}
    }

    // Radio. The whitelist and the reasons are the SAME ones `radio_from_uri` shows for
    // the bare `r` key; dispatch calls that function, so the two cannot drift.
    let seed = t.match_uri.clone().or_else(|| uri.clone());
    v.push(match seed.as_deref() {
        Some(u)
            if u.starts_with("song/") || u.starts_with("album/") || u.starts_with("artist/") =>
        {
            MenuRow::live(MenuAction::Radio(u.to_string()))
        }
        Some(u) if u.starts_with("station/") => MenuRow::blocked(
            I::Radio,
            "a saved station is a stream, not a library seed - play it instead",
        ),
        Some(u) if u.starts_with("list/") => {
            MenuRow::blocked(I::Radio, "can't start a radio from a list")
        }
        _ if t.kind == TargetKind::Playlist => {
            MenuRow::blocked(I::Radio, "can't start a radio from a playlist")
        }
        Some(u) if u.contains("://") => {
            MenuRow::blocked(I::Radio, "that row is a stream, can't start a radio")
        }
        _ => MenuRow::blocked(I::Radio, "can't start a radio from that row"),
    });

    // Star. The daemon's star surface takes songs and albums only.
    match t.kind {
        TargetKind::LibrarySong | TargetKind::Album => {
            if let Some(u) = &uri {
                v.push(MenuRow::live(MenuAction::Favorite(u.clone())));
            }
        }
        TargetKind::Stream => v.push(match &t.match_uri {
            Some(m) => {
                let mut r = MenuRow::live(MenuAction::Favorite(m.clone()));
                r.label = "star the matched track".to_string();
                r
            }
            None => MenuRow::blocked(I::Favorite, "a stream has no library track to star"),
        }),
        TargetKind::Artist => v.push(MenuRow::blocked(
            I::Favorite,
            "the star surface takes songs and albums only",
        )),
        _ => {}
    }

    if let Origin::Queue { pos } = t.origin {
        v.push(MenuRow::live(MenuAction::RemoveFromQueue(pos)));
    }
    // Canonical order is a property of the code ABOVE, not of a hand-written `ALL`
    // array that could silently disagree with it. This is what makes [`MenuItem::ord`]
    // load-bearing rather than decorative: reordering a block here trips in dev before
    // it can reach an eye that has learned one shape across kinds.
    debug_assert!(
        v.windows(2).all(|w| w[0].item.ord() < w[1].item.ord()),
        "rows_for built rows out of canonical order: {:?}",
        v.iter().map(|r| r.item).collect::<Vec<_>>()
    );
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare target: every optional datum absent, so a fixture only states what it
    /// actually has and an accidentally-live row is visible in the diff.
    fn target(kind: TargetKind, origin: Origin, uri: Option<&str>) -> Target {
        Target {
            kind,
            origin,
            label: "row".to_string(),
            uri: uri.map(str::to_string),
            album_uri: None,
            artist: None,
            artist_uri: None,
            match_uri: None,
        }
    }

    /// The (item, blocked?) sequence a fixture asserts - the exact shape of the table
    /// in the design, so a reordering or a silently-flipped availability fails here.
    fn shape(t: &Target) -> Vec<(MenuItem, bool)> {
        rows_for(t)
            .iter()
            .map(|r| (r.item, matches!(r.avail, Avail::No(_))))
            .collect()
    }

    #[test]
    fn classify_fails_closed_on_an_absent_uri_and_reads_every_prefix() {
        assert_eq!(classify(None, false), TargetKind::Stream, "a uri-less row is a stream");
        assert_eq!(classify(None, true), TargetKind::Stream);
        assert_eq!(classify(Some("song/1"), false), TargetKind::LibrarySong);
        assert_eq!(classify(Some("album/7"), true), TargetKind::Album);
        assert_eq!(classify(Some("artist/3"), true), TargetKind::Artist);
        assert_eq!(classify(Some("station/NTS 1"), false), TargetKind::Station);
        assert_eq!(classify(Some("https://stream.example/x"), false), TargetKind::Stream);
        assert_eq!(classify(Some("list/newest"), true), TargetKind::Dir);
        // A non-dir row with an unmodelled path is the only case that lands on
        // LibrarySong by elimination: an lsinfo `file:` row that is not `song/`.
        assert_eq!(classify(Some("Lists"), true), TargetKind::Dir);
        assert_eq!(classify(Some("weird"), false), TargetKind::LibrarySong);
    }

    #[test]
    fn a_queue_song_offers_the_jump_the_album_the_artist_the_radio_the_star_and_the_delete() {
        let mut t = target(TargetKind::LibrarySong, Origin::Queue { pos: 4 }, Some("song/1"));
        t.album_uri = Some("album/7".into());
        t.artist = Some("Alice Coltrane".into());
        assert_eq!(
            shape(&t),
            vec![
                (MenuItem::Play, false),
                (MenuItem::GoToAlbum, false),
                (MenuItem::GoToArtist, false),
                (MenuItem::Radio, false),
                (MenuItem::Favorite, false),
                (MenuItem::RemoveFromQueue, false),
            ]
        );
        let rows = rows_for(&t);
        assert_eq!(rows[0].avail, Avail::Yes(MenuAction::Play(PlayHow::QueuePos(4))));
        assert_eq!(rows[1].avail, Avail::Yes(MenuAction::GoToAlbum("album/7".into())));
        assert_eq!(rows[2].label, "go to artist (search)", "a name is a search, and says so");
        assert_eq!(rows[5].avail, Avail::Yes(MenuAction::RemoveFromQueue(4)));
    }

    #[test]
    fn a_queue_song_with_no_album_uri_says_the_listing_lacks_it() {
        let t = target(TargetKind::LibrarySong, Origin::Queue { pos: 0 }, Some("song/1"));
        let rows = rows_for(&t);
        assert_eq!(rows[1].avail, Avail::No("this listing carries no album uri"));
        assert_eq!(rows[2].avail, Avail::No("this listing carries no artist"));
    }

    #[test]
    fn a_queue_stream_row_keeps_the_jump_and_the_delete_and_refuses_the_rest() {
        // The uri-less case: `classify` fails closed to Stream, so the library actions
        // are refusals with reasons rather than rows that dead-end on a missing uri.
        let t = target(TargetKind::Stream, Origin::Queue { pos: 2 }, None);
        assert_eq!(
            shape(&t),
            vec![
                (MenuItem::Play, false),
                (MenuItem::GoToAlbum, true),
                (MenuItem::GoToArtist, true),
                (MenuItem::Radio, true),
                (MenuItem::Favorite, true),
                (MenuItem::RemoveFromQueue, false),
            ]
        );
        let rows = rows_for(&t);
        assert_eq!(rows[1].avail, Avail::No("a stream has no library album"));
        assert_eq!(rows[4].avail, Avail::No("a stream has no library track to star"));
    }

    #[test]
    fn a_queue_stream_with_an_icy_artist_can_still_search_for_it() {
        let mut t = target(
            TargetKind::Stream,
            Origin::Queue { pos: 0 },
            Some("https://stream.example/x"),
        );
        t.artist = Some("Sun Ra".into());
        let rows = rows_for(&t);
        assert_eq!(
            rows[2].avail,
            Avail::Yes(MenuAction::GoToArtist(ArtistRef::Name("Sun Ra".into())))
        );
        assert_eq!(rows[3].avail, Avail::No("that row is a stream, can't start a radio"));
    }

    #[test]
    fn an_album_dir_opens_plays_adds_seeds_and_stars_but_has_no_album_of_its_own() {
        let t = target(TargetKind::Album, Origin::Browse, Some("album/7"));
        assert_eq!(
            shape(&t),
            vec![
                (MenuItem::OpenContents, false),
                (MenuItem::Play, false),
                (MenuItem::Enqueue, false),
                // Blocked, not absent: an album HAS an artist, this listing just does
                // not carry one yet (the daemon slice makes it live).
                (MenuItem::GoToArtist, true),
                (MenuItem::Radio, false),
                (MenuItem::Favorite, false),
            ]
        );
        let rows = rows_for(&t);
        assert_eq!(rows[1].avail, Avail::Yes(MenuAction::Play(PlayHow::Enqueue("album/7".into()))));
        assert_eq!(rows[2].avail, Avail::Yes(MenuAction::Enqueue("album/7".into())));
        assert_eq!(rows[4].avail, Avail::Yes(MenuAction::Radio("album/7".into())));
    }

    #[test]
    fn an_album_row_that_carries_an_artist_uri_jumps_rather_than_searching() {
        // The door the daemon slice walks through: nothing here changes, the label
        // flips itself from "(search)" to a plain jump.
        let mut t = target(TargetKind::Album, Origin::Browse, Some("album/7"));
        t.artist_uri = Some("artist/3".into());
        t.artist = Some("Alice Coltrane".into());
        let rows = rows_for(&t);
        assert_eq!(
            rows[3].avail,
            Avail::Yes(MenuAction::GoToArtist(ArtistRef::Uri("artist/3".into())))
        );
        assert_eq!(rows[3].label, "go to artist");
    }

    #[test]
    fn a_drilled_song_row_reaches_its_album_and_artist() {
        let mut t = target(TargetKind::LibrarySong, Origin::Browse, Some("song/1"));
        t.album_uri = Some("album/7".into());
        t.artist = Some("C418".into());
        assert_eq!(
            shape(&t),
            vec![
                (MenuItem::Play, false),
                (MenuItem::Enqueue, false),
                (MenuItem::GoToAlbum, false),
                (MenuItem::GoToArtist, false),
                (MenuItem::Radio, false),
                (MenuItem::Favorite, false),
            ]
        );
    }

    #[test]
    fn a_smart_list_dir_opens_and_enqueues_but_cannot_seed_a_radio() {
        let t = target(TargetKind::Dir, Origin::Browse, Some("list/newest"));
        assert_eq!(
            shape(&t),
            vec![
                (MenuItem::OpenContents, false),
                (MenuItem::Play, false),
                (MenuItem::Enqueue, false),
                (MenuItem::Radio, true),
            ]
        );
        assert_eq!(rows_for(&t)[3].avail, Avail::No("can't start a radio from a list"));
    }

    #[test]
    fn a_playlist_row_loads_rather_than_opening_or_seeding() {
        let t = target(TargetKind::Playlist, Origin::Browse, Some("Starred"));
        assert_eq!(
            shape(&t),
            vec![
                (MenuItem::OpenContents, true),
                (MenuItem::Play, false),
                (MenuItem::Radio, true),
            ]
        );
        let rows = rows_for(&t);
        assert_eq!(rows[0].avail, Avail::No("a playlist has no browse level - load it instead"));
        assert_eq!(rows[1].label, "load into queue", "loading a playlist is not playing now");
        assert_eq!(
            rows[1].avail,
            Avail::Yes(MenuAction::Play(PlayHow::LoadPlaylist("Starred".into())))
        );
        assert_eq!(rows[2].avail, Avail::No("can't start a radio from a playlist"));
    }

    #[test]
    fn an_artist_hit_opens_and_seeds_but_refuses_the_enqueue_and_the_star() {
        let t = target(TargetKind::Artist, Origin::FindHit, Some("artist/3"));
        assert_eq!(
            shape(&t),
            vec![
                (MenuItem::OpenContents, false),
                (MenuItem::Play, true),
                (MenuItem::Enqueue, true),
                (MenuItem::Radio, false),
                (MenuItem::Favorite, true),
            ]
        );
        let rows = rows_for(&t);
        assert_eq!(rows[1].avail, Avail::No("an artist can't be enqueued - open it"));
        assert_eq!(rows[4].avail, Avail::No("the star surface takes songs and albums only"));
    }

    #[test]
    fn a_station_hit_plays_and_adds_but_has_nothing_to_open_or_seed() {
        let t = target(
            TargetKind::Station,
            Origin::FindHit,
            Some("station/Moon Mission Recordings, Tokyo Deep and Electronic"),
        );
        assert_eq!(
            shape(&t),
            vec![
                (MenuItem::OpenContents, true),
                (MenuItem::Play, false),
                (MenuItem::Enqueue, false),
                (MenuItem::Radio, true),
            ]
        );
        let rows = rows_for(&t);
        assert_eq!(rows[0].avail, Avail::No("a station has nothing to open - play it instead"));
        assert_eq!(
            rows[3].avail,
            Avail::No("a saved station is a stream, not a library seed - play it instead")
        );
    }

    #[test]
    fn the_playing_library_song_offers_no_play_or_add_at_all() {
        let mut t = target(TargetKind::LibrarySong, Origin::NowPlaying, Some("song/1"));
        t.album_uri = Some("album/7".into());
        t.artist = Some("C418".into());
        assert_eq!(
            shape(&t),
            vec![
                (MenuItem::GoToAlbum, false),
                (MenuItem::GoToArtist, false),
                (MenuItem::Radio, false),
                (MenuItem::Favorite, false),
            ],
            "playing it again is not an action"
        );
    }

    #[test]
    fn a_recognized_stream_stars_and_seeds_from_the_matched_track() {
        let mut t = target(
            TargetKind::Stream,
            Origin::NowPlaying,
            Some("https://stream-relay.ntslive.net/1"),
        );
        t.match_uri = Some("song/42".into());
        t.artist = Some("Floating Points".into());
        let rows = rows_for(&t);
        assert_eq!(
            shape(&t),
            vec![
                (MenuItem::GoToAlbum, true),
                (MenuItem::GoToArtist, false),
                (MenuItem::Radio, false),
                (MenuItem::Favorite, false),
            ]
        );
        assert_eq!(
            rows[0].avail,
            Avail::No("the matched track's album is not on the wire yet")
        );
        assert_eq!(rows[2].avail, Avail::Yes(MenuAction::Radio("song/42".into())));
        assert_eq!(rows[3].avail, Avail::Yes(MenuAction::Favorite("song/42".into())));
        assert_eq!(rows[3].label, "star the matched track", "the star names its real subject");
    }

    #[test]
    fn an_unrecognized_stream_refuses_everything_with_a_reason() {
        let t = target(
            TargetKind::Stream,
            Origin::NowPlaying,
            Some("https://stream-relay.ntslive.net/1"),
        );
        assert!(
            rows_for(&t).iter().all(|r| matches!(r.avail, Avail::No(_))),
            "nothing is live, and nothing is silent"
        );
    }

    #[test]
    fn rows_are_in_canonical_order_with_no_duplicate_item() {
        // The ONE ordering guard: rows are ordered by construction, so this is what
        // pins it (there is deliberately no hand-written MenuItem::ALL to disagree).
        let fixtures = every_fixture();
        for t in &fixtures {
            let rows = rows_for(t);
            let mut prev: Option<usize> = None;
            for r in &rows {
                if let Some(p) = prev {
                    assert!(
                        r.item.ord() > p,
                        "{:?} rows are out of canonical order at {:?}",
                        t.kind,
                        r.item
                    );
                }
                prev = Some(r.item.ord());
            }
        }
    }

    #[test]
    fn every_blocked_row_says_why_and_every_live_row_names_itself() {
        for t in &every_fixture() {
            for r in rows_for(t) {
                assert!(!r.label.is_empty(), "{:?} has an unlabelled row", t.kind);
                if let Avail::No(why) = r.avail {
                    assert!(!why.is_empty(), "{:?}/{:?} blocks with no reason", t.kind, r.item);
                }
            }
        }
    }

    #[test]
    fn hotkeys_are_unique_and_never_a_menu_control_key() {
        // The popup's own keys must stay reachable: a hotkey that collided with `j`
        // would make the menu unnavigable on the row that offers it.
        const CONTROLS: [char; 6] = ['j', 'k', 'g', 'G', 'q', 'l'];
        let items = [
            MenuItem::OpenContents,
            MenuItem::Play,
            MenuItem::Enqueue,
            MenuItem::GoToAlbum,
            MenuItem::GoToArtist,
            MenuItem::Radio,
            MenuItem::Favorite,
            MenuItem::RemoveFromQueue,
        ];
        let mut seen: Vec<char> = Vec::new();
        for i in items {
            let k = i.hotkey();
            assert!(!seen.contains(&k), "hotkey {k:?} is claimed twice");
            assert!(!CONTROLS.contains(&k), "hotkey {k:?} collides with a menu control key");
            seen.push(k);
        }
        // And the ord() ordering is a bijection onto 0..8, so no two items can render
        // at the same position.
        let mut ords: Vec<usize> = items.iter().map(|i| i.ord()).collect();
        ords.sort_unstable();
        assert_eq!(ords, (0..items.len()).collect::<Vec<_>>());
    }

    #[test]
    fn every_menu_action_variant_is_produced_by_some_fixture() {
        // Dispatch is exhaustive over MenuAction, so an action no fixture builds is an
        // untested dispatch arm. This is the coverage pin.
        let mut seen: Vec<MenuItem> = Vec::new();
        let mut load_playlist = false;
        let mut artist_by_uri = false;
        let mut artist_by_name = false;
        for t in &every_fixture() {
            for r in rows_for(t) {
                if let Avail::Yes(a) = &r.avail {
                    if !seen.contains(&r.item) {
                        seen.push(r.item);
                    }
                    match a {
                        MenuAction::Play(PlayHow::LoadPlaylist(_)) => load_playlist = true,
                        MenuAction::GoToArtist(ArtistRef::Uri(_)) => artist_by_uri = true,
                        MenuAction::GoToArtist(ArtistRef::Name(_)) => artist_by_name = true,
                        _ => {}
                    }
                }
            }
        }
        assert_eq!(seen.len(), 8, "an item is never produced live: {seen:?}");
        assert!(load_playlist && artist_by_uri && artist_by_name);
    }

    /// One target per row of the design's contents table, so the ordering / reason /
    /// coverage properties above are checked against the whole surface at once.
    fn every_fixture() -> Vec<Target> {
        let song_in_queue = {
            let mut t = target(TargetKind::LibrarySong, Origin::Queue { pos: 3 }, Some("song/1"));
            t.album_uri = Some("album/7".into());
            t.artist = Some("Alice Coltrane".into());
            t
        };
        let stream_in_queue = target(TargetKind::Stream, Origin::Queue { pos: 1 }, None);
        let album_dir = target(TargetKind::Album, Origin::Browse, Some("album/7"));
        let album_dir_with_artist = {
            let mut t = target(TargetKind::Album, Origin::Browse, Some("album/7"));
            t.artist_uri = Some("artist/3".into());
            t
        };
        let drilled_song = {
            let mut t = target(TargetKind::LibrarySong, Origin::Browse, Some("song/2"));
            t.album_uri = Some("album/7".into());
            t.artist = Some("C418".into());
            t
        };
        let list_dir = target(TargetKind::Dir, Origin::Browse, Some("list/newest"));
        let playlist = target(TargetKind::Playlist, Origin::Browse, Some("Starred"));
        let artist_hit = target(TargetKind::Artist, Origin::FindHit, Some("artist/3"));
        let station_hit = target(TargetKind::Station, Origin::FindHit, Some("station/NTS 1"));
        let playing_song = {
            let mut t = target(TargetKind::LibrarySong, Origin::NowPlaying, Some("song/9"));
            t.album_uri = Some("album/7".into());
            t.artist = Some("C418".into());
            t
        };
        let playing_match = {
            let mut t =
                target(TargetKind::Stream, Origin::NowPlaying, Some("https://nts.example/1"));
            t.match_uri = Some("song/42".into());
            t
        };
        let playing_raw =
            target(TargetKind::Stream, Origin::NowPlaying, Some("https://nts.example/1"));
        vec![
            song_in_queue,
            stream_in_queue,
            album_dir,
            album_dir_with_artist,
            drilled_song,
            list_dir,
            playlist,
            artist_hit,
            station_hit,
            playing_song,
            playing_match,
            playing_raw,
        ]
    }
}
