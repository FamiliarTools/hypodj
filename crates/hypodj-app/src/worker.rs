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
    // The ONE plan waiting on the user. A UI shows one confirm at a time, so a
    // newer plan supersedes the older: the older token is cancelled on the daemon
    // here, before the UI is handed the newer one, and can never be left pending
    // behind a dialog that no longer shows it.
    let mut pending: Option<String> = None;
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
        if let Req::Confirm(t) | Req::Cancel(t) = &req {
            if pending.as_deref() == Some(t.as_str()) {
                pending = None;
            }
        }
        match handle(c, req, &mut msgs) {
            Ok(()) => {}
            Err(MpdError::Ack(m)) => msgs.push(Msg::Say(spoken::refusal_ack(&m).text())),
            Err(e) => {
                conn = None;
                msgs.push(Msg::Down(format!("lost the player - {e}")));
            }
        }
        if let Some(c) = conn.as_mut() {
            supersede_pending(c, &mut pending, &msgs);
        }
        for m in msgs {
            if out.send_blocking(m).is_err() {
                return;
            }
        }
    }
}

/// One pending confirm at a time: when `msgs` hands the UI a new plan, cancel the
/// one it replaces (best effort - an expired token is already gone) and remember
/// the new one.
fn supersede_pending(c: &mut MpdConn, pending: &mut Option<String>, msgs: &[Msg]) {
    let Some(token) = msgs.iter().rev().find_map(|m| match m {
        Msg::Confirm { token, .. } => Some(token.clone()),
        _ => None,
    }) else {
        return;
    };
    if let Some(old) = pending.replace(token) {
        if Some(&old) != pending.as_ref() {
            let _ = c.command(&format!("nl cancel {old}"));
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

/// The `Id` an `addid` answered with (an album's first row), if any.
fn play_uri_id(added: &[(String, String)]) -> Option<&str> {
    hypodj_client::model::find(added, "Id").filter(|v| v.parse::<u64>().is_ok())
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
            // `addid` then `playid`: the daemon names the row it added, so an add from
            // another connection between the two cannot make us start the wrong one
            // (reading playlistlength first and then `play N` could).
            let added = c.command(&format!("addid {}", quote_arg(&uri)))?;
            if let Some(id) = play_uri_id(&added) {
                c.command(&format!("playid {id}"))?;
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::sync::{Arc, Mutex};

    /// A fake MPD on a loopback socket that records every command line and answers
    /// `addid` with `Id: 7` and a `playlistlength` of 3 in `status`, everything else OK.
    fn fake_mpd() -> Option<(u16, Arc<Mutex<Vec<String>>>)> {
        let l = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
        let port = l.local_addr().ok()?.port();
        let log = Arc::new(Mutex::new(Vec::new()));
        let seen = log.clone();
        std::thread::spawn(move || {
            let Ok((sock, _)) = l.accept() else { return };
            let mut w = sock.try_clone().expect("clone");
            let _ = w.write_all(b"OK MPD 0.23.0\n");
            for line in BufReader::new(sock).lines() {
                let Ok(line) = line else { return };
                let reply = if line.starts_with("addid") {
                    "Id: 7\nOK\n"
                } else if line == "status" {
                    "playlistlength: 3\nOK\n"
                } else {
                    "OK\n"
                };
                seen.lock().unwrap().push(line);
                if w.write_all(reply.as_bytes()).is_err() {
                    return;
                }
            }
        });
        Some((port, log))
    }

    /// PlayUri must start the row the daemon says it added (`playid <Id>`), never a
    /// position read before the add: a concurrent add would make that the wrong row.
    #[test]
    fn play_uri_starts_the_added_id_not_a_position() {
        let Some((port, log)) = fake_mpd() else { return };
        let mut c = MpdConn::connect("127.0.0.1", port).expect("connect");
        let mut msgs = Vec::new();
        handle(&mut c, Req::PlayUri("song/s-1".into()), &mut msgs).expect("handled");
        let log = log.lock().unwrap().clone();
        assert!(log.iter().any(|l| l == "playid 7"), "{log:?}");
        assert!(!log.iter().any(|l| l.starts_with("play ") || l.starts_with("add ")), "{log:?}");
    }

    fn plan(token: &str) -> Msg {
        Msg::Confirm { token: token.into(), steps: vec![], note: None }
    }

    /// A second plan while the first is still waiting cancels the first on the
    /// daemon: one pending confirm at a time, never an orphaned token.
    #[test]
    fn a_newer_plan_cancels_the_one_it_supersedes() {
        let Some((port, log)) = fake_mpd() else { return };
        let mut c = MpdConn::connect("127.0.0.1", port).expect("connect");
        let mut pending = None;
        supersede_pending(&mut c, &mut pending, &[plan("a")]);
        assert_eq!(pending.as_deref(), Some("a"));
        supersede_pending(&mut c, &mut pending, &[Msg::Say("x".into())]);
        assert_eq!(pending.as_deref(), Some("a"), "a non-plan message keeps the pending one");
        supersede_pending(&mut c, &mut pending, &[plan("b")]);
        assert_eq!(pending.as_deref(), Some("b"));
        // A ping round trip so the fake has logged every line before we read it.
        c.command("ping").expect("ping");
        let log = log.lock().unwrap().clone();
        assert_eq!(log.iter().filter(|l| l.starts_with("nl cancel")).collect::<Vec<_>>(), ["nl cancel a"]);
    }
}
