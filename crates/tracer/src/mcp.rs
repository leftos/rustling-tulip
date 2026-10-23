//! `rt-tracer mcp`: the stdio MCP server an agent session's injected
//! `--mcp-config` runs.
//!
//! One invocation serves one session: `rt-tracer mcp --session <id>
//! --config-dir <dir>`. It reads newline-delimited JSON-RPC 2.0 messages from
//! stdin and writes one line of JSON per reply to stdout. Its one tool,
//! `show_mock`, POSTs the page to the daemon's `/mock/<session id>` route over
//! the same proved connection the hook shim uses (see [`hook::post_json`]).
//!
//! Like the hook shim, it is dispatched before the tracer builds its tokio
//! runtime and stays plain synchronous std code.

use crate::hook::{self, Args, Outcome};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

/// The budget for one `show_mock` POST: the connect, the proof round trip,
/// the writes and the status line.
const CALL_BUDGET: Duration = Duration::from_secs(10);

/// The protocol version answered when `initialize` names none.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

/// The one tool's name.
const SHOW_MOCK: &str = "show_mock";

/// Runs the `mcp` subcommand on the process's stdin and stdout until stdin
/// closes. Bad arguments write one line to stderr and return.
pub(crate) fn run_from_env() {
    let argv: Vec<OsString> = std::env::args_os().skip(2).collect();
    let Some(args) = Args::parse(&argv) else {
        let _ = writeln!(
            std::io::stderr(),
            "rt-tracer mcp: expected --session <id> --config-dir <dir>"
        );
        return;
    };
    serve(std::io::stdin().lock(), std::io::stdout().lock(), &args);
}

/// Answers every message on `input` until it ends, one reply line each on
/// `output`; a notification gets none.
fn serve(input: impl BufRead, mut output: impl Write, args: &Args) {
    for line in input.lines() {
        let Ok(line) = line else {
            return;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Some(reply) = answer(&line, args) else {
            continue;
        };
        if writeln!(output, "{reply}").is_err() || output.flush().is_err() {
            return;
        }
    }
}

/// The reply to one line, or `None` when it needs none: a notification (no
/// `id`) or a message with no `method` (a response, which this server never
/// asks for).
fn answer(line: &str, args: &Args) -> Option<Value> {
    let Ok(message) = serde_json::from_str::<Value>(line) else {
        return Some(error_reply(&Value::Null, -32700, "Parse error"));
    };
    let id = message.get("id")?;
    let method = message.get("method")?.as_str().unwrap_or_default();
    let params = message.get("params").unwrap_or(&Value::Null);
    Some(match method {
        "initialize" => result_reply(id, initialize_result(params)),
        "ping" => result_reply(id, json!({})),
        "tools/list" => result_reply(id, json!({ "tools": [show_mock_tool()] })),
        "tools/call" => match call_tool(params, args) {
            Ok(result) => result_reply(id, result),
            Err(message) => error_reply(id, -32602, &message),
        },
        _ => error_reply(id, -32601, "Method not found"),
    })
}

fn result_reply(id: &Value, result: Value) -> Value {
    let mut reply = json!({ "jsonrpc": "2.0", "id": id });
    reply["result"] = result;
    reply
}

fn error_reply(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// The `initialize` result, echoing the client's protocol version.
fn initialize_result(params: &Value) -> Value {
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "rustling-tulip", "version": env!("CARGO_PKG_VERSION") },
    })
}

/// The `show_mock` tool's definition.
fn show_mock_tool() -> Value {
    json!({
        "name": SHOW_MOCK,
        "description": "Show the user a visual right inside rustling-tulip, in a panel beside this \
            terminal session: a UI mock, a layout or styling option, a diagram, a chart. Pass one \
            complete, self-contained HTML document (inline <style>, inline <svg>, inline <script>; \
            no external files). Prefer this over describing a visual in text or publishing it \
            elsewhere. Calling it again replaces the panel's page.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "Short title for the panel header" },
                "html": { "type": "string", "description": "A complete HTML document" },
            },
            "required": ["title", "html"],
        },
    })
}

/// Runs a `tools/call`; `Err` is the message of a `-32602` invalid-params
/// error.
fn call_tool(params: &Value, args: &Args) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if name != SHOW_MOCK {
        return Err(format!("unknown tool `{name}`"));
    }
    let arguments = params.get("arguments");
    let field = |key: &str| arguments.and_then(|a| a.get(key)).and_then(Value::as_str);
    let (Some(title), Some(html)) = (field("title"), field("html")) else {
        return Err("show_mock needs string `title` and `html` arguments".to_owned());
    };
    let outcome = post_mock(args, title, html);
    if outcome == Outcome::Posted {
        let file = args
            .config_dir
            .join("sessions")
            .join(&args.session_id)
            .join("mock.html");
        let text = format!(
            "Shown to the user in the panel beside this session. File: {}",
            file.display()
        );
        return Ok(json!({ "content": [{ "type": "text", "text": text }] }));
    }
    let text = format!("The rustling-tulip daemon did not take the mock ({outcome:?}).");
    Ok(json!({ "content": [{ "type": "text", "text": text }], "isError": true }))
}

/// POSTs the page to the daemon that `args.config_dir`'s `daemon.json` names.
fn post_mock(args: &Args, title: &str, html: &str) -> Outcome {
    let deadline = Instant::now() + CALL_BUDGET;
    let Some(daemon) = hook::read_daemon(&args.config_dir) else {
        return Outcome::Skipped;
    };
    let body = json!({ "title": title, "html": html }).to_string();
    hook::post_json(
        &daemon,
        &format!("/mock/{}", args.session_id),
        "",
        body.as_bytes(),
        deadline,
    )
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::path::Path;

    #[test]
    fn answers_initialize_list_and_call_without_a_daemon() {
        // The directory exists but holds no `daemon.json`.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.tmp/mcp-tests")
            .join(format!(
                "no-daemon-{}-{}",
                std::process::id(),
                hook::unix_nanos()
            ));
        std::fs::create_dir_all(&dir).expect("create the scratch dir");
        let argv = [
            OsString::from("--session"),
            OsString::from("0b6f4c7e-1111"),
            OsString::from("--config-dir"),
            dir.clone().into_os_string(),
        ];
        let args = Args::parse(&argv).expect("the arguments parse");
        let input = [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"show_mock","arguments":{"title":"T","html":"<p>hi</p>"}}}"#,
        ]
        .join("\n");
        let mut output = Vec::new();

        serve(Cursor::new(input), &mut output, &args);

        let text = String::from_utf8(output).expect("the replies are UTF-8");
        let replies: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).expect("each reply line is JSON"))
            .collect();
        assert_eq!(replies.len(), 3, "replies: {text}");
        assert_eq!(replies[0]["id"], 1);
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(replies[0]["result"]["serverInfo"]["name"], "rustling-tulip");
        assert_eq!(replies[1]["id"], 2);
        assert_eq!(replies[1]["result"]["tools"][0]["name"], "show_mock");
        assert_eq!(replies[2]["id"], 3);
        assert_eq!(replies[2]["result"]["isError"], true, "replies: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
