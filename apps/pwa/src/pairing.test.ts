import { expect, test } from "vitest";

import {
  CONNECTION_DROPPED,
  ENTER_THE_CODE,
  INITIAL_PAIRING,
  SAVE_FAILED,
  canAsk,
  isPairingCode,
  pairingStep,
  type PairingState,
} from "./pairing";

const PENDING: PairingState = {
  phase: "pending",
  requestId: "req-1",
  expiresInSecs: 300,
  error: null,
};

const APPROVED: PairingState = { ...PENDING, phase: "approved" };

test("pairing_ask_from_idle_is_asking", () => {
  expect(pairingStep(INITIAL_PAIRING, { type: "ask" })).toEqual({
    phase: "asking",
    requestId: null,
    expiresInSecs: null,
    error: null,
  });
  expect(canAsk(INITIAL_PAIRING)).toBe(true);
});

test("pairing_ask_while_a_request_is_open_changes_nothing", () => {
  const asking = pairingStep(INITIAL_PAIRING, { type: "ask" });

  for (const state of [asking, PENDING, APPROVED]) {
    expect(canAsk(state)).toBe(false);
    expect(pairingStep(state, { type: "ask" })).toBe(state);
  }
});

test("pairing_ask_clears_an_earlier_error", () => {
  const denied: PairingState = { ...INITIAL_PAIRING, phase: "denied", error: "denied" };

  expect(pairingStep(denied, { type: "ask" })).toEqual({
    phase: "asking",
    requestId: null,
    expiresInSecs: null,
    error: null,
  });
});

test("pairing_pending_keeps_the_request_id_and_lifetime", () => {
  const asking = pairingStep(INITIAL_PAIRING, { type: "ask" });

  expect(
    pairingStep(asking, {
      type: "message",
      message: { type: "device_pair_pending", request_id: "req-1", expires_in_secs: 300 },
    }),
  ).toEqual(PENDING);
});

test("pairing_approved_decision_is_approved", () => {
  expect(
    pairingStep(PENDING, {
      type: "message",
      message: { type: "device_pair_decided", request_id: "req-1", outcome: "approved" },
    }),
  ).toEqual(APPROVED);
});

test("pairing_decision_for_another_request_is_ignored", () => {
  expect(
    pairingStep(PENDING, {
      type: "message",
      message: { type: "device_pair_decided", request_id: "req-other", outcome: "approved" },
    }),
  ).toBe(PENDING);
});

test("pairing_other_decisions_are_denied_in_their_own_words", () => {
  for (const outcome of ["denied", "expired", "superseded"] as const) {
    expect(
      pairingStep(PENDING, {
        type: "message",
        message: { type: "device_pair_decided", request_id: "req-1", outcome },
      }),
    ).toEqual({ phase: "denied", requestId: null, expiresInSecs: null, error: outcome });
  }
});

test("pairing_device_token_is_paired", () => {
  expect(
    pairingStep(APPROVED, {
      type: "message",
      message: { type: "device_token", device_id: "device-1", token: "new-token" },
    }),
  ).toEqual({ phase: "paired", requestId: null, expiresInSecs: null, error: null });
});

test("pairing_failure_shows_the_reason_verbatim", () => {
  expect(
    pairingStep(APPROVED, {
      type: "message",
      message: { type: "device_pair_failed", reason: "That code is not right." },
    }),
  ).toEqual({ ...APPROVED, error: "That code is not right.", justFailed: true });
});

test("pairing_failure_while_asking_goes_back_to_idle", () => {
  const asking = pairingStep(INITIAL_PAIRING, { type: "ask" });
  const reason = "Too many devices are waiting; try again in a few minutes.";

  expect(
    pairingStep(asking, { type: "message", message: { type: "device_pair_failed", reason } }),
  ).toEqual({ ...INITIAL_PAIRING, error: reason, justFailed: true });
});

test("pairing_lost_while_pending_drops_the_request", () => {
  expect(pairingStep(PENDING, { type: "lost" })).toEqual({
    phase: "idle",
    requestId: null,
    expiresInSecs: null,
    error: CONNECTION_DROPPED,
  });
});

test("pairing_lost_while_asking_drops_the_ask", () => {
  const asking = pairingStep(INITIAL_PAIRING, { type: "ask" });

  expect(pairingStep(asking, { type: "lost" })).toEqual({
    ...INITIAL_PAIRING,
    error: CONNECTION_DROPPED,
  });
});

function failed(state: PairingState, reason: string): PairingState {
  return pairingStep(state, { type: "message", message: { type: "device_pair_failed", reason } });
}

test("pairing_lost_right_after_a_failure_keeps_the_reason", () => {
  const reason = "Too many wrong codes; ask again from the device.";

  expect(pairingStep(failed(APPROVED, reason), { type: "lost" })).toEqual({
    ...INITIAL_PAIRING,
    error: reason,
  });
});

test("pairing_lost_after_a_later_event_says_the_connection_dropped", () => {
  const wrong = failed(APPROVED, "That code is not right.");
  const later: PairingState[] = [
    pairingStep(wrong, { type: "ask" }),
    pairingStep(wrong, { type: "redeem", code: "123456" }),
    pairingStep(wrong, {
      type: "message",
      message: { type: "device_pair_decided", request_id: "req-other", outcome: "approved" },
    }),
  ];

  for (const state of later) {
    expect(pairingStep(state, { type: "lost" })).toEqual({
      ...INITIAL_PAIRING,
      error: CONNECTION_DROPPED,
    });
  }
});

test("pairing_lost_after_a_local_refusal_says_the_connection_dropped", () => {
  const refused = pairingStep(APPROVED, { type: "redeem", code: "12a" });
  expect(refused.error).toBe(ENTER_THE_CODE);

  expect(pairingStep(refused, { type: "lost" })).toEqual({
    ...INITIAL_PAIRING,
    error: CONNECTION_DROPPED,
  });
});

test("pairing_forgotten_starts_over", () => {
  expect(pairingStep({ ...INITIAL_PAIRING, phase: "paired" }, { type: "forgotten" })).toEqual(
    INITIAL_PAIRING,
  );
});

test("pairing_lost_with_nothing_open_changes_nothing", () => {
  const paired: PairingState = { ...INITIAL_PAIRING, phase: "paired" };

  expect(pairingStep(INITIAL_PAIRING, { type: "lost" })).toBe(INITIAL_PAIRING);
  expect(pairingStep(paired, { type: "lost" })).toBe(paired);
});

test("pairing_redeem_refuses_anything_but_six_digits", () => {
  for (const code of ["12a", "12345", "1234567", "", " 123456", "１２３４５６"]) {
    expect(isPairingCode(code)).toBe(false);
    expect(pairingStep(APPROVED, { type: "redeem", code })).toEqual({
      ...APPROVED,
      error: ENTER_THE_CODE,
    });
  }
  expect(isPairingCode("123456")).toBe(true);
});

test("pairing_redeem_of_a_good_code_clears_the_error", () => {
  const failed: PairingState = { ...APPROVED, error: "That code is not right." };

  expect(pairingStep(failed, { type: "redeem", code: "123456" })).toEqual(APPROVED);
});

test("pairing_redeem_without_a_request_changes_nothing", () => {
  expect(pairingStep(INITIAL_PAIRING, { type: "redeem", code: "123456" })).toBe(INITIAL_PAIRING);
});

test("pairing_save_failure_keeps_paired_and_says_so", () => {
  const paired: PairingState = { ...INITIAL_PAIRING, phase: "paired" };

  expect(pairingStep(paired, { type: "saveFailed" })).toEqual({ ...paired, error: SAVE_FAILED });
});
