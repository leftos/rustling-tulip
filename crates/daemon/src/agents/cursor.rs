//! Cursor (`cursor-agent` CLI from cursor.com) backend.
//!
//! Interactive only: this backend has no headless path. A fresh interactive
//! spawn first pre-creates an empty chat with `cursor-agent create-chat`
//! ([`create_chat`]) and opens it with `--resume <id>`, so the session
//! records a chat id it can be resumed with later; a resumed spawn passes
//! the recorded id. Every spawn with a working directory passes
//! `--workspace <cwd> --trust`, since a chat is matched by its workspace.
//! `--sandbox enabled` is dropped on Windows, where cursor-agent's sandbox
//! fails.
//!
//! `--workspace` takes a single directory and there is no `--add-dir`
//! equivalent: against a multi-repo workspace target the daemon still
//! creates worktrees for every member, but cursor only sees the first
//! member's worktree.

use super::{AgentBackend, CommonSpawnFields, Launch, PRECREATE_TIMEOUT};
use anyhow::{Context as _, anyhow};
use futures::future::BoxFuture;
use protocol::{AgentOptions, CursorSandbox, SessionMember};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, Command};
use tracing::{debug, warn};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// How long [`create_chat`] waits for its killed process to be reaped.
const REAP_TIMEOUT: Duration = Duration::from_secs(2);

pub struct CursorBackend;

impl AgentBackend for CursorBackend {
    fn program_env_var(&self) -> &'static str {
        "RUSTLING_TULIP_CURSOR_AGENT"
    }

    fn default_program(&self) -> &'static str {
        "cursor-agent"
    }

    fn build_interactive_args(
        &self,
        opts: &AgentOptions,
        common: &CommonSpawnFields,
        members: &[SessionMember],
        initial_prompt: Option<&str>,
    ) -> Vec<String> {
        let (plan_mode, sandbox) = match opts {
            AgentOptions::Cursor { plan_mode, sandbox } => (*plan_mode, *sandbox),
            AgentOptions::Claude { .. } | AgentOptions::Codex { .. } => {
                debug_assert!(false, "cursor backend invoked with non-cursor options");
                (false, None)
            }
        };
        // cursor-agent takes a single `--workspace` folder, so a workspace or
        // standalone target spawns in the first folder only; name the folders
        // this drops so a multi-folder recovery is not silent.
        let dropped: Vec<&str> = members
            .iter()
            .skip(1)
            .map(|m| m.worktree_path.as_str())
            .chain(common.add_dirs.iter().map(String::as_str))
            .collect();
        if !dropped.is_empty() {
            let cwd = common.cwd.unwrap_or_default();
            warn!(
                "cursor-agent takes one --workspace folder; running in {cwd} without {dropped:?}"
            );
        }
        build_args(common, plan_mode, sandbox, initial_prompt)
    }

    /// The pre-created chat, else the resumed one.
    fn own_conversation_at_spawn(
        &self,
        resume: Option<&str>,
        created: Option<&str>,
    ) -> Option<String> {
        created.or(resume).map(str::to_owned)
    }

    fn precreate_conversation<'a>(
        &self,
        launch: &'a Launch<'a>,
    ) -> Option<BoxFuture<'a, anyhow::Result<String>>> {
        Some(Box::pin(create_chat(
            launch.program,
            launch.prepend,
            launch.cwd,
            launch.env,
            PRECREATE_TIMEOUT,
        )))
    }
}

/// Pure arg construction lifted out as a free fn for unit testing.
///
/// Layout:
/// - `--resume <id>` when [`CommonSpawnFields::resume_conversation`] is set
/// - `--workspace <cwd> --trust` when [`CommonSpawnFields::cwd`] is set: a
///   chat is matched by its workspace, so a spawn and its later resume pass
///   the same path
/// - `--model <id>` when [`CommonSpawnFields::model`] is set
/// - `--plan` when `plan_mode` is true
/// - permission/sandbox: `--yolo` overrides everything; otherwise
///   `--sandbox <value>` when a sandbox is set, except `enabled` on Windows,
///   where cursor-agent's sandbox fails
/// - trailing positional `<prompt>` when no prompt-injector is attached,
///   resumed or not.
///   Cursor does not expose a system-prompt flag, so no workspace prelude is
///   emitted. Cursor runs in the first folder only and is told nothing about
///   the others: a multi-folder spawn drops the extra members and `add_dirs`
///   with a warning.
fn build_args(
    common: &CommonSpawnFields<'_>,
    plan_mode: bool,
    sandbox: Option<CursorSandbox>,
    initial_prompt: Option<&str>,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if let Some(id) = common.resume_conversation {
        args.push("--resume".to_string());
        args.push(id.to_string());
    }
    if let Some(cwd) = common.cwd {
        args.push("--workspace".to_string());
        args.push(cwd.to_string());
        args.push("--trust".to_string());
    }
    if let Some(model) = common.model {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if plan_mode {
        args.push("--plan".to_string());
    }
    if common.dangerously_skip_permissions {
        args.push("--yolo".to_string());
    } else if let Some(s) = usable_sandbox(sandbox) {
        args.push("--sandbox".to_string());
        args.push(s.as_cli_arg().to_string());
    }
    if !common.has_prompt_injector
        && let Some(prompt) = initial_prompt
    {
        args.push(prompt.to_string());
    }
    args
}

/// The sandbox a spawn passes: `sandbox`, except that `enabled` is dropped
/// with a warning on Windows, where cursor-agent's sandbox fails.
fn usable_sandbox(sandbox: Option<CursorSandbox>) -> Option<CursorSandbox> {
    if cfg!(windows) && sandbox == Some(CursorSandbox::Enabled) {
        warn!("cursor-agent's sandbox is not available on Windows; spawning without it");
        return None;
    }
    sandbox
}

/// Pre-create an empty Cursor chat and return its id: runs `program` with
/// `prepend` and `create-chat` in `cwd`, with `env` set on top of the
/// daemon's environment as for the spawn, and reads the first stdout line
/// within `timeout`, which must be a UUID. `create-chat` prints the id at
/// once but may keep running, so its whole process tree is killed however
/// this ends (an npm shim's child outlives a kill of the top process alone),
/// including when the caller drops the returned future.
///
/// # Errors
///
/// The program fails to start, closes its output or prints nothing within
/// `timeout`, or its first line is not a UUID.
pub async fn create_chat(
    program: &str,
    prepend: &[String],
    cwd: &Path,
    env: &[(String, String)],
    timeout: Duration,
) -> anyhow::Result<String> {
    let mut cmd = Command::new(program);
    cmd.args(prepend)
        .envs(env.iter().map(|(name, value)| (name, value)))
        .arg("create-chat")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // On Windows the guard's taskkill ends the tree on a drop; killing
        // the top process first would hide its children from that walk.
        .kill_on_drop(cfg!(not(windows)));
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {program} create-chat in {}", cwd.display()))?;
    let mut guard = TreeGuard(child.id());
    let first = first_line(&mut child, timeout).await;
    kill_tree(&mut child).await;
    guard.0 = None;
    let line = first?;
    let id = line.trim();
    uuid::Uuid::parse_str(id)
        .with_context(|| format!("{program} create-chat printed {id:?}, not a chat id"))?;
    Ok(id.to_owned())
}

/// Holds a `create-chat` process id until [`kill_tree`] has run, and starts a
/// `taskkill` of its tree without waiting when dropped before that (the
/// caller dropped [`create_chat`]'s future).
struct TreeGuard(Option<u32>);

impl Drop for TreeGuard {
    fn drop(&mut self) {
        let Some(pid) = self.0 else {
            return;
        };
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            let started = std::process::Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .spawn();
            if let Err(err) = started {
                warn!(
                    pid,
                    ?err,
                    "running taskkill for a dropped create-chat failed"
                );
            }
        }
        #[cfg(not(windows))]
        debug!(pid, "a dropped create-chat is killed by kill_on_drop");
    }
}

/// The first line `child` prints on stdout within `timeout`.
async fn first_line(child: &mut Child, timeout: Duration) -> anyhow::Result<String> {
    let stdout = child
        .stdout
        .take()
        .context("create-chat's stdout was not piped")?;
    let mut lines = BufReader::new(stdout).lines();
    match tokio::time::timeout(timeout, lines.next_line()).await {
        Ok(Ok(Some(line))) => Ok(line),
        Ok(Ok(None)) => Err(anyhow!("create-chat exited without printing a chat id")),
        Ok(Err(err)) => Err(err).context("reading create-chat's output"),
        Err(_) => Err(anyhow!("create-chat printed no chat id within {timeout:?}")),
    }
}

/// Kill `child` and every process it started, then reap it.
async fn kill_tree(child: &mut Child) {
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let mut taskkill = Command::new("taskkill");
        taskkill
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        match taskkill.status().await {
            // A failure is expected when create-chat has already exited.
            Ok(status) if !status.success() => {
                debug!(pid, %status, "taskkill of create-chat's process tree failed");
            }
            Ok(_) => {}
            Err(err) => warn!(pid, ?err, "running taskkill for create-chat failed"),
        }
    }
    if let Err(err) = child.start_kill() {
        debug!(?err, "killing create-chat failed; it has likely exited");
    }
    match tokio::time::timeout(REAP_TIMEOUT, child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => warn!(?err, "waiting for create-chat to exit failed"),
        Err(_) => warn!("create-chat did not exit after being killed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAT_ID: &str = "3f1c2b9e-7a4d-4e8f-9b2a-6c5d4e3f2a1b";
    const CWD: &str = r"C:\wt\repo";

    fn resumed<'a>(
        resume: Option<&'a str>,
        cwd: Option<&'a str>,
        model: Option<&'a str>,
    ) -> CommonSpawnFields<'a> {
        CommonSpawnFields {
            resume_conversation: resume,
            cwd,
            ..common(false, model, false)
        }
    }

    #[test]
    fn resume_id_precedes_flags_and_prompt() {
        let args = build_args(
            &resumed(Some(CHAT_ID), None, Some("sonnet-4")),
            true,
            Some(CursorSandbox::Disabled),
            Some("hello"),
        );
        assert_eq!(
            args,
            vec![
                "--resume",
                CHAT_ID,
                "--model",
                "sonnet-4",
                "--plan",
                "--sandbox",
                "disabled",
                "hello",
            ]
        );
    }

    #[test]
    fn resume_without_prompt() {
        let args = build_args(&resumed(Some(CHAT_ID), None, None), false, None, None);
        assert_eq!(args, vec!["--resume", CHAT_ID]);
    }

    #[test]
    fn resume_passes_workspace_and_trust() {
        let mut fields = resumed(Some(CHAT_ID), Some(CWD), Some("sonnet-4"));
        fields.dangerously_skip_permissions = true;
        let args = build_args(&fields, true, None, Some("hi"));
        assert_eq!(
            args,
            vec![
                "--resume",
                CHAT_ID,
                "--workspace",
                CWD,
                "--trust",
                "--model",
                "sonnet-4",
                "--plan",
                "--yolo",
                "hi",
            ]
        );
    }

    #[test]
    fn fresh_spawn_with_cwd_passes_workspace_and_trust() {
        let args = build_args(&resumed(None, Some(CWD), None), false, None, Some("x"));
        assert_eq!(args, vec!["--workspace", CWD, "--trust", "x"]);
    }

    #[test]
    fn no_cwd_passes_neither_workspace_nor_trust() {
        let args = build_args(&resumed(Some(CHAT_ID), None, None), false, None, Some("x"));
        assert!(!args.contains(&"--workspace".to_string()), "{args:?}");
        assert!(!args.contains(&"--trust".to_string()), "{args:?}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_never_passes_sandbox_enabled() {
        let enabled = build_args(
            &common(false, None, false),
            false,
            Some(CursorSandbox::Enabled),
            Some("x"),
        );
        assert_eq!(enabled, vec!["x"]);
        let disabled = build_args(
            &common(false, None, false),
            false,
            Some(CursorSandbox::Disabled),
            Some("x"),
        );
        assert_eq!(disabled, vec!["--sandbox", "disabled", "x"]);
    }

    #[test]
    fn standalone_add_dirs_are_not_passed_to_cursor() {
        let extra = vec!["X:/dev/b".to_string()];
        let fields = CommonSpawnFields {
            add_dirs: &extra,
            cwd: Some("X:/dev/a"),
            ..common(false, None, false)
        };
        let args = build_args(&fields, false, None, None);
        assert!(!args.contains(&"--add-dir".to_string()), "{args:?}");
        assert_eq!(args, ["--workspace", "X:/dev/a", "--trust"]);
    }

    fn common(skip: bool, model: Option<&str>, has_injector: bool) -> CommonSpawnFields<'_> {
        CommonSpawnFields {
            dangerously_skip_permissions: skip,
            model,
            has_prompt_injector: has_injector,
            claude_session_id: None,
            resume_conversation: None,
            add_dirs: &[],
            cwd: None,
        }
    }

    #[test]
    fn single_repo_plan_mode_with_sandbox_and_prompt() {
        let args = build_args(
            &common(false, Some("sonnet-4"), false),
            true,
            Some(CursorSandbox::Disabled),
            Some("hello"),
        );
        assert_eq!(
            args,
            vec![
                "--model",
                "sonnet-4",
                "--plan",
                "--sandbox",
                "disabled",
                "hello",
            ]
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn sandbox_enabled_passes_outside_windows() {
        let args = build_args(
            &common(false, None, false),
            false,
            Some(CursorSandbox::Enabled),
            None,
        );
        assert_eq!(args, vec!["--sandbox", "enabled"]);
    }

    #[test]
    fn yolo_overrides_sandbox() {
        let args = build_args(
            &common(true, None, false),
            false,
            Some(CursorSandbox::Disabled),
            Some("go"),
        );
        assert!(args.contains(&"--yolo".to_string()));
        assert!(!args.contains(&"--sandbox".to_string()));
        assert_eq!(args.last(), Some(&"go".to_string()));
    }

    #[test]
    fn no_options_no_prompt_is_empty() {
        let args = build_args(&common(false, None, false), false, None, None);
        assert_eq!(args, [] as [std::string::String; 0]);
    }

    #[test]
    fn injector_present_omits_positional_prompt() {
        let args = build_args(
            &common(false, None, true),
            false,
            None,
            Some("would-be-prompt"),
        );
        assert_eq!(args, [] as [std::string::String; 0]);
    }

    #[test]
    fn plan_mode_without_sandbox_emits_just_plan() {
        let args = build_args(&common(false, None, false), true, None, Some("x"));
        assert_eq!(args, vec!["--plan", "x"]);
    }
}

/// [`create_chat`] against a `pwsh -NoProfile -Command` stand-in for
/// `cursor-agent`. The script defines `f` and ends with a call to it, so the
/// `create-chat` argument appended after the script becomes `f`'s argument.
#[cfg(all(test, windows))]
#[expect(clippy::expect_used, reason = "tests fail loudly on setup errors")]
mod create_chat_tests {
    use super::*;
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

    const CHAT_ID: &str = "3f1c2b9e-7a4d-4e8f-9b2a-6c5d4e3f2a1b";

    fn stand_in(body: &str) -> Vec<String> {
        vec![
            "-NoProfile".to_owned(),
            "-Command".to_owned(),
            format!("function f {{ {body} }}; f"),
        ]
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rt-create-chat-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("scratch folder");
        dir
    }

    fn is_alive(pid: u32) -> bool {
        let mut sys = System::new_with_specifics(
            RefreshKind::new().with_processes(ProcessRefreshKind::new()),
        );
        let pid = Pid::from_u32(pid);
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            false,
            ProcessRefreshKind::new(),
        );
        sys.process(pid).is_some()
    }

    #[tokio::test]
    async fn returns_the_printed_id_and_kills_the_process_tree() {
        let dir = scratch("tree");
        let pid_file = dir.join("child.pid");
        let body = format!(
            "if ($args[0] -ne 'create-chat') {{ exit 3 }}; \
             $c = Start-Process pwsh -ArgumentList '-NoProfile','-Command','Start-Sleep 60' \
             -PassThru -NoNewWindow; \
             Set-Content -LiteralPath '{}' $c.Id; '{CHAT_ID}'; Start-Sleep 60",
            pid_file.display()
        );

        let id = create_chat("pwsh", &stand_in(&body), &dir, &[], Duration::from_secs(8))
            .await
            .expect("the chat id");

        assert_eq!(id, CHAT_ID);
        let child: u32 = std::fs::read_to_string(&pid_file)
            .expect("the stand-in's child pid")
            .trim()
            .parse()
            .expect("a pid");
        let mut alive = is_alive(child);
        for _ in 0..30 {
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            alive = is_alive(child);
        }
        assert!(!alive, "the stand-in's child {child} outlived create_chat");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_dropped_call_kills_the_process_tree() {
        let dir = scratch("dropped");
        let pid_file = dir.join("child.pid");
        let body = format!(
            "$c = Start-Process pwsh -ArgumentList '-NoProfile','-Command','Start-Sleep 60' \
             -PassThru -NoNewWindow; \
             Set-Content -LiteralPath '{}' $c.Id; Start-Sleep 60",
            pid_file.display()
        );
        let args = stand_in(&body);
        let mut chat = Box::pin(create_chat(
            "pwsh",
            &args,
            &dir,
            &[],
            Duration::from_secs(30),
        ));
        let child_started = async {
            while !std::fs::read_to_string(&pid_file).is_ok_and(|pid| !pid.trim().is_empty()) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };

        let finished = tokio::select! {
            result = &mut chat => Some(result),
            _ = tokio::time::timeout(Duration::from_secs(8), child_started) => None,
        };
        assert!(finished.is_none(), "create_chat ended early: {finished:?}");
        tokio::time::sleep(Duration::from_millis(500)).await;
        drop(chat);

        let child: u32 = std::fs::read_to_string(&pid_file)
            .expect("the stand-in's child pid")
            .trim()
            .parse()
            .expect("a pid");
        let mut alive = is_alive(child);
        for _ in 0..30 {
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            alive = is_alive(child);
        }
        assert!(
            !alive,
            "the stand-in's child {child} outlived the dropped call"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn times_out_when_nothing_is_printed() {
        let dir = scratch("silent");
        let err = create_chat(
            "pwsh",
            &stand_in("Start-Sleep 60"),
            &dir,
            &[],
            Duration::from_millis(1500),
        )
        .await
        .expect_err("no id printed");
        assert!(err.to_string().contains("no chat id within"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn fails_when_the_first_line_is_not_a_uuid() {
        let dir = scratch("junk");
        let err = create_chat(
            "pwsh",
            &stand_in("'not-a-uuid'; Start-Sleep 60"),
            &dir,
            &[],
            Duration::from_secs(8),
        )
        .await
        .expect_err("junk printed");
        assert!(err.to_string().contains("not a chat id"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn runs_with_the_spawns_env_rows() {
        let dir = scratch("env");
        let env = vec![("RT_TEST_CREATE_CHAT_ID".to_owned(), CHAT_ID.to_owned())];
        let id = create_chat(
            "pwsh",
            &stand_in("$env:RT_TEST_CREATE_CHAT_ID; Start-Sleep 60"),
            &dir,
            &env,
            Duration::from_secs(8),
        )
        .await
        .expect("the chat id from the env row");
        assert_eq!(id, CHAT_ID);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
