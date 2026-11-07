import { expect, test } from "vitest";

import { answer, NO_ACTIONS, stop, type HomeActions } from "./home-actions";
import { reset, type HomeModel } from "./home-model";
import {
  homeScreen,
  homeVisible,
  sameScreen,
  tappedElsewhere,
  updatedSession,
  type HomeState,
} from "./home-screen";
import type { PendingInput, SessionSnapshot, SessionStatus } from "./protocol";

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

function waiting(
  id: string,
  prompt: PendingInput = { kind: "permission", tool_name: "Bash", summary: "npm publish" },
): SessionSnapshot {
  return session(id, { status: "awaiting_input", status_since: SINCE, pending_input: prompt });
}

function model(sessions: readonly SessionSnapshot[]): HomeModel {
  return { ...reset(), sessions: [...sessions] };
}

/** Home's state at rest: ready, paired, shown, on the first card, online. */
function view(overrides: Partial<HomeState> = {}): HomeState {
  return {
    status: "ready",
    paired: true,
    shown: true,
    card: null,
    index: 0,
    offline: false,
    ...overrides,
  };
}

function withError(sessionId: string, text: string): HomeActions {
  return { ...NO_ACTIONS, errors: new Map([[sessionId, text]]) };
}

function armed(sessionId: string): HomeActions {
  return { ...NO_ACTIONS, armed: sessionId };
}

/** The first row of the first group, which a model with no repos files under Detached. */
function firstRow(screen: ReturnType<typeof homeScreen>) {
  return screen.groups[0]?.rows[0];
}

test("home_screen_tray_is_hidden_with_no_needs_you", () => {
  expect(
    homeScreen(model([session("s1", { status: "working" })]), NO_ACTIONS, view()).card,
  ).toBeNull();
  expect(homeScreen(model([]), NO_ACTIONS, view()).card).toBeNull();
  for (const quiet of [
    session("parked", { status: "awaiting_input", is_inactive: true }),
    session("gone", { status: "awaiting_input", is_abandoned: true }),
  ]) {
    expect(homeScreen(model([quiet]), NO_ACTIONS, view()).card, quiet.id).toBeNull();
  }
});

test("home_screen_tray_shows_one_card_with_its_label_detail_buttons_and_position", () => {
  const screen = homeScreen(model([waiting("s1")]), NO_ACTIONS, view({ card: "s1" }));

  expect(screen.card).not.toBeNull();
  expect(screen.card?.sessionId).toBe("s1");
  expect(screen.card?.label).toBe("s1");
  expect(screen.card?.detail).toBe("Allow Bash?");
  expect(screen.card?.buttons).toEqual([
    { label: "Allow", hint: null, choice: { kind: "allow" }, enabled: true },
    { label: "Deny", hint: null, choice: { kind: "deny" }, enabled: true },
  ]);
  expect(screen.card?.position).toBe("1 of 1");
  expect(screen.card?.count).toBe(1);
});

test("home_screen_next_and_previous_stop_at_the_ends", () => {
  const two = model([waiting("s1"), waiting("s2", { kind: "other", message: "Ship it?" })]);

  const first = homeScreen(two, NO_ACTIONS, view({ card: "s1" }));
  expect(first.card?.position).toBe("1 of 2");
  expect(first.card?.previous).toBeNull();
  expect(first.card?.next).toBe("s2");

  const second = homeScreen(two, NO_ACTIONS, view({ card: "s2" }));
  expect(second.card?.position).toBe("2 of 2");
  expect(second.card?.previous).toBe("s1");
  expect(second.card?.next).toBeNull();
});

test("home_screen_tracks_the_shown_card_by_id", () => {
  const onB = view({ card: "B", index: 1 });
  const abc = model([waiting("A"), waiting("B"), waiting("C")]);
  expect(homeScreen(abc, NO_ACTIONS, onB).card?.sessionId).toBe("B");

  // A leaves: the shown card keeps its place by id.
  const withoutA = model([waiting("B"), waiting("C")]);
  expect(homeScreen(withoutA, NO_ACTIONS, onB).card?.sessionId).toBe("B");

  // B leaves: the card that took its place, C, shows.
  const withoutB = model([waiting("A"), waiting("C")]);
  expect(homeScreen(withoutB, NO_ACTIONS, onB).card?.sessionId).toBe("C");

  // C leaves while shown: the end of the list falls back to B.
  const onC = view({ card: "C", index: 2 });
  const withoutC = model([waiting("A"), waiting("B")]);
  expect(homeScreen(withoutC, NO_ACTIONS, onC).card?.sessionId).toBe("B");
});

test("home_screen_shows_a_cards_error_text", () => {
  const refused = homeScreen(
    model([waiting("s1")]),
    withError("s1", "the prompt is no longer up"),
    view({ card: "s1" }),
  );
  expect(refused.card?.error).toBe("the prompt is no longer up");

  expect(
    homeScreen(model([waiting("s1")]), NO_ACTIONS, view({ card: "s1" })).card?.error,
  ).toBeNull();
});

test("home_screen_shows_a_rows_error_text", () => {
  const running = model([session("s1", { status: "working" })]);

  expect(firstRow(homeScreen(running, withError("s1", "the session is gone"), view()))?.error).toBe(
    "the session is gone",
  );
  expect(firstRow(homeScreen(running, NO_ACTIONS, view()))?.error).toBeNull();
});

test("home_screen_stop_reads_stop_then_tap_again_to_stop", () => {
  const running = model([session("s1", { status: "working" })]);

  expect(firstRow(homeScreen(running, NO_ACTIONS, view()))?.stop).toBe("Stop");
  expect(firstRow(homeScreen(running, armed("s1"), view()))?.stop).toBe("Tap again to stop");

  const two = model([session("s1", { status: "working" }), session("s2", { status: "working" })]);
  expect(homeScreen(two, armed("s2"), view()).groups[0]?.rows[0]?.stop).toBe("Stop");
});

test("home_screen_an_armed_stop_drops_on_any_other_tap", () => {
  const running = model([session("s1", { status: "working" })]);
  const armedState = armed("s1");
  expect(firstRow(homeScreen(running, armedState, view()))?.stop).toBe("Tap again to stop");

  const afterTap = tappedElsewhere(armedState);
  expect(firstRow(homeScreen(running, afterTap, view()))?.stop).toBe("Stop");
  expect(tappedElsewhere(NO_ACTIONS)).toBe(NO_ACTIONS);
});

test("home_screen_the_second_stop_tap_sends_the_stop", () => {
  const running = model([session("s1", { status: "working" })]);
  const first = stop(NO_ACTIONS, "s1");
  expect(first.message).toBeNull();

  const second = stop(first.state, "s1");
  expect(second.message).toEqual({ type: "stop_session", session_id: "s1", cleanup: [] });
  expect(firstRow(homeScreen(running, second.state, view()))?.stop).toBe("Stop");
});

test("home_screen_an_armed_stop_drops_on_the_sessions_update", () => {
  const armedState = armed("s1");

  expect(updatedSession(armedState, "s1").armed).toBeNull();
  expect(updatedSession(armedState, "s2")).toBe(armedState);

  const running = model([session("s1", { status: "working" })]);
  expect(firstRow(homeScreen(running, updatedSession(armedState, "s1"), view()))?.stop).toBe(
    "Stop",
  );
});

test("home_screen_card_disables_its_answers_while_one_is_pending", () => {
  const asked = model([waiting("s1")]);
  const pressed = answer(NO_ACTIONS, () => "req-1", "s1", SINCE, { kind: "allow" }).state;

  const screen = homeScreen(asked, pressed, view({ card: "s1" }));
  expect(screen.card?.sending).toBe("Sending…");
  for (const choice of screen.card?.buttons ?? []) {
    expect(choice.enabled, choice.label).toBe(false);
  }

  const idle = homeScreen(asked, NO_ACTIONS, view({ card: "s1" }));
  expect(idle.card?.sending).toBeNull();
  expect(idle.card?.buttons.every((choice) => choice.enabled)).toBe(true);
});

test("home_screen_repaints_only_when_a_screen_changed", () => {
  const shown = model([session("s1", { status: "working" })]);
  const first = homeScreen(shown, NO_ACTIONS, view());
  expect(sameScreen(first, homeScreen(shown, NO_ACTIONS, view()))).toBe(true);

  const renamed = model([session("s1", { status: "working", user_label: "Build" })]);
  expect(sameScreen(first, homeScreen(renamed, NO_ACTIONS, view()))).toBe(false);
  expect(sameScreen(first, homeScreen(shown, NO_ACTIONS, view({ offline: true })))).toBe(false);
});

test("home_screen_resume_appears_only_when_offered", () => {
  const stopped = firstRow(
    homeScreen(model([session("s1", { status: "stopped" })]), NO_ACTIONS, view()),
  );
  expect(stopped?.resume).toBe(true);
  expect(stopped?.stop).toBeNull();

  for (const overrides of [
    { status: "error" as const },
    { is_inactive: true },
    { is_abandoned: true },
  ]) {
    const row = firstRow(homeScreen(model([session("s1", overrides)]), NO_ACTIONS, view()));
    expect(row?.resume, JSON.stringify(overrides)).toBe(true);
    expect(row?.stop, JSON.stringify(overrides)).toBeNull();
  }

  const headless = session("s1", { status: "stopped", mode: "headless" });
  expect(firstRow(homeScreen(model([headless]), NO_ACTIONS, view()))?.resume).toBe(false);

  const running = firstRow(
    homeScreen(model([session("s1", { status: "working" })]), NO_ACTIONS, view()),
  );
  expect(running?.resume).toBe(false);
});

test("home_screen_shows_the_status_words", () => {
  const words: Record<SessionStatus, string> = {
    spawning: "spawning",
    idle: "Idle",
    working: "Working",
    awaiting_input: "Needs you",
    stopped: "Stopped",
    error: "Error",
  };

  for (const [status, word] of Object.entries(words)) {
    const row = firstRow(
      homeScreen(
        model([session("s1", { status: status as SessionStatus, user_label: "Build" })]),
        NO_ACTIONS,
        view(),
      ),
    );
    expect(row?.status, status).toBe(word);
    expect(row?.label, status).toBe("Build");
  }
});

test("home_screen_shows_the_offline_bar", () => {
  expect(homeScreen(model([]), NO_ACTIONS, view({ offline: true })).bar).toBe("Offline");
  expect(homeScreen(model([]), NO_ACTIONS, view()).bar).toBeNull();
});

test("home_screen_stays_up_while_the_connection_reconnects", () => {
  // Never shown, and not ready and paired: the page's own content stands.
  expect(homeVisible("connecting", false, false)).toBe(false);
  expect(homeVisible("ready", false, false)).toBe(false);
  expect(homeVisible("ready", false, true)).toBe(true);

  // Shown once: reconnecting keeps Home, behind its bar, with every action off.
  const reconnecting = homeScreen(
    model([session("s1", { status: "working" })]),
    NO_ACTIONS,
    view({ status: "backoff" }),
  );
  expect(reconnecting.visible).toBe(true);
  expect(reconnecting.bar).toBe("Reconnecting…");
  expect(reconnecting.actionsEnabled).toBe(false);
  expect(reconnecting.groups[0]?.rows[0]?.enabled).toBe(false);
  expect(reconnecting.groups[0]?.rows[0]?.stop).toBe("Stop");

  // The pairing screen replaces Home.
  expect(homeScreen(model([]), NO_ACTIONS, view({ status: "pairing" })).visible).toBe(false);
  expect(homeScreen(model([]), NO_ACTIONS, view({ status: "notApproved" })).visible).toBe(false);

  // Ready and paired: Home, and no bar.
  expect(homeScreen(model([]), NO_ACTIONS, view()).visible).toBe(true);
  expect(homeScreen(model([]), NO_ACTIONS, view()).bar).toBeNull();
});
