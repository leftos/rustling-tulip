import type { ChatEntry, ChatPrompt, ChatScreen, ClientMessage, DaemonMessage } from "./protocol";

/** Where one session's chat stands on this connection. */
export type ChatPhase = "idle" | "loading" | "loaded" | "refused";

/** The rows a chat keeps; older rows past it are counted into `hidden` (the chat log's display cap). */
export const DISPLAY_CAP = 500;

/**
 * One session's chat as the phone shows it: the rows the mirror delivered,
 * the rows the display cap dropped, the composer and dialog state, and the
 * load whose reply is still awaited.
 */
export interface ChatState {
  /** The session entered; `null` before one is and after leaving. */
  readonly sessionId: string | null;
  readonly phase: ChatPhase;
  readonly entries: ChatEntry[];
  /** How many rows the display cap dropped in front of `entries`. */
  readonly hidden: number;
  readonly screen: ChatScreen | null;
  /** The daemon's reason for refusing the load, verbatim; `null` otherwise. */
  readonly notice: string | null;
  /** The `load_chat` request whose reply is awaited. */
  readonly requestId: string | null;
  /** The ids that left since the last load; a later append carrying one is ignored. */
  readonly gone: ReadonlySet<string>;
}

export const INITIAL_CHAT: ChatState = {
  sessionId: null,
  phase: "idle",
  entries: [],
  hidden: 0,
  screen: null,
  notice: null,
  requestId: null,
  gone: new Set<string>(),
};

export type ChatEvent =
  /** The user opened a session's screen. */
  | { readonly type: "enter"; readonly sessionId: string }
  /** The user left the screen: the mirror stops loading the session. */
  | { readonly type: "leave" }
  /** The page went to the background, so nothing shows the chat. */
  | { readonly type: "hidden" }
  /** The connection became ready, so the daemon's state must be asked for again. */
  | { readonly type: "ready" }
  /** A daemon message the connection delivered. */
  | { readonly type: "message"; readonly message: DaemonMessage };

/** One event applied to the model: the state after it, and what to send. */
export interface ChatTransition {
  readonly state: ChatState;
  /** The messages to send, in order, as the event asks for them. */
  readonly effects: readonly ClientMessage[];
}

type ChatReply = Extract<DaemonMessage, { type: "chat" }>;
type ChatAppend = Extract<DaemonMessage, { type: "chat_append" }>;
type ChatScreenChanged = Extract<DaemonMessage, { type: "chat_screen_changed" }>;
type ErrorReply = Extract<DaemonMessage, { type: "error" }>;

/**
 * One event applied to one session's chat. Pure; `nextRequestId` supplies each
 * `load_chat` request id, so the caller decides where ids come from.
 */
export function chatStep(
  state: ChatState,
  event: ChatEvent,
  nextRequestId: () => string,
): ChatTransition {
  switch (event.type) {
    case "enter":
      return onEnter(state, event.sessionId, nextRequestId);
    case "leave":
    case "hidden":
      return onLeave(state);
    case "ready":
      return onReady(state, nextRequestId);
    case "message":
      return onMessage(state, event.message);
  }
}

/** The prompt card the session is waiting on, for the sheet; `null` when none is up. */
export function latestPrompt(state: ChatState): ChatPrompt | null {
  return state.screen?.prompt ?? null;
}

/** Whether the message's `sessionId` is the session this screen entered. */
function sameSession(state: ChatState, sessionId: string): boolean {
  return state.sessionId !== null && state.sessionId === sessionId;
}

/** The `unload_chat` a loaded or loading session needs before it is dropped. */
function unload(state: ChatState): ClientMessage[] {
  const sessionId = state.sessionId;
  if (sessionId === null || (state.phase !== "loading" && state.phase !== "loaded")) {
    return [];
  }
  return [{ type: "unload_chat", session_id: sessionId }];
}

function onEnter(state: ChatState, sessionId: string, nextRequestId: () => string): ChatTransition {
  // A screen that asks again for the session it already loads keeps that load;
  // one whose load was refused asks again.
  if (state.sessionId === sessionId && (state.phase === "loading" || state.phase === "loaded")) {
    return { state, effects: [] };
  }
  const requestId = nextRequestId();
  return {
    state: { ...INITIAL_CHAT, sessionId, phase: "loading", requestId },
    effects: [
      ...unload(state),
      { type: "load_chat", session_id: sessionId, request_id: requestId },
    ],
  };
}

function onLeave(state: ChatState): ChatTransition {
  if (state.sessionId === null) {
    return { state, effects: [] };
  }
  return { state: INITIAL_CHAT, effects: unload(state) };
}

function onReady(state: ChatState, nextRequestId: () => string): ChatTransition {
  const sessionId = state.sessionId;
  if (sessionId === null) {
    return { state, effects: [] };
  }
  // A reconnect: the daemon resends its state only to a fresh load, and the
  // rows on screen stay until that reply replaces them.
  const requestId = nextRequestId();
  return {
    state: { ...state, phase: "loading", notice: null, requestId },
    effects: [{ type: "load_chat", session_id: sessionId, request_id: requestId }],
  };
}

function onMessage(state: ChatState, message: DaemonMessage): ChatTransition {
  switch (message.type) {
    case "chat":
      return onChat(state, message);
    case "chat_append":
      return onAppend(state, message);
    case "chat_screen_changed":
      return onScreenChanged(state, message);
    case "error":
      return onError(state, message);
    default:
      return { state, effects: [] };
  }
}

function onChat(state: ChatState, message: ChatReply): ChatTransition {
  if (!sameSession(state, message.session_id)) {
    return { state, effects: [] };
  }
  const replied = message.request_id ?? null;
  // A reply to an older load says nothing about the one awaited now.
  if (replied !== null && replied !== state.requestId) {
    return { state, effects: [] };
  }
  // A load sent unasked (no request id) replaces the rows, and leaves a load
  // still awaited in place for its own reply.
  const settles = replied !== null || state.requestId === null;
  // A load reply replaces the rows and forgets which ids left before it.
  const gone = new Set<string>();
  const trimmed = trim((message.entries ?? []).filter(knownEntry), message.hidden ?? 0, gone);
  return {
    state: {
      ...state,
      phase: settles ? "loaded" : state.phase,
      entries: trimmed.entries,
      hidden: trimmed.hidden,
      screen: message.screen,
      notice: null,
      requestId: settles ? null : state.requestId,
      gone,
    },
    effects: [],
  };
}

function onAppend(state: ChatState, message: ChatAppend): ChatTransition {
  if (!sameSession(state, message.session_id)) {
    return { state, effects: [] };
  }
  const removed = new Set(message.removed ?? []);
  const added = (message.entries ?? []).filter(knownEntry);
  const gone = new Set(state.gone);
  for (const id of removed) {
    gone.add(id);
  }
  // An id this same append removes and carries comes back at the bottom.
  for (const entry of added) {
    if (removed.has(entry.id)) {
      gone.delete(entry.id);
    }
  }
  const kept = state.entries.filter((entry) => !removed.has(entry.id));
  const rows = upsert(
    kept,
    added.filter((entry) => !gone.has(entry.id)),
  );
  const trimmed = trim(rows, state.hidden, gone);
  return {
    state: { ...state, entries: trimmed.entries, hidden: trimmed.hidden, gone },
    effects: [],
  };
}

function onScreenChanged(state: ChatState, message: ChatScreenChanged): ChatTransition {
  if (!sameSession(state, message.session_id)) {
    return { state, effects: [] };
  }
  return { state: { ...state, screen: message.screen }, effects: [] };
}

function onError(state: ChatState, message: ErrorReply): ChatTransition {
  const requestId = state.requestId;
  if (requestId === null || message.request_id !== requestId) {
    return { state, effects: [] };
  }
  // The mirror refused this session, so nothing it holds is shown again.
  return {
    state: {
      ...INITIAL_CHAT,
      sessionId: state.sessionId,
      phase: "refused",
      notice: message.message,
    },
    effects: [],
  };
}

/**
 * Whether `entry` is a row kind this build draws. A row of any other kind is
 * one the daemon knows and this build does not, and is dropped.
 */
function knownEntry(entry: ChatEntry): boolean {
  switch (entry.kind) {
    case "user":
    case "answer":
    case "thinking":
    case "tool":
    case "system":
    case "receipt":
    case "turn_end":
      return true;
    default:
      return false;
  }
}

/** Applies appended rows: a known id replaces its row in place, a new one is pushed. */
function upsert(entries: readonly ChatEntry[], added: readonly ChatEntry[]): ChatEntry[] {
  const next = [...entries];
  for (const entry of added) {
    const at = next.findIndex((known) => known.id === entry.id);
    if (at === -1) {
      next.push(entry);
    } else {
      next[at] = entry;
    }
  }
  return next;
}

/**
 * Drops the oldest rows past the display cap, counting them into `hidden` and
 * forgetting their ids so a later append cannot bring them back.
 */
function trim(
  rows: ChatEntry[],
  hidden: number,
  gone: Set<string>,
): { entries: ChatEntry[]; hidden: number } {
  const excess = rows.length - DISPLAY_CAP;
  if (excess <= 0) {
    return { entries: rows, hidden };
  }
  for (const entry of rows.slice(0, excess)) {
    gone.add(entry.id);
  }
  return { entries: rows.slice(excess), hidden: hidden + excess };
}
