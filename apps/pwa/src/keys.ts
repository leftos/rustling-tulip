import type { ClientMessage } from "./protocol";

/**
 * The bytes each named key sends: the sequences a terminal reads. `ShiftTab`
 * is the `ESC [ Z` a terminal reports for Shift+Tab; `Backspace` is DEL, which
 * a terminal reads as erase.
 */
export const NAMED_KEY_BYTES = {
  Escape: "\x1b",
  Tab: "\t",
  ShiftTab: "\x1b[Z",
  Enter: "\r",
  Backspace: "\x7f",
  Up: "\x1b[A",
  Down: "\x1b[B",
  Right: "\x1b[C",
  Left: "\x1b[D",
  Home: "\x1b[H",
  End: "\x1b[F",
  PageUp: "\x1b[5~",
  PageDown: "\x1b[6~",
  Delete: "\x1b[3~",
  Space: " ",
} as const;

/** A key {@link NAMED_KEY_BYTES} names. */
export type NamedKey = keyof typeof NAMED_KEY_BYTES;

/** The digit keys `0` to `9`, as themselves. */
export const DIGITS: readonly string[] = [..."0123456789"];

/**
 * The byte a Ctrl chord sends: `ctrl("c")` is `\x03`, the interrupt. Only a
 * lowercase letter from `a` to `z` names a control byte; anything else throws.
 */
export function ctrl(letter: string): string {
  if (!/^[a-z]$/.test(letter)) {
    throw new Error(`ctrl expects one lowercase letter a-z, got ${JSON.stringify(letter)}`);
  }
  return String.fromCharCode(letter.charCodeAt(0) - 0x60);
}

/**
 * The `SendInput` message that types `bytes` into `sessionId`: the bytes as
 * UTF-8, then base64, which is what the wire carries.
 */
export function keyInput(sessionId: string, bytes: string): ClientMessage {
  return { type: "send_input", session_id: sessionId, data_b64: base64(bytes) };
}

/** `bytes` as base64, through a binary string so it works in Safari and in node. */
function base64(bytes: string): string {
  let binary = "";
  for (const byte of new TextEncoder().encode(bytes)) {
    binary += String.fromCharCode(byte);
  }
  return btoa(binary);
}

/** One button of the Keys tray: the name on it, and what a tap sends. */
export interface KeyTrayEntry {
  readonly label: string;
  readonly bytes: string;
}

/**
 * The Keys tray's strip, in collie's order (its `key-board.ts` `CORE_KEYS`
 * and `NAMED_KEYS`): Esc, Tab, Shift+Tab, the two arrows, Enter and Ctrl+C,
 * then the digits 1 to 9.
 */
export const KEY_TRAY: readonly KeyTrayEntry[] = [
  { label: "Esc", bytes: NAMED_KEY_BYTES.Escape },
  { label: "Tab", bytes: NAMED_KEY_BYTES.Tab },
  { label: "Shift+Tab", bytes: NAMED_KEY_BYTES.ShiftTab },
  { label: "↑", bytes: NAMED_KEY_BYTES.Up },
  { label: "↓", bytes: NAMED_KEY_BYTES.Down },
  { label: "Enter", bytes: NAMED_KEY_BYTES.Enter },
  { label: "Ctrl+C", bytes: ctrl("c") },
  ...DIGITS.slice(1).map((digit) => ({ label: digit, bytes: digit })),
];
