import type { DaemonMessage, WorkShell, WorkSubagent } from "./protocol";

/**
 * The work still running beside a session's chat, as the daemon's last
 * `chat_work` reported it, and the one line the session screen words it as.
 * A port of the desktop's running-work strip (`RunningWork::strip` and
 * `next_repaint` in `apps/native/src/chat.rs`): the same words, separators
 * and ages, without the strip's per-item tooltips.
 */
export interface RunningWork {
  readonly subagents: readonly WorkSubagent[];
  readonly shells: readonly WorkShell[];
  /** When the earliest pending wakeup fires; null when none is pending. */
  readonly wakeupAt: string | null;
}

/** Nothing running and no wakeup pending. */
export const NO_WORK: RunningWork = { subagents: [], shells: [], wakeupAt: null };

/** What separates the line's parts. */
export const WORK_SEPARATOR = " · ";

/** A shell's command or description is cut to this many characters. */
const COMMAND_CAP = 24;

/** The repaint delay while an age or countdown reads in seconds. */
const FAST_REPAINT_MS = 1000;

/** The repaint delay otherwise. */
const SLOW_REPAINT_MS = 30_000;

type ChatWork = Extract<DaemonMessage, { type: "chat_work" }>;

/** The work a `chat_work` message reports. */
export function workOf(message: ChatWork): RunningWork {
  return {
    subagents: message.subagents ?? [],
    shells: message.shells ?? [],
    wakeupAt: message.wakeup_at ?? null,
  };
}

/** Whether nothing runs and no wakeup is pending: no line shows. */
export function isIdle(work: RunningWork): boolean {
  return work.subagents.length === 0 && work.shells.length === 0 && work.wakeupAt === null;
}

/** The line as of `now` (ms since the epoch), or null when nothing runs. */
export function workLine(work: RunningWork, now: number): string | null {
  const parts: string[] = [];
  if (work.subagents.length > 0) {
    parts.push(counted(work.subagents.length, "subagent"));
  }
  if (work.shells.length > 0) {
    parts.push(shellsText(work.shells, now));
  }
  const at = stamp(work.wakeupAt);
  if (at !== null) {
    parts.push(wakeupText(now, at));
  }
  return parts.length === 0 ? null : parts.join(WORK_SEPARATOR);
}

/**
 * How many ms after `now` the line next needs a repaint, or null when it is
 * hidden: a second while an age or countdown it shows reads in seconds, else
 * 30 seconds or the moment the soonest wakeup's countdown starts reading in
 * seconds, whichever comes first.
 */
export function nextRepaint(work: RunningWork, now: number): number | null {
  if (isIdle(work)) {
    return null;
  }
  if (countsSeconds(work, now)) {
    return FAST_REPAINT_MS;
  }
  const at = stamp(work.wakeupAt);
  const lead = at !== null && at > now ? at - now - 60_000 : -1;
  if (lead < 0) {
    return SLOW_REPAINT_MS;
  }
  return Math.min(Math.max(lead, FAST_REPAINT_MS), SLOW_REPAINT_MS);
}

/** Whether some age or countdown the line shows as of `now` reads in seconds. */
export function countsSeconds(work: RunningWork, now: number): boolean {
  const oldest = oldestShell(work.shells);
  const youngShell = oldest !== null && wholeSeconds(now - oldest) < 60;
  const at = stamp(work.wakeupAt);
  const nearWakeup = at !== null && at > now && countdownSeconds(now, at) < 60;
  return youngShell || nearWakeup;
}

/**
 * "1 background shell (cmd, 3m)", "1 background shell" for one only the
 * screen counts, or "N background shells (oldest 3m)".
 */
function shellsText(shells: readonly WorkShell[], now: number): string {
  const [only] = shells;
  if (shells.length === 1 && only !== undefined) {
    const since = stamp(only.since ?? null);
    if (since === null) {
      return "1 background shell";
    }
    const age = waited(now, since);
    const name = shellName(only);
    return name === null
      ? `1 background shell (${age})`
      : `1 background shell (${cut(name)}, ${age})`;
  }
  const count = counted(shells.length, "background shell");
  const oldest = oldestShell(shells);
  return oldest === null ? count : `${count} (oldest ${waited(now, oldest)})`;
}

/** When the longest-running reported shell started; null when none reports it. */
function oldestShell(shells: readonly WorkShell[]): number | null {
  let oldest: number | null = null;
  for (const shell of shells) {
    const since = stamp(shell.since ?? null);
    if (since !== null && (oldest === null || since < oldest)) {
      oldest = since;
    }
  }
  return oldest;
}

/** "1 subagent", "2 subagents". */
function counted(count: number, noun: string): string {
  return count === 1 ? `1 ${noun}` : `${count} ${noun}s`;
}

/** `text` cut to 24 characters, with an ellipsis when it was longer. */
function cut(text: string): string {
  const chars = Array.from(text);
  return chars.length <= COMMAND_CAP ? text : `${chars.slice(0, COMMAND_CAP).join("")}…`;
}

/** A shell's command, else its description; null when it has neither. */
function shellName(shell: WorkShell): string | null {
  if (shell.command !== undefined && shell.command !== null && shell.command !== "") {
    return shell.command;
  }
  return shell.description === "" ? null : shell.description;
}

/** The whole seconds until `at`, rounded up so a wakeup 3m 59.5s off still reads "4m". */
function countdownSeconds(now: number, at: number): number {
  return Math.ceil(Math.max(at - now, 0) / 1000);
}

/** "wakeup in 4m" while `at` is ahead of `now`, else "wakeup due". */
function wakeupText(now: number, at: number): string {
  if (at <= now) {
    return "wakeup due";
  }
  return `wakeup in ${span(countdownSeconds(now, at))}`;
}

/** How long since `since` as of `now`, as the desktop's `waited` writes it. */
function waited(now: number, since: number): string {
  return span(Math.max(wholeSeconds(now - since), 0));
}

/** Whole seconds in `ms`, truncated toward zero as chrono's `num_seconds` does. */
function wholeSeconds(ms: number): number {
  return Math.trunc(ms / 1000);
}

/** `45s`, `3m`, `1h 5m` or `2h`. */
function span(seconds: number): string {
  if (seconds < 60) {
    return `${seconds}s`;
  }
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) {
    return `${minutes}m`;
  }
  const hours = Math.floor(minutes / 60);
  const rest = minutes % 60;
  return rest === 0 ? `${hours}h` : `${hours}h ${rest}m`;
}

/** A wire timestamp as ms since the epoch; null when absent or unreadable. */
function stamp(text: string | null): number | null {
  if (text === null) {
    return null;
  }
  const ms = Date.parse(text);
  return Number.isNaN(ms) ? null : ms;
}
