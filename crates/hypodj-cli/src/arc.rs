//! `djc arc` - the listening arcs the daemon reads back out of its heard ledger: the
//! recent sessions with their shape, naming one, and replaying one. A PRINTER over the
//! daemon's own `arc` lines, like `heard`: the ledger lives in the daemon's state dir.
//!
//! Intercepted before `route()` for the same reason as `heard`: route knows no `arc`
//! verb and would hand `arc save 1 Warm Room` to the NL translator.

use hypodj_client::mpd::{MpdConn, MpdError};
use hypodj_client::nl::quote_arg;

const USAGE: &str = "\
usage:
  djc arc                    the recent listening sessions, newest first, with their arc
  djc arc limit <n>          the last n sessions
  djc arc save <n> <name>    name session n and keep it
  djc arc saved              the named arcs
  djc arc play <name|n>      replay a named arc (or session n) - skipped tracks stay out;
                             journaled, so `djc undo` takes it back";

/// Run `djc arc`. `words` are the argv tokens AFTER the leading `arc`.
pub fn run(conn: &mut MpdConn, words: &[String]) -> Result<bool, MpdError> {
    let Some(line) = verb_line(words) else {
        eprintln!("{USAGE}");
        return Ok(false);
    };
    // A replay resolves each track against the library: allow it a while.
    let _ = conn.set_read_timeout(Some(std::time::Duration::from_secs(60)));
    match conn.command(&line) {
        Ok(pairs) => {
            for (k, v) in &pairs {
                if k == "arc" {
                    println!("{v}");
                }
            }
            Ok(true)
        }
        Err(MpdError::Ack(msg)) => {
            println!("arc: {msg}");
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// Map the argv tail to the one daemon verb line, or None on a usage error. Pure.
fn verb_line(words: &[String]) -> Option<String> {
    let (first, rest) = match words.split_first() {
        None => return Some("arc".to_string()),
        Some((f, r)) => (f.as_str(), r),
    };
    let name = rest.join(" ");
    match first {
        "limit" | "-n" if rest.len() == 1 => {
            rest[0].parse::<usize>().ok().filter(|n| *n > 0).map(|n| format!("arc limit {n}"))
        }
        "saved" if rest.is_empty() => Some("arc saved".to_string()),
        "play" if !name.trim().is_empty() => Some(format!("arc play {}", quote_arg(name.trim()))),
        "save" if rest.len() >= 2 => {
            let n = rest[0].parse::<usize>().ok().filter(|n| *n > 0)?;
            Some(format!("arc save {n} {}", quote_arg(rest[1..].join(" ").trim())))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(w: &[&str]) -> Option<String> {
        verb_line(&w.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn argv_maps_to_one_verb_line() {
        assert_eq!(line(&[]), Some("arc".into()));
        assert_eq!(line(&["limit", "3"]), Some("arc limit 3".into()));
        assert_eq!(line(&["saved"]), Some("arc saved".into()));
        assert_eq!(line(&["save", "1", "Warm", "Room"]), Some("arc save 1 \"Warm Room\"".into()));
        assert_eq!(line(&["play", "Warm Room"]), Some("arc play \"Warm Room\"".into()));
        for bad in [&["limit", "0"][..], &["save", "x", "n"], &["save", "1"], &["play"], &["nope"]] {
            assert_eq!(line(bad), None, "{bad:?}");
        }
    }
}
