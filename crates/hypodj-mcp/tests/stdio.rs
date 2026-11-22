//! The surface as harn actually meets it: the real `dj-mcp` binary, driven over a
//! real pipe, talking to a real socket.
//!
//! Everything asserted here is an OBSERVATION rather than an exit code - either the
//! JSON that came back out of the child's stdout, or the exact bytes a scripted MPD
//! daemon received on the wire. That second half is the important one: the four hard
//! rules are all claims about what does and does not reach the socket, and only a
//! transcript can settle them.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};

/// A scripted stand-in for the daemon. It answers the handful of verbs `dj-mcp`
/// sends, keeps its own little journal so the read-back path is real, and records
/// every line it received so a test can assert on the transcript.
#[derive(Default)]
struct Wire {
    /// Every command line received, in order.
    log: Vec<String>,
    /// (id, origin, act, did, undoable), newest last.
    journal: Vec<(u64, String, String, String, bool)>,
    next_jid: u64,
    /// When true the next `plan add` reports a no-op: a `result` and NO journal
    /// entry, which is what an action that changed nothing really does.
    noop_next: bool,
}

struct Daemon {
    port: u16,
    wire: Arc<Mutex<Wire>>,
}

fn spawn_daemon() -> Daemon {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("addr").port();
    let wire = Arc::new(Mutex::new(Wire { next_jid: 1, ..Wire::default() }));
    let w = wire.clone();
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(sock) = sock else { break };
            let w = w.clone();
            std::thread::spawn(move || serve(sock, w));
        }
    });
    Daemon { port, wire }
}

fn serve(sock: TcpStream, wire: Arc<Mutex<Wire>>) {
    let mut out = sock.try_clone().expect("clone");
    let mut reader = BufReader::new(sock);
    if out.write_all(b"OK MPD 0.23.5\n").is_err() {
        return;
    }
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let line = line.trim_end_matches(['\n', '\r']).to_string();
        let reply = {
            let mut w = wire.lock().expect("wire");
            w.log.push(line.clone());
            respond(&mut w, &line)
        };
        if out.write_all(reply.as_bytes()).is_err() {
            return;
        }
    }
}

fn respond(w: &mut Wire, line: &str) -> String {
    let toks = hypodj_dsl::tokenize(line);
    let verb = toks.first().map(|s| s.as_str()).unwrap_or("");
    match verb {
        "status" => "volume: 70\nplaylist: 4\nplaylistlength: 2\nstate: play\nsong: 0\nsongid: 11\nduration: 215.000\nOK\n".into(),
        "currentsong" => "file: song/42\nTitle: Blue in Green\nArtist: Miles Davis\nAlbum: Kind of Blue\nOK\n".into(),
        "playlistinfo" => concat!(
            "file: song/42\nTitle: Blue in Green\nArtist: Miles Davis\nAlbum: Kind of Blue\nTime: 337\nPos: 0\nId: 11\n",
            "file: song/43\nTitle: So What\nArtist: Miles Davis\nAlbum: Kind of Blue\nTime: 545\nPos: 1\nId: 12\n",
            "OK\n"
        )
        .into(),
        "search" => "file: song/99\nTitle: Good Vibes\nArtist: Someone\nAlbum: An Album\nOK\n".into(),
        "heard" => "heard: 3 rows, this session\nheard: 18:02  Miles Davis - So What\nOK\n".into(),
        "journal" => {
            let mut s = String::new();
            for (id, origin, act, did, undoable) in w.journal.iter().rev() {
                s.push_str(&format!(
                    "jid: {id}\njage: 3\njorigin: {origin}\njact: {act}\njdid: {did}\njundoable: {}\n",
                    if *undoable { 1 } else { 0 }
                ));
            }
            s.push_str("OK\n");
            s
        }
        "plan" => {
            let origin = toks
                .iter()
                .position(|t| t == "origin")
                .and_then(|i| toks.get(i + 1))
                .cloned()
                .unwrap_or_default();
            let act = toks
                .iter()
                .position(|t| t == "action")
                .and_then(|i| toks.get(i + 1))
                .cloned()
                .unwrap_or_default();
            if w.noop_next {
                w.noop_next = false;
                // A `result` with NO journal entry: exactly the shape of an action
                // that changed nothing and so took no ring slot.
                return "plan_id: 9\nresult: added 0\nOK\n".into();
            }
            let id = w.next_jid;
            w.next_jid += 1;
            // The journal's sentence is DELIBERATELY different from the `result`
            // pair, so a test can tell which one a tool reported.
            w.journal.push((id, origin, act, format!("journal says {id}"), true));
            "plan_id: 9\nresult: result pair says something else\nOK\n".into()
        }
        "undo" => {
            let want: Option<u64> = toks.get(1).and_then(|t| t.parse().ok());
            match w.journal.iter().rposition(|e| e.4 && Some(e.0) == want) {
                Some(i) => {
                    let e = w.journal.remove(i);
                    let id = w.next_jid;
                    w.next_jid += 1;
                    w.journal.push((id, "undo".into(), "undo".into(), format!("undone: {}", e.3), false));
                    format!("jid: {}\njdid: undone: {}\nOK\n", e.0, e.3)
                }
                None => "ACK [50@0] {undo} nothing to undo\n".into(),
            }
        }
        "pause" => "OK\n".into(),
        _ => "ACK [5@0] {} unknown command\n".into(),
    }
}

/// The child, plus a line-reader on its stdout.
struct Server {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl Server {
    fn start(port: u16) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_dj-mcp"))
            .args(["--host", "127.0.0.1", "--port", &port.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn dj-mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || pump(stdout, tx));
        Server { child, stdin, lines: rx }
    }

    /// Send one request and read its response. Every request this test sends has an
    /// id, so exactly one line comes back.
    fn call(&mut self, req: &str) -> serde_json::Value {
        writeln!(self.stdin, "{req}").expect("write to child");
        self.stdin.flush().expect("flush");
        let line = self
            .lines
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("the child answered");
        serde_json::from_str(&line).expect("a response line is valid JSON")
    }

    /// Send a notification and assert nothing comes back within a short window.
    fn notify(&mut self, req: &str) {
        writeln!(self.stdin, "{req}").expect("write to child");
        self.stdin.flush().expect("flush");
        assert!(
            self.lines.recv_timeout(std::time::Duration::from_millis(300)).is_err(),
            "a notification must be answered with silence"
        );
    }

    /// Run a tool and return the parsed JSON its text block carries.
    fn tool(&mut self, name: &str, args: serde_json::Value) -> (bool, serde_json::Value, String) {
        let req = serde_json::json!({
            "jsonrpc": "2.0", "id": 99, "method": "tools/call",
            "params": {"name": name, "arguments": args},
        });
        let resp = self.call(&req.to_string());
        assert!(resp.get("error").is_none(), "a tool fault must be a RESULT: {resp}");
        let is_error = resp["result"]["isError"].as_bool().unwrap_or(false);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("").to_string();
        let parsed = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        (is_error, parsed, text)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn pump(stdout: ChildStdout, tx: mpsc::Sender<String>) {
    for line in BufReader::new(stdout).lines() {
        match line {
            Ok(l) => {
                if tx.send(l).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

fn handshake(s: &mut Server) {
    let init = s.call(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"harn","version":"0"}}}"#);
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    s.notify(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
}

fn log_of(d: &Daemon) -> Vec<String> {
    d.wire.lock().expect("wire").log.clone()
}

#[test]
fn the_whole_surface_over_a_real_pipe() {
    let d = spawn_daemon();
    let mut s = Server::start(d.port);
    handshake(&mut s);

    // tools/list: thirteen, with the read-only hints harn's auto-approval trusts.
    let list = s.call(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
    let tools = list["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 13);
    let hint = |n: &str| {
        tools
            .iter()
            .find(|t| t["name"] == n)
            .unwrap_or_else(|| panic!("{n} missing"))["annotations"]["readOnlyHint"]
            .as_bool()
    };
    assert_eq!(hint("dj_queue"), Some(true));
    assert_eq!(hint("dj_clear"), Some(false));

    // A read reaches the wire and comes back structured, with the qid the write
    // tools take.
    let (err, q, _) = s.tool("dj_queue", serde_json::json!({}));
    assert!(!err);
    assert_eq!(q["total"], 2);
    assert_eq!(q["items"][1]["qid"], 12);
    assert_eq!(q["items"][1]["title"], "So What");

    // RULE 3 + RULE 4, as a wire transcript: the multi-word value arrived as ONE
    // quoted token, the trigger is immediate, and the origin is the last thing on
    // the line.
    let (err, r, text) = s.tool("dj_enqueue", serde_json::json!({"what": "good vibes"}));
    assert!(!err, "{text}");
    let sent = log_of(&d).into_iter().find(|l| l.starts_with("plan add")).expect("a plan add");
    let toks = hypodj_dsl::tokenize(&sent);
    assert_eq!(
        &toks[..8],
        &["plan", "add", "trigger", "immediate", "action", "enqueue", "query", "good vibes"]
            .map(String::from)
    );
    assert_eq!(toks[8], "3", "the default count");
    assert_eq!(toks[9], "origin");
    let origin = toks[10].clone();
    assert!(origin.starts_with("mcp:sess-"), "{origin}");
    assert_eq!(toks.len(), 11, "nothing after the origin: {sent}");

    // `did` is the JOURNAL's sentence, not the `result` pair the same reply carried
    // and not anything the caller said. The stand-in makes the two differ on purpose.
    assert_eq!(r["did"], "journal says 1");
    assert_eq!(r["jid"], 1);
    assert_eq!(r["undoable"], true);
    assert_eq!(r["queue_len"], 2);

    // An action that changed nothing takes no ring slot, and must NOT inherit the
    // previous entry's sentence. This is the before/after id gate doing its job.
    d.wire.lock().expect("wire").noop_next = true;
    let (err, r, text) = s.tool("dj_enqueue", serde_json::json!({"what": "nothing at all"}));
    assert!(!err, "{text}");
    assert_eq!(r["did"], "added 0", "the daemon's own result pair, not entry 1");
    assert_eq!(r["jid"], serde_json::Value::Null);
    assert_eq!(r["undoable"], false);

    // dj_undo retracts this session's own entry, and reports the daemon's words for
    // the retraction.
    let (err, r, text) = s.tool("dj_undo", serde_json::json!({}));
    assert!(!err, "{text}");
    assert!(log_of(&d).iter().any(|l| l == "undo 1"), "it named the entry");
    assert_eq!(r["did"], "undone: journal says 1");

    // ... and refuses to retract an entry that is not this session's.
    {
        let mut w = d.wire.lock().expect("wire");
        let id = w.next_jid;
        w.next_jid += 1;
        w.journal.push((id, "mpd".into(), "clear".into(), "cleared 9".into(), true));
    }
    let (err, _, text) = s.tool("dj_undo", serde_json::json!({}));
    assert!(err, "{text}");
    assert!(text.contains("not this session's"), "{text}");
    assert!(text.contains("cleared 9"), "it says what it saw: {text}");

    // The one wire verb, and the honest claim about it.
    let (err, r, text) = s.tool("dj_transport", serde_json::json!({"do": "resume"}));
    assert!(!err, "{text}");
    assert!(log_of(&d).iter().any(|l| l == "pause 0"));
    assert_eq!(r["did"], "resumed");
    assert_eq!(r["undoable"], false);
}

#[test]
fn nothing_reaches_the_wire_that_should_not() {
    let d = spawn_daemon();
    let mut s = Server::start(d.port);
    handshake(&mut s);
    // Touch the wire once so the socket is definitely open - otherwise "no plan add
    // was sent" could be true merely because nothing ever connected.
    let (err, _, _) = s.tool("dj_now", serde_json::json!({}));
    assert!(!err);
    assert!(!log_of(&d).is_empty(), "the socket is live");

    // RULE 2: a newline in a value is refused, and no `setvol` line exists anywhere
    // in the transcript.
    let (err, _, text) = s.tool("dj_enqueue", serde_json::json!({"what": "a\nsetvol 100"}));
    assert!(err, "{text}");
    assert!(text.contains("control character"), "{text}");

    // RULE 1: an invented key fails the call. This is the one harn does not check,
    // so it is the only thing standing between a poisoned preview and the daemon.
    let (err, _, text) = s.tool("dj_clear", serde_json::json!({"path": "/", "scope": "all"}));
    assert!(err, "{text}");
    assert!(text.contains("unknown field `path`"), "{text}");

    // Bounds are advice in the schema and enforcement here.
    let (err, _, text) = s.tool("dj_enqueue", serde_json::json!({"what": "jazz", "count": 40}));
    assert!(err, "{text}");
    assert!(text.contains("1..=10"), "{text}");
    let (err, _, text) = s.tool("dj_volume", serde_json::json!({"to": 400}));
    assert!(err, "{text}");
    assert!(text.contains("0..=100"), "{text}");

    let log = log_of(&d);
    assert!(!log.iter().any(|l| l.contains("setvol")), "no injected verb: {log:?}");
    assert!(!log.iter().any(|l| l.starts_with("plan add")), "no write at all: {log:?}");
    assert!(!log.iter().any(|l| l.contains("clear")), "no clear: {log:?}");
}

#[test]
fn it_starts_and_keeps_serving_with_no_daemon_and_across_a_daemon_restart() {
    // harn spawns this child once and never restarts it, so a server that dies with
    // the daemon is absent for harn's whole run.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener); // nothing is listening on `port` now

    let mut s = Server::start(port);
    handshake(&mut s);
    let (err, _, text) = s.tool("dj_now", serde_json::json!({}));
    assert!(err, "{text}");
    assert!(text.contains("not reachable"), "{text}");

    // Now bring a daemon up on that port and use the SAME server process.
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("rebind");
    let wire = Arc::new(Mutex::new(Wire { next_jid: 1, ..Wire::default() }));
    let w = wire.clone();
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(sock) = sock else { break };
            let w = w.clone();
            std::thread::spawn(move || serve(sock, w));
        }
    });
    let (err, now, text) = s.tool("dj_now", serde_json::json!({}));
    assert!(!err, "{text}");
    assert_eq!(now["title"], "Blue in Green");
    assert_eq!(now["qid"], 11);
    assert_eq!(now["state"], "play");
    // hypodj's status carries no `elapsed` pair; the tool says so rather than
    // inventing a number.
    assert_eq!(now["elapsed"], serde_json::Value::Null);
}
