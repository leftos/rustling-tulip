import { expect, test } from "vitest";

import {
  NO_ACTIONS,
  answer,
  containerKey,
  disarm,
  errorFor,
  expireLaunches,
  expirePresetLaunches,
  foldReply,
  keepLaunches,
  launchLast,
  launchPreset,
  newSession,
  presetKey,
  reset,
  resendLaunches,
  resendPresetLaunches,
  resume,
  stop,
  type FoldResult,
  type HomeActions,
  type PresetLaunchRequest,
} from "./home-actions";
import type { ClientMessage, ContainerRef, SessionSnapshot } from "./protocol";

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

/** The repo and workspace a launch names on the wire. */
const REPO: ContainerRef = { kind: "repo", id: "a" };
const WORKSPACE: ContainerRef = { kind: "workspace", id: "w" };

/** The clock a launch tap reads; every deadline below counts from it. */
const NOW = 1_700_000_000_000;
const MINUTE = 60 * 1000;

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

test("home_launch_last_sends_the_container_with_a_fresh_id", () => {
  const sent = launchLast(NO_ACTIONS, REPO, counter(), NOW);

  expect(sent.message).toEqual({ type: "launch_last", target: REPO, request_id: "req-1" });
  // Recorded before the send, so a tap that reaches no daemon still waits.
  expect(sent.state.launches).toEqual([
    { kind: "launch_last", requestId: "req-1", target: REPO, sentAt: NOW, lastSentAt: NOW },
  ]);
});

test("home_a_second_launch_tap_on_the_same_container_sends_nothing", () => {
  const sent = launchLast(NO_ACTIONS, REPO, counter(), NOW);

  const again = launchLast(sent.state, REPO, unused(), NOW + 1);
  expect(again.message).toBeNull();
  expect(again.state).toBe(sent.state);
});

test("home_a_launch_tap_on_another_container_sends", () => {
  const ids = counter();
  const sent = launchLast(NO_ACTIONS, REPO, ids, NOW);

  const other = launchLast(sent.state, WORKSPACE, ids, NOW);
  expect(other.message).toMatchObject({ target: WORKSPACE, request_id: "req-2" });
  expect(other.state.launches.map((launch) => launch.target)).toEqual([REPO, WORKSPACE]);
});

test("home_a_launch_echo_clears_the_pending_launch", () => {
  const sent = launchLast(NO_ACTIONS, REPO, counter(), NOW);

  const landed = foldReply(sent.state, {
    type: "session_updated",
    request_id: "req-1",
    session: session("a-copy", { status: "spawning" }),
  });

  expect(landed.state.launches).toEqual([]);
  expect(landed.effects).toEqual([]);
});

test("home_a_launch_refusal_shows_the_daemon_message_and_frees_the_button", () => {
  const sent = launchLast(NO_ACTIONS, REPO, counter(), NOW);

  const refusedLaunch = foldReply(sent.state, {
    type: "error",
    request_id: "req-1",
    code: "spawn_refused",
    message: "alpha has no last launch yet.",
  });

  expect(refusedLaunch.state.launches).toEqual([]);
  expect(refusedLaunch.state.launchErrors.get(containerKey(REPO))).toBe(
    "alpha has no last launch yet.",
  );
});

test("home_a_failed_launch_shows_the_detail", () => {
  const sent = launchLast(NO_ACTIONS, REPO, counter(), NOW);

  const failed = foldReply(sent.state, {
    type: "action_failed",
    title: "Couldn't launch",
    detail: "a worktree already uses that branch",
    hint: null,
    request_id: "req-1",
  });

  expect(failed.state.launches).toEqual([]);
  expect(failed.state.launchErrors.get(containerKey(REPO))).toBe(
    "a worktree already uses that branch",
  );
});

test("home_an_answer_for_another_request_leaves_the_launch_alone", () => {
  const sent = launchLast(NO_ACTIONS, REPO, counter(), NOW);

  const other = foldReply(sent.state, {
    type: "error",
    request_id: "req-other",
    code: "not_found",
    message: "No such repo or workspace.",
  });
  expect(other.state).toBe(sent.state);

  const update = foldReply(sent.state, {
    type: "session_updated",
    request_id: "req-other",
    session: session("s9", { status: "spawning" }),
  });
  expect(update.state.launches).toEqual(sent.state.launches);
});

test("home_keep_launches_keeps_the_pending_launch_and_drops_the_errors", () => {
  const sent = launchLast(NO_ACTIONS, REPO, counter(), NOW);
  const before: HomeActions = {
    ...sent.state,
    armed: "s1",
    errors: new Map([["s1", "the prompt is no longer up"]]),
    launchErrors: new Map([[containerKey(REPO), "alpha has no last launch yet."]]),
  };

  const kept = keepLaunches(before);
  expect(kept.launches).toEqual(sent.state.launches);
  expect(kept.launchErrors.size).toBe(0);
  expect(kept.errors.size).toBe(0);
  expect(kept.armed).toBeNull();
  expect(kept.pending).toEqual([]);
});

test("home_resend_keeps_the_same_id_on_a_host_that_launches", () => {
  const ids = counter();
  const one = launchLast(NO_ACTIONS, REPO, ids, NOW);
  const two = launchLast(one.state, WORKSPACE, ids, NOW);

  const resent = resendLaunches(two.state, () => true, NOW + 5_000);
  expect(resent.messages).toEqual([
    { type: "launch_last", target: REPO, request_id: "req-1" },
    { type: "launch_last", target: WORKSPACE, request_id: "req-2" },
  ]);
  // The answer wait restarts from the latest send; the journal clock does not.
  expect(resent.state.launches.map((launch) => launch.sentAt)).toEqual([NOW, NOW]);
  expect(resent.state.launches.map((launch) => launch.lastSentAt)).toEqual([
    NOW + 5_000,
    NOW + 5_000,
  ]);
});

test("home_resend_on_an_older_host_drops_the_launch_and_says_so", () => {
  const sent = launchLast(NO_ACTIONS, REPO, counter(), NOW);

  const resent = resendLaunches(sent.state, () => false, NOW);
  expect(resent.messages).toEqual([]);
  expect(resent.state.launches).toEqual([]);
  expect(resent.state.launchErrors.get(containerKey(REPO))).toBe(
    "This host can't launch from the phone.",
  );

  expect(resendLaunches(NO_ACTIONS, () => false, NOW).state).toBe(NO_ACTIONS);
});

test("home_a_resend_under_the_journal_window_keeps_the_id", () => {
  const sent = launchLast(NO_ACTIONS, REPO, counter(), NOW);

  const resent = resendLaunches(sent.state, () => true, NOW + 9 * MINUTE - 1000);
  expect(resent.messages).toEqual([{ type: "launch_last", target: REPO, request_id: "req-1" }]);
  expect(resent.state.launches).toHaveLength(1);
  expect(resent.state.launchErrors.size).toBe(0);
});

test("home_a_resend_past_the_journal_window_drops_the_launch", () => {
  const sent = launchLast(NO_ACTIONS, REPO, counter(), NOW);

  const resent = resendLaunches(sent.state, () => true, NOW + 9 * MINUTE);
  expect(resent.messages).toEqual([]);
  expect(resent.state.launches).toEqual([]);
  expect(resent.state.launchErrors.get(containerKey(REPO))).toBe(
    "This launch may have started a session. Check the list before launching again.",
  );
});

test("home_a_resend_judges_each_launch_by_its_own_age", () => {
  const ids = counter();
  const old = launchLast(NO_ACTIONS, REPO, ids, NOW - 10 * MINUTE);
  const young = launchLast(old.state, WORKSPACE, ids, NOW);

  const resent = resendLaunches(young.state, () => true, NOW);
  expect(resent.messages).toEqual([
    { type: "launch_last", target: WORKSPACE, request_id: "req-2" },
  ]);
  expect(resent.state.launches.map((launch) => launch.target)).toEqual([WORKSPACE]);
  expect(resent.state.launchErrors.get(containerKey(REPO))).toBe(
    "This launch may have started a session. Check the list before launching again.",
  );
});

test("home_a_launch_unanswered_for_a_minute_expires_without_sending", () => {
  const sent = launchLast(NO_ACTIONS, REPO, counter(), NOW);

  expect(expireLaunches(sent.state, NOW + MINUTE - 1000)).toBe(sent.state);

  const expired = expireLaunches(sent.state, NOW + MINUTE);
  expect(expired.launches).toEqual([]);
  expect(expired.launchErrors.get(containerKey(REPO))).toBe(
    "This launch may have started a session. Check the list before launching again.",
  );
  expect(expireLaunches(NO_ACTIONS, NOW)).toBe(NO_ACTIONS);
});

test("home_a_resend_restarts_the_answer_wait", () => {
  const sent = launchLast(NO_ACTIONS, REPO, counter(), NOW);
  const resent = resendLaunches(sent.state, () => true, NOW + 5 * MINUTE);

  const kept = expireLaunches(resent.state, NOW + 5 * MINUTE + 30_000);
  expect(kept.launches).toHaveLength(1);
  expect(kept.launchErrors.size).toBe(0);

  const expired = expireLaunches(resent.state, NOW + 5 * MINUTE + MINUTE);
  expect(expired.launches).toEqual([]);
  expect(expired.launchErrors.get(containerKey(REPO))).toBe(
    "This launch may have started a session. Check the list before launching again.",
  );
});

/** A preset launch of `p1` from `REPO`, filling its folder source. */
const PRESET_LAUNCH: PresetLaunchRequest = {
  target: REPO,
  presetId: "p1",
  source: { kind: "folder", path: "C:\\dev\\alpha\\docs\\plans" },
};

/** The `launch_preset` the phone sends for `PRESET_LAUNCH` under `jobId`. */
function presetMessage(jobId: string): ClientMessage {
  return {
    type: "launch_preset",
    job_id: jobId,
    target: { kind: "repo", repo_id: "a" },
    preset_id: "p1",
    source: { kind: "folder", path: "C:\\dev\\alpha\\docs\\plans" },
    variable_values: [],
    use_worktree_override: null,
    max_panes_per_tab_override: null,
  };
}

const PRESET_KEY = presetKey(REPO, "p1");

const RESEND_REFUSED = "This host can't resend a preset launch; check the desktop.";
const MAYBE_STARTED =
  "This launch may have started a session. Check the list before launching again.";

test("home_a_preset_launch_is_kept_pending_from_its_send", () => {
  const sent = launchPreset(NO_ACTIONS, PRESET_LAUNCH, counter(), NOW);

  expect(sent.message).toEqual(presetMessage("phone-req-1"));
  expect(sent.state.presetLaunches).toEqual([
    { ...PRESET_LAUNCH, jobId: "phone-req-1", sentAt: NOW, lastSentAt: NOW },
  ]);
  // The Launch last list is the other kind's, and stays empty.
  expect(sent.state.launches).toEqual([]);
});

test("home_a_second_launch_of_a_pending_preset_sends_nothing", () => {
  const sent = launchPreset(NO_ACTIONS, PRESET_LAUNCH, counter(), NOW);

  const again = launchPreset(sent.state, PRESET_LAUNCH, unused(), NOW + 1);
  expect(again.message).toBeNull();
  expect(again.state).toBe(sent.state);
});

test("home_a_pending_preset_is_resent_with_its_job_id_on_a_host_that_repeats", () => {
  const sent = launchPreset(NO_ACTIONS, PRESET_LAUNCH, counter(), NOW);

  const resent = resendPresetLaunches(keepLaunches(sent.state), true, NOW + 5_000);
  expect(resent.messages).toEqual([presetMessage("phone-req-1")]);
  expect(resent.state.presetLaunches.map((launch) => [launch.sentAt, launch.lastSentAt])).toEqual([
    [NOW, NOW + 5_000],
  ]);
});

test("home_a_pending_preset_is_dropped_with_the_resend_text_on_a_host_that_does_not_repeat", () => {
  const sent = launchPreset(NO_ACTIONS, PRESET_LAUNCH, counter(), NOW);

  const resent = resendPresetLaunches(sent.state, false, NOW + 5_000);
  expect(resent.messages).toEqual([]);
  expect(resent.state.presetLaunches).toEqual([]);
  expect(resent.state.presetErrors.get(PRESET_KEY)).toBe(RESEND_REFUSED);
  expect(resendPresetLaunches(NO_ACTIONS, false, NOW).state).toBe(NO_ACTIONS);
});

test("home_a_pending_preset_past_nine_minutes_is_dropped_not_resent", () => {
  const sent = launchPreset(NO_ACTIONS, PRESET_LAUNCH, counter(), NOW);

  const resent = resendPresetLaunches(sent.state, true, NOW + 9 * MINUTE);
  expect(resent.messages).toEqual([]);
  expect(resent.state.presetLaunches).toEqual([]);
  expect(resent.state.presetErrors.get(PRESET_KEY)).toBe(MAYBE_STARTED);
});

test("home_a_preset_launch_without_a_job_update_for_a_minute_expires", () => {
  const sent = launchPreset(NO_ACTIONS, PRESET_LAUNCH, counter(), NOW);

  expect(expirePresetLaunches(sent.state, NOW + MINUTE - 1000)).toBe(sent.state);

  const expired = expirePresetLaunches(sent.state, NOW + MINUTE);
  expect(expired.presetLaunches).toEqual([]);
  expect(expired.presetErrors.get(PRESET_KEY)).toBe(MAYBE_STARTED);
  expect(expirePresetLaunches(NO_ACTIONS, NOW)).toBe(NO_ACTIONS);
});

test("home_the_first_job_update_clears_the_pending_preset", () => {
  const sent = launchPreset(NO_ACTIONS, PRESET_LAUNCH, counter(), NOW);
  const update = (jobId: string): FoldResult =>
    foldReply(sent.state, {
      type: "preset_launch_job_updated",
      job: {
        job_id: jobId,
        preset_id: "p1",
        target: { kind: "repo", repo_id: "a" },
        status: "resolving",
      },
    });

  expect(update("phone-req-1").state.presetLaunches).toEqual([]);
  expect(update("someone-else").state).toBe(sent.state);
});

/** The `new_session` the phone sends for `target` under `requestId`. */
function nextupMessage(target: ContainerRef, requestId: string): ClientMessage {
  return { type: "new_session", target, initial_prompt: "/nextup", request_id: requestId };
}

test("home_new_session_sends_nextup_to_the_container_with_a_fresh_id", () => {
  const sent = newSession(NO_ACTIONS, REPO, counter(), NOW);

  expect(sent.message).toEqual(nextupMessage(REPO, "req-1"));
  expect(sent.state.launches).toEqual([
    { kind: "new_session", requestId: "req-1", target: REPO, sentAt: NOW, lastSentAt: NOW },
  ]);
});

test("home_a_second_new_session_tap_on_the_same_container_sends_nothing", () => {
  const sent = newSession(NO_ACTIONS, REPO, counter(), NOW);

  const again = newSession(sent.state, REPO, unused(), NOW + 1);
  expect(again.message).toBeNull();
  expect(again.state).toBe(sent.state);
});

test("home_new_session_and_launch_last_never_wait_at_once_on_one_container", () => {
  const launched = launchLast(NO_ACTIONS, REPO, counter(), NOW);
  const blocked = newSession(launched.state, REPO, unused(), NOW);
  expect(blocked.message).toBeNull();
  expect(blocked.state).toBe(launched.state);

  const started = newSession(NO_ACTIONS, REPO, counter(), NOW);
  const blockedLaunch = launchLast(started.state, REPO, unused(), NOW);
  expect(blockedLaunch.message).toBeNull();
  expect(blockedLaunch.state).toBe(started.state);

  // Another container is free.
  expect(newSession(launched.state, WORKSPACE, counter(), NOW).message).toEqual(
    nextupMessage(WORKSPACE, "req-1"),
  );
});

test("home_a_new_session_echo_clears_it", () => {
  const sent = newSession(NO_ACTIONS, REPO, counter(), NOW);

  const landed = foldReply(sent.state, {
    type: "session_updated",
    request_id: "req-1",
    session: session("fresh", { status: "spawning" }),
  });

  expect(landed.state.launches).toEqual([]);
  expect(landed.effects).toEqual([]);
});

test("home_a_new_session_refusal_shows_under_the_picker_row_and_frees_it", () => {
  const ids = counter();
  const sent = newSession(NO_ACTIONS, REPO, ids, NOW);

  const refused = foldReply(sent.state, {
    type: "error",
    request_id: "req-1",
    code: "not_found",
    message: "No such repo or workspace.",
  });

  expect(refused.state.launches).toEqual([]);
  expect(refused.state.newSessionErrors.get(containerKey(REPO))).toBe("No such repo or workspace.");
  // Never under the group header.
  expect(refused.state.launchErrors.size).toBe(0);

  const again = newSession(refused.state, REPO, ids, NOW + 1);
  expect(again.message).toEqual(nextupMessage(REPO, "req-2"));
  expect(again.state.newSessionErrors.size).toBe(0);
});

test("home_resend_sends_each_launch_as_its_own_kind_with_the_same_id", () => {
  const ids = counter();
  const one = launchLast(NO_ACTIONS, REPO, ids, NOW);
  const two = newSession(one.state, WORKSPACE, ids, NOW);

  const resent = resendLaunches(two.state, () => true, NOW + 5_000);
  expect(resent.messages).toEqual([
    { type: "launch_last", target: REPO, request_id: "req-1" },
    nextupMessage(WORKSPACE, "req-2"),
  ]);
  expect(resent.state.launches.map((launch) => launch.lastSentAt)).toEqual([
    NOW + 5_000,
    NOW + 5_000,
  ]);
});

test("home_resend_on_a_host_without_new_session_drops_it_and_says_so", () => {
  const ids = counter();
  const one = launchLast(NO_ACTIONS, REPO, ids, NOW);
  const two = newSession(one.state, WORKSPACE, ids, NOW);

  const resent = resendLaunches(two.state, (kind) => kind === "launch_last", NOW);
  expect(resent.messages).toEqual([{ type: "launch_last", target: REPO, request_id: "req-1" }]);
  expect(resent.state.launches.map((launch) => launch.target)).toEqual([REPO]);
  expect(resent.state.newSessionErrors.get(containerKey(WORKSPACE))).toBe(
    "This host can't launch from the phone.",
  );
  expect(resent.state.launchErrors.size).toBe(0);
});

test("home_a_new_session_past_the_journal_window_is_dropped", () => {
  const sent = newSession(NO_ACTIONS, REPO, counter(), NOW);

  const kept = resendLaunches(sent.state, () => true, NOW + 9 * MINUTE - 1000);
  expect(kept.messages).toEqual([nextupMessage(REPO, "req-1")]);

  const resent = resendLaunches(sent.state, () => true, NOW + 9 * MINUTE);
  expect(resent.messages).toEqual([]);
  expect(resent.state.launches).toEqual([]);
  expect(resent.state.newSessionErrors.get(containerKey(REPO))).toBe(MAYBE_STARTED);
});

test("home_a_new_session_unanswered_for_a_minute_expires", () => {
  const sent = newSession(NO_ACTIONS, REPO, counter(), NOW);

  expect(expireLaunches(sent.state, NOW + MINUTE - 1000)).toBe(sent.state);

  const expired = expireLaunches(sent.state, NOW + MINUTE);
  expect(expired.launches).toEqual([]);
  expect(expired.newSessionErrors.get(containerKey(REPO))).toBe(MAYBE_STARTED);
  expect(expired.launchErrors.size).toBe(0);
});
