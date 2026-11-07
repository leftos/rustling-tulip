import { ctrl, keyInput, type KeyTrayEntry } from "./keys";
import type { ClientMessage } from "./protocol";
import { quickReplyBytes } from "./quick";

/** Whether the key bar's Ctrl is armed for the next key. */
export interface KeyBarState {
  readonly ctrl: boolean;
}

/** The key bar before anything is armed. */
export const INITIAL_KEY_BAR: KeyBarState = { ctrl: false };

/** One key bar action: the bar after it, and what it types. */
export interface KeyTransition {
  readonly state: KeyBarState;
  readonly message: ClientMessage | null;
}

/** A tap on the Ctrl button: arm the next key, or disarm when it is armed. */
export function toggleCtrl(state: KeyBarState): KeyBarState {
  return { ctrl: !state.ctrl };
}

/**
 * A tap on a Keys tray button: its bytes, or the Ctrl chord of a letter
 * button while Ctrl is armed. Either way the tap consumes the armed Ctrl,
 * since a key without a chord of its own goes out as itself.
 */
export function trayKey(state: KeyBarState, sessionId: string, entry: KeyTrayEntry): KeyTransition {
  if (!state.ctrl) {
    return { state, message: keyInput(sessionId, entry.bytes) };
  }
  return { state: INITIAL_KEY_BAR, message: keyInput(sessionId, chord(entry.bytes)) };
}

/**
 * Text the terminal reports through `onData`: sent unchanged, unless Ctrl is
 * armed, where its first letter becomes the chord and the rest follows it. An
 * empty string sends nothing and leaves Ctrl as it was.
 */
export function typedKey(state: KeyBarState, sessionId: string, text: string): KeyTransition {
  if (text === "") {
    return { state, message: null };
  }
  if (!state.ctrl) {
    return { state, message: keyInput(sessionId, text) };
  }
  return { state: INITIAL_KEY_BAR, message: keyInput(sessionId, chord(text)) };
}

/**
 * A reply the terminal itself sends while it replays history (its answer to
 * the child's queries, such as `ESC [ ? 1 ; 2 c`): sent as it is, marked
 * automatic so it can never answer a hook prompt, and never a key: it does
 * not chord, does not disarm Ctrl, and is not typing, so the view must not
 * count it toward the size claim. An empty string sends nothing.
 */
export function replyKey(state: KeyBarState, sessionId: string, text: string): KeyTransition {
  if (text === "") {
    return { state, message: null };
  }
  const message = keyInput(sessionId, text);
  return {
    state,
    message: message.type === "send_input" ? { ...message, automatic: true } : message,
  };
}

/**
 * A Quick reply: its text and Enter, as the session would read a typed
 * answer. It is not a key, so an armed Ctrl stays armed for the next one.
 */
export function quickKey(state: KeyBarState, sessionId: string, text: string): KeyTransition {
  return { state, message: keyInput(sessionId, quickReplyBytes(text)) };
}

/** `bytes` with Ctrl applied to its first character when that is one letter. */
function chord(bytes: string): string {
  const first = bytes.charAt(0);
  return /^[A-Za-z]$/.test(first) ? ctrl(first.toLowerCase()) + bytes.slice(1) : bytes;
}
