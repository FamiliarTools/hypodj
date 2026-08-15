//! `dj-mcp` - the agent's whole reach into hypodj.
//!
//! A stdio MCP server. It speaks JSON-RPC on stdin/stdout to one client (harn) and
//! the MPD text protocol to one daemon (hypodj), and it is the ONLY thing an agent
//! persona is given: thirteen tools, five of which only look.
//!
//! The claim it is built to make true is narrow and worth stating exactly. This is
//! NOT a security boundary - the MPD socket has no authentication, `password` is not
//! a verb, and any local process can already do everything here with `nc`. What the
//! surface buys is that every effect the AGENT causes is (a) attributable, because
//! its only write channel is `plan add ... origin mcp:<sess>`, (b) described by
//! hypodj's own words rather than by the model's claim, because `did` is read back
//! out of the daemon's journal, and (c) retractable, because everything it can reach
//! is a `plan::Action` and every Action is journaled. Anything that cannot be
//! retracted is not exposed at all - absence, not permission. The twelve direct
//! Navidrome writes (star, rating, playlist, station) are reachable only from wire
//! verbs, so no tool here can reach them.
//!
//! Two operational facts shape the whole file. harn spawns an MCP child ONCE, at
//! startup, and never restarts it - so this process must start with no daemon
//! listening and must never exit on socket loss, or it is absent for harn's entire
//! run. And harn forwards the model's arguments to `tools/call` without validating
//! them against the schema it advertised - so every bound is enforced in Rust.

mod rpc;
mod tools;
mod wire;

use std::io::{BufRead, Read, Write};

use hypodj_client::config::{self, Env};
use serde_json::{json, Value};

const HELP: &str = "\
dj-mcp - the hypodj MCP tool surface (stdio JSON-RPC)

Spoken to by an MCP client (harn) on stdin/stdout; speaks MPD to the hypodj daemon.
Not meant to be run by hand, though piping JSON-RPC lines into it works.

OPTIONS:
  --host <h>    daemon host (default 127.0.0.1)
  --port <p>    daemon port (default 6600)
  -h, --help    this help
  -V, --version print version and exit

CONFIG precedence: flags > HYPODJ_HOST/HYPODJ_PORT > MPD_HOST/MPD_PORT
                   > 127.0.0.1:6600
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{HELP}");
        return;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("dj-mcp {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let (host, port) = match parse_args(&args) {
        Ok((h, p)) => {
            let env = Env { get: &|k| std::env::var(k).ok() };
            config::resolve(h, p, &env)
        }
        Err(e) => {
            eprintln!("dj-mcp: {e}");
            std::process::exit(2);
        }
    };
    let session = mint_session();
    let origin = format!("mcp:{session}");
    // stderr, never stdout: stdout is the JSON-RPC frame and one stray line
    // desynchronises the client permanently. harn inherits stderr, so this lands in
    // its log.
    eprintln!("dj-mcp: session {session}, daemon {host}:{port} (connected on demand)");

    let mut dj = wire::Dj::new(host, port);
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            // A read error on stdin is the client going away; there is nothing left
            // to serve.
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(resp) = handle_line(&mut dj, &origin, &line) {
            // A write failure means the client's pipe is gone - the same end as EOF.
            if writeln!(out, "{resp}").is_err() || out.flush().is_err() {
                break;
            }
        }
    }
}

/// Parse `--host` / `--port`. Anything else is an error rather than a silent
/// ignore: a mistyped flag that meant a different daemon must not quietly point at
/// the live one.
fn parse_args(args: &[String]) -> Result<(Option<String>, Option<u16>), String> {
    let mut host = None;
    let mut port = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--host" => host = Some(it.next().ok_or("--host needs a value")?.clone()),
            "--port" => {
                let v = it.next().ok_or("--port needs a value")?;
                port = Some(v.parse::<u16>().map_err(|_| format!("bad port: {v}"))?);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok((host, port))
}

/// Serve one request line. Returns the response line, or `None` for a notification
/// (which is answered with silence - a response carrying no id is a protocol fault).
///
/// Every failure path here returns a response. This function is the whole loop body,
/// so anything that returned early without one would hang the client until its call
/// timeout.
fn handle_line(dj: &mut wire::Dj, origin: &str, line: &str) -> Option<String> {
    let req = match rpc::parse_request(line) {
        Ok(r) => r,
        // The id could not be recovered, so the error carries a null one, which is
        // what JSON-RPC reserves it for.
        Err((code, msg)) => return Some(rpc::error(Value::Null, code, &msg).to_string()),
    };
    let result = match req.method.as_str() {
        "initialize" => Ok(rpc::initialize_result(env!("CARGO_PKG_VERSION"))),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools::catalog()})),
        "tools/call" => {
            let name = req.params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = req.params.get("arguments").cloned().unwrap_or(Value::Null);
            Ok(tools::call(dj, origin, name, &args))
        }
        // A notification (no id) is answered with silence; `notifications/initialized`
        // is the one that actually arrives.
        m if m.starts_with("notifications/") => return None,
        other => Err(format!("no such method: {other}")),
    };
    let id = req.id?;
    Some(match result {
        Ok(v) => rpc::response(id, v).to_string(),
        Err(msg) => rpc::error(id, rpc::METHOD_NOT_FOUND, &msg).to_string(),
    })
}

/// Mint `sess-<8 hex>`: the tag that makes every effect this process causes
/// attributable, and that `dj_undo` matches the journal against so one agent cannot
/// retract another's (or a human's) action.
///
/// Kernel entropy when it is there, a time+pid mix when it is not. The fallback is
/// not cryptographic and does not need to be - a session id is an attribution label,
/// not a capability, and the socket it rides has no authentication at all.
fn mint_session() -> String {
    let mut b = [0u8; 4];
    let from_kernel = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_ok();
    if !from_kernel {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let mut x = nanos ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        // One splitmix64 round: enough to keep two servers started in the same
        // millisecond from colliding on the low bits.
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;
        b.copy_from_slice(&(x as u32).to_le_bytes());
    }
    format!("sess-{:02x}{:02x}{:02x}{:02x}", b[0], b[1], b[2], b[3])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn serve(dj: &mut wire::Dj, line: &str) -> Option<Value> {
        handle_line(dj, "mcp:sess-00000000", line)
            .map(|s| serde_json::from_str(&s).expect("a response is always valid JSON"))
    }

    fn down() -> wire::Dj {
        // Port 0 is unconnectable, which is the point: the server must serve the
        // whole handshake with no daemon anywhere.
        wire::Dj::new("127.0.0.1".into(), 0)
    }

    #[test]
    fn the_handshake_works_with_no_daemon_reachable_at_all() {
        let mut dj = down();
        let init = serve(&mut dj, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .expect("initialize is answered");
        assert_eq!(init["id"], json!(1));
        assert_eq!(init["result"]["protocolVersion"], json!(rpc::PROTOCOL_VERSION));
        assert_eq!(init["result"]["serverInfo"]["name"], json!("dj-mcp"));
        assert!(init["result"]["capabilities"]["tools"].is_object());
        // The notification is answered with silence, not with an empty result.
        assert!(serve(&mut dj, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
        let list = serve(&mut dj, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).unwrap();
        assert_eq!(list["result"]["tools"].as_array().map(|a| a.len()), Some(13));
    }

    #[test]
    fn a_tool_call_against_a_dead_daemon_answers_rather_than_hanging_or_exiting() {
        let mut dj = down();
        let r = serve(
            &mut dj,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"dj_now","arguments":{}}}"#,
        )
        .unwrap();
        // A RESULT with isError, never a JSON-RPC error: harn renders the latter as
        // "mcp server dj terminated", which is both false and unactionable.
        assert!(r.get("error").is_none(), "{r}");
        assert_eq!(r["result"]["isError"], json!(true));
        assert!(r["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("not reachable"));
        // And the next call still works - the server latched nothing.
        assert!(serve(&mut dj, r#"{"jsonrpc":"2.0","id":4,"method":"ping"}"#).is_some());
    }

    #[test]
    fn an_unknown_method_is_a_jsonrpc_error_and_a_broken_line_carries_a_null_id() {
        let mut dj = down();
        let r = serve(&mut dj, r#"{"jsonrpc":"2.0","id":5,"method":"resources/list"}"#).unwrap();
        assert_eq!(r["error"]["code"], json!(rpc::METHOD_NOT_FOUND));
        let r = serve(&mut dj, "{ this is not json").unwrap();
        assert_eq!(r["id"], Value::Null);
        assert_eq!(r["error"]["code"], json!(rpc::PARSE_ERROR));
        // An unknown NOTIFICATION stays silent (it has no id to answer to).
        assert!(serve(&mut dj, r#"{"jsonrpc":"2.0","method":"notifications/cancelled"}"#).is_none());
    }

    #[test]
    fn a_session_is_a_bare_word_so_it_can_never_split_a_plan_line() {
        let s = mint_session();
        assert!(s.starts_with("sess-"), "{s}");
        assert_eq!(s.len(), 13, "sess- plus 8 hex: {s}");
        assert!(s[5..].chars().all(|c| c.is_ascii_hexdigit()), "{s}");
        // The origin token is emitted unquoted, so it must survive the daemon's
        // tokenizer as exactly one token.
        let origin = format!("mcp:{s}");
        assert_eq!(hypodj_dsl::dsl_value(&origin).as_deref(), Some(origin.as_str()));
        assert_eq!(hypodj_dsl::tokenize(&origin), vec![origin.clone()]);
        // Two mints in the same process differ (the entropy is not per-process).
        assert_ne!(mint_session(), mint_session(), "ids collide");
    }

    #[test]
    fn a_mistyped_flag_is_an_error_not_a_silent_point_at_the_live_daemon() {
        assert!(parse_args(&["--prot".into(), "6699".into()]).is_err());
        assert!(parse_args(&["--port".into()]).is_err());
        assert!(parse_args(&["--port".into(), "nope".into()]).is_err());
        assert_eq!(
            parse_args(&["--host".into(), "h".into(), "--port".into(), "6699".into()]).unwrap(),
            (Some("h".to_string()), Some(6699))
        );
    }
}
