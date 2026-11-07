import { backoffDelayMs } from "./backoff";
import { buildHello } from "./hello";
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
  | "notApproved";

export type ConnectionState =
  | { readonly name: "idle" }
  | { readonly name: "connecting"; readonly attempt: number }
  | { readonly name: "handshaking"; readonly attempt: number }
  | {
      readonly name: "ready";
      readonly attempt: number;
      readonly features: ReadonlySet<string>;
      readonly daemonId: string | null;
    }
  | { readonly name: "backoff"; readonly attempt: number }
  | { readonly name: "hidden" }
  | { readonly name: "notApproved" };

export type ConnectionEvent =
  | { readonly type: "start" }
  | { readonly type: "opened"; readonly token: string | undefined }
  | { readonly type: "message"; readonly message: unknown }
  | { readonly type: "closed" }
  | { readonly type: "timer" }
  | { readonly type: "visible" }
  | { readonly type: "hiddenEvent" }
  | { readonly type: "tokenChanged" };

export type ConnectionEffect =
  | { readonly type: "open" }
  | { readonly type: "send"; readonly message: ClientMessage }
  | { readonly type: "close" }
  | { readonly type: "setTimer"; readonly delayMs: number }
  | { readonly type: "clearTimer" }
  | { readonly type: "deliver"; readonly message: DaemonMessage }
  | { readonly type: "status"; readonly state: ConnectionStateName };

export interface StepResult {
  readonly state: ConnectionState;
  readonly effects: ConnectionEffect[];
}

type WelcomeMessage = Extract<DaemonMessage, { type: "welcome" }>;

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
      return dial(state);
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
  }
}

function onOpened(state: ConnectionState, token: string | undefined): StepResult {
  if (state.name !== "connecting") {
    return unchanged(state);
  }
  return transition(state, { name: "handshaking", attempt: state.attempt }, [
    { type: "send", message: buildHello(token) },
    { type: "setTimer", delayMs: WELCOME_TIMEOUT_MS },
  ]);
}

function onLost(state: ConnectionState): StepResult {
  if (state.name === "connecting" || state.name === "handshaking" || state.name === "ready") {
    return backOff(state, state.attempt, []);
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
    return backOff(state, state.attempt, [{ type: "close" }]);
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
  return transition(state, { name: "hidden" }, [{ type: "clearTimer" }, { type: "close" }]);
}

function onMessage(state: ConnectionState, message: unknown): StepResult {
  const type = messageType(message);
  if (type === undefined) {
    return unchanged(state);
  }
  if (type === "device_pair_failed" && (state.name === "handshaking" || state.name === "ready")) {
    // The daemon answered `Welcome` and then said the token is not approved:
    // redialling would only repeat that, and only a new token redials. A
    // handshake still waiting has its Welcome timer cleared.
    const effects: ConnectionEffect[] =
      state.name === "handshaking"
        ? [{ type: "clearTimer" }, { type: "close" }]
        : [{ type: "close" }];
    return transition(state, { name: "notApproved" }, effects);
  }
  if (state.name === "handshaking") {
    return type === "welcome" ? welcome(state, message as WelcomeMessage) : unchanged(state);
  }
  if (state.name === "ready") {
    return { state, effects: [{ type: "deliver", message: message as DaemonMessage }] };
  }
  return unchanged(state);
}

function welcome(state: ConnectionState, message: WelcomeMessage): StepResult {
  return transition(
    state,
    {
      name: "ready",
      attempt: 0,
      features: new Set(message.features ?? []),
      daemonId: message.daemon_id ?? null,
    },
    [{ type: "clearTimer" }, { type: "deliver", message }],
  );
}

/** Open a socket from any state: no earlier timer may fire into the new one. */
function dial(state: ConnectionState): StepResult {
  return transition(state, { name: "connecting", attempt: 0 }, [
    { type: "clearTimer" },
    { type: "open" },
  ]);
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
