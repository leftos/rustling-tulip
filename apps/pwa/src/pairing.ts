import type { DaemonMessage } from "./protocol";

/**
 * The reason the daemon gives a no-token connection right after `Welcome`
 * (`crates/daemon/src/server/pairing.rs`): it opens the pairing channel.
 */
export const DEVICE_NOT_APPROVED = "device not approved";

/** Shown when the typed or scanned code is not six digits; nothing is sent. */
export const ENTER_THE_CODE = "Enter the six-digit code.";

/** Shown when the socket carrying a request is lost: the request went with it. */
export const CONNECTION_DROPPED = "The connection dropped. Ask again.";

/** Shown when the token arrived but could not be written to the phone. */
export const SAVE_FAILED =
  "Could not save the pairing on this phone; it lasts until the app closes.";

export type PairingPhase = "idle" | "asking" | "pending" | "approved" | "denied" | "paired";

/**
 * What the pairing screen shows. `requestId` is the open request on the
 * current socket, if any; `error` is the text to show, the daemon's reasons
 * and decisions verbatim.
 */
export interface PairingState {
  readonly phase: PairingPhase;
  readonly requestId: string | null;
  readonly expiresInSecs: number | null;
  readonly error: string | null;
  /**
   * Set only by the event just folded being a `device_pair_failed`: a `lost`
   * right after it is the daemon closing on that reason, which stays shown.
   */
  readonly justFailed?: true;
}

export const INITIAL_PAIRING: PairingState = {
  phase: "idle",
  requestId: null,
  expiresInSecs: null,
  error: null,
};

export type PairingEvent =
  /** The user tapped to ask; the request goes out on the pairing channel. */
  | { readonly type: "ask" }
  /** The user entered or scanned `code`. */
  | { readonly type: "redeem"; readonly code: string }
  /** A daemon message the pairing channel delivered. */
  | { readonly type: "message"; readonly message: DaemonMessage }
  /** The socket that carried the ask or the request is gone. */
  | { readonly type: "lost" }
  /** The token arrived but the store could not keep it. */
  | { readonly type: "saveFailed" }
  /** The device token was forgotten: pairing starts over. */
  | { readonly type: "forgotten" };

const PAIRING_CODE = /^\d{6}$/;

/** Whether `code` is a pairing code worth sending: exactly six ASCII digits. */
export function isPairingCode(code: string): boolean {
  return PAIRING_CODE.test(code);
}

/** Whether a tap may ask: no ask or request is already open. */
export function canAsk(state: PairingState): boolean {
  return state.phase !== "asking" && state.phase !== "pending" && state.phase !== "approved";
}

/**
 * One event applied to the pairing model. Pure; an event that changes
 * nothing returns `state` itself, so a caller can tell by identity.
 */
export function pairingStep(state: PairingState, event: PairingEvent): PairingState {
  const next = fold(state, event);
  if (event.type === "message" && event.message.type === "device_pair_failed") {
    return { ...next, justFailed: true };
  }
  return withoutJustFailed(next);
}

function withoutJustFailed(state: PairingState): PairingState {
  if (!state.justFailed) {
    return state;
  }
  return {
    phase: state.phase,
    requestId: state.requestId,
    expiresInSecs: state.expiresInSecs,
    error: state.error,
  };
}

function fold(state: PairingState, event: PairingEvent): PairingState {
  switch (event.type) {
    case "ask":
      return canAsk(state) ? { ...INITIAL_PAIRING, phase: "asking" } : state;
    case "redeem":
      if (!isPairingCode(event.code)) {
        return { ...state, error: ENTER_THE_CODE };
      }
      if (state.requestId === null) {
        return state;
      }
      return state.error === null ? state : { ...state, error: null };
    case "message":
      return onMessage(state, event.message);
    case "lost":
      if (state.phase !== "asking" && state.requestId === null) {
        return state;
      }
      // A reason the daemon sent just before closing explains the close
      // better than the drop does, so it stays; any other error is stale.
      return {
        ...INITIAL_PAIRING,
        error: state.justFailed ? state.error : CONNECTION_DROPPED,
      };
    case "saveFailed":
      return { ...state, error: SAVE_FAILED };
    case "forgotten":
      return INITIAL_PAIRING;
  }
}

function onMessage(state: PairingState, message: DaemonMessage): PairingState {
  switch (message.type) {
    case "device_pair_pending":
      return {
        phase: "pending",
        requestId: message.request_id,
        expiresInSecs: message.expires_in_secs,
        error: null,
      };
    case "device_pair_decided":
      if (message.request_id !== state.requestId) {
        return state;
      }
      if (message.outcome === "approved") {
        return { ...state, phase: "approved", error: null };
      }
      return { ...INITIAL_PAIRING, phase: "denied", error: message.outcome };
    case "device_token":
      return { ...INITIAL_PAIRING, phase: "paired" };
    case "device_pair_failed":
      // A refused ask leaves nothing open, so the next tap may ask again.
      return state.phase === "asking"
        ? { ...INITIAL_PAIRING, error: message.reason }
        : { ...state, error: message.reason };
    default:
      return state;
  }
}
