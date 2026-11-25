//! Pure view models: MPD pairs in, rows with their words out. No GTK here, so every
//! word the app shows is unit-tested without a display.

use hypodj_client::model::{find, group_blocks_on, QueueItem};

/// One album row on the Albums page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumRow {
    pub title: String,
    pub artist: Option<String>,
    pub uri: String,
    pub songs: Option<u32>,
    /// Songs of this album on disk, when the store answered (`None` = unknown).
    pub held: Option<u32>,
}

impl AlbumRow {
    /// Availability in words: "on disk", "2 of 5 on disk" or "stream only". `None`
    /// when the store did not answer - unknown is never drawn as "stream only".
    pub fn availability(&self) -> Option<String> {
        let held = self.held?;
        Some(match self.songs {
            _ if held == 0 => "stream only".to_string(),
            Some(n) if n > held => format!("{held} of {n} on disk"),
            _ => "on disk".to_string(),
        })
    }
}

/// One song row (an album's tracks, or a search hit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SongRow {
    pub title: String,
    pub artist: Option<String>,
    pub uri: String,
    /// "on disk" / "stream only" from the daemon's per-row `X-Offline`.
    pub availability: Option<&'static str>,
}

/// What a search hit is, which decides what activating it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitKind {
    Artist,
    Album,
    Song,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub kind: HitKind,
    pub title: String,
    pub subtitle: Option<String>,
    pub uri: String,
}

/// `X-Offline` as words. The daemon emits `1` (held) or `0`; absent means unknown.
pub fn offline_words(pairs: &[(String, String)]) -> Option<&'static str> {
    find(pairs, "X-Offline").map(|v| if v == "1" { "on disk" } else { "stream only" })
}

/// The availability word for a queue row (Up Next).
pub fn queue_availability(item: &QueueItem) -> Option<&'static str> {
    offline_words(&item.details)
}

/// The `store albums` records as `(uri, name, songs on disk)`. Rows with no album id
/// (`-`) carry no uri and cannot be browsed, so they are dropped here.
pub fn held_albums(pairs: &[(String, String)]) -> Vec<(String, String, u32)> {
    let mut out: Vec<(Option<String>, String, u32)> = Vec::new();
    for (k, v) in pairs {
        match k.as_str() {
            "OfflineAlbum" => out.push((None, v.clone(), 0)),
            "OfflineAlbumUri" if v != "-" => {
                if let Some(l) = out.last_mut() {
                    l.0 = Some(v.clone());
                }
            }
            "OfflineAlbumName" => {
                if let Some(l) = out.last_mut() {
                    l.1 = v.clone();
                }
            }
            "OfflineAlbumSongs" => {
                if let Some(l) = out.last_mut() {
                    l.2 = v.parse().unwrap_or(0);
                }
            }
            _ => {}
        }
    }
    out.into_iter().filter_map(|(u, n, s)| Some((u?, n, s))).collect()
}

/// The Albums page from `lsinfo albums/all` (when the server answered) joined with
/// `store albums` (when the store answered). With no listing, the held albums alone
/// are the page, so an unreachable server still leaves the mirror browsable.
pub fn albums(listed: Option<&[(String, String)]>, held: Option<&[(String, String)]>) -> Vec<AlbumRow> {
    let held = held.map(held_albums);
    let held_of = |uri: &str| -> Option<u32> {
        held.as_ref()
            .map(|h| h.iter().find(|(u, _, _)| u == uri).map(|(_, _, s)| *s).unwrap_or(0))
    };
    match listed {
        Some(pairs) => group_blocks_on(pairs, "directory")
            .iter()
            .filter_map(|b| {
                let uri = find(b, "directory")?;
                if !uri.starts_with("album/") {
                    return None;
                }
                Some(AlbumRow {
                    title: find(b, "Album").unwrap_or(uri).to_string(),
                    artist: find(b, "AlbumArtist").or(find(b, "Artist")).map(str::to_string),
                    uri: uri.to_string(),
                    songs: find(b, "X-SongCount").and_then(|v| v.parse().ok()),
                    held: held_of(uri),
                })
            })
            .collect(),
        None => held
            .unwrap_or_default()
            .into_iter()
            .map(|(uri, name, songs)| AlbumRow {
                title: name,
                artist: None,
                uri,
                songs: None,
                held: Some(songs),
            })
            .collect(),
    }
}

/// Song rows from any `file`-blocked listing (`lsinfo album/<id>`).
pub fn songs(pairs: &[(String, String)]) -> Vec<SongRow> {
    group_blocks_on(pairs, "file")
        .iter()
        .filter_map(|b| {
            let uri = find(b, "file")?.to_string();
            Some(SongRow {
                title: find(b, "Title").unwrap_or(&uri).to_string(),
                artist: find(b, "Artist").map(str::to_string),
                availability: offline_words(b),
                uri,
            })
        })
        .collect()
}

/// `searchall` hits: artists, then albums, then songs, so a long song list never
/// buries what is above it. Station rows are left to the TUI for now.
pub fn search_hits(pairs: &[(String, String)]) -> Vec<Hit> {
    let mut blocks: Vec<Vec<(String, String)>> = Vec::new();
    for (k, v) in pairs {
        if k == "directory" || k == "file" {
            blocks.push(Vec::new());
        }
        if let Some(b) = blocks.last_mut() {
            b.push((k.clone(), v.clone()));
        }
    }
    let mut out: Vec<Hit> = Vec::new();
    for b in &blocks {
        let (k, uri) = (&b[0].0, &b[0].1);
        let hit = if k == "file" {
            Hit {
                kind: HitKind::Song,
                title: find(b, "Title").unwrap_or(uri).to_string(),
                subtitle: find(b, "Artist").map(str::to_string),
                uri: uri.clone(),
            }
        } else if uri.starts_with("artist/") {
            Hit {
                kind: HitKind::Artist,
                title: find(b, "Artist").unwrap_or(uri).to_string(),
                subtitle: Some("artist".into()),
                uri: uri.clone(),
            }
        } else if uri.starts_with("album/") {
            Hit {
                kind: HitKind::Album,
                title: find(b, "Album").unwrap_or(uri).to_string(),
                subtitle: find(b, "AlbumArtist").or(find(b, "Artist")).map(str::to_string),
                uri: uri.clone(),
            }
        } else {
            continue;
        };
        out.push(hit);
    }
    out.sort_by_key(|h| match h.kind {
        HitKind::Artist => 0,
        HitKind::Album => 1,
        HitKind::Song => 2,
    });
    out
}

/// The status control in the bottom bar: a short label, a symbolic icon, and the
/// details its popover shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// "Online - 1 of 3 on disk, 2 downloading". Short enough to sit beside the
    /// ask entry; it still ellipsizes rather than clipping.
    pub label: String,
    pub icon: &'static str,
    /// True when the server is not usable: drawn as a banner too.
    pub problem: bool,
    /// The popover lines: the server in words, then the store's own sentence with
    /// every clause the label had to drop.
    pub details: Vec<String>,
}

/// Remove the daemon's fill gauge (` [...]`, a TUI glyph meter) from a clause. The
/// GTK app says the same thing in words, so the glyphs never reach a GTK label.
fn strip_gauge(clause: &str) -> String {
    match clause.find(" [") {
        Some(i) if clause.ends_with(']') => clause[..i].to_string(),
        _ => clause.to_string(),
    }
}

/// The store's head clause in GTK words: `1 of 3 songs - 2 still coming [..]` reads
/// `1 of 3 on disk, 2 downloading`; `all 409 songs` reads `all 409 songs on disk`.
/// An unrecognised head is kept as the daemon wrote it (minus any gauge): a label
/// that says too much is read, one that silently drops the reason is trusted.
pub fn store_short(line: &str) -> Option<String> {
    let head = strip_gauge(line.split(", ").next()?.trim());
    if head.is_empty() {
        return None;
    }
    if head == "nothing being mirrored" {
        return Some("nothing on disk".into());
    }
    if let Some(rest) = head.strip_prefix("all ") {
        return Some(format!("all {rest} on disk"));
    }
    let (counts, coming) = match head.split_once(" - ") {
        Some((c, m)) => (c, m.strip_suffix(" still coming")),
        None => (head.as_str(), None),
    };
    let counts = match counts.strip_suffix(" songs").or_else(|| counts.strip_suffix(" song")) {
        Some(c) if c.contains(" of ") => format!("{c} on disk"),
        _ => return Some(head.clone()),
    };
    Some(match coming {
        Some(n) => format!("{counts}, {n} downloading"),
        None => counts,
    })
}

pub fn status(server: Option<&str>, store: Option<&str>) -> Status {
    let (words, problem) = match server {
        Some(s) => hypodj_client::model::server_words(s),
        None => ("connected".to_string(), false),
    };
    let headline = match server {
        Some("online") | None => "Online".to_string(),
        Some("no-network") => "No network".to_string(),
        Some("unreachable") => "Server unreachable".to_string(),
        Some("error") => "Server error".to_string(),
        Some("unknown") => "Checking server".to_string(),
        Some(_) => {
            let mut w = words.clone();
            if let Some(f) = w.get_mut(0..1) {
                f.make_ascii_uppercase();
            }
            w
        }
    };
    let short = store.and_then(store_short);
    let label = match &short {
        Some(s) => format!("{headline} - {s}"),
        None => headline,
    };
    let mut details = vec![words];
    if let Some(line) = store {
        let full: Vec<String> = line.split(", ").map(|c| strip_gauge(c.trim())).collect();
        details.push(format!("Offline: {}", full.join(", ")));
    }
    Status {
        label,
        icon: if problem { "network-offline-symbolic" } else { "network-server-symbolic" },
        problem,
        details,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn albums_join_the_store_and_never_guess_stream_only_when_it_did_not_answer() {
        let listed = p(&[
            ("directory", "album/1"),
            ("Album", "Kid A"),
            ("X-SongCount", "10"),
            ("directory", "album/2"),
            ("Album", "Blue Train"),
            ("X-SongCount", "5"),
            ("directory", "album/3"),
            ("Album", "Unheld"),
        ]);
        let held = p(&[
            ("OfflineAlbum", "Kid A"),
            ("OfflineAlbumUri", "album/1"),
            ("OfflineAlbumSongs", "10"),
            ("OfflineAlbum", "Blue Train"),
            ("OfflineAlbumUri", "album/2"),
            ("OfflineAlbumSongs", "2"),
        ]);
        let rows = albums(Some(&listed), Some(&held));
        let words: Vec<_> = rows.iter().map(|r| r.availability()).collect();
        assert_eq!(
            words,
            vec![Some("on disk".into()), Some("2 of 5 on disk".into()), Some("stream only".into())]
        );
        let unknown = albums(Some(&listed), None);
        assert!(unknown.iter().all(|r| r.availability().is_none()));
    }

    #[test]
    fn an_unreachable_server_still_lists_what_is_on_disk() {
        let held = p(&[
            ("OfflineAlbum", "Kid A"),
            ("OfflineAlbumUri", "album/1"),
            ("OfflineAlbumName", "Kid A"),
            ("OfflineAlbumSongs", "10"),
            ("OfflineAlbum", "loose"),
            ("OfflineAlbumUri", "-"),
        ]);
        let rows = albums(None, Some(&held));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].uri, "album/1");
        assert_eq!(rows[0].availability().as_deref(), Some("on disk"));
    }

    #[test]
    fn search_puts_artists_and_albums_above_songs() {
        let pairs = p(&[
            ("X-Hits", "song 1 20"),
            ("file", "song/9"),
            ("Title", "Idioteque"),
            ("Artist", "Radiohead"),
            ("directory", "album/1"),
            ("Album", "Kid A"),
            ("directory", "artist/4"),
            ("Artist", "Radiohead"),
        ]);
        let kinds: Vec<_> = search_hits(&pairs).iter().map(|h| h.kind).collect();
        assert_eq!(kinds, vec![HitKind::Artist, HitKind::Album, HitKind::Song]);
    }

    #[test]
    fn status_is_short_words_with_no_tui_glyphs() {
        let live = "1 of 3 songs - 2 still coming [\u{25c9}\u{25ce}\u{25ce}], 0.1 of 0.1 GiB, 301 songs left out";
        let st = status(Some("online"), Some(live));
        assert_eq!(st.label, "Online - 1 of 3 on disk, 2 downloading");
        assert!(!st.problem);
        assert!(st.details.iter().all(|d| !d.contains('[') && !d.contains('\u{25c9}')), "{:?}", st.details);
        assert!(st.details[1].ends_with("301 songs left out"));
        assert_eq!(store_short("all 409 songs, 15.9 GiB").as_deref(), Some("all 409 songs on disk"));
        assert_eq!(store_short("318 of 347 songs, 12.1 of 16.0 GiB").as_deref(), Some("318 of 347 on disk"));
        assert_eq!(store_short("nothing being mirrored").as_deref(), Some("nothing on disk"));
        let down = status(Some("unreachable"), None);
        assert!(down.problem);
        assert_eq!(down.label, "Server unreachable");
        assert_eq!(down.icon, "network-offline-symbolic");
    }
}
