//! Pure parsing of `status` + `currentsong` + `playlistinfo` pairs into structured
//! values. Model-free: just the parse half, no text formatting (the CLI card and
//! queue text formatters live in hypodj-cli/render.rs on top of this). The server
//! emits only a known subset of keys - there is NO `elapsed` and NO `time` key.

/// The now-playing state assembled from `status` + `currentsong` pairs. Fully
/// Option-typed - the server may omit any field.
#[derive(Debug, Default, PartialEq)]
pub struct NowPlaying {
    pub state: Option<String>, // "play" / "pause" / "stop"
    pub volume: Option<i32>,   // -1 or absent => unknown, hidden
    pub playlistlength: Option<usize>,
    pub song: Option<usize>,   // 0-based index of current
    pub duration: Option<f64>, // library songs only
    pub title: Option<String>,
    /// The station / show NAME for a raw stream, from the `currentsong` `Name` pair
    /// (real MPD radio convention: `Name:` = station, `Title:` = now-playing). Present
    /// for a stream the daemon named - via live ICY icy-name, or a resolved station
    /// identity such as an NTS mixtape title (task lq54isr) - and `None` for a library
    /// song or an unnamed stream. Lets the dj-gui show the station name instead of the
    /// bare URL.
    pub name: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// The playing track's album as a BROWSE uri (`album/<id>`), from the `currentsong`
    /// `X-AlbumUri` pair the daemon already emits via `push_song_tags` - the same pair
    /// `playlistinfo` carries per queue row. `album` above is the album's NAME, which is
    /// a display string; this is the handle a client can navigate with. `None` for a raw
    /// stream (no library album).
    pub album_uri: Option<String>,
    /// The current song's uri from `currentsong` `file` (`song/<id>` for a library
    /// track, an `http(s)://...` URL for a raw stream). Needed to favorite the
    /// current track (`playlistadd Starred <uri>`); a stream has no star surface.
    pub file: Option<String>,
    /// True when the current track is a Subsonic favorite (the daemon emits the
    /// non-standard `X-Starred` pair on `currentsong`), so the clients can show a
    /// heart. Parsed from `currentsong`, coexisting with the `armed` status pairs.
    pub starred: bool,
    /// A RECOGNIZED remote cover URL for the current raw stream, surfaced by the
    /// daemon as the non-standard `X-CoverArt` pair on `currentsong` (task
    /// kmrhj8m). Present ONLY for a stream whose songrec identify yielded a cover;
    /// a library song and a coverless stream leave it `None`. The tui uses it as
    /// the second half of the art-request key so a stream lights the now-playing
    /// art pane and a re-identify refetches, while the bytes still arrive over the
    /// plain MPD albumart protocol (the client gains no HTTP dependency).
    pub cover: Option<String>,
    /// The library counterpart of a RECOGNIZED radio track, as a `song/<id>` uri, from
    /// the `currentsong` `X-MatchUri` extension (task g96g064). Present only for a raw
    /// stream whose songrec identify matched a song the user actually owns. `file:` stays
    /// the stream url - this is a SEPARATE field precisely because the playing entry is
    /// never rewritten - so it is the one handle a client has for starring what the radio
    /// is playing.
    pub match_uri: Option<String>,
    /// The armed human-features, surfaced by the daemon as X- status pairs and
    /// present ONLY when armed. Startle-safe equals trust only if the machine's
    /// hold on the night is VISIBLE - these back that render.
    pub armed: ArmedFeatures,
    /// The active latent-field pulls, surfaced by the daemon as X- status pairs and
    /// present ONLY while a pull is active. Backs the passive "see the field" HUD:
    /// an inspectable, decaying magnetism map is what makes the nondeterministic
    /// field trustworthy.
    pub field: FieldState,
    /// The single most-pertinent ambient context hint - the "btw, DJ knows" surface.
    /// Present ONLY when the daemon emits a just-finished or up-next hint pair; the
    /// currently-playing case is suppressed at the daemon (the pane already shows it),
    /// so `Some` here always names something the pane does NOT, and a lean status
    /// leaves it `None`.
    pub hint: Option<AmbientHint>,
    /// The end-of-queue CONTINUATION station, present ONLY when continuation is ARMED
    /// (the daemon emits `X-hypodj-continuation: on` + `X-hypodj-continuation-station`).
    /// `Some(station)` warrants a standing "then: <station>" queue-tail hint - the
    /// future made visible BEFORE the drain handoff; `None` (disarmed / unconfigured)
    /// renders nothing, keeping a lean status silent exactly like the armed/hint HUD.
    pub continuation: Option<String>,
    /// The OFFLINE STORE in one already-rendered sentence, from the daemon's `X-Store`
    /// status pair - e.g. `318/347 tracks, 12.1/16.0 GiB, waiting (playback-remote)` or
    /// `complete, 347 tracks, 9.8 GiB`.
    ///
    /// Rendered daemon-side on purpose: the numbers and the reasons live there, and one
    /// formatter means `dj status` and the dj-gui badge can never disagree about the
    /// same mirror. This is the ONLY passive window a client has into a backfill that
    /// runs for days - without it "is it running, stuck, or done?" is answerable only by
    /// opening a socket and typing `store` by hand, because `store` is deliberately
    /// absent from the `commands` advertisement.
    ///
    /// `None` with no store configured, and also until the reconciler has completed one
    /// full pass with an authoritative pin set - a row of zeros would read as "the
    /// mirror is empty", which is a different claim from "nobody has looked yet".
    pub store: Option<String>,
    /// How many times the SERVER records this user playing the current track, from the
    /// daemon's non-standard `X-Plays` pair on `currentsong`.
    ///
    /// `None` means the server has NO play record, which is a different claim from zero
    /// and stays different here: the daemon omits the pair rather than sending a `0`,
    /// so an un-played track and an old daemon both read as `None` and neither invents
    /// a count.
    pub plays: Option<u32>,
    /// Whole days since the server last recorded this user playing the current track,
    /// from the daemon's `X-LastPlayed` pair. Rendered daemon-side in DAYS rather than
    /// as a stamp, so one formatter serves every client and the date arithmetic has one
    /// implementation. `None` when there is no play record.
    pub last_played_days: Option<u32>,
}

/// One active pull, reconstructed from the daemon's `X-hypodj-field-{i}-*` pairs.
/// `strength` is a basis-of-100 integer (the wire value); render as `strength/100`.
#[derive(Debug, Default, PartialEq, Clone)]
pub struct FieldPull {
    /// The pull label - the matched lexicon token(s), e.g. `calmer` or `less energy`.
    pub label: String,
    /// Decayed strength as an integer 0..=100 (the wire basis-of-100 value).
    pub strength: u8,
    /// Whole minutes since the pull was born/reinforced.
    pub age_mins: u64,
}

/// The active latent-field, parsed from the daemon's `X-hypodj-field-*` status
/// pairs. Empty when no pull is active, so a lean status leaves this empty and the
/// clients render nothing.
#[derive(Debug, Default, PartialEq, Clone)]
pub struct FieldState {
    /// The live pulls in insertion order (most recent last).
    pub pulls: Vec<FieldPull>,
}

impl FieldState {
    /// `true` when at least one pull is active - the HUD render gate.
    pub fn active(&self) -> bool {
        !self.pulls.is_empty()
    }

    fn parse(status: &[(String, String)]) -> Self {
        let count = find(status, "X-hypodj-field-count")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        let mut pulls = Vec::new();
        for i in 0..count {
            // Skip any index missing a key (defensive against a torn snapshot).
            let label = match find(status, &format!("X-hypodj-field-{i}-label")) {
                Some(l) => l.to_string(),
                None => continue,
            };
            let strength = match find(status, &format!("X-hypodj-field-{i}-strength"))
                .and_then(|v| v.parse::<u8>().ok())
            {
                Some(s) => s,
                None => continue,
            };
            let age_mins = match find(status, &format!("X-hypodj-field-{i}-age"))
                .and_then(|v| v.parse::<u64>().ok())
            {
                Some(a) => a,
                None => continue,
            };
            pulls.push(FieldPull { label, strength, age_mins });
        }
        FieldState { pulls }
    }
}

/// Which pertinence branch the daemon's ambient hint resolved to. Mirrors the
/// daemon-side seed ordering; the currently-playing branch is never on the wire (it
/// is suppressed at the daemon so the pane is not duplicated). The enum is the future
/// extension point (a selected / time-of-day / ask variant).
#[derive(Debug, PartialEq, Clone)]
pub enum HintKind {
    /// The track that just finished - the recency seed, the most in-the-face string.
    JustFinished,
    /// The first queued song when nothing has played yet.
    UpNext,
}

/// The single most-pertinent context string, parsed from the daemon's
/// `X-hypodj-hint-*` status pairs. Both `kind` and `title` are required - a torn
/// snapshot missing either yields `None`, never a half-guessed hint.
#[derive(Debug, PartialEq, Clone)]
pub struct AmbientHint {
    pub kind: HintKind,
    pub title: String,
}

impl AmbientHint {
    fn parse(status: &[(String, String)]) -> Option<Self> {
        let kind = match find(status, "X-hypodj-hint-kind")? {
            "just-finished" => HintKind::JustFinished,
            "up-next" => HintKind::UpNext,
            // An unknown or torn kind token yields nothing, never a guess.
            _ => return None,
        };
        // Both keys required: a snapshot with a kind but no title is torn -> None.
        Some(AmbientHint { kind, title: find(status, "X-hypodj-hint-title")?.to_string() })
    }

    /// The bare phrase for the terse card HUD (no "btw" prefix), matching the
    /// `sleep 12m` / `toward calmer 0.58 3m` register.
    pub fn phrase(&self) -> String {
        match self.kind {
            HintKind::JustFinished => format!("just finished {}", self.title),
            HintKind::UpNext => format!("up next {}", self.title),
        }
    }
}

/// The armed sleep / wind-down / wake state parsed from the daemon's X- status
/// pairs. Every field is `None`/`false` when nothing is armed, so a lean status
/// leaves this empty and the clients render nothing.
#[derive(Debug, Default, PartialEq, Clone)]
pub struct ArmedFeatures {
    /// Seconds until the sleep fade-to-stop fires (`X-hypodj-sleep-remaining`).
    pub sleep_remaining: Option<u64>,
    /// A wind-down plan is armed (`X-hypodj-winddown-active`).
    pub winddown_active: bool,
    /// Seconds until a scheduled wind-down fires (`X-hypodj-winddown-remaining`);
    /// absent for an immediate wind-down.
    pub winddown_remaining: Option<u64>,
    /// Seconds until the scheduled wake alarm (`X-hypodj-wake-remaining`).
    pub wake_remaining: Option<u64>,
    /// The wake alarm as a unix epoch second (`X-hypodj-wake-at`).
    pub wake_at: Option<u64>,
}

impl ArmedFeatures {
    /// `true` if any feature is armed - the render gate.
    pub fn any(&self) -> bool {
        self.sleep_remaining.is_some()
            || self.winddown_active
            || self.wake_remaining.is_some()
    }

    fn parse(status: &[(String, String)]) -> Self {
        let num = |k: &str| find(status, k).and_then(|v| v.parse::<u64>().ok());
        ArmedFeatures {
            sleep_remaining: num("X-hypodj-sleep-remaining"),
            winddown_active: find(status, "X-hypodj-winddown-active").is_some(),
            winddown_remaining: num("X-hypodj-winddown-remaining"),
            wake_remaining: num("X-hypodj-wake-remaining"),
            wake_at: num("X-hypodj-wake-at"),
        }
    }
}

/// The armed continuation station, parsed from the daemon's `X-hypodj-continuation`
/// pairs. `Some(station)` ONLY when the toggle pair reads `on` AND a non-empty station
/// accompanies it - a torn/half snapshot (toggle without a station, or vice versa)
/// yields `None`, never a guessed station.
fn parse_continuation(status: &[(String, String)]) -> Option<String> {
    if find(status, "X-hypodj-continuation") != Some("on") {
        return None;
    }
    match find(status, "X-hypodj-continuation-station") {
        Some(s) if !s.trim().is_empty() => Some(s.to_string()),
        _ => None,
    }
}

fn find<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

pub fn now_playing(status: &[(String, String)], current: &[(String, String)]) -> NowPlaying {
    NowPlaying {
        state: find(status, "state").map(str::to_string),
        volume: find(status, "volume").and_then(|v| v.parse::<i32>().ok()),
        playlistlength: find(status, "playlistlength").and_then(|v| v.parse().ok()),
        song: find(status, "song").and_then(|v| v.parse().ok()),
        duration: find(status, "duration").and_then(|v| v.parse().ok()),
        title: find(current, "Title").map(str::to_string),
        name: find(current, "Name").map(str::to_string),
        artist: find(current, "Artist").map(str::to_string),
        album: find(current, "Album").map(str::to_string),
        album_uri: find(current, "X-AlbumUri").map(str::to_string),
        file: find(current, "file").map(str::to_string),
        starred: find(current, "X-Starred").is_some(),
        cover: find(current, "X-CoverArt").map(str::to_string),
        match_uri: find(current, "X-MatchUri").map(str::to_string),
        armed: ArmedFeatures::parse(status),
        field: FieldState::parse(status),
        hint: AmbientHint::parse(status),
        continuation: parse_continuation(status),
        store: find(status, "X-Store").map(str::to_string),
        // Absent pair -> None, never a fabricated zero: the daemon emits these only
        // when the server actually has a record.
        plays: find(current, "X-Plays").and_then(|v| v.parse().ok()),
        last_played_days: find(current, "X-LastPlayed").and_then(|v| v.parse().ok()),
    }
}

/// The offline-store line as a COMPACT badge: everything up to the first comma, plus
/// any of the trailing clauses that report something is wrong or held ("waiting",
/// "deferred", "given up"). So a healthy mirror reads `318/347 tracks` or `complete`
/// and a held one reads `318/347 tracks, waiting (playback-remote)` - the size and
/// budget figures, which only matter when he is actually asking, stay in the full
/// `dj status` line.
///
/// Split on the daemon's own comma-joined shape rather than re-deriving anything, so
/// the two renders cannot drift apart. `None` when there is no store line at all.
pub fn store_badge(line: &str) -> Option<String> {
    let mut parts = line.split(", ");
    let head = parts.next()?.trim();
    if head.is_empty() {
        return None;
    }
    let mut out = head.to_string();
    for p in parts {
        let p = p.trim();
        if p.starts_with("waiting") || p.ends_with("deferred") || p.ends_with("given up") {
            out.push_str(", ");
            out.push_str(p);
        }
    }
    Some(out)
}

/// Format a `secs` remaining as a compact human-readable string: `Hh MMm`, `MMm`,
/// or `Ss`. Used by both clients so the armed-feature render reads consistently.
pub fn fmt_remaining(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{s}s")
    }
}

/// One entry in the queue parsed from a `playlistinfo` block. `pos` is the 0-based
/// MPD `Pos` (fall back to the block index if absent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueItem {
    pub pos: usize,
    pub title: String,
    pub artist: Option<String>,
    /// The row's uri from the block `file` key (`song/<id>` for a library track,
    /// an `http(s)://...` URL for a raw stream). Needed to favorite the SELECTED
    /// row (`playlistadd Starred <uri>`); a stream has no star surface.
    pub uri: Option<String>,
    /// The album browse uri (`album/<id>`) from the non-standard `X-AlbumUri` pair
    /// the daemon emits per library song, so the TUI can group the queue by album
    /// for the browse queue markers. `None` for a raw stream (no album).
    pub album_uri: Option<String>,
}

/// Parse the flat `playlistinfo` pair list into structured queue items. Each entry
/// begins at a `file` key; group by that boundary and pull Pos/Title/Artist.
pub fn parse_queue(pairs: &[(String, String)]) -> Vec<QueueItem> {
    group_blocks(pairs)
        .iter()
        .enumerate()
        .map(|(i, b)| QueueItem {
            pos: find(b, "Pos").and_then(|v| v.parse::<usize>().ok()).unwrap_or(i),
            title: find(b, "Title").unwrap_or("(unknown)").to_string(),
            artist: find(b, "Artist").map(str::to_string),
            uri: find(b, "file").map(str::to_string),
            album_uri: find(b, "X-AlbumUri").map(str::to_string),
        })
        .collect()
}

/// Split a flat pair list into per-song blocks, each beginning at a `file` key.
fn group_blocks(pairs: &[(String, String)]) -> Vec<Vec<(String, String)>> {
    let mut blocks: Vec<Vec<(String, String)>> = Vec::new();
    for (k, v) in pairs {
        if k == "file" {
            blocks.push(Vec::new());
        }
        if let Some(cur) = blocks.last_mut() {
            cur.push((k.clone(), v.clone()));
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn nowplaying_playing() {
        // Canned status WITHOUT elapsed/time (the server never emits them).
        let status = p(&[
            ("volume", "70"),
            ("playlistlength", "12"),
            ("state", "play"),
            ("song", "2"),
            ("duration", "215.000"),
        ]);
        let current = p(&[
            ("file", "song/42"),
            ("Title", "Blue in Green"),
            ("Artist", "Miles Davis"),
            ("Album", "Kind of Blue"),
            ("Pos", "2"),
            ("Id", "42"),
        ]);
        let np = now_playing(&status, &current);
        assert_eq!(np.state.as_deref(), Some("play"));
        assert_eq!(np.volume, Some(70));
        assert_eq!(np.playlistlength, Some(12));
        assert_eq!(np.song, Some(2));
        assert_eq!(np.duration, Some(215.0));
        assert_eq!(np.title.as_deref(), Some("Blue in Green"));
        assert_eq!(np.artist.as_deref(), Some("Miles Davis"));
        assert_eq!(np.album.as_deref(), Some("Kind of Blue"));
        assert_eq!(np.file.as_deref(), Some("song/42"));
        // No armed X- pairs -> nothing armed.
        assert!(!np.armed.any());
        // No X-Starred pair -> not a favorite.
        assert!(!np.starred);
    }

    #[test]
    fn nowplaying_parses_stream_name_pair() {
        // A raw stream renders with `Name` (station/show) alongside `Title` (real MPD
        // radio convention). The client must parse `Name` into `np.name` so the dj-gui
        // can show the station name instead of the bare URL (task lq54isr).
        let status = p(&[("state", "play"), ("playlistlength", "1"), ("song", "0")]);
        let current = p(&[
            ("file", "https://stream-mixtape-geo.ntslive.net/mixtape5"),
            ("Title", "https://stream-mixtape-geo.ntslive.net/mixtape5"),
            ("Name", "4 To The Floor"),
            ("X-CoverArt", "https://media.ntslive.co.uk/resize/400x400/ftf.jpeg"),
            ("Pos", "0"),
            ("Id", "1"),
        ]);
        let np = now_playing(&status, &current);
        assert_eq!(np.name.as_deref(), Some("4 To The Floor"), "the station Name is parsed");
        assert_eq!(
            np.cover.as_deref(),
            Some("https://media.ntslive.co.uk/resize/400x400/ftf.jpeg"),
            "the stream cover still parses alongside Name"
        );
        // A library song (no Name pair) leaves np.name None.
        let lib = now_playing(&status, &p(&[("file", "song/1"), ("Title", "T")]));
        assert!(lib.name.is_none(), "a library song carries no station Name");
    }

    #[test]
    fn nowplaying_parses_the_servers_play_history_and_never_invents_it() {
        // The daemon emits these ONLY when the server has a record, so an absent pair
        // must stay `None` here rather than becoming a zero. "Never played" and
        // "played zero times just now" are different claims, and an old daemon that
        // sends neither must not be reported as a library nobody has ever listened to.
        let current = p(&[
            ("file", "song/42"),
            ("Title", "Blue in Green"),
            ("X-Plays", "23"),
            ("X-LastPlayed", "3"),
        ]);
        let np = now_playing(&[], &current);
        assert_eq!(np.plays, Some(23));
        assert_eq!(np.last_played_days, Some(3), "whole days, rendered daemon-side");

        let bare = now_playing(&[], &p(&[("file", "song/42"), ("Title", "T")]));
        assert_eq!(bare.plays, None, "an absent pair is NOT zero plays");
        assert_eq!(bare.last_played_days, None);

        // Independently optional, and a malformed value degrades to None rather than
        // to a fabricated number.
        let partial = now_playing(&[], &p(&[("file", "song/42"), ("X-Plays", "1")]));
        assert_eq!((partial.plays, partial.last_played_days), (Some(1), None));
        let junk = now_playing(&[], &p(&[("X-Plays", "lots"), ("X-LastPlayed", "-4")]));
        assert_eq!((junk.plays, junk.last_played_days), (None, None));
    }

    #[test]
    fn nowplaying_parses_x_starred_coexisting_with_armed() {
        // A starred current track WHILE a sleep timer is armed: the two X- sources
        // (currentsong X-Starred + status X-hypodj-*) must parse independently.
        let status = p(&[
            ("state", "play"),
            ("X-hypodj-sleep-remaining", "600"),
        ]);
        let current = p(&[
            ("file", "song/42"),
            ("Title", "Blue in Green"),
            ("X-Starred", "1"),
        ]);
        let np = now_playing(&status, &current);
        assert!(np.starred);
        assert!(np.armed.any());
        assert_eq!(np.armed.sleep_remaining, Some(600));
        // Absent pair -> not starred, armed untouched.
        let np2 = now_playing(&status, &p(&[("file", "song/7"), ("Title", "X")]));
        assert!(!np2.starred);
        assert!(np2.armed.any());
    }

    #[test]
    fn nowplaying_parses_x_coverart() {
        // A recognized stream cover surfaces as the currentsong X-CoverArt pair
        // (task kmrhj8m); its absence leaves cover None.
        let status = p(&[("state", "play")]);
        let current = p(&[
            ("file", "https://stream.example/live"),
            ("Title", "Some Artist - Some Track"),
            ("X-CoverArt", "https://is1.example/hq.jpg"),
        ]);
        let np = now_playing(&status, &current);
        assert_eq!(np.cover.as_deref(), Some("https://is1.example/hq.jpg"));
        // Absent pair -> no cover (a library song or a coverless stream).
        let np2 = now_playing(&status, &p(&[("file", "song/7"), ("Title", "X")]));
        assert_eq!(np2.cover, None);
    }

    #[test]
    fn nowplaying_parses_x_match_uri() {
        // A recognized stream that matched the library surfaces its counterpart as
        // X-MatchUri (task g96g064 phase 2), while `file` stays the stream url.
        let status = p(&[("state", "play")]);
        let current = p(&[
            ("file", "https://stream.example/live"),
            ("Title", "Some Track"),
            ("X-MatchUri", "song/s7"),
        ]);
        let np = now_playing(&status, &current);
        assert_eq!(np.match_uri.as_deref(), Some("song/s7"));
        assert_eq!(np.file.as_deref(), Some("https://stream.example/live"));
        // Absent pair -> no match uri (an unmatched stream or a library song).
        let np2 = now_playing(&status, &p(&[("file", "song/7"), ("Title", "X")]));
        assert_eq!(np2.match_uri, None);
    }

    #[test]
    fn nowplaying_parses_continuation_only_when_armed() {
        // Armed: on + a station name -> Some(station).
        let status = p(&[
            ("state", "stop"),
            ("X-hypodj-continuation", "on"),
            ("X-hypodj-continuation-station", "NTS 1"),
        ]);
        let np = now_playing(&status, &[]);
        assert_eq!(np.continuation.as_deref(), Some("NTS 1"));
        // No continuation pairs -> None (lean status, render nothing).
        let np = now_playing(&p(&[("state", "stop")]), &[]);
        assert_eq!(np.continuation, None);
        // Torn snapshot (toggle without a station) -> None, never a guess.
        let np = now_playing(&p(&[("X-hypodj-continuation", "on")]), &[]);
        assert_eq!(np.continuation, None);
        // A station name without an `on` toggle -> None.
        let np = now_playing(&p(&[("X-hypodj-continuation-station", "NTS 1")]), &[]);
        assert_eq!(np.continuation, None);
    }

    #[test]
    fn nowplaying_parses_armed_feature_pairs() {
        let status = p(&[
            ("volume", "70"),
            ("state", "play"),
            ("X-hypodj-sleep-remaining", "720"),
            ("X-hypodj-winddown-active", "1"),
            ("X-hypodj-wake-remaining", "25200"),
            ("X-hypodj-wake-at", "1750000000"),
        ]);
        let np = now_playing(&status, &[]);
        assert!(np.armed.any());
        assert_eq!(np.armed.sleep_remaining, Some(720));
        assert!(np.armed.winddown_active);
        assert_eq!(np.armed.winddown_remaining, None);
        assert_eq!(np.armed.wake_remaining, Some(25200));
        assert_eq!(np.armed.wake_at, Some(1750000000));
    }

    #[test]
    fn armed_absent_when_no_pairs() {
        let np = now_playing(&p(&[("state", "play")]), &[]);
        assert_eq!(np.armed, ArmedFeatures::default());
    }

    #[test]
    fn nowplaying_parses_field_pull_pairs() {
        let status = p(&[
            ("state", "play"),
            ("X-hypodj-field-count", "2"),
            ("X-hypodj-field-0-label", "calmer"),
            ("X-hypodj-field-0-strength", "58"),
            ("X-hypodj-field-0-age", "3"),
            ("X-hypodj-field-1-label", "warmer"),
            ("X-hypodj-field-1-strength", "41"),
            ("X-hypodj-field-1-age", "1"),
        ]);
        let np = now_playing(&status, &[]);
        assert!(np.field.active());
        assert_eq!(np.field.pulls.len(), 2);
        assert_eq!(np.field.pulls[0], FieldPull { label: "calmer".into(), strength: 58, age_mins: 3 });
        assert_eq!(np.field.pulls[1], FieldPull { label: "warmer".into(), strength: 41, age_mins: 1 });
    }

    #[test]
    fn field_absent_when_no_pairs() {
        let np = now_playing(&p(&[("state", "play")]), &[]);
        assert!(!np.field.active());
        assert_eq!(np.field, FieldState::default());
    }

    #[test]
    fn field_skips_torn_index_missing_key() {
        // A count of 2 but the second pull's strength pair is missing (a torn
        // snapshot): the incomplete index is skipped, never a garbage pull.
        let status = p(&[
            ("X-hypodj-field-count", "2"),
            ("X-hypodj-field-0-label", "calmer"),
            ("X-hypodj-field-0-strength", "58"),
            ("X-hypodj-field-0-age", "3"),
            ("X-hypodj-field-1-label", "warmer"),
            ("X-hypodj-field-1-age", "1"),
        ]);
        let np = now_playing(&status, &[]);
        assert_eq!(np.field.pulls.len(), 1);
        assert_eq!(np.field.pulls[0].label, "calmer");
    }

    #[test]
    fn nowplaying_parses_just_finished_hint() {
        let status = p(&[
            ("state", "stop"),
            ("X-hypodj-hint-kind", "just-finished"),
            ("X-hypodj-hint-title", "303 (Ninajirachi Remix)"),
        ]);
        let np = now_playing(&status, &[]);
        let hint = np.hint.expect("a just-finished hint");
        assert_eq!(hint.kind, HintKind::JustFinished);
        assert_eq!(hint.title, "303 (Ninajirachi Remix)");
        assert_eq!(hint.phrase(), "just finished 303 (Ninajirachi Remix)");
    }

    #[test]
    fn nowplaying_parses_up_next_hint() {
        let status = p(&[
            ("X-hypodj-hint-kind", "up-next"),
            ("X-hypodj-hint-title", "Blue in Green"),
        ]);
        let hint = now_playing(&status, &[]).hint.expect("an up-next hint");
        assert_eq!(hint.kind, HintKind::UpNext);
        assert_eq!(hint.phrase(), "up next Blue in Green");
    }

    #[test]
    fn hint_absent_when_no_pairs() {
        // A lean status (no hint pairs) leaves the hint None - the clients draw nothing.
        let np = now_playing(&p(&[("state", "play")]), &[]);
        assert_eq!(np.hint, None);
    }

    #[test]
    fn hint_skips_torn_snapshot_missing_title() {
        // A kind pair present but the title pair missing (a torn snapshot): parse
        // yields None, never a half-guessed hint (mirrors field_skips_torn_index).
        let status = p(&[("X-hypodj-hint-kind", "just-finished")]);
        assert_eq!(now_playing(&status, &[]).hint, None);
    }

    #[test]
    fn hint_rejects_unknown_kind_token() {
        // An unknown/future kind token yields nothing rather than a guess.
        let status = p(&[
            ("X-hypodj-hint-kind", "time-of-day"),
            ("X-hypodj-hint-title", "evening"),
        ]);
        assert_eq!(now_playing(&status, &[]).hint, None);
    }

    #[test]
    fn fmt_remaining_reads_human() {
        assert_eq!(fmt_remaining(45), "45s");
        assert_eq!(fmt_remaining(720), "12m");
        assert_eq!(fmt_remaining(25200), "7h 00m");
    }

    #[test]
    fn nowplaying_stopped_empty_current() {
        let status = p(&[("volume", "50"), ("playlistlength", "3"), ("state", "stop")]);
        let np = now_playing(&status, &[]);
        assert_eq!(np.state.as_deref(), Some("stop"));
        assert_eq!(np.title, None);
    }

    #[test]
    fn nowplaying_unknown_volume() {
        let status = p(&[("volume", "-1"), ("playlistlength", "1"), ("state", "play"), ("song", "0")]);
        let np = now_playing(&status, &[]);
        assert_eq!(np.volume, Some(-1));
    }

    #[test]
    fn parse_queue_pos_title_artist() {
        let pairs = p(&[
            ("file", "song/1"),
            ("Title", "One"),
            ("Artist", "A"),
            ("Pos", "0"),
            ("Id", "1"),
            ("file", "song/2"),
            ("Title", "Two"),
            ("Pos", "1"),
            ("Id", "2"),
        ]);
        let q = parse_queue(&pairs);
        assert_eq!(q.len(), 2);
        assert_eq!(
            q[0],
            QueueItem {
                pos: 0,
                title: "One".into(),
                artist: Some("A".into()),
                uri: Some("song/1".into()),
                album_uri: None,
            }
        );
        // Second block has no Artist -> None.
        assert_eq!(
            q[1],
            QueueItem {
                pos: 1,
                title: "Two".into(),
                artist: None,
                uri: Some("song/2".into()),
                album_uri: None,
            }
        );
    }

    #[test]
    fn parse_queue_reads_album_uri() {
        // The daemon's non-standard X-AlbumUri pair groups a queued song by album.
        let pairs = p(&[
            ("file", "song/1"),
            ("Title", "One"),
            ("X-AlbumUri", "album/al-9"),
            ("Pos", "0"),
            ("Id", "1"),
            // A stream row carries no X-AlbumUri -> None.
            ("file", "http://stream.example/live"),
            ("Title", "Live"),
            ("Pos", "1"),
            ("Id", "2"),
        ]);
        let q = parse_queue(&pairs);
        assert_eq!(q[0].album_uri.as_deref(), Some("album/al-9"));
        assert_eq!(q[1].album_uri, None);
    }

    #[test]
    fn now_playing_reads_the_current_songs_album_uri() {
        // The SAME pair `playlistinfo` carries per row: `push_song_tags` emits it on
        // `currentsong` too, and the client simply never read it. `Album` is the album's
        // NAME (a display string); this is the handle a client can navigate with.
        let current = p(&[
            ("file", "song/1"),
            ("Title", "Sweden"),
            ("Album", "Volume Alpha"),
            ("X-AlbumUri", "album/al-9"),
        ]);
        let np = now_playing(&[], &current);
        assert_eq!(np.album.as_deref(), Some("Volume Alpha"), "still the name");
        assert_eq!(np.album_uri.as_deref(), Some("album/al-9"), "and now the uri");
        // A raw stream carries no album at all -> None, never a guessed one.
        let stream = p(&[("file", "http://stream.example/live"), ("Title", "Live")]);
        assert_eq!(now_playing(&[], &stream).album_uri, None);
    }

    #[test]
    fn parse_queue_empty() {
        assert!(parse_queue(&[]).is_empty());
    }

    #[test]
    fn now_playing_carries_the_offline_store_line() {
        // The daemon has emitted `X-Store` on `status` all along and no client read it,
        // so a multi-day starred backfill was invisible from `dj` and `dj-gui` alike -
        // reachable only by opening a socket and typing `store`, which is deliberately
        // not in the `commands` advertisement. This is that wire, and a status without
        // the pair (no store, or no full pass yet) must stay None rather than invent a
        // row of zeros.
        let np = now_playing(&p(&[("X-Store", "318/347 tracks, 12.1/16.0 GiB")]), &[]);
        assert_eq!(np.store.as_deref(), Some("318/347 tracks, 12.1/16.0 GiB"));
        assert_eq!(now_playing(&[], &[]).store, None);
    }

    #[test]
    fn the_store_badge_keeps_the_headline_and_every_reason_it_is_held() {
        // The badge is the compact half of the same sentence: the headline count always,
        // plus anything that says the mirror is HELD - because "stuck" and "slow" looking
        // identical is the exact failure the status surface exists to eliminate. The size
        // and budget figures only matter when he is asking, so they stay in `dj status`.
        assert_eq!(
            store_badge("318/347 tracks, 12.1/16.0 GiB").as_deref(),
            Some("318/347 tracks"),
        );
        assert_eq!(store_badge("complete, 347 tracks, 9.8 GiB").as_deref(), Some("complete"));
        assert_eq!(
            store_badge("318/347 tracks, 12.1/16.0 GiB, waiting (playback-remote), 3 deferred, 2 given up")
                .as_deref(),
            Some("318/347 tracks, waiting (playback-remote), 3 deferred, 2 given up"),
        );
        // Total over anything the daemon might say, including nothing.
        assert_eq!(store_badge(""), None);
        assert_eq!(store_badge("starting").as_deref(), Some("starting"));
    }
}
