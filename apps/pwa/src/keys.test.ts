import { expect, test } from "vitest";

import { KEY_TRAY, NAMED_KEY_BYTES, ctrl, keyInput, type NamedKey } from "./keys";
import type { ClientMessage } from "./protocol";

const NAMED: readonly (readonly [NamedKey, string])[] = [
  ["Escape", "\x1b"],
  ["Tab", "\t"],
  ["ShiftTab", "\x1b[Z"],
  ["Enter", "\r"],
  ["Backspace", "\x7f"],
  ["Up", "\x1b[A"],
  ["Down", "\x1b[B"],
  ["Right", "\x1b[C"],
  ["Left", "\x1b[D"],
  ["Home", "\x1b[H"],
  ["End", "\x1b[F"],
  ["PageUp", "\x1b[5~"],
  ["PageDown", "\x1b[6~"],
  ["Delete", "\x1b[3~"],
  ["Space", " "],
];

function sentBase64(message: ClientMessage): string {
  if (message.type !== "send_input") {
    throw new Error(`expected send_input, saw ${message.type}`);
  }
  return message.data_b64;
}

function decode(dataB64: string): string {
  const binary = atob(dataB64);
  return new TextDecoder().decode(Uint8Array.from(binary, (c) => c.charCodeAt(0)));
}

test("keys_named_keys_map_to_their_terminal_bytes", () => {
  for (const [name, bytes] of NAMED) {
    expect(NAMED_KEY_BYTES[name]).toBe(bytes);
  }
  expect(Object.keys(NAMED_KEY_BYTES).sort()).toEqual(NAMED.map(([name]) => name).sort());
});

test("keys_ctrl_sends_the_control_byte", () => {
  expect(ctrl("c")).toBe("\x03");
  expect(ctrl("a")).toBe("\x01");
  expect(ctrl("z")).toBe("\x1a");
});

test("keys_ctrl_refuses_anything_but_a_lowercase_letter", () => {
  for (const input of ["1", "A", "", "ctrl"]) {
    expect(() => ctrl(input)).toThrow();
  }
});

test("keys_key_input_carries_utf8_base64_for_the_session", () => {
  const arrow = keyInput("session-1", "\x1b[A");

  expect(arrow).toEqual({ type: "send_input", session_id: "session-1", data_b64: "G1tB" });
  expect(decode(sentBase64(arrow))).toBe("\x1b[A");

  const accented = keyInput("session-1", "é");
  expect(decode(sentBase64(accented))).toBe("é");
});

test("keys_tray_is_the_core_strip_then_the_digits", () => {
  expect(KEY_TRAY).toEqual([
    { label: "Esc", bytes: "\x1b" },
    { label: "Tab", bytes: "\t" },
    { label: "Shift+Tab", bytes: "\x1b[Z" },
    { label: "↑", bytes: "\x1b[A" },
    { label: "↓", bytes: "\x1b[B" },
    { label: "Enter", bytes: "\r" },
    { label: "Ctrl+C", bytes: "\x03" },
    { label: "1", bytes: "1" },
    { label: "2", bytes: "2" },
    { label: "3", bytes: "3" },
    { label: "4", bytes: "4" },
    { label: "5", bytes: "5" },
    { label: "6", bytes: "6" },
    { label: "7", bytes: "7" },
    { label: "8", bytes: "8" },
    { label: "9", bytes: "9" },
  ]);
});
