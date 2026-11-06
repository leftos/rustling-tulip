//! The fixed tool surface: `launch`, `stop`, `call`, `batch` and
//! `screenshot`. A tool failure is a result with `isError`, never a
//! JSON-RPC error, so the agent reads its message and the error feed.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use dev_daemon::TestShell;
use serde_json::{Map, Value, json};

use crate::launch::{LaunchSpec, launch};
use crate::pipe::{CallError, Transport};
use crate::session::{Session, Sessions};

/// What every session argument tells the agent.
const SESSION_DOC: &str =
    "The session name `launch` returned; pass it on every call (there is no default session).";

/// A tool's result: its content items and whether it failed.
pub struct ToolResult {
    content: Vec<Value>,
    is_error: bool,
}

impl ToolResult {
    fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![json!({"type": "text", "text": text.into()})],
            is_error: false,
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            is_error: true,
            ..Self::text(text)
        }
    }

    /// The `tools/call` result object.
    #[must_use]
    pub fn into_value(self) -> Value {
        json!({"content": self.content, "isError": self.is_error})
    }
}

/// The `tools/list` array: every tool with its schema and all three hints.
#[must_use]
pub fn list() -> Value {
    let session = json!({"type": "string", "description": SESSION_DOC});
    let params = json!({"type": "object", "description": "The endpoint method's params; absent reads as {}."});
    let method = json!({"type": "string", "description": "An endpoint method (find, key, ping, screenshot, state, ...); an unknown one is refused listing the methods."});
    json!([
        tool(
            "launch",
            "Launch an isolated daemon and a cloaked, driven native client from a shadow copy of a driver build, under <repo_root>/.tmp/driver/. Returns {session, pid, pipe, scratch}.",
            &json!({
                "session": {"type": "string", "description": "A new session name: letters, digits, '-' and '_'."},
                "build_dir": {"type": "string", "description": "The folder holding the driver build; default <repo_root>/target/test-release."},
                "repo_root": {"type": "string", "description": "The checkout to run from; default the server's --repo-root."},
                "shell": {"type": "string", "enum": ["cmd", "default"], "description": "The isolated daemon's shell: cmd (default) for a fixed prompt and echo, or default to leave it to the Default shell setting with shell integration on."},
            }),
            &["session"],
            [false, false, false],
        ),
        tool(
            "stop",
            "Stop a session: kill the client's tree and the isolated daemon, and remove its scratch folder.",
            &json!({"session": session}),
            &["session"],
            [false, true, false],
        ),
        tool(
            "call",
            "Call one driver endpoint method and return its result as JSON text.",
            &json!({"session": session, "method": method, "params": params}),
            &["session", "method"],
            [false, true, false],
        ),
        tool(
            "batch",
            "Call endpoint methods in order, stopping at the first error. Returns {passed, steps, failedAt}.",
            &json!({
                "session": session,
                "steps": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"method": method, "params": params},
                        "required": ["method"],
                        "additionalProperties": false,
                    },
                },
            }),
            &["session", "steps"],
            [false, true, false],
        ),
        tool(
            "screenshot",
            "Capture the client's window as a PNG image, with its path, width and height.",
            &json!({"session": session}),
            &["session"],
            [true, false, true],
        ),
    ])
}

/// One tool's listing; `hints` is `[readOnly, destructive, idempotent]`.
fn tool(
    name: &str,
    description: &str,
    properties: &Value,
    required: &[&str],
    hints: [bool; 3],
) -> Value {
    let [read_only, destructive, idempotent] = hints;
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        },
        "annotations": {
            "readOnlyHint": read_only,
            "destructiveHint": destructive,
            "idempotentHint": idempotent,
            "openWorldHint": false,
        },
    })
}

/// The tools and the sessions they share.
pub struct Tools {
    repo_root: PathBuf,
    sessions: Sessions,
}

impl Tools {
    #[must_use]
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            sessions: Sessions::default(),
        }
    }

    /// Runs tool `name`; `None` when there is no such tool.
    pub fn call(&mut self, name: &str, args: &Map<String, Value>) -> Option<ToolResult> {
        let result = match name {
            "launch" => self.launch(args),
            "stop" => self.stop(args),
            "call" => self.call_method(args),
            "batch" => self.batch(args),
            "screenshot" => self.screenshot(args),
            _ => return None,
        };
        Some(result.unwrap_or_else(ToolResult::error))
    }

    /// Stops every session.
    pub fn stop_all(&mut self) {
        self.sessions.stop_all();
    }

    fn launch(&mut self, args: &Map<String, Value>) -> Result<ToolResult, String> {
        check_args(
            args,
            &["session", "build_dir", "repo_root", "shell"],
            &["session"],
        )?;
        let shell = match optional_string(args, "shell")? {
            None | Some("cmd") => TestShell::Cmd,
            Some("default") => TestShell::Default,
            Some(other) => {
                return Err(format!(
                    "shell must be \"cmd\" or \"default\", not \"{other}\""
                ));
            }
        };
        let session = string_arg(args, "session")?.to_owned();
        check_session_name(&session)?;
        if self.sessions.contains(&session) {
            return Err(format!(
                "session {session} is already live; stop it or pick another name ({})",
                self.sessions.listing()
            ));
        }
        let repo_root = match optional_string(args, "repo_root")? {
            Some(root) => PathBuf::from(root),
            None => self.repo_root.clone(),
        };
        let build_dir = match optional_string(args, "build_dir")? {
            Some(dir) => repo_root.join(dir),
            None => repo_root.join("target").join("test-release"),
        };
        let scratch = repo_root
            .join(".tmp")
            .join("driver")
            .join(format!("{session}-{}", std::process::id()));
        let launched = launch(&LaunchSpec {
            session,
            repo_root,
            build_dir,
            scratch,
            shell,
        })?;
        let summary = launched.summary();
        tracing::info!(%summary, "launched");
        self.sessions.insert(launched);
        Ok(ToolResult::text(summary.to_string()))
    }

    fn stop(&mut self, args: &Map<String, Value>) -> Result<ToolResult, String> {
        check_args(args, &["session"], &["session"])?;
        let name = self.session_name(args)?;
        let session = self.sessions.remove(&name);
        drop(session);
        tracing::info!(session = %name, "stopped");
        Ok(ToolResult::text(format!("stopped {name}")))
    }

    fn call_method(&mut self, args: &Map<String, Value>) -> Result<ToolResult, String> {
        check_args(
            args,
            &["session", "method", "params"],
            &["session", "method"],
        )?;
        let session = self.session(args)?;
        let method = string_arg(args, "method").map_err(|err| session.with_feed(&err))?;
        let params = args.get("params").cloned().unwrap_or_else(|| json!({}));
        Ok(match session.pipe().call(method, params) {
            Ok(result) => ToolResult::text(result.to_string()),
            Err(err) => ToolResult::error(session.with_feed(&call_error(method, &err))),
        })
    }

    fn batch(&mut self, args: &Map<String, Value>) -> Result<ToolResult, String> {
        check_args(args, &["session", "steps"], &["session", "steps"])?;
        let session = self.session(args)?;
        let steps = parse_steps(args).map_err(|err| session.with_feed(&err))?;
        let outcome = run_batch(session.pipe(), &steps);
        let passed = outcome["passed"] == json!(true);
        let mut result = ToolResult::text(outcome.to_string());
        if !passed {
            result
                .content
                .push(json!({"type": "text", "text": session.with_feed("a step failed")}));
        }
        Ok(result)
    }

    fn screenshot(&mut self, args: &Map<String, Value>) -> Result<ToolResult, String> {
        check_args(args, &["session"], &["session"])?;
        let session = self.session(args)?;
        let shot = match session.pipe().call("screenshot", json!({})) {
            Ok(shot) => shot,
            Err(err) => return Err(session.with_feed(&call_error("screenshot", &err))),
        };
        let image = image_content(&shot).map_err(|err| session.with_feed(&err))?;
        Ok(ToolResult {
            content: vec![image, json!({"type": "text", "text": shot.to_string()})],
            is_error: false,
        })
    }

    /// The live session `args` names.
    fn session(&mut self, args: &Map<String, Value>) -> Result<&mut Session, String> {
        let name = self.session_name(args)?;
        self.sessions
            .get_mut(&name)
            .ok_or_else(|| "the session vanished between lookup and use".to_owned())
    }

    /// The name of a live session `args` names, or a refusal listing them.
    fn session_name(&self, args: &Map<String, Value>) -> Result<String, String> {
        let Some(name) = args.get("session") else {
            return Err(format!(
                "session is required: pass the name `launch` returned ({})",
                self.sessions.listing()
            ));
        };
        let Some(name) = name.as_str() else {
            return Err(format!("session takes a string, not {name}"));
        };
        if !self.sessions.contains(name) {
            return Err(format!(
                "no live session named {name}; pass the name `launch` returned ({})",
                self.sessions.listing()
            ));
        }
        Ok(name.to_owned())
    }
}

/// A `call` failure's text, naming the method.
fn call_error(method: &str, err: &CallError) -> String {
    format!("{method} failed: {err}")
}

/// Refuses an argument the tool does not take, and a required one missing.
fn check_args(args: &Map<String, Value>, takes: &[&str], required: &[&str]) -> Result<(), String> {
    if let Some(unknown) = args.keys().find(|key| !takes.contains(&key.as_str())) {
        return Err(format!(
            "this tool has no argument '{unknown}'; it takes: {}",
            takes.join(", ")
        ));
    }
    match required.iter().find(|key| !args.contains_key(**key)) {
        Some(missing) if *missing == "session" => Ok(()),
        Some(missing) => Err(format!("{missing} is required")),
        None => Ok(()),
    }
}

fn string_arg<'a>(args: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    optional_string(args, key)?.ok_or_else(|| format!("{key} is required"))
}

fn optional_string<'a>(args: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(other) => Err(format!("{key} takes a string, not {other}")),
    }
}

/// Refuses a name that cannot be a folder name of its own.
fn check_session_name(name: &str) -> Result<(), String> {
    let fits = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    if !name.is_empty() && name.len() <= 64 && name.chars().all(fits) {
        return Ok(());
    }
    Err(format!(
        "session name {name:?} is not usable: use 1-64 letters, digits, '-' or '_'"
    ))
}

/// One batch step: a method and its params.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub method: String,
    pub params: Value,
}

/// `args.steps` as steps, refusing any key a step does not take.
fn parse_steps(args: &Map<String, Value>) -> Result<Vec<Step>, String> {
    let Some(Value::Array(steps)) = args.get("steps") else {
        return Err("steps takes an array of {method, params?}".to_owned());
    };
    steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let Some(step) = step.as_object() else {
                return Err(format!("steps[{index}] takes an object, not {step}"));
            };
            check_args(step, &["method", "params"], &["method"])
                .map_err(|err| format!("steps[{index}]: {err}"))?;
            let method =
                string_arg(step, "method").map_err(|err| format!("steps[{index}]: {err}"))?;
            Ok(Step {
                method: method.to_owned(),
                params: step.get("params").cloned().unwrap_or_else(|| json!({})),
            })
        })
        .collect()
}

/// Runs `steps` in order and stops at the first error:
/// `{passed, steps: [{method, result} | {method, error}], failedAt}`.
pub fn run_batch(transport: &mut dyn Transport, steps: &[Step]) -> Value {
    let mut done = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        match transport.call(&step.method, step.params.clone()) {
            Ok(result) => done.push(json!({"method": step.method, "result": result})),
            Err(err) => {
                done.push(json!({"method": step.method, "error": err.to_string()}));
                return json!({"passed": false, "steps": done, "failedAt": index});
            }
        }
    }
    json!({"passed": true, "steps": done, "failedAt": null})
}

/// The PNG a `screenshot` result names, as MCP image content.
fn image_content(shot: &Value) -> Result<Value, String> {
    let Some(path) = shot["path"].as_str() else {
        return Err(format!("the screenshot result names no path: {shot}"));
    };
    let bytes = std::fs::read(Path::new(path))
        .map_err(|err| format!("reading the screenshot {path}: {err}"))?;
    Ok(json!({
        "type": "image",
        "mimeType": "image/png",
        "data": base64::engine::general_purpose::STANDARD.encode(bytes),
    }))
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::expect_used,
        reason = "a spec fails with the message of the step it lost"
    )]

    use std::path::PathBuf;

    use serde_json::{Value, json};

    use super::{Step, Tools, list, run_batch};
    use crate::pipe::{CallError, Transport};

    /// Answers each method from a script and records the calls.
    struct Scripted {
        calls: Vec<String>,
    }

    impl Transport for Scripted {
        fn call(&mut self, method: &str, _params: Value) -> Result<Value, CallError> {
            self.calls.push(method.to_owned());
            if method == "nope" {
                return Err(CallError::Endpoint {
                    code: "unknown_method".to_owned(),
                    message: "unknown method: nope".to_owned(),
                    hint: Some("methods: find, key, ping, screenshot, state".to_owned()),
                });
            }
            Ok(json!({"answered": method}))
        }
    }

    fn step(method: &str) -> Step {
        Step {
            method: method.to_owned(),
            params: json!({}),
        }
    }

    #[test]
    fn batch_stops_at_the_first_error() {
        let mut transport = Scripted { calls: Vec::new() };
        let outcome = run_batch(&mut transport, &[step("ping"), step("nope"), step("state")]);

        assert_eq!(
            transport.calls,
            ["ping", "nope"],
            "nothing runs after the failed step"
        );
        assert_eq!(outcome["passed"], json!(false), "{outcome}");
        assert_eq!(outcome["failedAt"], json!(1), "{outcome}");
        assert_eq!(
            outcome["steps"][0]["result"],
            json!({"answered": "ping"}),
            "{outcome}"
        );
        let error = outcome["steps"][1]["error"].as_str().unwrap_or_default();
        assert!(
            error.contains("unknown_method") && error.contains("hint: methods"),
            "{outcome}"
        );
        assert_eq!(
            outcome["steps"].as_array().map(Vec::len),
            Some(2),
            "{outcome}"
        );
    }

    /// The `launch` tool's entry in `tools/list`.
    fn launch_tool() -> Value {
        list()
            .as_array()
            .expect("the listing is an array")
            .iter()
            .find(|tool| tool["name"] == json!("launch"))
            .cloned()
            .expect("the launch tool is listed")
    }

    #[test]
    fn launch_lists_the_shell_property() {
        let launch = launch_tool();
        let shell = &launch["inputSchema"]["properties"]["shell"];
        assert_eq!(shell["enum"], json!(["cmd", "default"]), "{launch}");
    }

    #[test]
    fn launch_refuses_an_unknown_shell() {
        let mut tools = Tools::new(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        let args = json!({"session": "a", "shell": "bash"});
        let refused = tools
            .call("launch", args.as_object().expect("an object of arguments"))
            .expect("the launch tool exists")
            .into_value();
        assert_eq!(refused["isError"], json!(true), "{refused}");
        let text = refused["content"][0]["text"].as_str().expect("a text item");
        assert!(
            text.contains("shell must be \"cmd\" or \"default\", not \"bash\""),
            "{text}"
        );
    }
}
