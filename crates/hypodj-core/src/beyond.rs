//! DISCOVERY BEYOND THE LIBRARY: picking a live stream for the lane he is in.
//!
//! `docs/design/discovery/design.md` section 2 is the why. The ledger already showed
//! that an unowned stream holds him for hours and that `mark` turns into acquisition;
//! the one thing hypodj could not do was PICK the stream. This module is that pick, and
//! nothing else: it never writes to Navidrome, never counts a click at Radio Browser,
//! and never plays anything itself. The handler owns the network calls and the queue;
//! everything here is pure over fetched text, so the whole ranking is unit-tested on
//! captured fixtures in the certless, network-less nix sandbox.
//!
//! Two sources, isolated so one changing shape degrades to the other:
//!
//! - **NTS mixtapes** (`GET /api/v2/mixtapes`). UNOFFICIAL and undocumented: it can
//!   change without notice. Everything that knows its shape is [`nts_candidates`], and
//!   a body that does not parse is reported as a missing source, never as an empty lane.
//!   A mixtape matches a lane when one of the lane's tags appears as a WORD in its title,
//!   subtitle or description - the descriptions are written by people and name genres.
//! - **Radio Browser** (`/json/stations/search?tag=<t>&tagExact=true`). `tagExact` is
//!   load-bearing: plain `tag=` is a substring match and `dub` returns dubstep. The
//!   index is self-declared tags only, so a lane is a short list of TAGS, never an
//!   artist. Server discovery is the round-robin DNS name the API documents
//!   (`all.api.radio-browser.info`). The click counter (`/json/url/<uuid>`) is an
//!   outward write in his name and is never called.
//!
//! A LANE is derived, in order, from what he asked for, the genre of the song playing,
//! the genre of the last finished song, or the commonest genre of his starred set
//! ([`Lane`]). Its tags come from [`lane_tags`], which widens a genre by at most two
//! neighbours so "Trip-Hop" also reaches the `downtempo` stations and "Techno" reaches
//! `detroit techno`, while keeping the two lanes disjoint.
//!
//! Every row is PROBED before it is shown (the handler does the GET; [`is_audio_type`]
//! decides): his own `.pls` collection held an HTML player page and a 503, and a row
//! that does not answer with audio at that moment is dropped rather than offered.

use serde::Deserialize;

/// The NTS mixtapes catalogue (the same document `station_identity` matches against).
pub const NTS_MIXTAPES_URL: &str = crate::station_identity::NTS_MIXTAPES_URL;

/// Radio Browser's documented server-discovery name: a DNS round robin over the mirrors,
/// with a certificate that covers it, so one name stands in for "pick a live server".
pub const RADIO_BROWSER_BASE: &str = "https://all.api.radio-browser.info";

/// The descriptive User-Agent Radio Browser asks every client to send.
pub const USER_AGENT: &str = concat!(
    "hypodj/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/FamiliarTools/hypodj; station suggest)"
);

/// The bitrate floor, kbps. Under it a stream is a phone call, not an evening. A
/// station that reports 0 (unknown) is kept: an HLS or AAC+ entry often does.
pub const MIN_BITRATE: u32 = 128;

/// How many candidates are probed. Probes run concurrently and each is bounded, so this
/// is a cap on outbound connections per ask, not on latency.
pub const PROBE_TOP: usize = 10;

/// How many probed rows a suggestion shows at most.
pub const SHOW_MAX: usize = 8;

/// Radio Browser rows asked for per tag. Ranked by votes server-side, so the head of
/// the list is what survives anyway.
pub const RB_LIMIT_PER_TAG: usize = 40;

/// At most this many tags per lane, so one ask is at most this many Radio Browser GETs.
pub const MAX_LANE_TAGS: usize = 3;

/// Where a lane came from. Printed, so a wrong lane can be traced to its input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneSource {
    /// He named it.
    Asked,
    /// The genre of the song playing now (the label names the song).
    Playing(String),
    /// The genre of the last finished library song.
    LastFinished(String),
    /// The commonest genre of his starred songs.
    Starred,
}

/// A lane: a human label plus the Radio Browser tags (lowercase) it searches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lane {
    pub label: String,
    pub tags: Vec<String>,
    pub source: LaneSource,
}

impl Lane {
    /// Build a lane from a genre or a typed lane word. `None` when the text normalizes to
    /// nothing a station index could match.
    pub fn from_text(text: &str, source: LaneSource) -> Option<Lane> {
        let tags = lane_tags(text);
        if tags.is_empty() {
            return None;
        }
        Some(Lane { label: tags[0].clone(), tags, source })
    }

    /// One line saying what lane this is and why, for the reply.
    pub fn describe(&self) -> String {
        let why = match &self.source {
            LaneSource::Asked => "as asked".to_string(),
            LaneSource::Playing(s) => format!("from what is playing ({s})"),
            LaneSource::LastFinished(s) => format!("from the last song that finished ({s})"),
            LaneSource::Starred => "from the commonest genre of your starred songs".to_string(),
        };
        format!("{} [{}] {why}", self.label, self.tags.join(", "))
    }
}

/// Lowercase, `&` to `and`, `_` and runs of whitespace to one space, and the common
/// spellings of a hyphenated genre ("trip hop", "triphop") folded to the index's own.
pub fn normalize_tag(text: &str) -> String {
    let lower = text.trim().to_lowercase().replace('&', " and ").replace('_', " ");
    let collapsed = lower.split_whitespace().collect::<Vec<_>>().join(" ");
    match collapsed.as_str() {
        "trip hop" | "triphop" | "trip-hop" => "trip-hop".to_string(),
        "chill out" | "chill-out" => "chillout".to_string(),
        "drum and bass" | "drum n bass" | "dnb" | "d and b" => "drum and bass".to_string(),
        "hip hop" | "hiphop" => "hip-hop".to_string(),
        "nu jazz" | "nu-jazz" => "nu jazz".to_string(),
        "lo fi" | "lofi" | "lo-fi" => "lo-fi".to_string(),
        other => other.to_string(),
    }
}

/// The tags a lane searches: the genre itself first, then at most two NEIGHBOURS from a
/// small hand table. The table is the one opinion in this module and it is deliberately
/// short: it exists so a library genre lands on the tag the index actually uses
/// ("Electronic" is useless there; "Trip-Hop" alone has 26 stations, with `downtempo`
/// beside it 127), not to model taste. A genre the table does not know searches only
/// itself.
pub fn lane_tags(text: &str) -> Vec<String> {
    let first = normalize_tag(text);
    if first.is_empty() || first.chars().all(|c| !c.is_alphanumeric()) {
        return Vec::new();
    }
    let neighbours: &[&str] = match first.as_str() {
        "trip-hop" => &["downtempo"],
        "downtempo" => &["trip-hop", "chillout"],
        "chillout" | "chill" | "lounge" => &["downtempo", "chillout"],
        "ambient" | "drone" => &["ambient", "drone"],
        "techno" | "detroit techno" | "minimal techno" => &["detroit techno", "techno"],
        "house" | "deep house" => &["deep house", "house"],
        "acid jazz" | "nu jazz" => &["acid jazz", "nu jazz"],
        "jazz" => &["jazz", "acid jazz"],
        "dub" | "reggae" | "roots reggae" => &["dub", "reggae"],
        "balearic" => &["balearic", "chillout"],
        "drum and bass" | "jungle" => &["drum and bass", "jungle"],
        _ => &[],
    };
    let mut out = vec![first];
    for n in neighbours {
        let n = n.to_string();
        if !out.contains(&n) {
            out.push(n);
        }
        if out.len() >= MAX_LANE_TAGS {
            break;
        }
    }
    out
}

/// An absolute http(s) url: the only shape the stream path plays.
fn is_stream_url(u: &str) -> bool {
    u.starts_with("http://") || u.starts_with("https://")
}

/// MusicBrainz artist search, the bridge when a library song carries NO genre (most of
/// his Tosca does not). DOC: 1 request per second per IP and a meaningful User-Agent;
/// one ask makes at most one call, and the handler caches the answer per artist.
pub const MUSICBRAINZ_BASE: &str = "https://musicbrainz.org/ws/2";

/// The artist-search url for `artist` (exact phrase, best match first).
pub fn musicbrainz_artist_url(base: &str, artist: &str) -> String {
    let mut u = url::Url::parse(&format!("{}/artist/", base.trim_end_matches('/')))
        .unwrap_or_else(|_| url::Url::parse("https://invalid.invalid/").expect("static url"));
    let q = artist.replace('"', " ");
    u.query_pairs_mut()
        .append_pair("query", &format!("artist:\"{}\"", q.trim()))
        .append_pair("fmt", "json")
        .append_pair("limit", "3");
    u.to_string()
}

#[derive(Debug, Deserialize)]
struct MbEnvelope {
    #[serde(default)]
    artists: Vec<MbArtist>,
}

#[derive(Debug, Deserialize)]
struct MbArtist {
    #[serde(default)]
    name: String,
    #[serde(default)]
    score: u32,
    /// `null` and absent both mean "no tags" (the captured search sends `null` for an
    /// untagged namesake, and a strict Vec would fail the whole parse on it).
    #[serde(default)]
    tags: Option<Vec<MbTag>>,
}

#[derive(Debug, Deserialize)]
struct MbTag {
    #[serde(default)]
    count: i64,
    #[serde(default)]
    name: String,
}

/// The genre word a MusicBrainz artist search implies for `artist`: the best-scoring
/// artist whose name matches (case-insensitive) with a score of at least 90, then its
/// most-voted tag that the lane table KNOWS (so "downtempo" wins over a tie with
/// "chillout" only by votes, and a vague "electronic" never wins over a known lane),
/// else its most-voted tag. `None` when nothing confident matched - an Italian pop
/// singer of the same name scores lower and carries no tags.
pub fn mb_lane_word(body: &str, artist: &str) -> Option<String> {
    let env: MbEnvelope = serde_json::from_str(body).ok()?;
    let want = artist.trim().to_lowercase();
    let a = env
        .artists
        .into_iter()
        .filter(|a| {
            a.score >= 90 && a.name.trim().to_lowercase() == want && a.tags.as_ref().is_some_and(|t| !t.is_empty())
        })
        .max_by_key(|a| a.score)?;
    let mut tags = a.tags.unwrap_or_default();
    tags.sort_by(|x, y| y.count.cmp(&x.count));
    let known = tags.iter().find(|t| lane_tags(&t.name).len() > 1).map(|t| t.name.clone());
    known.or_else(|| tags.first().map(|t| t.name.clone()))
}

/// Which source a suggestion came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Nts,
    RadioBrowser,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Nts => "nts",
            Source::RadioBrowser => "radio-browser",
        }
    }
}

/// One candidate station.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub name: String,
    /// The raw stream url, the one `add` plays and the probe fetches.
    pub url: String,
    pub source: Source,
    /// The lane tags this candidate matched.
    pub matched: Vec<String>,
    /// The station's own tags (Radio Browser) or the NTS one-line subtitle.
    pub about: String,
    /// kbps, 0 when unknown.
    pub bitrate: u32,
    /// Radio Browser votes, 0 for NTS.
    pub votes: u64,
}

/// Fold a stream url to the identity used for "already saved" and dedupe: scheme and a
/// trailing slash dropped, host lowercased. Two spellings of one stream compare equal;
/// two different paths never do.
pub fn url_key(url: &str) -> String {
    let rest = url.trim().split_once("://").map(|(_, r)| r).unwrap_or(url.trim());
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    format!("{}{}", host.to_lowercase(), path.trim_end_matches('/'))
}

/// Whether `tag` appears in `text` as a whole word (both lowercase). A hyphenated tag
/// also matches its spaced spelling, so "trip-hop" finds "trip hop". Word boundaries
/// are non-alphanumerics, so "ambient" does not match "ambience" and "house" does not
/// match "houses".
fn has_word(text: &str, tag: &str) -> bool {
    let spellings = [tag.to_string(), tag.replace('-', " ")];
    for t in spellings.iter() {
        let mut from = 0usize;
        while let Some(i) = text[from..].find(t.as_str()) {
            let start = from + i;
            let end = start + t.len();
            let before_ok = text[..start].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
            let after_ok = text[end..].chars().next().is_none_or(|c| !c.is_alphanumeric());
            if before_ok && after_ok {
                return true;
            }
            from = start + t.len().max(1);
            if from >= text.len() {
                break;
            }
        }
    }
    false
}

#[derive(Debug, Deserialize)]
struct NtsEnvelope {
    results: Vec<NtsRow>,
}

#[derive(Debug, Deserialize)]
struct NtsRow {
    title: Option<String>,
    subtitle: Option<String>,
    description: Option<String>,
    audio_stream_endpoint: Option<String>,
}

/// The NTS mixtapes that match `lane`, in catalogue order, or `Err` when the body is
/// not the shape this was written against - which is the signal that the unofficial
/// API moved, and is reported as such rather than as "nothing in this lane".
pub fn nts_candidates(body: &str, lane: &Lane) -> Result<Vec<Suggestion>, String> {
    let env: NtsEnvelope = serde_json::from_str(body)
        .map_err(|e| format!("NTS mixtapes answered in a shape this does not read ({e})"))?;
    if env.results.is_empty() {
        return Err("NTS mixtapes answered with no mixtapes".to_string());
    }
    let mut out = Vec::new();
    for r in env.results {
        let (Some(title), Some(url)) = (r.title, r.audio_stream_endpoint) else { continue };
        if !is_stream_url(&url) {
            continue;
        }
        let subtitle = r.subtitle.unwrap_or_default();
        let text = format!(
            "{} {} {}",
            title.to_lowercase(),
            subtitle.to_lowercase(),
            r.description.unwrap_or_default().to_lowercase()
        );
        let matched: Vec<String> = lane.tags.iter().filter(|t| has_word(&text, t)).cloned().collect();
        if matched.is_empty() {
            continue;
        }
        out.push(Suggestion {
            name: format!("NTS {title}"),
            url,
            source: Source::Nts,
            matched,
            about: subtitle,
            bitrate: 0,
            votes: 0,
        });
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct RbRow {
    #[serde(default)]
    name: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    url_resolved: String,
    #[serde(default)]
    tags: String,
    #[serde(default)]
    bitrate: u32,
    #[serde(default)]
    lastcheckok: u8,
    #[serde(default)]
    votes: u64,
}

/// The Radio Browser search url for ONE exact tag. `hidebroken` asks the server to drop
/// what its own checker found dead; [`rb_candidates`] re-checks `lastcheckok` anyway.
pub fn rb_search_url(base: &str, tag: &str) -> String {
    let mut u = url::Url::parse(&format!("{}/json/stations/search", base.trim_end_matches('/')))
        .unwrap_or_else(|_| url::Url::parse("https://invalid.invalid/").expect("static url"));
    u.query_pairs_mut()
        .append_pair("tag", tag)
        .append_pair("tagExact", "true")
        .append_pair("hidebroken", "true")
        .append_pair("order", "votes")
        .append_pair("reverse", "true")
        .append_pair("limit", &RB_LIMIT_PER_TAG.to_string());
    u.to_string()
}

/// Parse one Radio Browser search body for `tag` into candidates: last check OK, at or
/// above [`MIN_BITRATE`] (or unknown), a real http(s) url, and the tag genuinely in the
/// station's own list (a belt over `tagExact`, so a server-side change to substring
/// matching cannot leak dubstep into dub).
pub fn rb_candidates(body: &str, tag: &str) -> Result<Vec<Suggestion>, String> {
    let rows: Vec<RbRow> = serde_json::from_str(body)
        .map_err(|e| format!("Radio Browser answered in a shape this does not read ({e})"))?;
    let mut out = Vec::new();
    for r in rows {
        if r.lastcheckok != 1 {
            continue;
        }
        if r.bitrate != 0 && r.bitrate < MIN_BITRATE {
            continue;
        }
        let url = if is_stream_url(&r.url_resolved) { r.url_resolved } else { r.url };
        if !is_stream_url(&url) {
            continue;
        }
        let own: Vec<String> = r.tags.split(',').map(normalize_tag).collect();
        if !own.iter().any(|t| t == tag) {
            continue;
        }
        let name = r.name.trim().to_string();
        if name.is_empty() {
            continue;
        }
        out.push(Suggestion {
            name,
            url,
            source: Source::RadioBrowser,
            matched: vec![tag.to_string()],
            about: r.tags,
            bitrate: r.bitrate,
            votes: r.votes,
        });
    }
    Ok(out)
}

/// Merge, dedupe and rank. NTS first (the broadcaster his ledger says he actually uses,
/// in catalogue order), then Radio Browser by votes; a station found under two tags is
/// one row carrying both. Anything whose url is already saved is dropped: a suggestion
/// is something he does not have yet.
pub fn rank(nts: Vec<Suggestion>, rb: Vec<Suggestion>, saved_urls: &[String]) -> Vec<Suggestion> {
    let saved: Vec<String> = saved_urls.iter().map(|u| url_key(u)).collect();
    let mut out: Vec<Suggestion> = Vec::new();
    let mut rb = rb;
    // Stable sort by votes descending; ties keep tag order.
    rb.sort_by(|a, b| b.votes.cmp(&a.votes));
    for s in nts.into_iter().chain(rb) {
        let key = url_key(&s.url);
        if saved.contains(&key) {
            continue;
        }
        if let Some(prev) = out.iter_mut().find(|p| url_key(&p.url) == key) {
            for t in s.matched {
                if !prev.matched.contains(&t) {
                    prev.matched.push(t);
                }
            }
            continue;
        }
        out.push(s);
    }
    out
}

/// Whether a probe's `Content-Type` names something mpv plays as a stream. Audio of any
/// kind, Ogg, and the HLS playlist types; NEVER `text/html` (the player page) or JSON.
/// A missing type is not audio: a server that cannot say what it sends is the dead-URL
/// case the probe exists to catch.
pub fn is_audio_type(content_type: Option<&str>) -> bool {
    let Some(ct) = content_type else { return false };
    let ct = ct.split(';').next().unwrap_or("").trim().to_lowercase();
    ct.starts_with("audio/")
        || ct == "application/ogg"
        || ct == "application/vnd.apple.mpegurl"
        || ct == "application/x-mpegurl"
        || ct == "video/mp2t"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("fixture {}: {e}", p.display()))
    }

    fn lane(t: &str) -> Lane {
        Lane::from_text(t, LaneSource::Asked).expect("a lane")
    }

    #[test]
    fn library_genres_land_on_disjoint_lanes() {
        assert_eq!(lane_tags("Trip-Hop"), vec!["trip-hop", "downtempo"]);
        assert_eq!(lane_tags("trip hop"), vec!["trip-hop", "downtempo"]);
        assert_eq!(lane_tags("Techno"), vec!["techno", "detroit techno"]);
        assert_eq!(lane_tags("Detroit Techno"), vec!["detroit techno", "techno"]);
        let tosca = lane_tags("Downtempo");
        let may = lane_tags("Techno");
        assert!(tosca.iter().all(|t| !may.contains(t)), "{tosca:?} vs {may:?}");
        assert_eq!(lane_tags("Shoegaze"), vec!["shoegaze"], "unknown genres search only themselves");
        assert!(lane_tags("  ").is_empty());
        assert!(lane_tags("--").is_empty());
        assert_eq!(lane_tags("Drum & Bass"), vec!["drum and bass", "jungle"]);
    }

    #[test]
    fn nts_matches_lane_words_in_the_captured_catalogue() {
        let body = fixture("nts-mixtapes-2026-11-26.json");
        let ambient = nts_candidates(&body, &lane("ambient")).unwrap();
        let names: Vec<&str> = ambient.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"NTS Slow Focus"), "{names:?}");
        assert!(!names.contains(&"NTS Field Recordings"), "ambience is not ambient: {names:?}");
        let house = nts_candidates(&body, &lane("house")).unwrap();
        assert!(house.iter().any(|s| s.name == "NTS 4 To The Floor"));
        let down = nts_candidates(&body, &lane("downtempo")).unwrap();
        assert!(down.iter().any(|s| s.name == "NTS Low Key"), "{down:?}");
        let techno = nts_candidates(&body, &lane("techno")).unwrap();
        assert!(techno.iter().all(|s| !down.iter().any(|d| d.url == s.url)));
    }

    #[test]
    fn a_moved_nts_api_is_an_error_not_an_empty_lane() {
        assert!(nts_candidates("<html>maintenance</html>", &lane("ambient")).is_err());
        assert!(nts_candidates("{\"data\": []}", &lane("ambient")).is_err());
        assert!(nts_candidates("{\"results\": []}", &lane("ambient")).is_err());
    }

    #[test]
    fn radio_browser_rows_filter_and_rank() {
        let body = fixture("radio-browser-ambient-2026-11-26.json");
        let rows = rb_candidates(&body, "ambient").unwrap();
        assert!(rows.len() >= 5, "{}", rows.len());
        assert!(rows.iter().all(|r| r.bitrate == 0 || r.bitrate >= MIN_BITRATE));
        assert!(rows.iter().all(|r| r.url.starts_with("http")));
        assert!(rows.iter().all(|r| r.about.to_lowercase().contains("ambient")));
        // The 64 kbps AAC+ chillout stream in the capture does not survive the floor.
        assert!(!rows.iter().any(|r| r.name.contains("0 N - Chillout")));
        assert!(rb_candidates("not json", "ambient").is_err());
    }

    #[test]
    fn exact_tag_is_rechecked_client_side() {
        let body = r#"[{"name":"Dubstep FM","url":"https://x.example/a","url_resolved":"https://x.example/a","tags":"dubstep,edm","bitrate":128,"lastcheckok":1,"votes":9},
                       {"name":"Roots","url":"https://y.example/b","url_resolved":"","tags":"Dub, Reggae","bitrate":192,"lastcheckok":1,"votes":3},
                       {"name":"Dead","url":"https://z.example/c","url_resolved":"","tags":"dub","bitrate":192,"lastcheckok":0,"votes":99}]"#;
        let rows = rb_candidates(body, "dub").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Roots");
    }

    #[test]
    fn rank_drops_saved_dedupes_and_orders() {
        let body = fixture("nts-mixtapes-2026-11-26.json");
        let nts = nts_candidates(&body, &lane("ambient")).unwrap();
        let rb = rb_candidates(&fixture("radio-browser-ambient-2026-11-26.json"), "ambient").unwrap();
        let rb_len = rb.len();
        // His saved "NTS Infinite Mixtapes 2" IS Slow Focus (`/mixtape`), spelled http.
        let saved = vec!["http://stream-mixtape-geo.ntslive.net/mixtape/".to_string()];
        let ranked = rank(nts, rb, &saved);
        assert!(!ranked.iter().any(|s| s.name == "NTS Slow Focus"), "a saved station is not a suggestion");
        assert!(ranked.len() <= rb_len + 1);
        let rb_votes: Vec<u64> =
            ranked.iter().filter(|s| s.source == Source::RadioBrowser).map(|s| s.votes).collect();
        assert!(rb_votes.windows(2).all(|w| w[0] >= w[1]), "{rb_votes:?}");
        let mut keys: Vec<String> = ranked.iter().map(|s| url_key(&s.url)).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), ranked.len());
    }

    #[test]
    fn url_key_folds_spellings_not_paths() {
        assert_eq!(url_key("https://Ice4.SomaFM.com/groovesalad/"), url_key("http://ice4.somafm.com/groovesalad"));
        assert_ne!(url_key("https://s.example/mixtape"), url_key("https://s.example/mixtape4"));
    }

    #[test]
    fn only_audio_passes_the_probe() {
        assert!(is_audio_type(Some("audio/mpeg")));
        assert!(is_audio_type(Some("audio/aacp; charset=x")));
        assert!(is_audio_type(Some("application/ogg")));
        assert!(is_audio_type(Some("application/vnd.apple.mpegurl")));
        assert!(!is_audio_type(Some("text/html; charset=utf-8")));
        assert!(!is_audio_type(Some("application/json")));
        assert!(!is_audio_type(None));
    }

    #[test]
    fn rb_url_asks_for_exact_tags() {
        let u = rb_search_url("https://all.api.radio-browser.info/", "trip-hop");
        assert!(u.starts_with("https://all.api.radio-browser.info/json/stations/search?"));
        assert!(u.contains("tag=trip-hop") && u.contains("tagExact=true") && u.contains("hidebroken=true"));
    }

    #[test]
    fn musicbrainz_tags_bridge_a_genreless_artist() {
        let body = fixture("musicbrainz-artist-tosca-2026-11-26.json");
        assert_eq!(mb_lane_word(&body, "Tosca").as_deref(), Some("downtempo"));
        assert_eq!(mb_lane_word(&body, "Somebody Else"), None);
        assert_eq!(mb_lane_word("<html>", "Tosca"), None);
        let u = musicbrainz_artist_url(MUSICBRAINZ_BASE, "Tosca");
        assert!(u.contains("fmt=json") && u.contains("query=artist"), "{u}");
    }
}
