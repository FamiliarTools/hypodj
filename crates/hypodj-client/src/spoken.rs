//! A plan or a daemon reply, as words plus the state those words are in.
//!
//! The CLI, the TUI, and later the GUI all produce this and hand it to a
//! surface adapter. A surface does not decide the state itself.

use crate::model::{fmt_remaining, ArmedFeatures};
use crate::nl::{self, EchoParts};

/// Echo, armed, live, refused. A wake caveat is a note: it is not a refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Echo,
    Armed,
    Live,
    Refused,
    Note,
}

/// One state and the lines that carry it. Every line shares the state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spoken {
    pub state: State,
    pub lines: Vec<String>,
}

impl Spoken {
    pub fn new(state: State, lines: Vec<String>) -> Self {
        Self { state, lines }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }
}

/// The y/N line both surfaces show under an unconfirmed plan.
pub const CONFIRM_LINE: &str = "confirm? [y/N]";

/// An unconfirmed plan: the trust footnote, the steps, and the confirm line
/// are one echo. A wake caveat, when the echo has one, is a separate note.
pub fn plan_echo(parts: &EchoParts) -> Vec<Spoken> {
    let mut lines = Vec::new();
    if let Some(trust) = &parts.trust {
        lines.push(format!("({trust})"));
    }
    lines.extend(parts.steps.iter().cloned());
    lines.push(CONFIRM_LINE.to_string());
    let mut out = vec![Spoken::new(State::Echo, lines)];
    if let Some(note) = &parts.note {
        out.push(Spoken::new(State::Note, vec![note.clone()]));
    }
    out
}

/// What a confirm just did. A `result` pair already happened (live). A
/// `plan_id` is armed. With neither, the steps are the armed banner, or the
/// bare word "armed" when there is nothing to name.
pub fn from_arm_pairs(pairs: &[(String, String)], steps: &[String]) -> Spoken {
    if let Some(line) = nl::result_line_from_pairs(pairs) {
        return Spoken::new(State::Live, vec![line]);
    }
    if let Some((_, id)) = pairs.iter().find(|(k, _)| k == "plan_id") {
        return Spoken::new(State::Armed, vec![nl::armed_line(id)]);
    }
    armed_fallback(steps)
}

/// The armed banner built from the plan summary when the daemon sent no id.
pub fn armed_fallback(steps: &[String]) -> Spoken {
    match steps.iter().map(|s| s.trim()).find(|s| !s.is_empty()) {
        Some(s) => Spoken::new(State::Armed, vec![format!("armed: {s}")]),
        None => Spoken::new(State::Armed, vec!["armed".into()]),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unconfirmed_plan_is_an_echo_plus_a_note() {
        let parts = nl::split_echo(
            "via rules | [1] fade out | NOTE: a wake also needs a play",
        );
        let spoken = plan_echo(&parts);
        assert_eq!(spoken.len(), 2);
        assert_eq!(spoken[0].state, State::Echo);
        assert_eq!(
            spoken[0].lines,
            vec![
                "(via rules)".to_string(),
                "[1] fade out".to_string(),
                CONFIRM_LINE.to_string(),
            ]
        );
        assert_eq!(spoken[1].state, State::Note);
        assert_eq!(spoken[1].lines, vec!["NOTE: a wake also needs a play".to_string()]);
    }

    #[test]
    fn arm_pairs_distinguish_live_armed_and_the_bare_fallback() {
        let live = from_arm_pairs(&[("result".into(), "played Blue".into())], &[]);
        assert_eq!(live.state, State::Live);
        assert_eq!(live.lines, vec!["played Blue".to_string()]);

        let armed = from_arm_pairs(&[("plan_id".into(), "4".into())], &[]);
        assert_eq!(armed.state, State::Armed);
        assert_eq!(armed.lines, vec!["armed (plan 4)".to_string()]);

        let bare = armed_fallback(&[]);
        assert_eq!(bare.lines, vec!["armed".to_string()]);
        let named = armed_fallback(&["fade out".into()]);
        assert_eq!(named.lines, vec!["armed: fade out".to_string()]);
    }

    #[test]
    fn a_refusal_is_the_mapped_sentence() {
        let s = refusal_ack("not understood");
        assert_eq!(s.state, State::Refused);
        assert!(!s.text().is_empty());
        assert!(!s.text().contains('!'));
    }
}
