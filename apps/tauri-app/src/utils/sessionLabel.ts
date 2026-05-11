import type { SessionSnapshot } from "../types";

/// Name shown in sidebars / pane headers / pop-out window titles. The
/// OSC window title (`terminal_title`) wins whenever a process has
/// emitted one — matches what VS Code / Windows Terminal / iTerm /
/// every other terminal host does. Covers Claude's `/rename`, codex's
/// equivalent, *and* shells that emit a meaningful title (powershell's
/// `$Host.UI.RawUI.WindowTitle = ...`, bash's `PROMPT_COMMAND` setting
/// `\033]0;...\007`, etc.). When no title has been emitted, falls back
/// to the daemon-curated `<repo>:<branch>` label.
///
/// Pre-iter-51 this was tooltip-only (iter 4 demoted OSC titles after
/// `cmd.exe` clobbered canonical labels at the source). Iter 51 keeps
/// the canonical label visible in the tooltip so nothing is lost, and
/// promotes the OSC title to primary regardless of session mode.
export function sessionDisplayLabel(s: SessionSnapshot): string {
  return s.terminal_title ?? s.label;
}

/// Tooltip combining the display label and the canonical spawn-time
/// label when they differ, so a renamed session still surfaces the
/// `<repo>:<branch>` origin on hover.
export function sessionLabelTooltip(s: SessionSnapshot): string {
  const display = sessionDisplayLabel(s);
  if (display === s.label) return s.label;
  return `${display}\n${s.label}`;
}

/// Short token identifying what's running in this session — "claude" /
/// "codex" for agent sessions, "pwsh" / "powershell" / "cmd" / "bash" /
/// "sh" for plain shells. Falls back to the `agent` field for sessions
/// reattached from a pre-iter-51 sidecar that didn't write
/// `program_name`. Returns null only when nothing is knowable (which
/// shouldn't happen in practice on a fresh spawn).
export function sessionRuntimeLabel(s: SessionSnapshot): string | null {
  if (s.program_name) return s.program_name;
  // Pre-iter-51 fallback: orphan reattach without program_name. For
  // plain-shell sessions we have no information, so return null. For
  // agent sessions the protocol's `agent` field gives us the answer.
  if (s.mode !== "plain_shell") return s.agent;
  return null;
}
