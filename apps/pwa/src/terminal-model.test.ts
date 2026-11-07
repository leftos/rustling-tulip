import { expect, test, vi } from "vitest";

import type { DaemonMessage } from "./protocol";
import {
  INITIAL_TERMINAL,
  REQUEST_TIMEOUT,
  RETRY_DELAYS,
  attach,
  detach,
  fold,
  ready,
  tick,
  type TerminalTransition,
} from "./terminal-model";

const T0 = 1_000;

/** A generator giving `req-1`, `req-2`, … in the order it is called. */
function counter(): () => string {
  let next = 0;
  return () => {
    next += 1;
    return `req-${next}`;
  };
}

/** `text` as base64, the way `keys.ts` encodes a send. */
function b64(text: string): string {
  let binary = "";
  for (const byte of new TextEncoder().encode(text)) {
    binary += String.fromCharCode(byte);
  }
  return btoa(binary);
}

function text(bytes: Uint8Array): string {
  return new TextDecoder().decode(bytes);
}

/** The daemon's live output for a session. */
function output(sessionId: string, data: string): DaemonMessage {
  return { type: "pty_output", session_id: sessionId, data_b64: b64(data) };
}

/** The daemon's reply to one `load_scrollback`, or its resync when `requestId` is null. */
function scrollback(
  sessionId: string,
  data: string,
  requestId: string | null,
  forwarderRestarted = false,
): DaemonMessage {
  return {
    type: "scrollback",
    session_id: sessionId,
    data_b64: b64(data),
    truncated: false,
    request_id: requestId,
    forwarder_restarted: forwarderRestarted,
  };
}

/** The transition that ended a load after its whole schedule. */
function failedLoad(): TerminalTransition {
  const ids = counter();
  let current = attach(INITIAL_TERMINAL, "s1", ids, T0);
  while (current.state.phase === "loading") {
    const deadline = current.state.deadline;
    if (deadline === null) {
      break;
    }
    current = tick(current.state, deadline, ids);
  }
  expect(current.state.phase).toBe("timed-out");
  return current;
}

test("terminal_attach_asks_for_the_scrollback_with_a_fresh_request_id", () => {
  const attached = attach(INITIAL_TERMINAL, "s1", counter(), T0);
  expect(attached.messages).toEqual([
    { type: "load_scrollback", session_id: "s1", request_id: "req-1" },
  ]);
  expect(attached.state.phase).toBe("loading");
  expect(attached.state.attempt).toBe(1);
  expect(attached.state.backoff).toBe(false);
  expect(attached.state.deadline).toBe(T0 + REQUEST_TIMEOUT);
  expect(attached.writes).toEqual([]);
  expect(attached.reset).toBe(false);
});

test("terminal_attach_to_another_session_detaches_the_first", () => {
  const ids = counter();
  const first = attach(INITIAL_TERMINAL, "s1", ids, T0);
  const second = attach(first.state, "s2", ids, T0 + 100);
  expect(second.messages).toEqual([
    { type: "detach", session_id: "s1" },
    { type: "load_scrollback", session_id: "s2", request_id: "req-2" },
  ]);
  expect(second.state.sessionId).toBe("s2");
});

test("terminal_held_output_follows_the_history_in_order", () => {
  const attached = attach(INITIAL_TERMINAL, "s1", () => "req-1", T0);
  const first = fold(attached.state, output("s1", "a"));
  const second = fold(first.state, output("s1", "b"));
  expect(first.writes).toEqual([]);
  expect(second.writes).toEqual([]);

  const replied = fold(second.state, scrollback("s1", "history", "req-1"));
  expect(replied.writes.map(text)).toEqual(["history", "a", "b"]);
  expect(replied.state.phase).toBe("loaded");
  expect(replied.state.held).toEqual([]);

  const live = fold(replied.state, output("s1", "c"));
  expect(live.writes.map(text)).toEqual(["c"]);
});

test("terminal_a_reply_to_an_older_request_is_dropped", () => {
  const ids = counter();
  const attached = attach(INITIAL_TERMINAL, "s1", ids, T0);
  const timedOut = tick(attached.state, T0 + REQUEST_TIMEOUT, ids);
  const retried = tick(timedOut.state, T0 + REQUEST_TIMEOUT + RETRY_DELAYS[0], ids);
  expect(retried.state.requestId).toBe("req-2");

  const stale = fold(retried.state, scrollback("s1", "old", "req-1"));
  expect(stale.writes).toEqual([]);
  expect(stale.state.phase).toBe("loading");

  const fresh = fold(retried.state, scrollback("s1", "new", "req-2"));
  expect(fresh.writes.map(text)).toEqual(["new"]);
  expect(fresh.state.phase).toBe("loaded");
});

test("terminal_a_forwarder_restart_discards_the_held_output", () => {
  const attached = attach(INITIAL_TERMINAL, "s1", () => "req-1", T0);
  const held = fold(attached.state, output("s1", "held"));
  expect(held.state.held).toHaveLength(1);

  const restarted = fold(held.state, scrollback("s1", "history", "req-1", true));
  expect(restarted.writes.map(text)).toEqual(["history"]);
  expect(restarted.state.phase).toBe("loaded");
});

test("terminal_tick_runs_the_desktop_timeout_and_retry_schedule", () => {
  const ids = counter();
  const attached = attach(INITIAL_TERMINAL, "s1", ids, T0);
  const held = fold(attached.state, output("s1", "before"));

  const t1 = T0 + REQUEST_TIMEOUT;
  const t2 = t1 + RETRY_DELAYS[0];
  const t3 = t2 + REQUEST_TIMEOUT;
  const t4 = t3 + RETRY_DELAYS[1];
  const t5 = t4 + REQUEST_TIMEOUT;

  const early = tick(held.state, t1 - 1, ids);
  expect(early.messages).toEqual([]);
  expect(early.state.backoff).toBe(false);

  const timedOut = tick(held.state, t1, ids);
  expect(timedOut.messages).toEqual([]);
  expect(timedOut.state.backoff).toBe(true);
  expect(timedOut.state.deadline).toBe(t2);

  const retried = tick(timedOut.state, t2, ids);
  expect(retried.messages).toEqual([
    { type: "load_scrollback", session_id: "s1", request_id: "req-2" },
  ]);
  expect(retried.state.attempt).toBe(2);
  expect(retried.state.backoff).toBe(false);
  expect(retried.state.deadline).toBe(t3);
  // The retry's snapshot carries everything held back so far.
  expect(retried.state.held).toEqual([]);

  const heldAgain = fold(retried.state, output("s1", "during"));
  const timedOutAgain = tick(heldAgain.state, t3, ids);
  expect(timedOutAgain.state.deadline).toBe(t4);

  const retriedAgain = tick(timedOutAgain.state, t4, ids);
  expect(retriedAgain.messages).toEqual([
    { type: "load_scrollback", session_id: "s1", request_id: "req-3" },
  ]);
  expect(retriedAgain.state.deadline).toBe(t5);

  const late = fold(retriedAgain.state, output("s1", "late"));
  const failed = tick(late.state, t5, ids);
  expect(failed.messages).toEqual([]);
  expect(failed.state.phase).toBe("timed-out");
  expect(failed.state.deadline).toBeNull();
  // Output held when the load gave up is live: it is written, not dropped.
  expect(failed.writes.map(text)).toEqual(["late"]);
});

test("terminal_a_finished_load_asks_for_the_size_again", () => {
  const attached = attach(INITIAL_TERMINAL, "s1", () => "req-1", T0);
  const loaded = fold(attached.state, scrollback("s1", "history", "req-1"));
  expect(loaded.resize).toBe(true);

  expect(failedLoad().resize).toBe(true);
});

test("terminal_undecodable_bytes_leave_the_state_alone", () => {
  const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
  try {
    const attached = attach(INITIAL_TERMINAL, "s1", () => "req-1", T0);

    const badOutput: DaemonMessage = { type: "pty_output", session_id: "s1", data_b64: "%%%" };
    const folded = fold(attached.state, badOutput);
    expect(folded.writes).toEqual([]);
    expect(folded.state).toEqual(attached.state);

    const badReply: DaemonMessage = {
      type: "scrollback",
      session_id: "s1",
      data_b64: "***",
      truncated: false,
      request_id: "req-1",
    };
    const replied = fold(attached.state, badReply);
    expect(replied.writes).toEqual([]);
    expect(replied.state).toEqual(attached.state);

    expect(warn).toHaveBeenCalled();
  } finally {
    warn.mockRestore();
  }
});

test("terminal_a_resync_after_the_load_clears_and_replays", () => {
  const attached = attach(INITIAL_TERMINAL, "s1", () => "req-1", T0);
  const loaded = fold(attached.state, scrollback("s1", "history", "req-1"));
  expect(loaded.state.phase).toBe("loaded");

  const resynced = fold(loaded.state, scrollback("s1", "fresh", null, true));
  expect(resynced.reset).toBe(true);
  expect(resynced.writes.map(text)).toEqual(["fresh"]);
  expect(resynced.resize).toBe(true);
  expect(resynced.state.phase).toBe("loaded");
  expect(resynced.state.requestId).toBeNull();
});

test("terminal_a_resync_after_a_timed_out_load_clears_and_replays", () => {
  const resynced = fold(failedLoad().state, scrollback("s1", "fresh", null, true));
  expect(resynced.reset).toBe(true);
  expect(resynced.writes.map(text)).toEqual(["fresh"]);
  expect(resynced.resize).toBe(true);
  expect(resynced.state.phase).toBe("loaded");
});

test("terminal_a_resync_while_a_load_is_pending_is_dropped", () => {
  const attached = attach(INITIAL_TERMINAL, "s1", () => "req-1", T0);
  const held = fold(attached.state, output("s1", "a"));

  const dropped = fold(held.state, scrollback("s1", "fresh", null, true));
  expect(dropped.writes).toEqual([]);
  expect(dropped.reset).toBe(false);
  expect(dropped.resize).toBe(false);
  expect(dropped.state).toEqual(held.state);
});

test("terminal_detach_sends_detach_and_clears_the_load", () => {
  const attached = attach(INITIAL_TERMINAL, "s1", () => "req-1", T0);
  const held = fold(attached.state, output("s1", "x"));

  const left = detach(held.state);
  expect(left.messages).toEqual([{ type: "detach", session_id: "s1" }]);
  expect(left.state).toEqual(INITIAL_TERMINAL);

  expect(detach(INITIAL_TERMINAL).messages).toEqual([]);
});

test("terminal_ready_reconnects_with_a_new_request", () => {
  const ids = counter();
  const attached = attach(INITIAL_TERMINAL, "s1", ids, T0);
  const held = fold(attached.state, output("s1", "x"));

  const back = ready(held.state, ids, T0 + 5_000);
  expect(back.messages).toEqual([
    { type: "load_scrollback", session_id: "s1", request_id: "req-2" },
  ]);
  expect(back.state.requestId).toBe("req-2");
  expect(back.state.held).toEqual([]);
  expect(back.state.deadline).toBe(T0 + 5_000 + REQUEST_TIMEOUT);

  expect(ready(INITIAL_TERMINAL, ids, T0).messages).toEqual([]);
});
