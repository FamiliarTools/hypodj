//! `dj journal` and `dj undo` - the read-back and the retraction for the daemon's
//! undo ring.
//!
//! The ring holds the last 32 `plan::Action` executions, each with the daemon's OWN
//! sentence for what it did (`PlanOutcome::render()`), and every one of them is
//! retractable while nothing else has touched the world. That last clause is why the
//! render marks exactly ONE row: the ring is a stack, so `undo` only ever takes the
//! newest still-undoable entry, and a listing that offered "can undo" on four rows
//! would be advertising three refusals.
//!
//! Like `heard` and `store`, this is intercepted BEFORE `route()`: route is a pure
//! verb-vs-NL split shared with dj-gui and knows no journal verb, so `dj journal`
//! would otherwise be handed to the NL translator, which has no journal action.

use hypodj_client::model::{journal_lines, parse_journal, JournalEntry};
use hypodj_client::mpd::{MpdConn, MpdError};

const USAGE: &str = "\
usage:
  dj journal    what the daemon did, newest first
  dj undo       retract the newest thing that can still be retracted
";

/// Run `dj journal`. `words` are the argv tokens AFTER the leading `journal`.
/// Returns false (exit non-zero) on a usage error.
pub fn run(conn: &mut MpdConn, words: &[String]) -> Result<bool, MpdError> {
    if !words.is_empty() {
        print!("{USAGE}");
        return Ok(false);
    }
    match conn.command("journal") {
        Ok(pairs) => {
            println!("{}", render(&parse_journal(&pairs)));
            Ok(true)
        }
        // An older daemon has no `journal` verb at all; say that rather than
        // printing an empty ring, which reads as "nothing happened".
        Err(MpdError::Ack(_)) => {
            println!("this daemon keeps no journal yet");
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// Run `dj undo`. Every refusal the daemon makes is loud (an ACK), and it is
/// printed verbatim - a human who pressed undo and saw nothing would press again.
pub fn undo(conn: &mut MpdConn) -> Result<bool, MpdError> {
    match conn.command("undo") {
        Ok(pairs) => {
            let did = pairs
                .iter()
                .find(|(k, _)| k == "jdid")
                .map(|(_, v)| v.as_str())
                .unwrap_or("done");
            println!("undone: {did}");
            Ok(true)
        }
        Err(MpdError::Ack(msg)) => {
            println!("nothing undone: {msg}");
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// Render the ring. The row layout and the one-mark rule live in `hypodj-client`
/// so the `dj-gui` overlay cannot drift from this listing; only the header and the
/// key name belong to the CLI.
pub fn render(entries: &[JournalEntry]) -> String {
    if entries.is_empty() {
        return "nothing to retract - the journal records plan actions, and none have run"
            .to_string();
    }
    let mut out = vec!["what the daemon did, newest first".to_string()];
    out.extend(journal_lines(entries, "  <- dj undo").into_iter().map(|l| format!("  {l}")));
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u64, age: u64, origin: &str, did: &str, undoable: bool) -> JournalEntry {
        JournalEntry {
            id,
            age_s: age,
            origin: origin.to_string(),
            act: "clear".to_string(),
            did: did.to_string(),
            undoable,
        }
    }

    #[test]
    fn an_empty_ring_says_what_it_records_rather_than_printing_nothing() {
        let out = render(&[]);
        assert!(out.contains("nothing to retract"), "{out}");
        assert!(out.contains("plan actions"), "and what a journal entry even is: {out}");
    }

    #[test]
    fn exactly_one_row_is_marked_retractable_because_undo_is_a_stack() {
        let rows = [
            entry(12, 4, "mcp:sess-3f2a19bc", "cleared 7", true),
            entry(11, 130, "mpd", "added 3", true),
            entry(10, 400, "sleep", "faded out", false),
        ];
        let out = render(&rows);
        assert_eq!(out.matches("<- dj undo").count(), 1, "{out}");
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[1].contains("<- dj undo"), "the NEWEST undoable one: {out}");
        assert!(!lines[2].contains("<- dj undo"), "not the one under it: {out}");
    }

    #[test]
    fn a_ring_whose_top_has_gone_stale_marks_the_newest_that_has_not() {
        let rows = [
            entry(12, 4, "mcp:sess-1", "cleared 7", false),
            entry(11, 130, "mpd", "added 3", true),
        ];
        let out = render(&rows);
        let lines: Vec<&str> = out.lines().collect();
        assert!(!lines[1].contains("<- dj undo"));
        assert!(lines[2].contains("<- dj undo"), "{out}");
        // And a ring where NOTHING is undoable offers nothing.
        let rows = [entry(12, 4, "mcp:sess-1", "cleared 7", false)];
        assert!(!render(&rows).contains("<- dj undo"));
    }

    #[test]
    fn the_daemons_own_sentence_and_the_origin_are_shown_verbatim() {
        let out = render(&[entry(12, 4, "mcp:sess-3f2a19bc", "removed The Man Machine", true)]);
        assert!(out.contains("removed The Man Machine"), "{out}");
        assert!(out.contains("agent sess-3f2a19bc"), "{out}");
        // A non-agent origin is NOT relabelled - `sleep` is not "you".
        let out = render(&[entry(9, 4, "sleep", "faded out", false)]);
        assert!(out.contains("sleep"), "{out}");
        assert!(!out.contains("agent"), "{out}");
        // A missing origin says so instead of looking like a human's action.
        assert!(render(&[entry(9, 4, "", "stopped", false)]).contains("(no origin)"));
    }

    #[test]
    fn the_age_reads_human() {
        let out = render(&[entry(1, 4, "mpd", "added 1", false), entry(2, 130, "mpd", "added 1", false)]);
        assert!(out.contains("4s"), "{out}");
        assert!(out.contains("2m"), "{out}");
    }
}
