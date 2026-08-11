//! `dj store` - the human surface over the offline mirror and the ranking that
//! decides what fits in it.
//!
//! WHY THIS EXISTS AT ALL: the daemon has answered `store` for a while, but nothing
//! he runs could ask. `dj` and `dj-gui` had no store command, and the client library
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
//! in order. One formatter, so `dj store` and a future dj-gui panel can never
//! disagree about the same mirror.

use hypodj_client::mpd::{MpdConn, MpdError};
use hypodj_client::model;

const USAGE: &str = "\
usage:
  dj store                 the mirror: what is held, what is budgeted, and every
                            favourite that did not fit, each with WHY it lost
  dj store frontier        the whole ranked order, best to worst - what a refused
                            group lost to is the line above it
  dj store now             run a full reconcile pass now instead of waiting
  dj store pause           suspend bulk mirroring for this daemon process
  dj store resume          resume it (and kick a pass)

pause/resume/now are ACTIONS and are words only; the two views are read-only.";

/// Run the `store` gesture. `words` are the argv tokens AFTER the leading `store`.
///
/// Returns `Ok(true)` when the daemon answered. A bad subcommand prints the usage and
/// returns `Ok(false)` (which the caller maps to a non-zero exit) rather than quietly
/// showing the default view, which would let a typo pass as an answer.
pub fn run(conn: &mut MpdConn, words: &[String]) -> Result<bool, MpdError> {
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
    if let Some(rule) = find("X-StoreRule") {
        println!("rule: {rule}");
    }
    if find("X-StorePaused") == Some("1") {
        println!("bulk mirroring is PAUSED (dj store resume)");
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
        _ => None,
    }
}

#[cfg(test)]
mod tests {
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
