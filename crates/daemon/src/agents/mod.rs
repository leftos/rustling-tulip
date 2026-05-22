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
use protocol::{Agent, AgentOptions, SessionMember};

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
