import type { SessionSnapshot } from "../types";

/// Name shown in sidebars / pane headers / pop-out window titles. The daemon
/// label is already the effective identity: explicit user label when one is
/// set, otherwise the canonical repo/workspace label. Terminal-emitted OSC
/// titles stay secondary because shells often publish noisy process titles.
export function sessionDisplayLabel(s: SessionSnapshot): string {
  const label = nonEmptyLabel(s.label);
  if (label) return label;
  return sessionRuntimeLabel(s) ?? s.id;
}

/// Tooltip surfaces secondary runtime title context without letting it replace
/// the stable primary label.
export function sessionLabelTooltip(s: SessionSnapshot): string {
  const display = sessionDisplayLabel(s);
  const terminalTitle = nonEmptyLabel(s.terminal_title);
  if (!terminalTitle || terminalTitle === display) return display;
  return `${display}\nTerminal title: ${terminalTitle}`;
}

/// Short token identifying the user-facing runtime: "claude" / "codex" for
/// agent sessions, "pwsh" / "powershell" / "cmd" / "bash" / "sh" for plain
/// shells.
export function sessionRuntimeLabel(s: SessionSnapshot): string | null {
  if (s.mode !== "plain_shell") return s.agent;
  if (s.program_name) return s.program_name;
  return null;
}

function nonEmptyLabel(value: string | null | undefined): string | null {
  const trimmed = value?.trim();
  return trimmed ? trimmed : null;
}
