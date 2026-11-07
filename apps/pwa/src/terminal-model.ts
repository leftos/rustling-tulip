import type { NewRequestId } from "./home-actions";
import type { ClientMessage, DaemonMessage } from "./protocol";

/**
 * How long one `load_scrollback` waits for its reply, the desktop's
 * `REQUEST_TIMEOUT` (`apps/native/src/scrollback_load.rs`).
 */
export const REQUEST_TIMEOUT = 8_000;

/**
 * The pause after each timed-out attempt before the next one goes out; the
 * attempt after the last of these timing out ends the load (`RETRY_DELAYS`).
 */
export const RETRY_DELAYS = [2_000, 4_000] as const;

/** Where a session's terminal load stands. */
export type TerminalPhase = "idle" | "loading" | "loaded" | "timed-out";

/**
 * One session's terminal on the phone: the scrollback load still running, the
 * live output held until its reply, and the timer that retries it.
 */
export interface TerminalState {
  /** The session attached; `null` before one is and after a detach. */
  readonly sessionId: string | null;
  readonly phase: TerminalPhase;
  /** The attempt whose reply is awaited, 1-based; 0 when none is out. */
  readonly attempt: number;
  /** Whether the load is pausing before its next attempt rather than awaiting a reply. */
  readonly backoff: boolean;
  /** The clock reading at which the current wait ends; `null` when nothing is scheduled. */
  readonly deadline: number | null;
  /** The `load_scrollback` whose reply is written; only that id's reply is. */
  readonly requestId: string | null;
  /** Live output held back until the history is written, in arrival order. */
  readonly held: readonly Uint8Array[];
}

export const INITIAL_TERMINAL: TerminalState = {
  sessionId: null,
  phase: "idle",
  attempt: 0,
  backoff: false,
  deadline: null,
  requestId: null,
  held: [],
};

/** One event applied to the model: the state after it, and what it produces. */
export interface TerminalTransition {
  readonly state: TerminalState;
  /** The messages to send, in order. */
  readonly messages: readonly ClientMessage[];
  /** The bytes to write into the terminal, in order. */
  readonly writes: readonly Uint8Array[];
  /** Whether the terminal is cleared before `writes`, as a resync is replayed from its start. */
  readonly reset: boolean;
  /** Whether the pane's current size is sent again, unclaimed: a load is over. */
  readonly resize: boolean;
}

type PtyOutput = Extract<DaemonMessage, { type: "pty_output" }>;
type Scrollback = Extract<DaemonMessage, { type: "scrollback" }>;

/**
 * Attach `sessionId`: leave the session already attached, then the
 * `load_scrollback` under a fresh request id, its 8 s timeout running from
 * `now`, and the session's live output held until the reply arrives.
 */
export function attach(
  state: TerminalState,
  sessionId: string,
  nextRequestId: NewRequestId,
  now: number,
): TerminalTransition {
  const messages: ClientMessage[] = [];
  const previous = state.sessionId;
  if (previous !== null && previous !== sessionId) {
    messages.push({ type: "detach", session_id: previous });
  }
  const requestId = nextRequestId();
  messages.push({ type: "load_scrollback", session_id: sessionId, request_id: requestId });
  return outcome(load(sessionId, requestId, now), messages);
}

/**
 * The connection came back: attach the session again under a fresh request
 * id. Output held from the old connection is gone, so the load starts empty.
 */
export function ready(
  state: TerminalState,
  nextRequestId: NewRequestId,
  now: number,
): TerminalTransition {
  const sessionId = state.sessionId;
  if (sessionId === null) {
    return outcome(state);
  }
  return attach(state, sessionId, nextRequestId, now);
}

/**
 * Leave the session: the `detach` message that stops its forwarder, and the
 * empty model, which clears the load's timer and held output.
 */
export function detach(state: TerminalState): TerminalTransition {
  const sessionId = state.sessionId;
  if (sessionId === null) {
    return outcome(INITIAL_TERMINAL);
  }
  return outcome(INITIAL_TERMINAL, [{ type: "detach", session_id: sessionId }]);
}

/**
 * One daemon message folded into the load: the session's live output, or the
 * reply that ends the load and releases the held output. Any other message
 * changes nothing.
 */
export function fold(state: TerminalState, message: DaemonMessage): TerminalTransition {
  switch (message.type) {
    case "pty_output":
      return onOutput(state, message);
    case "scrollback":
      return onReply(state, message);
    default:
      return outcome(state);
  }
}

/**
 * Advances the load's timeout and retry schedule to `now`: a timed-out
 * attempt pauses for the retry delay and then sends its attempt again under a
 * fresh request id, leaving the held output behind since that attempt's
 * snapshot carries it; after the last attempt times out the load gives up,
 * writes the output still held and asks for the pane's size again, since the
 * grid may have changed while the history was missing.
 */
export function tick(
  state: TerminalState,
  now: number,
  nextRequestId: NewRequestId,
): TerminalTransition {
  const sessionId = state.sessionId;
  if (state.phase !== "loading" || sessionId === null) {
    return outcome(state);
  }
  const messages: ClientMessage[] = [];
  const writes: Uint8Array[] = [];
  let current = state;
  while (current.phase === "loading" && current.deadline !== null && now >= current.deadline) {
    if (current.backoff) {
      const requestId = nextRequestId();
      current = {
        ...current,
        attempt: current.attempt + 1,
        backoff: false,
        deadline: now + REQUEST_TIMEOUT,
        requestId,
        held: [],
      };
      messages.push({ type: "load_scrollback", session_id: sessionId, request_id: requestId });
      continue;
    }
    const delay = RETRY_DELAYS[current.attempt - 1];
    if (delay === undefined) {
      writes.push(...current.held);
      current = ended(current, "timed-out");
      break;
    }
    current = { ...current, backoff: true, deadline: now + delay };
  }
  return outcome(current, messages, writes, false, current.phase === "timed-out");
}

/** The model of a fresh load for `sessionId`, its first reply awaited until `now`'s timeout. */
function load(sessionId: string, requestId: string, now: number): TerminalState {
  return {
    sessionId,
    phase: "loading",
    attempt: 1,
    backoff: false,
    deadline: now + REQUEST_TIMEOUT,
    requestId,
    held: [],
  };
}

/** Live output: held until the history is written, and written at once after it. */
function onOutput(state: TerminalState, message: PtyOutput): TerminalTransition {
  if (!sameSession(state, message.session_id)) {
    return outcome(state);
  }
  const bytes = decode(message.data_b64);
  if (bytes === null) {
    return outcome(state);
  }
  if (state.phase === "loading") {
    return outcome({ ...state, held: [...state.held, bytes] });
  }
  return outcome(state, [], [bytes]);
}

/**
 * The scrollback reply: its history, then any output held behind it. Only the
 * latest request's reply is written, and a reply to an earlier one is
 * dropped; a reply carrying no request id is the daemon's resync, which is
 * handled on its own. A reply from a restarted forwarder is written without
 * the held output, which is already in its history.
 */
function onReply(state: TerminalState, message: Scrollback): TerminalTransition {
  if (!sameSession(state, message.session_id)) {
    return outcome(state);
  }
  const replied = message.request_id ?? null;
  if (replied === null) {
    return onResync(state, message);
  }
  if (state.phase !== "loading" || replied !== state.requestId) {
    return outcome(state);
  }
  const history = decode(message.data_b64);
  if (history === null) {
    return outcome(state);
  }
  return replay(state, history, message.forwarder_restarted === true);
}

/**
 * A reply with no request id is the daemon's resync of a session whose output
 * outran this connection. While the pane's own load is pending its reply
 * supersedes the resync, so the resync is dropped; otherwise the terminal
 * starts over from the snapshot: it is cleared, the history is written, and
 * the pane's size is sent again, unclaimed.
 */
function onResync(state: TerminalState, message: Scrollback): TerminalTransition {
  if (state.phase === "loading" || message.forwarder_restarted !== true) {
    return outcome(state);
  }
  const history = decode(message.data_b64);
  if (history === null) {
    return outcome(state);
  }
  const written = history.length === 0 ? [] : [history];
  return outcome(ended(state, "loaded"), [], written, true, true);
}

/** Writes a load's history, then the output held behind it unless the forwarder restarted. */
function replay(state: TerminalState, history: Uint8Array, restarted: boolean): TerminalTransition {
  const written = history.length === 0 ? [] : [history];
  const held = restarted ? [] : state.held;
  return outcome(ended(state, "loaded"), [], [...written, ...held], false, true);
}

/** The state once the load is over: no timer, no request, nothing held. */
function ended(state: TerminalState, phase: TerminalPhase): TerminalState {
  return {
    ...state,
    phase,
    attempt: 0,
    backoff: false,
    deadline: null,
    requestId: null,
    held: [],
  };
}

/** Whether `sessionId` is the session this model attached. */
function sameSession(state: TerminalState, sessionId: string): boolean {
  return state.sessionId !== null && state.sessionId === sessionId;
}

/**
 * `dataB64` as bytes, through a binary string so it works in node and in
 * Safari; `null`, with a warning, when it is not base64, which leaves the
 * model as it was rather than writing garbage into the terminal.
 */
function decode(dataB64: string): Uint8Array | null {
  try {
    const binary = atob(dataB64);
    return Uint8Array.from(binary, (c) => c.charCodeAt(0));
  } catch (error) {
    console.warn("pwa: dropped terminal bytes that are not base64", error);
    return null;
  }
}

/** A transition: `state` and what it produces, nothing when the rest is left out. */
function outcome(
  state: TerminalState,
  messages: readonly ClientMessage[] = [],
  writes: readonly Uint8Array[] = [],
  reset = false,
  resize = false,
): TerminalTransition {
  return { state, messages, writes, reset, resize };
}
