//! Build a [`hypodj_style::Spoken`] from a daemon reply.
//!
//! The type lives in hypodj-style. This module only decides which state a
//! reply is, so the CLI and the TUI print the same line for the same pairs.
//!
//! The state comes from what the reply says happened. A `result` means it
//! ran. A `plan_id` with no `result` means a plan exists and has not run.
//! A bare OK takes its past tense from the action kind the client sent, never
//! from words found in a step.

use hypodj_style::{Spoken, State};

use crate::model::{fmt_remaining, ArmedFeatures};
use crate::nl::{self, EchoParts};

/// The y/N line both surfaces show under an unconfirmed plan.
pub const CONFIRM_LINE: &str = "confirm? [y/N]";

/// A command whose success is a bare OK, so the reply itself has no sentence.
/// The kind is the verb the client sent (`clear`, `sleep off`), not a scan of
/// the plan's step text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Clear,
    SleepCancel,
    WinddownCancel,
    WakeCancel,
}

impl Act {
    /// Past tense for that kind. Fixed sentences, one per kind.
    pub fn past(self) -> &'static str {
        match self {
            Act::Clear => "Cleared the queue.",
            Act::SleepCancel => "Cancelled sleep.",
            Act::WinddownCancel => "Cancelled the wind-down.",
            Act::WakeCancel => "Cancelled the wake.",
        }
    }
}

/// The action kind of a command line, when that command's success is a bare OK.
pub fn act_from_command(command: &str) -> Option<Act> {
    let mut words = command.split_whitespace();
    let verb = words.next()?;
    let next = words.next();
    match verb {
        "clear" => Some(Act::Clear),
        "sleep" if matches!(next, Some("off" | "cancel")) => Some(Act::SleepCancel),
        "winddown" if matches!(next, Some("off" | "cancel")) => Some(Act::WinddownCancel),
        "wake" if matches!(next, Some("off" | "cancel")) => Some(Act::WakeCancel),
        _ => None,
    }
}

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

/// What a confirm just did, from the pairs the daemon returned.
///
/// `result` wins, even when a `plan_id` is also present: the action ran, and
/// the words are the daemon's. `plan_id` with no `result` is armed. Anything
/// else is a status pair or, for a bare OK, the past tense of `act`.
pub fn from_reply(pairs: &[(String, String)], act: Option<Act>) -> Spoken {
    let results: Vec<String> = pairs
        .iter()
        .filter(|(k, _)| k == "result")
        .map(|(_, v)| v.clone())
        .collect();
    if !results.is_empty() {
        return Spoken::new(State::Live, results);
    }
    let ids: Vec<String> = pairs
        .iter()
        .filter(|(k, _)| k == "plan_id")
        .map(|(_, v)| v.clone())
        .collect();
    if !ids.is_empty() {
        let lines = ids.iter().map(|id| nl::armed_line(id)).collect();
        return Spoken::new(State::Armed, lines);
    }
    if let Some(spoken) = status_pair(pairs) {
        return spoken;
    }
    match act {
        Some(act) => Spoken::new(State::Live, vec![act.past().to_string()]),
        None => Spoken::new(State::Live, Vec::new()),
    }
}

/// Sleep and wake status, from the keys the handler actually sends.
fn status_pair(pairs: &[(String, String)]) -> Option<Spoken> {
    if let Some((_, secs)) = pairs.iter().find(|(k, _)| k == "sleep_remaining") {
        return Some(Spoken::new(
            State::Armed,
            vec![format!("sleep in {}", span(secs))],
        ));
    }
    if let Some((_, secs)) = pairs.iter().find(|(k, _)| k == "wake_remaining") {
        return Some(Spoken::new(
            State::Armed,
            vec![format!("wake in {}", span(secs))],
        ));
    }
    if pairs.iter().any(|(k, v)| k == "sleep" && v == "none") {
        return Some(Spoken::new(State::Live, vec!["sleep: none".into()]));
    }
    if pairs.iter().any(|(k, v)| k == "wake" && v == "none") {
        return Some(Spoken::new(State::Live, vec!["wake: none".into()]));
    }
    None
}

fn span(secs: &str) -> String {
    match secs.parse::<u64>() {
        Ok(n) => fmt_remaining(n),
        Err(_) => secs.to_string(),
    }
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

/// One reply shape the handler can emit, and the state and words it must
/// produce. The strings are the handler's: `PlanOutcome::render`, the effect
/// verbs, and the ACK bodies. A core test locks this table to those sources.
pub struct ReplyShape {
    pub name: &'static str,
    pub pairs: Vec<(String, String)>,
    pub ack: Option<String>,
    pub act: Option<Act>,
    pub state: State,
    pub lines: Vec<String>,
}

fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn shape(
    name: &'static str,
    pairs: Vec<(String, String)>,
    ack: Option<&str>,
    act: Option<Act>,
    state: State,
    lines: &[&str],
) -> ReplyShape {
    ReplyShape {
        name,
        pairs,
        ack: ack.map(str::to_string),
        act,
        state,
        lines: lines.iter().map(|s| (*s).to_string()).collect(),
    }
}

/// Every reply shape `handle_plan`, `nl confirm`, `clear`, `sleep`,
/// `winddown`, and `wake` can emit, plus the refusals those paths ACK.
pub fn reply_shapes() -> Vec<ReplyShape> {
    let mut out = Vec::new();
    // Deferred plan add, a single confirm, and sleep / wind-down / wake set:
    // a plan id, and no result, because nothing has run.
    out.push(shape(
        "plan id only",
        pairs(&[("plan_id", "4")]),
        None,
        None,
        State::Armed,
        &["armed (plan 4)"],
    ));
    // A wake batch confirm emits one plan_id per plan.
    out.push(shape(
        "confirm batch",
        pairs(&[("plan_id", "1"), ("plan_id", "2")]),
        None,
        None,
        State::Armed,
        &["armed (plan 1)", "armed (plan 2)"],
    ));
    // Immediate plan add: plan_id AND result. The result is what ran.
    // Key order must not matter, so one row leads with result.
    let results: &[(&str, &str)] = &[
        ("added some", "added 5 tracks"),
        ("added one", "added 1 track"),
        ("added none", "added 0 - no matches for \"jazz\""),
        ("played title", "played \"Blue\""),
        ("played count", "played 2 tracks"),
        ("removed", "removed 2 tracks"),
        ("moved", "moved 2 tracks"),
        ("cleared count", "cleared 3 tracks"),
        ("jumped", "jumped to the matching track"),
        ("no jump", "no matching track to play"),
        ("fading", "fading"),
        ("stopped", "stopped"),
        ("paused", "paused"),
        ("volume", "set volume to 30"),
        ("waking", "waking"),
        ("nothing", "nothing to do"),
        ("failed", "could not do that: gone"),
    ];
    for (i, (name, line)) in results.iter().enumerate() {
        let keys: &[(&str, &str)] = if i == 0 {
            &[("result", line), ("plan_id", "4")]
        } else {
            &[("plan_id", "4"), ("result", line)]
        };
        out.push(shape(name, pairs(keys), None, None, State::Live, &[line]));
    }
    // Status. The handler sends the seconds, or the word none.
    out.push(shape(
        "sleep remaining",
        pairs(&[("sleep_remaining", "720")]),
        None,
        None,
        State::Armed,
        &["sleep in 12m"],
    ));
    out.push(shape(
        "sleep none",
        pairs(&[("sleep", "none")]),
        None,
        None,
        State::Live,
        &["sleep: none"],
    ));
    out.push(shape(
        "wake remaining",
        pairs(&[("wake_remaining", "45")]),
        None,
        None,
        State::Armed,
        &["wake in 45s"],
    ));
    out.push(shape(
        "wake none",
        pairs(&[("wake", "none")]),
        None,
        None,
        State::Live,
        &["wake: none"],
    ));
    // Bare OK. The sentence is the action kind, not the step text.
    out.push(shape(
        "clear",
        Vec::new(),
        None,
        Some(Act::Clear),
        State::Live,
        &["Cleared the queue."],
    ));
    out.push(shape(
        "sleep cancel",
        Vec::new(),
        None,
        Some(Act::SleepCancel),
        State::Live,
        &["Cancelled sleep."],
    ));
    out.push(shape(
        "wind-down cancel",
        Vec::new(),
        None,
        Some(Act::WinddownCancel),
        State::Live,
        &["Cancelled the wind-down."],
    ));
    out.push(shape(
        "wake cancel",
        Vec::new(),
        None,
        Some(Act::WakeCancel),
        State::Live,
        &["Cancelled the wake."],
    ));
    // A bare OK with no kind says nothing. It must not become "armed" or
    // borrow a sentence from a step that happens to contain "clear".
    out.push(shape("bare ok", Vec::new(), None, None, State::Live, &[]));
    // Refusals. The ack body is what `ack_message` returns.
    let acks: &[(&str, &str, &str)] = &[
        (
            "no token",
            "no such nl token",
            "that plan expired, run the phrase again",
        ),
        (
            "stale plan",
            "plan no longer valid: deadline already in the past",
            "that plan is no longer valid: deadline already in the past - run the phrase again",
        ),
        (
            "plan invalid",
            "plan invalid: target queue slot is gone",
            "plan invalid: target queue slot is gone",
        ),
        (
            "translator down",
            "translator task failed",
            "translator task failed",
        ),
        ("not understood", "not understood", ""),
        (
            "wake duration",
            "duration out of range",
            "duration out of range",
        ),
        ("wake time", "bad time", "bad time"),
        (
            "plan error",
            "deadline already in the past",
            "deadline already in the past",
        ),
        ("no such plan", "no such plan", "no such plan"),
    ];
    for (name, ack, line) in acks {
        let lines: Vec<String> = if line.is_empty() {
            vec![nl::map_ack_reason(ack)]
        } else {
            vec![(*line).to_string()]
        };
        out.push(ReplyShape {
            name,
            pairs: Vec::new(),
            ack: Some((*ack).to_string()),
            act: None,
            state: State::Refused,
            lines,
        });
    }
    out
}

/// Classify one catalog row the way the binaries do.
pub fn speak_shape(shape: &ReplyShape) -> Spoken {
    match &shape.ack {
        Some(msg) => refusal_ack(msg),
        None => from_reply(&shape.pairs, shape.act),
    }
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
        assert_eq!(spoken[1].state, State::Note);
        assert_eq!(spoken[2].lines, vec![CONFIRM_LINE.to_string()]);
        assert_eq!(spoken.last().unwrap().lines.last().unwrap(), CONFIRM_LINE);
    }

    #[test]
    fn every_handler_shape_has_one_state_and_its_words() {
        for shape in reply_shapes() {
            let spoken = speak_shape(&shape);
            assert_eq!(spoken.state, shape.state, "{}", shape.name);
            assert_eq!(spoken.lines, shape.lines, "{}", shape.name);
        }
    }

    #[test]
    fn a_result_is_live_even_when_a_plan_id_comes_first_and_the_act_is_clear() {
        let spoken = from_reply(
            &[
                ("plan_id".into(), "4".into()),
                ("result".into(), "added 5 tracks".into()),
            ],
            Some(Act::Clear),
        );
        assert_eq!(spoken.state, State::Live);
        assert_eq!(spoken.lines, vec!["added 5 tracks".to_string()]);
    }

    #[test]
    fn clear_comes_from_the_action_kind_not_from_step_text() {
        assert_eq!(act_from_command("clear"), Some(Act::Clear));
        assert_eq!(act_from_command("sleep off"), Some(Act::SleepCancel));
        assert_eq!(
            act_from_command("winddown cancel"),
            Some(Act::WinddownCancel)
        );
        assert_eq!(act_from_command("wake off"), Some(Act::WakeCancel));
        assert_eq!(act_from_command("plan add fade"), None);
        let spoken = from_reply(&[], None);
        assert!(
            spoken.lines.is_empty(),
            "a bare OK with no kind invents nothing"
        );
        assert!(!spoken.text().contains("clear"));
        assert!(!spoken.text().contains("armed"));
        let hint = reply_shapes()
            .into_iter()
            .find(|s| s.name == "not understood")
            .unwrap();
        assert!(hint.lines[0].contains("I did not understand"));
        assert!(!hint.lines[0].contains('!'));
    }
}
