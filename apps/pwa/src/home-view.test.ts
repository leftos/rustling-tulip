import { expect, test } from "vitest";

import { createConnectedLatch } from "./connection-bar";
import { mountHomeView } from "./home-view";
import type {
  ClientMessage,
  DaemonMessage,
  PendingInput,
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

/** Home over a scripted connection whose sends all land. */
function harness(): Harness {
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
    supports: () => true,
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

test("the returned unsubscribe removes Home's listener", () => {
  const h = harness();
  expect(h.listeners).toHaveLength(1);

  h.unmount();

  expect(h.listeners).toHaveLength(0);
});
