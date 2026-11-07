import { backoffDelayMs } from "./backoff";
import { buildHello } from "./hello";
import { DEVICE_NOT_APPROVED } from "./pairing";
import type { ClientMessage, DaemonMessage } from "./protocol";

/** The native client's `WELCOME_TIMEOUT` (`apps/native/src/net.rs`). */
export const WELCOME_TIMEOUT_MS = 10_000;

export type ConnectionStateName =
  | "idle"
  | "connecting"
  | "handshaking"
  | "ready"
  | "backoff"
  | "hidden"
  | "notApproved"
  | "pairing";

/**
 * `unpaired` marks a connection that presented no token, whose
 * `device not approved` opens the pairing channel; `ask` marks a dial made
 * by a tap, which sends `device_pair_request` once that channel opens.
 */
interface DialFlags {
  readonly unpaired?: true;
  readonly ask?: true;
}

export type ConnectionState =
  | { readonly name: "idle" }
  | ({ readonly name: "connecting"; readonly attempt: number } & Pick<DialFlags, "ask">)
  | ({ readonly name: "handshaking"; readonly attempt: number } & DialFlags)
  | ({
      readonly name: "ready";
      readonly attempt: number;
      readonly features: ReadonlySet<string>;
      readonly daemonId: string | null;
    } & DialFlags)
  | { readonly name: "backoff"; readonly attempt: number }
  | { readonly name: "hidden" }
  | { readonly name: "notApproved" }
  /**
   * The no-token pairing channel. `connected` is whether its socket is still
   * open; with none, only a tap (`askPair`) or a new token dials.
   */
  | { readonly name: "pairing"; readonly connected: boolean };

export type ConnectionEvent =
  | { readonly type: "start" }
  | { readonly type: "opened"; readonly token: string | undefined }
  | { readonly type: "message"; readonly message: unknown }
  | { readonly type: "closed" }
  | { readonly type: "timer" }
  | { readonly type: "visible" }
  | { readonly type: "hiddenEvent" }
  | { readonly type: "tokenChanged" }
  /** The user tapped to ask for pairing. */
  | { readonly type: "askPair" }
  /** The user redeems `code` for the open request `requestId`. */
  | { readonly type: "redeem"; readonly requestId: string; readonly code: string };

export type ConnectionEffect =
  | { readonly type: "open" }
  | { readonly type: "send"; readonly message: ClientMessage }
  | { readonly type: "close" }
  | { readonly type: "setTimer"; readonly delayMs: number }
  | { readonly type: "clearTimer" }
  | { readonly type: "deliver"; readonly message: DaemonMessage }
  | { readonly type: "status"; readonly state: ConnectionStateName }
  /** The socket that carried a tap's ask or an open request is gone. */
  | { readonly type: "pairingLost" }
  /**
   * The daemon refused the presented token with `device not approved`: stop
   * presenting it for the rest of the page visit and redial (as
   * `tokenChanged`) without it. The stored record is kept, since the daemon
   * says the same when it cannot read its own device store.
   */
  | { readonly type: "dropToken" };

export interface StepResult {
  readonly state: ConnectionState;
  readonly effects: ConnectionEffect[];
}

type WelcomeMessage = Extract<DaemonMessage, { type: "welcome" }>;

/** The daemon messages the pairing channel hands to the pairing model. */
const PAIRING_MESSAGES: ReadonlySet<string> = new Set([
  "device_pair_pending",
  "device_pair_decided",
  "device_token",
  "device_pair_failed",
]);

/**
 * One event applied to the connection's state: the next state and the
 * effects the driver must run, in order. Pure: no socket, no timer, no DOM.
 *
 * After a reconnect the daemon sends its usual initial state (`Welcome`, then
 * the lists), so a listener treats every `ready` as a fresh start: the
 * `status` effect is what announces it.
 */
export function step(state: ConnectionState, event: ConnectionEvent): StepResult {
  switch (event.type) {
    case "start":
      return dial(state);
    case "tokenChanged":
      if (state.name === "hidden") {
        // Nothing is connected while hidden, and the next `visible` reads the
        // token then: a new token alone never dials in the background.
        return unchanged(state);
      }
      // A redial leaves the socket an open request or a tap's ask lived on.
      return dial(state, false, lostOnRedial(state));
    case "opened":
      return onOpened(state, event.token);
    case "message":
      return onMessage(state, event.message);
    case "closed":
      return onLost(state);
    case "timer":
      return onTimer(state);
    case "visible":
      return onVisible(state);
    case "hiddenEvent":
      return onHidden(state);
    case "askPair":
      return onAskPair(state);
    case "redeem":
      return onRedeem(state, event.requestId, event.code);
  }
}

function onOpened(state: ConnectionState, token: string | undefined): StepResult {
  if (state.name !== "connecting") {
    return unchanged(state);
  }
  const unpaired = token === undefined || token === "";
  return transition(
    state,
    {
      name: "handshaking",
      attempt: state.attempt,
      ...(unpaired ? { unpaired: true } : {}),
      ...(state.ask ? { ask: true } : {}),
    },
    [
      { type: "send", message: buildHello(token) },
      { type: "setTimer", delayMs: WELCOME_TIMEOUT_MS },
    ],
  );
}

function onLost(state: ConnectionState): StepResult {
  if (state.name === "connecting" || state.name === "handshaking" || state.name === "ready") {
    return backOff(state, state.attempt, lostAsk(state));
  }
  if (state.name === "pairing" && state.connected) {
    // The daemon's idle close, its close after a refusal, or a drop: nothing
    // redials by itself, and an open request went with the socket.
    return { state: { name: "pairing", connected: false }, effects: [{ type: "pairingLost" }] };
  }
  return unchanged(state);
}

function onTimer(state: ConnectionState): StepResult {
  if (state.name === "backoff") {
    return transition(state, { name: "connecting", attempt: state.attempt }, [{ type: "open" }]);
  }
  // The Welcome timeout: the socket never answered, so it is closed rather
  // than left open while the machine backs off.
  if (state.name === "connecting" || state.name === "handshaking" || state.name === "ready") {
    return backOff(state, state.attempt, [{ type: "close" }, ...lostAsk(state)]);
  }
  return unchanged(state);
}

function onVisible(state: ConnectionState): StepResult {
  if (state.name === "hidden" || state.name === "backoff") {
    return dial(state);
  }
  return unchanged(state);
}

function onHidden(state: ConnectionState): StepResult {
  if (state.name === "notApproved") {
    return unchanged(state);
  }
  if (state.name === "pairing") {
    // The pairing channel stays the screen: its socket closes, and only a tap
    // dials again.
    return state.connected
      ? {
          state: { name: "pairing", connected: false },
          effects: [{ type: "close" }, { type: "pairingLost" }],
        }
      : unchanged(state);
  }
  return transition(state, { name: "hidden" }, [
    { type: "clearTimer" },
    { type: "close" },
    ...lostAsk(state),
  ]);
}

function onAskPair(state: ConnectionState): StepResult {
  if (state.name !== "pairing") {
    return unchanged(state);
  }
  if (state.connected) {
    return { state, effects: [{ type: "send", message: { type: "device_pair_request" } }] };
  }
  // The daemon closed the idle channel: dial again, and ask once the new
  // socket says `device not approved`.
  return dial(state, true);
}

function onRedeem(state: ConnectionState, requestId: string, code: string): StepResult {
  if (state.name !== "pairing" || !state.connected) {
    return unchanged(state);
  }
  return {
    state,
    effects: [
      { type: "send", message: { type: "device_pair_redeem", request_id: requestId, code } },
    ],
  };
}

function onMessage(state: ConnectionState, message: unknown): StepResult {
  const type = messageType(message);
  if (type === undefined) {
    return unchanged(state);
  }
  if (type === "device_pair_failed" && (state.name === "handshaking" || state.name === "ready")) {
    return refused(state, message as Extract<DaemonMessage, { type: "device_pair_failed" }>);
  }
  if (state.name === "handshaking") {
    return type === "welcome" ? welcome(state, message as WelcomeMessage) : unchanged(state);
  }
  if (state.name === "ready") {
    return { state, effects: [{ type: "deliver", message: message as DaemonMessage }] };
  }
  if (state.name === "pairing" && state.connected && PAIRING_MESSAGES.has(type)) {
    return { state, effects: [{ type: "deliver", message: message as DaemonMessage }] };
  }
  return unchanged(state);
}

/**
 * `DevicePairFailed` after `Welcome`. A connection that presented a token is
 * refused: it closes, and redialling would only repeat that. With the reason
 * `device not approved` the driver stops presenting the token for this visit
 * and redials without it; with any other only a new token redials. A no-token connection keeps its
 * socket as the pairing channel; a reason other than `device not approved`
 * is delivered to be shown, and the daemon decides whether to close. A
 * handshake still waiting has its Welcome timer cleared.
 */
function refused(
  state: Extract<ConnectionState, { name: "handshaking" | "ready" }>,
  message: Extract<DaemonMessage, { type: "device_pair_failed" }>,
): StepResult {
  const clear: ConnectionEffect[] = state.name === "handshaking" ? [{ type: "clearTimer" }] : [];
  if (!state.unpaired) {
    // `device not approved` for a presented token: the daemon does not know
    // it (or cannot read its device store), so this visit redials without
    // it and lands on the pairing channel.
    const drop: ConnectionEffect[] =
      message.reason === DEVICE_NOT_APPROVED ? [{ type: "dropToken" }] : [];
    return transition(state, { name: "notApproved" }, [...clear, { type: "close" }, ...drop]);
  }
  const effects: ConnectionEffect[] = [...clear];
  if (message.reason !== DEVICE_NOT_APPROVED) {
    effects.push({ type: "deliver", message });
  } else if (state.ask) {
    effects.push({ type: "send", message: { type: "device_pair_request" } });
  }
  return transition(state, { name: "pairing", connected: true }, effects);
}

function welcome(
  state: Extract<ConnectionState, { name: "handshaking" }>,
  message: WelcomeMessage,
): StepResult {
  return transition(
    state,
    {
      name: "ready",
      attempt: 0,
      features: new Set(message.features ?? []),
      daemonId: message.daemon_id ?? null,
      ...(state.unpaired ? { unpaired: true } : {}),
      ...(state.ask ? { ask: true } : {}),
    },
    [{ type: "clearTimer" }, { type: "deliver", message }],
  );
}

/** Open a socket from any state: no earlier timer may fire into the new one. */
function dial(state: ConnectionState, ask = false, lost: ConnectionEffect[] = []): StepResult {
  return transition(state, { name: "connecting", attempt: 0, ...(ask ? { ask: true } : {}) }, [
    { type: "clearTimer" },
    { type: "open" },
    ...lost,
  ]);
}

/** What a redial loses: an open pairing channel, or a tap's dial. */
function lostOnRedial(state: ConnectionState): ConnectionEffect[] {
  return state.name === "pairing" && state.connected ? [{ type: "pairingLost" }] : lostAsk(state);
}

/** A dial made by a tap that is lost before it could ask. */
function lostAsk(state: ConnectionState): ConnectionEffect[] {
  return "ask" in state && state.ask ? [{ type: "pairingLost" }] : [];
}

function backOff(state: ConnectionState, attempt: number, effects: ConnectionEffect[]): StepResult {
  return transition(state, { name: "backoff", attempt: attempt + 1 }, [
    ...effects,
    { type: "setTimer", delayMs: backoffDelayMs(attempt) },
  ]);
}

function transition(
  current: ConnectionState,
  next: ConnectionState,
  effects: ConnectionEffect[],
): StepResult {
  if (current.name === next.name) {
    return { state: next, effects };
  }
  return { state: next, effects: [...effects, { type: "status", state: next.name }] };
}

function unchanged(state: ConnectionState): StepResult {
  return { state, effects: [] };
}

function messageType(message: unknown): string | undefined {
  if (typeof message !== "object" || message === null || !("type" in message)) {
    return undefined;
  }
  return typeof message.type === "string" ? message.type : undefined;
}
