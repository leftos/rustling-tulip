import { expect, test } from "vitest";

import type { WorkShell, WorkSubagent } from "./protocol";
import {
  NO_WORK,
  countsSeconds,
  isIdle,
  nextRepaint,
  workLine,
  workOf,
  type RunningWork,
} from "./work-line";

// The desktop's strip tests (`apps/native/src/chat.rs`), with the same inputs and strings.

const NOW = Date.parse("2026-10-30T12:00:00Z");
const SECOND = 1000;
const MINUTE = 60 * SECOND;

function at(offsetMs: number): string {
  return new Date(NOW + offsetMs).toISOString();
}

function shell(command: string | null, description: string, ageMs: number | null): WorkShell {
  return {
    id: "b1",
    description,
    command,
    since: ageMs === null ? null : at(-ageMs),
    from_screen: false,
  };
}

function screenShell(): WorkShell {
  return { id: "", description: "", command: null, since: null, from_screen: true };
}

function subagent(description: string, agentType: string | null): WorkSubagent {
  return {
    id: "a1",
    description,
    agent_type: agentType,
    since: at(-2 * MINUTE),
    from_screen: false,
  };
}

function work(
  subagents: readonly WorkSubagent[],
  shells: readonly WorkShell[],
  wakeupAt: string | null,
): RunningWork {
  return { subagents, shells, wakeupAt };
}

function text(
  subagents: readonly WorkSubagent[],
  shells: readonly WorkShell[],
  wakeupAt: string | null,
): string | null {
  return workLine(work(subagents, shells, wakeupAt), NOW);
}

test("one_shell_reads_its_command_and_age", () => {
  const shells = [shell("cargo test", "Run the tests", 3 * MINUTE)];
  expect(text([], shells, null)).toBe("1 background shell (cargo test, 3m)");
});

test("a_long_command_is_cut_to_24_characters", () => {
  const command = "cargo nextest run --workspace --cargo-profile test-release";
  expect(text([], [shell(command, "", 3 * MINUTE)], null)).toBe(
    "1 background shell (cargo nextest run --work…, 3m)",
  );
  // 24 characters stay whole.
  expect(text([], [shell("cargo nextest run footer", "", 3 * MINUTE)], null)).toBe(
    "1 background shell (cargo nextest run footer, 3m)",
  );
});

test("a_shell_without_a_command_reads_its_description", () => {
  for (const command of [null, ""]) {
    expect(text([], [shell(command, "Watch the build", MINUTE)], null), String(command)).toBe(
      "1 background shell (Watch the build, 1m)",
    );
  }
  expect(text([], [shell(null, "", 45 * SECOND)], null)).toBe("1 background shell (45s)");
});

test("a_screen_read_shell_reads_only_its_count", () => {
  expect(text([], [screenShell()], null)).toBe("1 background shell");
});

test("several_shells_read_the_oldest_age", () => {
  const shells = [shell("a", "", 3 * MINUTE), shell("b", "", 65 * MINUTE), screenShell()];
  expect(text([], shells, null)).toBe("3 background shells (oldest 1h 5m)");
  // No reported start, no oldest age.
  expect(text([], [screenShell(), screenShell()], null)).toBe("2 background shells");
});

test("subagents_count_in_the_singular_and_the_plural", () => {
  expect(text([subagent("Sleeper A", null)], [], null)).toBe("1 subagent");
  const two = [subagent("Sleeper A", null), subagent("Sleeper B", null)];
  expect(text(two, [], null)).toBe("2 subagents");
});

test("a_wakeup_ahead_counts_down", () => {
  expect(text([], [], at(4 * MINUTE)), "exactly 4 minutes ahead").toBe("wakeup in 4m");
  expect(text([], [], at(4 * MINUTE - 500)), "the countdown rounds up to the second").toBe(
    "wakeup in 4m",
  );
  expect(text([], [], at(65 * MINUTE))).toBe("wakeup in 1h 5m");
  expect(text([], [], at(59 * SECOND))).toBe("wakeup in 59s");
});

test("a_passed_wakeup_is_due", () => {
  for (const wakeup of [at(0), at(-2 * MINUTE)]) {
    expect(text([], [], wakeup), wakeup).toBe("wakeup due");
  }
});

test("nothing_running_gives_no_strip", () => {
  expect(text([], [], null)).toBeNull();
  expect(isIdle(NO_WORK)).toBe(true);
});

test("the_parts_join_in_order", () => {
  expect(
    text(
      [subagent("Sleeper A", null), subagent("Sleeper B", null)],
      [shell("cargo nextest run footer", "", 3 * MINUTE)],
      at(4 * MINUTE),
    ),
  ).toBe("2 subagents · 1 background shell (cargo nextest run footer, 3m) · wakeup in 4m");
});

test("a_wakeup_75_seconds_off_repaints_when_it_reads_in_seconds", () => {
  expect(nextRepaint(work([], [], at(75 * SECOND)), NOW)).toBe(15 * SECOND);
});

test("an_age_under_a_minute_repaints_each_second", () => {
  expect(nextRepaint(work([], [shell("a", "", 40 * SECOND)], null), NOW)).toBe(SECOND);
});

test("nothing_timed_repaints_every_30_seconds", () => {
  expect(nextRepaint(work([], [screenShell()], null), NOW)).toBe(30 * SECOND);
  expect(nextRepaint(work([], [], at(10 * MINUTE)), NOW), "a wakeup far off waits the 30 s").toBe(
    30 * SECOND,
  );
});

test("an_empty_strip_needs_no_repaint", () => {
  expect(nextRepaint(NO_WORK, NOW)).toBeNull();
});

test("ages_and_countdowns_under_a_minute_count_seconds", () => {
  expect(countsSeconds(work([], [shell("a", "", 3 * MINUTE)], null), NOW)).toBe(false);
  expect(countsSeconds(work([], [], at(30 * SECOND)), NOW)).toBe(true);
  expect(countsSeconds(work([], [shell("a", "", 10 * SECOND)], null), NOW)).toBe(true);
});

test("a_chat_work_message_reads_as_running_work", () => {
  expect(workOf({ type: "chat_work", session_id: "s1" })).toEqual(NO_WORK);
  const shells = [screenShell()];
  expect(workOf({ type: "chat_work", session_id: "s1", shells, wakeup_at: at(MINUTE) })).toEqual({
    subagents: [],
    shells,
    wakeupAt: at(MINUTE),
  });
});
