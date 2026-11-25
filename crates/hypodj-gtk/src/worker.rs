//! The IO side. Every socket lives on a std thread; the GTK main loop only ever
//! receives [`Msg`] values over an `async_channel` and never blocks.
//!
//! - ONE command thread owns ONE persistent [`MpdConn`]. The daemon stamps a
//!   per-connection owner key, so `nl confirm` / `nl cancel` only work on the socket
//!   that ran the translate - which is why requests are serialised here rather than
//!   each opening its own connection.
//! - An idle thread parks on `idle` and asks the command thread to refresh.
//! - Art and search are slow and stateless, so each runs on its own short-lived
//!   socket and can never delay a transport press.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use std::time::Duration;

use hypodj_client::model::{now_playing, parse_queue, NowPlaying, QueueItem};
use hypodj_client::mpd::{MpdConn, MpdError};
use hypodj_client::nl::{answer_from_pairs, echo_from_pairs, nl_request, quote_arg, split_echo, token_from_pairs};
use hypodj_client::route::{route, Action};
use hypodj_client::spoken;

use crate::view::{self, AlbumRow, Hit, SongRow};

/// What the UI asks for.
#[derive(Debug)]
pub enum Req {
    Refresh,
    /// One MPD command line (transport, setvol, add, play).
    Command(String),
    /// Add `uri` to the queue and start playing it.
    PlayUri(String),
    /// What the user typed in the ask entry: routed like `djc`.
    Ask(String),
    Confirm(String),
    Cancel(String),
    Albums,
    Album { uri: String, title: String },
    Search(String),
    Art(String),
}

/// What the UI receives.
pub enum Msg {
    Snapshot { now: Box<NowPlaying>, queue: Vec<QueueItem> },
    /// The connection is down; the UI says so in words and keeps its last state.
    Down(String),
    Albums { rows: Vec<AlbumRow>, note: Option<String> },
    Album { uri: String, title: String, rows: Vec<SongRow> },
    Search { query: String, result: Result<Vec<Hit>, String> },
    Art { file: String, bytes: Option<Vec<u8>> },
    /// One sentence for a toast.
    Say(String),
    /// A plan echoed and waiting: the UI asks, then sends Confirm or Cancel.
    Confirm { token: String, steps: Vec<String>, note: Option<String> },
    /// A destructive verb the router gates behind a yes/no (`clear`).
    ConfirmCommand { command: String, question: String },
    /// Show the Playing page (the router's `now` / `queue`).
    ShowPlaying,
}

pub struct Handles {
    pub cmd: Sender<Req>,
}

pub fn spawn(host: String, port: u16, out: async_channel::Sender<Msg>) -> Handles {
    let (tx, rx) = channel::<Req>();
    {
        let (h, out) = (host.clone(), out.clone());
        thread::spawn(move || command_thread(&h, port, rx, out));
    }
    {
        let (h, tx) = (host.clone(), tx.clone());
        thread::spawn(move || idle_thread(&h, port, tx));
    }
    let _ = tx.send(Req::Refresh);
    let _ = tx.send(Req::Albums);
    Handles { cmd: tx }
}

fn idle_thread(host: &str, port: u16, tx: Sender<Req>) {
    loop {
        let Ok(mut conn) = MpdConn::connect(host, port) else {
            thread::sleep(Duration::from_secs(2));
            continue;
        };
        if conn.clear_read_timeout().is_err() {
            continue;
        }
        while conn.idle_once().is_ok() {
            if tx.send(Req::Refresh).is_err() {
                return;
            }
        }
        // Dropped: the command thread will notice too; ask it to report.
        if tx.send(Req::Refresh).is_err() {
            return;
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn command_thread(host: &str, port: u16, rx: Receiver<Req>, out: async_channel::Sender<Msg>) {
    let mut conn: Option<MpdConn> = None;
    // A request read ahead while coalescing refreshes, run next - never dropped.
    let mut ahead: Option<Req> = None;
    while let Some(req) = ahead.take().or_else(|| rx.recv().ok()) {
        // Coalesce a burst of idle wakes into one refresh.
        if matches!(req, Req::Refresh) {
            loop {
                match rx.try_recv() {
                    Ok(Req::Refresh) => continue,
                    Ok(other) => {
                        ahead = Some(other);
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
        // Art and search never touch the command socket.
        match req {
            Req::Art(file) => {
                let (h, out) = (host.to_string(), out.clone());
                thread::spawn(move || {
                    let bytes = hypodj_client::art::fetch_albumart(&h, port, &file);
                    let _ = out.send_blocking(Msg::Art { file, bytes });
                });
                continue;
            }
            Req::Search(query) => {
                let (h, out) = (host.to_string(), out.clone());
                thread::spawn(move || {
                    let result = MpdConn::connect(&h, port)
                        .map_err(|_| "search unavailable - could not reach the player".to_string())
                        .and_then(|mut c| match c.command(&format!("searchall {}", quote_arg(&query))) {
                            Ok(p) => Ok(view::search_hits(&p)),
                            Err(MpdError::Ack(m)) => Err(hypodj_client::nl::map_ack_reason(&m)),
                            Err(_) => Err("search timed out - try a narrower query".into()),
                        });
                    let _ = out.send_blocking(Msg::Search { query, result });
                });
                continue;
            }
            Req::Albums => {
                let (h, out) = (host.to_string(), out.clone());
                thread::spawn(move || {
                    let msg = MpdConn::connect(&h, port)
                        .and_then(|mut c| {
                            // A whole-library listing pages through the server and
                            // can outlast the interactive 5 s timeout; nobody is
                            // waiting on a transport press here.
                            let _ = c.set_read_timeout(Some(Duration::from_secs(60)));
                            albums(&mut c)
                        })
                        .unwrap_or_else(|e| Msg::Albums {
                            rows: Vec::new(),
                            note: Some(format!("Could not list albums - {e}")),
                        });
                    let _ = out.send_blocking(msg);
                });
                continue;
            }
            _ => {}
        }
        if conn.is_none() {
            match MpdConn::connect(host, port) {
                Ok(c) => conn = Some(c),
                Err(e) => {
                    let _ = out.send_blocking(Msg::Down(format!("cannot reach the player at {host}:{port} - {e}")));
                    continue;
                }
            }
        }
        let c = conn.as_mut().expect("connected above");
        let mut msgs = Vec::new();
        match handle(c, req, &mut msgs) {
            Ok(()) => {}
            Err(MpdError::Ack(m)) => msgs.push(Msg::Say(spoken::refusal_ack(&m).text())),
            Err(e) => {
                conn = None;
                msgs.push(Msg::Down(format!("lost the player - {e}")));
            }
        }
        for m in msgs {
            if out.send_blocking(m).is_err() {
                return;
            }
        }
    }
}

fn snapshot(c: &mut MpdConn, msgs: &mut Vec<Msg>) -> Result<(), MpdError> {
    let status = c.command("status")?;
    let current = c.command("currentsong")?;
    let queue = parse_queue(&c.command("playlistinfo")?);
    msgs.push(Msg::Snapshot { now: Box::new(now_playing(&status, &current)), queue });
    Ok(())
}

fn handle(c: &mut MpdConn, req: Req, msgs: &mut Vec<Msg>) -> Result<(), MpdError> {
    match req {
        Req::Refresh => snapshot(c, msgs),
        Req::Command(line) => {
            let pairs = c.command(&line)?;
            if let Some(s) = spoken::command_spoken(&line, &pairs) {
                msgs.push(Msg::Say(s.text()));
            }
            snapshot(c, msgs)
        }
        Req::PlayUri(uri) => {
            let before: usize = hypodj_client::model::find(&c.command("status")?, "playlistlength")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            c.command(&format!("add {}", quote_arg(&uri)))?;
            c.command(&format!("play {before}"))?;
            snapshot(c, msgs)
        }
        Req::Ask(text) => ask(c, &text, msgs),
        Req::Confirm(token) => {
            let pairs = c.command(&format!("nl confirm {token}"))?;
            msgs.push(Msg::Say(spoken::from_confirm(&pairs).text()));
            snapshot(c, msgs)
        }
        Req::Cancel(token) => {
            let _ = c.command(&format!("nl cancel {token}"));
            Ok(())
        }
        Req::Album { uri, title } => {
            let rows = view::songs(&c.command(&format!("lsinfo {}", quote_arg(&uri)))?);
            msgs.push(Msg::Album { uri, title, rows });
            Ok(())
        }
        Req::Art(_) | Req::Search(_) | Req::Albums => Ok(()),
    }
}

/// The ask entry, routed exactly like `djc`: a bare verb (or verb plus its one
/// scalar) runs deterministically; everything else is natural language, where a
/// question comes back as an answer and an action comes back as a plan to confirm.
fn ask(c: &mut MpdConn, text: &str, msgs: &mut Vec<Msg>) -> Result<(), MpdError> {
    let args: Vec<String> = text.split_whitespace().map(str::to_string).collect();
    match route(&args) {
        Action::NowPlaying | Action::Queue => {
            msgs.push(Msg::ShowPlaying);
            snapshot(c, msgs)
        }
        Action::Command(line) => handle(c, Req::Command(line), msgs),
        Action::ClearConfirm => {
            msgs.push(Msg::ConfirmCommand {
                command: "clear".into(),
                question: "Clear the whole queue?".into(),
            });
            Ok(())
        }
        Action::Help => {
            msgs.push(Msg::Say(hypodj_client::nl::not_understood_hint()));
            Ok(())
        }
        Action::FavoriteCurrent => {
            let cur = c.command("currentsong")?;
            let uri = hypodj_client::model::find(&cur, "X-MatchUri")
                .or_else(|| hypodj_client::model::find(&cur, "file").filter(|f| f.starts_with("song/")));
            match uri {
                Some(u) => {
                    let line = format!("playlistadd Starred {}", quote_arg(u));
                    handle(c, Req::Command(line), msgs)
                }
                None => {
                    msgs.push(Msg::Say("Nothing to favourite - this is not a song in the library.".into()));
                    Ok(())
                }
            }
        }
        Action::ToggleMode(m) => {
            let status = c.command("status")?;
            let line = now_playing(&status, &[]).modes.toggle_command(m);
            handle(c, Req::Command(line), msgs)
        }
        Action::Nl(phrase) => {
            let pairs = c.command(&nl_request(&phrase))?;
            if let Some(answer) = answer_from_pairs(&pairs) {
                let words = spoken::command_spoken("nl", &pairs)
                    .unwrap_or_else(|| spoken::live_text(answer));
                msgs.push(Msg::Say(words.text()));
                return Ok(());
            }
            match token_from_pairs(&pairs) {
                Some(token) => {
                    let parts = echo_from_pairs(&pairs).map(|e| split_echo(&e));
                    let (steps, note) = parts.map(|p| (p.steps, p.note)).unwrap_or_default();
                    msgs.push(Msg::Confirm { token, steps, note });
                }
                None => msgs.push(Msg::Say("The player returned no plan to confirm.".into())),
            }
            Ok(())
        }
    }
}

/// The Albums page: the server's listing joined with what is on disk. With the
/// server unreachable the held albums alone are the page.
fn albums(c: &mut MpdConn) -> Result<Msg, MpdError> {
    let listed = match c.command("lsinfo albums/all") {
        Ok(p) => Ok(p),
        Err(MpdError::Ack(m)) => Err(hypodj_client::nl::map_ack_reason(&m)),
        Err(e) => return Err(e),
    };
    let held = match c.command("store albums") {
        Ok(p) => Some(p),
        Err(MpdError::Ack(_)) => None,
        Err(e) => return Err(e),
    };
    let rows = view::albums(listed.as_deref().ok(), held.as_deref());
    let note = match &listed {
        Ok(_) => None,
        Err(reason) if rows.is_empty() => Some(format!("Could not list albums: {reason}")),
        Err(_) => Some(format!(
            "The server is unreachable, showing {} on disk",
            if rows.len() == 1 { "1 album".to_string() } else { format!("{} albums", rows.len()) }
        )),
    };
    Ok(Msg::Albums { rows, note })
}
