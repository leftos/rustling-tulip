import type { NewRequestId } from "./home-actions";
import { keyInput } from "./keys";
import type { ClientMessage, DaemonMessage } from "./protocol";
import { quickReplyBytes } from "./quick";

/**
 * How long a send stays out at most when nothing settles it sooner: the
 * desktop's `SEND_BACKSTOP` (`apps/native/src/chat.rs`).
 */
export const SEND_BACKSTOP = 10_000;

/** One `chat_send`: its request id, its text as sent, and when it went out. */
export interface Send {
  readonly requestId: string;
  readonly text: string;
  /** The clock reading, in milliseconds, when the send went out. */
  readonly at: number;
}

/**
 * Where a chat composer's draft and its last send stand: what the box holds,
 * the send still out, the last send that settled, and the daemon's reason for
 * refusing one of them.
 */
export interface ComposerModel {
  /** The box's text, as typed. */
  readonly draft: string;
  /** The send still out; null when none is. */
  readonly sending: Send | null;
  /** The last send that settled, kept so a refusal arriving after its entry still gives its text back. */
  readonly settled: Send | null;
  /** The daemon's refusal, verbatim; null when the last send was not refused. */
  readonly error: string | null;
}

/** The model before anything was typed or sent. */
export const INITIAL_COMPOSER: ComposerModel = {
  draft: "",
  sending: null,
  settled: null,
  error: null,
};

/** What a send produces: the message to send, if any, and the model after it. */
export interface Submitted {
  /** The message to send; null when the tap sends nothing. */
  readonly message: ClientMessage | null;
  readonly state: ComposerModel;
}

type ChatAppend = Extract<DaemonMessage, { type: "chat_append" }>;

/**
 * The draft `text` sets, and no error: typing again answers the last
 * refusal. Pure; an edit that changes nothing returns the model itself.
 */
export function edit(state: ComposerModel, text: string): ComposerModel {
  if (state.draft === text && state.error === null) {
    return state;
  }
  return { ...state, draft: text, error: null };
}

/**
 * Send the draft to `sessionId`: the `chat_send` carrying the text as typed,
 * under a fresh request id, and the model marked sending from `now` until the
 * send settles. A blank draft, or one whose send is still out, sends nothing.
 * The draft stays until the send settles or the daemon refuses it.
 */
export function submit(
  state: ComposerModel,
  sessionId: string,
  newRequestId: NewRequestId,
  now: number,
): Submitted {
  if (state.sending !== null || state.draft.trim() === "") {
    return { message: null, state };
  }
  const requestId = newRequestId();
  return {
    message: { type: "chat_send", session_id: sessionId, text: state.draft, request_id: requestId },
    state: {
      ...state,
      sending: { requestId, text: state.draft, at: now },
      error: null,
    },
  };
}

/** The `send_input` a Quick reply types into `sessionId`: its text, then Enter. */
export function quick(sessionId: string, text: string): ClientMessage {
  return keyInput(sessionId, quickReplyBytes(text));
}

/** The `send_input` the Keys tray types when `bytes` is tapped. */
export function key(sessionId: string, bytes: string): ClientMessage {
  return keyInput(sessionId, bytes);
}

/**
 * One daemon message folded into the composer's send: a refusal echoing a
 * send's request id ends it and shows the daemon's text verbatim, giving the
 * text back when it was the last settled send and the box is empty; the sent
 * text arriving as a pending user row in `sessionId`'s chat settles the send.
 * Pure; a message the composer does not read returns the model itself.
 */
export function foldReply(
  state: ComposerModel,
  sessionId: string,
  message: DaemonMessage,
): ComposerModel {
  switch (message.type) {
    case "error":
      return refuse(state, message.request_id ?? null, message.message);
    case "chat_append":
      return onAppend(state, sessionId, message);
    default:
      return state;
  }
}

/**
 * A send whose backstop has passed is out no more: it settles, so a refusal
 * that arrives later still gives its text back. Pure; a send with time left
 * returns the model itself.
 */
export function tick(state: ComposerModel, now: number): ComposerModel {
  const sending = state.sending;
  if (sending === null || now < sending.at + SEND_BACKSTOP) {
    return state;
  }
  return { ...state, sending: null, settled: sending };
}

/**
 * The model after the connection dropped a send before it left: the send is
 * out no more, the draft stays, and `text` says why. Pure; another send's
 * request id returns the model itself.
 */
export function unsent(state: ComposerModel, requestId: string, text: string): ComposerModel {
  const sending = state.sending;
  if (sending === null || sending.requestId !== requestId) {
    return state;
  }
  return { ...state, sending: null, error: text };
}

/**
 * The model a new connection starts from (the screen calls it on every
 * ready): nothing is out, so neither the send nor its settled record nor the
 * refusal can be answered any more; the draft stays.
 */
export function reset(state: ComposerModel): ComposerModel {
  return { ...INITIAL_COMPOSER, draft: state.draft };
}

/** The refusal of the send still out, or of the last settled one. */
function refuse(state: ComposerModel, requestId: string | null, text: string): ComposerModel {
  if (requestId === null) {
    return state;
  }
  const sending = state.sending;
  if (sending !== null && sending.requestId === requestId) {
    return { ...state, sending: null, error: text };
  }
  const settled = state.settled;
  if (settled !== null && settled.requestId === requestId) {
    return {
      ...state,
      settled: null,
      draft: state.draft === "" ? settled.text : state.draft,
      error: text,
    };
  }
  return state;
}

/** The sent text arriving as the pending user row the send made. */
function onAppend(state: ComposerModel, sessionId: string, message: ChatAppend): ComposerModel {
  const sending = state.sending;
  if (sending === null || message.session_id !== sessionId) {
    return state;
  }
  const ours = sentText(sending.text);
  const arrived = (message.entries ?? []).some(
    (entry) => entry.kind === "user" && entry.pending === true && sentText(entry.text) === ours,
  );
  if (!arrived) {
    return state;
  }
  const untouched = state.draft === sending.text;
  return {
    ...state,
    draft: untouched ? "" : state.draft,
    sending: null,
    settled: sending,
  };
}

/** `text` as the desktop compares a send with its row: CRLF folded, ends trimmed. */
function sentText(text: string): string {
  return text.replace(/\r\n/g, "\n").trim();
}
