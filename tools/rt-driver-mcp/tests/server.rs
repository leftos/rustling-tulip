//! The server as an MCP client starts it: the built exe over stdio, every
//! stdout line a JSON-RPC 2.0 response. The live spec drives a real,
//! cloaked client through it and runs only with a driver build.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a spec fails with the message of the step it lost"
)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};

/// How long a spec waits on one stdout line before it fails as hung; a
/// live `launch` starts a daemon (its own guard is 300 s) and a client.
const HANG_GUARD: Duration = Duration::from_secs(290);

/// The server exe, started as `.mcp.json`'s launcher starts it.
struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Server {
    fn spawn(repo_root: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rt-driver-mcp"))
            .arg("--repo-root")
            .arg(repo_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("the server exe starts");
        let stdout = child.stdout.take().expect("stdout is piped");
        let (send, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if send.send(line).is_err() {
                    return;
                }
            }
        });
        let stdin = child.stdin.take();
        Self {
            child,
            stdin,
            lines,
        }
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin is still open");
        let mut text = message.to_string();
        text.push('\n');
        stdin.write_all(text.as_bytes()).expect("writes a frame");
        stdin.flush().expect("flushes a frame");
    }

    /// The next stdout line, or `None` once stdout closed.
    fn next_line(&self) -> Option<String> {
        match self.lines.recv_timeout(HANG_GUARD) {
            Ok(line) => Some(line),
            Err(mpsc::RecvTimeoutError::Disconnected) => None,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic_hung("a stdout line");
            }
        }
    }

    /// Sends request `id` and returns its response.
    fn request(&mut self, id: u64, method: &str, params: &Value) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let line = self.next_line().expect("the server answers");
        let response: Value = serde_json::from_str(&line).expect("a JSON frame");
        assert_eq!(response["id"], json!(id), "{response}");
        response
    }

    /// Closes stdin and waits for the server to exit.
    fn close(&mut self) -> ExitStatus {
        drop(self.stdin.take());
        let deadline = Instant::now() + HANG_GUARD;
        loop {
            if let Some(status) = self.child.try_wait().expect("polls the server") {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic_hung("the server's exit after stdin closed");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn panic_hung(what: &str) -> ! {
    panic!("hung: no {what} within {HANG_GUARD:?}")
}

fn initialize() -> Value {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "spec", "version": "0"},
        },
    })
}

fn tool_call(id: u64, name: &str, arguments: &Value) -> Value {
    json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": {"name": name, "arguments": arguments},
    })
}

/// A repo root of the spec's own, which holds no build.
fn scratch_repo(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("rt-driver-mcp-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("makes the scratch repo");
    dir
}

/// Sends `messages`, closes stdin, and returns every stdout line once the
/// server has exited, which it must do cleanly.
fn exchange(name: &str, messages: &[Value]) -> Vec<String> {
    let mut server = Server::spawn(&scratch_repo(name));
    for message in messages {
        server.send(message);
    }
    drop(server.stdin.take());
    let mut lines = Vec::new();
    while let Some(line) = server.next_line() {
        lines.push(line);
    }
    let status = server.close();
    assert!(status.success(), "the server exits cleanly: {status}");
    lines
}

/// The text of a tool result's first content item.
fn first_text(result: &Value) -> &str {
    result["content"][0]["text"]
        .as_str()
        .expect("a text content item")
}

#[test]
fn stdio_smoke_every_line_is_json_rpc() {
    let lines = exchange(
        "smoke",
        &[
            initialize(),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}),
            tool_call(4, "call", &json!({"session": "nope", "method": "ping"})),
        ],
    );
    assert_eq!(lines.len(), 4, "one response per request: {lines:#?}");
    let responses: Vec<Value> = lines
        .iter()
        .map(|line| serde_json::from_str(line).expect("every stdout line is JSON"))
        .collect();
    for (response, id) in responses.iter().zip(1..) {
        assert_eq!(response["jsonrpc"], json!("2.0"), "{response}");
        assert_eq!(response["id"], json!(id), "{response}");
        assert!(
            response.get("result").is_some() != response.get("error").is_some(),
            "a response has a result or an error: {response}"
        );
    }
    let init = &responses[0]["result"];
    assert_eq!(init["protocolVersion"], json!("2025-06-18"), "{init}");
    assert_eq!(init["serverInfo"]["name"], json!("rt-driver-mcp"), "{init}");
    assert_eq!(init["capabilities"], json!({"tools": {}}), "{init}");
    assert_eq!(responses[2]["result"], json!({}));
    let call = &responses[3]["result"];
    assert_eq!(call["isError"], json!(true), "{call}");
    let text = first_text(call);
    assert!(
        text.contains("nope") && text.contains("live sessions: none"),
        "{text}"
    );
}

#[test]
fn tools_list_pins_the_surface_and_annotations() {
    let lines = exchange(
        "list",
        &[
            initialize(),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        ],
    );
    let list: Value = serde_json::from_str(&lines[1]).expect("a JSON frame");
    let tools = list["result"]["tools"].as_array().expect("a tool array");
    let mut names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("a tool name"))
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["batch", "call", "launch", "screenshot", "stop"]);
    for tool in tools {
        for hint in ["readOnlyHint", "destructiveHint", "idempotentHint"] {
            assert!(
                tool["annotations"][hint].is_boolean(),
                "{} sets {hint}: {tool}",
                tool["name"]
            );
        }
        assert_eq!(
            tool["inputSchema"]["additionalProperties"],
            json!(false),
            "{tool}"
        );
    }
}

#[test]
fn call_without_a_session_is_refused_listing_sessions() {
    let lines = exchange(
        "no-session",
        &[
            initialize(),
            tool_call(2, "call", &json!({"method": "ping"})),
        ],
    );
    let response: Value = serde_json::from_str(&lines[1]).expect("a JSON frame");
    let result = &response["result"];
    assert_eq!(result["isError"], json!(true), "{response}");
    let text = first_text(result);
    assert!(
        text.contains("session is required") && text.contains("live sessions: none"),
        "{text}"
    );
}

#[test]
fn unknown_method_is_a_json_rpc_error() {
    let lines = exchange(
        "unknown",
        &[
            initialize(),
            json!({"jsonrpc": "2.0", "id": 7, "method": "resources/list"}),
        ],
    );
    let response: Value = serde_json::from_str(&lines[1]).expect("a JSON frame");
    assert_eq!(response["id"], json!(7), "{response}");
    assert_eq!(response["error"]["code"], json!(-32601), "{response}");
    let message = response["error"]["message"].as_str().expect("a message");
    assert!(message.contains("resources/list"), "{message}");
}

/// The checkout this crate sits in, two levels up.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("tools/rt-driver-mcp sits two levels under the repo root")
        .to_path_buf()
}

/// Calls tool `name` and returns its result, failing on a tool error.
fn tool(server: &mut Server, id: u64, name: &str, arguments: &Value) -> Value {
    let request = tool_call(id, name, arguments);
    let response = server.request(id, "tools/call", &request["params"]);
    let result = response["result"].clone();
    assert_eq!(result["isError"], json!(false), "{name}: {response}");
    result
}

/// A PNG's width and height, from its IHDR chunk.
fn png_size(bytes: &[u8]) -> (u64, u64) {
    assert!(
        bytes.starts_with(b"\x89PNG\r\n\x1a\n") && &bytes[12..16] == b"IHDR",
        "the image is a PNG"
    );
    let word = |at: usize| {
        let mut be = [0u8; 4];
        be.copy_from_slice(&bytes[at..at + 4]);
        u64::from(u32::from_be_bytes(be))
    };
    (word(16), word(20))
}

#[test]
#[ignore = "live: run via the gate with a driver build"]
fn driver_mcp_launch_ping_screenshot_stop() {
    let mut server = Server::spawn(&repo_root());
    server.request(1, "initialize", &initialize()["params"]);
    let session = format!("live-{}", std::process::id());
    let launched = tool(&mut server, 2, "launch", &json!({"session": session}));
    let info: Value = serde_json::from_str(first_text(&launched)).expect("launch says JSON");
    let scratch = PathBuf::from(info["scratch"].as_str().expect("a scratch folder"));

    let ping = tool(
        &mut server,
        3,
        "call",
        &json!({"session": session, "method": "ping"}),
    );
    let pong: Value = serde_json::from_str(first_text(&ping)).expect("ping says JSON");
    assert!(pong.is_object(), "{pong}");
    let state = tool(
        &mut server,
        4,
        "call",
        &json!({"session": session, "method": "state"}),
    );
    let state: Value = serde_json::from_str(first_text(&state)).expect("state says JSON");
    assert!(state.is_object(), "{state}");

    let shot = tool(&mut server, 5, "screenshot", &json!({"session": session}));
    let image = &shot["content"][0];
    assert_eq!(image["type"], json!("image"), "{shot}");
    assert_eq!(image["mimeType"], json!("image/png"), "{shot}");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(image["data"].as_str().expect("base64 data"))
        .expect("the data is base64");
    let said: Value =
        serde_json::from_str(shot["content"][1]["text"].as_str().expect("a text item"))
            .expect("the text item is JSON");
    assert_eq!(
        png_size(&bytes),
        (
            said["width"].as_u64().expect("a width"),
            said["height"].as_u64().expect("a height")
        ),
        "{said}"
    );

    tool(&mut server, 6, "stop", &json!({"session": session}));
    let status = server.close();
    assert!(status.success(), "the server exits cleanly: {status}");
    let left = dev_daemon::processes_under(&scratch);
    assert!(
        left.is_empty(),
        "processes left under {}: {left:?}",
        scratch.display()
    );
}
