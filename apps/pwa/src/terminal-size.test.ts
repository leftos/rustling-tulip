import { expect, test } from "vitest";

import type { ClientMessage } from "./protocol";
import { INPUT_WINDOW, noteInput, resizeFor, sessionSize } from "./terminal-size";

const T0 = 1_000;

function resize(message: ClientMessage | null): { cols: number; rows: number; claim: boolean } {
  if (message === null || message.type !== "resize") {
    throw new Error(`expected a resize, saw ${message === null ? "none" : message.type}`);
  }
  return { cols: message.cols, rows: message.rows, claim: message.claim === true };
}

test("size_attach_sends_an_unclaimed_resize", () => {
  const attached = resizeFor(sessionSize("s1"), 80, 24, "attach", T0);
  expect(attached.message).toEqual({
    type: "resize",
    session_id: "s1",
    cols: 80,
    rows: 24,
    claim: false,
  });
  expect(attached.state).toEqual({ sessionId: "s1", cols: 80, rows: 24, typedAt: null });
});

test("size_a_reconnect_is_unclaimed_and_the_first_input_after_it_claims", () => {
  const attached = resizeFor(sessionSize("s1"), 80, 24, "attach", T0).state;
  const typed = noteInput(attached, T0 + 1_000).state;

  const reconnect = resizeFor(typed, 80, 24, "reconnect", T0 + 1_500);
  expect(resize(reconnect.message)).toEqual({ cols: 80, rows: 24, claim: false });
  expect(reconnect.state.typedAt).toBeNull();

  const first = noteInput(reconnect.state, T0 + 1_600);
  expect(resize(first.message)).toEqual({ cols: 80, rows: 24, claim: true });
});

test("size_the_terminal_toggle_is_unclaimed", () => {
  const attached = resizeFor(sessionSize("s1"), 80, 24, "attach", T0).state;
  const typed = noteInput(attached, T0 + 1_000).state;

  const toggle = resizeFor(typed, 40, 20, "toggle", T0 + 1_100);
  expect(resize(toggle.message)).toEqual({ cols: 40, rows: 20, claim: false });
  expect(toggle.state.typedAt).toBe(T0 + 1_000);
});

test("size_a_user_pane_resize_claims", () => {
  const resized = resizeFor(sessionSize("s1"), 40, 20, "pane-resize", T0);
  expect(resize(resized.message)).toEqual({ cols: 40, rows: 20, claim: true });
  expect(resized.state.cols).toBe(40);
  expect(resized.state.rows).toBe(20);
});

test("size_the_first_input_after_five_seconds_quiet_claims_before_it", () => {
  const attached = resizeFor(sessionSize("s1"), 80, 24, "attach", T0).state;

  const first = noteInput(attached, T0 + 1_000);
  expect(resize(first.message)).toEqual({ cols: 80, rows: 24, claim: true });

  const soon = noteInput(first.state, T0 + 1_000 + INPUT_WINDOW - 1);
  expect(soon.message).toBeNull();

  const quiet = noteInput(soon.state, T0 + 1_000 + 2 * INPUT_WINDOW - 1);
  expect(resize(quiet.message)).toEqual({ cols: 80, rows: 24, claim: true });
});

test("size_a_measure_claims_only_while_the_user_typed_recently", () => {
  const attached = resizeFor(sessionSize("s1"), 80, 24, "attach", T0).state;
  const typed = noteInput(attached, T0 + 1_000).state;

  const recent = resizeFor(typed, 81, 24, "measure", T0 + 1_000 + INPUT_WINDOW - 1);
  expect(resize(recent.message)).toEqual({ cols: 81, rows: 24, claim: true });

  const quiet = resizeFor(typed, 81, 24, "measure", T0 + 1_000 + INPUT_WINDOW);
  expect(resize(quiet.message)).toEqual({ cols: 81, rows: 24, claim: false });
});

test("size_a_zero_size_sends_and_stores_nothing", () => {
  const attached = resizeFor(sessionSize("s1"), 80, 24, "attach", T0).state;
  for (const [cols, rows] of [
    [0, 24],
    [80, 0],
  ] as const) {
    const measure = resizeFor(attached, cols, rows, "pane-resize", T0 + 100);
    expect(measure.message).toBeNull();
    expect(measure.state).toEqual(attached);
  }
});

test("size_the_end_of_a_load_sends_an_unclaimed_resize", () => {
  const attached = resizeFor(sessionSize("s1"), 80, 24, "attach", T0).state;
  const done = resizeFor(attached, 80, 24, "load", T0 + 500);
  expect(resize(done.message)).toEqual({ cols: 80, rows: 24, claim: false });
});
