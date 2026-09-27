import { describe, expect, it } from "vitest";
import type {
  RepoEntry,
  SessionEnd,
  SessionHistoryItem,
  SpawnConfig,
} from "../types";
import {
  badgeLabel,
  defaultConversations,
  disabledReason,
  endedText,
  endReasonLabel,
  formatClock,
  groupHistory,
  isPreTicked,
  OTHER_GROUP_TITLE,
  otherToggleLabel,
  recoverAsOptions,
  recoverBadgeCount,
} from "./recoverModel";

const SPAWN_CONFIG: SpawnConfig = {
  target: {
    kind: "single",
    repo_id: "repo-yaat",
    branch_name: "main",
    base_branch: null,
    use_worktree: false,
  },
  mode: "interactive",
  dangerously_skip_permissions: false,
  agent_options: { kind: "claude", permission_mode: null },
  model: null,
  extra_env: [],
};

const CANDIDATE = {
  id: "85573bb1-0000-0000-0000-000000000000",
  last_active: "2026-09-27T18:27:37Z",
  title: "Fix the build",
};

function item(
  id: string,
  endedAt: string,
  overrides: {
    end?: SessionEnd;
    mode?: SessionHistoryItem["entry"]["mode"];
    spawnConfig?: SpawnConfig | null;
    recoveredAt?: string | null;
    candidates?: SessionHistoryItem["candidates"];
    folderIsGitRepo?: boolean;
    folderRepoId?: string | null;
    endTimeKnown?: boolean;
    folder?: string;
  } = {},
): SessionHistoryItem {
  return {
    entry: {
      session_id: id,
      label: id,
      kind: "single",
      mode: overrides.mode ?? "interactive",
      agent: "claude",
      spawn_config:
        overrides.spawnConfig === undefined ? SPAWN_CONFIG : overrides.spawnConfig,
      members: [],
      workspace_id: null,
      primary_cwd: overrides.folder ?? "D:\\yaat",
      current_cwd: null,
      program_name: "claude",
      started_at: "2026-09-27T11:00:00Z",
      ended_at: endedAt,
      end: overrides.end ?? { type: "tracer_lost" },
      claude_session_id: null,
      source: "tracer_log",
      recovered_at: overrides.recoveredAt ?? null,
      end_time_known: overrides.endTimeKnown ?? true,
      import_rev: 1,
    },
    candidates: overrides.candidates ?? [CANDIDATE],
    folder_is_git_repo: overrides.folderIsGitRepo ?? true,
    folder_repo_id:
      overrides.folderRepoId === undefined ? "repo-yaat" : overrides.folderRepoId,
  };
}

const REPOS: RepoEntry[] = [
  {
    id: "repo-yaat",
    name: "yaat",
    path: "D:\\yaat",
    default_branch: "main",
    default_use_worktree: false,
    appearance: {
      accent_color: null,
      terminal_background_color: null,
      terminal_frame_color: null,
      terminal_font_family: null,
      terminal_font_size: null,
      terminal_font_bold: null,
    },
    last_agent: null,
    last_spawn_config: null,
  },
];

function ids(group: { items: SessionHistoryItem[] }): string[] {
  return group.items.map((i) => i.entry.session_id);
}

describe("groupHistory", () => {
  it("puts lost sessions ending within 60 s of each other in one group", () => {
    const groups = groupHistory([
      item("a", "2026-09-27T18:29:06Z"),
      item("b", "2026-09-27T18:28:10Z"),
      item("c", "2026-09-27T18:27:10Z"),
    ]);
    expect(groups).toHaveLength(1);
    expect(ids(groups[0]!)).toEqual(["a", "b", "c"]);
    const clock = formatClock(new Date("2026-09-27T18:29:06Z"));
    expect(groups[0]!.title).toBe(`3 sessions lost at ${clock}`);
  });

  it("chains the window from the previous item, not from the first", () => {
    // a→b is 50 s and b→c is 50 s: c is 100 s from a but still joins.
    const groups = groupHistory([
      item("a", "2026-09-27T18:30:00Z"),
      item("b", "2026-09-27T18:29:10Z"),
      item("c", "2026-09-27T18:28:20Z"),
    ]);
    expect(groups.map(ids)).toEqual([["a", "b", "c"]]);
  });

  it("starts a new group when the gap exceeds 60 s", () => {
    const groups = groupHistory([
      item("a", "2026-09-27T18:30:00Z"),
      item("b", "2026-09-27T18:28:59Z"),
    ]);
    expect(groups.map(ids)).toEqual([["a"], ["b"]]);
    expect(groups[0]!.title).toMatch(/^1 session lost at /);
  });

  it("groups exactly 60 s apart together", () => {
    const groups = groupHistory([
      item("a", "2026-09-27T18:30:00Z"),
      item("b", "2026-09-27T18:29:00Z"),
    ]);
    expect(groups.map(ids)).toEqual([["a", "b"]]);
  });

  it("sorts newest first regardless of input order", () => {
    const groups = groupHistory([
      item("old", "2026-09-27T18:00:00Z"),
      item("new", "2026-09-27T18:00:30Z"),
    ]);
    expect(groups.map(ids)).toEqual([["new", "old"]]);
  });

  it("breaks a burst on an intervening non-lost session", () => {
    const groups = groupHistory([
      item("a", "2026-09-27T18:30:00Z"),
      item("x", "2026-09-27T18:29:50Z", { end: { type: "stopped_by_user" } }),
      item("b", "2026-09-27T18:29:40Z"),
    ]);
    expect(groups.map((g) => g.title.includes("lost"))).toEqual([
      true,
      true,
      false,
    ]);
    expect(groups.map(ids)).toEqual([["a"], ["b"], ["x"]]);
    expect(groups[2]!.title).toBe(OTHER_GROUP_TITLE);
  });

  it("drops headless sessions", () => {
    const groups = groupHistory([
      item("h", "2026-09-27T18:30:00Z", { mode: "headless" }),
    ]);
    expect(groups).toEqual([]);
  });

  it("returns no groups for an empty history", () => {
    expect(groupHistory([])).toEqual([]);
  });
});

describe("pre-tick and disabled rules", () => {
  it("pre-ticks an unrecovered lost session", () => {
    expect(isPreTicked(item("a", "2026-09-27T18:30:00Z"))).toBe(true);
  });

  it("does not pre-tick sessions that ended any other way", () => {
    const exited = item("a", "2026-09-27T18:30:00Z", {
      end: { type: "exited", code: 1 },
    });
    expect(isPreTicked(exited)).toBe(false);
    expect(disabledReason(exited)).toBeNull();
  });

  it("disables an already recovered session and names when", () => {
    const recovered = item("a", "2026-09-27T18:30:00Z", {
      recoveredAt: "2026-09-27T19:05:00Z",
    });
    const clock = formatClock(new Date("2026-09-27T19:05:00Z"));
    expect(disabledReason(recovered)).toBe(`recovered ${clock}`);
    expect(isPreTicked(recovered)).toBe(false);
  });

  it("disables a Claude session with no conversation", () => {
    const none = item("a", "2026-09-27T18:30:00Z", { candidates: [] });
    expect(disabledReason(none)).toBe("no conversation found");
    expect(isPreTicked(none)).toBe(false);
  });

  it("keeps a plain shell with no conversation recoverable", () => {
    const shell = item("a", "2026-09-27T18:30:00Z", {
      mode: "plain_shell",
      candidates: [],
    });
    expect(disabledReason(shell)).toBeNull();
    expect(isPreTicked(shell)).toBe(true);
  });
});

describe("recoverAsOptions", () => {
  const shellRow = (over: Parameters<typeof item>[2]) =>
    item("a", "2026-09-27T18:30:00Z", { mode: "plain_shell", ...over });

  it("offers a Claude session in the registered repo first", () => {
    const opts = recoverAsOptions(shellRow({ folderRepoId: "repo-yaat" }), REPOS);
    expect(opts.map((o) => o.label)).toEqual([
      "Claude session in yaat",
      "Shell running claude --resume",
    ]);
    expect(opts[0]!.how).toEqual({ type: "claude" });
  });

  it("offers to register an unregistered git folder", () => {
    const opts = recoverAsOptions(
      shellRow({ folderRepoId: null, folderIsGitRepo: true }),
      REPOS,
    );
    expect(opts[0]!.label).toBe(
      "Register D:\\yaat as a repo, then Claude session",
    );
    expect(opts[0]!.how).toEqual({
      type: "register_repo_then_claude",
      path: "D:\\yaat",
    });
    expect(opts[1]!.how).toEqual({ type: "shell" });
  });

  it("defaults to the shell for a folder that is not a git repo", () => {
    const opts = recoverAsOptions(
      shellRow({ folderRepoId: null, folderIsGitRepo: false }),
      REPOS,
    );
    expect(opts).toHaveLength(1);
    expect(opts[0]!.how).toEqual({ type: "shell" });
  });

  it("offers only a plain shell when there is no conversation", () => {
    const opts = recoverAsOptions(shellRow({ candidates: [] }), REPOS);
    expect(opts.map((o) => o.label)).toEqual(["Plain shell"]);
  });

  it("prefers current_cwd over primary_cwd as the folder", () => {
    const row = shellRow({ folderRepoId: null, folderIsGitRepo: true });
    row.entry.current_cwd = "D:\\other";
    expect(recoverAsOptions(row, REPOS)[0]!.how).toEqual({
      type: "register_repo_then_claude",
      path: "D:\\other",
    });
  });
});

describe("badge", () => {
  it("counts unrecovered lost sessions only", () => {
    expect(
      recoverBadgeCount([
        item("a", "2026-09-27T18:30:00Z"),
        item("b", "2026-09-27T18:30:00Z", { recoveredAt: "2026-09-27T19:00:00Z" }),
        item("c", "2026-09-27T18:30:00Z", { end: { type: "daemon_shutdown" } }),
        item("d", "2026-09-27T18:30:00Z", { mode: "headless" }),
      ]),
    ).toBe(1);
  });

  it("hides at 0 and caps above 99", () => {
    expect(badgeLabel(0)).toBeNull();
    expect(badgeLabel(1)).toBe("1");
    expect(badgeLabel(99)).toBe("99");
    expect(badgeLabel(100)).toBe("99+");
  });
});

describe("endReasonLabel", () => {
  it("names each end reason", () => {
    expect(endReasonLabel({ type: "tracer_lost" })).toBe("lost");
    expect(endReasonLabel({ type: "exited", code: 3 })).toBe("exited (code 3)");
    expect(endReasonLabel({ type: "stopped_by_user" })).toBe("stopped");
    expect(endReasonLabel({ type: "daemon_shutdown" })).toBe("daemon shut down");
    expect(
      endReasonLabel({ type: "from_the_future" } as unknown as SessionEnd),
    ).toBe("ended");
  });
});

describe("lost at an unknown time", () => {
  const unknown = (id: string, endedAt: string, recoveredAt?: string) =>
    item(id, endedAt, { endTimeKnown: false, recoveredAt: recoveredAt ?? null });

  it("puts every unrecovered one in one group, first", () => {
    const groups = groupHistory([
      item("known", "2026-09-27T18:29:06Z"),
      unknown("u1", "2026-09-27T10:31:00Z"),
      unknown("u2", "2026-09-26T22:27:00Z"),
      unknown("u3", "2026-09-27T04:07:00Z"),
    ]);
    expect(groups.map((g) => g.title)).toEqual([
      "3 sessions lost (time unknown)",
      `1 session lost at ${formatClock(new Date("2026-09-27T18:29:06Z"))}`,
    ]);
    expect(ids(groups[0]!)).toEqual(["u1", "u3", "u2"]);
    expect(groups[0]!.items.every(isPreTicked)).toBe(true);
  });

  it("keeps recovered ones under Other", () => {
    const groups = groupHistory([
      unknown("u1", "2026-09-27T10:31:00Z"),
      unknown("done", "2026-09-27T10:30:00Z", "2026-09-27T12:00:00Z"),
    ]);
    expect(groups.map((g) => g.title)).toEqual([
      "1 session lost (time unknown)",
      OTHER_GROUP_TITLE,
    ]);
    expect(ids(groups[1]!)).toEqual(["done"]);
  });

  it("keeps 60 s bursts for known times around unknown ones", () => {
    const groups = groupHistory([
      item("a", "2026-09-27T18:30:00Z"),
      unknown("u", "2026-09-27T18:29:30Z"),
      item("b", "2026-09-27T18:29:10Z"),
    ]);
    expect(groups.map(ids)).toEqual([["u"], ["a", "b"]]);
  });

  it("says the time is unknown in the row", () => {
    const now = new Date("2026-09-27T19:00:00Z");
    expect(endedText(unknown("u", "2026-09-27T10:31:00Z"), now)).toBe(
      "lost · time unknown",
    );
    const known = item("k", "2026-09-27T18:29:06Z");
    expect(endedText(known, now)).toBe(
      `${formatClock(new Date("2026-09-27T18:29:06Z"))} · lost`,
    );
  });

  it("counts toward the badge", () => {
    expect(
      recoverBadgeCount([
        unknown("u", "2026-09-27T10:31:00Z"),
        item("k", "2026-09-27T18:29:06Z"),
      ]),
    ).toBe(2);
  });
});

describe("defaultConversations", () => {
  const newer = {
    id: "conv-newer",
    last_active: "2026-09-27T18:00:00Z",
    title: "newer",
  };
  const older = {
    id: "conv-older",
    last_active: "2026-09-27T12:00:00Z",
    title: "older",
  };

  it("gives two shells in one folder different conversations", () => {
    const rows = [
      item("s1", "2026-09-27T18:29:06Z", {
        mode: "plain_shell",
        folder: "D:\\",
        candidates: [newer, older],
      }),
      item("s2", "2026-09-27T18:29:05Z", {
        mode: "plain_shell",
        folder: "d:/",
        candidates: [newer, older],
      }),
    ];
    const defaults = defaultConversations(rows);
    expect(defaults.get("s1")).toBe("conv-newer");
    expect(defaults.get("s2")).toBe("conv-older");
  });

  it("keeps the newest when every candidate is taken", () => {
    const rows = ["s1", "s2", "s3"].map((id) =>
      item(id, "2026-09-27T18:29:06Z", { candidates: [newer, older] }),
    );
    expect([...defaultConversations(rows).values()]).toEqual([
      "conv-newer",
      "conv-older",
      "conv-newer",
    ]);
  });

  it("leaves a single row on its newest", () => {
    const rows = [item("s1", "2026-09-27T18:29:06Z", { candidates: [newer, older] })];
    expect(defaultConversations(rows).get("s1")).toBe("conv-newer");
  });

  it("does not share across folders", () => {
    const rows = [
      item("a", "2026-09-27T18:29:06Z", { folder: "D:\\a", candidates: [newer] }),
      item("b", "2026-09-27T18:29:05Z", { folder: "D:\\b", candidates: [newer] }),
    ];
    const defaults = defaultConversations(rows);
    expect(defaults.get("a")).toBe("conv-newer");
    expect(defaults.get("b")).toBe("conv-newer");
  });
});

describe("otherToggleLabel", () => {
  it("names the count and the direction", () => {
    expect(otherToggleLabel(4, false)).toBe("Show 4 other recent sessions");
    expect(otherToggleLabel(4, true)).toBe("Hide 4 other recent sessions");
    expect(otherToggleLabel(1, false)).toBe("Show 1 other recent session");
  });
});
