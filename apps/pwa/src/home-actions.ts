import type { ClientMessage, DaemonMessage, PromptChoice, SessionSnapshot } from "./protocol";

/** The three things the Home screen sends about a session. */
export type ActionKind = "answer" | "stop" | "resume";

/** A request the daemon has not answered yet. */
export interface PendingRequest {
  /** The message's `request_id`; null for `ResumeAbandoned`, which carries none. */
  readonly requestId: string | null;
  readonly sessionId: string;
  readonly action: ActionKind;
}

/**
 * What the Home screen keeps between taps: the requests on their way, the
 * Stop waiting for its confirming second tap, and the daemon's refusal text
 * per session.
 */
export interface HomeActions {
  readonly pending: readonly PendingRequest[];
  /** The session whose Stop waits for its second tap; null when none does. */
  readonly armed: string | null;
  /** The daemon's refusal text per session, shown on its row or card. */
  readonly errors: ReadonlyMap<string, string>;
}

/** The state before any tap: what `reset` returns. */
export const NO_ACTIONS: HomeActions = { pending: [], armed: null, errors: new Map() };

/** The state a screen starts from on every `ready`, since no reply is coming. */
export function reset(): HomeActions {
  return NO_ACTIONS;
}

/** A fresh `request_id`; the screen injects `crypto.randomUUID` or a counter. */
export type NewRequestId = () => string;

/** What a tap produces: the message to send, if any, and the state after it. */
export interface ActionResult {
  /** The message to send; null when the tap sends nothing. */
  readonly message: ClientMessage | null;
  readonly state: HomeActions;
}

/** What a folded message leaves to send: empty except after a landed resume. */
export interface FoldResult {
  readonly state: HomeActions;
  readonly effects: readonly ClientMessage[];
}

/**
 * The answer to a card's choice, sent against the `status_since` the card
 * was built from. A tap while an answer for the session is still pending
 * sends nothing.
 */
export function answer(
  state: HomeActions,
  newRequestId: NewRequestId,
  sessionId: string,
  since: string,
  choice: PromptChoice,
): ActionResult {
  if (isPending(state, sessionId, "answer")) {
    return { message: null, state };
  }
  const requestId = newRequestId();
  return {
    message: { type: "answer_prompt", session_id: sessionId, since, choice, request_id: requestId },
    state: withPending(clearError(state, sessionId), {
      requestId,
      sessionId,
      action: "answer",
    }),
  };
}

/**
 * Stop a session: the first tap arms the confirm and sends nothing, the
 * second sends `StopSession` with nothing to clean up. Arming another
 * session disarms the first.
 */
export function stop(state: HomeActions, sessionId: string): ActionResult {
  if (state.armed !== sessionId) {
    return { message: null, state: { ...state, armed: sessionId } };
  }
  return {
    message: { type: "stop_session", session_id: sessionId, cleanup: [] },
    state: { ...clearError(state, sessionId), armed: null },
  };
}

/** Drops an armed Stop, as the screen does when its list scrolls away. */
export function disarm(state: HomeActions): HomeActions {
  return state.armed === null ? state : { ...state, armed: null };
}

/**
 * Resume a session, or nothing when it offers no resume: a headless session
 * is never rerun, a running one has nothing to resume, and a tap while a
 * resume is still pending sends nothing. An abandoned session resumes from
 * the config the daemon captured; a stopped or parked one reruns in its own
 * checkout, keeping its "don't count as busy".
 */
export function resume(
  state: HomeActions,
  newRequestId: NewRequestId,
  session: SessionSnapshot,
): ActionResult {
  if (session.mode === "headless" || isPending(state, session.id, "resume")) {
    return { message: null, state };
  }
  if (session.is_abandoned === true) {
    // `ResumeAbandoned` carries no request id, so a failure naming none cannot
    // be matched to this session; the record clears on the session's next
    // update or on `reset`.
    return {
      message: { type: "resume_abandoned", session_id: session.id },
      state: withPending(clearError(state, session.id), {
        requestId: null,
        sessionId: session.id,
        action: "resume",
      }),
    };
  }
  if (!isStopped(session)) {
    return { message: null, state };
  }
  const requestId = newRequestId();
  return {
    message: {
      type: "duplicate_session",
      session_id: session.id,
      request_id: requestId,
      carry_busy_excluded: true,
      same_checkout: true,
    },
    state: withPending(clearError(state, session.id), {
      requestId,
      sessionId: session.id,
      action: "resume",
    }),
  };
}

/** The refusal text shown for `sessionId`, if the daemon last refused it. */
export function errorFor(state: HomeActions, sessionId: string): string | null {
  return state.errors.get(sessionId) ?? null;
}

/**
 * One daemon message folded into the state. A refusal echoing a pending
 * request (an `Error` or an `ActionFailed`) becomes that session's error
 * text, the daemon's message verbatim, and frees the request so the user can
 * tap again. A `SessionUpdated` echoing a request means it landed; for a
 * resume it also returns the `DiscardSession` the desktop sends for the
 * original, so the row it replaced leaves the list. A session's own update
 * clears its error text and any answer or abandoned-resume wait, which have
 * no reply of their own. Pure; a message the state does not read returns the
 * state itself.
 */
export function foldReply(state: HomeActions, message: DaemonMessage): FoldResult {
  switch (message.type) {
    case "error":
      return refuse(state, message.request_id ?? null, message.message);
    case "action_failed":
      return refuse(state, message.request_id ?? null, message.detail);
    case "session_updated":
      return onSessionUpdated(state, message.session, message.request_id ?? null);
    default:
      return { state, effects: [] };
  }
}

/** A refusal echoing `requestId`: the session's error text and its record freed. */
function refuse(state: HomeActions, requestId: string | null, text: string): FoldResult {
  if (requestId === null) {
    return { state, effects: [] };
  }
  const request = findRequest(state, requestId);
  if (request === undefined) {
    return { state, effects: [] };
  }
  const errors = new Map(state.errors);
  errors.set(request.sessionId, text);
  return {
    state: { ...state, pending: withoutRequest(state, requestId), errors },
    effects: [],
  };
}

/** A session's update: it says which requests landed and clears its error. */
function onSessionUpdated(
  state: HomeActions,
  session: SessionSnapshot,
  requestId: string | null,
): FoldResult {
  const landed = requestId === null ? undefined : findRequest(state, requestId);
  const done = requestId === null ? state : forgetRequest(state, requestId);
  const idle = forgetSessionRequests(done, session.id);
  const discardOriginal =
    landed?.action === "resume" && landed.requestId !== null
      ? [{ type: "discard_session" as const, session_id: landed.sessionId, cleanup: [] }]
      : [];
  return { state: clearError(idle, session.id), effects: discardOriginal };
}

/** Whether a stopped session is the state a Resume reruns in its checkout. */
function isStopped(session: SessionSnapshot): boolean {
  return session.is_inactive === true || session.status === "stopped" || session.status === "error";
}

function isPending(state: HomeActions, sessionId: string, action: ActionKind): boolean {
  return state.pending.some(
    (request) => request.sessionId === sessionId && request.action === action,
  );
}

function findRequest(state: HomeActions, requestId: string): PendingRequest | undefined {
  return state.pending.find((request) => request.requestId === requestId);
}

function withoutRequest(state: HomeActions, requestId: string): readonly PendingRequest[] {
  return state.pending.filter((request) => request.requestId !== requestId);
}

function forgetRequest(state: HomeActions, requestId: string): HomeActions {
  const pending = withoutRequest(state, requestId);
  return pending.length === state.pending.length ? state : { ...state, pending };
}

/** Drops the session's waits that no reply can end: answers and abandoned resumes. */
function forgetSessionRequests(state: HomeActions, sessionId: string): HomeActions {
  const pending = state.pending.filter(
    (request) =>
      request.sessionId !== sessionId ||
      (request.action === "resume" && request.requestId !== null),
  );
  return pending.length === state.pending.length ? state : { ...state, pending };
}

function withPending(state: HomeActions, request: PendingRequest): HomeActions {
  return { ...state, pending: [...state.pending, request] };
}

function clearError(state: HomeActions, sessionId: string): HomeActions {
  if (!state.errors.has(sessionId)) {
    return state;
  }
  const errors = new Map(state.errors);
  errors.delete(sessionId);
  return { ...state, errors };
}
