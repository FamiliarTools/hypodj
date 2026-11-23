//! A plan or a daemon reply: words plus the state those words are in.
//!
//! Plain data. The client builds it. A surface only renders it.

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
