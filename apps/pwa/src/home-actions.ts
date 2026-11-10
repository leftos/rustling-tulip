import type {
  ClientMessage,
  ContainerRef,
  DaemonMessage,
  PromptChoice,
  SessionSnapshot,
} from "./protocol";

/** The three things the Home screen sends about a session. */
export type ActionKind = "answer" | "stop" | "resume";

/** A request the daemon has not answered yet. */
export interface PendingRequest {
  /** The message's `request_id`; null for `ResumeAbandoned`, which carries none. */
  readonly requestId: string | null;
  readonly sessionId: string;
  readonly action: ActionKind;
  /**
   * A resume whose landing leaves the original for the phone to discard: a
   * parked or stopped session's rerun. An abandoned session's resume
   * replaces it on the host, so there is nothing to discard. Absent on
   * answers.
   */
  readonly discardsOriginal?: boolean;
}

/** A Launch last on its way: one per container, kept until the daemon answers it. */
export interface PendingLaunch {
  readonly requestId: string;
  readonly target: ContainerRef;
  /** When the request's first send went out, ms since the epoch: the daemon's journal clock. */
  readonly sentAt: number;
  /** When the latest send, the first or a resend, went out: the answer clock. */
  readonly lastSentAt: number;
}

/**
 * How long the daemon's spawn journal remembers a request id. A resend later
 * than this is a new request and spawns a second session.
 */
export const LAUNCH_RESEND_LIMIT_MS = 9 * 60 * 1000;

/**
 * How long an unanswered launch waits for its echo. The daemon keeps quiet to
 * a repeat while the first spawn still runs, and that spawn's echo went to
 * the connection that has since died.
 */
export const LAUNCH_ANSWER_LIMIT_MS = 60 * 1000;

/** What a container says when its launch may have started a session anyway. */
export const LAUNCH_MAYBE_STARTED =
  "This launch may have started a session. Check the list before launching again.";

/**
 * What the Home screen keeps between taps: the requests on their way, the
 * Stop waiting for its confirming second tap, the daemon's refusal text per
 * session, and the launches the daemon has not answered yet.
 */
export interface HomeActions {
  readonly pending: readonly PendingRequest[];
  /** The session whose Stop waits for its second tap; null when none does. */
  readonly armed: string | null;
  /** The daemon's refusal text per session, shown on its row or card. */
  readonly errors: ReadonlyMap<string, string>;
  /** The launches waiting for the daemon, one per container. */
  readonly launches: readonly PendingLaunch[];
  /** The daemon's refusal text per container, keyed by `containerKey`. */
  readonly launchErrors: ReadonlyMap<string, string>;
}

/** The state before any tap: what `reset` returns. */
export const NO_ACTIONS: HomeActions = {
  pending: [],
  armed: null,
  errors: new Map(),
  launches: [],
  launchErrors: new Map(),
};

/** What the phone says when the host no longer lists `launch_last`. */
export const UNSUPPORTED_LAUNCH = "This host can't launch from the phone.";

/** The key a launch's pending record and its refusal text are filed under. */
export function containerKey(target: ContainerRef): string {
  return `${target.kind}:${target.id}`;
}

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
 * checkout, keeping its "don't count as busy". A host listing
 * `resume_session` gets one `ResumeSession` for either and picks how; an
 * older host gets `ResumeAbandoned` or the duplicate recipe.
 */
export function resume(
  state: HomeActions,
  newRequestId: NewRequestId,
  session: SessionSnapshot,
  supportsResumeSession: boolean,
): ActionResult {
  if (session.mode === "headless" || isPending(state, session.id, "resume")) {
    return { message: null, state };
  }
  const abandoned = session.is_abandoned === true;
  if (supportsResumeSession && (abandoned || isStopped(session))) {
    const requestId = newRequestId();
    return {
      message: { type: "resume_session", session_id: session.id, request_id: requestId },
      state: withPending(clearError(state, session.id), {
        requestId,
        sessionId: session.id,
        action: "resume",
        discardsOriginal: !abandoned,
      }),
    };
  }
  if (abandoned) {
    // `ResumeAbandoned` carries no request id, so a failure naming none cannot
    // be matched to this session; the record clears on the session's next
    // update or on `reset`.
    return {
      message: { type: "resume_abandoned", session_id: session.id },
      state: withPending(clearError(state, session.id), {
        requestId: null,
        sessionId: session.id,
        action: "resume",
        discardsOriginal: false,
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
      discardsOriginal: true,
    }),
  };
}

/** The refusal text shown for `sessionId`, if the daemon last refused it. */
export function errorFor(state: HomeActions, sessionId: string): string | null {
  return state.errors.get(sessionId) ?? null;
}

/**
 * Launch the container's stored last spawn config again, or nothing while its
 * earlier tap still waits for the daemon. The launch is recorded, with `now`
 * as its first send, before the send, so a tap that reaches no daemon is
 * resent on the next reconnect.
 */
export function launchLast(
  state: HomeActions,
  target: ContainerRef,
  newRequestId: NewRequestId,
  now: number,
): ActionResult {
  const key = containerKey(target);
  if (state.launches.some((launch) => containerKey(launch.target) === key)) {
    return { message: null, state };
  }
  const requestId = newRequestId();
  return {
    message: { type: "launch_last", target, request_id: requestId },
    state: {
      ...state,
      launches: [...state.launches, { requestId, target, sentAt: now, lastSentAt: now }],
      launchErrors: withoutLaunchError(state.launchErrors, key),
    },
  };
}

/**
 * The state a screen starts from on every `ready`: the requests on their way
 * are gone, but a pending launch still waits for an answer and is resent.
 */
export function keepLaunches(old: HomeActions): HomeActions {
  return { ...NO_ACTIONS, launches: old.launches };
}

/** What a reconnect leaves to send: the pending launches, each with its id. */
export interface LaunchResend {
  readonly messages: readonly ClientMessage[];
  readonly state: HomeActions;
}

/**
 * The messages a reconnect sends for the launches still waiting. A host that
 * launches gets each one again with its earlier id, so the daemon's journal
 * makes one session: a launch older than the journal is dropped instead,
 * since a resend would make a second session. An older host gets nothing,
 * its launches are dropped, and each container shows why.
 */
export function resendLaunches(
  state: HomeActions,
  supportsLaunchLast: boolean,
  now: number,
): LaunchResend {
  if (state.launches.length === 0) {
    return { messages: [], state };
  }
  const launchErrors = new Map(state.launchErrors);
  const messages: ClientMessage[] = [];
  const launches: PendingLaunch[] = [];
  for (const launch of state.launches) {
    const key = containerKey(launch.target);
    if (!supportsLaunchLast) {
      launchErrors.set(key, UNSUPPORTED_LAUNCH);
    } else if (now - launch.sentAt >= LAUNCH_RESEND_LIMIT_MS) {
      launchErrors.set(key, LAUNCH_MAYBE_STARTED);
    } else {
      launches.push({ ...launch, lastSentAt: now });
      messages.push({ type: "launch_last", target: launch.target, request_id: launch.requestId });
    }
  }
  return { messages, state: { ...state, launches, launchErrors } };
}

/**
 * Drops the launches whose answer never came, a minute after their latest
 * send, each container saying it may have started a session anyway. Sends
 * nothing; returns the state itself when every launch is still waiting.
 */
export function expireLaunches(state: HomeActions, now: number): HomeActions {
  if (state.launches.length === 0) {
    return state;
  }
  const launchErrors = new Map(state.launchErrors);
  const launches: PendingLaunch[] = [];
  for (const launch of state.launches) {
    if (now - launch.lastSentAt >= LAUNCH_ANSWER_LIMIT_MS) {
      launchErrors.set(containerKey(launch.target), LAUNCH_MAYBE_STARTED);
    } else {
      launches.push(launch);
    }
  }
  return launches.length === state.launches.length ? state : { ...state, launches, launchErrors };
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

/** A refusal echoing `requestId`: the session's or the launch's error text, its record freed. */
function refuse(state: HomeActions, requestId: string | null, text: string): FoldResult {
  if (requestId === null) {
    return { state, effects: [] };
  }
  const request = findRequest(state, requestId);
  if (request !== undefined) {
    const errors = new Map(state.errors);
    errors.set(request.sessionId, text);
    return {
      state: { ...state, pending: withoutRequest(state, requestId), errors },
      effects: [],
    };
  }
  const launch = findLaunch(state, requestId);
  if (launch === undefined) {
    return { state, effects: [] };
  }
  const launchErrors = new Map(state.launchErrors);
  launchErrors.set(containerKey(launch.target), text);
  return {
    state: { ...state, launches: withoutLaunch(state, requestId), launchErrors },
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
  const settled = requestId === null ? done : forgetLaunch(done, requestId);
  const idle = forgetSessionRequests(settled, session.id);
  const discardOriginal =
    landed?.action === "resume" && landed.discardsOriginal === true
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

function findLaunch(state: HomeActions, requestId: string): PendingLaunch | undefined {
  return state.launches.find((launch) => launch.requestId === requestId);
}

function withoutLaunch(state: HomeActions, requestId: string): readonly PendingLaunch[] {
  return state.launches.filter((launch) => launch.requestId !== requestId);
}

function forgetLaunch(state: HomeActions, requestId: string): HomeActions {
  const launches = withoutLaunch(state, requestId);
  return launches.length === state.launches.length ? state : { ...state, launches };
}

function withoutLaunchError(
  errors: ReadonlyMap<string, string>,
  key: string,
): ReadonlyMap<string, string> {
  if (!errors.has(key)) {
    return errors;
  }
  const next = new Map(errors);
  next.delete(key);
  return next;
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
