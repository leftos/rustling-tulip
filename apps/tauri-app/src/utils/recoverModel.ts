// Pure rules behind the "Recover sessions" dialog and its sidebar badge. No
// React here, so every rule is unit-tested without a DOM.

import type {
  ConversationCandidate,
  RecoverHow,
  RepoEntry,
  SessionEndReason,
  SessionHistoryItem,
  WorkspaceEntry,
} from "../types";

/// Consecutive lost sessions whose end times sit this close together were
/// taken down by the same event and are listed as one group.
export const BURST_WINDOW_MS = 60_000;

export const OTHER_GROUP_TITLE = "Other recent sessions";

export interface RecoverGroup {
  key: string;
  title: string;
  items: SessionHistoryItem[];
}

export interface RecoverAsOption {
  key: string;
  label: string;
  how: RecoverHow;
}

function endedMs(item: SessionHistoryItem): number {
  return Date.parse(item.entry.ended_at);
}

function isLost(item: SessionHistoryItem): boolean {
  return item.entry.end.type === "tracer_lost";
}

/// Headless runs have no terminal to bring back, so the dialog never lists them.
export function isListed(item: SessionHistoryItem): boolean {
  return item.entry.mode !== "headless";
}

/// Listed items, newest `ended_at` first.
export function listedItems(items: SessionHistoryItem[]): SessionHistoryItem[] {
  return items.filter(isListed).sort((a, b) => endedMs(b) - endedMs(a));
}

/// Lost sessions nobody has recovered yet: the sidebar badge's count.
export function recoverBadgeCount(items: SessionHistoryItem[]): number {
  return items.filter(
    (i) => isListed(i) && isLost(i) && i.entry.recovered_at === null,
  ).length;
}

/// The badge text: nothing at 0, "99+" above 99.
export function badgeLabel(count: number): string | null {
  if (count <= 0) return null;
  return count > 99 ? "99+" : String(count);
}

/// "HH:MM" in local time.
export function formatClock(date: Date): string {
  const hh = String(date.getHours()).padStart(2, "0");
  const mm = String(date.getMinutes()).padStart(2, "0");
  return `${hh}:${mm}`;
}

function lostGroupTitle(items: SessionHistoryItem[]): string {
  const first = items[0];
  const at = first ? formatClock(new Date(first.entry.ended_at)) : "";
  const noun = items.length === 1 ? "session" : "sessions";
  return `${items.length} ${noun} lost at ${at}`;
}

/**
 * Group the listed items. Walking newest first, a lost session within
 * BURST_WINDOW_MS of the lost session before it joins that session's group;
 * any other lost session starts a new one. Everything that was not lost goes
 * into one trailing "Other recent sessions" group.
 */
export function groupHistory(items: SessionHistoryItem[]): RecoverGroup[] {
  const bursts: SessionHistoryItem[][] = [];
  const others: SessionHistoryItem[] = [];
  let previousLost: SessionHistoryItem | null = null;
  for (const item of listedItems(items)) {
    if (!isLost(item)) {
      others.push(item);
      previousLost = null;
      continue;
    }
    const current = bursts[bursts.length - 1];
    const joins =
      current !== undefined &&
      previousLost !== null &&
      endedMs(previousLost) - endedMs(item) <= BURST_WINDOW_MS;
    if (joins) {
      current.push(item);
    } else {
      bursts.push([item]);
    }
    previousLost = item;
  }
  const groups: RecoverGroup[] = bursts.map((burst) => ({
    key: `lost-${burst[0]?.entry.session_id ?? ""}`,
    title: lostGroupTitle(burst),
    items: burst,
  }));
  if (others.length > 0) {
    groups.push({ key: "other", title: OTHER_GROUP_TITLE, items: others });
  }
  return groups;
}

/// Plain shells and folder-only sessions (no spawn config) let the user pick
/// how they come back; a Claude session with a spawn config always returns as one.
export function hasRecoverAsChoice(item: SessionHistoryItem): boolean {
  return item.entry.mode === "plain_shell" || item.entry.spawn_config === null;
}

/// Why a row cannot be recovered, or null when it can.
export function disabledReason(item: SessionHistoryItem): string | null {
  const recoveredAt = item.entry.recovered_at;
  if (recoveredAt !== null) {
    return `recovered ${formatClock(new Date(recoveredAt))}`;
  }
  if (item.candidates.length === 0 && item.entry.mode !== "plain_shell") {
    return "no conversation found";
  }
  return null;
}

/// Lost, unrecovered, recoverable rows start ticked.
export function isPreTicked(item: SessionHistoryItem): boolean {
  return isLost(item) && disabledReason(item) === null;
}

/// The folder the session ran in: its last reported cwd, else where it started,
/// else its first member's folder — the same order the daemon matches on.
export function historyFolder(item: SessionHistoryItem): string {
  const { current_cwd, primary_cwd, members } = item.entry;
  return current_cwd ?? primary_cwd ?? members[0]?.worktree_path ?? "";
}

function folderName(path: string): string {
  const parts = path.split(/[\\/]+/).filter((p) => p.length > 0);
  return parts[parts.length - 1] ?? path;
}

/**
 * The "Recover as" choices for a row, in display order. The first is the
 * default: a Claude option when there is one, else the shell. A row with no
 * conversation to resume gets only a plain shell, since there is nothing for
 * a Claude session to pick up.
 */
export function recoverAsOptions(
  item: SessionHistoryItem,
  repos: RepoEntry[],
): RecoverAsOption[] {
  const folder = historyFolder(item);
  if (item.candidates.length === 0) {
    return [{ key: "shell", label: "Plain shell", how: { type: "shell" } }];
  }
  const options: RecoverAsOption[] = [];
  if (item.folder_repo_id !== null) {
    const repo = repos.find((r) => r.id === item.folder_repo_id);
    options.push({
      key: "claude",
      label: `Claude session in ${repo?.name ?? folderName(folder)}`,
      how: { type: "claude" },
    });
  } else if (item.folder_is_git_repo) {
    options.push({
      key: "register",
      label: `Register ${folder} as a repo, then Claude session`,
      how: { type: "register_repo_then_claude", path: folder },
    });
  }
  options.push({
    key: "shell",
    label: "Shell running claude --resume",
    how: { type: "shell" },
  });
  return options;
}

/// The row's title: its label, or the folder when the label is empty.
export function historyLabel(item: SessionHistoryItem): string {
  return item.entry.label.trim() !== "" ? item.entry.label : historyFolder(item);
}

/// Where the session ran: its workspace, else its repo, else its folder.
export function historyWhere(
  item: SessionHistoryItem,
  repos: RepoEntry[],
  workspaces: WorkspaceEntry[],
): string {
  const { entry } = item;
  if (entry.workspace_id !== null) {
    const ws = workspaces.find((w) => w.id === entry.workspace_id);
    if (ws) return ws.name;
  }
  const member = entry.members[0];
  if (member) {
    const repo = repos.find((r) => r.id === member.repo_id);
    return repo?.name ?? member.repo_name;
  }
  if (item.folder_repo_id !== null) {
    const repo = repos.find((r) => r.id === item.folder_repo_id);
    if (repo) return repo.name;
  }
  return historyFolder(item);
}

export function endReasonLabel(end: SessionEndReason): string {
  switch (end.type) {
    case "tracer_lost":
      return "lost";
    case "exited":
      return `exited (code ${end.code})`;
    case "stopped_by_user":
      return "stopped";
    case "daemon_shutdown":
      return "daemon shut down";
    default:
      return "ended";
  }
}

/// "HH:MM" for today, else "<short date> HH:MM".
export function formatEnded(ended: Date, now: Date): string {
  const sameDay = ended.toDateString() === now.toDateString();
  if (sameDay) return formatClock(ended);
  const day = ended.toLocaleDateString(undefined, {
    month: "short",
    day: "numeric",
  });
  return `${day} ${formatClock(ended)}`;
}

export function relativeTime(then: Date, now: Date): string {
  const delta = Math.floor((now.getTime() - then.getTime()) / 1000);
  if (delta < 60) return "just now";
  if (delta < 3600) return `${Math.floor(delta / 60)}m ago`;
  if (delta < 86400) return `${Math.floor(delta / 3600)}h ago`;
  return `${Math.floor(delta / 86400)}d ago`;
}

/// "<title or id prefix> · <relative last_active>".
export function candidateLabel(
  candidate: ConversationCandidate,
  now: Date,
): string {
  const title =
    candidate.title !== null && candidate.title.trim() !== ""
      ? candidate.title
      : candidate.id.slice(0, 8);
  return `${title} · ${relativeTime(new Date(candidate.last_active), now)}`;
}
