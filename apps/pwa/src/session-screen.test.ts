import { expect, test } from "vitest";

import { INITIAL_CHAT, chatStep, type ChatState } from "./chat-model";
import type { ConnectionStateName } from "./connection-machine";
import { INITIAL_COMPOSER, unsent as unsentComposer, type ComposerModel } from "./composer";
import { reset as resetHome } from "./home-model";
import { KEY_TRAY } from "./keys";
import {
  NO_SHEET,
  reset as resetSheet,
  swipe,
  unsent as unsentSheet,
  type SheetState,
} from "./prompt-sheet";
import { quickRepliesFor } from "./quick";
import {
  OFFLINE_NOT_SENT,
  opensOnTerminal,
  parseRoute,
  sessionLook,
  sessionScreen,
  terminalWithSnapshot,
  type AnswerCache,
  type DockKind,
  type SessionScreen,
} from "./session-screen";
import type {
  ChatEntry,
  ChatPrompt,
  ChatScreen,
  DaemonMessage,
  SessionMode,
  SessionSnapshot,
} from "./protocol";

const USER: ChatEntry = { kind: "user", id: "u1", text: "Add the reset on a target switch" };
const ANSWER: ChatEntry = { kind: "answer", id: "a1", text: "Added reset_for_target." };
const TOOL: ChatEntry = {
  kind: "tool",
  id: "t1",
  name: "Edit",
  summary: "lib.rs +38 −2",
  input: "apps/native/src/lib.rs",
  result: { text: "Edited 1 file." },
};
const RECEIPT: ChatEntry = {
  kind: "receipt",
  id: "r1",
  question: "Allow Bash?",
  answer: "Allow",
};
const SYSTEM: ChatEntry = { kind: "system", id: "y1", text: "Conversation cleared." };

const PROMPT: ChatPrompt = {
  id: "p1",
  kind: "permission",
  subject: "Bash",
  body: "Run the tests?",
  options: [
    { number: 1, label: "Allow" },
    { number: 2, label: "Deny" },
  ],
};
const READY: ChatScreen = { composer: { state: "ready" } };
const ASKING: ChatScreen = { composer: { state: "dialog" }, prompt: PROMPT };
const PLAN: ChatPrompt = {
  id: "p2",
  kind: "plan",
  subject: "Plan ready",
  body: "",
  options: [
    { number: 1, label: "Approve (auto)" },
    { number: 2, label: "Approve (manual edits)" },
    { number: 3, label: "Tell the agent what to change" },
  ],
};
const PLANNING: ChatScreen = { composer: { state: "dialog" }, prompt: PLAN };

/** A chat the daemon answered, holding `entries`. */
function loaded(entries: readonly ChatEntry[], hidden = 0, look: ChatScreen = READY): ChatState {
  const entered = chatStep(INITIAL_CHAT, { type: "enter", sessionId: "s1" }, () => "req-1").state;
  return chatStep(
    entered,
    {
      type: "message",
      message: {
        type: "chat",
        session_id: "s1",
        entries: [...entries],
        hidden,
        screen: look,
        request_id: "req-1",
      },
    },
    () => "req-2",
  ).state;
}

interface Overrides {
  readonly title?: string;
  readonly ended?: boolean;
  readonly isAgent?: boolean;
  readonly plan?: string | null;
  readonly chat?: ChatState;
  readonly composer?: ComposerModel;
  readonly sheet?: SheetState;
  readonly dock?: DockKind | null;
  readonly offline?: string | null;
  readonly terminal?: boolean;
  readonly ctrlArmed?: boolean;
  readonly mirrorRefused?: boolean;
  readonly status?: ConnectionStateName;
  readonly sendRefused?: boolean;
  readonly connectedOnce?: boolean;
  readonly answers?: AnswerCache;
  readonly now?: number;
}

/** The clock the running-work line reads in these tests. */
const NOW = Date.parse("2026-10-30T12:00:00Z");

/** The screen over the brief's defaults. */
function screen(overrides: Overrides = {}): SessionScreen {
  return sessionScreen({
    title: overrides.title ?? "Session",
    ended: overrides.ended ?? false,
    isAgent: overrides.isAgent ?? false,
    plan: overrides.plan ?? null,
    chat: overrides.chat ?? loaded([]),
    composer: overrides.composer ?? INITIAL_COMPOSER,
    sheet: overrides.sheet ?? NO_SHEET,
    dock: overrides.dock ?? null,
    offline: overrides.offline ?? null,
    terminal: overrides.terminal ?? false,
    ctrlArmed: overrides.ctrlArmed ?? false,
    mirrorRefused: overrides.mirrorRefused ?? false,
    status: overrides.status ?? "ready",
    sendRefused: overrides.sendRefused ?? false,
    connectedOnce: overrides.connectedOnce ?? true,
    answers: overrides.answers ?? new Map(),
    now: overrides.now ?? NOW,
  });
}

test("session_screen_shows_no_bar_before_the_first_connect", () => {
  // A deep link opened cold: nothing has been ready yet, so nothing is coming back.
  expect(screen({ status: "connecting", connectedOnce: false }).bar).toBeNull();
  // Ready once, then dropped: the connection is coming back.
  expect(screen({ status: "backoff", connectedOnce: true }).bar).toBe("Reconnecting…");
});

test("session_screen_shows_reconnecting_while_the_connection_comes_back", () => {
  for (const status of ["connecting", "handshaking", "backoff", "hidden"] as const) {
    expect(screen({ status }).bar, status).toBe("Reconnecting…");
    expect(screen({ status, terminal: true }).bar, `${status} terminal`).toBe("Reconnecting…");
    expect(screen({ status, ended: true }).bar, `${status} ended`).toBe("Reconnecting…");
    // Reconnecting wins over a refused send, as on Home.
    expect(screen({ status, sendRefused: true }).bar, `${status} refused`).toBe("Reconnecting…");
  }
});

test("session_screen_shows_offline_after_a_refused_send", () => {
  expect(screen({ sendRefused: true }).bar).toBe("Offline");
  expect(screen({ sendRefused: true, terminal: true }).bar).toBe("Offline");
});

test("session_screen_shows_no_bar_while_the_connection_is_open", () => {
  expect(screen().bar).toBeNull();
  expect(screen({ terminal: true }).bar).toBeNull();
  expect(screen({ ended: true }).bar).toBeNull();
});

/** One session as Home's model holds it. */
function session(id: string, overrides: Partial<SessionSnapshot> = {}): SessionSnapshot {
  return {
    id,
    label: id,
    kind: "single",
    members: [],
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

test("parseRoute: / is Home", () => {
  expect(parseRoute("/")).toEqual({ kind: "home" });
});

test("parseRoute: /s/abc opens that session", () => {
  expect(parseRoute("/s/abc")).toEqual({ kind: "session", id: "abc" });
});

test("parseRoute: /s/ with no id is Home", () => {
  expect(parseRoute("/s/")).toEqual({ kind: "home" });
});

test("parseRoute: /s/abc/extra is Home", () => {
  expect(parseRoute("/s/abc/extra")).toEqual({ kind: "home" });
});

test("parseRoute: an escaped id is decoded", () => {
  expect(parseRoute("/s/a%20b")).toEqual({ kind: "session", id: "a b" });
});

test("parseRoute: a malformed escape is Home", () => {
  expect(parseRoute("/s/%E0%A4%A")).toEqual({ kind: "home" });
});

test("sessionLook: a listed session gives its title and whether it runs an agent", () => {
  const home = {
    ...resetHome(),
    sessions: [session("s1", { label: "repo", user_label: "work" })],
  };
  expect(sessionLook(home, "s1")).toEqual({
    title: "work",
    isAgent: false,
    ended: false,
    plan: null,
  });
  expect(sessionLook(home, "s2")).toEqual({ title: "", isAgent: false, ended: false, plan: null });
});

test("sessionLook: a session missing once Home loaded has ended", () => {
  const home = { ...resetHome(), sessions: [session("s1")], loaded: true };
  expect(sessionLook(home, "s2")).toEqual({ title: "", isAgent: false, ended: true, plan: null });
  expect(sessionLook(home, "s1").ended).toBe(false);
});

test("sessionLook reads the plan from pending_input", () => {
  const home = {
    ...resetHome(),
    sessions: [
      session("s1", { pending_input: { kind: "plan_approval", plan: "1. Do the thing" } }),
      session("s2", { pending_input: { kind: "plan_approval" } }),
      session("s3", { pending_input: { kind: "question", questions: [] } }),
    ],
  };
  expect(sessionLook(home, "s1").plan).toBe("1. Do the thing");
  expect(sessionLook(home, "s2").plan).toBeNull();
  expect(sessionLook(home, "s3").plan).toBeNull();
  expect(sessionLook(home, "s4").plan).toBeNull();
});

test("a_plain_shell_opens_on_the_terminal", () => {
  expect(opensOnTerminal("plain_shell")).toBe(true);
});

test("an_interactive_session_opens_on_chat", () => {
  expect(opensOnTerminal("interactive")).toBe(false);
  expect(opensOnTerminal("headless")).toBe(false);
  // A mode a newer daemon names, which this build does not know, opens on the chat too.
  expect(opensOnTerminal("future_mode" as unknown as SessionMode)).toBe(false);
});

test("a_late_snapshot_does_not_override_the_toggle", () => {
  // Nothing pressed yet, so the arriving snapshot decides where the screen opens.
  expect(terminalWithSnapshot(null, "plain_shell")).toBe(true);
  expect(terminalWithSnapshot(null, "interactive")).toBe(false);
  // Pressed before the snapshot arrived, the user's own choice stands.
  expect(terminalWithSnapshot(false, "plain_shell")).toBe(false);
  expect(terminalWithSnapshot(true, "interactive")).toBe(true);
});

test("a user entry and an answer draw as bubbles", () => {
  const view = screen({ chat: loaded([USER, ANSWER]) });
  expect(view.rows.map((row) => [row.kind, row.text])).toEqual([
    ["user", "Add the reset on a target switch"],
    ["answer", "Added reset_for_target."],
  ]);
});

test("an answer row carries its Markdown blocks; user, tool and muted rows carry none", () => {
  const answer: ChatEntry = { kind: "answer", id: "a2", text: "**Done**\n\n- one" };
  const view = screen({ chat: loaded([USER, answer, TOOL, RECEIPT]) });
  expect(view.rows.map((row) => [row.kind, row.markdown])).toEqual([
    ["user", undefined],
    [
      "answer",
      [
        { kind: "paragraph", spans: [{ text: "Done", strong: true }] },
        {
          kind: "list",
          ordered: false,
          start: 1,
          items: [{ task: null, blocks: [{ kind: "paragraph", spans: [{ text: "one" }] }] }],
        },
      ],
    ],
    ["tool", undefined],
    ["muted", undefined],
  ]);
  expect(view.rows.filter((row) => "markdown" in row).map((row) => row.kind)).toEqual(["answer"]);
});

test("an answer's blocks are parsed once for the same id and text", () => {
  const answers: AnswerCache = new Map();
  const first = screen({ chat: loaded([ANSWER]), answers }).rows[0]?.markdown;
  const again = screen({ chat: loaded([ANSWER]), answers }).rows[0]?.markdown;
  expect(first).toBeDefined();
  expect(again).toBe(first);
  expect(answers.get("a1")?.blocks).toBe(first);
  const longer: ChatEntry = { kind: "answer", id: "a1", text: "Added reset_for_target. More." };
  const grown = screen({ chat: loaded([longer]), answers }).rows[0]?.markdown;
  expect(grown).not.toBe(first);
  expect(grown).toEqual([
    { kind: "paragraph", spans: [{ text: "Added reset_for_target. More." }] },
  ]);
  // The cache keeps only the answers still in the chat.
  screen({ chat: loaded([USER]), answers });
  expect([...answers.keys()]).toEqual([]);
});

test("a tool call is one line with its name and summary", () => {
  const view = screen({ chat: loaded([TOOL]) });
  expect(view.rows).toHaveLength(1);
  expect(view.rows[0]?.kind).toBe("tool");
  expect(view.rows[0]?.text).toBe("Edit lib.rs +38 −2");
  expect(view.rows[0]?.detail).toContain("Edited 1 file.");
});

test("a receipt draws as one muted line", () => {
  const view = screen({ chat: loaded([RECEIPT]) });
  expect(view.rows.map((row) => [row.kind, row.text])).toEqual([["muted", "Allow Bash? — Allow"]]);
});

test("the other kinds draw a muted line: their own text, the desktop's labels, or the kind", () => {
  // The fold drops a kind this build does not know, so the row is pinned on a
  // state built with one directly.
  const known = loaded([
    SYSTEM,
    { kind: "thinking", id: "k1", secs: 14 },
    { kind: "turn_end", id: "e1", duration_ms: 12_000 },
  ]);
  const unknown = { kind: "mystery", id: "m1" } as unknown as ChatEntry;
  const view = screen({ chat: { ...known, entries: [...known.entries, unknown] } });
  expect(view.rows.map((row) => [row.kind, row.text])).toEqual([
    ["muted", "Conversation cleared."],
    ["muted", "Thought for 14s"],
    ["muted", "Worked for 12s"],
    ["muted", "mystery"],
  ]);
});

test("older rows left out show how many", () => {
  const view = screen({ chat: loaded([USER], 3) });
  expect(view.hiddenLine).toBe("3 earlier messages");
});

test("a refused chat shows its notice and the terminal with the key bar, and no toggle", () => {
  const chat: ChatState = {
    ...INITIAL_CHAT,
    sessionId: "s1",
    phase: "refused",
    notice: "This session's chat is not mirrored.",
  };
  const view = screen({ chat });
  expect(view.notice).toBe("This session's chat is not mirrored.");
  expect(view.terminal).toBe(true);
  expect(view.toggle).toBeNull();
  expect(view.keyBar?.keys).toEqual(KEY_TRAY);
  expect(view.keys).toEqual([]);
  expect(view.composer).toBeNull();
});

test("a refused chat stays on the terminal while it reloads", () => {
  const chat: ChatState = { ...INITIAL_CHAT, sessionId: "s1", phase: "loading" };
  const view = screen({ chat, mirrorRefused: true });
  expect(view.terminal).toBe(true);
  expect(view.toggle).toBeNull();
  expect(view.composer).toBeNull();
  expect(view.notice).toBeNull();
});

test("a dropped key bar tap shows Offline: not sent above the terminal", () => {
  const view = screen({ terminal: true, offline: "Offline: not sent" });
  expect(view.terminal).toBe(true);
  expect(view.notice).toBe("Offline: not sent");
});

test("the chat screen shows the Terminal toggle and no key bar", () => {
  const view = screen();
  expect(view.terminal).toBe(false);
  expect(view.toggle).toBe("Terminal");
  expect(view.keyBar).toBeNull();
  expect(view.composer).not.toBeNull();
});

test("the toggle on shows the terminal and the Chat toggle, and no rows, composer or sheet", () => {
  const view = screen({ terminal: true, chat: loaded([USER, ANSWER], 0, ASKING) });
  expect(view.terminal).toBe(true);
  expect(view.toggle).toBe("Chat");
  expect(view.keyBar).toEqual({ keys: KEY_TRAY, ctrl: "Ctrl", ctrlArmed: false, quick: "Quick" });
  expect(view.rows).toEqual([]);
  expect(view.composer).toBeNull();
  expect(view.sheet).toBeNull();
});

test("an armed Ctrl shows in the key bar", () => {
  expect(screen({ terminal: true, ctrlArmed: true }).keyBar?.ctrlArmed).toBe(true);
  expect(screen({ terminal: true }).keyBar?.ctrlArmed).toBe(false);
});

test("in terminal mode only the Quick dock opens", () => {
  const quick = screen({ terminal: true, dock: "quick", isAgent: true });
  expect(quick.dock?.kind).toBe("quick");
  expect(quick.dock?.groups).toEqual(quickRepliesFor(true));

  expect(screen({ terminal: true, dock: "keys" }).dock).toBeNull();
});

test("an ended session shows its text, and no composer or sheet", () => {
  const view = screen({ ended: true, chat: loaded([USER], 0, ASKING) });
  expect(view.endText).toBe("This session has ended.");
  expect(view.rows).toEqual([]);
  expect(view.composer).toBeNull();
  expect(view.sheet).toBeNull();
});

test("an ended session shows no terminal, toggle or key bar even with the toggle on", () => {
  const view = screen({ ended: true, terminal: true });
  expect(view.terminal).toBe(false);
  expect(view.toggle).toBeNull();
  expect(view.keyBar).toBeNull();
});

test("Send is disabled while the box is blank or a send is out", () => {
  expect(screen().composer?.sendEnabled).toBe(false);
  expect(screen({ composer: { ...INITIAL_COMPOSER, draft: "   " } }).composer?.sendEnabled).toBe(
    false,
  );
  expect(screen({ composer: { ...INITIAL_COMPOSER, draft: "hello" } }).composer?.sendEnabled).toBe(
    true,
  );
  const sending: ComposerModel = {
    ...INITIAL_COMPOSER,
    draft: "hello",
    sending: { requestId: "r1", text: "hello", at: 0 },
  };
  expect(screen({ composer: sending }).composer?.sendEnabled).toBe(false);
});

test("one dock is open at a time", () => {
  const keys = screen({ dock: "keys", isAgent: true });
  expect(keys.dock?.kind).toBe("keys");
  expect(keys.dock?.keys).toHaveLength(KEY_TRAY.length);
  expect(keys.dock?.groups).toEqual([]);

  const quick = screen({ dock: "quick", isAgent: true });
  expect(quick.dock?.kind).toBe("quick");
  expect(quick.dock?.groups).toEqual(quickRepliesFor(true));
  expect(quick.dock?.keys).toEqual([]);

  expect(screen({ dock: null }).dock).toBeNull();
});

test("the sheet draws the prompt's options with key-caps, and a swipe puts it away", () => {
  const shown = screen({ chat: loaded([], 0, ASKING) });
  expect(shown.sheet?.title).toBe("Bash");
  expect(shown.sheet?.buttons.slice(0, 2).map((button) => [button.number, button.label])).toEqual([
    ["1", "Allow"],
    ["2", "Deny"],
  ]);
  expect(shown.sheet?.keys).toEqual(["Esc", "↑", "↓"]);

  const dismissed = screen({ chat: loaded([], 0, ASKING), sheet: swipe(NO_SHEET, ASKING) });
  expect(dismissed.sheet).toBeNull();

  const next: ChatScreen = { ...ASKING, prompt: { ...PROMPT, id: "p2" } };
  expect(
    screen({ chat: loaded([], 0, next), sheet: swipe(NO_SHEET, ASKING) }).sheet,
  ).not.toBeNull();

  expect(screen({ chat: loaded([]) }).sheet).toBeNull();
});

test("the sheet shows the session's plan", () => {
  const view = screen({ chat: loaded([], 0, PLANNING), plan: "1. Do the thing" });

  expect(view.sheet?.plan).toBe("1. Do the thing");
});

test("a session with no plan approval shows no plan", () => {
  const view = screen({ chat: loaded([], 0, PLANNING) });

  expect(view.sheet?.plan).toBeNull();
});

test("a plan that arrives later fills the same sheet", () => {
  const early = screen({ chat: loaded([], 0, PLANNING) });
  const late = screen({ chat: loaded([], 0, PLANNING), plan: "1. Do the thing" });

  expect(early.sheet?.id).toBe("p2");
  expect(late.sheet?.id).toBe("p2");
  expect(early.sheet?.plan).toBeNull();
  expect(late.sheet?.plan).toBe("1. Do the thing");
});

test("a send the connection dropped shows Offline: not sent and keeps the draft", () => {
  const sending: ComposerModel = {
    ...INITIAL_COMPOSER,
    draft: "hello",
    sending: { requestId: "r1", text: "hello", at: 0 },
  };
  const dropped = unsentComposer(sending, "r1", OFFLINE_NOT_SENT);
  expect(dropped.sending).toBeNull();
  expect(dropped.draft).toBe("hello");
  expect(screen({ composer: dropped }).composer?.error).toBe("Offline: not sent");
});

test("a Key or Quick tap the connection dropped shows Offline: not sent", () => {
  expect(screen({ offline: OFFLINE_NOT_SENT }).composer?.error).toBe("Offline: not sent");
});

test("a dropped answer unlocks the sheet and shows Offline: not sent", () => {
  const pending: SheetState = {
    dismissed: null,
    pending: { promptId: "p1", requestId: "r1" },
    error: null,
  };
  const unlocked = unsentSheet(pending, "p1", OFFLINE_NOT_SENT);
  expect(unlocked.pending).toBeNull();
  expect(screen({ chat: loaded([], 0, ASKING), sheet: unlocked }).sheet?.error).toBe(
    "Offline: not sent",
  );
});

test("ready drops the sheet's pending answer and its error", () => {
  const pending: SheetState = {
    dismissed: null,
    pending: { promptId: "p1", requestId: "r1" },
    error: { promptId: "p1", text: "That prompt is gone." },
  };
  const cleared = resetSheet(pending);
  expect(cleared.pending).toBeNull();
  expect(cleared.error).toBeNull();
});

test("a refusal and a sheet error show their text verbatim", () => {
  const composer: ComposerModel = { ...INITIAL_COMPOSER, draft: "hi", error: "Not connected." };
  expect(screen({ composer }).composer?.error).toBe("Not connected.");

  const sheet: SheetState = {
    dismissed: null,
    pending: null,
    error: { promptId: "p1", text: "That prompt is gone." },
  };
  expect(screen({ chat: loaded([], 0, ASKING), sheet }).sheet?.error).toBe("That prompt is gone.");
});

/** A loaded chat the daemon then sent `chat_work` for. */
function working(work: Omit<Extract<DaemonMessage, { type: "chat_work" }>, "type" | "session_id">) {
  return chatStep(
    loaded([]),
    { type: "message", message: { type: "chat_work", session_id: "s1", ...work } },
    () => "req-3",
  ).state;
}

test("the session screen shows the running-work line from ChatWork", () => {
  const chat = working({
    subagents: [{ id: "a1", description: "Sleeper A" }],
    shells: [
      {
        id: "b1",
        description: "",
        command: "cargo test",
        since: new Date(NOW - 3 * 60_000).toISOString(),
      },
    ],
    wakeup_at: new Date(NOW + 4 * 60_000).toISOString(),
  });
  expect(screen({ chat }).work).toBe(
    "1 subagent · 1 background shell (cargo test, 3m) · wakeup in 4m",
  );
  // The ages and countdowns read the clock the screen is built at.
  expect(screen({ chat, now: NOW + 60_000 }).work).toBe(
    "1 subagent · 1 background shell (cargo test, 4m) · wakeup in 3m",
  );
  // The terminal and the ended screen draw no chat, so no line either.
  expect(screen({ chat, terminal: true }).work).toBeNull();
  expect(screen({ chat, ended: true }).work).toBeNull();
});

test("no line when ChatWork is empty", () => {
  expect(screen().work).toBeNull();
  expect(screen({ chat: working({ subagents: [], shells: [] }) }).work).toBeNull();
});
