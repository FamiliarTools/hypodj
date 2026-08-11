//! Internal domain model.
//!
//! FOUNDATION. These are *our* types, decoupled from the wire types of the
//! `opensubsonic` crate. The `subsonic` module maps wire -> these. Keeping this
//! boundary means the rest of the daemon (player, mpd server, cache) never
//! depends on the exact shape of a third-party crate's structs.

/// Opaque server-side id for a song/album/artist. Kept as a newtype so we can
/// never accidentally cross-use an album id where a song id is expected.
///
/// `Serialize`/`Deserialize` so the P2 plan IR ([`crate::plan`]) can carry a
/// concrete song id in a `Selector` (append-only enqueue) across the wire.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SongId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AlbumId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ArtistId(pub String);

/// A single favoritable entity. This is the ONE authority for the favorite uri
/// scheme (`song/<id>` | `album/<id>` | `artist/<id>`), the routing of a star
/// gesture to the right Subsonic wire slice, and (future P4) a serializable
/// listening-intelligence signal.
///
/// The uri PREFIX carries the entity kind, so a `playlistadd Starred <uri>` can
/// never mis-target the wrong bucket: `song/` stars a song, `album/` an album,
/// `artist/` an artist, and anything else parses to `None` (a loud ACK, not a
/// silent no-op).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Favorite {
    Song(SongId),
    Album(AlbumId),
    Artist(ArtistId),
}

impl Favorite {
    /// The browse/gesture uri for this favorite (`song/<id>` etc.). Inverse of
    /// [`Favorite::from_uri`].
    pub fn uri(&self) -> String {
        match self {
            Favorite::Song(id) => format!("song/{}", id.0),
            Favorite::Album(id) => format!("album/{}", id.0),
            Favorite::Artist(id) => format!("artist/{}", id.0),
        }
    }

    /// Parse a favorite uri. The single parse site for star routing: the prefix
    /// is the sole routing authority. An unknown or prefixless uri yields `None`.
    pub fn from_uri(uri: &str) -> Option<Favorite> {
        if let Some(id) = uri.strip_prefix("song/") {
            Some(Favorite::Song(SongId(id.to_string())))
        } else if let Some(id) = uri.strip_prefix("album/") {
            Some(Favorite::Album(AlbumId(id.to_string())))
        } else if let Some(id) = uri.strip_prefix("artist/") {
            Some(Favorite::Artist(ArtistId(id.to_string())))
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
pub struct Artist {
    pub id: ArtistId,
    pub name: String,
    /// Number of albums. The wire type (`ArtistId3.album_count`) is
    /// `Option<i64>`; the `subsonic` mapper defaults missing to 0 and saturates
    /// the i64 into u32. This is a deliberate, documented lossy conversion kept
    /// in one place (see `subsonic::i64_to_u32`), not an accidental mismatch.
    pub album_count: u32,
    /// Whether the current user has starred this artist (wire `starred` is an
    /// ISO-8601 timestamp string; we only carry the boolean here).
    pub starred: bool,
    pub cover_art: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Album {
    pub id: AlbumId,
    pub name: String,
    pub artist: String,
    pub artist_id: Option<ArtistId>,
    pub year: Option<u32>,
    pub genre: Option<String>,
    pub cover_art: Option<String>,
    pub song_count: u32,
    /// The server's `created` timestamp for the album, when it reported one.
    ///
    /// Carried for ONE reason: ordering a starred ARTIST's albums newest-first in
    /// the offline store's pin frontier, so a partly-resident artist keeps their
    /// recent work rather than an arbitrary slice. `year` is the fallback when the
    /// server omits it.
    pub created: Option<String>,
}

/// A library track.
///
/// `Serialize`/`Deserialize` (plus `PartialEq`) so the offline audio store
/// ([`crate::store`]) can embed a WHOLE song in its per-song sidecar: that
/// embedded copy is what makes an offline restore and an offline `add song/<id>`
/// carry real metadata instead of a bare id.
///
/// EVERY optional field carries `#[serde(default)]`, which is load-bearing rather
/// than decorative: the TOML serializer OMITS a `None` field entirely, so without
/// the defaults a plain round-trip of a song with (say) no comment would fail to
/// deserialize its own output. `id` and `title` stay REQUIRED - a sidecar missing
/// either is genuinely corrupt and must fail the parse (the
/// [`crate::resume::from_toml`] corruption bar), never silently load as an empty
/// id that would then mis-key the store.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Song {
    pub id: SongId,
    pub title: String,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub album_id: Option<AlbumId>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub track: Option<u32>,
    #[serde(default)]
    pub duration_secs: Option<u32>,
    /// Cover-art id (NOT the song id). Used to resolve `albumart`/`readpicture`.
    /// When absent, the handler falls back to the song id itself (Navidrome and
    /// most servers accept the media id directly for getCoverArt).
    #[serde(default)]
    pub cover_art: Option<String>,
    #[serde(default)]
    pub starred: bool,
    // ── richer metadata (feature 7) - all Option so absent server data is clean.
    /// MusicBrainz recording/track id (wire `Child.music_brainz_id`).
    #[serde(default)]
    pub musicbrainz_id: Option<String>,
    /// Disc number (wire `Child.disc_number`).
    #[serde(default)]
    pub disc: Option<u32>,
    /// Release year (wire `Child.year`). Emitted as MPD `Date`.
    #[serde(default)]
    pub year: Option<u32>,
    /// Genre name (wire `Child.genre`).
    #[serde(default)]
    pub genre: Option<String>,
    /// Bitrate in kbps (wire `Child.bit_rate`).
    #[serde(default)]
    pub bitrate: Option<u32>,
    /// Free-form comment (wire `Child.comment`).
    #[serde(default)]
    pub comment: Option<String>,
    /// The current user's 0..=5 rating (wire `Child.user_rating`).
    #[serde(default)]
    pub user_rating: Option<u8>,
    /// Composer display string (OpenSubsonic). Prefer wire `Child.display_composer`;
    /// fall back to the `Child.contributors` entries whose role is "composer".
    /// Plain-Subsonic servers omit this - `None` then matches nothing (honest).
    #[serde(default)]
    pub composer: Option<String>,
    /// Performer display string (OpenSubsonic). There is no `display_performer`
    /// wire field; derived from `Child.contributors` entries whose role is
    /// "performer". Plain-Subsonic servers omit contributors - `None` then.
    #[serde(default)]
    pub performer: Option<String>,
    // ── store identity fingerprint (offline audio store) - the four fields the
    // sidecar's `fingerprint` is built from, all straight off the wire `Child`.
    // They are metadata about the ORIGINAL file, so they describe exactly what
    // `/rest/download` returns and are unaffected by server-side transcoding.
    /// Byte size of the ORIGINAL file (wire `Child.size`). The store's commit
    /// check: a download is only committed when its length equals this exactly,
    /// and [`crate::store::AudioStore::lookup`] re-confirms it with one stat.
    #[serde(default)]
    pub size: Option<u64>,
    /// File suffix of the original, e.g. `flac` / `mp3` (wire `Child.suffix`).
    /// Names the store's audio file; sanitized before it ever reaches a path.
    #[serde(default)]
    pub suffix: Option<String>,
    /// MIME type of the original (wire `Child.content_type`). Recorded in the
    /// sidecar for provenance; never used to build a path.
    #[serde(default)]
    pub content_type: Option<String>,
    /// Server-side creation timestamp, ISO-8601 / RFC 3339 (wire `Child.created`).
    /// The third leg of the store fingerprint: a re-import bumps it, which is how
    /// a background pass notices the server's bytes changed under a stable id.
    #[serde(default)]
    pub created: Option<String>,
    // ── the current user's listening history, as the SERVER records it.
    // Navidrome is the durable owner of play history (hypodj scrobbles TO it),
    // so these are read back rather than tracked locally. Both ride along on
    // responses the daemon already makes - getStarred2, getAlbum, search3,
    // getSimilarSongs2 - so they cost no extra round trip.
    //
    // CRITICAL to their meaning: the server OMITS both keys when there is no
    // play record at all. It never sends `0` and never sends `null`. So `None`
    // means "never played (within the server's history)" and `Some(0)` cannot
    // occur - absence is itself the signal, not a hole in the data.
    /// How many times THIS user has played the track (wire `Child.play_count`).
    /// `None` = no play record, which is NOT the same as zero-just-now.
    #[serde(default)]
    pub play_count: Option<u32>,
    /// When this user last played the track, ISO-8601 / RFC 3339 with an offset
    /// (wire `Child.played`), e.g. `2026-08-06T14:17:24+01:00`. Carried VERBATIM
    /// exactly like `created`; interpret it with [`Song::played_days_ago`], which
    /// never panics on a malformed stamp.
    #[serde(default)]
    pub played: Option<String>,
}

impl Song {
    /// Whole days since this song was last played, measured from an INJECTED
    /// epoch (unix seconds).
    ///
    /// The epoch is a parameter rather than a `SystemTime::now()` read because
    /// this is the input to a ranking: injection is what makes it table-testable
    /// at any chosen date (including sweeps across a threshold) instead of being
    /// a wall-clock read that can only be observed once and never rots visibly.
    /// It is the same shape as [`crate::plan::validate`]'s `now_civil`.
    ///
    /// `None` when the server has no record OR the stamp does not parse. Both
    /// mean the same thing to every caller - no usable recency - and collapsing
    /// them is deliberate: a parse regression must never masquerade as a fresh
    /// play. A FUTURE stamp (server timezone ahead, host clock behind, an NTP
    /// step) saturates to 0 rather than wrapping into a huge age.
    pub fn played_days_ago(&self, now_unix: u64) -> Option<u32> {
        let raw = self.played.as_deref()?;
        // Accepts both shapes the server emits: whole seconds with an offset
        // (`2026-08-06T14:17:24+01:00`) and sub-second precision
        // (`2026-07-10T11:45:09.98345312+01:00`), offsets as well as `Z`.
        let dt = chrono::DateTime::parse_from_rfc3339(raw).ok()?;
        let elapsed = (now_unix as i64).saturating_sub(dt.timestamp());
        // Clamp at 0 first, so a future stamp reads as "played today" instead of
        // underflowing; then whole days, truncating.
        Some((elapsed.max(0) / 86_400) as u32)
    }
}

/// One thing that can sit in the play queue: either a resolved Subsonic [`Song`]
/// or a raw internet-radio / HTTP stream URL added directly (MPD's `add <url>`).
///
/// A raw stream has no Subsonic song id, no rating, and is never scrobbled - it
/// is played by handing its URL straight to the player. Keeping this as an enum
/// (rather than an `Option<SongId>` bolted onto `Song`) means the stream case
/// carries only what it actually has: a URL and a display title.
#[derive(Debug, Clone)]
pub enum QueueEntry {
    /// A library track resolved from Subsonic. Playing it resolves a stream URL
    /// via the client and scrobbles on the usual threshold.
    Song(Song),
    /// A raw HTTP(S) stream (internet radio). `url` is played verbatim by the
    /// player; `title` is what MPD renders (defaults to the URL). No song id,
    /// no scrobble.
    Stream { url: String, title: String },
}

impl QueueEntry {
    /// The MPD `file:` / display title for this entry.
    pub fn title(&self) -> &str {
        match self {
            QueueEntry::Song(s) => &s.title,
            QueueEntry::Stream { title, .. } => title,
        }
    }
}

/// A genre with its song/album counts (wire `data::Genre`; `name` is the
/// renamed `value` field). Backs the `Genres` browse dir and `list genre`.
#[derive(Debug, Clone)]
pub struct Genre {
    pub name: String,
    pub song_count: u32,
    pub album_count: u32,
}

/// A stored playlist id (Subsonic playlist id). Distinct newtype from
/// [`SongId`]/[`AlbumId`] so a playlist id can never cross into a media call.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlaylistId(pub String);

/// A named, server-persisted playlist (Subsonic getPlaylists row). The songs are
/// only populated by the single-playlist fetch ([`crate::subsonic::SubsonicClient::get_playlist`]);
/// the list fetch leaves `songs` empty and carries `song_count` for the count.
#[derive(Debug, Clone)]
pub struct Playlist {
    pub id: PlaylistId,
    pub name: String,
    pub song_count: u32,
    /// Populated only by the single-playlist (getPlaylist) fetch; empty from the
    /// list (getPlaylists) fetch.
    pub songs: Vec<Song>,
}

/// A saved internet radio station id (Subsonic station id). Distinct newtype from
/// [`SongId`]/[`PlaylistId`] so a station id can never cross into a media or
/// playlist call.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StationId(pub String);

/// A saved internet radio station (Subsonic getInternetRadioStations row): a raw
/// stream URL played verbatim, plus its display name and an optional homepage.
/// Distinct from the synthetic algorithmic `Radio` browse dir (random);
/// this is a persisted station the user can save, browse, and play by URL or name.
#[derive(Debug, Clone)]
pub struct Station {
    pub id: StationId,
    pub name: String,
    pub stream_url: String,
    pub home_page_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference epoch every recency test measures from: 2026-08-10T00:00:00Z.
    /// A FIXED, injected instant rather than a wall-clock read - that is the whole
    /// point of `played_days_ago` taking its epoch as a parameter, and it is what
    /// lets a threshold be swept deterministically instead of observed once.
    const NOW: u64 = 1_786_320_000;

    /// A minimal song carrying only a `played` stamp, which is all these tests read.
    fn played_song(played: Option<&str>) -> Song {
        Song {
            id: SongId("so-1".into()),
            title: "t".into(),
            album: None,
            album_id: None,
            artist: None,
            track: None,
            duration_secs: None,
            cover_art: None,
            starred: false,
            musicbrainz_id: None,
            disc: None,
            year: None,
            genre: None,
            bitrate: None,
            comment: None,
            user_rating: None,
            composer: None,
            performer: None,
            size: None,
            suffix: None,
            content_type: None,
            created: None,
            play_count: None,
            played: played.map(|p| p.to_string()),
        }
    }

    #[test]
    fn played_days_ago_reads_both_stamp_shapes_the_server_emits() {
        // Whole seconds with a +01:00 OFFSET (not `Z`) is the common Navidrome
        // shape; some rows carry nanosecond precision. Both must parse.
        assert_eq!(
            played_song(Some("2026-08-06T14:17:24+01:00")).played_days_ago(NOW),
            Some(3)
        );
        assert_eq!(
            played_song(Some("2026-07-10T11:45:09.98345312+01:00")).played_days_ago(NOW),
            Some(30)
        );
        // `Z` is accepted too, so a non-Navidrome server is not silently unread.
        assert_eq!(
            played_song(Some("2026-08-09T00:00:00Z")).played_days_ago(NOW),
            Some(1)
        );
        // The offset is APPLIED, not ignored. 00:30+01:00 is 23:30 the previous
        // UTC day, so this is 24.5h ago = 1 day; reading the stamp as if it were
        // UTC would give 23.5h = 0 days. This case is chosen precisely because
        // the two interpretations straddle a whole-day boundary.
        assert_eq!(
            played_song(Some("2026-08-09T00:30:00+01:00")).played_days_ago(NOW),
            Some(1)
        );
    }

    #[test]
    fn played_days_ago_truncates_to_whole_days_across_a_threshold() {
        // The sweep that matters for any staleness rule: 59 / 60 / 61 whole days
        // must be distinct and exact, so a `>= N days` test lands on the intended
        // side. Truncating (not rounding) means 59d23h is still 59.
        assert_eq!(
            played_song(Some("2026-06-12T00:00:00Z")).played_days_ago(NOW),
            Some(59)
        );
        assert_eq!(
            played_song(Some("2026-06-11T00:00:00Z")).played_days_ago(NOW),
            Some(60)
        );
        assert_eq!(
            played_song(Some("2026-06-10T00:00:00Z")).played_days_ago(NOW),
            Some(61)
        );
        // 59 days and 23 hours has not become 60.
        assert_eq!(
            played_song(Some("2026-06-11T01:00:00Z")).played_days_ago(NOW),
            Some(59)
        );
    }

    #[test]
    fn played_days_ago_saturates_a_future_stamp_to_zero() {
        // Server timezone ahead, host clock behind, or an NTP step. This must read
        // as "played today", NEVER wrap into a huge age (which would make the
        // track look maximally neglected) and never panic.
        assert_eq!(
            played_song(Some("2027-01-01T00:00:00Z")).played_days_ago(NOW),
            Some(0)
        );
        assert_eq!(played_song(Some("2026-08-10T00:00:00Z")).played_days_ago(NOW), Some(0));
        // Even an absurdly distant future stamp stays at 0 rather than underflowing.
        assert_eq!(
            played_song(Some("9999-12-31T23:59:59Z")).played_days_ago(NOW),
            Some(0)
        );
    }

    #[test]
    fn played_days_ago_is_none_for_absent_or_unparseable_and_never_panics() {
        // No record and a malformed stamp collapse to the SAME answer, and that is
        // deliberate: both mean "no usable recency". The dangerous alternative
        // would be defaulting to the epoch itself, which would score an unreadable
        // stamp as freshly played.
        assert_eq!(played_song(None).played_days_ago(NOW), None);
        for bad in [
            "",
            "   ",
            "not a date",
            "2026-08-06",              // date only, no time - not RFC 3339
            "2026-08-06T14:17:24",     // no offset
            "2026-13-45T99:99:99Z",    // structurally shaped but impossible
            "2026-08-06T14:17:24+99:00",
            "\u{4e2d}\u{6587}",
        ] {
            assert_eq!(
                played_song(Some(bad)).played_days_ago(NOW),
                None,
                "unparseable stamp {bad:?} must be None, not a fabricated age"
            );
        }
        // A zero epoch is legal input and must not panic either.
        assert_eq!(
            played_song(Some("2026-08-06T14:17:24+01:00")).played_days_ago(0),
            Some(0)
        );
    }

    #[test]
    fn favorite_from_uri_routes_each_kind_by_prefix() {
        assert_eq!(
            Favorite::from_uri("song/so-1"),
            Some(Favorite::Song(SongId("so-1".into())))
        );
        assert_eq!(
            Favorite::from_uri("album/al-1"),
            Some(Favorite::Album(AlbumId("al-1".into())))
        );
        assert_eq!(
            Favorite::from_uri("artist/ar-1"),
            Some(Favorite::Artist(ArtistId("ar-1".into())))
        );
    }

    #[test]
    fn favorite_from_uri_rejects_unknown_and_prefixless() {
        // Unknown prefix, bare id, and empty all yield None -> a loud ACK in the
        // playlistadd Starred arm, never a mis-targeted bucket.
        assert_eq!(Favorite::from_uri("genre/x"), None);
        assert_eq!(Favorite::from_uri("al-1"), None);
        assert_eq!(Favorite::from_uri(""), None);
    }

    #[test]
    fn favorite_uri_round_trips_for_all_variants() {
        for f in [
            Favorite::Song(SongId("so-1".into())),
            Favorite::Album(AlbumId("al-1".into())),
            Favorite::Artist(ArtistId("ar-1".into())),
        ] {
            assert_eq!(Favorite::from_uri(&f.uri()), Some(f));
        }
    }
}
