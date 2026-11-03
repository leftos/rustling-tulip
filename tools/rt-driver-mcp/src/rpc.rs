//! MCP's JSON-RPC 2.0 over stdio: one frame per line. A request (a frame
//! with an `id`) gets exactly one response; a notification gets none.

use std::path::PathBuf;

use serde_json::{Map, Value, json};

use crate::tools::{self, Tools};

/// The text is not JSON.
const PARSE_ERROR: i64 = -32700;
/// The frame is not a request.
const INVALID_REQUEST: i64 = -32600;
/// No such method.
const METHOD_NOT_FOUND: i64 = -32601;
/// The params do not fit the method (an unknown tool, a missing name).
const INVALID_PARAMS: i64 = -32602;

/// The protocol version answered when `initialize` names none.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

/// The version every `initialize` reports: the crate version, with the
/// commit `install.ps1` built it from when it set `RT_DRIVER_MCP_SHA`.
#[must_use]
pub fn server_version() -> String {
    let version = env!("CARGO_PKG_VERSION");
    match option_env!("RT_DRIVER_MCP_SHA") {
        Some(sha) if !sha.is_empty() => format!("{version}+{sha}"),
        _ => version.to_owned(),
    }
}

/// A JSON-RPC error: its code and message.
type RpcError = (i64, String);

/// The server's state across frames.
pub struct Server {
    tools: Tools,
}

impl Server {
    #[must_use]
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            tools: Tools::new(repo_root),
        }
    }

    /// The response to one stdin line, or `None` for a notification.
    pub fn handle_line(&mut self, line: &str) -> Option<Value> {
        let frame: Value = match serde_json::from_str(line) {
            Ok(frame) => frame,
            Err(err) => {
                return Some(error_response(
                    &Value::Null,
                    PARSE_ERROR,
                    &format!("not JSON: {err}"),
                ));
            }
        };
        let id = frame.get("id").cloned();
        let Some(method) = frame.get("method").and_then(Value::as_str) else {
            return id
                .map(|id| error_response(&id, INVALID_REQUEST, "a request needs a string method"));
        };
        let Some(id) = id else {
            tracing::debug!(method, "notification");
            return None;
        };
        let params = frame.get("params").cloned().unwrap_or(Value::Null);
        Some(match self.request(method, &params) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => error_response(&id, code, &message),
        })
    }

    /// Stops every session.
    pub fn stop_all(&mut self) {
        self.tools.stop_all();
    }

    fn request(&mut self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "initialize" => Ok(initialize(params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools::list()})),
            "tools/call" => self.tools_call(params),
            other => Err((METHOD_NOT_FOUND, format!("method not found: {other}"))),
        }
    }

    fn tools_call(&mut self, params: &Value) -> Result<Value, RpcError> {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Err((INVALID_PARAMS, "tools/call needs a string name".to_owned()));
        };
        let empty = Map::new();
        let args = match params.get("arguments") {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(args)) => args,
            Some(other) => {
                return Err((
                    INVALID_PARAMS,
                    format!("arguments takes an object, not {other}"),
                ));
            }
        };
        tracing::info!(tool = name, "tools/call");
        self.tools
            .call(name, args)
            .map(tools::ToolResult::into_value)
            .ok_or_else(|| {
                (
                    INVALID_PARAMS,
                    format!("unknown tool: {name}; tools: launch, stop, call, batch, screenshot"),
                )
            })
    }
}

/// `initialize`'s result, echoing the client's protocol version.
fn initialize(params: &Value) -> Value {
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "rt-driver-mcp", "version": server_version()},
    })
}

fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}
