//! Per-agent backend abstraction.
//!
//! Each AI CLI the daemon knows how to spawn is represented by an
//! [`AgentBackend`] implementation in its own submodule; today that is Claude
//! Code alone. Callers route through [`backend_for`] to fetch the trait object
//! for a given [`Agent`] variant.
//!
//! Adding an agent means: add a variant to [`protocol::Agent`] and
//! [`protocol::AgentOptions`], implement [`AgentBackend`] in a new submodule,
//! and extend [`backend_for`] / [`Agent::all`] to dispatch to it.
mod claude;
pub(crate) mod providers;

pub(crate) use claude::build_hook_settings;

use crate::session::SessionRegistry;
use protocol::{Agent, AgentOptions, SessionMember};
use std::path::Path;

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
    /// interactive spawn. Ignored by headless paths.
    pub claude_session_id: Option<&'a str>,
    /// Conversation the Claude backend resumes with `--resume`, in place of
    /// `--session-id` and any initial prompt.
    pub resume_conversation: Option<&'a str>,
    /// Directories a standalone target carries as extra folders. The Claude
    /// backend adds them with `--add-dir` after the extra members' worktrees;
    /// headless spawns ignore them.
    pub add_dirs: &'a [String],
    /// The settings file the Claude backend passes as `--settings` on an
    /// interactive spawn, fresh or resumed: the hooks file this daemon wrote
    /// for the session (see [`build_hook_settings`]). `None` for a spawn
    /// whose file could not be written, which stays on the terminal
    /// heuristic. Headless spawns never carry it.
    pub hook_settings: Option<&'a Path>,
}

/// Implemented once per supported CLI. Methods are designed so the caller
/// (server.rs / headless.rs) doesn't need to know which backend it is talking
/// to; routing happens at [`backend_for`].
pub trait AgentBackend: Send + Sync {
    /// Environment-variable name that overrides the default executable name.
    /// Example: `"RUSTLING_TULIP_CLAUDE"`.
    fn program_env_var(&self) -> &'static str;

    /// Fallback executable name when [`Self::program_env_var`] is unset.
    /// This is the name we look up on `PATH`.
    fn default_program(&self) -> &'static str;

    /// Whether this CLI supports headless (non-PTY, structured JSON) mode.
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
    /// this to parse their stream-json format and update `metrics` /
    /// `recent_actions` on the session record.
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

/// Look up the backend instance for a given [`Agent`] variant. Always
/// returns a static reference (backends are stateless).
#[must_use]
pub fn backend_for(agent: Agent) -> &'static dyn AgentBackend {
    match agent {
        Agent::Claude => &CLAUDE,
    }
}

/// Build a workspace-context prelude for the agent. Returns `Some` only when
/// the session has 2+ members. The note maps each member's repo name to its
/// per-session worktree path so the agent doesn't try to navigate to
/// original-repo paths referenced in `CLAUDE.md` / `AGENTS.md` that no
/// longer match where the session is rooted.
///
/// Claude delivers it via `--append-system-prompt` (invisible to the user).
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
