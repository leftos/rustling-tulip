//! The fixed tool surface: `launch`, `stop`, `call`, `batch` and
//! `screenshot`. A tool failure is a result with `isError`, never a
//! JSON-RPC error, so the agent reads its message and the error feed.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use daemon_client::isolation::under_tmp;
use dev_daemon::TestShell;
use serde_json::{Map, Value, json};

use crate::launch::{LaunchSpec, launch};
use crate::pipe::{CallError, Transport};
use crate::session::{Session, Sessions};
use crate::token::{self, CredentialManager, TokenStore};

/// What every session argument tells the agent.
const SESSION_DOC: &str =
    "The session name `launch` returned; pass it on every call (there is no default session).";

/// The variables `launch`'s `env` may set: the agent and tailnet CLIs and the
/// phone app's folder. No isolation folder and no agent home is settable. The
/// one folder on the list, `RUSTLING_TULIP_TAILNET_DIR`, must sit under `.tmp`
/// like the isolation folders.
const ALLOWED_ENV: &[&str] = &[
    "RUSTLING_TULIP_CLAUDE",
    "RUSTLING_TULIP_TAILSCALE",
    "RUSTLING_TULIP_PWA_DIR",
    "RUSTLING_TULIP_TAILNET_DIR",
];

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
            "Launch an isolated daemon and a cloaked, driven native client from a shadow copy of a driver build, under <repo_root>/.tmp/driver/. Returns {session, pid, pipe, scratch, scratch_repo}: scratch_repo is an empty folder outside the checkout, <parent of repo_root>/.tmp/rt-driver/<session>-<server pid>/repo, removed at stop, for action add_repo (a path inside the checkout is refused). An env that sets RUSTLING_TULIP_CLAUDE logs the real CLI in with the driver token the server holds.",
            &json!({
                "session": {"type": "string", "description": "A new session name: letters, digits, '-' and '_'."},
                "build_dir": {"type": "string", "description": "The folder holding the driver build; default <repo_root>/target/test-release."},
                "repo_root": {"type": "string", "description": "The checkout to run from; default the server's --repo-root."},
                "shell": {"type": "string", "enum": ["cmd", "default"], "description": "The isolated daemon's shell: cmd (default) for a fixed prompt and echo, or default to leave it to the Default shell setting with shell integration on."},
                "env": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Variables set on the isolated daemon and the client after the isolation ones, for a run against a real Claude or the phone app; only RUSTLING_TULIP_CLAUDE, RUSTLING_TULIP_TAILSCALE, RUSTLING_TULIP_PWA_DIR and RUSTLING_TULIP_TAILNET_DIR are accepted. The config, binaries and worktrees folders and each agent's home stay isolated under .tmp whatever is set."},
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
    tokens: Box<dyn TokenStore>,
}

impl Tools {
    /// The tools, reading the driver token from Credential Manager.
    #[must_use]
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            sessions: Sessions::default(),
            tokens: Box::new(CredentialManager),
        }
    }

    /// The tools, reading the driver token from `tokens`.
    #[cfg(test)]
    #[must_use]
    pub fn with_tokens(repo_root: PathBuf, tokens: Box<dyn TokenStore>) -> Self {
        Self {
            tokens,
            ..Self::new(repo_root)
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
            &["session", "build_dir", "repo_root", "shell", "env"],
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
        let env = launch_env(args)?;
        let session = string_arg(args, "session")?.to_owned();
        check_session_name(&session)?;
        if self.sessions.contains(&session) {
            return Err(format!(
                "session {session} is already live; stop it or pick another name ({})",
                self.sessions.listing()
            ));
        }
        let daemon_only_env = token::daemon_only_env(&env, self.tokens.as_ref())?;
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
            env,
            daemon_only_env,
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
        let step = Step {
            method: method.to_owned(),
            params,
        };
        refuse_add_repo_in_checkout(std::slice::from_ref(&step), session.repo_root())?;
        Ok(match session.pipe().call(method, step.params) {
            Ok(result) => ToolResult::text(result.to_string()),
            Err(err) => ToolResult::error(session.with_feed(&call_error(method, &err))),
        })
    }

    fn batch(&mut self, args: &Map<String, Value>) -> Result<ToolResult, String> {
        check_args(args, &["session", "steps"], &["session", "steps"])?;
        let session = self.session(args)?;
        let steps = parse_steps(args).map_err(|err| session.with_feed(&err))?;
        refuse_add_repo_in_checkout(&steps, session.repo_root())?;
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

/// `args.env` as environment rows, in the object's own order, refusing a name
/// off [`ALLOWED_ENV`], a value that is not a string, an empty value and a
/// tailnet folder that is not an absolute path under `.tmp`.
fn launch_env(args: &Map<String, Value>) -> Result<Vec<(&'static str, OsString)>, String> {
    let Some(env) = args.get("env") else {
        return Ok(Vec::new());
    };
    let Some(rows) = env.as_object() else {
        return Err("env must be an object of names to strings".to_owned());
    };
    rows.iter()
        .map(|(name, value)| {
            let allowed = ALLOWED_ENV
                .iter()
                .find(|allowed| **allowed == name.as_str())
                .copied()
                .ok_or_else(|| format!("env may only set {}, not {name:?}", allowlist_of_env()))?;
            let Some(value) = value.as_str() else {
                return Err(format!("env value for {name} must be a string"));
            };
            if value.is_empty() {
                return Err(format!("env value for {name} must not be empty"));
            }
            // The one folder a row may set: a real certificate folder for a
            // phone-app run, never the user's own.
            if allowed == "RUSTLING_TULIP_TAILNET_DIR" {
                let path = Path::new(value);
                if !path.is_absolute() || !under_tmp(path) {
                    return Err(format!(
                        "env value for {name} must be an absolute path under a .tmp folder, \
                         not {value:?}"
                    ));
                }
            }
            Ok((allowed, OsString::from(value)))
        })
        .collect()
}

/// [`ALLOWED_ENV`] as a refusal words it: `a, b, c, d or e`.
fn allowlist_of_env() -> String {
    match ALLOWED_ENV.split_last() {
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => String::new(),
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

/// Refuses, before any step reaches the pipe, an `action add_repo` step
/// whose `args.path` lies inside the checkout `repo_root`: the driven agent
/// works in the scratch repo `launch` made outside it.
fn refuse_add_repo_in_checkout(steps: &[Step], repo_root: &Path) -> Result<(), String> {
    for step in steps {
        if step.method != "action" || step.params["name"] != json!("add_repo") {
            continue;
        }
        let Some(path) = step.params["args"]["path"].as_str() else {
            continue;
        };
        if inside(Path::new(path), repo_root) {
            return Err(format!(
                "add_repo: {path} is inside the checkout; use the scratch_repo launch returned"
            ));
        }
    }
    Ok(())
}

/// Whether `path` is `root` or under it, both canonicalized, ignoring case
/// on Windows. A path that cannot be resolved is left to the endpoint, which
/// refuses a folder that does not exist.
fn inside(path: &Path, root: &Path) -> bool {
    let (Ok(path), Ok(root)) = (std::fs::canonicalize(path), std::fs::canonicalize(root)) else {
        return false;
    };
    if cfg!(windows) {
        let folded = |path: &Path| PathBuf::from(path.to_string_lossy().to_lowercase());
        folded(&path).starts_with(folded(&root))
    } else {
        path.starts_with(&root)
    }
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

    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    use serde_json::{Map, Value, json};

    use super::{Step, Tools, launch_env, list, refuse_add_repo_in_checkout, run_batch};
    use crate::pipe::{CallError, Transport};
    use crate::token::MemoryStore;

    /// The workspace root, two levels above this package.
    fn checkout() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("tools/rt-driver-mcp sits two levels under the repo root")
            .to_path_buf()
    }

    /// An `action add_repo` step for `path`.
    fn add_repo(path: &str) -> Step {
        Step {
            method: "action".to_owned(),
            params: json!({"name": "add_repo", "args": {"path": path}}),
        }
    }

    #[test]
    fn a_real_claude_launch_without_a_token_is_refused() {
        let mut tools = Tools::with_tokens(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            Box::new(MemoryStore::default()),
        );
        let args = json!({"session": "a", "env": {"RUSTLING_TULIP_CLAUDE": "C:\\x\\claude.exe"}});
        let refused = tools
            .call("launch", args.as_object().expect("an object of arguments"))
            .expect("the launch tool exists")
            .into_value();
        assert_eq!(refused["isError"], json!(true), "{refused}");
        assert_eq!(
            refused["content"][0]["text"],
            json!(
                "no driver token stored: run tools/rt-driver-mcp/set-driver-token.ps1 once \
                 (it asks for the token from claude setup-token)"
            ),
            "{refused}"
        );
    }

    /// `call` checks its one step, and `batch` all of its steps, before
    /// anything reaches the pipe; the comparison ignores case on Windows.
    #[test]
    fn add_repo_inside_the_checkout_is_refused() {
        let root = checkout();
        let mut inside = root.join("tools").display().to_string();
        if cfg!(windows) {
            inside = inside.to_uppercase();
        }
        let expected = format!(
            "add_repo: {inside} is inside the checkout; use the scratch_repo launch returned"
        );

        assert_eq!(
            refuse_add_repo_in_checkout(&[add_repo(&inside)], &root),
            Err(expected.clone()),
            "call"
        );
        assert_eq!(
            refuse_add_repo_in_checkout(&[step("ping"), add_repo(&inside)], &root),
            Err(expected),
            "batch"
        );
    }

    #[test]
    fn add_repo_outside_the_checkout_passes_through() {
        let root = checkout();
        let outside = std::env::temp_dir();
        let outside = outside.to_str().expect("a UTF-8 temp dir");
        assert_eq!(
            refuse_add_repo_in_checkout(&[add_repo(outside)], &root),
            Ok(())
        );
        let other_action = Step {
            method: "action".to_owned(),
            params: json!({"name": "focus_pane", "args": {"path": root.display().to_string()}}),
        };
        assert_eq!(
            refuse_add_repo_in_checkout(&[step("ping"), other_action], &root),
            Ok(()),
            "only add_repo is checked"
        );
    }

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

    /// `args` holding only `env`.
    fn env_args(env: Value) -> Map<String, Value> {
        let mut args = Map::new();
        args.insert("env".to_owned(), env);
        args
    }

    #[test]
    fn launch_env_is_empty_without_the_parameter() {
        let rows = launch_env(&Map::new()).expect("no env is no rows");
        assert!(rows.is_empty(), "{rows:?}");
    }

    #[test]
    fn launch_env_keeps_allowed_names_in_order() {
        let tailnet = std::env::current_dir()
            .expect("the test runs in a folder")
            .join(".tmp")
            .join("driver")
            .join("tailnet");
        let tailnet = tailnet.to_str().expect("a UTF-8 path");
        let env = json!({
            "RUSTLING_TULIP_TAILNET_DIR": tailnet,
            "RUSTLING_TULIP_CLAUDE": "C:\\tmp\\claude.exe",
            "RUSTLING_TULIP_PWA_DIR": "C:\\tmp\\pwa",
            "RUSTLING_TULIP_TAILSCALE": "C:\\tmp\\tailscale.exe",
        });
        let rows = launch_env(&env_args(env.clone())).expect("every name is allowed");

        let expected: Vec<String> = env
            .as_object()
            .expect("an object")
            .keys()
            .cloned()
            .collect();
        let names: Vec<&str> = rows.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names, expected,
            "rows follow the object's order, none dropped"
        );
        for (name, value) in &rows {
            let given = env[*name].as_str().expect("a string");
            assert_eq!(value.to_str(), Some(given), "{name}");
        }
    }

    #[test]
    fn launch_env_refuses_a_name_off_the_allowlist() {
        let args = env_args(json!({"RUSTLING_TULIP_CONFIG_DIR": "C:\\tmp\\config"}));
        let refused = launch_env(&args).expect_err("an isolation variable is not settable");
        assert_eq!(
            refused,
            "env may only set RUSTLING_TULIP_CLAUDE, RUSTLING_TULIP_TAILSCALE, \
             RUSTLING_TULIP_PWA_DIR or RUSTLING_TULIP_TAILNET_DIR, not \"RUSTLING_TULIP_CONFIG_DIR\""
        );
    }

    /// The agent's config folder stays the isolated daemon's own scratch home.
    #[test]
    fn launch_env_refuses_claude_config_dir() {
        let args = env_args(json!({"CLAUDE_CONFIG_DIR": "C:\\Users\\x\\.claude"}));
        let refused = launch_env(&args).expect_err("the agent's home is not settable");
        assert_eq!(
            refused,
            "env may only set RUSTLING_TULIP_CLAUDE, RUSTLING_TULIP_TAILSCALE, \
             RUSTLING_TULIP_PWA_DIR or RUSTLING_TULIP_TAILNET_DIR, not \"CLAUDE_CONFIG_DIR\""
        );
    }

    #[test]
    fn launch_env_refuses_a_non_string_value() {
        let args = env_args(json!({"RUSTLING_TULIP_CLAUDE": 7}));
        let refused = launch_env(&args).expect_err("a non-string is refused");
        assert_eq!(
            refused,
            "env value for RUSTLING_TULIP_CLAUDE must be a string"
        );
    }

    #[test]
    fn launch_env_refuses_an_empty_value() {
        let args = env_args(json!({"RUSTLING_TULIP_PWA_DIR": ""}));
        let refused = launch_env(&args).expect_err("an empty value is refused");
        assert_eq!(
            refused,
            "env value for RUSTLING_TULIP_PWA_DIR must not be empty"
        );
    }

    #[test]
    fn launch_env_refuses_a_non_object() {
        let args = env_args(json!("RUSTLING_TULIP_CLAUDE=C:\\claude.exe"));
        let refused = launch_env(&args).expect_err("a string is not an object");
        assert_eq!(refused, "env must be an object of names to strings");
    }

    #[test]
    fn launch_env_refuses_a_tailnet_dir_outside_tmp() {
        for value in ["C:\\Users\\x\\tailnet", "tailnet"] {
            let args = env_args(json!({"RUSTLING_TULIP_TAILNET_DIR": value}));
            let refused = launch_env(&args).expect_err("only a folder under .tmp is accepted");
            assert_eq!(
                refused,
                format!(
                    "env value for RUSTLING_TULIP_TAILNET_DIR must be an absolute path under a \
                     .tmp folder, not {value:?}"
                )
            );
        }
    }

    #[test]
    fn launch_env_keeps_a_tailnet_dir_under_tmp() {
        let tailnet = std::env::current_dir()
            .expect("the test runs in a folder")
            .join(".tmp")
            .join("driver")
            .join("tailnet");
        let tailnet = tailnet.to_str().expect("a UTF-8 path");

        let rows = launch_env(&env_args(json!({"RUSTLING_TULIP_TAILNET_DIR": tailnet})))
            .expect("an absolute path under a .tmp folder is accepted");

        assert_eq!(
            rows,
            vec![("RUSTLING_TULIP_TAILNET_DIR", OsString::from(tailnet))]
        );
    }

    #[test]
    fn the_launch_schema_lists_env() {
        let launch = launch_tool();
        let env = &launch["inputSchema"]["properties"]["env"];
        assert_eq!(env["type"], json!("object"), "{launch}");
        assert_eq!(
            env["additionalProperties"],
            json!({"type": "string"}),
            "{launch}"
        );
    }
}
