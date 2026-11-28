//! Typo-tolerant fallback for `searchall` (task 01m8bhyp, approach (c) of
//! `docs/design/fuzzy-search.md`).
//!
//! search3 is a token LIKE match: it folds accents and ANDs words, but one dropped or
//! swapped letter ("masive attack", "hyperon") finds nothing. This module holds a local
//! NAME index of the whole library (artists, albums, song titles) and scores a query
//! against it. It only ever ADDS: a kind whose server hits are already plentiful is
//! returned untouched, and approximate hits always sit after the real ones, flagged so
//! a client can say "did you mean".
//!
//! Scoring (the spike's, ported): `0.5 * max(trigram Jaccard of the folded name, of the
//! space-stripped name) + 0.5 * mean over query words of the best normalised
//! Levenshtein against a name word` (words of <= 2 chars must match exactly). A trigram
//! posting list prefilters the candidates so a query never scans the whole library.
//! Non-Latin text (CJK) scores on trigrams only: per-word edit distance over a script
//! without spaces is meaningless.

use crate::model::{Album, Artist, Song};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// A kind with fewer real hits than this gets approximate hits appended.
pub const THIN: usize = 3;
/// At most this many approximate hits per kind.
pub const PER_KIND: usize = 5;
/// Minimum fuzzy score for an approximate hit. Chosen on the bench
/// (`fuzzy-bench`): every one of the 36 flawed real queries' intended item scores
/// above it (lowest 0.55, "parklif"; four sat at 0.59 under a 0.6 cut), and the 10
/// nonsense queries top out at 0.37, so the cut keeps a margin on both sides.
pub const THRESHOLD: f64 = 0.5;
/// Candidates scored per kind after the trigram prefilter.
const CANDIDATES: usize = 256;
/// Memory bound: the index stops ingesting a kind past this many entries.
pub const MAX_ENTRIES: usize = 100_000;

/// Lowercase, strip diacritics, drop apostrophes, punctuation to single spaces, and
/// split digit/letter boundaries ("sakurazaka46" -> "sakurazaka 46").
pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut gap = false;
    let mut prev: Option<char> = None;
    for c in s.chars().flat_map(char::to_lowercase) {
        if matches!(c, '\'' | '\u{2019}' | '\u{2018}' | '`') {
            continue;
        }
        let mapped = fold_char(c);
        for m in mapped.chars() {
            if m.is_alphanumeric() {
                let boundary = prev.is_some_and(|p| p.is_ascii_digit() != m.is_ascii_digit());
                if (gap || boundary) && !out.is_empty() {
                    out.push(' ');
                }
                gap = false;
                out.push(m);
                prev = Some(m);
            } else {
                gap = true;
                prev = None;
            }
        }
    }
    out
}

/// The Latin diacritic table (lowercase input). Anything not listed passes through,
/// which is right for CJK and every unaccented letter.
fn fold_char(c: char) -> std::borrow::Cow<'static, str> {
    use std::borrow::Cow::{Borrowed, Owned};
    let s: &'static str = match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => "a",
        'æ' => "ae",
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => "c",
        'ď' | 'đ' | 'ð' => "d",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => "e",
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => "g",
        'ĥ' | 'ħ' => "h",
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => "i",
        'ĵ' => "j",
        'ķ' => "k",
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => "l",
        'ñ' | 'ń' | 'ņ' | 'ň' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => "o",
        'œ' => "oe",
        'ŕ' | 'ŗ' | 'ř' => "r",
        'ś' | 'ŝ' | 'ş' | 'š' | 'ș' => "s",
        'ß' => "ss",
        'ţ' | 'ť' | 'ŧ' | 'ț' => "t",
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => "u",
        'ŵ' => "w",
        'ý' | 'ÿ' | 'ŷ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        'þ' => "th",
        _ => return Owned(c.to_string()),
    };
    Borrowed(s)
}

/// Hash of one char trigram, so a posting key is a `u32`, not a `String`.
fn tri_hash(a: char, b: char, c: char) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for ch in [a, b, c] {
        h ^= ch as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Sorted, deduplicated trigram hashes of `s`, padded like the spike (`"  s "`).
fn trigrams(s: &str) -> Vec<u32> {
    let chars: Vec<char> = "  ".chars().chain(s.chars()).chain(" ".chars()).collect();
    let mut v: Vec<u32> = chars.windows(3).map(|w| tri_hash(w[0], w[1], w[2])).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Jaccard of two sorted, deduplicated sets.
fn jaccard(a: &[u32], b: &[u32]) -> f64 {
    let (mut i, mut j, mut inter) = (0, 0, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                inter += 1;
                i += 1;
                j += 1;
            }
        }
    }
    let union = a.len() + b.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// A folded query, precomputed once and scored against many entries.
pub struct Query {
    folded: String,
    tri: Vec<u32>,
    tri_ns: Vec<u32>,
    latin: bool,
}

impl Query {
    pub fn new(q: &str) -> Option<Query> {
        let folded = fold(q);
        if folded.is_empty() {
            return None;
        }
        let tri = trigrams(&folded);
        let tri_ns = trigrams(&folded.replace(' ', ""));
        let latin = folded.chars().all(|c| c.is_ascii() || !c.is_alphabetic());
        Some(Query { folded, tri, tri_ns, latin })
    }
}

struct Entry<T> {
    item: T,
    folded: Box<str>,
    tri: Box<[u32]>,
    tri_ns: Box<[u32]>,
}

/// One kind's entries plus the trigram posting list over them.
struct Kind<T> {
    entries: Vec<Entry<T>>,
    post: HashMap<u32, Vec<u32>>,
}

impl<T: Clone> Kind<T> {
    fn build(items: Vec<T>, name: impl Fn(&T) -> &str) -> Kind<T> {
        let mut entries = Vec::with_capacity(items.len().min(MAX_ENTRIES));
        let mut post: HashMap<u32, Vec<u32>> = HashMap::new();
        for item in items.into_iter().take(MAX_ENTRIES) {
            let folded = fold(name(&item));
            if folded.is_empty() {
                continue;
            }
            let tri = trigrams(&folded);
            let tri_ns = trigrams(&folded.replace(' ', ""));
            let idx = entries.len() as u32;
            let mut keys: Vec<u32> = tri.iter().chain(tri_ns.iter()).copied().collect();
            keys.sort_unstable();
            keys.dedup();
            for k in keys {
                post.entry(k).or_default().push(idx);
            }
            entries.push(Entry {
                item,
                folded: folded.into_boxed_str(),
                tri: tri.into_boxed_slice(),
                tri_ns: tri_ns.into_boxed_slice(),
            });
        }
        for v in post.values_mut() {
            v.shrink_to_fit();
        }
        post.shrink_to_fit();
        Kind { entries, post }
    }

    /// The best `n` entries scoring at least `min`, best first, skipping `skip`.
    fn search(&self, q: &Query, n: usize, min: f64, skip: impl Fn(&T) -> bool) -> Vec<(f64, T)> {
        let mut shared: HashMap<u32, u16> = HashMap::new();
        let mut keys: Vec<u32> = q.tri.iter().chain(q.tri_ns.iter()).copied().collect();
        keys.sort_unstable();
        keys.dedup();
        for k in &keys {
            if let Some(list) = self.post.get(k) {
                for &i in list {
                    *shared.entry(i).or_default() += 1;
                }
            }
        }
        // Two shared trigrams at least: one is noise (every name has "  a" for an a).
        let floor = 2u16.max((keys.len() / 5) as u16);
        let mut cand: Vec<(u16, u32)> =
            shared.into_iter().filter(|&(_, c)| c >= floor).map(|(i, c)| (c, i)).collect();
        cand.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        cand.truncate(CANDIDATES);
        let mut scored: Vec<(f64, u32)> = cand
            .into_iter()
            .map(|(_, i)| (score(q, &self.entries[i as usize]), i))
            .filter(|&(s, i)| s >= min && !skip(&self.entries[i as usize].item))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1)));
        scored.truncate(n);
        scored.into_iter().map(|(s, i)| (s, self.entries[i as usize].item.clone())).collect()
    }
}

fn score<T>(q: &Query, e: &Entry<T>) -> f64 {
    let j = jaccard(&q.tri, &e.tri).max(jaccard(&q.tri_ns, &e.tri_ns));
    if !q.latin {
        return j;
    }
    let ew: Vec<&str> = e.folded.split(' ').collect();
    let qw: Vec<&str> = q.folded.split(' ').collect();
    let tot: f64 = qw
        .iter()
        .map(|w| {
            ew.iter()
                .map(|x| {
                    if w.chars().count() <= 2 {
                        f64::from(u8::from(w == x))
                    } else {
                        strsim::normalized_levenshtein(w, x)
                    }
                })
                .fold(0.0, f64::max)
        })
        .sum();
    0.5 * j + 0.5 * tot / qw.len() as f64
}

/// The whole-library name index.
pub struct FuzzyIndex {
    artists: Kind<Artist>,
    albums: Kind<Album>,
    songs: Kind<Song>,
    /// `getScanStatus` count at build time; a different count later means a rescan.
    pub scan_count: Option<i64>,
    pub built_at: Instant,
}

impl FuzzyIndex {
    pub fn build(artists: Vec<Artist>, albums: Vec<Album>, songs: Vec<Song>, scan_count: Option<i64>) -> Self {
        FuzzyIndex {
            artists: Kind::build(artists, |a| &a.name),
            albums: Kind::build(albums, |a| &a.name),
            songs: Kind::build(songs, |s| &s.title),
            scan_count,
            built_at: Instant::now(),
        }
    }

    /// (artists, albums, songs) entry counts.
    pub fn len(&self) -> (usize, usize, usize) {
        (self.artists.entries.len(), self.albums.entries.len(), self.songs.entries.len())
    }

    /// The best raw fuzzy score per kind (artists, albums, songs), with NO
    /// threshold. For the bench's precision check only.
    pub fn best_scores(&self, query: &str) -> [f64; 3] {
        let Some(q) = Query::new(query) else { return [0.0; 3] };
        [
            self.artists.search(&q, 1, 0.0, |_| false).first().map_or(0.0, |x| x.0),
            self.albums.search(&q, 1, 0.0, |_| false).first().map_or(0.0, |x| x.0),
            self.songs.search(&q, 1, 0.0, |_| false).first().map_or(0.0, |x| x.0),
        ]
    }

    pub fn is_empty(&self) -> bool {
        self.len() == (0, 0, 0)
    }
}

/// A `searchall` result after the fuzzy pass: the real hits first, then the
/// approximate ones, plus each approximate hit's score keyed by its id.
#[derive(Default)]
pub struct Augmented {
    pub artists: Vec<Artist>,
    pub albums: Vec<Album>,
    pub songs: Vec<Song>,
    /// id -> fuzzy score, ONLY for appended approximate rows.
    pub approx: HashMap<String, f64>,
}

/// Append approximate hits to every kind with fewer than [`THIN`] real ones. A kind
/// with enough real hits, a missing index, or an unfoldable query leaves the reply
/// exactly as the server sent it.
pub fn augment(
    query: &str,
    artists: Vec<Artist>,
    albums: Vec<Album>,
    songs: Vec<Song>,
    index: Option<&FuzzyIndex>,
) -> Augmented {
    let mut out = Augmented { artists, albums, songs, approx: HashMap::new() };
    let (Some(ix), Some(q)) = (index, Query::new(query)) else {
        return out;
    };
    if out.artists.len() < THIN {
        let have: Vec<String> = out.artists.iter().map(|a| a.id.0.clone()).collect();
        for (s, a) in ix.artists.search(&q, PER_KIND, THRESHOLD, |a| have.contains(&a.id.0)) {
            out.approx.insert(a.id.0.clone(), s);
            out.artists.push(a);
        }
    }
    if out.albums.len() < THIN {
        let have: Vec<String> = out.albums.iter().map(|a| a.id.0.clone()).collect();
        for (s, a) in ix.albums.search(&q, PER_KIND, THRESHOLD, |a| have.contains(&a.id.0)) {
            out.approx.insert(a.id.0.clone(), s);
            out.albums.push(a);
        }
    }
    if out.songs.len() < THIN {
        let have: Vec<String> = out.songs.iter().map(|s| s.id.0.clone()).collect();
        for (s, so) in ix.songs.search(&q, PER_KIND, THRESHOLD, |s| have.contains(&s.id.0)) {
            out.approx.insert(so.id.0.clone(), s);
            out.songs.push(so);
        }
    }
    out
}

/// The shared slot the handler reads and the refresher writes. Readers clone the
/// `Arc` out and score lock-free; a rebuild swaps it whole.
#[derive(Default)]
pub struct Slot {
    index: RwLock<Option<Arc<FuzzyIndex>>>,
}

impl Slot {
    pub fn get(&self) -> Option<Arc<FuzzyIndex>> {
        self.index.read().unwrap().clone()
    }

    pub fn set(&self, ix: FuzzyIndex) {
        *self.index.write().unwrap() = Some(Arc::new(ix));
    }
}

/// Fetch the whole name list: getArtists, getAlbumList2 paged, empty-query search3
/// paged. Each kind stops at [`MAX_ENTRIES`].
pub async fn fetch(client: &crate::subsonic::SubsonicClient) -> Result<FuzzyIndex, crate::subsonic::SubsonicError> {
    const PAGE: i32 = 500;
    let scan_count = client.scan_status().await.ok().and_then(|(_, c)| c);
    let artists = client.artists().await?;
    let mut albums = Vec::new();
    loop {
        let page = client
            .album_list(opensubsonic::AlbumListType::AlphabeticalByName, Some(PAGE), Some(albums.len() as i32))
            .await?;
        let n = page.len();
        albums.extend(page);
        if n < PAGE as usize || albums.len() >= MAX_ENTRIES {
            break;
        }
    }
    let mut songs = Vec::new();
    loop {
        let page = client.all_songs_page(PAGE, songs.len() as i32).await?;
        let n = page.len();
        songs.extend(page);
        if n < PAGE as usize || songs.len() >= MAX_ENTRIES {
            break;
        }
    }
    Ok(FuzzyIndex::build(artists, albums, songs, scan_count))
}

/// How often the refresher asks `getScanStatus` whether the library moved.
pub const POLL: Duration = Duration::from_secs(10 * 60);
/// Rebuild at least this often regardless (a rename keeps the count the same).
pub const MAX_AGE: Duration = Duration::from_secs(6 * 60 * 60);

/// Should a built index be replaced? Pure, so the policy is testable without a clock.
pub fn needs_rebuild(built: Option<(Option<i64>, Duration)>, scanning: bool, count: Option<i64>) -> bool {
    match built {
        None => true,
        Some(_) if scanning => false,
        Some((was, age)) => age >= MAX_AGE || (count.is_some() && count != was),
    }
}

/// The background refresher: build now, then poll the scan status and rebuild after a
/// finished rescan changed the item count, or when the index is [`MAX_AGE`] old.
pub async fn run<C: crate::clock::Clock>(slot: Arc<Slot>, client: Arc<crate::subsonic::SubsonicClient>, clock: C) {
    loop {
        let cur = slot.get();
        let (scanning, count) = client.scan_status().await.unwrap_or((false, None));
        let built = cur.as_ref().map(|ix| (ix.scan_count, ix.built_at.elapsed()));
        if needs_rebuild(built, scanning, count) {
            let started = Instant::now();
            match fetch(&client).await {
                Ok(ix) => {
                    let (a, b, s) = ix.len();
                    tracing::info!(artists = a, albums = b, songs = s, ms = started.elapsed().as_millis() as u64, "fuzzy name index built");
                    slot.set(ix);
                }
                Err(e) => tracing::warn!(error = %e, "fuzzy name index fetch failed; searchall stays server-only"),
            }
        }
        clock.sleep_until(clock.now() + POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ArtistId;

    fn artist(id: &str, name: &str) -> Artist {
        Artist { id: ArtistId(id.into()), name: name.into(), album_count: 1, starred: false, cover_art: None }
    }

    fn ix(names: &[&str]) -> FuzzyIndex {
        let a = names.iter().enumerate().map(|(i, n)| artist(&i.to_string(), n)).collect();
        FuzzyIndex::build(a, vec![], vec![], None)
    }

    #[test]
    fn fold_strips_accents_apostrophes_and_splits_digits() {
        assert_eq!(fold("Björk"), "bjork");
        assert_eq!(fold("Kész az egész"), "kesz az egesz");
        assert_eq!(fold("Sakurazaka46"), "sakurazaka 46");
        assert_eq!(fold("Don\u{2019}t Loose"), "dont loose");
        assert_eq!(fold("Bonnie 'Prince' Billy"), "bonnie prince billy");
        assert_eq!(fold("Straße"), "strasse");
    }

    #[test]
    fn typo_finds_the_artist_and_nonsense_finds_nothing() {
        let i = ix(&["Massive Attack, Mos Def", "Amon Tobin", "Tosca", "The Rolling Stones"]);
        let r = augment("masive attack", vec![], vec![], vec![], Some(&i));
        assert_eq!(r.artists.first().map(|a| a.name.as_str()), Some("Massive Attack, Mos Def"));
        assert!(r.approx.contains_key("0"));
        let r = augment("rollingstones", vec![], vec![], vec![], Some(&i));
        assert_eq!(r.artists.first().map(|a| a.name.as_str()), Some("The Rolling Stones"));
        let r = augment("xqzv plorth", vec![], vec![], vec![], Some(&i));
        assert!(r.artists.is_empty(), "nonsense must not produce approximate hits");
    }

    #[test]
    fn enough_real_hits_leave_the_reply_untouched() {
        let i = ix(&["Tosca", "Toska", "Tosco"]);
        let real = vec![artist("a", "x"), artist("b", "y"), artist("c", "z")];
        let r = augment("tosca", real, vec![], vec![], Some(&i));
        assert_eq!(r.artists.len(), 3);
        assert!(r.approx.is_empty());
    }

    #[test]
    fn a_real_hit_is_never_duplicated_as_approximate() {
        let i = ix(&["Amon Tobin"]);
        let r = augment("amon tobn", vec![artist("0", "Amon Tobin")], vec![], vec![], Some(&i));
        assert_eq!(r.artists.len(), 1);
        assert!(r.approx.is_empty());
    }

    #[test]
    fn no_index_means_server_only() {
        let r = augment("masive", vec![], vec![], vec![], None);
        assert!(r.artists.is_empty() && r.approx.is_empty());
    }

    #[test]
    fn rebuild_policy() {
        let fresh = Duration::from_secs(1);
        assert!(needs_rebuild(None, true, None));
        assert!(!needs_rebuild(Some((Some(10), fresh)), false, Some(10)));
        assert!(needs_rebuild(Some((Some(10), fresh)), false, Some(11)));
        assert!(!needs_rebuild(Some((Some(10), fresh)), true, Some(11)), "wait for the scan to finish");
        assert!(needs_rebuild(Some((Some(10), MAX_AGE)), false, Some(10)));
        assert!(!needs_rebuild(Some((Some(10), fresh)), false, None), "an unknown count is not a change");
    }
}
