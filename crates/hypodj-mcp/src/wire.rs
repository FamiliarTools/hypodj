//! The one socket to the daemon, and the outcome vocabulary a non-interactive
//! caller needs.
//!
//! Three properties this module exists to hold:
//!
//! - **Lazy + self-healing.** harn spawns an MCP child ONCE, at startup, and a
//!   `Dead` server is absent for harn's whole lifetime (`McpServer` has no restart
//!   in v1). So `dj-mcp` must start fine with no daemon listening and must survive
//!   the daemon going away and coming back - a `nixos-rebuild switch` restarts it.
//!   The socket is therefore opened on first use and dropped on any transport
//!   error, so the next call reconnects.
//! - **A 30s read timeout, not the interactive 5s.** `plan add` with an immediate
//!   action runs the action INLINE in the daemon before answering (a Subsonic fetch
//!   plus an mpv load for a `playnow`), which routinely outlives 5s. A spurious
//!   timeout is worse than waiting: `MpdConn::command` returns WITHOUT draining the
//!   response frame, so the socket is desynchronised and the action may well have
//!   happened anyway.
//! - **"Unknown" is a real answer.** A write whose reply never came did not
//!   necessarily fail. Reporting it as failed invites the model to retry a
//!   destructive act that already landed, so a dropped write reports UNKNOWN and
//!   points at the read tools.

use std::time::Duration;

use hypodj_client::mpd::{MpdConn, MpdError};

/// The read timeout installed on the command socket. See the module note: the
/// interactive 5s default is what keeps a wedged daemon from freezing a UI, and
/// there is no UI here - harn imposes its own per-call ceiling.
const NON_INTERACTIVE_TIMEOUT: Duration = Duration::from_secs(30);

/// What happened on the wire, in the only three flavours a caller must tell apart.
#[derive(Debug)]
pub enum WireError {
    /// The daemon answered and REFUSED. Definite: nothing happened.
    Ack(String),
    /// Nothing was sent, or the daemon is not reachable at all. Definite: nothing
    /// happened.
    Down(String),
    /// The request went out and the answer did not come back. INDETERMINATE - the
    /// effect may have landed. Never report this as a failure.
    Unknown(String),
}

impl WireError {
    /// The sentence the model reads. Each one says what to do next, because a model
    /// that cannot tell "refused" from "did not hear back" retries a destructive act.
    pub fn message(&self) -> String {
        match self {
            WireError::Ack(m) => format!("hypodj refused: {m}"),
            WireError::Down(m) => format!("hypodj is not reachable: {m}"),
            WireError::Unknown(m) => format!(
                "no answer from hypodj ({m}); the action MAY have happened - \
                 call dj_now / dj_journal to see, and do not repeat it blindly"
            ),
        }
    }

    /// True for the indeterminate case, which the write tools report as
    /// `outcome: "unknown"` rather than `ok: false`.
    pub fn is_unknown(&self) -> bool {
        matches!(self, WireError::Unknown(_))
    }
}

/// The lazily-connected command socket.
pub struct Dj {
    host: String,
    port: u16,
    conn: Option<MpdConn>,
}

impl Dj {
    pub fn new(host: String, port: u16) -> Self {
        Dj { host, port, conn: None }
    }

    /// Open the socket if it is closed. A failed connect leaves it closed, so every
    /// later call retries rather than latching a dead state.
    fn ensure(&mut self) -> Result<&mut MpdConn, WireError> {
        if self.conn.is_none() {
            let c = MpdConn::connect(&self.host, self.port)
                .map_err(|e| WireError::Down(e.to_string()))?;
            // Best effort: a socket that would not take the timeout still works, it
            // just keeps the 5s default, and failing the whole call over that would
            // be worse than the risk it guards.
            let _ = c.set_read_timeout(Some(NON_INTERACTIVE_TIMEOUT));
            self.conn = Some(c);
        }
        Ok(self.conn.as_mut().expect("just connected"))
    }

    /// Run a READ. A transport error is `Down`: a read that did not answer changed
    /// nothing, so there is no ambiguity to preserve.
    pub fn read(&mut self, line: &str) -> Result<Vec<(String, String)>, WireError> {
        self.run(line, false)
    }

    /// Run a WRITE. A transport error is `Unknown` - see the module note.
    pub fn write(&mut self, line: &str) -> Result<Vec<(String, String)>, WireError> {
        self.run(line, true)
    }

    fn run(&mut self, line: &str, mutating: bool) -> Result<Vec<(String, String)>, WireError> {
        let conn = self.ensure()?;
        match conn.command(line) {
            Ok(pairs) => Ok(pairs),
            Err(MpdError::Ack(m)) => Err(WireError::Ack(m)),
            // Refused BEFORE any byte was written, so nothing happened and the
            // socket is still framed correctly. This should be unreachable - every
            // value is built through `hypodj_dsl::dsl_value`, which refuses control
            // chars - and it is kept as the structural backstop, not as a path.
            Err(MpdError::BadCommand) => Err(WireError::Down(
                "the command line carried a newline and was refused before sending".into(),
            )),
            Err(e) => {
                // Any transport-level fault poisons the frame: drop the socket so the
                // next call reconnects instead of reading someone else's answer.
                self.conn = None;
                let m = e.to_string();
                Err(if mutating { WireError::Unknown(m) } else { WireError::Down(m) })
            }
        }
    }
}

// The pair/record readers and the journal parser live in `hypodj_client::model`,
// not here: the CLI badge, the TUI footer and these tools all read the same frames,
// and a second copy of "which key names the id" is a second place it can drift.
pub use hypodj_client::model::{find, group_blocks_on as blocks, parse_journal};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_outcomes_read_differently_and_only_one_is_indeterminate() {
        assert!(WireError::Ack("no such plan".into()).message().contains("refused"));
        assert!(!WireError::Ack("x".into()).is_unknown());
        assert!(!WireError::Down("x".into()).is_unknown());
        let u = WireError::Unknown("timed out".into());
        assert!(u.is_unknown());
        assert!(u.message().contains("MAY have happened"), "{}", u.message());
        assert!(u.message().contains("dj_journal"), "it says where to look");
    }
}
