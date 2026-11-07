import { expect, test } from "vitest";

import type { ConnectionStateName } from "./connection-machine";
import type { PairingPhase, PairingState } from "./pairing";
import {
  CAMERA_OFF,
  cameraStep,
  pairingScreen,
  shouldSendScan,
  type CameraTrack,
} from "./pairing-screen";

const PHASES: readonly PairingPhase[] = [
  "idle",
  "asking",
  "pending",
  "approved",
  "denied",
  "paired",
];

const STATUSES: readonly ConnectionStateName[] = [
  "idle",
  "connecting",
  "handshaking",
  "ready",
  "backoff",
  "hidden",
  "notApproved",
  "pairing",
];

function pairingFor(phase: PairingPhase, error: string | null = null): PairingState {
  return { phase, requestId: null, expiresInSecs: null, error };
}

/** The screen for one phase on the pairing channel, with the camera off. */
function screenFor(phase: PairingPhase) {
  return pairingScreen(pairingFor(phase), "pairing", "off");
}

test("pairing_screen_asks_on_a_tap_from_idle_and_from_denied", () => {
  for (const phase of ["idle", "denied"] as const) {
    const screen = screenFor(phase);

    expect(screen.visible).toBe(true);
    expect(screen.title).toBe("Pair this phone");
    expect(screen.body).toBe("Ask your computer to let this phone in.");
    expect(screen.button).toEqual({ label: "Ask to pair", enabled: true });
    expect(screen.scanner).toBe(false);
    expect(screen.entry).toBeNull();
    expect(screen.cameraNote).toBeNull();
    expect(screen.error).toBeNull();
    expect(screen.camera).toBe(false);
  }
});

test("pairing_screen_asking_shows_a_disabled_ask_button", () => {
  const screen = screenFor("asking");

  expect(screen.body).toBe("Asking…");
  expect(screen.button).toEqual({ label: "Ask to pair", enabled: false });
  expect(screen.scanner).toBe(false);
  expect(screen.entry).toBeNull();
  expect(screen.camera).toBe(false);
});

test("pairing_screen_pending_shows_the_scanner_and_the_typed_entry", () => {
  const screen = screenFor("pending");

  expect(screen.body).toBe("Approve this phone on your computer, then scan the code it shows.");
  expect(screen.button).toBeNull();
  expect(screen.scanner).toBe(true);
  expect(screen.entry).toEqual({
    label: "Six-digit code",
    inputMode: "numeric",
    autoComplete: "one-time-code",
    maxLength: 6,
    button: { label: "Pair", enabled: true },
  });
  expect(screen.camera).toBe(true);
});

test("pairing_screen_approved_shows_the_scanner_and_the_typed_entry", () => {
  const screen = screenFor("approved");

  expect(screen.body).toBe("Scan the code your computer shows, or type it.");
  expect(screen.button).toBeNull();
  expect(screen.scanner).toBe(true);
  expect(screen.entry?.button).toEqual({ label: "Pair", enabled: true });
  expect(screen.camera).toBe(true);
});

test("pairing_screen_paired_says_connecting_and_offers_nothing", () => {
  const screen = screenFor("paired");

  expect(screen.visible).toBe(true);
  expect(screen.body).toBe("Paired. Connecting…");
  expect(screen.button).toBeNull();
  expect(screen.scanner).toBe(false);
  expect(screen.entry).toBeNull();
  expect(screen.cameraNote).toBeNull();
  expect(screen.camera).toBe(false);
});

test("pairing_screen_says_why_the_camera_is_off_while_scanning", () => {
  expect(pairingScreen(pairingFor("pending"), "pairing", "denied").cameraNote).toBe(
    "The camera is off for this app. Type the code instead.",
  );
  expect(pairingScreen(pairingFor("approved"), "pairing", "unavailable").cameraNote).toBe(
    "No camera found. Type the code instead.",
  );
  for (const camera of ["off", "starting", "on"] as const) {
    const screen = pairingScreen(pairingFor("approved"), "pairing", camera);
    expect(screen.cameraNote).toBeNull();
    expect(screen.scanner).toBe(true);
  }
});

test("pairing_screen_shows_the_models_error_verbatim", () => {
  const denied = pairingScreen(pairingFor("denied", "denied by the owner"), "pairing", "off");
  expect(denied.error).toBe("denied by the owner");

  const failed = pairingScreen(pairingFor("asking", "exceeded attempts"), "pairing", "off");
  expect(failed.error).toBe("exceeded attempts");

  expect(pairingScreen(pairingFor("idle"), "pairing", "off").error).toBeNull();
});

test("pairing_screen_shows_only_on_the_pairing_channel_or_while_paired_connects", () => {
  for (const status of STATUSES) {
    expect(pairingScreen(pairingFor("idle"), status, "off").visible, status).toBe(
      status === "pairing",
    );
  }

  const connecting = pairingScreen(pairingFor("paired"), "connecting", "off");
  expect(connecting.visible).toBe(true);
  expect(connecting.body).toBe("Paired. Connecting…");
  expect(pairingScreen(pairingFor("paired"), "ready", "off").visible).toBe(false);
});

test("pairing_screen_is_hidden_once_the_phone_is_ready", () => {
  for (const phase of PHASES) {
    const screen = pairingScreen(pairingFor(phase), "ready", "on");

    expect(screen.visible).toBe(false);
    expect(screen.camera).toBe(false);
    expect(screen.scanner).toBe(false);
    expect(screen.entry).toBeNull();
  }
});

test("pairing_screen_wants_the_camera_only_while_scanning_on_the_pairing_channel", () => {
  for (const phase of PHASES) {
    for (const status of STATUSES) {
      const wanted = (phase === "pending" || phase === "approved") && status === "pairing";
      expect(
        pairingScreen(pairingFor(phase), status, "on").camera,
        `${phase} while ${status}`,
      ).toBe(wanted);
    }
  }
});

test("pairing_camera_a_start_numbers_its_attempt", () => {
  expect(cameraStep(CAMERA_OFF, { type: "start" })).toEqual({ state: "starting", attempt: 1 });
});

test("pairing_camera_a_stale_start_answer_never_overwrites_a_newer_one", () => {
  const started = cameraStep(CAMERA_OFF, { type: "start" });
  const stopped = cameraStep(started, { type: "stop" });
  const restarted = cameraStep(stopped, { type: "start" });

  expect(cameraStep(restarted, { type: "opened", attempt: 1 })).toBe(restarted);
  expect(cameraStep(restarted, { type: "failed", attempt: 1, state: "denied" })).toBe(restarted);
  expect(cameraStep(restarted, { type: "opened", attempt: 3 })).toEqual({
    state: "on",
    attempt: 3,
  });
  expect(cameraStep(restarted, { type: "failed", attempt: 3, state: "unavailable" })).toEqual({
    state: "unavailable",
    attempt: 3,
  });
});

test("pairing_camera_stopping_keeps_a_denied_or_missing_camera_as_it_is", () => {
  for (const state of ["denied", "unavailable"] as const) {
    const track: CameraTrack = { state, attempt: 4 };
    expect(cameraStep(track, { type: "stop" })).toBe(track);
  }

  expect(cameraStep({ state: "on", attempt: 4 }, { type: "stop" })).toEqual({
    state: "off",
    attempt: 5,
  });
  expect(cameraStep({ state: "starting", attempt: 4 }, { type: "stop" })).toEqual({
    state: "off",
    attempt: 5,
  });
  expect(cameraStep(CAMERA_OFF, { type: "stop" })).toBe(CAMERA_OFF);
});

test("pairing_screen_resends_a_scanned_code_only_once_the_daemon_refused_another", () => {
  expect(shouldSendScan("123456", "123456")).toBe(false);
  expect(shouldSendScan("654321", "123456")).toBe(true);
  expect(shouldSendScan("123456", null)).toBe(true);
});
