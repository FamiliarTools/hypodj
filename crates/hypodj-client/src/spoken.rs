//! Build a [`hypodj_style::Spoken`] from a daemon reply.
//!
//! The type lives in hypodj-style. This module only decides which state a
//! reply is, so the CLI and the TUI print the same line for the same pairs.

use hypodj_style::{Spoken, State};

use crate::model::{fmt_remaining, ArmedFeatures};
use crate::nl::{self, EchoParts};

/// The y/N line both surfaces show under an unconfirmed plan.
pub const CONFIRM_LINE: &str = "confirm? [y/N]";

/// An unconfirmed plan. Steps are echo. A wake caveat is a note, and it comes
/// before the confirm question. The question is always the last line.
pub fn plan_echo(parts: &EchoParts) -> Vec<Spoken> {
    let mut lines = Vec::new();
    if let Some(trust) = &parts.trust {
        lines.push(format!("({trust})"));
    }
    lines.extend(parts.steps.iter().cloned());
    let mut out = Vec::new();
    if !lines.is_empty() {
        out.push(Spoken::new(State::Echo, lines));
    }
    if let Some(note) = &parts.note {
        out.push(Spoken::new(State::Note, vec![note.clone()]));
    }
    out.push(Spoken::new(State::Echo, vec![CONFIRM_LINE.to_string()]));
    out
}

/// What a confirm just did.
///
/// A `plan_id` is armed. A `result` already happened, so it is live in the
/// daemon's own words. Anything else already happened too: live, in the past
/// tense, never the word "armed".
pub fn from_arm_pairs(
    pairs: &[(String, String)],
    command: Option<&str>,
    steps: &[String],
) -> Spoken {
    if let Some((_, id)) = pairs.iter().find(|(k, _)| k == "plan_id") {
        return Spoken::new(State::Armed, vec![nl::armed_line(id)]);
    }
    if let Some(line) = nl::result_line_from_pairs(pairs) {
        return Spoken::new(State::Live, vec![line]);
    }
    Spoken::new(State::Live, vec![past_tense(command, steps)])
}

/// Past tense for a confirmed act that returned neither a plan id nor a result.
fn past_tense(command: Option<&str>, steps: &[String]) -> String {
    let command = command.unwrap_or("").trim();
    let verb = command.split_whitespace().next().unwrap_or("");
    let mentions_clear = verb == "clear"
        || steps.iter().any(|s| {
            let t = s.trim();
            t == "clear" || t.contains("clear the queue") || t.contains("clear the whole")
        });
    if mentions_clear {
        return "Cleared the queue.".to_string();
    }
    if let Some(step) = steps.iter().map(|s| s.trim()).find(|s| !s.is_empty()) {
        return format!("Done: {step}.");
    }
    if !command.is_empty() {
        return format!("Done: {command}.");
    }
    "Done.".to_string()
}

/// A daemon ACK, mapped to one calm sentence.
pub fn refusal_ack(msg: &str) -> Spoken {
    Spoken::new(State::Refused, vec![nl::map_ack_reason(msg)])
}

/// A refusal whose words are already the sentence a person should read.
pub fn refused_text(line: impl Into<String>) -> Spoken {
    Spoken::new(State::Refused, vec![line.into()])
}

/// Something that already happened: a result line, a mark, a heard pin.
pub fn live_text(line: impl Into<String>) -> Spoken {
    Spoken::new(State::Live, vec![line.into()])
}

/// The y/N question other confirms ask ("clear the whole queue?").
pub fn confirm_question(question: &str) -> Spoken {
    Spoken::new(State::Echo, vec![format!("{question} [y/N]")])
}

/// Sleep, wind-down, and wake, as one armed line. None when nothing is armed.
pub fn armed_hold(a: &ArmedFeatures) -> Option<Spoken> {
    if !a.any() {
        return None;
    }
    let mut bits = Vec::new();
    if let Some(s) = a.sleep_remaining {
        bits.push(format!("sleep in {}", fmt_remaining(s)));
    }
    if a.winddown_active {
        match a.winddown_remaining {
            Some(s) => bits.push(format!("wind-down in {}", fmt_remaining(s))),
            None => bits.push("winding down".to_string()),
        }
    }
    if let Some(s) = a.wake_remaining {
        bits.push(format!("wake in {}", fmt_remaining(s)));
    }
    Some(Spoken::new(State::Armed, vec![bits.join(" \u{00b7} ")]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_note_comes_before_the_question_and_the_question_is_last() {
        let parts = nl::split_echo("via rules | [1] fade out | NOTE: a wake also needs a play");
        let spoken = plan_echo(&parts);
        assert_eq!(spoken.len(), 3);
        assert_eq!(spoken[0].state, State::Echo);
        assert_eq!(
            spoken[0].lines,
            vec!["(via rules)".to_string(), "[1] fade out".to_string()]
        );
        assert_eq!(spoken[1].state, State::Note);
        assert_eq!(spoken[2].state, State::Echo);
        assert_eq!(spoken[2].lines, vec![CONFIRM_LINE.to_string()]);
        assert_eq!(spoken.last().unwrap().lines.last().unwrap(), CONFIRM_LINE);
    }

    #[test]
    fn only_a_plan_id_is_armed_and_clear_is_live() {
        let played = from_arm_pairs(&[("result".into(), "played Blue".into())], None, &[]);
        assert_eq!(played.state, State::Live);
        assert_eq!(played.lines, vec!["played Blue".to_string()]);

        let armed = from_arm_pairs(&[("plan_id".into(), "4".into())], Some("clear"), &[]);
        assert_eq!(armed.state, State::Armed);
        assert_eq!(armed.lines, vec!["armed (plan 4)".to_string()]);

        let cleared = from_arm_pairs(&[], Some("clear"), &[]);
        assert_eq!(cleared.state, State::Live);
        assert_eq!(cleared.lines, vec!["Cleared the queue.".to_string()]);
        assert!(!cleared.text().contains("armed"));
    }

    #[test]
    fn a_refusal_is_the_mapped_sentence() {
        let s = refusal_ack("not understood");
        assert_eq!(s.state, State::Refused);
        assert!(!s.text().is_empty());
        assert!(!s.text().contains('!'));
    }
}
