//! `dj heard` - the human artifact over the daemon's append-only heard ledger.
//!
//! The always-on half (ICY titles, and a recognition on the streams that announce
//! nothing) writes rows nobody is expected to open. THIS is the surface that is meant
//! to be read: last session, marked rows at the top, unowned first, owned collapsed to
//! a count, repeats collapsed over a window - about the size of the files the user
//! actually kept by hand.
//!
//! The RENDER lives daemon-side and that is forced rather than stylistic: the state
//! directory is mode 0700 under the systemd unit, so this process cannot read the
//! ledger file at all, and the client is pure MPD/TCP with no knowledge of the state
//! dir. So this module is a PRINTER: it turns argv into one `heard` verb line and puts
//! the daemon's lines on stdout, verbatim and in order - the coverage line first,
//! because a thin file must read as a thin sample and never as a quiet evening.

use hypodj_client::mpd::{MpdConn, MpdError};

const USAGE: &str = "\
usage:
  dj heard                 the last session: marks first, unowned only, deduped
  dj heard all             every row, owned included, no cap (redirect it to a file)
  dj heard marks           every mark you pressed, across the retained sessions
  dj heard limit <n>       the last session, capped at n unowned rows

Flag spellings work too (--all, --marks, --limit n). The first line is always the
COVERAGE line: it prints its own inputs, because the recognizer samples a stream on a
clock and a short file is a thin sample, not a quiet evening.";

/// Run the `heard` gesture. `words` are the argv tokens AFTER the leading `heard`.
///
/// Returns `Ok(true)` when the daemon answered. A bad subcommand prints the usage and
/// returns `Ok(false)` (which the caller maps to a non-zero exit) rather than guessing
/// at a view the user did not ask for.
pub fn run(conn: &mut MpdConn, words: &[String]) -> Result<bool, MpdError> {
    let Some(line) = verb_line(words) else {
        eprintln!("{USAGE}");
        return Ok(false);
    };
    match conn.command(&line) {
        Ok(pairs) => {
            let mut printed = 0usize;
            for (k, v) in &pairs {
                if k == "heard" {
                    println!("{v}");
                    printed += 1;
                }
            }
            // The daemon always renders at least a coverage line or a reason, so an
            // empty reply means an OLDER daemon that has no `heard` verb yet. Say that
            // rather than exiting silently on a question the user asked out loud.
            if printed == 0 {
                println!("nothing to read back (is the daemon new enough to keep a heard ledger?)");
            }
            Ok(true)
        }
        Err(MpdError::Ack(msg)) => {
            println!("could not read the ledger: {msg}");
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// Map the argv tail to the ONE daemon verb line, or None when it names no view.
///
/// Both spellings of every view are accepted - the subcommand WORD (which is what the
/// daemon's own grammar uses, and what `:heard all` types in the TUI) and the flag,
/// which is what a hand reaches for at a shell. Anything else is a usage error, never a
/// silent fallback to the default view: `dj heard marsk` must not quietly print
/// something else and let the typo pass as an answer.
///
/// Pure, so the whole argv surface is testable without a socket.
fn verb_line(words: &[String]) -> Option<String> {
    let (first, rest) = match words.split_first() {
        None => return Some("heard".to_string()),
        Some((f, r)) => (f.as_str(), r),
    };
    match first {
        "all" | "--all" | "-a" if rest.is_empty() => Some("heard all".to_string()),
        "marks" | "--marks" | "-m" if rest.is_empty() => Some("heard marks".to_string()),
        "limit" | "--limit" | "-n" if rest.len() == 1 => match rest[0].parse::<usize>() {
            // A zero limit renders a coverage line and no rows, which reads as an empty
            // evening - the one thing this surface must never do. It is a usage error.
            Ok(n) if n > 0 => Some(format!("heard limit {n}")),
            _ => None,
        },
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
    fn bare_heard_is_the_default_view() {
        assert_eq!(line(&[]), Some("heard".to_string()));
    }

    #[test]
    fn both_spellings_of_every_view_reach_the_same_verb() {
        // The daemon's grammar is subcommand WORDS; a shell hand reaches for flags.
        // Both must land on the identical line, so the two surfaces cannot diverge.
        assert_eq!(line(&["all"]), line(&["--all"]));
        assert_eq!(line(&["all"]), Some("heard all".to_string()));
        assert_eq!(line(&["-a"]), Some("heard all".to_string()));
        assert_eq!(line(&["marks"]), line(&["--marks"]));
        assert_eq!(line(&["marks"]), Some("heard marks".to_string()));
        assert_eq!(line(&["-m"]), Some("heard marks".to_string()));
        assert_eq!(line(&["limit", "5"]), line(&["--limit", "5"]));
        assert_eq!(line(&["limit", "5"]), Some("heard limit 5".to_string()));
        assert_eq!(line(&["-n", "40"]), Some("heard limit 40".to_string()));
    }

    #[test]
    fn a_view_that_names_nothing_is_a_usage_error_not_a_silent_default() {
        // The trap this guards: falling back to the default view would let a typo pass
        // as an answer, and the user would read a thin default as "that is all there
        // was" - the exact misreading the coverage line exists to prevent.
        assert_eq!(line(&["marsk"]), None);
        assert_eq!(line(&["everything"]), None);
        assert_eq!(line(&["all", "now"]), None);
        assert_eq!(line(&["marks", "please"]), None);
        assert_eq!(line(&["limit"]), None);
        assert_eq!(line(&["limit", "x"]), None);
        assert_eq!(line(&["limit", "5", "6"]), None);
        // Zero rows would render as an empty evening; it is a usage error.
        assert_eq!(line(&["limit", "0"]), None);
        // A negative is not a usize and must not wrap into a huge cap.
        assert_eq!(line(&["limit", "-3"]), None);
    }
}
