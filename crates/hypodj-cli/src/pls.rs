//! A pure parser for the `.pls` playlist format - the shape a curated collection of
//! internet radio streams actually arrives in.
//!
//! `.pls` is a tiny INI-ish file: a `[playlist]` header, `NumberOfEntries`, then
//! `FileN=<url>` / `TitleN=<name>` / `LengthN=<secs>` per entry, and a `Version`.
//! Nothing here touches the network or the filesystem: [`parse_pls`] takes the file's
//! TEXT and returns entries, so every real-world hazard is testable from a string.
//!
//! The rules below are earned from a real 22-file collection, not assumed:
//!
//! - **Line endings are not uniform.** One file in the set is CRLF throughout, so a
//!   split on `\n` leaves a trailing `\r` on its url and its title; every line is
//!   trimmed, which is what keeps that `\r` out of the saved url.
//! - **Values carry `=`.** One `File1` is a player url with a query string
//!   (`?v=...&lowLatency=false`), so a line splits on the FIRST `=` only.
//! - **Titles carry trailing whitespace.** One `Title1` ends in two spaces. The daemon
//!   resolves `add station/<name>` by an EXACT (ASCII-case-folded) name match, so an
//!   untrimmed name could never be typed by a human: trimming is load-bearing, not
//!   cosmetic.
//! - **The final newline is optional.** Most of the set has none; two end with a blank
//!   line.
//! - **Key ORDER is not guaranteed** by the format, so keys are matched by name and
//!   index rather than by position, and `NumberOfEntries` is advisory (a mismatch is
//!   reported, never used to truncate - the entries that ARE there are real).
//!
//! Not every entry is an endless live stream: a `.pls` can legitimately point at a
//! finite archived show recording that will eventually 404, at an HLS `.m3u8` playlist,
//! or at a bare homepage. Parsing says nothing about whether a url plays; that is the
//! player's question, and refusing here would silently drop stations the user keeps.

/// One entry of a `.pls` file: the stream url, plus the display title when the file
/// carries a non-blank one. `title` is `None` rather than a guess so the caller can say
/// what it fell back to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlsEntry {
    pub url: String,
    pub title: Option<String>,
}

/// Parse the TEXT of a `.pls` file into its entries, in ascending entry index.
///
/// Tolerant by design: unknown keys, a missing `[playlist]` header, a missing final
/// newline, CRLF, and a `NumberOfEntries` that disagrees with reality are all survivable.
/// The one thing an entry cannot lack is a non-blank `FileN` - an entry with no url is
/// not a station, so it is dropped rather than emitted as an unplayable row.
pub fn parse_pls(text: &str) -> Vec<PlsEntry> {
    // Keyed by the numeric suffix, so `Title1` before `File1` parses the same as after.
    let mut slots: std::collections::BTreeMap<u32, (Option<String>, Option<String>)> =
        std::collections::BTreeMap::new();

    for raw in text.split('\n') {
        // Trimming the whole line is what makes a CRLF file safe: the `\r` the split
        // left behind is whitespace, so it never reaches a url. It also absorbs the
        // trailing spaces one real `Title1` carries, which matters because the daemon
        // resolves a station name EXACTLY.
        let line = raw.trim();
        if line.is_empty() || line.starts_with('[') || line.starts_with(';') {
            continue;
        }
        // FIRST '=' only: a url value legitimately contains more of them.
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        let value = value.trim();
        let Some((kind, index)) = split_indexed_key(key) else { continue };
        let slot = slots.entry(index).or_default();
        match kind {
            IndexedKey::File => slot.0 = Some(value.to_string()),
            IndexedKey::Title => slot.1 = Some(value.to_string()),
        }
    }

    slots
        .into_values()
        .filter_map(|(url, title)| {
            let url = url.filter(|u| !u.is_empty())?;
            Some(PlsEntry { url, title: title.filter(|t| !t.is_empty()) })
        })
        .collect()
}

/// The `NumberOfEntries` the file CLAIMS, when it states one. Advisory only: the caller
/// reports a disagreement with the parsed count rather than trusting it, because a
/// truncation driven by a wrong header would silently drop a real station.
pub fn declared_entry_count(text: &str) -> Option<usize> {
    for raw in text.split('\n') {
        let line = raw.trim();
        let Some((key, value)) = line.split_once('=') else { continue };
        if key.trim().eq_ignore_ascii_case("NumberOfEntries") {
            return value.trim().parse().ok();
        }
    }
    None
}

/// The two per-entry keys that carry meaning here. `LengthN` is deliberately not
/// modelled: it is `-1` (unknown) for every stream, and a duration a station does not
/// have is not worth carrying.
enum IndexedKey {
    File,
    Title,
}

/// Split `File12` / `title3` into its kind and its entry index. Key names are matched
/// ASCII-case-insensitively (the format does not pin their case); a key with no numeric
/// suffix, or one we do not model, yields `None` and is skipped.
fn split_indexed_key(key: &str) -> Option<(IndexedKey, u32)> {
    let digits_at = key.find(|c: char| c.is_ascii_digit())?;
    let (name, index) = key.split_at(digits_at);
    let index: u32 = index.parse().ok()?;
    let kind = if name.eq_ignore_ascii_case("File") {
        IndexedKey::File
    } else if name.eq_ignore_ascii_case("Title") {
        IndexedKey::Title
    } else {
        return None;
    };
    Some((kind, index))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_ordinary_single_entry_shape() {
        // The shape all 22 real files share, with no trailing newline (14 of them).
        let text = "[playlist]\nNumberOfEntries=1\nFile1=https://stream-mixtape-geo.ntslive.net/mixtape5\nTitle1=NTS 4 To The Floor\nLength1=-1\nVersion=2";
        assert_eq!(
            parse_pls(text),
            vec![PlsEntry {
                url: "https://stream-mixtape-geo.ntslive.net/mixtape5".into(),
                title: Some("NTS 4 To The Floor".into()),
            }]
        );
        assert_eq!(declared_entry_count(text), Some(1));
    }

    #[test]
    fn strips_carriage_returns_from_a_crlf_file() {
        // The real `Savage Radio - The Soul of the Pacific.pls` is CRLF throughout and
        // ends with a blank line. An un-stripped '\r' would ride into the saved url and
        // break the url-match idempotency key forever.
        let text = "[playlist]\r\nNumberOfEntries=1\r\nFile1=http://s2.stationplaylist.com:7178/listen.mp3\r\nTitle1=Savage Radio - The Soul of the Pacific\r\nLength1=-1\r\nVersion=2\r\n\r\n";
        let e = parse_pls(text);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].url, "http://s2.stationplaylist.com:7178/listen.mp3");
        assert!(!e[0].url.contains('\r'));
        assert_eq!(e[0].title.as_deref(), Some("Savage Radio - The Soul of the Pacific"));
    }

    #[test]
    fn trims_a_title_with_trailing_spaces() {
        // The real `nts_live_2.pls` has two trailing spaces. `add station/<name>` matches
        // EXACTLY (after ASCII case folding), so an untrimmed name is unreachable.
        let text = "[playlist]\nNumberOfEntries=1\nFile1=http://stream-relay-geo.ntslive.net/stream2\nTitle1=NTS Radio Live 2 - Los Angeles  \nLength1=-1\nVersion=2\n";
        assert_eq!(
            parse_pls(text)[0].title.as_deref(),
            Some("NTS Radio Live 2 - Los Angeles")
        );
    }

    #[test]
    fn splits_on_the_first_equals_only() {
        // The real `lot_radio.pls` url carries a query string with two more '='.
        let text = "[playlist]\nNumberOfEntries=1\nFile1=https://www.lvpr.tv/?v=85c28sa2o8wppm58&lowLatency=false&muted=false\nTitle1=The Lot Radio - NYC\nLength1=-1\nVersion=2";
        assert_eq!(
            parse_pls(text)[0].url,
            "https://www.lvpr.tv/?v=85c28sa2o8wppm58&lowLatency=false&muted=false"
        );
    }

    #[test]
    fn keeps_a_title_with_a_comma_and_a_url_with_an_explicit_port() {
        // `Moon Mission Recordings, Tokyo Deep and Electronic` is the one comma title;
        // oxigenio/radar carry a redundant explicit `:80` that must round-trip verbatim
        // or the url-match idempotency key breaks.
        let text = "[playlist]\nNumberOfEntries=2\nFile1=http://uk5.internet-radio.com:8306/stream\nTitle1=Moon Mission Recordings, Tokyo Deep and Electronic\nLength1=-1\nFile2=http://proic1.evspt.com:80/oxigenio_aac\nTitle2=Oxigenio 102.6 FM Lisboa\nLength2=-1\nVersion=2";
        let e = parse_pls(text);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].title.as_deref(), Some("Moon Mission Recordings, Tokyo Deep and Electronic"));
        assert_eq!(e[1].url, "http://proic1.evspt.com:80/oxigenio_aac");
    }

    #[test]
    fn keeps_non_ascii_in_a_title_intact() {
        // The real collection happens to be pure ASCII, but a `.pls` grabbed from a
        // directory is not guaranteed to be, and a mangled name is a broken idempotency
        // key. Multibyte characters must survive the trim/split untouched.
        let text = "[playlist]\nNumberOfEntries=1\nFile1=http://proic1.evspt.com:80/oxigenio_aac\nTitle1=Rádio Oxigénio 102.6 FM Lisboa  \nLength1=-1\nVersion=2";
        assert_eq!(
            parse_pls(text)[0].title.as_deref(),
            Some("Rádio Oxigénio 102.6 FM Lisboa")
        );
    }

    #[test]
    fn key_order_does_not_matter_and_entries_come_out_in_index_order() {
        let text = "Version=2\nTitle2=Second\nLength1=-1\nFile2=http://b/\n[playlist]\nTitle1=First\nFile1=http://a/\nNumberOfEntries=2";
        assert_eq!(
            parse_pls(text),
            vec![
                PlsEntry { url: "http://a/".into(), title: Some("First".into()) },
                PlsEntry { url: "http://b/".into(), title: Some("Second".into()) },
            ]
        );
    }

    #[test]
    fn a_missing_or_blank_title_is_none_never_a_guess() {
        let text = "[playlist]\nNumberOfEntries=1\nFile1=http://earthsongradio.com\nLength1=-1\nVersion=2";
        assert_eq!(parse_pls(text)[0].title, None);
        let text = "[playlist]\nNumberOfEntries=1\nFile1=http://earthsongradio.com\nTitle1=   \nVersion=2";
        assert_eq!(parse_pls(text)[0].title, None);
    }

    #[test]
    fn a_declared_count_never_truncates_the_real_entries() {
        // The header is ADVISORY. A file claiming 1 while carrying 2 must still yield
        // both - a wrong header must never silently drop a real station.
        let text = "[playlist]\nNumberOfEntries=1\nFile1=http://a/\nTitle1=A\nFile2=http://b/\nTitle2=B\nVersion=2";
        assert_eq!(parse_pls(text).len(), 2);
        assert_eq!(declared_entry_count(text), Some(1));
    }

    #[test]
    fn an_entry_without_a_url_is_dropped() {
        let text = "[playlist]\nNumberOfEntries=2\nTitle1=Titled but urlless\nFile2=http://b/\nTitle2=B\n";
        assert_eq!(
            parse_pls(text),
            vec![PlsEntry { url: "http://b/".into(), title: Some("B".into()) }]
        );
    }

    #[test]
    fn a_non_playlist_file_yields_nothing() {
        assert!(parse_pls("").is_empty());
        assert!(parse_pls("this is just prose\nwith no keys at all").is_empty());
        // An m3u, the other thing a stream file might be, has no FileN keys.
        assert!(parse_pls("#EXTM3U\nhttp://example.org/stream\n").is_empty());
        assert_eq!(declared_entry_count("no header here"), None);
    }

    /// The parser against his REAL files, gated on `HYPODJ_STATIONS_DIR` so it is a
    /// no-op in the certless, network-less nix sandbox and in any checkout that does not
    /// have the collection. Proves the real bytes without vendoring them into the repo.
    #[test]
    fn real_pls_corpus_parses() {
        let Ok(dir) = std::env::var("HYPODJ_STATIONS_DIR") else { return };
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        let mut names = Vec::new();
        let mut urls = Vec::new();
        let mut files = 0usize;
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("pls") {
                continue;
            }
            files += 1;
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{} is not valid UTF-8 text: {e}", path.display()));
            let parsed = parse_pls(&text);
            assert!(!parsed.is_empty(), "{} parsed to zero entries", path.display());
            for entry in parsed {
                assert!(
                    entry.url.starts_with("http://") || entry.url.starts_with("https://"),
                    "{}: {} is not an http(s) stream url",
                    path.display(),
                    entry.url
                );
                assert_eq!(entry.url.trim(), entry.url, "{}: url carries whitespace", path.display());
                if let Some(t) = entry.title {
                    assert_eq!(t.trim(), t, "{}: title carries whitespace", path.display());
                    names.push(t.to_lowercase());
                }
                urls.push(entry.url);
            }
        }
        // The env var is the gate; once it points somewhere readable, an EMPTY answer is
        // a misconfiguration worth failing on, not a silent pass.
        assert!(files > 0, "{dir} holds no .pls files");
        eprintln!("real_pls_corpus_parses: {files} files, {} entries", urls.len());
        // Both idempotency keys must be unambiguous across the whole collection.
        let unique_names: std::collections::BTreeSet<_> = names.iter().collect();
        assert_eq!(unique_names.len(), names.len(), "two stations share a name");
        let unique_urls: std::collections::BTreeSet<_> = urls.iter().collect();
        assert_eq!(unique_urls.len(), urls.len(), "two stations share a url");
    }
}
