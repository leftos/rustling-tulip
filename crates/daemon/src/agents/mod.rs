//! Per-agent backend abstraction.
//!
//! Each AI CLI the daemon knows how to spawn (claude, codex, …) is represented
//! by an [`AgentBackend`] implementation in its own submodule. Callers route
//! through [`backend_for`] to fetch the trait object for a given [`Agent`]
//! variant — there is no per-agent branching in `server.rs` or `headless.rs`
//! anymore.
//!
//! Adding a third agent means: add a variant to [`protocol::Agent`] and
//! [`protocol::AgentOptions`], implement [`AgentBackend`] in a new submodule,
//! and extend [`backend_for`] / [`Agent::all`] to dispatch to it.
mod claude;
mod codex;
mod cursor;

use crate::session::SessionRegistry;
use futures::future::BoxFuture;
use protocol::{Agent, AgentOptions, SessionMember};
use std::path::Path;
use std::time::Duration;
use tracing::{info, warn};

/// How long a fresh spawn waits for its agent to pre-create a conversation.
const PRECREATE_TIMEOUT: Duration = Duration::from_secs(5);

/// How an interactive spawn's agent is launched, for running it once before
/// the spawn to pre-create a conversation.
pub struct Launch<'a> {
    /// The resolved program, as the spawn runs it.
    pub program: &'a str,
    /// Arguments the resolved program takes before the agent's own.
    pub prepend: &'a [String],
    /// The session's working directory.
    pub cwd: &'a Path,
    /// The environment the spawned agent gets.
    pub env: &'a [(String, String)],
}

/// The agent conversation an interactive spawn opens and records.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SpawnConversation {
    /// A conversation pre-created for this spawn.
    pub created: Option<String>,
    /// The conversation the argv resumes: the pre-created one, else the
    /// requested one.
    pub resume: Option<String>,
    /// The id the session records as its own agent's conversation.
    pub own: Option<String>,
}

impl SpawnConversation {
    /// The conversation a spawn of `agent` opens, given the conversation it
    /// was asked to resume and the one pre-created for it.
    #[must_use]
    pub fn new(agent: Agent, resume: Option<&str>, created: Option<String>) -> Self {
        let own = backend_for(agent).own_conversation_at_spawn(resume, created.as_deref());
        let resume = created.clone().or_else(|| resume.map(str::to_owned));
        Self {
            created,
            resume,
            own,
        }
    }
}

/// Decide the conversation an interactive spawn of `agent` opens: a fresh
/// run of an agent that pre-creates its conversation does so first (with a
/// [`PRECREATE_TIMEOUT`] wait), and a failure is logged and leaves the spawn
/// as it would be without one.
pub async fn spawn_conversation(
    agent: Agent,
    resume: Option<&str>,
    launch: &Launch<'_>,
) -> SpawnConversation {
    let pending = if resume.is_none() {
        backend_for(agent).precreate_conversation(launch)
    } else {
        None
    };
    let created = match pending {
        None => None,
        Some(pending) => match pending.await {
            Ok(id) => {
                info!(agent = agent.as_label(), conversation = %id, cwd = %launch.cwd.display(), "pre-created the agent's conversation");
                Some(id)
            }
            Err(err) => {
                warn!(
                    agent = agent.as_label(),
                    error = %format!("{err:#}"),
                    cwd = %launch.cwd.display(),
                    "pre-creating the agent's conversation failed; spawning without a recorded conversation"
                );
                None
            }
        },
    };
    SpawnConversation::new(agent, resume, created)
}

/// Cross-agent context bundled together so trait signatures stay readable.
/// Fields are borrowed from the in-flight [`SpawnArgs`] so the caller doesn't
/// have to clone anything to call a backend method.
pub struct CommonSpawnFields<'a> {
    pub dangerously_skip_permissions: bool,
    pub model: Option<&'a str>,
    /// `true` when the spawn has a [`PromptInjector`] attached, in which case
    /// the backend must NOT emit the initial prompt as a CLI arg — the
    /// injector delivers it through the PTY post-spawn instead. Ignored by
    /// headless paths.
    pub has_prompt_injector: bool,
    /// Conversation id the Claude backend passes as `--session-id` on an
    /// interactive spawn. Ignored by headless paths and by other backends.
    pub claude_session_id: Option<&'a str>,
    /// Conversation the Claude backend resumes with `--resume` and the Codex
    /// backend with `codex resume`, in place of `--session-id` (Claude) and
    /// any initial prompt. The Cursor backend passes `--resume <id>` ahead of
    /// its other flags and keeps the initial prompt.
    pub resume_conversation: Option<&'a str>,
    /// Directories the Claude backend adds with `--add-dir` after the extra
    /// members' worktrees: a standalone target's `add_dirs`. Ignored by other
    /// backends.
    pub add_dirs: &'a [String],
    /// The session's working directory, which the Codex backend passes as
    /// `-C <cwd>` with a `-c` override marking it trusted on every
    /// interactive spawn and resume, and the Cursor backend as
    /// `--workspace <cwd> --trust` on every spawn. Ignored by the Claude
    /// backend.
    pub cwd: Option<&'a str>,
}

/// Implemented once per supported CLI. Methods are designed so the caller
/// (server.rs / headless.rs) doesn't need to know which backend it is talking
/// to; routing happens entirely at [`backend_for`].
pub trait AgentBackend: Send + Sync {
    /// Environment-variable name that overrides the default executable name.
    /// Example: `"RUSTLING_TULIP_CLAUDE"`.
    fn program_env_var(&self) -> &'static str;

    /// Fallback executable name when [`Self::program_env_var`] is unset.
    /// This is the name we look up on `PATH`.
    fn default_program(&self) -> &'static str;

    /// Whether this CLI supports headless (non-PTY, structured JSON) mode.
    /// PR1: only claude returns `true`.
    fn supports_headless(&self) -> bool {
        false
    }

    /// Build the argv passed to the CLI for an interactive (PTY-attached)
    /// session. Does not include the executable name itself — that is
    /// resolved by [`Self::resolve_program`].
    fn build_interactive_args(
        &self,
        opts: &AgentOptions,
        common: &CommonSpawnFields,
        members: &[SessionMember],
        initial_prompt: Option<&str>,
    ) -> Vec<String>;

    /// Build the argv passed to the CLI for a headless session. Returns
    /// an empty vec for backends that do not support headless mode (callers
    /// must check [`Self::supports_headless`] before invoking).
    fn build_headless_args(
        &self,
        _opts: &AgentOptions,
        _common: &CommonSpawnFields,
        _members: &[SessionMember],
        _initial_prompt: &str,
    ) -> Vec<String> {
        Vec::new()
    }

    /// Process one raw stdout line emitted by a headless child. The default
    /// implementation does nothing; backends with headless support override
    /// this to parse their stream-json/exec-json format and update
    /// `metrics` / `recent_actions` on the session record.
    fn handle_headless_line(
        &self,
        _registry: &SessionRegistry,
        _session_id: &str,
        _raw_line: &str,
    ) {
    }

    /// Resolve the executable path (the env-var override, or the default).
    /// Concrete `which`/shim resolution lives in `server.rs::resolve_agent_program`.
    fn resolve_program(&self) -> String {
        std::env::var(self.program_env_var()).unwrap_or_else(|_| self.default_program().to_string())
    }

    /// The id an interactive spawn records as the session's own agent
    /// conversation, given the conversation it resumes and the one
    /// pre-created for it. The default records none (Claude's id is
    /// `claude_session_id`).
    fn own_conversation_at_spawn(
        &self,
        _resume: Option<&str>,
        _created: Option<&str>,
    ) -> Option<String> {
        None
    }

    /// Pre-create an empty conversation for a fresh interactive spawn, so
    /// the session knows its id up front. `None` for agents that don't.
    fn precreate_conversation<'a>(
        &self,
        _launch: &'a Launch<'a>,
    ) -> Option<BoxFuture<'a, anyhow::Result<String>>> {
        None
    }
}

static CLAUDE: claude::ClaudeBackend = claude::ClaudeBackend;
static CODEX: codex::CodexBackend = codex::CodexBackend;
static CURSOR: cursor::CursorBackend = cursor::CursorBackend;

/// Look up the backend instance for a given [`Agent`] variant. Always
/// returns a static reference (backends are stateless).
#[must_use]
pub fn backend_for(agent: Agent) -> &'static dyn AgentBackend {
    match agent {
        Agent::Claude => &CLAUDE,
        Agent::Codex => &CODEX,
        Agent::Cursor => &CURSOR,
    }
}

/// Build a workspace-context prelude for the agent. Returns `Some` only when
/// the session has 2+ members. The note maps each member's repo name to its
/// per-session worktree path so the agent doesn't try to navigate to
/// original-repo paths referenced in `CLAUDE.md` / `AGENTS.md` that no
/// longer match where the session is rooted.
///
/// Claude delivers it via `--append-system-prompt` (invisible to the user);
/// codex prepends it to the positional prompt (no system-prompt flag). Both
/// backends call this helper.
pub(crate) fn workspace_prelude(members: &[SessionMember]) -> Option<String> {
    if members.len() < 2 {
        return None;
    }
    let mut out = String::from(
        "Workspace member paths for this session (use these for cross-repo \
         file access — they override any absolute paths referenced in \
         CLAUDE.md / AGENTS.md):\n",
    );
    let name_width = members.iter().map(|m| m.repo_name.len()).max().unwrap_or(0);
    for m in members {
        use std::fmt::Write as _;
        // Width-padded for visual alignment in the agent's view; failure
        // here would mean OOM during string formatting, which we treat as
        // unreachable for a few dozen members at most.
        let _ = writeln!(
            out,
            "  {name:<width$}  ->  {path}",
            name = m.repo_name,
            width = name_width,
            path = m.worktree_path,
        );
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_conversation_claude_untouched() {
        let resumed = SpawnConversation::new(Agent::Claude, Some("claude-1"), None);
        assert_eq!(resumed.own, None);
        assert_eq!(resumed.resume.as_deref(), Some("claude-1"));
        assert_eq!(
            SpawnConversation::new(Agent::Claude, None, None),
            SpawnConversation::default()
        );
    }

    #[test]
    fn own_conversation_codex_resumed_keeps_id() {
        let conversation = SpawnConversation::new(Agent::Codex, Some("codex-1"), None);
        assert_eq!(conversation.own.as_deref(), Some("codex-1"));
        assert_eq!(conversation.resume.as_deref(), Some("codex-1"));
    }

    #[test]
    fn own_conversation_codex_fresh_has_none() {
        assert_eq!(
            SpawnConversation::new(Agent::Codex, None, None),
            SpawnConversation::default()
        );
    }

    #[test]
    fn own_conversation_cursor_uses_created_id() {
        let fresh = SpawnConversation::new(Agent::Cursor, None, Some("chat-new".to_owned()));
        assert_eq!(fresh.own.as_deref(), Some("chat-new"));
        assert_eq!(fresh.resume.as_deref(), Some("chat-new"));
        let resumed = SpawnConversation::new(Agent::Cursor, Some("chat-old"), None);
        assert_eq!(
            resumed.own.as_deref(),
            Some("chat-old"),
            "a resumed Cursor spawn keeps the recorded chat"
        );
        assert_eq!(resumed.resume.as_deref(), Some("chat-old"));
    }

    #[test]
    fn own_conversation_cursor_failed_create_has_none() {
        assert_eq!(
            SpawnConversation::new(Agent::Cursor, None, None),
            SpawnConversation::default()
        );
    }
}
