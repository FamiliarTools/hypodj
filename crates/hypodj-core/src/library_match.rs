//! Match a songrec-RECOGNIZED radio track back into the local library (task g96g064).
//!
//! `recognize.rs` names what a raw stream is playing, but the resulting metadata is
//! Shazam's: remote cover art, and album/year/genre that are frequently blank. When the
//! same recording already sits in the user's own Navidrome library, that library entry is
//! a strictly better source for those fields - and its cover art is LOCAL, so the
//! now-playing pane can serve bytes the user already owns instead of a third-party URL.
//!
//! This module is the PURE half of that: normalization, the search query, and the
//! matcher. It performs no I/O and knows nothing about the handler, so the whole
//! confidence rule is unit-testable against offline fixtures (the same shape as
//! `station_identity.rs`). The caller does the one `search3` round-trip and hands the
//! hits here.
//!
//! ## The bar: abstain rather than guess
//!
//! `search3` is full-text and ALWAYS returns something plausible, so a naive
//! "take the first hit" would happily decorate a radio track with a stranger's album
//! art. A wrong match is strictly worse than no match: no match degrades to exactly
//! today's Shazam-only behaviour, while a wrong one fabricates metadata the user has no
//! reason to distrust. So every tier here requires EXACT normalized title equality, two
//! hard vetoes kill the classic false positives (a different version of the same song, a
//! karaoke/tribute impostor), and genuinely ambiguous result sets abstain outright.

use std::collections::BTreeSet;

use crate::model::{Song, SongId};

/// How confidently a library song was matched to a recognized track. Provenance only -
/// both tiers are equally usable; surfaced on the `identify` verb so the thresholds can
/// be judged against a real library without a debugger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchBasis {
    /// Normalized artist AND title both compare EQUAL.
    Exact,
    /// Normalized titles are equal and the artist credits are a subset chain or overlap
    /// past the jaccard floor (a collaborator list versus the primary credit).
    Strong,
}

impl MatchBasis {
    /// The wire word for the `identify_match_basis` pair.
    pub fn as_str(self) -> &'static str {
        match self {
            MatchBasis::Exact => "exact",
            MatchBasis::Strong => "strong",
        }
    }
}

/// A confident library counterpart for a recognized radio track. Deliberately NOT a
/// whole [`Song`]: only the fields the read sites need, so the slot cannot grow into a
/// second source of truth for playback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryMatch {
    pub song_id: SongId,
    /// The cover id to resolve (`Song.cover_art`, else the song id - the same fallback
    /// the library `albumart` path uses).
    pub cover_id: String,
    pub album: Option<String>,
    pub year: Option<u32>,
    pub genre: Option<String>,
    pub basis: MatchBasis,
}

/// Words that mark a DIFFERENT recording of the same song. Compared, never stripped:
/// "Blessings" and "Blessings (Extended Mix)" are different recordings with different
/// art, so matching one to the other would serve the wrong cover.
const VERSION_WORDS: &[&str] = &[
    "remix", "mix", "edit", "version", "remaster", "remastered", "live", "radio",
    "extended", "instrumental", "acoustic", "dub", "vip", "bootleg", "demo", "mono",
    "stereo", "single", "club", "karaoke", "cover",
];

/// Artist-credit markers of a re-recording that is NOT the track the radio played. Real
/// libraries accumulate these and their titles match perfectly, so they need a veto.
const IMPOSTOR_WORDS: &[&str] = &["karaoke", "tribute", "covers"];

/// Credit words that introduce a secondary performer. Everything from the first one on
/// is dropped from BOTH sides, so "Blessings (feat. X)" and "Blessings" compare equal on
/// the primary credit. Matched as whole TOKENS after punctuation has been flattened, so
/// "(feat.", " ft. " and " featuring " are all caught by the same rule.
///
/// `with` is deliberately NOT here: it is a credit marker on an artist line but an
/// ordinary word in a title, and cutting on it would collapse "Dancing With Myself" and
/// "Dancing With Strangers" to the same string - manufacturing a false match, which is
/// the one outcome this module exists to prevent.
const FEAT_MARKERS: &[&str] = &["feat", "ft", "featuring"];

/// The jaccard floor for the [`MatchBasis::Strong`] artist tier.
const ARTIST_JACCARD_FLOOR: f64 = 0.60;

/// Fold one Latin-1 / Latin-Extended-A letter to ASCII. Anything outside the table
/// passes through unchanged - the crate carries no unicode-normalization dependency and
/// does not need one for the accents that actually appear in artist credits.
fn fold_char(c: char) -> &'static str {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => "a",
        'è' | 'é' | 'ê' | 'ë' => "e",
        'ì' | 'í' | 'î' | 'ï' => "i",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' => "o",
        'ù' | 'ú' | 'û' | 'ü' => "u",
        'ç' => "c",
        'ñ' => "n",
        'ý' | 'ÿ' => "y",
        'æ' => "ae",
        'ß' => "ss",
        _ => "",
    }
}

/// Normalize a title or artist credit for comparison: lowercase, diacritics folded,
/// `&` spelled out, any `feat.`/`with` clause dropped, punctuation flattened to spaces,
/// whitespace collapsed. The comparison currency for every tier below.
fn norm(s: &str) -> String {
    let spelled = s.to_lowercase().replace('&', " and ");
    let mut out = String::with_capacity(spelled.len());
    for c in spelled.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            let folded = fold_char(c);
            if folded.is_empty() {
                // Punctuation, whitespace and unknown scripts all become a separator, so
                // "Rock'n'Roll" and "Rock n Roll" agree.
                out.push(' ');
            } else {
                out.push_str(folded);
            }
        }
    }
    // Drop everything from the first credit word on. Done AFTER flattening so the marker
    // is matched as a whole token regardless of the punctuation that introduced it.
    out.split_whitespace()
        .take_while(|t| !FEAT_MARKERS.contains(t))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The comparison token set: normalized words, dropping single characters (an initial or
/// a stray letter carries no matching signal and only inflates jaccard denominators).
fn tokens(s: &str) -> BTreeSet<String> {
    norm(s)
        .split_whitespace()
        .filter(|t| t.chars().count() > 1)
        .map(str::to_string)
        .collect()
}

/// Jaccard similarity of two token sets; `0.0` when either side is empty.
fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// The version markers a title carries, read from its parenthetical/bracket groups and
/// any trailing " - <words>" suffix. Computed on the RAW string, BEFORE normalization
/// flattens the punctuation that delimits those groups.
fn version_tags(s: &str) -> BTreeSet<&'static str> {
    let lower = s.to_lowercase();
    let mut regions: Vec<String> = Vec::new();
    let mut depth = 0usize;
    let mut cur = String::new();
    for c in lower.chars() {
        match c {
            '(' | '[' => {
                depth += 1;
                if depth == 1 {
                    cur.clear();
                }
            }
            ')' | ']' => {
                if depth > 0 {
                    depth -= 1;
                    if depth == 0 {
                        regions.push(std::mem::take(&mut cur));
                    }
                }
            }
            _ if depth > 0 => cur.push(c),
            _ => {}
        }
    }
    // A trailing " - Radio Edit" style suffix is the same signal without brackets.
    if let Some(i) = lower.rfind(" - ") {
        regions.push(lower[i + 3..].to_string());
    }
    let mut tags = BTreeSet::new();
    for region in regions {
        for word in region.split(|c: char| !c.is_ascii_alphanumeric()) {
            if let Some(known) = VERSION_WORDS.iter().find(|w| **w == word) {
                tags.insert(*known);
            }
        }
    }
    tags
}

/// Is this library credit a karaoke / tribute re-recording that the recognized credit is
/// not? Such an entry matches the title perfectly and is never the track on the radio.
fn is_impostor(song_artist: &str, recognized_artist: &str) -> bool {
    let sa = norm(song_artist);
    let ra = norm(recognized_artist);
    if sa.contains("made popular by") && !ra.contains("made popular by") {
        return true;
    }
    let song_tokens = tokens(song_artist);
    let rec_tokens = tokens(recognized_artist);
    IMPOSTOR_WORDS
        .iter()
        .any(|w| song_tokens.contains(*w) && !rec_tokens.contains(*w))
}

/// Build the `search3` query for a recognized title, or `None` when nothing usable is
/// left.
///
/// TITLE ONLY, deliberately. `search3` is AND-over-tokens, so gluing the artist on
/// returns ZERO hits whenever the Shazam credit differs from the library's tag by even
/// one word - which is the common case for collaborations. Precision is recovered
/// entirely client-side by [`best_library_match`], which is strict.
///
/// Accents are KEPT: the server does its own folding, and stripping them here can break
/// the index match. Parenthetical groups and credit clauses are dropped because they are
/// exactly the parts most likely to differ between Shazam and a local tag.
pub fn search_query(title: &str) -> Option<String> {
    let lower = title.to_lowercase();
    // Drop bracketed groups wholesale.
    let mut stripped = String::with_capacity(lower.len());
    let mut depth = 0usize;
    for c in lower.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => stripped.push(c),
            _ => {}
        }
    }
    let q: String = stripped
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .take_while(|t| !FEAT_MARKERS.contains(t))
        .collect::<Vec<_>>()
        .join(" ");
    if q.is_empty() {
        None
    } else {
        Some(q)
    }
}

/// The cache key for one recognized (artist, title) pair. Normalized so trivial spelling
/// differences share a cache entry, and tab-joined so the two halves cannot run together.
pub fn match_key(artist: &str, title: &str) -> String {
    format!("{}\t{}", norm(artist), norm(title))
}

/// A candidate that survived the vetoes, carrying what the tie-break needs.
struct Candidate<'a> {
    song: &'a Song,
    basis: MatchBasis,
    artist_tokens: BTreeSet<String>,
}

/// Pick the confident library counterpart among `hits`, or `None` when nothing clears
/// the bar. PURE - the whole confidence rule lives here.
///
/// Requires a non-blank recognized artist AND title: a one-sided Shazam hit has nothing
/// to verify a candidate against, and matching on title alone is exactly how a stranger's
/// album art ends up on the pane.
pub fn best_library_match(
    hits: &[Song],
    artist: &str,
    title: &str,
    rec_album: Option<&str>,
) -> Option<LibraryMatch> {
    let (n_artist, n_title) = (norm(artist), norm(title));
    if n_artist.is_empty() || n_title.is_empty() {
        return None;
    }
    let rec_version = version_tags(title);
    let rec_artist_tokens = tokens(artist);

    let mut accepted: Vec<Candidate> = Vec::new();
    for song in hits {
        // V3 EMPTY: nothing to verify the credit against.
        let Some(song_artist) = song.artist.as_deref().filter(|a| !a.trim().is_empty()) else {
            continue;
        };
        // Both tiers require EXACT normalized title equality - there is no fuzzy title
        // tier, because a fuzzy title is precisely what admits the wrong recording.
        if norm(&song.title) != n_title {
            continue;
        }
        // V1 VERSION: a remix/live/remaster is a different recording with different art.
        if version_tags(&song.title) != rec_version {
            continue;
        }
        // V2 IMPOSTOR: karaoke / tribute re-recordings match titles perfectly.
        if is_impostor(song_artist, artist) {
            continue;
        }
        let song_tokens = tokens(song_artist);
        let basis = if norm(song_artist) == n_artist {
            MatchBasis::Exact
        } else if rec_artist_tokens.is_subset(&song_tokens)
            || song_tokens.is_subset(&rec_artist_tokens)
            || jaccard(&rec_artist_tokens, &song_tokens) >= ARTIST_JACCARD_FLOOR
        {
            MatchBasis::Strong
        } else {
            continue;
        };
        accepted.push(Candidate { song, basis, artist_tokens: song_tokens });
    }

    if accepted.is_empty() {
        return None;
    }

    // AMBIGUITY ABSTAIN. Every accepted candidate already shares the normalized title, so
    // any disagreement is on the artist. Credit variants of one recording form a subset
    // chain (a single credit versus the full collaborator list); anything else means the
    // rule was too loose for this input, and no match beats a wrong one.
    for (i, a) in accepted.iter().enumerate() {
        for b in accepted.iter().skip(i + 1) {
            if !a.artist_tokens.is_subset(&b.artist_tokens)
                && !b.artist_tokens.is_subset(&a.artist_tokens)
            {
                return None;
            }
        }
    }

    let n_rec_album = rec_album.map(norm);
    // DETERMINISTIC selection, so the same library and hit always yield the same match
    // regardless of the order Navidrome happened to return the rows in.
    let best = accepted
        .iter()
        .min_by(|a, b| {
            let key = |c: &Candidate| {
                (
                    // Exact before Strong.
                    matches!(c.basis, MatchBasis::Strong),
                    // A starred song first.
                    !c.song.starred,
                    // The album the recognition named first, when it named one.
                    match (&n_rec_album, c.song.album.as_deref()) {
                        (Some(want), Some(have)) => norm(have) != *want,
                        _ => true,
                    },
                    // Earliest release, unknown last.
                    c.song.year.unwrap_or(u32::MAX),
                )
            };
            key(a)
                .cmp(&key(b))
                // SongId has no Ord; compare the inner string so the order is total.
                .then_with(|| a.song.id.0.as_str().cmp(b.song.id.0.as_str()))
        })
        .expect("accepted is non-empty");

    Some(LibraryMatch {
        song_id: best.song.id.clone(),
        cover_id: best
            .song
            .cover_art
            .clone()
            .unwrap_or_else(|| best.song.id.0.clone()),
        album: best.song.album.clone(),
        year: best.song.year,
        genre: best.song.genre.clone(),
        basis: best.basis,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A library song carrying only the fields the matcher reads.
    fn song(id: &str, title: &str, artist: Option<&str>) -> Song {
        Song {
            id: SongId(id.to_string()),
            title: title.to_string(),
            artist: artist.map(str::to_string),
            album: None,
            album_id: None,
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
        }
    }

    #[test]
    fn norm_folds_case_diacritics_and_ampersand() {
        assert_eq!(norm("Björk & Sadé"), "bjork and sade");
        assert_eq!(norm("ROCK'N'ROLL"), "rock n roll");
        // A credit clause is dropped from the first marker on.
        assert_eq!(norm("Blessings (feat. Clementine)"), "blessings");
        assert_eq!(norm("Someone ft. Another"), "someone");
    }

    #[test]
    fn with_is_not_a_credit_cut_so_distinct_titles_stay_distinct() {
        // Regression guard for a false-match trap: if `with` were treated as a credit
        // marker, these two different songs would both normalize to "dancing" and the
        // matcher would confidently serve the wrong track's art.
        assert_ne!(norm("Dancing With Myself"), norm("Dancing With Strangers"));
        assert_eq!(norm("Dancing With Myself"), "dancing with myself");
        let hits = vec![song("s1", "Dancing With Strangers", Some("A Band"))];
        assert_eq!(best_library_match(&hits, "A Band", "Dancing With Myself", None), None);
    }

    #[test]
    fn feat_is_cut_regardless_of_the_punctuation_introducing_it() {
        // The marker is matched as a whole token after flattening, so every spelling of
        // a credit clause collapses to the same primary credit.
        for variant in [
            "Blessings (feat. Clementine)",
            "Blessings ft. Clementine",
            "Blessings featuring Clementine",
            "Blessings [feat Clementine]",
        ] {
            assert_eq!(norm(variant), "blessings", "variant: {variant}");
        }
    }

    #[test]
    fn exact_tier_on_identical_artist_and_title() {
        let hits = vec![song("s1", "Blessings", Some("Calvin Harris"))];
        let m = best_library_match(&hits, "Calvin Harris", "Blessings", None).expect("a match");
        assert_eq!(m.song_id, SongId("s1".into()));
        assert_eq!(m.basis, MatchBasis::Exact);
        // No cover_art on the song, so the cover id falls back to the song id.
        assert_eq!(m.cover_id, "s1");
    }

    #[test]
    fn cover_id_prefers_cover_art_when_present() {
        let mut s = song("s1", "Blessings", Some("Calvin Harris"));
        s.cover_art = Some("al-9".into());
        let m = best_library_match(&[s], "Calvin Harris", "Blessings", None).expect("a match");
        assert_eq!(m.cover_id, "al-9");
    }

    #[test]
    fn collaborator_subset_is_strong() {
        // Shazam credits the full collaboration, the library tags only the primary. Their
        // jaccard is below the floor, so the SUBSET clause is what has to admit this.
        let hits = vec![song("s1", "Blessings", Some("Calvin Harris"))];
        let m = best_library_match(&hits, "Calvin Harris & Clementine Douglas", "Blessings", None)
            .expect("a match");
        assert_eq!(m.basis, MatchBasis::Strong);
    }

    #[test]
    fn version_tag_mismatch_vetoes() {
        // THE most important negative: a different recording with different art.
        let hits = vec![song("s1", "Blessings (Extended Mix)", Some("Calvin Harris"))];
        assert_eq!(best_library_match(&hits, "Calvin Harris", "Blessings", None), None);
        // And symmetrically, a recognized remix must not match the plain library cut.
        let plain = vec![song("s2", "Blessings", Some("Calvin Harris"))];
        assert_eq!(
            best_library_match(&plain, "Calvin Harris", "Blessings (Extended Mix)", None),
            None
        );
        // Matching version tags on BOTH sides is fine.
        let both = vec![song("s3", "Blessings (Extended Mix)", Some("Calvin Harris"))];
        assert!(
            best_library_match(&both, "Calvin Harris", "Blessings (Extended Mix)", None).is_some()
        );
    }

    #[test]
    fn same_title_unrelated_artist_rejected() {
        let hits = vec![song("s1", "Blessings", Some("Some Other Band"))];
        assert_eq!(best_library_match(&hits, "Calvin Harris", "Blessings", None), None);
    }

    #[test]
    fn karaoke_and_tribute_impostors_vetoed() {
        let karaoke = vec![song("s1", "Blessings", Some("Karaoke Allstars"))];
        assert_eq!(best_library_match(&karaoke, "Calvin Harris", "Blessings", None), None);
        let tribute = vec![song("s2", "Blessings", Some("Tribute Band"))];
        assert_eq!(best_library_match(&tribute, "Calvin Harris", "Blessings", None), None);
        let popular = vec![song("s3", "Blessings", Some("Made Popular By Calvin Harris"))];
        assert_eq!(best_library_match(&popular, "Calvin Harris", "Blessings", None), None);
    }

    #[test]
    fn blank_artist_or_title_never_matches() {
        let hits = vec![song("s1", "Blessings", Some("Calvin Harris"))];
        assert_eq!(best_library_match(&hits, "   ", "Blessings", None), None);
        assert_eq!(best_library_match(&hits, "Calvin Harris", "  ", None), None);
        // A library row with no artist has nothing to verify against.
        let no_artist = vec![song("s2", "Blessings", None)];
        assert_eq!(best_library_match(&no_artist, "Calvin Harris", "Blessings", None), None);
    }

    #[test]
    fn empty_hits_returns_none() {
        assert_eq!(best_library_match(&[], "Calvin Harris", "Blessings", None), None);
    }

    #[test]
    fn disagreeing_survivors_abstain() {
        // Two accepted candidates whose credits are NOT a subset chain: the rule was too
        // loose for this input, so abstain rather than pick one.
        let hits = vec![
            song("s1", "Blessings", Some("Calvin Harris")),
            song("s2", "Blessings", Some("Clementine Douglas")),
        ];
        assert_eq!(
            best_library_match(&hits, "Calvin Harris & Clementine Douglas", "Blessings", None),
            None
        );
    }

    #[test]
    fn credit_variants_do_not_abstain() {
        // A subset chain is the SAME recording credited two ways, so it must still match.
        let hits = vec![
            song("s1", "Blessings", Some("Calvin Harris")),
            song("s2", "Blessings", Some("Calvin Harris & Clementine Douglas")),
        ];
        let m = best_library_match(&hits, "Calvin Harris & Clementine Douglas", "Blessings", None)
            .expect("a match, not an abstention");
        // The exact credit wins the tier ordering.
        assert_eq!(m.song_id, SongId("s2".into()));
        assert_eq!(m.basis, MatchBasis::Exact);
    }

    #[test]
    fn tie_break_is_order_independent() {
        let a = song("s1", "Blessings", Some("Calvin Harris"));
        let b = song("s2", "Blessings", Some("Calvin Harris"));
        let fwd = best_library_match(&[a.clone(), b.clone()], "Calvin Harris", "Blessings", None);
        let rev = best_library_match(&[b, a], "Calvin Harris", "Blessings", None);
        assert_eq!(fwd, rev);
        assert_eq!(fwd.expect("a match").song_id, SongId("s1".into()));
    }

    #[test]
    fn tie_break_prefers_the_recognized_album_then_earliest_year() {
        let mut single = song("s1", "Blessings", Some("Calvin Harris"));
        single.album = Some("Blessings - Single".into());
        single.year = Some(2024);
        let mut compilation = song("s2", "Blessings", Some("Calvin Harris"));
        compilation.album = Some("Summer Hits".into());
        compilation.year = Some(2025);
        let hits = vec![compilation, single];
        let m = best_library_match(&hits, "Calvin Harris", "Blessings", Some("Blessings - Single"))
            .expect("a match");
        assert_eq!(m.song_id, SongId("s1".into()), "the recognized album wins");
        assert_eq!(m.album.as_deref(), Some("Blessings - Single"));
        assert_eq!(m.year, Some(2024));
    }

    #[test]
    fn search_query_drops_parentheticals_feat_and_punctuation_but_keeps_accents() {
        assert_eq!(search_query("Blessings (Extended Mix)").as_deref(), Some("blessings"));
        assert_eq!(search_query("Someone feat. Another").as_deref(), Some("someone"));
        assert_eq!(search_query("Rock'n'Roll").as_deref(), Some("rock n roll"));
        // Accents survive - the server does its own folding and stripping can miss.
        assert_eq!(search_query("Café Boheme").as_deref(), Some("café boheme"));
        // Nothing usable left.
        assert_eq!(search_query("   "), None);
        assert_eq!(search_query("(Live)"), None);
    }

    #[test]
    fn match_key_is_normalized_and_two_sided() {
        assert_eq!(match_key("Calvin Harris", "Blessings"), "calvin harris\tblessings");
        // Trivial spelling differences share one cache entry.
        assert_eq!(match_key("CALVIN  HARRIS", "Blessings"), match_key("Calvin Harris", "blessings"));
    }
}
