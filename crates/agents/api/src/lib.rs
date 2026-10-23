//! The agent plugin seam.
//!
//! Each agent CLI the daemon knows how to spawn is an [`AgentPlugin`]: it
//! builds the CLI's argv from a [`SpawnContext`], names the CLI's executable,
//! writes the hook settings file its interactive spawns pass, and turns the
//! CLI's headless output into [`HeadlessEvent`]s the daemon applies to the
//! session. [`workspace_prelude`] is the workspace note a plugin hands its CLI
//! for a multi-member session.

use protocol::{AgentOptions, SessionMember, SessionStatus};
use std::path::Path;

/// Cross-agent spawn context bundled together so trait signatures stay
/// readable. Fields are borrowed from the daemon's in-flight spawn so the
/// caller doesn't have to clone anything to call a plugin method.
pub struct SpawnContext<'a> {
    pub dangerously_skip_permissions: bool,
    pub model: Option<&'a str>,
    /// `true` when the spawn has a prompt injector attached, in which case
    /// the plugin must NOT emit the initial prompt as a CLI arg — the
    /// injector delivers it through the PTY post-spawn instead. Ignored by
    /// headless paths.
    pub has_prompt_injector: bool,
    /// Conversation id the plugin passes its CLI on an interactive spawn
    /// (Claude's `--session-id`). Ignored by headless paths.
    pub conversation_id: Option<&'a str>,
    /// Conversation the CLI resumes (Claude's `--resume`), in place of the
    /// conversation id and any initial prompt.
    pub resume_conversation: Option<&'a str>,
    /// Directories a standalone target carries as extra folders. The plugin
    /// adds them after the extra members' worktrees; headless spawns ignore
    /// them.
    pub add_dirs: &'a [String],
    /// The settings file the plugin passes its CLI on an interactive spawn,
    /// fresh or resumed: the file the daemon wrote for the session from
    /// [`AgentPlugin::hook_settings_file`]. `None` for a spawn whose file
    /// could not be written, which stays on the terminal heuristic. Headless
    /// spawns never carry it.
    pub settings_file: Option<&'a Path>,
}

/// One change a plugin read from a line of its CLI's headless output, for the
/// daemon to apply to the session's record in order.
#[derive(Debug, Clone, PartialEq)]
pub enum HeadlessEvent {
    /// The session's new status.
    Status(SessionStatus),
    /// A line for the session's recent-actions list.
    RecentAction(String),
    /// Usage figures; each `Some` replaces the session's current figure and
    /// each `None` leaves it as it is.
    Metrics {
        cost_usd: Option<f64>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    },
}

/// Implemented once per supported CLI. Methods are designed so the daemon
/// doesn't need to know which plugin it is talking to; the registry maps an
/// agent to its plugin.
pub trait AgentPlugin: Send + Sync {
    /// Environment-variable name that overrides the default executable name.
    /// Example: `"RUSTLING_TULIP_CLAUDE"`.
    fn program_env_var(&self) -> &'static str;

    /// Fallback executable name when [`Self::program_env_var`] is unset.
    /// This is the name looked up on `PATH`.
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
        common: &SpawnContext,
        members: &[SessionMember],
        initial_prompt: Option<&str>,
    ) -> Vec<String>;

    /// Build the argv passed to the CLI for a headless session. Returns
    /// an empty vec for plugins that do not support headless mode (callers
    /// must check [`Self::supports_headless`] before invoking).
    fn build_headless_args(
        &self,
        _opts: &AgentOptions,
        _common: &SpawnContext,
        _members: &[SessionMember],
        _initial_prompt: &str,
    ) -> Vec<String> {
        Vec::new()
    }

    /// Read one raw stdout line emitted by a headless child into the changes
    /// it makes to the session, in order. A line that carries none, or that
    /// does not parse, yields no events.
    fn parse_headless_line(&self, _line: &str) -> Vec<HeadlessEvent> {
        Vec::new()
    }

    /// The contents of the hook settings file an interactive spawn of this
    /// CLI passes, or `None` for a CLI that takes none. `tracer_exe` is the
    /// cached tracer whose `hook` shim the hooks run, `session_id` the
    /// session they report for, and `config_dir` the daemon's config dir,
    /// where the shim reads the daemon's port and auth token.
    fn hook_settings_file(
        &self,
        _tracer_exe: &Path,
        _session_id: &str,
        _config_dir: &Path,
    ) -> Option<String> {
        None
    }

    /// Resolve the executable path (the env-var override, or the default).
    /// Concrete `which`/shim resolution lives in the daemon's
    /// `resolve_agent_program`.
    fn resolve_program(&self) -> String {
        std::env::var(self.program_env_var()).unwrap_or_else(|_| self.default_program().to_string())
    }
}

/// Build a workspace-context prelude for the agent. Returns `Some` only when
/// the session has 2+ members. The note maps each member's repo name to its
/// per-session worktree path so the agent doesn't try to navigate to
/// original-repo paths referenced in its `instruction_files` that no longer
/// match where the session is rooted.
#[must_use]
pub fn workspace_prelude(members: &[SessionMember], instruction_files: &[&str]) -> Option<String> {
    if members.len() < 2 {
        return None;
    }
    let mut out = format!(
        "Workspace member paths for this session (use these for cross-repo \
         file access — they override any absolute paths referenced in \
         {}):\n",
        instruction_files.join(" / ")
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

    fn member(name: &str, path: &str) -> SessionMember {
        SessionMember {
            repo_id: format!("id-{name}"),
            repo_name: name.to_string(),
            branch: "main".to_string(),
            worktree_path: path.to_string(),
        }
    }

    #[test]
    fn workspace_prelude_names_the_instruction_files() {
        let files = ["CLAUDE.md", "AGENTS.md"];
        let two = [member("a", "X:/wt/a"), member("longer", "X:/wt/longer")];
        assert_eq!(
            workspace_prelude(&two, &files).as_deref(),
            Some(
                "Workspace member paths for this session (use these for cross-repo file access \
                 — they override any absolute paths referenced in CLAUDE.md / AGENTS.md):\n  \
                 a       ->  X:/wt/a\n  longer  ->  X:/wt/longer\n"
            )
        );
        assert_eq!(workspace_prelude(&two[..1], &files), None);
    }
}
