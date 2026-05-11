//! Headless `claude --print --output-format stream-json` driver.
//!
//! Unlike interactive mode, headless sessions do not need a PTY — Claude Code
//! emits structured JSON over stdout. We line-buffer the stream, parse each
//! event, and update the session record's `metrics` + `recent_actions`.

use crate::orphan;
use crate::paths::Dirs;
use crate::scrollback;
use crate::session::{SessionRegistry, push_recent_action};
use anyhow::{Context as _, anyhow};
use protocol::SessionStatus;
use serde::Deserialize;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex as AsyncMutex;
use tracing::{debug, warn};

#[derive(Debug, Clone)]
pub struct HeadlessSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

pub struct HeadlessHandle {
    child: AsyncMutex<Option<Child>>,
    pid: Option<u32>,
}

impl HeadlessHandle {
    pub async fn kill(&self) {
        let mut guard = self.child.lock().await;
        if let Some(mut child) = guard.take()
            && let Err(err) = child.start_kill()
        {
            warn!(?err, "failed to kill headless child");
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }
}

/// Spawn the headless child and wire its stdout into the registry.
/// Returns immediately; the parser runs in a background task. `dirs` is used
/// to clean up the orphan-meta sidecar when the child exits.
pub fn spawn(
    spec: &HeadlessSpec,
    registry: &Arc<SessionRegistry>,
    session_id: String,
    dirs: Dirs,
) -> anyhow::Result<Arc<HeadlessHandle>> {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {} for headless mode", spec.program))?;
    let pid = child.id();
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;

    let handle = Arc::new(HeadlessHandle {
        child: AsyncMutex::new(Some(child)),
        pid,
    });

    // stdout: parse stream-json line by line. Also persist each line to the
    // scrollback file so the headless event log can be replayed on reattach.
    let registry_for_stdout = Arc::clone(registry);
    let session_for_stdout = session_id.clone();
    let dirs_for_stdout = dirs.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        loop {
            match reader.next_line().await {
                Ok(Some(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let mut chunk = line.clone().into_bytes();
                    chunk.push(b'\n');
                    scrollback::append(&dirs_for_stdout, &session_for_stdout, &chunk);
                    handle_stream_event(&registry_for_stdout, &session_for_stdout, &line);
                }
                Ok(None) => break,
                Err(err) => {
                    warn!(?err, "headless stdout read error");
                    break;
                }
            }
        }
    });

    // stderr: collect into recent_actions.
    let registry_for_stderr = Arc::clone(registry);
    let session_for_stderr = session_id.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if !line.trim().is_empty() {
                registry_for_stderr.update(&session_for_stderr, |rec| {
                    push_recent_action(rec, format!("stderr: {line}"));
                });
            }
        }
    });

    // Exit waiter.
    let registry_for_exit = Arc::clone(registry);
    let session_for_exit = session_id;
    let handle_for_exit = Arc::clone(&handle);
    let dirs_for_exit = dirs;
    tokio::spawn(async move {
        let mut guard = handle_for_exit.child.lock().await;
        let exit = if let Some(mut child) = guard.take() {
            child.wait().await.ok().and_then(|s| s.code())
        } else {
            None
        };
        registry_for_exit.update(&session_for_exit, |rec| {
            rec.status = SessionStatus::Stopped;
            rec.exit_code = exit;
            push_recent_action(
                rec,
                format!("exited with code {}", exit.map_or_else(|| "?".into(), |c| c.to_string())),
            );
        });
        registry_for_exit.fan_out_attention(
            session_for_exit.clone(),
            protocol::AttentionReason::Stopped,
        );
        orphan::try_delete_meta(&dirs_for_exit, &session_for_exit);
    });

    Ok(handle)
}

fn handle_stream_event(registry: &SessionRegistry, session_id: &str, raw_line: &str) {
    let parsed = match serde_json::from_str::<StreamEvent>(raw_line) {
        Ok(p) => p,
        Err(err) => {
            debug!(?err, line = raw_line, "unrecognized stream-json event");
            return;
        }
    };
    registry.update(session_id, |rec| {
        match parsed {
            StreamEvent::System { subtype, .. } => {
                rec.status = SessionStatus::Working;
                push_recent_action(rec, format!("system: {subtype}"));
            }
            StreamEvent::Assistant { message } => {
                if let Some(content) = message.and_then(|m| m.content) {
                    for block in content {
                        match block {
                            ContentBlock::ToolUse { name, .. } => {
                                push_recent_action(rec, format!("tool: {name}"));
                            }
                            ContentBlock::Text { text } => {
                                let trimmed = text.trim();
                                if !trimmed.is_empty() {
                                    let snippet =
                                        trimmed.chars().take(120).collect::<String>();
                                    push_recent_action(rec, format!("assistant: {snippet}"));
                                }
                            }
                            ContentBlock::Other => {}
                        }
                    }
                }
            }
            StreamEvent::Result {
                total_cost_usd,
                usage,
                subtype,
                ..
            } => {
                if let Some(cost) = total_cost_usd {
                    rec.metrics.cost_usd = cost;
                }
                if let Some(u) = usage {
                    rec.metrics.input_tokens = u.input_tokens.unwrap_or(0);
                    rec.metrics.output_tokens = u.output_tokens.unwrap_or(0);
                }
                rec.status = SessionStatus::Stopped;
                push_recent_action(rec, format!("result: {subtype}"));
            }
            StreamEvent::User {} | StreamEvent::Other => {}
        }
    });
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamEvent {
    System {
        #[serde(default)]
        subtype: String,
    },
    Assistant {
        #[serde(default)]
        message: Option<AssistantMessage>,
    },
    User {},
    Result {
        #[serde(default)]
        subtype: String,
        #[serde(default)]
        total_cost_usd: Option<f64>,
        #[serde(default)]
        usage: Option<UsageInfo>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct AssistantMessage {
    #[serde(default)]
    content: Option<Vec<ContentBlock>>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        #[serde(default)]
        text: String,
    },
    ToolUse {
        #[serde(default)]
        name: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct UsageInfo {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
}
