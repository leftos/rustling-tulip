import type {
  ClientMessage,
  ContainerRef,
  DaemonMessage,
  LaunchPresetSource,
  PresetTarget,
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

/** The two launches a container's tap sends: its stored last launch, or a new `/nextup` session. */
export type LaunchKind = "launch_last" | "new_session";

/** Whether the host takes a launch of this kind: it lists the kind's feature. */
export type LaunchSupport = (kind: LaunchKind) => boolean;

/** The first prompt a New session from the phone starts with. */
export const NEW_SESSION_PROMPT = "/nextup";

/**
 * A Launch last or a New session on its way: one per container, whatever its
 * kind, kept until the daemon answers it.
 */
export interface PendingLaunch {
  readonly kind: LaunchKind;
  readonly requestId: string;
  readonly target: ContainerRef;
  /** When the request's first send went out, ms since the epoch: the daemon's journal clock. */
  readonly sentAt: number;
  /** When the latest send, the first or a resend, went out: the answer clock. */
  readonly lastSentAt: number;
}

/** A preset launch the phone asks for: the preset, its container, and the source it fills. */
export interface PresetLaunchRequest {
  readonly target: ContainerRef;
  readonly presetId: string;
  readonly source: LaunchPresetSource;
}

/**
 * A preset launch on its way: one per preset per container, kept until the
 * job's first update answers it. Its job id is its journal key on the host.
 */
export interface PendingPresetLaunch extends PresetLaunchRequest {
  readonly jobId: string;
  /** When the launch's first send went out, ms since the epoch: the journal clock. */
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
  /** The daemon's refusal of a Launch last per container, keyed by `containerKey`. */
  readonly launchErrors: ReadonlyMap<string, string>;
  /** The daemon's refusal of a New session per container, keyed by `containerKey`. */
  readonly newSessionErrors: ReadonlyMap<string, string>;
  /** The preset launches waiting for their job's first update, one per preset per container. */
  readonly presetLaunches: readonly PendingPresetLaunch[];
  /** Why a preset's launch was dropped, keyed by `presetKey`. */
  readonly presetErrors: ReadonlyMap<string, string>;
}

/** The state before any tap: what `reset` returns. */
export const NO_ACTIONS: HomeActions = {
  pending: [],
  armed: null,
  errors: new Map(),
  launches: [],
  launchErrors: new Map(),
  newSessionErrors: new Map(),
  presetLaunches: [],
  presetErrors: new Map(),
};

/** What the phone says when the host no longer lists `launch_last`, or `new_session` for a New session. */
export const UNSUPPORTED_LAUNCH = "This host can't launch from the phone.";

/** What a preset says when its unanswered launch cannot be resent: the host lacks `preset_job_repeat`. */
export const UNSUPPORTED_PRESET_RESEND =
  "This host can't resend a preset launch; check the desktop.";

/** The key a preset's pending launch and its dropped-launch text are filed under. */
export function presetKey(target: ContainerRef, presetId: string): string {
  return `${containerKey(target)}/${presetId}`;
}

/** The kind of launch waiting on a container; null when none waits. */
export function pendingLaunchKind(state: HomeActions, target: ContainerRef): LaunchKind | null {
  const key = containerKey(target);
  return state.launches.find((launch) => containerKey(launch.target) === key)?.kind ?? null;
}

/** The key a launch's pending record and its refusal text are filed under. */
export function containerKey(target: ContainerRef): string {
  return `${target.kind}:${target.id}`;
}

/** A container as the preset messages name it. */
export function presetTarget(target: ContainerRef): PresetTarget {
  return target.kind === "repo"
    ? { kind: "repo", repo_id: target.id }
    : { kind: "workspace", workspace_id: target.id };
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
  return startLaunch(state, "launch_last", target, newRequestId, now);
}

/**
 * Start a new session in the container with `/nextup` as its first prompt,
 * or nothing while a launch of either kind on it still waits for the
 * daemon. Recorded before the send, as a Launch last is, and resent the same way.
 */
export function newSession(
  state: HomeActions,
  target: ContainerRef,
  newRequestId: NewRequestId,
  now: number,
): ActionResult {
  return startLaunch(state, "new_session", target, newRequestId, now);
}

/** A launch of `kind` on `target`, refused while any launch on that container waits. */
function startLaunch(
  state: HomeActions,
  kind: LaunchKind,
  target: ContainerRef,
  newRequestId: NewRequestId,
  now: number,
): ActionResult {
  const key = containerKey(target);
  if (state.launches.some((launch) => containerKey(launch.target) === key)) {
    return { message: null, state };
  }
  const launch: PendingLaunch = {
    kind,
    requestId: newRequestId(),
    target,
    sentAt: now,
    lastSentAt: now,
  };
  const errors = launchErrorMaps(state);
  errors[kind].delete(key);
  return {
    message: launchMessage(launch),
    state: { ...withLaunchErrorMaps(state, errors), launches: [...state.launches, launch] },
  };
}

/** The message a pending launch sends, first and on every resend, under its request id. */
export function launchMessage(launch: PendingLaunch): ClientMessage {
  if (launch.kind === "new_session") {
    return {
      type: "new_session",
      target: launch.target,
      initial_prompt: NEW_SESSION_PROMPT,
      request_id: launch.requestId,
    };
  }
  return { type: "launch_last", target: launch.target, request_id: launch.requestId };
}

/** Writable copies of the two refusal maps, by the launch kind each one's text is for. */
type LaunchErrorMaps = Record<LaunchKind, Map<string, string>>;

function launchErrorMaps(state: HomeActions): LaunchErrorMaps {
  return {
    launch_last: new Map(state.launchErrors),
    new_session: new Map(state.newSessionErrors),
  };
}

function withLaunchErrorMaps(state: HomeActions, errors: LaunchErrorMaps): HomeActions {
  return { ...state, launchErrors: errors.launch_last, newSessionErrors: errors.new_session };
}

/**
 * The state a screen starts from on every `ready`: the requests on their way
 * are gone, but a pending launch still waits for an answer and is resent.
 */
export function keepLaunches(old: HomeActions): HomeActions {
  return { ...NO_ACTIONS, launches: old.launches, presetLaunches: old.presetLaunches };
}

/**
 * Launch a preset, or nothing while its earlier launch from this container
 * still waits for its job's first update. The phone mints the job id, and
 * records the launch before the send, so a tap that reaches no daemon is
 * resent on the next reconnect.
 */
export function launchPreset(
  state: HomeActions,
  request: PresetLaunchRequest,
  newRequestId: NewRequestId,
  now: number,
): ActionResult {
  const key = presetKey(request.target, request.presetId);
  if (state.presetLaunches.some((launch) => presetKey(launch.target, launch.presetId) === key)) {
    return { message: null, state };
  }
  const launch: PendingPresetLaunch = {
    ...request,
    jobId: `phone-${newRequestId()}`,
    sentAt: now,
    lastSentAt: now,
  };
  return {
    message: presetLaunchMessage(launch),
    state: {
      ...state,
      presetLaunches: [...state.presetLaunches, launch],
      presetErrors: withoutLaunchError(state.presetErrors, key),
    },
  };
}

/**
 * The `launch_preset` for a launch under its job id: no variable values, the
 * preset's own worktree and tab settings, and no client id, so its tabs land
 * in the daemon's default layout.
 */
export function presetLaunchMessage(
  launch: PresetLaunchRequest & { readonly jobId: string },
): ClientMessage {
  return {
    type: "launch_preset",
    job_id: launch.jobId,
    target: presetTarget(launch.target),
    preset_id: launch.presetId,
    source: launch.source,
    variable_values: [],
    use_worktree_override: null,
    max_panes_per_tab_override: null,
  };
}

/**
 * The messages a reconnect sends for the preset launches still waiting. A
 * host listing `preset_job_repeat` gets each one again with its job id, so
 * its journal starts one job: a launch older than the journal is dropped
 * instead. Any other host gets nothing, and each preset says why.
 */
export function resendPresetLaunches(
  state: HomeActions,
  supportsRepeat: boolean,
  now: number,
): LaunchResend {
  if (state.presetLaunches.length === 0) {
    return { messages: [], state };
  }
  const presetErrors = new Map(state.presetErrors);
  const messages: ClientMessage[] = [];
  const presetLaunches: PendingPresetLaunch[] = [];
  for (const launch of state.presetLaunches) {
    const key = presetKey(launch.target, launch.presetId);
    if (!supportsRepeat) {
      presetErrors.set(key, UNSUPPORTED_PRESET_RESEND);
    } else if (now - launch.sentAt >= LAUNCH_RESEND_LIMIT_MS) {
      presetErrors.set(key, LAUNCH_MAYBE_STARTED);
    } else {
      const resent = { ...launch, lastSentAt: now };
      presetLaunches.push(resent);
      messages.push(presetLaunchMessage(resent));
    }
  }
  return { messages, state: { ...state, presetLaunches, presetErrors } };
}

/**
 * Drops the preset launches whose job never answered, a minute after their
 * latest send, each preset saying it may have started sessions anyway;
 * returns the state itself when every launch is still waiting.
 */
export function expirePresetLaunches(state: HomeActions, now: number): HomeActions {
  if (state.presetLaunches.length === 0) {
    return state;
  }
  const presetErrors = new Map(state.presetErrors);
  const presetLaunches: PendingPresetLaunch[] = [];
  for (const launch of state.presetLaunches) {
    if (now - launch.lastSentAt >= LAUNCH_ANSWER_LIMIT_MS) {
      presetErrors.set(presetKey(launch.target, launch.presetId), LAUNCH_MAYBE_STARTED);
    } else {
      presetLaunches.push(launch);
    }
  }
  return presetLaunches.length === state.presetLaunches.length
    ? state
    : { ...state, presetLaunches, presetErrors };
}

/** The state once a job has answered: its pending launch is gone; the state itself for any other job. */
function answerPresetLaunch(state: HomeActions, jobId: string | undefined): FoldResult {
  const presetLaunches = state.presetLaunches.filter((launch) => launch.jobId !== jobId);
  return presetLaunches.length === state.presetLaunches.length
    ? { state, effects: [] }
    : { state: { ...state, presetLaunches }, effects: [] };
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
 * since a resend would make a second session. A host that does not list a
 * launch's kind gets nothing for it, the launch is dropped, and its
 * container shows why.
 */
export function resendLaunches(
  state: HomeActions,
  supports: LaunchSupport,
  now: number,
): LaunchResend {
  if (state.launches.length === 0) {
    return { messages: [], state };
  }
  const errors = launchErrorMaps(state);
  const messages: ClientMessage[] = [];
  const launches: PendingLaunch[] = [];
  for (const launch of state.launches) {
    const key = containerKey(launch.target);
    if (!supports(launch.kind)) {
      errors[launch.kind].set(key, UNSUPPORTED_LAUNCH);
    } else if (now - launch.sentAt >= LAUNCH_RESEND_LIMIT_MS) {
      errors[launch.kind].set(key, LAUNCH_MAYBE_STARTED);
    } else {
      const resent = { ...launch, lastSentAt: now };
      launches.push(resent);
      messages.push(launchMessage(resent));
    }
  }
  return { messages, state: { ...withLaunchErrorMaps(state, errors), launches } };
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
  const errors = launchErrorMaps(state);
  const launches: PendingLaunch[] = [];
  for (const launch of state.launches) {
    if (now - launch.lastSentAt >= LAUNCH_ANSWER_LIMIT_MS) {
      errors[launch.kind].set(containerKey(launch.target), LAUNCH_MAYBE_STARTED);
    } else {
      launches.push(launch);
    }
  }
  return launches.length === state.launches.length
    ? state
    : { ...withLaunchErrorMaps(state, errors), launches };
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
    case "preset_launch_job_updated":
      return answerPresetLaunch(state, message.job.job_id);
    case "preset_launch_failed":
      return answerPresetLaunch(state, message.job_id);
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
  // A Launch last's refusal shows under its group header, a New session's under its picker row.
  const errors = launchErrorMaps(state);
  errors[launch.kind].set(containerKey(launch.target), text);
  return {
    state: { ...withLaunchErrorMaps(state, errors), launches: withoutLaunch(state, requestId) },
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

/**
 * Drops a preset's dropped-launch note once its job has answered, so the
 * job's own outcome shows; `key` null, or a key with no note, returns the state itself.
 */
export function forgetPresetError(state: HomeActions, key: string | null): HomeActions {
  if (key === null || !state.presetErrors.has(key)) {
    return state;
  }
  return { ...state, presetErrors: withoutLaunchError(state.presetErrors, key) };
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
