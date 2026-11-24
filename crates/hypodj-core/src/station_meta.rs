//! STATION METADATA hypodj owns (task wxpnbso, part B).
//!
//! Subsonic's `getInternetRadioStations` schema is `{id, name, streamUrl, homePageUrl}`
//! and nothing else, so a saved station carries no genre, no tags, no description. A
//! search for `house` or `radio` could only ever match a NAME. This module is the
//! metadata hypodj learns for itself, keyed by the stream URL, from two sources:
//!
//! 1. ICY self-description - `icy-genre` / `icy-description`, read from the same mpv
//!    `metadata` map the now-playing line comes from. Measured inconsistent: KFJC sends
//!    `Eclectic`, Radar Lisboa sends the junk sentinel `(null)`, the NTS mixtapes send no
//!    ICY at all. So junk is filtered ([`clean_icy_value`]) and a value is only ever
//!    known AFTER a station has played once.
//! 2. What the station was HEARD playing - every `heard` track row (ICY-split or
//!    recognized) contributes its artist and, from a recognition, its genre. This is
//!    the only source covering a no-ICY stream, and it describes the real music rather
//!    than a self-declared label. Bounded to [`HEARD_CAP`] per station, newest first.
//!
//! The map is PURE data with pure update rules, so the cache is unit-testable; the
//! [`StationMetaStore`] around it owns persistence. Every mutation is a memory move plus
//! a lock-free channel send, because the ICY path is reached from the director spine
//! (the posture `heard.rs` documents): the whole-file rewrite runs on a dedicated task
//! inside `spawn_blocking`, never on the caller.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// How many distinct heard (artist, genre) entries one station keeps. Enough to
/// characterise a rotation, small enough that the whole file stays a few KB.
pub const HEARD_CAP: usize = 40;

/// One thing a station was heard playing. Only what search reads is kept - the full
/// row (title, ISRC, timestamp) lives in the heard ledger already.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HeardTag {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
}

/// What hypodj knows about one station beyond its Subsonic record.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StationMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Newest first, deduplicated, at most [`HEARD_CAP`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub heard: Vec<HeardTag>,
}

impl StationMeta {
    /// Does any learned text contain `query_lower`? Pure; the caller has already
    /// lowercased and trimmed the query.
    pub fn matches(&self, query_lower: &str) -> bool {
        let hit = |s: &Option<String>| s.as_deref().is_some_and(|v| v.to_lowercase().contains(query_lower));
        hit(&self.genre)
            || hit(&self.description)
            || self.heard.iter().any(|h| hit(&h.artist) || hit(&h.genre))
    }
}

/// The whole cache: stream URL to learned metadata.
pub type StationMetaMap = BTreeMap<String, StationMeta>;

/// Normalise one ICY value: trimmed, and `None` for a blank or a known junk sentinel.
/// Servers fill unset fields with placeholder text (`(null)` was measured on Radar
/// Lisboa), and a cached placeholder would make every station match `null`.
pub fn clean_icy_value(v: Option<&str>) -> Option<String> {
    let t = v?.trim();
    let lower = t.to_ascii_lowercase();
    let junk = t.is_empty()
        || matches!(
            lower.as_str(),
            "(null)" | "null" | "none" | "n/a" | "na" | "-" | "unknown" | "unspecified" | "various" | "other" | "default"
        )
        || t.chars().all(|c| !c.is_alphanumeric());
    (!junk).then(|| t.to_string())
}

/// Record ICY self-description for `url`. A `None` (absent or junk) never erases a
/// value learned before: mpv re-fires the map on every title change and a server may
/// omit a header on one connect. Returns whether anything changed.
pub fn record_icy(map: &mut StationMetaMap, url: &str, genre: Option<&str>, description: Option<&str>) -> bool {
    let genre = clean_icy_value(genre);
    let description = clean_icy_value(description);
    if genre.is_none() && description.is_none() {
        return false;
    }
    let e = map.entry(url.to_string()).or_default();
    let mut changed = false;
    if genre.is_some() && e.genre != genre {
        e.genre = genre;
        changed = true;
    }
    if description.is_some() && e.description != description {
        e.description = description;
        changed = true;
    }
    changed
}

/// Record that `url` was heard playing `artist` (and `genre`, from a recognition).
/// Dedupe is by case-folded artist (or genre when there is no artist): a repeat moves
/// to the front, a recognized genre fills a previously genre-less entry, and the list
/// is truncated to [`HEARD_CAP`]. Returns whether anything changed.
pub fn record_heard(map: &mut StationMetaMap, url: &str, artist: Option<&str>, genre: Option<&str>) -> bool {
    let clean = |s: Option<&str>| s.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let artist = clean(artist);
    let genre = clean(genre);
    if artist.is_none() && genre.is_none() {
        return false;
    }
    let key = |t: &HeardTag| {
        t.artist
            .as_deref()
            .map(|a| format!("a:{}", a.to_lowercase()))
            .or_else(|| t.genre.as_deref().map(|g| format!("g:{}", g.to_lowercase())))
    };
    let mut tag = HeardTag { artist, genre };
    let e = map.entry(url.to_string()).or_default();
    let k = key(&tag);
    let before = e.heard.clone();
    if let Some(i) = e.heard.iter().position(|t| key(t) == k) {
        let old = e.heard.remove(i);
        if tag.genre.is_none() {
            tag.genre = old.genre;
        }
    }
    e.heard.insert(0, tag);
    e.heard.truncate(HEARD_CAP);
    e.heard != before
}

/// The cloneable store handle: the map behind a std mutex (never held across an await
/// - every method is sync) plus a dirty signal to the writer task.
#[derive(Clone)]
pub struct StationMetaStore {
    map: Arc<Mutex<StationMetaMap>>,
    dirty: Option<mpsc::UnboundedSender<()>>,
}

impl StationMetaStore {
    /// An in-memory store with no persistence (tests, or no state dir).
    pub fn in_memory() -> Self {
        StationMetaStore { map: Arc::new(Mutex::new(BTreeMap::new())), dirty: None }
    }

    /// Load `path` (a missing or unreadable file is an empty cache, never fatal) and
    /// spawn the writer task that rewrites it after each change. Must run inside a
    /// tokio runtime.
    pub fn open(path: PathBuf) -> Self {
        let initial: StationMetaMap = std::fs::read(&path)
            .ok()
            .and_then(|b| match serde_json::from_slice(&b) {
                Ok(m) => Some(m),
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(), "station metadata unreadable; starting empty");
                    None
                }
            })
            .unwrap_or_default();
        let map = Arc::new(Mutex::new(initial));
        let (tx, mut rx) = mpsc::unbounded_channel::<()>();
        let writer_map = map.clone();
        tokio::spawn(async move {
            while rx.recv().await.is_some() {
                // Coalesce a burst into one rewrite.
                while rx.try_recv().is_ok() {}
                let bytes = {
                    let m = writer_map.lock().unwrap();
                    serde_json::to_vec_pretty(&*m)
                };
                let Ok(bytes) = bytes else { continue };
                let p = path.clone();
                let res = tokio::task::spawn_blocking(move || {
                    if let Some(dir) = p.parent() {
                        std::fs::create_dir_all(dir)?;
                    }
                    crate::resume::atomic_write_bytes(&p, &bytes)
                })
                .await;
                if let Ok(Err(e)) = res {
                    tracing::warn!(error = %e, "station metadata write failed");
                }
            }
        });
        StationMetaStore { map, dirty: Some(tx) }
    }

    fn mutate(&self, f: impl FnOnce(&mut StationMetaMap) -> bool) {
        let changed = f(&mut self.map.lock().unwrap());
        if changed {
            if let Some(tx) = &self.dirty {
                let _ = tx.send(());
            }
        }
    }

    /// See [`record_icy`]. Spine-safe: a memory move and a channel send.
    pub fn note_icy(&self, url: &str, genre: Option<&str>, description: Option<&str>) {
        self.mutate(|m| record_icy(m, url, genre, description));
    }

    /// See [`record_heard`]. Spine-safe.
    pub fn note_heard(&self, url: &str, artist: Option<&str>, genre: Option<&str>) {
        self.mutate(|m| record_heard(m, url, artist, genre));
    }

    /// A clone of the whole map, for a search pass.
    pub fn snapshot(&self) -> StationMetaMap {
        self.map.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn junk_icy_values_are_filtered() {
        assert_eq!(clean_icy_value(Some(" Eclectic ")), Some("Eclectic".into()));
        for junk in ["(null)", "", "   ", "NULL", "-", "unknown", "N/A", "..."] {
            assert_eq!(clean_icy_value(Some(junk)), None, "{junk:?}");
        }
        assert_eq!(clean_icy_value(None), None);
    }

    #[test]
    fn icy_learns_and_junk_never_erases() {
        let mut m = StationMetaMap::new();
        assert!(record_icy(&mut m, "u", Some("Eclectic"), Some("Freeform")));
        assert!(!record_icy(&mut m, "u", Some("Eclectic"), None), "a repeat changes nothing");
        assert!(!record_icy(&mut m, "u", Some("(null)"), Some("")), "junk never erases");
        assert_eq!(m["u"].genre.as_deref(), Some("Eclectic"));
        assert_eq!(m["u"].description.as_deref(), Some("Freeform"));
        assert!(!record_icy(&mut m, "v", Some("(null)"), None));
        assert!(!m.contains_key("v"), "junk alone creates no entry");
    }

    #[test]
    fn heard_dedupes_moves_to_front_and_is_bounded() {
        let mut m = StationMetaMap::new();
        assert!(record_heard(&mut m, "u", Some("Floorplan"), Some("House")));
        assert!(record_heard(&mut m, "u", Some("DJ Koze"), None));
        // A repeat (case-folded) moves to the front and keeps the genre it had.
        assert!(record_heard(&mut m, "u", Some("floorplan"), None));
        assert_eq!(m["u"].heard.len(), 2);
        assert_eq!(m["u"].heard[0].artist.as_deref(), Some("floorplan"));
        assert_eq!(m["u"].heard[0].genre.as_deref(), Some("House"));
        assert!(!record_heard(&mut m, "u", Some("floorplan"), None), "same front entry is no change");
        assert!(!record_heard(&mut m, "u", Some("  "), None));
        for i in 0..(HEARD_CAP + 10) {
            record_heard(&mut m, "u", Some(&format!("a{i}")), None);
        }
        assert_eq!(m["u"].heard.len(), HEARD_CAP);
        assert_eq!(m["u"].heard[0].artist.as_deref(), Some(&*format!("a{}", HEARD_CAP + 9)));
    }

    #[test]
    fn meta_matches_genre_description_and_heard() {
        let mut m = StationMetaMap::new();
        record_icy(&mut m, "k", Some("Eclectic"), Some("KFJC freeform radio"));
        record_heard(&mut m, "n", Some("Floorplan"), Some("House"));
        assert!(m["k"].matches("eclectic"));
        assert!(m["k"].matches("radio"));
        assert!(m["n"].matches("house"));
        assert!(m["n"].matches("floorplan"));
        assert!(!m["n"].matches("radio"));
    }

    #[tokio::test]
    async fn the_store_persists_and_reloads() {
        let dir = std::env::temp_dir().join(format!("hypodj-station-meta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("station_meta.json");
        let s = StationMetaStore::open(path.clone());
        s.note_icy("u", Some("Eclectic"), None);
        s.note_heard("u", Some("Floorplan"), Some("House"));
        // Wait for the writer to land the file (bounded; the write is local).
        for _ in 0..200 {
            if std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<StationMetaMap>(&b).ok())
                .is_some_and(|m| m.get("u").is_some_and(|e| !e.heard.is_empty()))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let reloaded = StationMetaStore::open(path);
        let m = reloaded.snapshot();
        assert_eq!(m["u"].genre.as_deref(), Some("Eclectic"));
        assert_eq!(m["u"].heard[0].artist.as_deref(), Some("Floorplan"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
