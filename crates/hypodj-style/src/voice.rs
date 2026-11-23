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

/// Armed, after a deferred act was confirmed. Present plan, past decision.
pub fn armed(what: &str) -> Spoken {
    Spoken {
        state: State::Armed,
        words: format!("Armed: {what}"),
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

/// One sample of each state, for the cross-surface parity check.
pub fn spoken_states() -> Vec<Spoken> {
    vec![
        speak(Act::Stop),
        armed("stop at the end of this track."),
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
        let states: Vec<_> = spoken_states().into_iter().map(|s| s.state).collect();
        assert!(states.contains(&State::Echo));
        assert!(states.contains(&State::Armed));
        assert!(states.contains(&State::Live));
        assert!(states.contains(&State::Refused));
    }
}
