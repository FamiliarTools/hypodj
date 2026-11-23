//! Build a [`hypodj_style::Spoken`] from a daemon reply.
//!
//! The type lives in hypodj-style. This module only decides which state a
//! reply is, so the CLI and the TUI print the same line for the same pairs.
//!
//! The state comes from what the reply says happened. A `result` means it
//! ran. A `plan_id` with no `result` means a plan exists and has not run.
//! A bare OK takes its past tense from the action kind the client sent, never
//! from words found in a step.

use hypodj_style::Spoken;
pub use hypodj_style::State;

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
    /// `undo` returned a `jdid`. The words are that id.
    Undo,
}

impl Act {
    /// Past tense for that kind. Fixed sentences, one per kind.
    pub fn past(self) -> &'static str {
        match self {
            Act::Clear => "Cleared the queue.",
            Act::SleepCancel => "Cancelled sleep.",
            Act::WinddownCancel => "Cancelled the wind-down.",
            Act::WakeCancel => "Cancelled the wake.",
            // The sentence is the daemon's `jdid`, built in `noted`.
            Act::Undo => "undone",
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
        "undo" => Some(Act::Undo),
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

/// One reply. A confirm is several parts (one per plan); everything else is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub parts: Vec<Spoken>,
}

impl Reply {
    pub fn one(spoken: Spoken) -> Self {
        Self {
            parts: vec![spoken],
        }
    }

    /// The status line: every part, joined the way master printed a confirm.
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .map(Spoken::text)
            .collect::<Vec<_>>()
            .join("; ")
    }
}

impl From<Spoken> for Reply {
    fn from(spoken: Spoken) -> Self {
        Self::one(spoken)
    }
}

/// An `nl confirm` reply: one utterance, one part per plan, in wire order.
///
/// A `result` that follows a `plan_id` replaces that plan's armed line. The
/// words are the daemon's. A failure or a no-match is refused. A `plan_id`
/// with no following `result` is armed: it is waiting, it has not run.
pub fn from_confirm(pairs: &[(String, String)]) -> Reply {
    let mut parts = Vec::new();
    for (k, v) in pairs {
        match k.as_str() {
            "plan_id" => parts.push(Spoken::new(State::Armed, vec![nl::armed_line(v)])),
            "result" => {
                let spoken = Spoken::new(state_of_result(v), vec![v.clone()]);
                match parts.last_mut() {
                    Some(last) => *last = spoken,
                    None => parts.push(spoken),
                }
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        parts.push(from_reply(pairs, None));
    }
    Reply { parts }
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
        let state = if results
            .iter()
            .all(|line| state_of_result(line) == State::Refused)
        {
            State::Refused
        } else {
            State::Live
        };
        return Spoken::new(state, results);
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
    if let Some(spoken) = noted(pairs, act) {
        return spoken;
    }
    match act {
        Some(act) => Spoken::new(State::Live, vec![act.past().to_string()]),
        // A bare OK still happened. One short line, not silence and not a guess
        // borrowed from the step text.
        None => Spoken::new(State::Live, vec!["OK.".to_string()]),
    }
}

/// A result that did not happen. The sentences are the daemon's constants.
fn state_of_result(line: &str) -> State {
    if line.starts_with(hypodj_dsl::RESULT_ADDED_NONE)
        || line == hypodj_dsl::RESULT_NO_JUMP
        || line.starts_with(hypodj_dsl::RESULT_FAILED)
    {
        State::Refused
    } else {
        State::Live
    }
}

/// Sleep, wake, a read-side answer, or an undo.
///
/// A timer that is counting down is armed: it is set to fire and can be
/// disarmed. Nothing pending is a note, not ember.
fn noted(pairs: &[(String, String)], act: Option<Act>) -> Option<Spoken> {
    if let Some(spoken) = winddown_from_pairs(pairs) {
        return Some(spoken);
    }
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
        return Some(Spoken::new(
            State::Note,
            vec!["no sleep timer armed".into()],
        ));
    }
    if pairs.iter().any(|(k, v)| k == "wake" && v == "none") {
        return Some(Spoken::new(State::Note, vec!["no wake alarm armed".into()]));
    }
    if let Some((_, answer)) = pairs.iter().find(|(k, _)| k == "nl_answer") {
        return Some(Spoken::new(State::Live, vec![answer.clone()]));
    }
    if act == Some(Act::Undo) {
        if let Some((_, did)) = pairs.iter().find(|(k, _)| k == "jdid") {
            return Some(Spoken::new(State::Live, vec![format!("undone: {did}")]));
        }
    }
    None
}

/// The sentence both surfaces speak for a successful command, when it has one.
/// `None` for a verb whose answer is the queue itself (`play`, `next`).
pub fn command_spoken(line: &str, pairs: &[(String, String)]) -> Option<Spoken> {
    if let Some((_, sentence)) = pairs.iter().find(|(k, _)| k == "mark_result") {
        return Some(live_text(sentence.clone()));
    }
    let act = act_from_command(line);
    if noted(pairs, act).is_some() {
        return Some(from_reply(pairs, act));
    }
    mode_sentence(line).map(|sentence| Spoken::new(State::Live, vec![sentence]))
}

/// `random 1` / `repeat 0` after the daemon has already said OK.
fn mode_sentence(line: &str) -> Option<String> {
    let mut words = line.split_whitespace();
    let verb = words.next()?;
    let bit = words.next()?;
    if words.next().is_some() {
        return None;
    }
    if !matches!(verb, "random" | "repeat" | "single" | "consume") {
        return None;
    }
    let on = match bit {
        "1" => "on",
        "0" => "off",
        _ => return None,
    };
    Some(format!("{verb} {on}"))
}

fn winddown_from_pairs(pairs: &[(String, String)]) -> Option<Spoken> {
    let phase = pairs.iter().find(|(k, _)| k == hypodj_dsl::WINDDOWN_KEY).map(|(_, v)| v.as_str());
    let when = pairs
        .iter()
        .find(|(k, _)| k == hypodj_dsl::WINDDOWN_WHEN_KEY)
        .map(|(_, v)| v.as_str());
    let remaining = pairs
        .iter()
        .find(|(k, _)| k == "X-hypodj-winddown-remaining")
        .and_then(|(_, v)| v.parse().ok());
    winddown_spoken(phase, when, remaining)
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

/// Sleep, a future wind-down, and wake are armed: set to fire later.
/// A wind-down that has already started is live, in the present tense.
/// Empty when nothing is set and nothing is winding down.
pub fn armed_hold(a: &ArmedFeatures) -> Vec<Spoken> {
    let mut out = Vec::new();
    let mut later = Vec::new();
    if let Some(s) = a.sleep_remaining {
        later.push(format!("sleep in {}", fmt_remaining(s)));
    }
    if let Some(s) = a.wake_remaining {
        later.push(format!("wake in {}", fmt_remaining(s)));
    }
    if let Some(spoken) = winddown_spoken(
        a.winddown_phase.as_deref(),
        a.winddown_when.as_deref(),
        a.winddown_remaining,
    ) {
        if spoken.state == State::Armed {
            later.extend(spoken.lines);
        } else {
            out.push(spoken);
        }
    }
    if !later.is_empty() {
        out.push(Spoken::new(State::Armed, vec![later.join(" \u{00b7} ")]));
    }
    out
}

/// Map the daemon's explicit wind-down pair. No inference from a missing remaining.
pub fn winddown_spoken(
    phase: Option<&str>,
    when: Option<&str>,
    remaining: Option<u64>,
) -> Option<Spoken> {
    match phase {
        Some(p) if p == hypodj_dsl::WINDDOWN_RUNNING => {
            Some(Spoken::new(State::Live, vec!["winding down".into()]))
        }
        Some(p) if p == hypodj_dsl::WINDDOWN_ARMED => {
            let line = match when {
                Some(w) if w == hypodj_dsl::WINDDOWN_WHEN_ALBUM => {
                    "wind down at album end".to_string()
                }
                Some(w) if w == hypodj_dsl::WINDDOWN_WHEN_TRACK => {
                    "wind down after this track".to_string()
                }
                Some(w) if w == hypodj_dsl::WINDDOWN_WHEN_DEADLINE => match remaining {
                    Some(s) => format!("wind down in {}", fmt_remaining(s)),
                    None => "wind down at the deadline".to_string(),
                },
                _ => return None,
            };
            Some(Spoken::new(State::Armed, vec![line]))
        }
        _ => None,
    }
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
    /// When set, [`command_spoken`] is what the CLI and the TUI call.
    pub command: Option<&'static str>,
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
        command: None,
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
        out.push(shape(
            name,
            pairs(keys),
            None,
            None,
            state_of_result(line),
            &[line],
        ));
    }
    // Playback modes. The handler writes `effect_mode(word, on)`.
    for mode in ["random", "repeat", "single", "consume"] {
        for bit in ["on", "off"] {
            let line = format!("{mode} {bit}");
            out.push(ReplyShape {
                name: match (mode, bit) {
                    ("random", "on") => "mode random on",
                    ("random", "off") => "mode random off",
                    ("repeat", "on") => "mode repeat on",
                    ("repeat", "off") => "mode repeat off",
                    ("single", "on") => "mode single on",
                    ("single", "off") => "mode single off",
                    ("consume", "on") => "mode consume on",
                    _ => "mode consume off",
                },
                pairs: pairs(&[("plan_id", "4"), ("result", line.as_str())]),
                ack: None,
                act: None,
                state: State::Live,
                lines: vec![line],
                command: None,
            });
        }
    }
    out.push(shape(
        "nl answer",
        pairs(&[("nl_answer", "yes, \"Blue\" is a favorite")]),
        None,
        None,
        State::Live,
        &["yes, \"Blue\" is a favorite"],
    ));
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
        State::Note,
        &["no sleep timer armed"],
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
        State::Note,
        &["no wake alarm armed"],
    ));
    out.push(ReplyShape {
        name: "undo",
        pairs: pairs(&[("jdid", "cleared 7")]),
        ack: None,
        act: Some(Act::Undo),
        state: State::Live,
        lines: vec!["undone: cleared 7".into()],
        command: Some("undo"),
    });
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
    // A bare OK with no kind is still live. The line is minimal, and it must
    // not become "armed" or borrow a sentence from a step.
    out.push(shape(
        "bare ok",
        Vec::new(),
        None,
        None,
        State::Live,
        &["OK."],
    ));
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
            command: None,
        });
    }
    out.push(shape(
        "wind down at album end",
        pairs(&[
            (hypodj_dsl::WINDDOWN_KEY, hypodj_dsl::WINDDOWN_ARMED),
            (hypodj_dsl::WINDDOWN_WHEN_KEY, hypodj_dsl::WINDDOWN_WHEN_ALBUM),
        ]),
        None,
        None,
        State::Armed,
        &["wind down at album end"],
    ));
    out.push(shape(
        "wind down after this track",
        pairs(&[
            (hypodj_dsl::WINDDOWN_KEY, hypodj_dsl::WINDDOWN_ARMED),
            (hypodj_dsl::WINDDOWN_WHEN_KEY, hypodj_dsl::WINDDOWN_WHEN_TRACK),
        ]),
        None,
        None,
        State::Armed,
        &["wind down after this track"],
    ));
    out.push(shape(
        "wind down in 5m",
        pairs(&[
            (hypodj_dsl::WINDDOWN_KEY, hypodj_dsl::WINDDOWN_ARMED),
            (hypodj_dsl::WINDDOWN_WHEN_KEY, hypodj_dsl::WINDDOWN_WHEN_DEADLINE),
            ("X-hypodj-winddown-remaining", "300"),
        ]),
        None,
        None,
        State::Armed,
        &["wind down in 5m"],
    ));
    out.push(shape(
        "winding down",
        pairs(&[(hypodj_dsl::WINDDOWN_KEY, hypodj_dsl::WINDDOWN_RUNNING)]),
        None,
        None,
        State::Live,
        &["winding down"],
    ));
    for shape in &mut out {
        shape.command = match shape.name {
            "sleep remaining" | "sleep none" => Some("sleep"),
            "wake remaining" | "wake none" => Some("wake"),
            "nl answer" => Some("nl"),
            "undo" => Some("undo"),
            "wind down at album end"
            | "wind down after this track"
            | "wind down in 5m"
            | "winding down" => Some("winddown"),
            _ => shape.command,
        };
    }
    out
}

/// Classify one catalog row the way the binaries do.
///
/// Rows with a `command` go through [`command_spoken`], the function the CLI
/// and the TUI call for sleep, wake, an answer, and undo.
pub fn speak_shape(shape: &ReplyShape) -> Spoken {
    if let Some(msg) = &shape.ack {
        return refusal_ack(msg);
    }
    if let Some(cmd) = shape.command {
        return command_spoken(cmd, &shape.pairs)
            .unwrap_or_else(|| from_reply(&shape.pairs, shape.act));
    }
    from_reply(&shape.pairs, shape.act)
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
        assert_eq!(spoken.state, State::Live);
        assert_eq!(spoken.lines, vec!["OK.".to_string()]);
        assert!(!spoken.text().contains("clear"));
        assert!(!spoken.text().contains("armed"));
        let hint = reply_shapes()
            .into_iter()
            .find(|s| s.name == "not understood")
            .unwrap();
        assert!(hint.lines[0].contains("I did not understand"));
        assert!(!hint.lines[0].contains('!'));
    }

    #[test]
    fn a_mixed_confirm_keeps_each_plan() {
        let reply = from_confirm(&[
            ("plan_id".into(), "1".into()),
            ("result".into(), "added 5 tracks".into()),
            ("plan_id".into(), "2".into()),
        ]);
        assert_eq!(reply.parts.len(), 2);
        assert_eq!(reply.parts[0].state, State::Live);
        assert_eq!(reply.parts[0].lines, vec!["added 5 tracks".to_string()]);
        assert_eq!(reply.parts[1].state, State::Armed);
        assert_eq!(reply.parts[1].lines, vec!["armed (plan 2)".to_string()]);
        assert_eq!(reply.text(), "added 5 tracks; armed (plan 2)");
        let words: Vec<String> = reply.parts.iter().map(Spoken::text).collect();
        assert_eq!(
            words,
            nl::confirm_lines(&[
                ("plan_id".into(), "1".into()),
                ("result".into(), "added 5 tracks".into()),
                ("plan_id".into(), "2".into()),
            ])
        );
    }
}
