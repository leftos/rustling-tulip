import { expect, test } from "vitest";

import {
  DISPLAY_CAP,
  INITIAL_CHAT,
  chatStep,
  latestPrompt,
  type ChatEvent,
  type ChatState,
} from "./chat-model";
import type { ChatEntry, ChatPrompt, ChatScreen } from "./protocol";

const USER: ChatEntry = { kind: "user", id: "u1", text: "Fix the flaky test." };
const FIXED: ChatEntry = { kind: "user", id: "u1", text: "Fixed." };
const ANSWER: ChatEntry = { kind: "answer", id: "a1", text: "Running it again.", live: true };
const FINISHED: ChatEntry = { kind: "answer", id: "a2", text: "It passes now." };
const PROMPT: ChatPrompt = {
  id: "p1",
  kind: "permission",
  subject: "Bash",
  body: "Run the tests?",
};
const SCREEN: ChatScreen = { composer: { state: "dialog" }, prompt: PROMPT };

const ENTER: ChatEvent = { type: "enter", sessionId: "s1" };

/** A state whose load the daemon answered. */
const LOADED: ChatState = chatStep(
  chatStep(INITIAL_CHAT, ENTER, () => "req-1").state,
  {
    type: "message",
    message: {
      type: "chat",
      session_id: "s1",
      entries: [USER, ANSWER],
      hidden: 0,
      screen: { composer: { state: "working" } },
      request_id: "req-1",
    },
  },
  () => "req-unused",
).state;

/** An id generator that hands out `req-1`, `req-2`, … in turn. */
function counter(): () => string {
  let n = 0;
  return () => `req-${++n}`;
}

/** A row kind this build does not know, as a daemon newer than it may send. */
function unknownRow(id: string): ChatEntry {
  return { kind: "mystery", id, text: "?" } as unknown as ChatEntry;
}

test("chat_enter_loads_with_a_request_id", () => {
  expect(chatStep(INITIAL_CHAT, ENTER, counter())).toEqual({
    state: {
      sessionId: "s1",
      phase: "loading",
      entries: [],
      hidden: 0,
      screen: null,
      notice: null,
      requestId: "req-1",
      gone: new Set(),
    },
    effects: [{ type: "load_chat", session_id: "s1", request_id: "req-1" }],
  });
});

test("chat_reply_fills_entries_hidden_and_screen", () => {
  const loading = chatStep(INITIAL_CHAT, ENTER, counter()).state;

  expect(
    chatStep(
      loading,
      {
        type: "message",
        message: {
          type: "chat",
          session_id: "s1",
          entries: [USER, ANSWER],
          hidden: 12,
          screen: SCREEN,
          request_id: "req-1",
        },
      },
      counter(),
    ),
  ).toEqual({
    state: {
      sessionId: "s1",
      phase: "loaded",
      entries: [USER, ANSWER],
      hidden: 12,
      screen: SCREEN,
      notice: null,
      requestId: null,
      gone: new Set(),
    },
    effects: [],
  });
});

test("chat_for_another_session_is_ignored", () => {
  const step = chatStep(
    LOADED,
    {
      type: "message",
      message: {
        type: "chat",
        session_id: "s2",
        entries: [FINISHED],
        hidden: 0,
        screen: SCREEN,
      },
    },
    counter(),
  );

  expect(step.state).toBe(LOADED);
  expect(step.effects).toEqual([]);
});

test("chat_append_appends_and_removes", () => {
  const step = chatStep(
    LOADED,
    {
      type: "message",
      message: {
        type: "chat_append",
        session_id: "s1",
        entries: [FINISHED, { kind: "answer", id: "a1", text: "It passes now." }],
        removed: ["u1"],
      },
    },
    counter(),
  );

  expect(step.state.entries).toEqual([
    { kind: "answer", id: "a1", text: "It passes now." },
    FINISHED,
  ]);
});

const PENDING: ChatEntry = { kind: "user", id: "pending:1", text: "Also the docs.", pending: true };
const PENDING_LATER: ChatEntry = {
  kind: "user",
  id: "pending:2",
  text: "And the log.",
  pending: true,
};
const TOOL: ChatEntry = {
  kind: "tool",
  id: "t1",
  name: "Read",
  summary: "src/chat.rs",
  input: "{}",
};
const CONFIRMED: ChatEntry = { kind: "answer", id: "a3", text: "Done." };
const LIVE: ChatEntry = { kind: "answer", id: "live:m1", text: "Working.", live: true };
const SETTLED: ChatEntry = { kind: "user", id: "u2", text: "Also the docs." };

/** `at` after one append of `entries`. */
function appended(at: ChatState, entries: ChatEntry[]): ChatState {
  return chatStep(
    at,
    { type: "message", message: { type: "chat_append", session_id: "s1", entries } },
    counter(),
  ).state;
}

/** The ids of the state's rows, in display order. */
function rowIds(state: ChatState): string[] {
  return state.entries.map((entry) => entry.id);
}

test("chat_pending_prompt_stays_below_entries_that_arrive_after_it", () => {
  const state = appended(appended(LOADED, [PENDING]), [CONFIRMED, TOOL]);

  expect(rowIds(state)).toEqual(["u1", "a1", "a3", "t1", "pending:1"]);
});

test("chat_two_pending_prompts_keep_their_order", () => {
  const state = appended(appended(LOADED, [PENDING, PENDING_LATER]), [CONFIRMED]);

  expect(rowIds(state)).toEqual(["u1", "a1", "a3", "pending:1", "pending:2"]);
});

test("chat_live_answer_lands_above_a_pending_prompt", () => {
  const state = appended(appended(LOADED, [PENDING]), [LIVE]);

  expect(rowIds(state)).toEqual(["u1", "a1", "live:m1", "pending:1"]);
});

test("chat_a_removed_pending_prompt_leaves_the_next_one_waiting", () => {
  const queued = appended(LOADED, [PENDING, PENDING_LATER]);
  const step = chatStep(
    queued,
    {
      type: "message",
      message: {
        type: "chat_append",
        session_id: "s1",
        entries: [SETTLED],
        removed: ["pending:1"],
      },
    },
    counter(),
  );

  expect(rowIds(step.state)).toEqual(["u1", "a1", "u2", "pending:2"]);
});

test("chat_screen_changed_replaces_the_screen_and_latest_prompt_follows_it", () => {
  expect(latestPrompt(LOADED)).toBeNull();

  const step = chatStep(
    LOADED,
    { type: "message", message: { type: "chat_screen_changed", session_id: "s1", screen: SCREEN } },
    counter(),
  );

  expect(step.state.screen).toBe(SCREEN);
  expect(latestPrompt(step.state)).toBe(PROMPT);
  expect(latestPrompt({ ...LOADED, screen: { composer: { state: "ready" } } })).toBeNull();
});

test("chat_leave_unloads_and_clears", () => {
  const loading = chatStep(INITIAL_CHAT, ENTER, counter()).state;

  for (const state of [LOADED, loading]) {
    expect(chatStep(state, { type: "leave" }, counter())).toEqual({
      state: INITIAL_CHAT,
      effects: [{ type: "unload_chat", session_id: "s1" }],
    });
  }
});

test("chat_hidden_unloads", () => {
  expect(chatStep(LOADED, { type: "hidden" }, counter())).toEqual({
    state: INITIAL_CHAT,
    effects: [{ type: "unload_chat", session_id: "s1" }],
  });
});

test("chat_ready_reloads_the_entered_session", () => {
  const step = chatStep(LOADED, { type: "ready" }, counter());

  expect(step.state.phase).toBe("loading");
  expect(step.state.requestId).toBe("req-1");
  expect(step.state.entries).toEqual([USER, ANSWER]);
  expect(step.effects).toEqual([{ type: "load_chat", session_id: "s1", request_id: "req-1" }]);
});

test("chat_enter_another_session_unloads_the_first_and_loads_the_second", () => {
  const step = chatStep(LOADED, { type: "enter", sessionId: "s2" }, counter());

  expect(step.state.sessionId).toBe("s2");
  expect(step.state.phase).toBe("loading");
  expect(step.effects).toEqual([
    { type: "unload_chat", session_id: "s1" },
    { type: "load_chat", session_id: "s2", request_id: "req-1" },
  ]);
});

test("chat_error_echoing_the_load_refuses_with_the_message_verbatim", () => {
  const loading = chatStep(INITIAL_CHAT, ENTER, counter()).state;
  const reason = "This session's agent does not offer a chat.";

  expect(
    chatStep(
      loading,
      { type: "message", message: { type: "error", message: reason, request_id: "req-1" } },
      counter(),
    ),
  ).toEqual({
    state: {
      sessionId: "s1",
      phase: "refused",
      entries: [],
      hidden: 0,
      screen: null,
      notice: reason,
      requestId: null,
      gone: new Set(),
    },
    effects: [],
  });
});

test("chat_error_for_another_request_is_ignored", () => {
  const loading = chatStep(INITIAL_CHAT, ENTER, counter()).state;

  for (const requestId of ["req-other", null]) {
    const step = chatStep(
      loading,
      { type: "message", message: { type: "error", message: "Nope.", request_id: requestId } },
      counter(),
    );

    expect(step.state).toBe(loading);
    expect(step.effects).toEqual([]);
  }
});

test("chat_display_cap_trims_the_oldest_rows_into_hidden", () => {
  const rows: ChatEntry[] = Array.from({ length: DISPLAY_CAP + 2 }, (_, n): ChatEntry => ({
    kind: "system",
    id: `r${n}`,
    text: `Row ${n}`,
  }));
  const loading = chatStep(INITIAL_CHAT, ENTER, counter()).state;
  const loaded = chatStep(
    loading,
    {
      type: "message",
      message: {
        type: "chat",
        session_id: "s1",
        entries: rows,
        hidden: 3,
        screen: SCREEN,
        request_id: "req-1",
      },
    },
    counter(),
  ).state;

  expect(loaded.entries).toHaveLength(DISPLAY_CAP);
  expect(loaded.entries[0]).toEqual(rows[2]);
  expect(loaded.hidden).toBe(5);

  const appended = chatStep(
    loaded,
    { type: "message", message: { type: "chat_append", session_id: "s1", entries: [FINISHED] } },
    counter(),
  ).state;

  expect(appended.entries).toHaveLength(DISPLAY_CAP);
  expect(appended.entries[DISPLAY_CAP - 1]).toEqual(FINISHED);
  expect(appended.hidden).toBe(6);
  expect(appended.gone.has("r2")).toBe(true);
});

test("chat_unknown_entry_kinds_are_dropped", () => {
  const loaded = chatStep(
    LOADED,
    {
      type: "message",
      message: {
        type: "chat",
        session_id: "s1",
        entries: [USER, unknownRow("x1"), ANSWER],
        hidden: 0,
        screen: SCREEN,
      },
    },
    counter(),
  ).state;

  expect(loaded.entries).toEqual([USER, ANSWER]);

  const appended = chatStep(
    LOADED,
    {
      type: "message",
      message: { type: "chat_append", session_id: "s1", entries: [unknownRow("x2")] },
    },
    counter(),
  ).state;

  expect(appended.entries).toEqual(LOADED.entries);
  expect(appended.hidden).toBe(LOADED.hidden);
});

test("chat_removed_ids_are_ignored_until_the_next_load", () => {
  const removedStep = chatStep(
    LOADED,
    { type: "message", message: { type: "chat_append", session_id: "s1", removed: ["u1"] } },
    counter(),
  ).state;

  expect(removedStep.entries).toEqual([ANSWER]);
  expect(removedStep.gone.has("u1")).toBe(true);

  const late = chatStep(
    removedStep,
    { type: "message", message: { type: "chat_append", session_id: "s1", entries: [USER] } },
    counter(),
  ).state;

  expect(late.entries).toEqual([ANSWER]);

  const reloaded = chatStep(
    removedStep,
    {
      type: "message",
      message: {
        type: "chat",
        session_id: "s1",
        entries: [USER, ANSWER],
        hidden: 0,
        screen: SCREEN,
      },
    },
    counter(),
  ).state;

  expect(reloaded.gone.size).toBe(0);

  const again = chatStep(
    reloaded,
    { type: "message", message: { type: "chat_append", session_id: "s1", entries: [FIXED] } },
    counter(),
  ).state;

  expect(again.entries).toEqual([FIXED, ANSWER]);

  // An append that removes and carries one id brings it back at the bottom.
  const carried = chatStep(
    reloaded,
    {
      type: "message",
      message: { type: "chat_append", session_id: "s1", removed: ["u1"], entries: [FIXED] },
    },
    counter(),
  ).state;

  expect(carried.entries).toEqual([ANSWER, FIXED]);
});

test("chat work is kept", () => {
  const shells = [{ id: "b1", description: "Watch the build", command: "pnpm dev" }];
  const kept = chatStep(
    LOADED,
    {
      type: "message",
      message: { type: "chat_work", session_id: "s1", shells, wakeup_at: "2026-10-30T12:04:00Z" },
    },
    counter(),
  );
  expect(kept).toEqual({
    state: { ...LOADED, work: { subagents: [], shells, wakeupAt: "2026-10-30T12:04:00Z" } },
    effects: [],
  });
  // Another session's work is not this chat's.
  const other = chatStep(
    LOADED,
    { type: "message", message: { type: "chat_work", session_id: "s2", shells } },
    counter(),
  );
  expect(other.state.work).toBeUndefined();
  // Leaving the screen drops it with the rest of the chat.
  expect(chatStep(kept.state, { type: "leave" }, counter()).state.work).toBeUndefined();
});
