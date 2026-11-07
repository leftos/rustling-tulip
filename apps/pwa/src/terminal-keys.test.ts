import { expect, test } from "vitest";

import { KEY_TRAY, type KeyTrayEntry } from "./keys";
import type { ClientMessage } from "./protocol";
import {
  INITIAL_KEY_BAR,
  isTerminalReply,
  quickKey,
  replyKey,
  toggleCtrl,
  trayKey,
  typedKey,
  type KeyBarState,
} from "./terminal-keys";

/** The text a transition's `send_input` types. */
function typedText(message: ClientMessage | null): string {
  if (message === null || message.type !== "send_input") {
    throw new Error(`expected send_input, saw ${message === null ? "none" : message.type}`);
  }
  const binary = atob(message.data_b64);
  return new TextDecoder().decode(Uint8Array.from(binary, (c) => c.charCodeAt(0)));
}

/** A tray button carrying one letter, which the tray ships no example of. */
const LETTER_C: KeyTrayEntry = { label: "c", bytes: "c" };
const UP: KeyTrayEntry = { label: "↑", bytes: "\x1b[A" };

function armed(): KeyBarState {
  return toggleCtrl(INITIAL_KEY_BAR);
}

test("keys_a_ctrl_tap_arms_and_the_next_key_takes_the_chord", () => {
  const sent = trayKey(armed(), "s1", LETTER_C);
  expect(typedText(sent.message)).toBe("\x03");
  expect(sent.state.ctrl).toBe(false);
});

test("keys_a_second_ctrl_tap_disarms", () => {
  const off = toggleCtrl(armed());
  expect(off.ctrl).toBe(false);
  expect(typedText(trayKey(off, "s1", LETTER_C).message)).toBe("c");
});

test("keys_typed_text_passes_through_when_ctrl_is_not_armed", () => {
  const sent = typedKey(INITIAL_KEY_BAR, "s1", "hello\r");
  expect(typedText(sent.message)).toBe("hello\r");
  expect(sent.state.ctrl).toBe(false);
});

test("keys_typed_text_takes_the_chord_on_its_first_letter", () => {
  const sent = typedKey(armed(), "s1", "cabc");
  expect(typedText(sent.message)).toBe("\x03abc");
  expect(sent.state.ctrl).toBe(false);

  const upper = typedKey(armed(), "s1", "C");
  expect(typedText(upper.message)).toBe("\x03");

  const next = typedKey(sent.state, "s1", "d");
  expect(typedText(next.message)).toBe("d");
});

test("keys_tray_keys_send_their_own_bytes", () => {
  for (const entry of KEY_TRAY) {
    const sent = trayKey(INITIAL_KEY_BAR, "s1", entry);
    expect(typedText(sent.message)).toBe(entry.bytes);
    expect(sent.state.ctrl).toBe(false);
  }
});

test("keys_a_key_without_a_chord_passes_through_and_disarms", () => {
  const sent = trayKey(armed(), "s1", UP);
  expect(typedText(sent.message)).toBe("\x1b[A");
  expect(sent.state.ctrl).toBe(false);

  const digit = typedKey(armed(), "s1", "1");
  expect(typedText(digit.message)).toBe("1");
  expect(digit.state.ctrl).toBe(false);
});

test("keys_an_empty_typed_string_sends_nothing_and_keeps_ctrl", () => {
  const sent = typedKey(armed(), "s1", "");
  expect(sent.message).toBeNull();
  expect(sent.state.ctrl).toBe(true);
});

test("keys_a_quick_reply_types_its_text_and_enter", () => {
  const sent = quickKey(INITIAL_KEY_BAR, "s1", "yes");
  expect(typedText(sent.message)).toBe("yes\r");
  expect(sent.state).toEqual(INITIAL_KEY_BAR);
});

test("keys_a_replay_reply_is_sent_as_it_is_marked_automatic_and_keeps_ctrl", () => {
  const reply = "\x1b[?1;2c";
  const sent = replyKey(armed(), "s1", reply);
  expect(typedText(sent.message)).toBe(reply);
  expect(sent.state.ctrl).toBe(true);
  expect(sent.message?.type === "send_input" ? sent.message.automatic : false).toBe(true);

  expect(replyKey(INITIAL_KEY_BAR, "s1", "").message).toBeNull();
});

test("keys_the_terminal_answering_the_child_is_a_reply", () => {
  const replies = [
    "\x1b[?1;2c",
    "\x1b[?62;22c",
    "\x1b[>0;276;0c",
    "\x1b[12;40R",
    "\x1b[0n",
    "\x1b[?1;2$y",
    "\x1b[2004;1$y",
    "\x1b[8;24;80t",
    "\x1b]11;rgb:1e1e/1e1e/2e2e\x07",
    "\x1b]10;rgb:cdcd/d6d6/f4f4\x1b\\",
    "\x1bP1$r0m\x1b\\",
    "\x1b[I",
    "\x1b[O",
  ];
  for (const reply of replies) {
    expect(isTerminalReply(reply), JSON.stringify(reply)).toBe(true);
  }
});

test("keys_keys_the_user_pressed_are_not_a_reply", () => {
  const keys = [
    "\x1b[A",
    "\x1b[B",
    "\x1b[C",
    "\x1b[D",
    "\x1bOA",
    "\x1b[1;5A",
    "\x1b[3~",
    "\x1b[15~",
    "\x1b[Z",
    "\x1b",
    "\r",
    "\t",
    "\x7f",
    "a",
    "Q",
    "pasted text\nwith two lines",
  ];
  for (const key of keys) {
    expect(isTerminalReply(key), JSON.stringify(key)).toBe(false);
  }
});
