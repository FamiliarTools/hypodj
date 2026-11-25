//! `djc store` - the human surface over the offline mirror and the ranking that
//! decides what fits in it.
//!
//! WHY THIS EXISTS AT ALL: the daemon has answered `store` for a while, but nothing
//! he runs could ask. `djc` and `dj-gui` had no store command, and the client library
//! parses only the one-line `X-Store` badge - so the deferred list, and now the
//! reason each group lost, were reachable only by hand-writing MPD over `nc`. A
//! ranking he cannot interrogate is one he cannot tell a good decision from a bug in,
//! which is the whole point of ranking rather than taking the pin set in arrival
//! order.
//!
//! Like `stations` and `heard`, this is intercepted BEFORE `route()` and for the
//! identical stated reason: `route` is a pure verb-vs-NL split shared with dj-gui, so
//! `store frontier` left to the fallthrough would be handed to the NL translator,
//! which has no store action at all.
//!
//! It is a PRINTER. Every number and every sentence is rendered daemon-side - the
//! badge line, the rule, the per-group reason - and this module puts them on stdout
//! in order. One formatter, so `djc store` and a future dj-gui panel can never
//! disagree about the same mirror.

use hypodj_client::mpd::{MpdConn, MpdError};
use hypodj_client::model;
use hypodj_client::route::parse_size;

const USAGE: &str = "\
usage:
  djc store                what is held, what is still coming, and every favourite
                            that did not fit
  djc store limit <size>   set how much disk the mirror may use (e.g. 24G, 500M).
                            \"djc store limit default\" goes back to the configured cap
  djc store frontier       the whole ranked order, best to worst, with the evidence
                            each group was ranked on - what a refused group lost to
                            is the line above it
  djc store now            run a full reconcile pass now instead of waiting
  djc store pause          suspend bulk mirroring for this daemon process
  djc store resume         resume it (and kick a pass)
  djc store albums         what plays offline right now, by album (no server needed)
  djc store play <words>   play the held album whose name matches <words>

pause/resume/now/limit are ACTIONS and are words only; the views are read-only.
Sizes are binary (24G is 24 GiB) and are clamped up to 64 MiB, never rejected.";

/// Run the `store` gesture. `words` are the argv tokens AFTER the leading `store`.
///
/// Returns `Ok(true)` when the daemon answered. A bad subcommand prints the usage and
/// returns `Ok(false)` (which the caller maps to a non-zero exit) rather than quietly
/// showing the default view, which would let a typo pass as an answer.
pub fn run(conn: &mut MpdConn, words: &[String]) -> Result<bool, MpdError> {
    match words.split_first() {
        Some((w, [])) if w == "albums" => return albums(conn),
        Some((w, rest)) if w == "play" && !rest.is_empty() => return play(conn, &rest.join(" ")),
        _ => {}
    }
    let Some(line) = verb_line(words) else {
        eprintln!("{USAGE}");
        return Ok(false);
    };
    // The one-line badge is the daemon's own already-worded summary, and it rides
    // `status` rather than `store` - so read it from there instead of re-deriving a
    // second wording here out of the raw byte counts.
    let badge = model::now_playing(&conn.command("status")?, &[]).store;
    let pairs = match conn.command(&line) {
        Ok(p) => p,
        Err(MpdError::Ack(msg)) => {
            // The live daemon can lag master by several switches, and `store` is
            // deliberately absent from the `commands` advertisement, so an unknown
            // command here is a DEPLOY fact rather than a failure. Say which.
            println!("could not read the store: {msg}");
            return Ok(false);
        }
        Err(e) => return Err(e),
    };

    let find = |k: &str| -> Option<&str> {
        pairs
            .iter()
            .find(|(pk, _)| pk == k)
            .map(|(_, v)| v.as_str())
    };
    match find("X-Store") {
        // A real, permanent state, not a failure: no store configured, no state
        // directory, or a directory the daemon could not use.
        Some("off") => {
            println!("store: off (no offline mirror configured)");
            return Ok(true);
        }
        // The reconciler has not completed a full pass with an authoritative pin set,
        // so every number would be invented and the frontier does not exist yet.
        Some("starting") => {
            println!("store: starting (no full pass has published yet)");
            return Ok(true);
        }
        _ => {}
    }

    match badge {
        Some(b) => println!("store: {b}"),
        None => println!("store: (no summary yet)"),
    }
    // THE LIMIT AND WHOSE IT IS, always - it is the one number on this view the user
    // can change, and the badge above now points at it by name ("did not fit in
    // 16.0 GiB"). Saying which source is in force is what stops a configured cap
    // reading as immovable.
    if let Some(limit) = find("X-StoreLimit").and_then(|s| s.parse::<u64>().ok()) {
        let source = match find("X-StoreLimitSource") {
            Some("user") => "set by you - \"djc store limit default\" to undo",
            _ => "from the config - change it with \"djc store limit <size>\"",
        };
        println!("limit: {} ({source})", human(limit));
    }
    // The RULE only comes back on the frontier view now, so this prints itself
    // exactly where it is the question and nowhere else.
    if let Some(rule) = find("X-StoreRule") {
        println!("rule: {rule}");
    }
    if find("X-StorePaused") == Some("1") {
        println!("bulk mirroring is PAUSED (djc store resume)");
    }

    // The daemon's lines, verbatim and in order. `Frontier` is the whole ranked walk
    // and `Deferred` only what lost; a view asks for one or the other, never both.
    let mut printed = 0usize;
    for (k, v) in &pairs {
        if k == "Frontier" {
            println!("  {v}");
            printed += 1;
        }
    }
    let deferred: Vec<&String> = pairs
        .iter()
        .filter(|(k, _)| k == "Deferred")
        .map(|(_, v)| v)
        .collect();
    if !deferred.is_empty() {
        println!("deferred ({}):", deferred.len());
        for v in deferred {
            println!("  {v}");
        }
        printed += 1;
    }
    // The songs the SERVER'S INDEX mis-sizes (task 01m2300p), by name - before this
    // only journalctl knew which ones. Their own section because the fix is on the
    // server, not in hypodj: no limit or retry here can change them.
    let mis: Vec<&String> =
        pairs.iter().filter(|(k, _)| k == "SizeMismatch").map(|(_, v)| v).collect();
    if !mis.is_empty() {
        println!("mis-sized by the server ({}) - a server rescan fixes these:", mis.len());
        for v in mis {
            println!("  {v}");
        }
    }
    // Held, but the server's index still mis-sizes them (task 01m86ehm): the copy
    // is good, the index is stale. Only a FULL rescan fixes it (a quick scan does
    // not look at size).
    if let Some(n) = pairs
        .iter()
        .find(|(k, _)| k == "X-StoreStaleIndex")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
    {
        println!(
            "{n} held song{} the server's index mis-sizes - a full server rescan fixes it",
            if n == 1 { "" } else { "s" }
        );
    }
    // A frontier view with no groups is a real answer (an empty pin set), and so is a
    // store with nothing deferred. Say so rather than ending on a bare summary line,
    // which reads as truncated output.
    if printed == 0 {
        println!(
            "{}",
            if words.first().map(String::as_str) == Some("frontier") {
                "no pin groups (nothing is starred)"
            } else {
                "nothing deferred - every favourite fits"
            }
        );
    }
    Ok(true)
}

/// One held album as the daemon listed it: the worded row and its `album/<id>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldAlbum {
    pub line: String,
    pub uri: Option<String>,
}

/// Zip the daemon's `OfflineAlbum` rows with the `OfflineAlbumUri` that follows
/// each. Pure, so the pairing is testable without a socket.
pub fn held_albums(pairs: &[(String, String)]) -> Vec<HeldAlbum> {
    let mut out: Vec<HeldAlbum> = Vec::new();
    for (k, v) in pairs {
        match k.as_str() {
            "OfflineAlbum" => out.push(HeldAlbum { line: v.clone(), uri: None }),
            "OfflineAlbumUri" if v != "-" => {
                if let Some(last) = out.last_mut() {
                    last.uri = Some(v.clone());
                }
            }
            _ => {}
        }
    }
    out
}

/// The held albums a phrase names: every row containing all of its words,
/// case-blind. A row that matches the phrase EXACTLY (ignoring the song count)
/// wins alone, so a short album name is never ambiguous with a longer one.
pub fn match_albums<'a>(albums: &'a [HeldAlbum], phrase: &str) -> Vec<&'a HeldAlbum> {
    let want: Vec<String> = phrase.split_whitespace().map(str::to_lowercase).collect();
    let hits: Vec<&HeldAlbum> = albums
        .iter()
        .filter(|a| {
            let l = a.line.to_lowercase();
            want.iter().all(|w| l.contains(w.as_str()))
        })
        .collect();
    let p = phrase.trim().to_lowercase();
    let exact: Vec<&HeldAlbum> = hits
        .iter()
        .copied()
        .filter(|a| {
            let l = a.line.to_lowercase();
            let name = l.rsplit_once(" (").map_or(l.as_str(), |(n, _)| n);
            name == p || name.rsplit_once(" - ").is_some_and(|(_, album)| album == p)
        })
        .collect();
    if exact.len() == 1 { exact } else { hits }
}

/// `djc store albums`: what plays offline right now, grouped by album, plus how to
/// play one. Read from the daemon's mirror alone, so it answers with the server
/// unreachable - the moment the question is asked.
fn albums(conn: &mut MpdConn) -> Result<bool, MpdError> {
    let pairs = match conn.command("store albums") {
        Ok(p) => p,
        Err(MpdError::Ack(msg)) => {
            println!("could not read the offline albums: {msg}");
            return Ok(false);
        }
        Err(e) => return Err(e),
    };
    if let Some((_, head)) = pairs.iter().find(|(k, _)| k == "OfflineSummary") {
        println!("{head}");
    }
    print_held(&held_albums(&pairs));
    Ok(true)
}

/// The rows and, when there are any, the one line that says how to play one.
pub fn print_held(albums: &[HeldAlbum]) {
    for a in albums {
        println!("  {}", a.line);
    }
    if albums.iter().any(|a| a.uri.is_some()) {
        println!("play one: djc store play <album name>");
    }
}

/// `djc store play <words>`: add the held album the words name and start it. The
/// daemon's `add album/<id>` falls back to the songs the mirror holds when the
/// server cannot be reached, so this works offline - which is its point.
fn play(conn: &mut MpdConn, phrase: &str) -> Result<bool, MpdError> {
    let all = held_albums(&conn.command("store albums")?);
    let hits = match_albums(&all, phrase);
    let target = match hits.as_slice() {
        [] => {
            println!("no offline album matches \"{phrase}\" - djc store albums lists them");
            return Ok(false);
        }
        [one] => *one,
        many => {
            println!("\"{phrase}\" matches {} offline albums - say more:", many.len());
            for a in many {
                println!("  {}", a.line);
            }
            return Ok(false);
        }
    };
    let Some(uri) = target.uri.as_deref() else {
        println!("{} has no album id, so it cannot be played as an album", target.line);
        return Ok(false);
    };
    let added = conn.command(&format!("addid {uri}"))?;
    let Some(id) = added.iter().find(|(k, _)| k == "Id").map(|(_, v)| v.clone()) else {
        println!("the daemon did not say where it queued {}", target.line);
        return Ok(false);
    };
    conn.command(&format!("playid {id}"))?;
    println!("playing {}", target.line);
    Ok(true)
}

/// Map the argv tail to the ONE daemon verb line, or None when it names no view.
///
/// Pure, so the whole argv surface is testable without a socket.
fn verb_line(words: &[String]) -> Option<String> {
    let (first, rest) = match words.split_first() {
        None => return Some("store".to_string()),
        Some((f, r)) => (f.as_str(), r),
    };
    match first {
        "status" | "show" if rest.is_empty() => Some("store".to_string()),
        // Both spellings of the one new VIEW: the subcommand word the daemon's own
        // grammar uses, and the flag a hand reaches for at a shell.
        "frontier" | "--frontier" | "-f" if rest.is_empty() => Some("store frontier".to_string()),
        // The three ACTIONS. Word-only on purpose, exactly as `heard keep`/`drop` are:
        // every view above is safe to mistype into (you read the wrong thing and look
        // again), while these change what the daemon is doing.
        word @ ("pause" | "resume" | "now") if rest.is_empty() => Some(format!("store {word}")),
        // THE SIZE IS PARSED HERE, not daemon-side, so the wire carries one
        // unambiguous integer. Two clients spelling `G` differently (10^9 vs 2^30)
        // would otherwise set two different caches and both look right.
        "limit" if rest.len() == 1 => match rest[0].to_ascii_lowercase().as_str() {
            "default" => Some("store limit default".to_string()),
            s => parse_size(s).map(|n| format!("store limit {n}")),
        },
        // Bare `limit` is a READ and the daemon answers it with the ordinary view.
        "limit" if rest.is_empty() => Some("store".to_string()),
        _ => None,
    }
}

/// Bytes as a person reads them, matching the daemon's own badge formatting so the
/// `limit:` line and the "did not fit in N" clause above it cannot print the same
/// number two ways.
fn human(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    if n >= 1024 * MIB {
        let g = n as f64 / (1024.0 * MIB as f64);
        // Matches the daemon's own rule, INCLUDING dropping a trailing ".0". Shipping
        // without this printed `did not fit in 16 GiB` on the badge and
        // `limit: 16.0 GiB` two lines below it - one number, two spellings, on the one
        // view whose job is to make that number feel changeable.
        if (g - g.round()).abs() < 0.05 {
            format!("{} GiB", g.round() as u64)
        } else {
            format!("{g:.1} GiB")
        }
    } else if n >= MIB {
        format!("{} MiB", n / MIB)
    } else {
        format!("{} KiB", n / KIB)
    }
}


#[cfg(test)]
mod tests {

    fn p(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn held_albums_zip_each_row_with_its_uri() {
        let pairs = p(&[
            ("OfflineSummary", "2 albums play offline (4 songs)"),
            ("OfflineAlbum", "Fixture Band - Held Harbour (3 songs)"),
            ("OfflineAlbumUri", "album/a1"),
            ("OfflineAlbum", "Loose (1 song)"),
            ("OfflineAlbumUri", "-"),
        ]);
        let got = held_albums(&pairs);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].uri.as_deref(), Some("album/a1"));
        assert_eq!(got[1].uri, None);
    }

    #[test]
    fn a_phrase_picks_the_album_and_an_exact_name_beats_a_longer_one() {
        let rows = |ls: &[&str]| -> Vec<HeldAlbum> {
            ls.iter().map(|l| HeldAlbum { line: l.to_string(), uri: Some("album/x".into()) }).collect()
        };
        let a = rows(&["Fixture Band - Held Harbour (3 songs)", "Other - Summit (2 songs)"]);
        assert_eq!(match_albums(&a, "harbour").len(), 1);
        assert_eq!(match_albums(&a, "HELD harbour")[0].line, a[0].line);
        assert!(match_albums(&a, "jazz").is_empty());
        let b = rows(&["X - Blue (2 songs)", "X - Blue Train (5 songs)"]);
        assert_eq!(match_albums(&b, "blue").len(), 1, "exact album name wins");
        assert_eq!(match_albums(&b, "blue")[0].line, "X - Blue (2 songs)");
        assert_eq!(match_albums(&b, "x").len(), 2, "a vague phrase stays ambiguous");
    }

    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn line(items: &[&str]) -> Option<String> {
        verb_line(&v(items))
    }

    #[test]
    fn bare_store_is_the_default_view_and_frontier_takes_both_spellings() {
        assert_eq!(line(&[]), Some("store".to_string()));
        assert_eq!(line(&["status"]), Some("store".to_string()));
        assert_eq!(line(&["show"]), Some("store".to_string()));
        assert_eq!(line(&["frontier"]), Some("store frontier".to_string()));
        assert_eq!(line(&["--frontier"]), line(&["frontier"]));
        assert_eq!(line(&["-f"]), Some("store frontier".to_string()));
    }

    #[test]
    fn the_three_nudges_are_word_only_actions() {
        assert_eq!(line(&["pause"]), Some("store pause".to_string()));
        assert_eq!(line(&["resume"]), Some("store resume".to_string()));
        assert_eq!(line(&["now"]), Some("store now".to_string()));
        // A flag spelling would blur an ACTION into the read-only views beside it.
        assert_eq!(line(&["--pause"]), None);
        assert_eq!(line(&["--now"]), None);
    }

    #[test]
    fn limit_parses_human_sizes_as_binary_and_refuses_nonsense() {
        assert_eq!(parse_size("24G"), Some(24 * (1 << 30)));
        assert_eq!(parse_size("24g"), Some(24 * (1 << 30)));
        // BINARY, not decimal. A `24G` that meant 24_000_000_000 would disagree with
        // the "22.4 GiB" the badge would then print for the very same setting.
        assert_ne!(parse_size("24G"), Some(24_000_000_000));
        assert_eq!(parse_size("500M"), Some(500 * (1 << 20)));
        assert_eq!(parse_size("1.5G"), Some(1536 * (1 << 20)), "fractions are allowed");
        // Longest suffix wins, or `64mib` would parse as `64mi` bytes.
        assert_eq!(parse_size("64MiB"), Some(64 * (1 << 20)));
        assert_eq!(parse_size("1TiB"), Some(1 << 40));
        // A bare integer is bytes: useless at this scale but honest, and the daemon
        // clamps it up to its floor rather than refusing.
        assert_eq!(parse_size("1048576"), Some(1024 * 1024));
        // Nonsense must not become a number the user did not mean.
        assert_eq!(parse_size("big"), None);
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("-4G"), None);
        assert_eq!(parse_size("0"), None);
        assert_eq!(parse_size("0G"), None);
        assert_eq!(parse_size("G"), None);
    }

    #[test]
    fn limit_is_a_word_action_that_sends_bytes_and_default_clears() {
        assert_eq!(line(&["limit", "24G"]), Some(format!("store limit {}", 24u64 << 30)));
        assert_eq!(line(&["limit", "default"]), Some("store limit default".to_string()));
        assert_eq!(line(&["limit", "DEFAULT"]), line(&["limit", "default"]));
        // Bare `limit` READS, and reading is the ordinary view - there is one place
        // the current size is reported, not two that could drift.
        assert_eq!(line(&["limit"]), Some("store".to_string()));
        // An unparseable size is a usage error, never a silently-sent number.
        assert_eq!(line(&["limit", "lots"]), None);
        assert_eq!(line(&["limit", "24G", "extra"]), None);
    }

    #[test]
    fn a_view_that_names_nothing_is_a_usage_error_not_a_silent_default() {
        // Falling back to the bare view would let a typo pass as an answer: he would
        // ask for the ranking, get the summary, and read "no reasons" as the truth
        // about the ranking rather than about his spelling.
        assert_eq!(line(&["frontiers"]), None);
        assert_eq!(line(&["why"]), None);
        assert_eq!(line(&["frontier", "extra"]), None);
        assert_eq!(line(&["pause", "now"]), None);
        assert_eq!(line(&["status", "please"]), None);
    }
}
