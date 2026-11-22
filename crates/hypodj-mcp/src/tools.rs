//! The tool surface: five reads, eight writes, and the four rules that make the
//! surface the whole of the agent's reach.
//!
//! 1. **`deny_unknown_fields` on EVERY args struct.** harn forwards the model's
//!    arguments to `tools/call` verbatim; it does not validate them against the
//!    `inputSchema` it advertised (`function_schema` only passes the schema along as
//!    advice). So `additionalProperties: false` is decoration, and the only thing
//!    that actually rejects an invented key is this attribute. It matters twice: an
//!    extra key is what pads a compact-JSON approval preview past the point where a
//!    human still reads it, and it is what would reach harn's `ApprovalPolicy`
//!    fall-through to `args["path"]`.
//! 2. **No control character reaches the wire.** `MpdConn::command` refuses one now,
//!    but the check lives here too so the model gets a sentence it can act on
//!    instead of a transport error.
//! 3. **Every DSL value goes through [`hypodj_dsl::dsl_value`].** Never a `format!`
//!    of a raw string: the daemon's `match`/`query` selectors take exactly ONE
//!    token, so an unquoted `good vibes` selects on `good` and acts on the wrong
//!    rows.
//! 4. **Every write is `plan add trigger immediate action <...> origin mcp:<sess>`.**
//!    The trigger is always immediate (a deferred plan fires with nobody watching
//!    and nothing in the preview says when), and the origin is appended LAST so the
//!    daemon's own modifier stripper reads it as the origin rather than as part of
//!    the action.
//!
//! The one exemption, stated rather than hidden: `dj_transport {"do":"resume"}`
//! sends the wire verb `pause 0`. `plan::Action` has no resume, and `Play{Current}`
//! reloads the track from zero, which is not what a human means by "unpause". It is
//! ungated and unjournaled; it destroys nothing and its inverse is one keypress.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::rpc::tool_text;
use crate::wire::{blocks, find, parse_journal, Dj};

/// The longest string argument accepted, matching the advertised `maxLength`. A
/// query longer than this is a paste, not a search.
const MAX_STR: usize = 80;

// ── argument structs ────────────────────────────────────────────────────────────
// Every one carries `deny_unknown_fields` (rule 1). The numeric fields are `i64`
// rather than their natural narrow types so an out-of-range value produces OUR
// sentence ("count must be 1..=10") instead of serde's "invalid value: integer `40`,
// expected u8", which tells a model nothing about the real bound.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueArgs {
    #[serde(default)]
    offset: i64,
    #[serde(default = "twenty")]
    limit: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    #[serde(default = "ten")]
    limit: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HeardArgs {
    #[serde(default = "twenty")]
    limit: i64,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Query,
    Genre,
    Radio,
    SimilarCurrent,
}

impl Default for Kind {
    fn default() -> Self {
        Kind::Query
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddArgs {
    #[serde(default)]
    what: Option<String>,
    #[serde(default)]
    kind: Kind,
    count: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QidArgs {
    qid: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VolumeArgs {
    to: i64,
    #[serde(default = "eight")]
    over_seconds: i64,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum Transport {
    Pause,
    Stop,
    Resume,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TransportArgs {
    // `do` is a Rust keyword; the JSON key is what the schema advertises.
    #[serde(rename = "do")]
    act: Transport,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum Scope {
    All,
    AfterCurrent,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClearArgs {
    scope: Scope,
}

fn eight() -> i64 {
    8
}
fn ten() -> i64 {
    10
}
fn twenty() -> i64 {
    20
}

// ── the catalog ─────────────────────────────────────────────────────────────────

/// Every tool, in the order a model should discover them: look first, then act.
///
/// `readOnlyHint` is HONEST - it is true only for tools that issue no write verb at
/// all - because harn's `auto_approve_read_only` trusts it and a false hint would
/// auto-run a mutation with nobody in the loop.
pub fn catalog() -> Vec<Value> {
    let s = |props: Value, required: Value| {
        json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
    };
    let read = |name: &str, desc: &str, schema: Value| {
        json!({"name": name, "description": desc, "inputSchema": schema,
               "annotations": {"readOnlyHint": true}})
    };
    let write = |name: &str, desc: &str, schema: Value| {
        json!({"name": name, "description": desc, "inputSchema": schema,
               "annotations": {"readOnlyHint": false}})
    };
    let what_props = |count_default: i64, count_desc: &str| {
        json!({
            "what": {"type": "string", "maxLength": MAX_STR,
                     "description": "the words to select on - a mood, an artist, a genre. \
                                     Required for kind=query and kind=genre; must be OMITTED \
                                     for kind=radio and kind=similar_current."},
            "kind": {"type": "string", "enum": ["query", "genre", "radio", "similar_current"],
                     "default": "query",
                     "description": "query = free text over the library; genre = a genre tag; \
                                     radio = keep the current seed going; similar_current = more \
                                     like what is playing"},
            "count": {"type": "integer", "minimum": 1, "maximum": 10, "default": count_default,
                      "description": count_desc},
        })
    };
    vec![
        read(
            "dj_now",
            "What is playing right now, plus whether there is an agent action you could still \
             retract. Look here before answering any question about the music.",
            s(json!({}), json!([])),
        ),
        read(
            "dj_queue",
            "The queue. Each row carries a `qid` - the STABLE queue id. Every tool that names a \
             row takes that qid and never a position: the daemon trims played rows off the front \
             and tops the queue up on its own, so a position read now is a different song later. \
             Never guess a qid; read it here first.",
            s(
                json!({
                    "offset": {"type": "integer", "minimum": 0, "default": 0},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 20},
                }),
                json!([]),
            ),
        ),
        read(
            "dj_search",
            "Search the library. Returns matching songs; it does NOT queue anything. Use \
             dj_enqueue to actually add music - it selects server-side and is usually better \
             than picking rows by hand.",
            s(
                json!({
                    "query": {"type": "string", "maxLength": MAX_STR},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 20, "default": 10},
                }),
                json!(["query"]),
            ),
        ),
        read(
            "dj_heard",
            "The listening ledger, as the daemon renders it: what was heard recently and what \
             was marked. Lines of prose, newest session first.",
            s(
                json!({"limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 20}}),
                json!([]),
            ),
        ),
        read(
            "dj_journal",
            "The undo ring: the last actions the daemon executed, newest first, each with the \
             daemon's OWN description of what it did and whether it can still be retracted.",
            s(json!({}), json!([])),
        ),
        write(
            "dj_enqueue",
            "Add music to the end of the queue without interrupting what is playing. The \
             default way to put music on.",
            s(what_props(3, "how many tracks to add"), json!([])),
        ),
        write(
            "dj_play_now",
            "Add music AND start playing it immediately, interrupting the current track. Use \
             dj_enqueue unless the human asked for something right now.",
            s(what_props(1, "how many tracks to add"), json!([])),
        ),
        write(
            "dj_jump",
            "Jump to a row already in the queue and play it. The qid comes from dj_queue.",
            s(json!({"qid": {"type": "integer", "minimum": 0}}), json!(["qid"])),
        ),
        write(
            "dj_volume",
            "Glide the volume to a level over some seconds. A fade, not a jump - 0 seconds is \
             allowed but abrupt.",
            s(
                json!({
                    "to": {"type": "integer", "minimum": 0, "maximum": 100},
                    "over_seconds": {"type": "integer", "minimum": 0, "maximum": 300, "default": 8},
                }),
                json!(["to"]),
            ),
        ),
        write(
            "dj_transport",
            "Pause, stop, or resume playback. Resume picks up where a pause left off.",
            s(
                json!({"do": {"type": "string", "enum": ["pause", "stop", "resume"]}}),
                json!(["do"]),
            ),
        ),
        write(
            "dj_undo",
            "Retract the newest still-undoable action, but ONLY if this session caused it. Call \
             this the moment what came back disagrees with what was asked - and say so.",
            s(json!({}), json!([])),
        ),
        write(
            "dj_remove",
            "Remove one row from the queue by its qid, from dj_queue. One row per call.",
            s(json!({"qid": {"type": "integer", "minimum": 0}}), json!(["qid"])),
        ),
        write(
            "dj_clear",
            "Empty the queue: `all` wipes it, `after_current` keeps what is playing and drops \
             the rest. Destructive - be sure this is what was asked.",
            s(
                json!({"scope": {"type": "string", "enum": ["all", "after_current"]}}),
                json!(["scope"]),
            ),
        ),
    ]
}

// ── dispatch ────────────────────────────────────────────────────────────────────

/// Run one `tools/call`. Returns the RESULT object (with `isError`), never a
/// JSON-RPC error - see [`crate::rpc`] for why that distinction is load-bearing.
///
/// `origin` is the full `mcp:sess-xxxxxxxx` string; it is written into every plan
/// and is what `dj_undo` matches the journal against.
pub fn call(dj: &mut Dj, origin: &str, name: &str, args: &Value) -> Value {
    // A missing / null `arguments` is an empty object, which still has to pass
    // `deny_unknown_fields` - so a tool with required fields still fails loudly.
    let args = match args {
        Value::Null => json!({}),
        v => v.clone(),
    };
    match run(dj, origin, name, args) {
        Ok(v) => tool_text(v.to_string(), false),
        Err(msg) => tool_text(msg, true),
    }
}

fn run(dj: &mut Dj, origin: &str, name: &str, args: Value) -> Result<Value, String> {
    match name {
        "dj_now" => {
            decode::<NoArgs>(args)?;
            now(dj)
        }
        "dj_queue" => {
            let a = decode::<QueueArgs>(args)?;
            let offset = bounded("offset", a.offset, 0, i64::MAX)? as usize;
            let limit = bounded("limit", a.limit, 1, 50)? as usize;
            queue(dj, offset, limit)
        }
        "dj_search" => {
            let a = decode::<SearchArgs>(args)?;
            let q = text("query", &a.query)?;
            let limit = bounded("limit", a.limit, 1, 20)? as usize;
            search(dj, &q, limit)
        }
        "dj_heard" => {
            let a = decode::<HeardArgs>(args)?;
            let limit = bounded("limit", a.limit, 1, 50)?;
            heard(dj, limit)
        }
        "dj_journal" => {
            decode::<NoArgs>(args)?;
            journal(dj)
        }
        "dj_enqueue" => {
            let a = decode::<AddArgs>(args)?;
            add(dj, origin, "enqueue", a, 3)
        }
        "dj_play_now" => {
            let a = decode::<AddArgs>(args)?;
            add(dj, origin, "playnow", a, 1)
        }
        "dj_jump" => {
            let a = decode::<QidArgs>(args)?;
            let qid = bounded("qid", a.qid, 0, i64::MAX)?;
            write_plan(dj, origin, &format!("play id {qid}"))
        }
        "dj_volume" => {
            let a = decode::<VolumeArgs>(args)?;
            let to = bounded("to", a.to, 0, 100)?;
            let over = bounded("over_seconds", a.over_seconds, 0, 300)?;
            write_plan(dj, origin, &format!("fade to {to} {over}"))
        }
        "dj_transport" => {
            let a = decode::<TransportArgs>(args)?;
            match a.act {
                Transport::Pause => write_plan(dj, origin, "pause"),
                Transport::Stop => write_plan(dj, origin, "stop"),
                Transport::Resume => resume(dj),
            }
        }
        "dj_undo" => {
            decode::<NoArgs>(args)?;
            undo(dj, origin)
        }
        "dj_remove" => {
            let a = decode::<QidArgs>(args)?;
            let qid = bounded("qid", a.qid, 0, i64::MAX)?;
            write_plan(dj, origin, &format!("remove id {qid}"))
        }
        "dj_clear" => {
            let a = decode::<ClearArgs>(args)?;
            let scope = match a.scope {
                Scope::All => "all",
                Scope::AfterCurrent => "after",
            };
            write_plan(dj, origin, &format!("clear {scope}"))
        }
        other => Err(format!("no such tool: {other}")),
    }
}

/// Deserialize the arguments, turning serde's message into the model's whole
/// explanation. `deny_unknown_fields` is what makes an invented key land here.
fn decode<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("bad arguments: {e}"))
}

/// Enforce a numeric bound HERE, because the advertised `minimum`/`maximum` is only
/// advice: harn hands the model's arguments over without checking them.
fn bounded(label: &str, v: i64, lo: i64, hi: i64) -> Result<i64, String> {
    if v < lo || v > hi {
        if hi == i64::MAX {
            return Err(format!("{label} must be at least {lo} (got {v})"));
        }
        return Err(format!("{label} must be {lo}..={hi} (got {v})"));
    }
    Ok(v)
}

/// Rule 2: no control character reaches the wire, and no oversized paste either.
/// `dsl_value` would refuse the control char anyway; this runs first so the model
/// reads WHY instead of a transport-shaped error.
fn text(label: &str, s: &str) -> Result<String, String> {
    if s.chars().any(char::is_control) {
        return Err(format!(
            "{label} contains a control character (a newline, tab or similar). \
             Values are single-line text; nothing was sent."
        ));
    }
    if s.chars().count() > MAX_STR {
        return Err(format!("{label} is longer than {MAX_STR} characters"));
    }
    Ok(s.to_string())
}

/// Rule 3: the ONE way a caller-supplied value becomes a DSL token.
fn value(label: &str, s: &str) -> Result<String, String> {
    let s = text(label, s)?;
    hypodj_dsl::dsl_value(&s)
        .ok_or_else(|| format!("{label} cannot be expressed as a plan value; nothing was sent"))
}

// ── reads ───────────────────────────────────────────────────────────────────────

fn now(dj: &mut Dj) -> Result<Value, String> {
    let status = dj.read("status").map_err(|e| e.message())?;
    let current = dj.read("currentsong").map_err(|e| e.message())?;
    // A journal read is cheap (an in-memory ring) and it is what turns "I did
    // something wrong" into an action the model can actually take.
    let undo_available = dj
        .read("journal")
        .map(|p| parse_journal(&p).iter().any(|e| e.undoable))
        .unwrap_or(false);
    Ok(json!({
        "state": find(&status, "state"),
        "artist": find(&current, "Artist"),
        "title": find(&current, "Title"),
        "album": find(&current, "Album"),
        "qid": find(&status, "songid").and_then(|v| v.parse::<u64>().ok()),
        // hypodj's `status` carries no `elapsed` pair (it is not emitted at all), so
        // this is null rather than a fabricated number. Stated, not silently omitted.
        "elapsed": Value::Null,
        "duration": find(&status, "duration").and_then(|v| v.parse::<f64>().ok()),
        "volume": find(&status, "volume").and_then(|v| v.parse::<i64>().ok()),
        "queue_len": queue_len(&status),
        "undo_available": undo_available,
    }))
}

fn queue(dj: &mut Dj, offset: usize, limit: usize) -> Result<Value, String> {
    let pairs = dj.read("playlistinfo").map_err(|e| e.message())?;
    let rows = blocks(&pairs, "file");
    let total = rows.len();
    let items: Vec<Value> = rows
        .iter()
        .skip(offset)
        .take(limit)
        .enumerate()
        .map(|(i, b)| {
            json!({
                "pos": find(b, "Pos").and_then(|v| v.parse::<usize>().ok()).unwrap_or(offset + i),
                "qid": find(b, "Id").and_then(|v| v.parse::<u64>().ok()),
                "artist": find(b, "Artist"),
                "title": find(b, "Title"),
                "album": find(b, "Album"),
                "secs": find(b, "Time").and_then(|v| v.parse::<u64>().ok()),
            })
        })
        .collect();
    Ok(json!({"total": total, "items": items}))
}

fn search(dj: &mut Dj, query: &str, limit: usize) -> Result<Value, String> {
    let line = format!("search any {}", value("query", query)?);
    let pairs = dj.read(&line).map_err(|e| e.message())?;
    let hits: Vec<Value> = blocks(&pairs, "file")
        .iter()
        .take(limit)
        .map(|b| {
            json!({
                "artist": find(b, "Artist"),
                "title": find(b, "Title"),
                "album": find(b, "Album"),
            })
        })
        .collect();
    Ok(json!({"hits": hits}))
}

fn heard(dj: &mut Dj, limit: i64) -> Result<Value, String> {
    let pairs = dj.read(&format!("heard limit {limit}")).map_err(|e| e.message())?;
    // The daemon renders the ledger itself, as prose lines under a `heard` key -
    // there is no structured play list on the wire, so this passes the daemon's own
    // words through rather than inventing fields it would have to guess at.
    let lines: Vec<&str> = pairs
        .iter()
        .filter(|(k, _)| k == "heard")
        .map(|(_, v)| v.as_str())
        .collect();
    Ok(json!({"lines": lines}))
}

fn journal(dj: &mut Dj) -> Result<Value, String> {
    let pairs = dj.read("journal").map_err(|e| e.message())?;
    let entries: Vec<Value> = parse_journal(&pairs)
        .into_iter()
        .map(|e| {
            json!({"id": e.id, "age_s": e.age_s, "origin": e.origin,
                   "act": e.act, "did": e.did, "undoable": e.undoable})
        })
        .collect();
    Ok(json!({"entries": entries}))
}

// ── writes ──────────────────────────────────────────────────────────────────────

fn add(dj: &mut Dj, origin: &str, verb: &str, a: AddArgs, default_count: i64) -> Result<Value, String> {
    let count = bounded("count", a.count.unwrap_or(default_count), 1, 10)?;
    // `what` is required for the two selectors that take a value and FORBIDDEN for
    // the two that do not. Forbidden rather than ignored: a model that passes
    // `{"kind":"radio","what":"jazz"}` believes it asked for jazz radio, and
    // silently dropping the word would make the read-back the only place the truth
    // appears - after the fact.
    let tail = match a.kind {
        Kind::Query | Kind::Genre => {
            let what = a.what.as_deref().ok_or_else(|| {
                "what is required for kind=query and kind=genre".to_string()
            })?;
            let sel = if a.kind == Kind::Query { "query" } else { "genre" };
            format!("{verb} {sel} {} {count}", value("what", what)?)
        }
        Kind::Radio | Kind::SimilarCurrent => {
            if a.what.is_some() {
                return Err(
                    "what must be omitted for kind=radio and kind=similar_current \
                     (they take their seed from what is playing)"
                        .to_string(),
                );
            }
            let sel = if matches!(a.kind, Kind::Radio) { "radio" } else { "similar_current" };
            format!("{verb} {sel} {count}")
        }
    };
    write_plan(dj, origin, &tail)
}

/// Rule 4, and the read-back that makes `did` trustworthy.
///
/// The journal is sampled BEFORE the write so the entry claimed afterwards must be
/// NEWER than everything that existed - otherwise an action that changed nothing
/// (and so took no ring slot) would report the previous action's sentence as its
/// own, which is exactly the drift the whole design exists to prevent.
fn write_plan(dj: &mut Dj, origin: &str, action: &str) -> Result<Value, String> {
    let before = top_jid(dj);
    let line = format!("plan add trigger immediate action {action} origin {origin}");
    let pairs = match dj.write(&line) {
        Ok(p) => p,
        Err(e) if e.is_unknown() => {
            return Ok(json!({"ok": false, "outcome": "unknown", "hint": e.message()}))
        }
        Err(e) => return Err(e.message()),
    };
    // `result` is the daemon's own `PlanOutcome::render()` for THIS action (the
    // handler puts it there), so it is authoritative in the same way `jdid` is - it
    // is the fallback for an action that changed nothing and took no ring slot.
    let reported = find(&pairs, "result").unwrap_or("").to_string();
    let (jid, did, undoable) = match dj.read("journal").map(|p| parse_journal(&p)) {
        Ok(entries) => match entries
            .into_iter()
            .find(|e| e.origin == origin && Some(e.id) > before)
        {
            Some(e) => (Some(e.id), e.did, e.undoable),
            None => (None, reported, false),
        },
        Err(_) => (None, reported, false),
    };
    Ok(json!({
        "ok": true,
        "did": did,
        "jid": jid,
        "undoable": undoable,
        "queue_len": queue_len_now(dj),
    }))
}

/// The single wire verb this server emits. See the module note.
fn resume(dj: &mut Dj) -> Result<Value, String> {
    match dj.write("pause 0") {
        Ok(_) => {}
        Err(e) if e.is_unknown() => {
            return Ok(json!({"ok": false, "outcome": "unknown", "hint": e.message()}))
        }
        Err(e) => return Err(e.message()),
    }
    Ok(json!({
        "ok": true,
        "did": "resumed",
        "jid": Value::Null,
        // Truthfully false: resume is not a plan Action, so nothing journaled it.
        // Its inverse is dj_transport {"do":"pause"}.
        "undoable": false,
        "queue_len": queue_len_now(dj),
    }))
}

/// Retract this session's newest action - and refuse to retract anyone else's.
///
/// The check is on the TOP undoable entry rather than on "the newest of mine",
/// because the ring is a stack: retracting a buried entry would mean discarding
/// everything above it, including a human's own action.
fn undo(dj: &mut Dj, origin: &str) -> Result<Value, String> {
    let entries = dj.read("journal").map(|p| parse_journal(&p)).map_err(|e| e.message())?;
    let top = entries
        .iter()
        .find(|e| e.undoable)
        .ok_or_else(|| "there is nothing left to undo".to_string())?;
    if top.origin != origin {
        return Err(format!(
            "the newest undoable action is not this session's - it was {}'s ({}). \
             dj_undo only retracts what you did; tell the human what you see instead.",
            top.origin, top.did
        ));
    }
    let id = top.id;
    match dj.write(&format!("undo {id}")) {
        Ok(_) => {}
        Err(e) if e.is_unknown() => {
            return Ok(json!({"ok": false, "outcome": "unknown", "hint": e.message()}))
        }
        Err(e) => return Err(e.message()),
    }
    // The daemon journals the retraction itself, so its own sentence for what came
    // back is read out of the ring rather than composed here.
    let did = dj
        .read("journal")
        .map(|p| parse_journal(&p))
        .ok()
        .and_then(|e| e.into_iter().next())
        .map(|e| e.did)
        .unwrap_or_else(|| format!("undone: {}", top.did));
    Ok(json!({
        "ok": true,
        "did": did,
        "jid": id,
        // An undo is journaled but not itself undoable - a redo is not promised.
        "undoable": false,
        "queue_len": queue_len_now(dj),
    }))
}

/// The newest journal id, or `None` when the ring is empty or unreadable. A `None`
/// makes the `Some(e.id) > before` test above accept any entry, which is right: an
/// empty ring before the write means anything in it after is new.
fn top_jid(dj: &mut Dj) -> Option<u64> {
    dj.read("journal").ok().and_then(|p| parse_journal(&p).first().map(|e| e.id))
}

fn queue_len(status: &[(String, String)]) -> Option<u64> {
    find(status, "playlistlength").and_then(|v| v.parse().ok())
}

fn queue_len_now(dj: &mut Dj) -> Option<u64> {
    dj.read("status").ok().and_then(|s| queue_len(&s))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_err<T: serde::de::DeserializeOwned>(v: Value) -> String {
        decode::<T>(v).err().expect("should have been rejected")
    }

    // RULE 1. An invented key must FAIL the call, on every args struct - including
    // the ones whose schema is `{}`, which are otherwise the easiest to slip a key
    // past. This is the truncation-spoof carrier and the ApprovalPolicy poison
    // vector, and harn does not check it for us.
    #[test]
    fn every_args_struct_refuses_an_unknown_key() {
        assert!(decode_err::<NoArgs>(json!({"path": "/"})).contains("unknown field `path`"));
        assert!(decode_err::<QueueArgs>(json!({"limit": 5, "path": "/"})).contains("path"));
        assert!(decode_err::<SearchArgs>(json!({"query": "a", "command": "rm"})).contains("command"));
        assert!(decode_err::<HeardArgs>(json!({"x": 1})).contains("unknown field"));
        assert!(decode_err::<AddArgs>(json!({"what": "a", "path": "/"})).contains("path"));
        assert!(decode_err::<QidArgs>(json!({"qid": 3, "pos": 1})).contains("pos"));
        assert!(decode_err::<VolumeArgs>(json!({"to": 40, "db": -8})).contains("db"));
        assert!(decode_err::<TransportArgs>(json!({"do": "pause", "force": true})).contains("force"));
        assert!(decode_err::<ClearArgs>(json!({"scope": "all", "path": "/"})).contains("path"));
    }

    // The same rule through the FULL dispatch, with a Dj that has no daemon behind
    // it: the rejection happens before any connect is attempted, so a bad key can
    // never be the thing that opens a socket.
    #[test]
    fn dispatch_rejects_an_unknown_key_before_touching_the_wire() {
        // Port 0 cannot be connected to, so a call that reached the wire would say
        // "not reachable" instead of naming the field.
        let mut dj = Dj::new("127.0.0.1".into(), 0);
        let out = call(&mut dj, "mcp:sess-00000000", "dj_clear", &json!({"path": "/", "scope": "all"}));
        assert_eq!(out["isError"], json!(true));
        let text = out["content"][0]["text"].as_str().unwrap_or("");
        assert!(text.contains("unknown field `path`"), "{text}");
        assert!(!text.contains("not reachable"), "nothing was sent: {text}");
    }

    // RULE 2. A control character in ANY string argument is refused with a sentence,
    // before the wire. The classic payload is a newline that would frame a second
    // command line onto the socket.
    #[test]
    fn a_control_character_is_refused_and_nothing_is_sent() {
        let mut dj = Dj::new("127.0.0.1".into(), 0);
        for (tool, args) in [
            ("dj_enqueue", json!({"what": "a\nsetvol 100"})),
            ("dj_play_now", json!({"what": "a\rclear"})),
            ("dj_search", json!({"query": "tab\there"})),
        ] {
            let out = call(&mut dj, "mcp:sess-00000000", tool, &args);
            assert_eq!(out["isError"], json!(true), "{tool}");
            let text = out["content"][0]["text"].as_str().unwrap_or("");
            assert!(text.contains("control character"), "{tool}: {text}");
            assert!(text.contains("nothing was sent"), "{tool}: {text}");
        }
        // And the low-level guard agrees, so the rule holds even if a future caller
        // forgets the pre-check.
        assert!(hypodj_dsl::dsl_value("a\nsetvol 100").is_none());
    }

    // RULE 3. A multi-word value survives as ONE token. Unquoted, the daemon's
    // selector parser would take `good` and act on the wrong rows.
    #[test]
    fn a_multi_word_value_is_quoted_into_one_token() {
        let v = value("what", "good vibes").unwrap();
        assert_eq!(v, "\"good vibes\"");
        let line = format!("plan add trigger immediate action enqueue query {v} 3 origin mcp:sess-1");
        let toks = hypodj_dsl::tokenize(&line);
        assert_eq!(
            toks,
            ["plan", "add", "trigger", "immediate", "action", "enqueue", "query", "good vibes",
             "3", "origin", "mcp:sess-1"]
                .map(String::from)
                .to_vec()
        );
        // A quote and a backslash survive too, still as one token.
        let v = value("what", r#"say "hi" \ now"#).unwrap();
        assert_eq!(hypodj_dsl::tokenize(&v), vec![r#"say "hi" \ now"#.to_string()]);
    }

    // RULE 4. Every emitted plan line is `trigger immediate` and ends with the
    // origin - the last two tokens, because the daemon's stripper reads the FIRST
    // `origin` token as the marker and truncates the action there.
    #[test]
    fn every_write_is_immediate_and_ends_with_the_origin() {
        let origin = "mcp:sess-deadbeef";
        for tail in [
            "enqueue query \"good vibes\" 3",
            "playnow radio 1",
            "play id 88",
            "fade to 40 8",
            "pause",
            "stop",
            "remove id 12",
            "clear all",
            "clear after",
        ] {
            let line = format!("plan add trigger immediate action {tail} origin {origin}");
            let toks = hypodj_dsl::tokenize(&line);
            assert_eq!(toks[2], "trigger");
            assert_eq!(toks[3], "immediate");
            assert_eq!(toks[toks.len() - 2], "origin");
            assert_eq!(toks[toks.len() - 1], origin);
            // The origin is a bare word, so it can never need quoting and can never
            // be split into two tokens.
            assert_eq!(hypodj_dsl::dsl_value(origin).as_deref(), Some(origin));
        }
    }

    #[test]
    fn bounds_are_enforced_here_because_the_schema_is_only_advice() {
        assert!(bounded("count", 11, 1, 10).unwrap_err().contains("1..=10"));
        assert!(bounded("count", 0, 1, 10).unwrap_err().contains("1..=10"));
        assert_eq!(bounded("to", 100, 0, 100).unwrap(), 100);
        assert!(bounded("to", 101, 0, 100).unwrap_err().contains("0..=100"));
        assert!(bounded("qid", -1, 0, i64::MAX).unwrap_err().contains("at least 0"));
        assert!(text("query", &"x".repeat(81)).unwrap_err().contains("longer than 80"));
        assert!(text("query", &"x".repeat(80)).is_ok());
    }

    #[test]
    fn a_selector_that_takes_no_value_refuses_one_instead_of_dropping_it() {
        let mut dj = Dj::new("127.0.0.1".into(), 0);
        let out = call(&mut dj, "mcp:sess-1", "dj_enqueue", &json!({"kind": "radio", "what": "jazz"}));
        assert_eq!(out["isError"], json!(true));
        assert!(out["content"][0]["text"].as_str().unwrap_or("").contains("must be omitted"));
        // And the two that DO take one refuse to proceed without it.
        let out = call(&mut dj, "mcp:sess-1", "dj_enqueue", &json!({"kind": "genre"}));
        assert!(out["content"][0]["text"].as_str().unwrap_or("").contains("what is required"));
    }

    #[test]
    fn the_catalog_is_thirteen_tools_with_honest_read_only_hints() {
        let c = catalog();
        assert_eq!(c.len(), 13);
        let hint = |n: &str| {
            c.iter()
                .find(|t| t["name"] == json!(n))
                .unwrap_or_else(|| panic!("{n} missing"))["annotations"]["readOnlyHint"]
                .clone()
        };
        for r in ["dj_now", "dj_queue", "dj_search", "dj_heard", "dj_journal"] {
            assert_eq!(hint(r), json!(true), "{r} issues no write verb");
        }
        for w in [
            "dj_enqueue", "dj_play_now", "dj_jump", "dj_volume", "dj_transport", "dj_undo",
            "dj_remove", "dj_clear",
        ] {
            assert_eq!(hint(w), json!(false), "{w} mutates");
        }
        // Every schema closes itself, even though that is advice harn does not
        // enforce - the enforcement is `deny_unknown_fields`, tested above.
        for t in &c {
            assert_eq!(t["inputSchema"]["additionalProperties"], json!(false), "{}", t["name"]);
            assert_eq!(t["inputSchema"]["type"], json!("object"));
            assert!(t["description"].as_str().is_some_and(|d| !d.is_empty()));
        }
    }

    #[test]
    fn an_unknown_tool_name_is_an_error_result_not_a_panic() {
        let mut dj = Dj::new("127.0.0.1".into(), 0);
        let out = call(&mut dj, "mcp:sess-1", "dj_rm_rf", &json!({}));
        assert_eq!(out["isError"], json!(true));
        assert!(out["content"][0]["text"].as_str().unwrap_or("").contains("no such tool"));
    }

    #[test]
    fn a_missing_arguments_object_is_an_empty_one_and_still_has_to_pass() {
        let mut dj = Dj::new("127.0.0.1".into(), 0);
        // dj_now takes nothing, so null arguments are fine and it reaches the wire
        // (which is down here - that IS the evidence it got past decoding).
        let out = call(&mut dj, "mcp:sess-1", "dj_now", &Value::Null);
        assert_eq!(out["isError"], json!(true));
        assert!(out["content"][0]["text"].as_str().unwrap_or("").contains("not reachable"));
        // dj_clear requires a scope, so null arguments must NOT become a wipe.
        let out = call(&mut dj, "mcp:sess-1", "dj_clear", &Value::Null);
        let text = out["content"][0]["text"].as_str().unwrap_or("");
        assert!(text.contains("missing field `scope`"), "{text}");
        assert!(!text.contains("not reachable"), "nothing was sent: {text}");
    }
}
