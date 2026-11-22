//! The stdio JSON-RPC framing: newline-delimited JSON objects on stdin/stdout, the
//! consume-only MCP subset (`initialize`, `notifications/initialized`, `tools/list`,
//! `tools/call`, `ping`). Hand-rolled over `serde_json` rather than pulled from a
//! crate, because the subset is this small and the dependency budget of an MCP child
//! spawned on every harn start is not.
//!
//! ONE rule the rest of the server leans on: a bad ARGUMENT is never a JSON-RPC
//! error. harn resolves a `-32xxx` response as a transport-level failure and renders
//! it to the model as "mcp server dj terminated" (`src/agent/tools/mcp/client.rs`,
//! the `Ok(Err(_))` arm of `call`), which is both false and unactionable. A rejected
//! argument is a `tools/call` RESULT carrying `isError: true`, which harn flattens
//! into the model's transcript verbatim - so the model reads why and can fix it.

use serde_json::{json, Value};

/// The protocol revision this server reports from `initialize`. harn accepts
/// `2025-06-18`, `2025-03-26` and `2024-11-05` and disconnects on anything else,
/// so this tracks the newest of those.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// One parsed request line. `id` is absent for a notification, which is answered
/// with silence (never an empty result - a response to a notification is a protocol
/// error and some clients treat the stray id as a dangling reply).
#[derive(Debug)]
pub struct Request {
    pub id: Option<Value>,
    pub method: String,
    pub params: Value,
}

/// JSON-RPC error codes, only the ones this server can actually produce.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;

/// Parse one line. On failure the caller answers with a `PARSE_ERROR` /
/// `INVALID_REQUEST` carrying a null id, which is what the spec asks for when the
/// id itself could not be recovered.
pub fn parse_request(line: &str) -> Result<Request, (i64, String)> {
    let v: Value = serde_json::from_str(line).map_err(|e| (PARSE_ERROR, e.to_string()))?;
    let method = v
        .get("method")
        .and_then(|m| m.as_str())
        .ok_or((INVALID_REQUEST, "no method".to_string()))?
        .to_string();
    // A null id is NOT an id: JSON-RPC reserves it for the "could not be determined"
    // case, so treating it as a notification is the reading that cannot dangle.
    let id = v.get("id").filter(|i| !i.is_null()).cloned();
    let params = v.get("params").cloned().unwrap_or_else(|| json!({}));
    Ok(Request { id, method, params })
}

/// A successful response envelope.
pub fn response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// An error response envelope. Reserved for PROTOCOL faults (unparseable line,
/// unknown method) - never for a tool argument the caller got wrong.
pub fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// The `initialize` result. Capabilities are exactly `tools` (no resources, no
/// prompts, no `listChanged` - the catalog is frozen at compile time).
pub fn initialize_result(server_version: &str) -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "dj-mcp", "version": server_version},
    })
}

/// A `tools/call` result carrying one text block. `is_error` rides IN the result
/// (never as a JSON-RPC error) so the model reads the reason.
pub fn tool_text(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notification_carries_no_id_and_a_null_id_is_a_notification_too() {
        let r = parse_request(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap();
        assert_eq!(r.method, "notifications/initialized");
        assert!(r.id.is_none());
        let r = parse_request(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#).unwrap();
        assert!(r.id.is_none(), "a null id is the reserved 'unknown', not an id");
    }

    #[test]
    fn a_request_keeps_its_id_shape_and_defaults_its_params() {
        let r = parse_request(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#).unwrap();
        assert_eq!(r.id, Some(json!(7)));
        assert_eq!(r.params, json!({}));
        // A string id must survive as a string - echoing it back as a number would
        // leave the client's pending map unmatched forever.
        let r = parse_request(r#"{"jsonrpc":"2.0","id":"abc","method":"ping"}"#).unwrap();
        assert_eq!(r.id, Some(json!("abc")));
    }

    #[test]
    fn a_broken_line_and_a_methodless_object_both_fail_loud() {
        assert_eq!(parse_request("{not json").unwrap_err().0, PARSE_ERROR);
        assert_eq!(parse_request(r#"{"jsonrpc":"2.0","id":1}"#).unwrap_err().0, INVALID_REQUEST);
    }

    #[test]
    fn a_tool_error_is_a_result_never_a_jsonrpc_error() {
        let v = tool_text("unknown field `path`".into(), true);
        assert_eq!(v["isError"], json!(true));
        assert_eq!(v["content"][0]["type"], json!("text"));
        assert_eq!(v["content"][0]["text"], json!("unknown field `path`"));
        // The envelope around it is a RESULT, which is what keeps harn from
        // rendering "mcp server dj terminated" over a fixable typo.
        let env = response(json!(1), v);
        assert!(env.get("error").is_none());
        assert_eq!(env["result"]["isError"], json!(true));
    }
}
