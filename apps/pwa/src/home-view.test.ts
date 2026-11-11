import { expect, test } from "vitest";

import { createConnectedLatch } from "./connection-bar";
import { mountHomeView } from "./home-view";
import type {
  ClientMessage,
  DaemonMessage,
  PendingInput,
  PresetEntry,
  RepoEntry,
  SessionSnapshot,
  SpawnConfig,
} from "./protocol";
import type { Connection, ConnectionNotice } from "./socket";

const SINCE = "2026-11-06T12:00:00Z";

const ALPHA: RepoEntry = { id: "a", name: "alpha", path: "C:\\dev\\alpha", default_branch: null };

/** A stored launch config for `ALPHA`, interactive unless told otherwise. */
function spawnConfig(mode: SpawnConfig["mode"] = "interactive"): SpawnConfig {
  return {
    target: {
      kind: "single",
      repo_id: "a",
      branch_name: "main",
      base_branch: null,
      use_worktree: true,
    },
    mode,
    agent: null,
  };
}

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

function repos(...entries: RepoEntry[]): DaemonMessage {
  return { type: "repos", repos: entries };
}

function sessions(...entries: SessionSnapshot[]): DaemonMessage {
  return { type: "sessions", sessions: entries };
}

/** The node calls Home makes, its tag, and the handler a test fires. */
class FakeNode {
  className = "";
  textContent = "";
  type = "";
  disabled = false;
  hidden = false;
  readonly children: FakeNode[] = [];
  private readonly handlers = new Map<string, (() => void)[]>();

  constructor(readonly tag: string) {}

  append(...nodes: FakeNode[]): void {
    this.children.push(...nodes);
  }

  replaceChildren(...nodes: FakeNode[]): void {
    this.children.splice(0, this.children.length, ...nodes);
  }

  addEventListener(kind: string, handler: () => void): void {
    const held = this.handlers.get(kind) ?? [];
    held.push(handler);
    this.handlers.set(kind, held);
  }

  fire(kind: string): void {
    for (const handler of this.handlers.get(kind) ?? []) {
      handler();
    }
  }

  /** Every descendant, depth first, in drawing order. */
  descendants(): FakeNode[] {
    const found: FakeNode[] = [];
    for (const child of this.children) {
      found.push(child, ...child.descendants());
    }
    return found;
  }

  /** The first descendant with this tag and class name. */
  pick(tag: string, className: string): FakeNode | undefined {
    return this.descendants().find((node) => node.tag === tag && node.className === className);
  }
}

class FakeDocument {
  createElement(tag: string): FakeNode {
    return new FakeNode(tag);
  }
}

// The page's views are written against the DOM; this file runs in node, so it
// gives them the little of it Home draws with.
const host = globalThis as unknown as { document: FakeDocument };
host.document = new FakeDocument();

interface Harness {
  readonly container: FakeNode;
  /** Every message Home sent, in order. */
  readonly sent: ClientMessage[];
  /** Every session id Home handed to `onOpen`. */
  readonly opened: string[];
  /** The notices Home is still listening for; the unsubscribe removes its own. */
  readonly listeners: ((notice: ConnectionNotice) => void)[];
  notice(notice: ConnectionNotice): void;
  unmount(): void;
}

/** Home over a scripted connection whose sends all land; `supports` answers its feature checks. */
function harness(supports: (feature: string) => boolean = () => true): Harness {
  const container = new FakeNode("div");
  const sent: ClientMessage[] = [];
  const opened: string[] = [];
  const listeners: ((notice: ConnectionNotice) => void)[] = [];
  const connection = {
    subscribe(listener: (notice: ConnectionNotice) => void) {
      listeners.push(listener);
      return () => {
        const at = listeners.indexOf(listener);
        if (at !== -1) {
          listeners.splice(at, 1);
        }
      };
    },
    send(message: ClientMessage) {
      sent.push(message);
      return true;
    },
    paired: () => true,
    supports,
    status: () => "ready",
  } as unknown as Connection;
  const unmount = mountHomeView(
    container as unknown as HTMLElement,
    connection,
    null,
    (sessionId) => opened.push(sessionId),
    () => {},
    createConnectedLatch(),
  );
  return {
    container,
    sent,
    opened,
    listeners,
    notice(notice) {
      for (const listener of listeners.slice()) {
        listener(notice);
      }
    },
    unmount,
  };
}

/** Home with one repo holding a stored launch and one session in it. */
function launched(): Harness {
  const h = harness();
  h.notice({ kind: "status", state: "ready" });
  h.notice({ kind: "message", message: repos({ ...ALPHA, last_spawn_config: spawnConfig() }) });
  h.notice({ kind: "message", message: sessions(session("s1")) });
  return h;
}

test("a repo with a stored launch draws its group, its Launch last and its row", () => {
  const h = launched();

  const section = h.container.pick("section", "home-group");
  expect(section).toBeDefined();
  expect(section?.pick("h2", "home-group-name")?.textContent).toBe("alpha");
  expect(section?.pick("button", "home-group-launch")?.textContent).toBe("Launch last");
  expect(section?.pick("div", "home-row")?.pick("button", "home-row-label")?.textContent).toBe(
    "s1",
  );
  h.unmount();
});

test("a tap on Launch last sends one launch_last for its container", () => {
  const h = launched();
  expect(h.sent).toEqual([]);

  h.container.pick("button", "home-group-launch")?.fire("click");

  expect(h.sent).toEqual([
    {
      type: "launch_last",
      target: { kind: "repo", id: "a" },
      request_id: expect.any(String),
    },
  ]);
  h.unmount();
});

test("a tap on a row label opens its session", () => {
  const h = launched();

  h.container.pick("button", "home-row-label")?.fire("click");

  expect(h.opened).toEqual(["s1"]);
  h.unmount();
});

test("a session that needs input draws a card whose choice tap sends the answer", () => {
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
  const h = harness();
  h.notice({ kind: "status", state: "ready" });
  h.notice({ kind: "message", message: repos(ALPHA) });
  h.notice({
    kind: "message",
    message: sessions(
      session("s1", { status: "awaiting_input", status_since: SINCE, pending_input: question }),
    ),
  });

  const tray = h.container.pick("div", "home-tray");
  expect(tray).toBeDefined();
  expect(tray?.pick("p", "home-card-label")?.textContent).toBe("s1");
  const choices = tray?.descendants().filter((node) => node.className === "home-choice") ?? [];
  expect(choices).toHaveLength(2);

  choices[0]?.fire("click");

  expect(h.sent).toEqual([
    {
      type: "answer_prompt",
      session_id: "s1",
      since: SINCE,
      choice: { kind: "option", index: 0 },
      request_id: expect.any(String),
    },
  ]);
  h.unmount();
});

/** A preset of repo `a` that launches from the phone: its folder source needs no input. */
const FOLDER_PRESET = {
  id: "plans",
  name: "Plans",
  source_repo_id: "a",
  prompt_sources: [{ kind: "folder", relative_path: "docs/plans" }],
  prompt_template: "{prompt}",
  branch_template: "plan-{index}",
  tab_grouping: { kind: "none" },
  injector: { steps: [] },
} as unknown as PresetEntry;

function sentOf(h: Harness, type: ClientMessage["type"]): ClientMessage[] {
  return h.sent.filter((message) => message.type === type);
}

/** The `request_id` of the latest `list_presets` Home sent. */
function listRequestId(h: Harness): string {
  const list = sentOf(h, "list_presets").at(-1);
  return list?.type === "list_presets" ? (list.request_id ?? "") : "";
}

function togglePresetsOf(h: Harness): void {
  h.container.pick("button", "home-group-presets")?.fire("click");
}

test("a presets toggle sends one list_presets per container per connection", () => {
  const h = launched();
  expect(h.container.pick("button", "home-group-presets")?.textContent).toBe("Presets");

  togglePresetsOf(h);
  togglePresetsOf(h);
  togglePresetsOf(h);
  expect(sentOf(h, "list_presets")).toEqual([
    {
      type: "list_presets",
      target: { kind: "repo", repo_id: "a" },
      request_id: expect.any(String),
    },
  ]);

  // A fresh connection lists the open container again.
  h.notice({ kind: "status", state: "ready" });
  expect(sentOf(h, "list_presets")).toHaveLength(2);
  h.unmount();
});

test("a listed preset launches from its button and cancels from its progress row", () => {
  const h = launched();
  togglePresetsOf(h);
  h.notice({
    kind: "message",
    message: {
      type: "presets",
      target: { kind: "repo", repo_id: "a" },
      entries: [FOLDER_PRESET],
      request_id: listRequestId(h),
    },
  });

  const launch = h.container.pick("button", "home-preset-launch");
  expect(launch?.textContent).toBe("Launch");
  launch?.fire("click");
  const sent = sentOf(h, "launch_preset")[0];
  expect(sent).toEqual({
    type: "launch_preset",
    job_id: expect.stringMatching(/^phone-/),
    target: { kind: "repo", repo_id: "a" },
    preset_id: "plans",
    source: { kind: "folder", path: "C:\\dev\\alpha\\docs\\plans" },
    variable_values: [],
    use_worktree_override: null,
    max_panes_per_tab_override: null,
  });
  const jobId = sent?.type === "launch_preset" ? (sent.job_id ?? "") : "";

  h.notice({
    kind: "message",
    message: {
      type: "preset_launch_job_updated",
      job: {
        job_id: jobId,
        preset_id: "plans",
        target: { kind: "repo", repo_id: "a" },
        status: "spawning",
        launched: 0,
        total: 2,
      },
    },
  });
  const presets = h.container.pick("div", "home-presets");
  expect(presets?.pick("span", "home-row-status")?.textContent).toBe("Launching 0/2");
  const cancel = presets?.pick("button", "home-preset-cancel");
  expect(cancel?.textContent).toBe("Cancel");
  cancel?.fire("click");
  expect(sentOf(h, "cancel_preset_launch")).toEqual([
    { type: "cancel_preset_launch", job_id: jobId },
  ]);
  h.unmount();
});

test("a refused presets list shows the daemon's message in the group", () => {
  const h = launched();
  togglePresetsOf(h);
  h.notice({
    kind: "message",
    message: {
      type: "error",
      request_id: listRequestId(h),
      code: "not_found",
      message: "No such repo or workspace.",
    },
  });

  const presets = h.container.pick("div", "home-presets");
  expect(presets?.pick("p", "home-row-error")?.textContent).toBe("No such repo or workspace.");
  // The rest of Home still draws.
  expect(h.container.pick("button", "home-group-launch")?.textContent).toBe("Launch last");
  h.unmount();
});

test("a tap on New session opens the list and a Start sends one new_session with /nextup", () => {
  const h = launched();
  const toggle = h.container.pick("button", "home-new-session");
  expect(toggle?.textContent).toBe("New session");
  // With no bar and no card, nothing sits above it; the groups come right after it.
  expect(h.container.children[0]).toBe(toggle);
  expect(h.container.children[1]?.className).toBe("home-group");
  expect(h.container.pick("button", "home-new-session-start")).toBeUndefined();

  toggle?.fire("click");
  const start = h.container.pick("button", "home-new-session-start");
  expect(start?.textContent).toBe("Start");
  start?.fire("click");
  h.container.pick("button", "home-new-session-start")?.fire("click");

  expect(h.sent).toEqual([
    {
      type: "new_session",
      target: { kind: "repo", id: "a" },
      initial_prompt: "/nextup",
      request_id: expect.any(String),
    },
  ]);
  expect(h.container.pick("button", "home-new-session-start")?.textContent).toBe("Starting…");
  h.unmount();
});

test("a New session resent to a host without new_session says why and offers no button", () => {
  let newSessionHost = true;
  const h = harness((feature) => feature !== "new_session" || newSessionHost);
  h.notice({ kind: "status", state: "ready" });
  h.notice({ kind: "message", message: repos({ ...ALPHA, last_spawn_config: spawnConfig() }) });
  h.notice({ kind: "message", message: sessions(session("s1")) });
  h.container.pick("button", "home-new-session")?.fire("click");
  h.container.pick("button", "home-new-session-start")?.fire("click");

  // The connection drops before an answer; the host it comes back to lacks the feature.
  newSessionHost = false;
  h.notice({ kind: "status", state: "backoff" });
  h.notice({ kind: "status", state: "ready" });
  h.notice({ kind: "message", message: repos({ ...ALPHA, last_spawn_config: spawnConfig() }) });
  h.notice({ kind: "message", message: sessions(session("s1")) });

  expect(sentOf(h, "new_session")).toHaveLength(1);
  expect(h.container.pick("button", "home-new-session")).toBeUndefined();
  expect(h.container.pick("button", "home-new-session-start")).toBeUndefined();
  const list = h.container.pick("div", "home-presets");
  expect(list?.pick("span", "home-row-label")?.textContent).toBe("alpha");
  expect(list?.pick("p", "home-row-error")?.textContent).toBe(
    "This host can't launch from the phone.",
  );
  h.unmount();
});

test("the returned unsubscribe removes Home's listener", () => {
  const h = harness();
  expect(h.listeners).toHaveLength(1);

  h.unmount();

  expect(h.listeners).toHaveLength(0);
});
