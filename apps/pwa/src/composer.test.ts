import { expect, test } from "vitest";

import {
  INITIAL_COMPOSER,
  SEND_BACKSTOP,
  edit,
  foldReply,
  key,
  quick,
  reset,
  submit,
  tick,
  type ComposerModel,
} from "./composer";
import type { ClientMessage, DaemonMessage } from "./protocol";

const NOW = 1_000;

/** A generator giving `req-1`, `req-2`, … in the order it is called. */
function counter(): () => string {
  let next = 0;
  return () => {
    next += 1;
    return `req-${next}`;
  };
}

/** A generator no send may call: it sends nothing. */
function unused(): () => string {
  return () => {
    throw new Error("a request id was generated for a send that sends nothing");
  };
}

/** The model after `text` was typed and sent at {@link NOW}. */
function sent(text: string): ComposerModel {
  return submit(edit(INITIAL_COMPOSER, text), "session-1", counter(), NOW).state;
}

/** The append carrying one user row with `text`. */
function landed(text: string, pending = true): DaemonMessage {
  return {
    type: "chat_append",
    session_id: "session-1",
    entries: [{ kind: "user", id: "u1", text, pending }],
  };
}

/** A refusal of `requestId`. */
function refused(requestId: string, message: string): DaemonMessage {
  return { type: "error", request_id: requestId, code: "chat_refused", message };
}

/** The text a `send_input` message types. */
function typed(message: ClientMessage): string {
  if (message.type !== "send_input") {
    throw new Error(`expected send_input, saw ${message.type}`);
  }
  const binary = atob(message.data_b64);
  return new TextDecoder().decode(Uint8Array.from(binary, (c) => c.charCodeAt(0)));
}

test("composer_edit_sets_the_draft_and_clears_the_error", () => {
  const refusedModel: ComposerModel = {
    draft: "hello",
    sending: null,
    settled: null,
    error: "The session is gone.",
  };

  expect(edit(refusedModel, "hello there")).toEqual({
    draft: "hello there",
    sending: null,
    settled: null,
    error: null,
  });
  expect(edit(INITIAL_COMPOSER, "")).toBe(INITIAL_COMPOSER);
});

test("composer_submit_sends_the_draft_as_typed", () => {
  const result = submit(edit(INITIAL_COMPOSER, "  hello\n"), "session-1", counter(), NOW);

  expect(result.message).toEqual({
    type: "chat_send",
    session_id: "session-1",
    text: "  hello\n",
    request_id: "req-1",
  });
  expect(result.state).toEqual({
    draft: "  hello\n",
    sending: { requestId: "req-1", text: "  hello\n", at: NOW },
    settled: null,
    error: null,
  });
});

test("composer_submit_sends_nothing_on_a_blank_draft", () => {
  const result = submit(edit(INITIAL_COMPOSER, "  \n"), "session-1", unused(), NOW);

  expect(result.message).toBeNull();
  expect(result.state.draft).toBe("  \n");
});

test("composer_submit_sends_nothing_while_a_send_is_out", () => {
  const result = submit(sent("hello"), "session-1", unused(), NOW);

  expect(result.message).toBeNull();
  expect(result.state.sending).toEqual({ requestId: "req-1", text: "hello", at: NOW });
});

test("composer_refusal_keeps_the_draft_and_shows_the_message", () => {
  const state = foldReply(sent("hello"), "session-1", refused("req-1", "The agent has exited."));

  expect(state).toEqual({
    draft: "hello",
    sending: null,
    settled: null,
    error: "The agent has exited.",
  });
});

test("composer_refusal_for_another_request_is_ignored", () => {
  const before = sent("hello");

  expect(foldReply(before, "session-1", refused("req-9", "Another request failed."))).toBe(before);
});

test("composer_settles_only_on_the_matching_pending_row", () => {
  const before = sent("hello");
  const otherText = foldReply(before, "session-1", landed("something else"));
  const notPending = foldReply(before, "session-1", landed("hello", false));
  const otherSession = foldReply(before, "session-1", {
    type: "chat_append",
    session_id: "session-2",
    entries: [{ kind: "user", id: "u1", text: "hello", pending: true }],
  });
  const mine = foldReply(before, "session-1", landed("hello"));

  expect(otherText).toBe(before);
  expect(notPending).toBe(before);
  expect(otherSession).toBe(before);
  expect(mine.sending).toBeNull();
  expect(mine.settled).toEqual({ requestId: "req-1", text: "hello", at: NOW });
});

test("composer_settles_on_the_row_with_the_sent_text_normalised", () => {
  const state = foldReply(sent("  hello\r\n"), "session-1", landed("hello"));

  expect(state.sending).toBeNull();
  expect(state.draft).toBe("");
});

test("composer_keeps_text_typed_during_the_send", () => {
  const typing = edit(sent("hello"), "hello, one more thing");
  const state = foldReply(typing, "session-1", landed("hello"));

  expect(state.sending).toBeNull();
  expect(state.draft).toBe("hello, one more thing");
});

test("composer_landing_clears_the_draft", () => {
  const state = foldReply(sent("hello"), "session-1", landed("hello"));

  expect(state).toEqual({
    draft: "",
    sending: null,
    settled: { requestId: "req-1", text: "hello", at: NOW },
    error: null,
  });
});

test("composer_reset_clears_the_send_and_keeps_the_draft", () => {
  const state = reset(edit(sent("hello"), "more"));

  expect(state).toEqual({ draft: "more", sending: null, settled: null, error: null });
});

test("composer_tick_settles_a_send_past_the_backstop", () => {
  const before = sent("hello");

  expect(tick(before, NOW + SEND_BACKSTOP - 1)).toBe(before);
  expect(tick(before, NOW + SEND_BACKSTOP)).toEqual({
    draft: "hello",
    sending: null,
    settled: { requestId: "req-1", text: "hello", at: NOW },
    error: null,
  });
  expect(tick(INITIAL_COMPOSER, NOW)).toBe(INITIAL_COMPOSER);
});

test("composer_late_refusal_restores_the_text", () => {
  const settled = foldReply(sent("hello"), "session-1", landed("hello"));

  expect(foldReply(settled, "session-1", refused("req-1", "The agent has exited."))).toEqual({
    draft: "hello",
    sending: null,
    settled: null,
    error: "The agent has exited.",
  });
});

test("composer_late_refusal_leaves_a_newer_draft", () => {
  const typing = edit(foldReply(sent("hello"), "session-1", landed("hello")), "a new thought");
  const state = foldReply(typing, "session-1", refused("req-1", "The agent has exited."));

  expect(state.draft).toBe("a new thought");
  expect(state.settled).toBeNull();
  expect(state.error).toBe("The agent has exited.");
});

test("composer_quick_types_the_reply_and_enter", () => {
  const message = quick("session-1", "continue");

  expect(message.type).toBe("send_input");
  expect(typed(message)).toBe("continue\r");
});

test("composer_key_types_the_bytes", () => {
  const message = key("session-1", "\x1b[A");

  expect(message.type).toBe("send_input");
  expect(typed(message)).toBe("\x1b[A");
});
