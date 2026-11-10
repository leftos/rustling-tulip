import { expect, test } from "vitest";

import {
  NO_ACTIONS,
  answer,
  disarm,
  errorFor,
  foldReply,
  reset,
  resume,
  stop,
  type HomeActions,
} from "./home-actions";
import type { SessionSnapshot } from "./protocol";

const SINCE = "2026-11-06T12:00:00Z";

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

/** A generator giving `req-1`, `req-2`, … in the order it is called. */
function counter(): () => string {
  let next = 0;
  return () => {
    next += 1;
    return `req-${next}`;
  };
}

/** A generator no tap may call: it sends nothing. */
function unused(): () => string {
  return () => {
    throw new Error("a request id was generated for a tap that sends nothing");
  };
}

function refused(newRequestId: () => string = counter()): HomeActions {
  const sent = answer(NO_ACTIONS, newRequestId, "s1", SINCE, { kind: "allow" });
  return foldReply(sent.state, {
    type: "error",
    request_id: "req-1",
    code: "chat_refused",
    message: "the prompt is no longer up",
  }).state;
}

test("home_answer_builds_answer_prompt_with_the_stamp", () => {
  const result = answer(NO_ACTIONS, counter(), "s1", SINCE, { kind: "option", index: 1 });

  expect(result.message).toEqual({
    type: "answer_prompt",
    session_id: "s1",
    since: SINCE,
    choice: { kind: "option", index: 1 },
    request_id: "req-1",
  });
  expect(result.state.pending).toEqual([{ requestId: "req-1", sessionId: "s1", action: "answer" }]);
});

test("home_stop_needs_two_taps", () => {
  const first = stop(NO_ACTIONS, "s1");
  expect(first.message).toBeNull();
  expect(first.state.armed).toBe("s1");

  const second = stop(first.state, "s1");
  expect(second.message).toEqual({ type: "stop_session", session_id: "s1", cleanup: [] });
  expect(second.state.armed).toBeNull();
});

test("home_stop_arming_another_session_disarms_the_first", () => {
  const armed = stop(NO_ACTIONS, "s1").state;

  const other = stop(armed, "s2");
  expect(other.message).toBeNull();
  expect(other.state.armed).toBe("s2");

  // The first session's Stop was disarmed: a tap on it arms again, not sends.
  const back = stop(other.state, "s1");
  expect(back.message).toBeNull();
  expect(back.state.armed).toBe("s1");
});

test("home_disarm_drops_the_armed_stop", () => {
  const armed = stop(NO_ACTIONS, "s1").state;

  expect(disarm(armed).armed).toBeNull();
  expect(disarm(NO_ACTIONS)).toBe(NO_ACTIONS);
});

test("home_reset_forgets_pending_armed_and_errors", () => {
  const armed = stop(NO_ACTIONS, "s1").state;
  const state = refused();
  expect(state.errors.size).toBeGreaterThan(0);

  expect(reset()).toEqual(NO_ACTIONS);
  expect(reset().pending).toEqual([]);
  expect(reset().armed).toBeNull();
  expect(reset().errors.size).toBe(0);
  expect(armed.armed).toBe("s1");
});

test("home_resume_for_a_stopped_session_is_a_duplicate", () => {
  const stopped = [
    session("s1", { status: "stopped" }),
    session("s2", { status: "error" }),
    session("s3", { is_inactive: true }),
  ];

  for (const target of stopped) {
    const result = resume(NO_ACTIONS, counter(), target, false);
    expect(result.message).toEqual({
      type: "duplicate_session",
      session_id: target.id,
      request_id: "req-1",
      carry_busy_excluded: true,
      same_checkout: true,
    });
    expect(result.state.pending).toEqual([
      { requestId: "req-1", sessionId: target.id, action: "resume", discardsOriginal: true },
    ]);
  }
});

test("home_resume_for_an_abandoned_session_resumes_it", () => {
  const result = resume(
    NO_ACTIONS,
    counter(),
    session("s1", { status: "stopped", is_abandoned: true }),
    false,
  );

  expect(result.message).toEqual({ type: "resume_abandoned", session_id: "s1" });
  // No request id on the wire, so the record is keyed by the session alone.
  expect(result.state.pending).toEqual([
    { requestId: null, sessionId: "s1", action: "resume", discardsOriginal: false },
  ]);
});

test("resume sends resume_session when the host supports it", () => {
  const stopped = [
    session("s1", { status: "stopped" }),
    session("s2", { status: "error" }),
    session("s3", { is_inactive: true }),
  ];

  for (const target of stopped) {
    const sent = resume(NO_ACTIONS, counter(), target, true);
    expect(sent.message).toEqual({
      type: "resume_session",
      session_id: target.id,
      request_id: "req-1",
    });

    // The phone still discards the original once the rerun lands.
    const landed = foldReply(sent.state, {
      type: "session_updated",
      request_id: "req-1",
      session: session(`${target.id}-copy`, { status: "spawning" }),
    });
    expect(landed.effects).toEqual([
      { type: "discard_session", session_id: target.id, cleanup: [] },
    ]);
    expect(landed.state.pending).toEqual([]);
  }

  const running = resume(NO_ACTIONS, unused(), session("s4", { status: "working" }), true);
  expect(running.message).toBeNull();
});

test("resume falls back to duplicate_session on an older host", () => {
  const result = resume(NO_ACTIONS, counter(), session("s1", { is_inactive: true }), false);

  expect(result.message).toEqual({
    type: "duplicate_session",
    session_id: "s1",
    request_id: "req-1",
    carry_busy_excluded: true,
    same_checkout: true,
  });
});

test("an abandoned resume sends resume_session with a request id", () => {
  const abandoned = session("s1", { status: "stopped", is_abandoned: true });
  const sent = resume(NO_ACTIONS, counter(), abandoned, true);

  expect(sent.message).toEqual({ type: "resume_session", session_id: "s1", request_id: "req-1" });

  // Its failure now names the request, so the row shows the refusal.
  const failed = foldReply(sent.state, {
    type: "error",
    request_id: "req-1",
    code: "resume_failed",
    message: "the captured config is gone",
  });
  expect(errorFor(failed.state, "s1")).toBe("the captured config is gone");
  expect(failed.state.pending).toEqual([]);
});

test("an abandoned resume_session echo discards nothing", () => {
  const abandoned = session("s1", { status: "stopped", is_abandoned: true });
  const sent = resume(NO_ACTIONS, counter(), abandoned, true);

  const landed = foldReply(sent.state, {
    type: "session_updated",
    request_id: "req-1",
    session: session("s1-resumed", { status: "spawning" }),
  });

  expect(landed.effects).toEqual([]);
  expect(landed.state.pending).toEqual([]);
});

test("home_abandoned_resume_waits_for_its_session", () => {
  const abandoned = session("s1", { status: "stopped", is_abandoned: true });
  const sent = resume(NO_ACTIONS, unused(), abandoned, false);

  const again = resume(sent.state, unused(), abandoned, false);
  expect(again.message).toBeNull();
  expect(again.state).toBe(sent.state);

  const cleared = foldReply(sent.state, {
    type: "session_updated",
    session: session("s1", { status: "spawning" }),
  });
  expect(cleared.state.pending).toEqual([]);
});

test("home_resume_is_not_offered_for_a_headless_session", () => {
  const headless = [
    session("s1", { mode: "headless", status: "stopped" }),
    session("s2", { mode: "headless", status: "stopped", is_abandoned: true }),
    session("s3", { mode: "headless", is_inactive: true }),
  ];

  for (const target of headless) {
    const result = resume(NO_ACTIONS, unused(), target, false);
    expect(result.message).toBeNull();
    expect(result.state).toBe(NO_ACTIONS);
  }
});

test("home_resume_is_not_offered_for_a_running_session", () => {
  const running: SessionSnapshot["status"][] = ["spawning", "idle", "working", "awaiting_input"];

  for (const status of running) {
    const result = resume(NO_ACTIONS, unused(), session(`s-${status}`, { status }), false);
    expect(result.message).toBeNull();
    expect(result.state).toBe(NO_ACTIONS);
  }
});

test("home_answer_refusal_shows_the_daemon_message_and_allows_a_retry", () => {
  const ids = counter();
  const state = refused(ids);

  expect(errorFor(state, "s1")).toBe("the prompt is no longer up");
  expect(state.pending).toEqual([]);

  const retry = answer(state, ids, "s1", SINCE, { kind: "allow" });
  expect(retry.message).toMatchObject({ request_id: "req-2" });
  expect(errorFor(retry.state, "s1")).toBeNull();
});

test("home_a_landed_answer_unblocks_the_card_on_the_next_session_update", () => {
  const ids = counter();
  const sent = answer(NO_ACTIONS, ids, "s1", SINCE, { kind: "allow" });
  expect(answer(sent.state, unused(), "s1", SINCE, { kind: "deny" }).message).toBeNull();

  // An answer has no success reply: the session's next update says it landed.
  const landed = foldReply(sent.state, {
    type: "session_updated",
    session: session("s1", { status: "working" }),
  });
  expect(landed.state.pending).toEqual([]);

  const again = answer(landed.state, ids, "s1", SINCE, { kind: "deny" });
  expect(again.message).toMatchObject({ request_id: "req-2" });
});

test("home_a_failed_resume_shows_the_daemon_message", () => {
  const sent = resume(NO_ACTIONS, counter(), session("s1", { status: "stopped" }), false);

  const failed = foldReply(sent.state, {
    type: "action_failed",
    title: "Couldn't start session",
    detail: "a worktree already uses that branch",
    hint: null,
    request_id: "req-1",
  });

  expect(errorFor(failed.state, "s1")).toBe("a worktree already uses that branch");
  expect(failed.state.pending).toEqual([]);
  expect(failed.effects).toEqual([]);
});

test("home_a_landed_resume_discards_the_original_once", () => {
  const sent = resume(NO_ACTIONS, counter(), session("s1", { status: "stopped" }), false);

  const update = {
    type: "session_updated",
    request_id: "req-1",
    session: session("s1-copy", { status: "spawning" }),
  } as const;
  const landed = foldReply(sent.state, update);

  expect(landed.effects).toEqual([{ type: "discard_session", session_id: "s1", cleanup: [] }]);
  expect(landed.state.pending).toEqual([]);

  const again = foldReply(landed.state, update);
  expect(again.effects).toEqual([]);
});

test("home_a_session_update_clears_the_refusal_text", () => {
  const state = refused();

  const updated = foldReply(state, {
    type: "session_updated",
    session: session("s1", { status: "working" }),
  });

  expect(errorFor(updated.state, "s1")).toBeNull();
  const other = foldReply(state, {
    type: "session_updated",
    session: session("s2"),
  });
  expect(errorFor(other.state, "s1")).toBe("the prompt is no longer up");
});

test("home_repeat_tap_while_pending_sends_nothing", () => {
  const sent = answer(NO_ACTIONS, counter(), "s1", SINCE, { kind: "allow" });

  const again = answer(sent.state, unused(), "s1", SINCE, { kind: "deny" });
  expect(again.message).toBeNull();
  expect(again.state).toBe(sent.state);

  // Another session's card still answers.
  expect(answer(sent.state, counter(), "s2", SINCE, { kind: "deny" }).message).toMatchObject({
    session_id: "s2",
  });
});

test("home_repeat_resume_while_pending_sends_nothing", () => {
  const stopped = session("s1", { status: "stopped" });
  const sent = resume(NO_ACTIONS, counter(), stopped, false);

  const again = resume(sent.state, unused(), stopped, false);
  expect(again.message).toBeNull();
  expect(again.state).toBe(sent.state);
});

test("home_error_for_another_request_changes_nothing", () => {
  const state = answer(NO_ACTIONS, counter(), "s1", SINCE, { kind: "allow" }).state;

  const other = foldReply(state, {
    type: "error",
    request_id: "req-other",
    code: "invalid_request",
    message: "that answer does not fit the prompt on screen",
  });
  expect(other.state).toBe(state);
  expect(other.effects).toEqual([]);

  const unrouted = foldReply(state, {
    type: "error",
    request_id: null,
    code: "not_found",
    message: "unknown session",
  });
  expect(unrouted.state).toBe(state);

  expect(errorFor(state, "s1")).toBeNull();
});
