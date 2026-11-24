//! The pure half of the search provider: what a typed query MEANS, and how a result
//! is named. No D-Bus, no socket, no clock - the service in main.rs owns those and
//! asks this module what to offer, so every decision here is unit-testable.
//!
//! The provider DECIDES what matches (GNOME only forwards the terms), so the
//! synonyms live here. Transport phrasing is not re-invented: it goes through
//! `hypodj_client::route::route`, the same router the `dj` CLI and the dj-gui `:`
//! line use, so "next song" means one thing on every surface. Only the reversible
//! transport verbs are run straight from the search box; an open-ended phrase is
//! offered as "Ask the DJ", which opens `djc <phrase>` in a terminal where the plan is
//! echoed and confirmed before anything is armed.

use hypodj_client::route::{route, Action};

/// Words that address hypodj itself. A query starting with one is explicitly for
/// us, which is what licenses the open-ended "Ask the DJ" row - without it every
/// Activities search would grow an ask row nobody wanted.
const TRIGGERS: &[&str] = &["hypodj", "dj", "music"];

/// How far a louder/quieter nudge moves the level, matching the TUI knob detent
/// doubled (one Activities search is a coarser gesture than a scroll tick).
pub const VOLUME_STEP: i32 = 10;

/// Below this many characters a library query is noise (every album has an "a").
const MIN_LIBRARY_QUERY: usize = 3;

/// One result row, before it is an id on the bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hit {
    /// A reversible transport line, run as-is on activation.
    Command(String),
    /// A relative volume nudge, resolved against the live level on activation.
    Volume(i32),
    /// A library item (artist/album/song uri): enqueued and played on activation.
    Library { uri: String, name: String, detail: String },
    /// An open-ended phrase: opens `dj <phrase>` for echo + confirm, never armed here.
    Ask(String),
}

/// What the query parsed to before any library lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    /// Deterministic rows, already decided.
    pub hits: Vec<Hit>,
    /// The phrase to look up in the library, when it is worth a round trip.
    pub library_query: Option<String>,
}

/// The transport lines safe to run from a search box: each is undone by another
/// keystroke (play/pause/next/previous/volume). `stop` drops the position, but so
/// does the media key; `clear`, plans and anything NL never come through here.
fn is_reversible(cmd: &str) -> bool {
    matches!(cmd, "play" | "pause" | "pause 0" | "stop" | "next" | "previous")
        || cmd.strip_prefix("setvol ").is_some_and(|n| n.parse::<u32>().is_ok_and(|n| n <= 100))
}

/// Single-word verbs offered while still being typed ("nex" -> next). Only the
/// unambiguous transport words, so a prefix never lands on the wrong one.
const PREFIX_VERBS: &[(&str, &str)] = &[
    ("next", "next"),
    ("skip", "next"),
    ("previous", "previous"),
    ("pause", "pause"),
    ("resume", "pause 0"),
    ("louder", "+"),
    ("quieter", "-"),
];

fn volume_phrase(words: &[&str]) -> Option<i32> {
    match words {
        ["louder"] | ["volume", "up"] | ["vol", "up"] | ["turn", "it", "up"] | ["turn", "up"] => {
            Some(VOLUME_STEP)
        }
        ["quieter"] | ["softer"] | ["volume", "down"] | ["vol", "down"] | ["turn", "it", "down"]
        | ["turn", "down"] => Some(-VOLUME_STEP),
        _ => None,
    }
}

/// Decide what a query offers. `terms` are GNOME's whitespace-split terms.
pub fn parse(terms: &[String]) -> Parsed {
    let lowered: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
    let mut words: Vec<&str> = lowered.iter().map(String::as_str).filter(|w| !w.is_empty()).collect();
    let triggered = words.first().is_some_and(|w| TRIGGERS.contains(w));
    if triggered {
        words.remove(0);
    }
    if words.is_empty() {
        return Parsed { hits: Vec::new(), library_query: None };
    }

    if let Some(d) = volume_phrase(&words) {
        return Parsed { hits: vec![Hit::Volume(d)], library_query: None };
    }
    let owned: Vec<String> = words.iter().map(|w| w.to_string()).collect();
    if let Action::Command(cmd) = route(&owned) {
        if is_reversible(&cmd) {
            return Parsed { hits: vec![Hit::Command(cmd)], library_query: None };
        }
    }

    let mut hits = Vec::new();
    if let [w] = words.as_slice() {
        if w.len() >= 3 {
            for (verb, cmd) in PREFIX_VERBS {
                if verb.starts_with(w) && verb != w {
                    hits.push(match *cmd {
                        "+" => Hit::Volume(VOLUME_STEP),
                        "-" => Hit::Volume(-VOLUME_STEP),
                        c => Hit::Command(c.to_string()),
                    });
                    break;
                }
            }
        }
    }
    // The phrase as the user typed it (original case), minus the trigger word.
    let phrase = terms
        .iter()
        .filter(|t| !t.is_empty())
        .skip(usize::from(triggered))
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    if triggered {
        hits.insert(0, Hit::Ask(phrase.clone()));
    }
    // "play radiohead" looks up "radiohead": the verb is the activation, not the name.
    let lookup = match phrase.split_once(' ') {
        Some((first, rest)) if first.eq_ignore_ascii_case("play") => rest.trim().to_string(),
        _ => phrase,
    };
    let library_query = (lookup.chars().count() >= MIN_LIBRARY_QUERY).then_some(lookup);
    Parsed { hits, library_query }
}

/// The result id on the bus. A tab cannot appear in a GNOME search term (terms are
/// whitespace-split), so it is a safe field separator.
pub fn id_of(hit: &Hit) -> String {
    match hit {
        Hit::Command(c) => format!("cmd\t{c}"),
        Hit::Volume(d) => format!("vol\t{d}"),
        Hit::Library { uri, name, detail } => format!("uri\t{uri}\t{name}\t{detail}"),
        Hit::Ask(p) => format!("ask\t{p}"),
    }
}

/// Inverse of [`id_of`]. An id is self-describing so GetResultMetas and
/// ActivateResult need no cache that a second Shell search could race.
pub fn hit_of(id: &str) -> Option<Hit> {
    let mut parts = id.split('\t');
    let kind = parts.next()?;
    match kind {
        "cmd" => parts.next().filter(|c| is_reversible(c)).map(|c| Hit::Command(c.to_string())),
        "vol" => parts.next()?.parse().ok().map(Hit::Volume),
        "uri" => {
            let uri = parts.next()?.to_string();
            let name = parts.next().unwrap_or_default().to_string();
            let detail = parts.next().unwrap_or_default().to_string();
            Some(Hit::Library { uri, name, detail })
        }
        "ask" => parts.next().map(|p| Hit::Ask(p.to_string())),
        _ => None,
    }
}

/// (name, description, themed icon) for a result row.
pub fn meta_of(hit: &Hit) -> (String, String, &'static str) {
    match hit {
        Hit::Command(c) => {
            let (name, icon) = match c.as_str() {
                "play" => ("Play", "media-playback-start-symbolic"),
                "pause" => ("Pause", "media-playback-pause-symbolic"),
                "pause 0" => ("Resume", "media-playback-start-symbolic"),
                "stop" => ("Stop", "media-playback-stop-symbolic"),
                "next" => ("Next track", "media-skip-forward-symbolic"),
                "previous" => ("Previous track", "media-skip-backward-symbolic"),
                _ => ("", "audio-volume-medium-symbolic"),
            };
            if let Some(n) = c.strip_prefix("setvol ") {
                (format!("Volume {n}%"), "hypodj".into(), "audio-volume-medium-symbolic")
            } else {
                (name.to_string(), "hypodj".into(), icon)
            }
        }
        Hit::Volume(d) if *d > 0 => {
            (format!("Louder (+{d})"), "hypodj".into(), "audio-volume-high-symbolic")
        }
        Hit::Volume(d) => (format!("Quieter ({d})"), "hypodj".into(), "audio-volume-low-symbolic"),
        Hit::Library { uri, name, detail } => {
            let icon = if uri.starts_with("artist/") {
                "avatar-default-symbolic"
            } else if uri.starts_with("album/") {
                "media-optical-symbolic"
            } else {
                "audio-x-generic-symbolic"
            };
            let kind = uri.split('/').next().unwrap_or_default();
            let desc = if detail.is_empty() {
                format!("hypodj - play {kind}")
            } else {
                format!("hypodj - play {kind} - {detail}")
            };
            (name.clone(), desc, icon)
        }
        Hit::Ask(p) => (
            format!("Ask the DJ: {p}"),
            "hypodj - opens the plan for confirmation".into(),
            "audio-x-generic-symbolic",
        ),
    }
}

/// Library rows from a `searchall` answer: artists, then albums, then songs, capped
/// so one provider never floods the Shell's overview. Tabs are stripped from the
/// display text because they are the id separator.
pub fn library_hits(pairs: &[(String, String)], cap: usize) -> Vec<Hit> {
    struct Row {
        uri: String,
        name: String,
        artist: String,
    }
    let mut rows: Vec<Row> = Vec::new();
    for (k, v) in pairs {
        match k.as_str() {
            "directory" | "file" => rows.push(Row { uri: v.clone(), name: String::new(), artist: String::new() }),
            "Artist" | "Album" | "Title" => {
                if let Some(r) = rows.last_mut() {
                    let is_artist_row = r.uri.starts_with("artist/");
                    let names_row = match k.as_str() {
                        "Artist" => is_artist_row,
                        "Album" => r.uri.starts_with("album/"),
                        _ => r.uri.starts_with("song/"),
                    };
                    if names_row {
                        r.name = v.clone();
                    } else if k == "Artist" {
                        r.artist = v.clone();
                    }
                }
            }
            _ => {}
        }
    }
    let rank = |u: &str| {
        if u.starts_with("artist/") {
            0
        } else if u.starts_with("album/") {
            1
        } else if u.starts_with("song/") {
            2
        } else {
            3
        }
    };
    rows.retain(|r| rank(&r.uri) < 3 && !r.name.is_empty());
    rows.sort_by_key(|r| rank(&r.uri));
    let clean = |s: &str| s.replace('\t', " ");
    rows.into_iter()
        .take(cap)
        .map(|r| Hit::Library { uri: clean(&r.uri), name: clean(&r.name), detail: clean(&r.artist) })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(q: &str) -> Parsed {
        parse(&q.split_whitespace().map(str::to_string).collect::<Vec<_>>())
    }

    #[test]
    fn transport_phrasing_runs_through_the_shared_router() {
        assert_eq!(p("next song").hits, vec![Hit::Command("next".into())]);
        assert_eq!(p("skip").hits, vec![Hit::Command("next".into())]);
        assert_eq!(p("pause").hits, vec![Hit::Command("pause".into())]);
        assert_eq!(p("resume").hits, vec![Hit::Command("pause 0".into())]);
        assert_eq!(p("vol 30").hits, vec![Hit::Command("setvol 30".into())]);
        assert_eq!(p("dj next").hits, vec![Hit::Command("next".into())], "trigger is stripped");
        assert_eq!(p("Next Song").library_query, None, "a verb is not a library search");
    }

    #[test]
    fn louder_and_quieter_are_relative_nudges() {
        assert_eq!(p("louder").hits, vec![Hit::Volume(VOLUME_STEP)]);
        assert_eq!(p("volume down").hits, vec![Hit::Volume(-VOLUME_STEP)]);
    }

    #[test]
    fn destructive_or_open_ended_lines_never_run_from_the_box() {
        // `clear` routes to a confirm, never a Command; nothing here may arm it.
        assert!(p("clear").hits.iter().all(|h| !matches!(h, Hit::Command(_))));
        assert_eq!(hit_of("cmd\tclear"), None, "a forged id cannot smuggle a line in");
        assert_eq!(hit_of("cmd\tsetvol 900"), None);
    }

    #[test]
    fn a_prefix_offers_the_verb_while_typing() {
        assert_eq!(p("nex").hits, vec![Hit::Command("next".into())]);
        assert_eq!(p("loud").hits, vec![Hit::Volume(VOLUME_STEP)]);
        assert_eq!(p("ne").hits, vec![], "two letters is too little to guess");
    }

    #[test]
    fn a_trigger_word_offers_ask_the_dj_and_keeps_the_typed_case() {
        let got = p("dj something calm for Reading");
        assert_eq!(got.hits, vec![Hit::Ask("something calm for Reading".into())]);
        assert_eq!(got.library_query.as_deref(), Some("something calm for Reading"));
        assert!(p("something calm").hits.is_empty(), "no trigger, no ask row");
        assert_eq!(p("play Radiohead").library_query.as_deref(), Some("Radiohead"));
        assert_eq!(p("dj").hits, vec![], "a bare trigger has nothing to ask");
    }

    #[test]
    fn ids_round_trip() {
        for h in [
            Hit::Command("next".into()),
            Hit::Command("setvol 40".into()),
            Hit::Volume(-10),
            Hit::Library { uri: "album/7".into(), name: "Seed".into(), detail: "Klangstof".into() },
            Hit::Ask("play something calmer".into()),
        ] {
            assert_eq!(hit_of(&id_of(&h)), Some(h));
        }
    }

    #[test]
    fn library_hits_rank_artists_then_albums_then_songs() {
        let pairs: Vec<(String, String)> = [
            ("file", "song/1"),
            ("Title", "Everest"),
            ("Artist", "Klangstof"),
            ("directory", "album/7"),
            ("Album", "Seed"),
            ("Artist", "Klangstof"),
            ("directory", "artist/3"),
            ("Artist", "Klangstof"),
            ("directory", "station/x"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let got = library_hits(&pairs, 5);
        let uris: Vec<_> = got
            .iter()
            .map(|h| match h {
                Hit::Library { uri, .. } => uri.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(uris, ["artist/3", "album/7", "song/1"]);
        assert_eq!(
            got[1],
            Hit::Library { uri: "album/7".into(), name: "Seed".into(), detail: "Klangstof".into() }
        );
        assert_eq!(library_hits(&pairs, 1).len(), 1, "capped");
    }
}
