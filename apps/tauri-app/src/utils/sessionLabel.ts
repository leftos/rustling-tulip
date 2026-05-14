import type { SessionSnapshot } from "../types";

/// Name shown in sidebars / pane headers / pop-out window titles. Explicit app
/// renames win. Otherwise a meaningful terminal title emitted by the running
/// program becomes the display label, with the daemon label as the stable
/// repo/workspace fallback.
export function sessionDisplayLabel(s: SessionSnapshot): string {
  const userLabel = nonEmptyLabel(s.user_label);
  if (userLabel) return userLabel;
  const cwdLabel = currentCwdLabel(s);
  if (cwdLabel) return cwdLabel;
  const terminalTitle = displayableTerminalTitle(s);
  if (terminalTitle) return terminalTitle;
  const label = nonEmptyLabel(s.label);
  if (label) return label;
  return sessionRuntimeLabel(s) ?? s.id;
}

/// Tooltip keeps the display label connected to the stable daemon label and
/// raw terminal title when those differ.
export function sessionLabelTooltip(s: SessionSnapshot): string {
  const display = sessionDisplayLabel(s);
  const fallbackLabel = nonEmptyLabel(s.label);
  const terminalTitle = nonEmptyLabel(s.terminal_title);
  const lines = [display];
  if (fallbackLabel && fallbackLabel !== display) {
    lines.push(`Session: ${fallbackLabel}`);
  }
  if (terminalTitle && terminalTitle !== display) {
    lines.push(`Terminal title: ${terminalTitle}`);
  }
  const cwd = nonEmptyLabel(s.current_cwd);
  if (cwd) {
    lines.push(`Cwd: ${cwd}`);
  }
  return lines.join("\n");
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

function displayableTerminalTitle(s: SessionSnapshot): string | null {
  const title = nonEmptyLabel(s.terminal_title);
  if (!title || isNoisyShellTitle(title)) return null;
  return title;
}

function currentCwdLabel(s: SessionSnapshot): string | null {
  if (s.mode !== "plain_shell") return null;
  const cwd = nonEmptyLabel(s.current_cwd);
  if (!cwd) return null;
  return pathLeafName(cwd);
}

function isNoisyShellTitle(title: string): boolean {
  const normalized = title.trim().toLowerCase().replaceAll("\\", "/");
  const basename = normalized.split("/").at(-1) ?? normalized;
  return (
    basename === "cmd.exe" ||
    basename === "powershell.exe" ||
    basename === "pwsh.exe" ||
    basename === "bash.exe" ||
    basename === "sh.exe" ||
    normalized === "windows powershell" ||
    normalized === "administrator: windows powershell"
  );
}

function pathLeafName(path: string): string {
  const trimmed = path.replace(/[\\/]+$/g, "");
  const parts = trimmed.split(/[\\/]/);
  return parts[parts.length - 1] || path;
}
