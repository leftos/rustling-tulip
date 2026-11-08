import type { ClientMessage } from "./protocol";

/**
 * How long after the user typed a size still counts as theirs: inside it a
 * measure claims the session's PTY size, and past it the next input claims
 * it back (`docs/wire-protocol.md`, the `Resize` lines).
 */
export const INPUT_WINDOW = 5_000;

/** Why a size is being sent. */
export type SizeReason =
  /** The terminal opened on the session: claimed, as opening it is the user asking to read it here. */
  | "attach"
  /** The connection came back: unclaimed, and the first input after it claims the size back. */
  | "reconnect"
  /** The Terminal toggle opened the pane: claimed, as opening it is the user asking to read it here. */
  | "toggle"
  /** A session's scrollback finished loading and the grid may have changed: unclaimed. */
  | "load"
  /** The user resized the pane (orientation, the keyboard, the font): claimed. */
  | "pane-resize"
  /** Any other measure, such as a re-layout: claimed only while the user typed recently. */
  | "measure";

/**
 * One session's size on the phone: the pane's size as last sent, and when the
 * user last typed, which decides whether the next input or measure claims it.
 */
export interface SizeState {
  readonly sessionId: string;
  /** The pane's size as last sent; what an input's claimed resize repeats. */
  readonly cols: number;
  readonly rows: number;
  /** When the user last typed since the attach or reconnect; `null` when none. */
  readonly typedAt: number | null;
}

/** The size model of a session whose pane has not been measured yet. */
export function sessionSize(sessionId: string): SizeState {
  return { sessionId, cols: 0, rows: 0, typedAt: null };
}

/** One size action: the model after it, and the resize to send. */
export interface SizeTransition {
  readonly state: SizeState;
  readonly message: ClientMessage | null;
}

/**
 * The `Resize` for a measure of `cols` by `rows` taken at `now`. An attach, a
 * Terminal toggle and a user's own pane resize claim the session, since each
 * is the user asking to read it here; a reconnect and the end of a load never
 * do, so they cannot take the size from a client that owns it; any other
 * measure claims only while the user typed within the last 5 s. An attach and
 * a reconnect start the typing clock over, so the first input after one
 * claims. A size that is not positive is no measure at all: nothing is sent
 * and nothing is stored.
 */
export function resizeFor(
  state: SizeState,
  cols: number,
  rows: number,
  reason: SizeReason,
  now: number,
): SizeTransition {
  if (cols <= 0 || rows <= 0) {
    return { state, message: null };
  }
  const reset = reason === "attach" || reason === "reconnect";
  return {
    state: { ...state, cols, rows, typedAt: reset ? null : state.typedAt },
    message: {
      type: "resize",
      session_id: state.sessionId,
      cols,
      rows,
      claim: claims(reason, state, now),
    },
  };
}

/**
 * The user typed at `now`. Typing arms the session's size: the first input
 * after 5 s of quiet, and the first after an attach or a reconnect, returns
 * the pane's size as a claimed `Resize` to send ahead of the input. Inputs
 * sooner than that send nothing, since a claim is already out.
 */
export function noteInput(state: SizeState, now: number): SizeTransition {
  const previous = state.typedAt;
  const next = { ...state, typedAt: now };
  const first = previous === null || now - previous >= INPUT_WINDOW;
  if (!first || state.cols === 0 || state.rows === 0) {
    return { state: next, message: null };
  }
  return {
    state: next,
    message: {
      type: "resize",
      session_id: state.sessionId,
      cols: state.cols,
      rows: state.rows,
      claim: true,
    },
  };
}

/** Whether a measure taken for `reason` claims the session's size. */
function claims(reason: SizeReason, state: SizeState, now: number): boolean {
  switch (reason) {
    case "attach":
    case "pane-resize":
    case "toggle":
      return true;
    case "reconnect":
    case "load":
      return false;
    case "measure":
      return typedWithin(state, now);
  }
}

/** Whether the user typed within the claim window before `now`. */
function typedWithin(state: SizeState, now: number): boolean {
  return state.typedAt !== null && now - state.typedAt < INPUT_WINDOW;
}
