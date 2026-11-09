import { expect, test } from "vitest";

import {
  INITIAL_HOME,
  PLAN_READY,
  WAITING_FOR_YOU,
  homeGroups,
  homeStep,
  needsYou,
  needsYouCount,
  reset,
  type HomeModel,
} from "./home-model";
import type {
  DaemonMessage,
  PendingInput,
  RepoEntry,
  SessionSnapshot,
  WorkspaceEntry,
} from "./protocol";

const SINCE = "2026-11-06T12:00:00Z";
const EARLIER = "2026-11-06T09:00:00Z";

const ALPHA: RepoEntry = { id: "a", name: "alpha", path: "C:\\dev\\alpha", default_branch: null };
const ZULU: RepoEntry = { id: "zz", name: "Zulu", path: "C:\\dev\\zulu", default_branch: null };
const ZETA: RepoEntry = { id: "z", name: "Zeta", path: "C:\\dev\\zeta", default_branch: null };

const WORKSPACE: WorkspaceEntry = { id: "w", name: "ws-one", member_repo_ids: ["z"] };

function session(id: string, overrides: Partial<SessionSnapshot> = {}): SessionSnapshot {
  return {
    id,
    label: id,
    kind: "single",
    members: [
      { repo_id: "a", repo_name: "alpha", branch: "main", worktree_path: "C:\\dev\\alpha" },
    ],
    status: "idle",
    mode: "interactive",
    started_at: "2026-11-06T10:00:00Z",
    exit_code: null,
    metrics: { input_tokens: 0, output_tokens: 0, cost_usd: 0, last_activity_at: null },
    recent_actions: [],
    agent: null,
    ...overrides,
  };
}

/** A session waiting on `pending_input` since `status_since`. */
function awaiting(
  id: string,
  pending_input: PendingInput | null,
  status_since: string | null = SINCE,
): SessionSnapshot {
  return session(id, { status: "awaiting_input", status_since, pending_input });
}

function sessions(...sessions: SessionSnapshot[]): DaemonMessage {
  return { type: "sessions", sessions };
}

function withRepos(...repos: RepoEntry[]): DaemonMessage {
  return { type: "repos", repos };
}

function modelFrom(...messages: DaemonMessage[]): HomeModel {
  return messages.reduce(homeStep, reset());
}

function groupTitles(model: HomeModel): string[] {
  return homeGroups(model).map((group) => `${group.kind}:${group.name}`);
}

function rowIds(model: HomeModel): string[] {
  return homeGroups(model).flatMap((group) => group.rows.map((row) => row.id));
}

test("home_status_change_keeps_row_order", () => {
  const before = modelFrom(withRepos(ALPHA), sessions(session("s1"), session("s2"), session("s3")));
  expect(rowIds(before)).toEqual(["s1", "s2", "s3"]);

  let model = before;
  model = homeStep(model, {
    type: "session_updated",
    session: session("s2", { status: "working" }),
  });
  model = homeStep(model, {
    type: "session_updated",
    session: awaiting("s3", null),
  });
  model = homeStep(model, {
    type: "session_updated",
    session: session("s1", { status: "stopped" }),
  });

  expect(rowIds(model)).toEqual(["s1", "s2", "s3"]);
  expect(homeGroups(model)[0]?.rows.map((row) => row.status)).toEqual([
    "stopped",
    "working",
    "awaiting_input",
  ]);
});

test("home_session_first_seen_by_an_update_goes_to_its_group_end", () => {
  let model = modelFrom(withRepos(ALPHA), sessions(session("s1"), session("s3")));

  model = homeStep(model, { type: "session_updated", session: session("s2") });

  expect(rowIds(model)).toEqual(["s1", "s3", "s2"]);
});

test("home_groups_follow_the_desktop_order_and_alphabetical_repos", () => {
  const model = modelFrom(
    withRepos(ZULU, ALPHA, ZETA),
    { type: "workspaces", workspaces: [WORKSPACE] },
    sessions(
      session("s1", { kind: "workspace", workspace_id: "w", members: [] }),
      session("s2"),
      session("s6", {
        members: [
          { repo_id: "zz", repo_name: "Zulu", branch: "main", worktree_path: "C:\\dev\\zulu" },
        ],
      }),
      session("s3", { kind: "standalone", members: [] }),
      session("s4", { mode: "plain_shell", current_cwd: "C:\\elsewhere" }),
      session("s5", {
        members: [
          { repo_id: "gone", repo_name: "gone", branch: "main", worktree_path: "C:\\gone" },
        ],
      }),
    ),
  );

  expect(groupTitles(model)).toEqual([
    "workspace:ws-one",
    "repo:alpha",
    "repo:Zulu",
    "shell:Shells",
    "folder:Folders",
    "detached:Detached",
  ]);
});

test("home_workspace_member_repo_folds_into_its_workspace", () => {
  const model = modelFrom(
    withRepos(ZETA, ALPHA),
    { type: "workspaces", workspaces: [WORKSPACE] },
    sessions(
      session("s1", { kind: "workspace", workspace_id: "w", members: [] }),
      session("s2"),
      session("s3"),
    ),
  );

  // Zeta is a member of ws-one, so it has no group of its own.
  expect(groupTitles(model)).toEqual(["workspace:ws-one", "repo:alpha"]);
  expect(homeGroups(model)[0]?.rows.map((row) => row.id)).toEqual(["s1"]);
  expect(homeGroups(model)[1]?.rows.map((row) => row.id)).toEqual(["s2", "s3"]);
});

test("home_needs_you_holds_awaiting_input_longest_wait_first", () => {
  const model = modelFrom(
    sessions(
      awaiting("newest", null),
      session("working", { status: "working", status_since: EARLIER }),
      awaiting("oldest", null, EARLIER),
      awaiting("no-stamp", null, null),
    ),
  );

  expect(needsYou(model).map((card) => card.sessionId)).toEqual(["oldest", "newest", "no-stamp"]);
});

test("home_needs_you_leaves_out_parked_and_abandoned", () => {
  const model = modelFrom(
    sessions(
      awaiting("asking", null),
      session("parked", { status: "awaiting_input", status_since: SINCE, is_inactive: true }),
      session("abandoned", { status: "awaiting_input", status_since: SINCE, is_abandoned: true }),
    ),
  );

  expect(needsYou(model).map((card) => card.sessionId)).toEqual(["asking"]);
});

test("home_needs_you_count_is_the_tray_length", () => {
  const empty = modelFrom(sessions(session("idle")));
  expect(needsYouCount(empty)).toBe(0);

  const model = modelFrom(sessions(awaiting("a", null), awaiting("b", null), session("c")));
  expect(needsYouCount(model)).toBe(2);
  expect(needsYouCount(reset())).toBe(0);
});

test("home_card_for_one_single_choice_question", () => {
  const question: PendingInput = {
    kind: "question",
    questions: [
      {
        question: "Which database?",
        header: "Storage",
        options: ["Postgres", "SQLite"],
        multi_select: false,
      },
    ],
  };

  expect(needsYou(modelFrom(sessions(awaiting("s1", question))))).toEqual([
    {
      sessionId: "s1",
      label: "s1",
      detail: "Which database?",
      since: SINCE,
      choices: [
        { label: "Postgres", hint: "1", choice: { kind: "option", index: 0 } },
        { label: "SQLite", hint: "2", choice: { kind: "option", index: 1 } },
      ],
      open: null,
    },
  ]);
});

test("a preview question offers no choices", () => {
  const preview: PendingInput = {
    kind: "question",
    questions: [{ question: "Which shape?", options: ["Current", "Other"], preview: true }],
  };

  const [card] = needsYou(modelFrom(sessions(awaiting("s1", preview))));
  expect(card?.detail).toBe("Which shape?");
  expect(card?.choices).toEqual([]);
});

test("home_card_for_a_permission", () => {
  const permission: PendingInput = {
    kind: "permission",
    tool_name: "Bash",
    summary: "rm -rf build",
  };

  expect(needsYou(modelFrom(sessions(awaiting("s1", permission))))).toEqual([
    {
      sessionId: "s1",
      label: "s1",
      detail: "Allow Bash?",
      since: SINCE,
      choices: [
        { label: "Allow", hint: null, choice: { kind: "allow" } },
        { label: "Deny", hint: null, choice: { kind: "deny" } },
      ],
      open: null,
    },
  ]);
});

test("home_card_without_buttons_for_a_longer_prompt", () => {
  const multiSelect: PendingInput = {
    kind: "question",
    questions: [{ question: "Which files?", options: ["a", "b"], multi_select: true }],
  };
  const several: PendingInput = {
    kind: "question",
    questions: [
      { question: "One?", options: ["a", "b"] },
      { question: "Two?", options: ["c"] },
    ],
  };
  const plan: PendingInput = { kind: "plan_approval", plan: "1. Do the thing" };
  const other: PendingInput = { kind: "other", message: "An MCP server asks for a value." };
  const unknown = { kind: "selector", prompt: "pick one" } as unknown as PendingInput;

  const cases: [PendingInput | null, string][] = [
    [multiSelect, "Which files?"],
    [several, "One?"],
    [plan, PLAN_READY],
    [other, "An MCP server asks for a value."],
    [unknown, WAITING_FOR_YOU],
    [null, WAITING_FOR_YOU],
  ];
  for (const [prompt, detail] of cases) {
    expect(needsYou(modelFrom(sessions(awaiting("s1", prompt))))).toEqual([
      { sessionId: "s1", label: "s1", detail, since: SINCE, choices: [], open: "Open" },
    ]);
  }
});

test("home_card_without_a_status_stamp_has_no_buttons", () => {
  const question: PendingInput = {
    kind: "question",
    questions: [{ question: "Ship it?", options: ["Yes", "No"] }],
  };
  const permission: PendingInput = { kind: "permission", tool_name: "Bash", summary: "ls" };

  for (const prompt of [question, permission]) {
    expect(needsYou(modelFrom(sessions(awaiting("s1", prompt, null))))).toEqual([
      {
        sessionId: "s1",
        label: "s1",
        detail: expect.any(String),
        since: null,
        choices: [],
        open: "Open",
      },
    ]);
  }
});

test("home_removed_session_leaves_the_list_and_the_tray", () => {
  const model = modelFrom(withRepos(ALPHA), sessions(awaiting("s1", null), awaiting("s2", null)));

  const after = homeStep(model, { type: "session_removed", session_id: "s1" });

  expect(rowIds(after)).toEqual(["s2"]);
  expect(needsYou(after).map((card) => card.sessionId)).toEqual(["s2"]);
});

test("home_reset_empties_the_model", () => {
  const model = modelFrom(withRepos(ALPHA), sessions(awaiting("s1", null)), {
    type: "sessions_reordered",
    container_id: "a",
    ordered_ids: ["s1"],
  });
  expect(homeGroups(model)).not.toEqual([]);

  expect(reset()).toBe(INITIAL_HOME);
  expect(homeGroups(reset())).toEqual([]);
  expect(needsYou(reset())).toEqual([]);
  expect(needsYouCount(reset())).toBe(0);
});

test("home_manual_order_wins_over_the_server_order", () => {
  const model = modelFrom(withRepos(ALPHA), sessions(session("s1"), session("s2")), {
    type: "sessions_reordered",
    container_id: "a",
    ordered_ids: ["s2", "s1"],
  });

  expect(rowIds(model)).toEqual(["s2", "s1"]);
});

test("home_manual_order_leaves_rows_it_does_not_name_after_them", () => {
  const model = modelFrom(withRepos(ALPHA), sessions(session("s1"), session("s2"), session("s3")), {
    type: "sessions_reordered",
    container_id: "a",
    ordered_ids: ["s3", "stale", "s1"],
  });

  expect(rowIds(model)).toEqual(["s3", "s1", "s2"]);
});

test("home_a_session_on_a_workspace_member_repo_is_detached", () => {
  const onMember = session("s1", {
    members: [
      { repo_id: "z", repo_name: "Zeta", branch: "main", worktree_path: "C:\\dev\\zeta" },
      { repo_id: "a", repo_name: "alpha", branch: "main", worktree_path: "C:\\dev\\alpha" },
    ],
  });
  const model = modelFrom(
    withRepos(ZETA, ALPHA),
    { type: "workspaces", workspaces: [WORKSPACE] },
    sessions(onMember, session("s2")),
  );

  // The owner is the first member, Zeta, which is a workspace member.
  expect(groupTitles(model)).toEqual(["repo:alpha", "detached:Detached"]);
  expect(homeGroups(model)[1]?.rows.map((row) => row.id)).toEqual(["s1"]);
});

test("home_container_order_puts_reordered_containers_first", () => {
  const model = modelFrom(
    withRepos(ZULU, ALPHA, ZETA),
    { type: "workspaces", workspaces: [WORKSPACE] },
    sessions(
      session("s1", { kind: "workspace", workspace_id: "w", members: [] }),
      session("s2"),
      session("s6", {
        members: [
          { repo_id: "zz", repo_name: "Zulu", branch: "main", worktree_path: "C:\\dev\\zulu" },
        ],
      }),
    ),
    {
      type: "containers_reordered",
      ordered: [
        { kind: "repo", id: "zz" },
        { kind: "workspace", id: "w" },
        { kind: "repo", id: "gone" },
      ],
    },
  );

  expect(groupTitles(model)).toEqual(["repo:Zulu", "workspace:ws-one", "repo:alpha"]);
});

test("home_other_prompt_with_no_message_says_waiting", () => {
  const blank: PendingInput = { kind: "other", message: "  " };

  expect(needsYou(modelFrom(sessions(awaiting("s1", blank))))).toEqual([
    {
      sessionId: "s1",
      label: "s1",
      detail: WAITING_FOR_YOU,
      since: SINCE,
      choices: [],
      open: "Open",
    },
  ]);
});
