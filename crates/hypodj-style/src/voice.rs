//! Echo-before-arm, with proportional confirmation.
//!
//! An immediate, reversible act (pause, skip, volume) goes straight to live
//! and carries an undo line. It does not echo. Only a future trigger, a stop,
//! a clear, or a wall-clock plan passes through echo and waits.
//!
//! The words are the meaning. Colour never carries them alone.

/// Echo, armed, live, refused. The four states of the grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Echo,
    Armed,
    Live,
    Refused,
}

/// What the person asked for, at the grain the confirmation rule cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Pause,
    Skip,
    Volume,
    FutureTrigger,
    Stop,
    Clear,
    WallClock,
}

/// One state, in words. `undo` is set only for an immediate live act.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spoken {
    pub state: State,
    pub words: String,
    pub undo: Option<String>,
}

/// How this act is spoken. Immediate acts are present tense plus undo.
/// Deferred acts are future tense and say how to confirm.
pub fn speak(act: Act) -> Spoken {
    match act {
        Act::Pause => live("Pausing.", "Undo: resume."),
        Act::Skip => live("Skipping.", "Undo: go back."),
        Act::Volume => live("Changing the volume.", "Undo: restore the previous level."),
        Act::FutureTrigger => echo("Will do: run that later. Confirm to arm."),
        Act::Stop => echo("Will do: stop. Confirm to arm."),
        Act::Clear => echo("Will do: clear the queue. Confirm to arm."),
        Act::WallClock => echo("Will do: run at the set time. Confirm to arm."),
    }
}

fn live(words: &str, undo: &str) -> Spoken {
    Spoken {
        state: State::Live,
        words: words.to_string(),
        undo: Some(undo.to_string()),
    }
}

fn echo(words: &str) -> Spoken {
    Spoken {
        state: State::Echo,
        words: words.to_string(),
        undo: None,
    }
}

/// A refusal is one calm sentence: what did not happen, and why.
pub fn refused(why: &str) -> Spoken {
    Spoken {
        state: State::Refused,
        words: why.to_string(),
        undo: None,
    }
}

/// The confirm prompt both surfaces show. Echo: quiet ink, underlined.
pub fn confirm_prompt() -> &'static str {
    "confirm? [y/N]"
}

/// A note above a confirm. Words, no alarm mark.
pub fn note_line(note: &str) -> String {
    format!("Note: {note}")
}

/// An armed banner. Ember on every surface. Wording matches the clients.
pub fn armed_banner(what: &str) -> String {
    format!("armed: {what}")
}

/// One line of each state, the lines the CLI and the TUI both render.
pub fn parity_lines() -> Vec<Spoken> {
    vec![
        Spoken { state: State::Echo, words: confirm_prompt().to_string(), undo: None },
        Spoken { state: State::Armed, words: armed_banner("sleep in 12m"), undo: None },
        speak(Act::Pause),
        refused("That did not run, because nothing is playing."),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immediate_acts_skip_echo_and_carry_undo() {
        for act in [Act::Pause, Act::Skip, Act::Volume] {
            let s = speak(act);
            assert_eq!(s.state, State::Live, "{act:?}");
            assert!(s.undo.is_some(), "{act:?} has an undo line");
            assert!(!s.words.starts_with("Will do"), "{act:?} is not an echo");
            assert!(!s.words.contains('!'));
            assert!(!s.words.contains('\u{2014}'));
        }
    }

    #[test]
    fn deferred_acts_echo_and_wait() {
        for act in [Act::FutureTrigger, Act::Stop, Act::Clear, Act::WallClock] {
            let s = speak(act);
            assert_eq!(s.state, State::Echo, "{act:?}");
            assert!(s.undo.is_none());
            assert!(s.words.starts_with("Will do: "), "{act:?}");
            assert!(s.words.contains("Confirm"), "{act:?} says how to confirm");
        }
    }

    #[test]
    fn samples_cover_every_state() {
        let states: Vec<_> = parity_lines().into_iter().map(|s| s.state).collect();
        assert!(states.contains(&State::Echo));
        assert!(states.contains(&State::Armed));
        assert!(states.contains(&State::Live));
        assert!(states.contains(&State::Refused));
    }
}
