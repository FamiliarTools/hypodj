//! Relevance ranking for `searchall` (the Find tab), FREE tier only.
//!
//! The objective is a SEARCH BOX: the user already knows the answer and is typing it,
//! so precision comes first and habit only breaks ties. This is deliberately NOT the
//! continuation selector (which wants discovery); the two share signals, never the
//! ranking.
//!
//! The sort key per hit is `(precision, habit desc, server order)`:
//!
//! - `precision` is a small ordinal computed from the query against the hit's own
//!   name fields. It is the PRIMARY key, so an exact title/artist match can never be
//!   outranked by a habit signal, however strong.
//! - `habit` uses only what Navidrome already put on the search3 row: `starred` and
//!   `play_count` (songs, albums), `starred` for artists plus the play counts of that
//!   artist's songs in the same reply. No extra round trip and no durable state in
//!   hypodj - the server owns all play history.
//! - server order is the final tiebreak (the sort is stable), so a query with no habit
//!   signal at all comes back exactly as Navidrome ordered it.
//!
//! See `docs/design/find-ranking.md` for the measurement and the tier-B decision.

use crate::model::{Album, Artist, Song};
use std::collections::HashMap;

/// How closely a hit's name fields match the query. Lower is better.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Precision {
    /// A name field equals the query (case and punctuation folded).
    Exact,
    /// Every query word appears as a whole word across the name fields.
    ///
    /// Deliberately ONE band whether the words lead the title or not: measured on the
    /// real library, a separate "starts with" band put `Love We Battle` (5 plays)
    /// above `Let Love Rumpel` (42 plays) for `love`, which is the opposite of what
    /// the person typing meant.
    Words,
    /// Anything else the server matched (substring, other fields, its own index).
    Loose,
    /// Not a server match at all: an approximate hit from the local fuzzy index
    /// (`crate::fuzzy`), carrying `1000 - round(score * 1000)` so a closer spelling
    /// sorts first. Below every real band, and the score is part of the PRIMARY key,
    /// so habit only breaks ties between equally close spellings and a starred but
    /// distant name can never outrank the one actually typed.
    Approx(u16),
}

impl Precision {
    /// The band for a fuzzy score in `[0, 1]`.
    pub fn approx(score: f64) -> Precision {
        Precision::Approx((1000.0 - (score.clamp(0.0, 1.0) * 1000.0).round()) as u16)
    }
}

/// Lowercase, keep alphanumerics, collapse everything else to single spaces.
fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut gap = false;
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            if gap && !out.is_empty() {
                out.push(' ');
            }
            gap = false;
            out.push(c);
        } else {
            gap = true;
        }
    }
    out
}

/// The precision of `fields` against `query` (both folded here).
pub fn precision(query: &str, fields: &[&str]) -> Precision {
    let q = fold(query);
    if q.is_empty() {
        return Precision::Loose;
    }
    let folded: Vec<String> = fields.iter().map(|f| fold(f)).collect();
    if folded.iter().any(|f| *f == q) {
        return Precision::Exact;
    }
    let words: std::collections::HashSet<&str> =
        folded.iter().flat_map(|f| f.split(' ')).collect();
    if q.split(' ').all(|w| words.contains(w)) {
        return Precision::Words;
    }
    Precision::Loose
}

/// Habit weight from the server's own counters. Starring is a deliberate act, so it
/// counts like several plays; plays are log-scaled so one heavily looped track cannot
/// drown everything below it within a precision band.
fn habit(starred: bool, plays: u32) -> f64 {
    (if starred { 3.0 } else { 0.0 }) + (1.0 + plays as f64).ln()
}

fn sort_by_key<T>(items: &mut Vec<T>, mut key: impl FnMut(&T) -> (Precision, f64)) {
    let mut keyed: Vec<(Precision, f64, usize, T)> = items
        .drain(..)
        .enumerate()
        .map(|(i, t)| {
            let (p, h) = key(&t);
            (p, h, i, t)
        })
        .collect();
    keyed.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.2.cmp(&b.2))
    });
    items.extend(keyed.into_iter().map(|(_, _, _, t)| t));
}

/// Re-rank one `searchall` reply in place. Pure; the sets are unchanged, only order.
///
/// `approx` maps the id of every APPROXIMATE (fuzzy, non-server) hit to its fuzzy
/// score; those rows take [`Precision::approx`] instead of a text band.
pub fn rank(
    query: &str,
    artists: &mut Vec<Artist>,
    albums: &mut Vec<Album>,
    songs: &mut Vec<Song>,
    approx: &HashMap<String, f64>,
) {
    let band = |id: &str, real: Precision| approx.get(id).map_or(real, |s| Precision::approx(*s));
    // Artist play weight is the sum of that artist's song plays IN THIS REPLY: the
    // only artist-level play signal the row carries for free.
    let mut artist_plays: HashMap<String, u32> = HashMap::new();
    for s in songs.iter() {
        if let (Some(a), Some(p)) = (&s.artist, s.play_count) {
            let e = artist_plays.entry(fold(a)).or_default();
            *e = e.saturating_add(p);
        }
    }
    sort_by_key(artists, |a| {
        let plays = artist_plays.get(&fold(&a.name)).copied().unwrap_or(0);
        (band(&a.id.0, precision(query, &[&a.name])), habit(a.starred, plays))
    });
    sort_by_key(albums, |a| {
        (band(&a.id.0, precision(query, &[&a.name, &a.artist])), habit(a.starred, a.play_count.unwrap_or(0)))
    });
    sort_by_key(songs, |s| {
        let fields = [s.title.as_str(), s.artist.as_deref().unwrap_or("")];
        let mut h = habit(s.starred, s.play_count.unwrap_or(0));
        if s.user_rating.unwrap_or(0) >= 4 {
            h += 1.0;
        }
        (band(&s.id.0, precision(query, &fields)), h)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AlbumId, ArtistId, SongId};

    fn song(id: &str, title: &str, artist: &str, plays: Option<u32>, starred: bool) -> Song {
        let toml = format!("id = \"{id}\"\ntitle = \"{title}\"\nartist = \"{artist}\"\n");
        let mut s: Song = toml::from_str(&toml).unwrap();
        s.id = SongId(id.into());
        s.play_count = plays;
        s.starred = starred;
        s
    }

    fn album(id: &str, name: &str, artist: &str, plays: Option<u32>, starred: bool) -> Album {
        Album {
            id: AlbumId(id.into()),
            name: name.into(),
            artist: artist.into(),
            artist_id: None,
            year: None,
            genre: None,
            cover_art: None,
            song_count: 1,
            created: None,
            play_count: plays,
            starred,
        }
    }

    fn artist(id: &str, name: &str, starred: bool) -> Artist {
        Artist { id: ArtistId(id.into()), name: name.into(), album_count: 1, starred, cover_art: None }
    }

    fn ids_s(v: &[Song]) -> Vec<&str> {
        v.iter().map(|s| s.id.0.as_str()).collect()
    }

    #[test]
    fn precision_levels_fold_case_and_punctuation() {
        assert_eq!(precision("Creep", &["creep"]), Precision::Exact);
        assert_eq!(precision("hey, jude!", &["Hey Jude"]), Precision::Exact);
        assert_eq!(precision("blue", &["Blue Monday"]), Precision::Words);
        assert_eq!(precision("love", &["Let Love Rumpel"]), Precision::Words);
        // A word must be whole: "blue" is not a word of "Bluebird".
        assert_eq!(precision("blue", &["Bluebird"]), Precision::Loose);
        assert_eq!(precision("monday new", &["Blue Monday", "New Order"]), Precision::Words);
        assert_eq!(precision("mond", &["Blue Monday"]), Precision::Loose);
        assert_eq!(precision("", &["x"]), Precision::Loose);
    }

    #[test]
    fn an_exact_title_is_never_outranked_by_habit() {
        let mut songs = vec![
            song("loved", "Creep On Me", "X", Some(900), true),
            song("exact", "Creep", "Radiohead", None, false),
        ];
        rank("creep", &mut vec![], &mut vec![], &mut songs, &HashMap::new());
        assert_eq!(ids_s(&songs), ["exact", "loved"]);
    }

    #[test]
    fn an_exact_artist_match_beats_a_loved_loose_title() {
        let mut songs = vec![
            song("loose", "Songs about Bjork", "Other", Some(500), true),
            song("art", "Joga", "Björk", None, false),
        ];
        rank("björk", &mut vec![], &mut vec![], &mut songs, &HashMap::new());
        assert_eq!(ids_s(&songs), ["art", "loose"]);
    }

    #[test]
    fn a_loved_track_with_the_word_inside_beats_a_cold_one_that_leads_with_it() {
        let mut songs = vec![
            song("lead", "Love We Battle", "A", Some(5), false),
            song("rumpel", "Let Love Rumpel", "Kalabrese", Some(42), false),
        ];
        rank("love", &mut vec![], &mut vec![], &mut songs, &HashMap::new());
        assert_eq!(ids_s(&songs), ["rumpel", "lead"]);
    }

    #[test]
    fn habit_orders_within_a_precision_band() {
        let mut songs = vec![
            song("cold", "Intro", "A", None, false),
            song("some", "Intro", "B", Some(3), false),
            song("star", "Intro", "C", Some(1), true),
        ];
        rank("intro", &mut vec![], &mut vec![], &mut songs, &HashMap::new());
        assert_eq!(ids_s(&songs), ["star", "some", "cold"]);
    }

    #[test]
    fn no_signal_keeps_the_server_order_exactly() {
        let mut songs = vec![
            song("1", "Night A", "x", None, false),
            song("2", "Night B", "x", None, false),
            song("3", "Night C", "x", None, false),
        ];
        rank("night", &mut vec![], &mut vec![], &mut songs, &HashMap::new());
        assert_eq!(ids_s(&songs), ["1", "2", "3"]);
    }

    #[test]
    fn albums_rank_by_precision_then_their_own_plays_and_star() {
        let mut albums = vec![
            album("loose", "The Best of Blue", "Z", Some(50), true),
            album("cold", "Blue", "A", None, false),
            album("warm", "Blue", "B", Some(10), false),
        ];
        rank("blue", &mut vec![], &mut albums, &mut vec![], &HashMap::new());
        let ids: Vec<_> = albums.iter().map(|a| a.id.0.as_str()).collect();
        assert_eq!(ids, ["warm", "cold", "loose"]);
    }

    #[test]
    fn an_artist_is_lifted_by_its_songs_plays_in_the_same_reply() {
        let mut artists = vec![artist("a", "Air Supply", false), artist("b", "Air", false)];
        let mut songs = vec![song("s", "La Femme d'Argent", "Air", Some(40), false)];
        // "air" exactly equals "Air", which wins on precision regardless.
        rank("air", &mut artists, &mut vec![], &mut songs, &HashMap::new());
        assert_eq!(artists[0].id.0, "b");
        // With equal precision, the played one leads.
        let mut artists = vec![artist("x", "Air Supply", false), artist("y", "Air France", false)];
        let mut songs = vec![song("s", "T", "Air France", Some(2), false)];
        rank("air", &mut artists, &mut vec![], &mut songs, &HashMap::new());
        assert_eq!(artists[0].id.0, "y");
    }

    #[test]
    fn ranking_never_changes_the_set() {
        let mut songs = vec![
            song("1", "a", "x", Some(1), false),
            song("2", "b", "y", None, true),
            song("3", "c", "z", Some(9), false),
        ];
        rank("q", &mut vec![], &mut vec![], &mut songs, &HashMap::new());
        let mut ids = ids_s(&songs);
        ids.sort();
        assert_eq!(ids, ["1", "2", "3"]);
    }

    #[test]
    fn habit_never_lifts_an_approximate_hit_over_a_closer_one() {
        // A starred, heavily played song that is a FARTHER spelling, a never played
        // closer spelling, and one real (server) loose hit.
        let mut songs = vec![
            song("far", "Hyperactive", "X", Some(500), true),
            song("near", "Hyperion", "Y", Some(0), false),
            song("real", "Something Hyper", "Z", Some(0), false),
        ];
        let approx: HashMap<String, f64> = [("far".to_string(), 0.62), ("near".to_string(), 0.81)].into();
        rank("hyperon", &mut vec![], &mut vec![], &mut songs, &approx);
        let ids: Vec<&str> = songs.iter().map(|s| s.id.0.as_str()).collect();
        assert_eq!(ids, ["real", "near", "far"]);
    }
}
