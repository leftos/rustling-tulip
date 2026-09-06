import { describe, expect, it } from "vitest";
import type { SessionSnapshot } from "../types";
import type { ExitWorktreeQueue } from "./exitWorktreeQueue";
import {
  exitProgress,
  recordExitChoice,
  skipVanished,
  startExitQueue,
  worktreeSessionsForExit,
} from "./exitWorktreeQueue";

function session(
  id: string,
  overrides: Partial<SessionSnapshot> = {},
): SessionSnapshot {
  return {
    id,
    label: id,
    kind: "single",
    members: [
      {
        repo_id: "repo1-id",
        repo_name: "repo1",
        branch: "wt/brave-otter",
        worktree_path: "C:/wt/brave-otter",
      },
    ],
    status: "working",
    mode: "interactive",
    started_at: "2026-09-06T10:00:00Z",
    exit_code: null,
    metrics: {
      input_tokens: 0,
      output_tokens: 0,
      cost_usd: 0,
      last_activity_at: null,
    },
    recent_actions: [],
    is_orphan: false,
    is_abandoned: false,
    last_prompt: null,
    workspace_id: null,
    agent: "claude",
    terminal_title: null,
    program_name: "claude",
    current_cwd: null,
    appearance: {
      accent_color: null,
      terminal_background_color: null,
      terminal_frame_color: null,
      terminal_font_family: null,
      terminal_font_size: null,
      terminal_font_bold: null,
    },
    elevated_authority: false,
    has_per_session_worktree: true,
    is_inactive: false,
    worktree_paths: ["C:/wt/brave-otter"],
    ...overrides,
  };
}

describe("worktreeSessionsForExit", () => {
  it("keeps only running sessions that own a worktree", () => {
    const sessions = [
      session("a"),
      session("b", { status: "stopped" }),
      session("c", { is_orphan: true }),
      session("d", { has_per_session_worktree: false }),
      session("e", { status: "awaiting_input" }),
    ];
    expect(worktreeSessionsForExit(sessions).map((s) => s.id)).toEqual([
      "a",
      "e",
    ]);
  });

  it("excludes a stopped session even when it still has a worktree", () => {
    expect(
      worktreeSessionsForExit([session("a", { status: "stopped" })]),
    ).toEqual([]);
  });

  it("excludes an orphan the daemon cannot stop", () => {
    expect(worktreeSessionsForExit([session("a", { is_orphan: true })])).toEqual(
      [],
    );
  });

  it("excludes a worktree-less session", () => {
    expect(
      worktreeSessionsForExit([
        session("a", { has_per_session_worktree: false }),
      ]),
    ).toEqual([]);
  });

  it("returns an empty list for no sessions", () => {
    expect(worktreeSessionsForExit([])).toEqual([]);
  });
});

describe("startExitQueue", () => {
  it("queues every worktree session in order with no answers yet", () => {
    expect(startExitQueue([session("a"), session("b")])).toEqual({
      pending: ["a", "b"],
      choices: {},
      total: 2,
    });
  });

  it("is null when no session owns a worktree", () => {
    expect(
      startExitQueue([
        session("a", { has_per_session_worktree: false }),
        session("b", { status: "stopped" }),
      ]),
    ).toBeNull();
  });

  it("is null for an empty session list", () => {
    expect(startExitQueue([])).toBeNull();
  });

  it("counts only the sessions it will ask about", () => {
    const queue = startExitQueue([
      session("a"),
      session("b", { is_orphan: true }),
      session("c"),
    ]);
    expect(queue).toEqual({ pending: ["a", "c"], choices: {}, total: 2 });
  });
});

describe("recordExitChoice", () => {
  it("pops the answered session off the front and keeps the rest in order", () => {
    const queue = { pending: ["a", "b", "c"], choices: {}, total: 3 };
    expect(recordExitChoice(queue, "a", "delete")).toEqual({
      pending: ["b", "c"],
      choices: { a: "delete" },
      total: 3,
    });
  });

  it("accumulates answers across the walk", () => {
    let queue: ExitWorktreeQueue = { pending: ["a", "b"], choices: {}, total: 2 };
    queue = recordExitChoice(queue, "a", "keep");
    queue = recordExitChoice(queue, "b", "delete");
    expect(queue).toEqual({
      pending: [],
      choices: { a: "keep", b: "delete" },
      total: 2,
    });
  });

  it("drops the session wherever it sits in the queue", () => {
    const queue = { pending: ["a", "b", "c"], choices: {}, total: 3 };
    expect(recordExitChoice(queue, "b", "keep")).toEqual({
      pending: ["a", "c"],
      choices: { b: "keep" },
      total: 3,
    });
  });

  it("overwrites an earlier answer for the same session", () => {
    const queue = {
      pending: ["a"],
      choices: { a: "keep" as const },
      total: 1,
    };
    expect(recordExitChoice(queue, "a", "delete").choices).toEqual({
      a: "delete",
    });
  });

  it("leaves the input queue untouched", () => {
    const queue = { pending: ["a", "b"], choices: {}, total: 2 };
    recordExitChoice(queue, "a", "delete");
    expect(queue).toEqual({ pending: ["a", "b"], choices: {}, total: 2 });
  });
});

describe("skipVanished", () => {
  it("drops pending sessions the daemon no longer reports", () => {
    const queue = { pending: ["a", "b", "c"], choices: {}, total: 3 };
    expect(skipVanished(queue, new Set(["a", "c"]))).toEqual({
      pending: ["a", "c"],
      choices: {},
      total: 3,
    });
  });

  it("records no choice for a vanished session, leaving it on auto", () => {
    const queue = {
      pending: ["a", "b"],
      choices: { c: "delete" as const },
      total: 3,
    };
    const next = skipVanished(queue, new Set(["a"]));
    expect(next.choices).toEqual({ c: "delete" });
    expect(next.choices["b"]).toBeUndefined();
  });

  it("empties the queue when every pending session is gone", () => {
    const queue = {
      pending: ["a", "b"],
      choices: { c: "keep" as const },
      total: 3,
    };
    expect(skipVanished(queue, new Set(["c"]))).toEqual({
      pending: [],
      choices: { c: "keep" },
      total: 3,
    });
  });

  it("keeps the queue as-is when everything is still live", () => {
    const queue = { pending: ["a", "b"], choices: {}, total: 2 };
    expect(skipVanished(queue, new Set(["a", "b", "z"]))).toEqual(queue);
  });

  it("keeps the original total so the progress hint doesn't renumber", () => {
    const queue = { pending: ["a", "b", "c"], choices: {}, total: 3 };
    expect(skipVanished(queue, new Set(["c"])).total).toBe(3);
  });
});

describe("exitProgress", () => {
  it("numbers the first prompt 1 of N", () => {
    expect(exitProgress({ pending: ["a", "b", "c"], choices: {}, total: 3 })).toEqual(
      { index: 1, total: 3 },
    );
  });

  it("numbers the last prompt N of N", () => {
    expect(
      exitProgress({
        pending: ["c"],
        choices: { a: "keep", b: "delete" },
        total: 3,
      }),
    ).toEqual({ index: 3, total: 3 });
  });

  it("advances one step per answer", () => {
    let queue: ExitWorktreeQueue = {
      pending: ["a", "b", "c"],
      choices: {},
      total: 3,
    };
    expect(exitProgress(queue).index).toBe(1);
    queue = recordExitChoice(queue, "a", "keep");
    expect(exitProgress(queue).index).toBe(2);
    queue = recordExitChoice(queue, "b", "keep");
    expect(exitProgress(queue).index).toBe(3);
  });

  it("counts a skipped session as answered", () => {
    const queue = skipVanished(
      { pending: ["a", "b"], choices: {}, total: 2 },
      new Set(["b"]),
    );
    expect(exitProgress(queue)).toEqual({ index: 2, total: 2 });
  });

  it("reports a single-session walk as 1 of 1", () => {
    expect(exitProgress({ pending: ["a"], choices: {}, total: 1 })).toEqual({
      index: 1,
      total: 1,
    });
  });
});
