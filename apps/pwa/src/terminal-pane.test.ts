import { expect, test } from "vitest";

import type { ConnectionStateName } from "./connection-machine";
import { ctrl, keyInput } from "./keys";
import type { ClientMessage, DaemonMessage } from "./protocol";
import { quickReplyBytes } from "./quick";
import type { ConnectionNotice } from "./socket";
import { REQUEST_TIMEOUT, RETRY_DELAYS } from "./terminal-model";
import { createTerminalPane, type TerminalLike, type TerminalPane } from "./terminal-pane";

const T0 = 1_000;

/** One call the stub terminal saw, in order. */
type TerminalCall = { readonly kind: "reset" } | { readonly kind: "write"; readonly text: string };

interface Harness {
  readonly pane: TerminalPane;
  /** Every `reset` and `write` the terminal saw, in order. */
  readonly log: TerminalCall[];
  /** Every message the pane sent, in order. */
  readonly sent: ClientMessage[];
  /** Runs every write's `done` still held, in order. */
  finishWrites(): void;
  /** Sets the clock to `ms`. */
  at(ms: number): void;
  /** Sets the connection's state, which `send` answers by as `Connection.send` does. */
  connection(state: ConnectionStateName): void;
  /** The messages sent since the last call, which drains them from view. */
  fresh(): ClientMessage[];
}

/** A pane over a stub terminal, a recording `send`, a mutable clock and ids `r1`, `r2`, …. */
function harness(): Harness {
  const log: TerminalCall[] = [];
  const sent: ClientMessage[] = [];
  const pending: (() => void)[] = [];
  let clock = T0;
  let current: ConnectionStateName = "ready";
  let next = 0;
  let seen = 0;
  const terminal: TerminalLike = {
    write(data, done) {
      log.push({ kind: "write", text: new TextDecoder().decode(data) });
      pending.push(done);
    },
    reset() {
      log.push({ kind: "reset" });
    },
  };
  const pane = createTerminalPane({
    sessionId: "s1",
    terminal,
    send: (message) => {
      sent.push(message);
      return current === "ready";
    },
    status: () => current,
    now: () => clock,
    newRequestId: () => {
      next += 1;
      return `r${next}`;
    },
  });
  return {
    pane,
    log,
    sent,
    finishWrites() {
      for (const done of pending.splice(0)) {
        done();
      }
    },
    at(ms) {
      clock = ms;
    },
    connection(state) {
      current = state;
    },
    fresh() {
      const out = sent.slice(seen);
      seen = sent.length;
      return out;
    },
  };
}

/** `text` as base64, the way `keys.ts` encodes a send. */
function b64(text: string): string {
  let binary = "";
  for (const byte of new TextEncoder().encode(text)) {
    binary += String.fromCharCode(byte);
  }
  return btoa(binary);
}

function message(daemon: DaemonMessage): ConnectionNotice {
  return { kind: "message", message: daemon };
}

/** The daemon's live output for `s1`. */
function output(data: string): ConnectionNotice {
  return message({ type: "pty_output", session_id: "s1", data_b64: b64(data) });
}

/** The daemon's reply to one `load_scrollback` for `s1`. */
function scrollback(data: string, requestId: string | null): ConnectionNotice {
  return message({
    type: "scrollback",
    session_id: "s1",
    data_b64: b64(data),
    truncated: false,
    request_id: requestId,
    forwarder_restarted: false,
  });
}

function status(state: ConnectionStateName): ConnectionNotice {
  return { kind: "status", state };
}

function resize(cols: number, rows: number, claim: boolean): ClientMessage {
  return { type: "resize", session_id: "s1", cols, rows, claim };
}

function typed(text: string): ClientMessage {
  return keyInput("s1", text);
}

function writes(...texts: string[]): TerminalCall[] {
  return texts.map((text) => ({ kind: "write", text }));
}

/** A harness opened at 80 by 24 whose first load already replied and finished writing. */
function loaded(): Harness {
  const h = harness();
  h.pane.open(80, 24, "attach");
  h.pane.notice(scrollback("history", "r1"));
  h.finishWrites();
  h.log.length = 0;
  h.fresh();
  return h;
}

test("pane_open_asks_for_the_scrollback_then_claims_the_size", () => {
  const h = harness();
  h.pane.open(80, 24, "attach");
  expect(h.fresh()).toEqual([
    { type: "load_scrollback", session_id: "s1", request_id: "r1" },
    resize(80, 24, true),
  ]);
  expect(h.log).toEqual([]);

  h.pane.open(80, 24, "attach");
  expect(h.fresh()).toEqual([]);
});

test("pane_reply_resets_then_writes_history_and_held_output_then_resizes", () => {
  const h = harness();
  h.pane.open(80, 24, "toggle");
  h.fresh();

  h.pane.notice(output("a"));
  h.pane.notice(output("b"));
  expect(h.log).toEqual([]);
  expect(h.fresh()).toEqual([]);

  h.pane.notice(scrollback("history", "r1"));
  expect(h.log).toEqual([{ kind: "reset" }, ...writes("history", "a", "b")]);
  expect(h.fresh()).toEqual([resize(80, 24, false)]);
});

test("pane_live_output_after_the_load_writes_at_once_without_a_reset", () => {
  const h = loaded();
  h.pane.notice(output("live"));
  expect(h.log).toEqual(writes("live"));
  expect(h.fresh()).toEqual([]);
});

test("pane_a_terminal_reply_is_automatic_and_a_letter_during_a_replay_is_typing", () => {
  const h = harness();
  h.pane.open(80, 24, "attach");
  h.pane.notice(scrollback("\x1b[6n", "r1"));
  h.fresh();

  // The replay's write is still unparsed.
  h.pane.typed("\x1b[12;40R");
  expect(h.fresh()).toEqual([{ ...typed("\x1b[12;40R"), automatic: true }]);

  h.pane.typed("x");
  expect(h.fresh()).toEqual([resize(80, 24, true), typed("x")]);

  h.finishWrites();
  h.at(T0 + 1_000);
  h.pane.typed("\x1b[3;1R");
  expect(h.fresh()).toEqual([{ ...typed("\x1b[3;1R"), automatic: true }]);
});

test("pane_typing_claims_the_size_ahead_of_the_input_once_per_quiet_spell", () => {
  const h = harness();
  h.pane.open(80, 24, "attach");
  h.fresh();

  h.pane.typed("a");
  expect(h.fresh()).toEqual([resize(80, 24, true), typed("a")]);

  h.at(T0 + 1_000);
  h.pane.typed("b");
  expect(h.fresh()).toEqual([typed("b")]);

  h.at(T0 + 6_000);
  h.pane.typed("c");
  expect(h.fresh()).toEqual([resize(80, 24, true), typed("c")]);
});

test("pane_tray_and_quick_take_the_claim_first_path_and_ctrl_chords_a_letter", () => {
  const h = harness();
  h.pane.open(80, 24, "attach");
  h.fresh();

  h.pane.key({ label: "Esc", bytes: "\x1b" });
  expect(h.fresh()).toEqual([resize(80, 24, true), typed("\x1b")]);

  h.at(T0 + 6_000);
  h.pane.quick("yes");
  expect(h.fresh()).toEqual([resize(80, 24, true), typed(quickReplyBytes("yes"))]);

  h.at(T0 + 7_000);
  expect(h.pane.ctrlArmed()).toBe(false);
  expect(h.pane.toggleCtrl()).toBe(true);
  expect(h.pane.ctrlArmed()).toBe(true);
  h.pane.key({ label: "C", bytes: "c" });
  expect(h.fresh()).toEqual([typed(ctrl("c"))]);
  expect(h.pane.ctrlArmed()).toBe(false);
});

test("pane_an_unchanged_measure_sends_nothing_and_a_pane_resize_claims", () => {
  const h = harness();
  h.pane.open(80, 24, "attach");
  h.fresh();

  h.pane.measured(80, 24, "measure");
  h.pane.measured(80, 24, "pane-resize");
  expect(h.fresh()).toEqual([]);

  h.pane.measured(100, 30, "pane-resize");
  expect(h.fresh()).toEqual([resize(100, 30, true)]);
});

test("pane_ready_after_a_drop_reattaches_and_its_reply_resets_first", () => {
  const h = loaded();

  h.pane.notice(status("ready"));
  expect(h.fresh()).toEqual([]);

  h.pane.notice(status("backoff"));
  h.pane.notice(status("ready"));
  expect(h.fresh()).toEqual([
    { type: "load_scrollback", session_id: "s1", request_id: "r2" },
    resize(80, 24, false),
  ]);

  h.pane.notice(scrollback("again", "r2"));
  expect(h.log).toEqual([{ kind: "reset" }, ...writes("again")]);
  expect(h.fresh()).toEqual([resize(80, 24, false)]);
});

test("pane_opened_while_connecting_reattaches_on_the_first_ready", () => {
  const h = harness();
  h.connection("connecting");
  h.pane.open(80, 24, "attach");
  h.fresh();

  h.connection("ready");
  h.pane.notice(status("ready"));
  expect(h.fresh()).toEqual([
    { type: "load_scrollback", session_id: "s1", request_id: "r2" },
    resize(80, 24, false),
  ]);
});

test("pane_a_reply_to_an_older_request_is_not_written", () => {
  const h = harness();
  h.pane.open(80, 24, "attach");
  h.at(T0 + REQUEST_TIMEOUT);
  h.pane.tick();
  h.at(T0 + REQUEST_TIMEOUT + RETRY_DELAYS[0]);
  h.pane.tick();
  h.fresh();

  h.pane.notice(scrollback("old", "r1"));
  expect(h.log).toEqual([]);
  expect(h.fresh()).toEqual([]);
});

test("pane_tick_resends_the_scrollback_after_the_retry_delay", () => {
  const h = harness();
  h.pane.open(80, 24, "attach");
  h.fresh();

  h.at(T0 + REQUEST_TIMEOUT);
  h.pane.tick();
  expect(h.fresh()).toEqual([]);

  h.at(T0 + REQUEST_TIMEOUT + RETRY_DELAYS[0]);
  h.pane.tick();
  expect(h.fresh()).toEqual([{ type: "load_scrollback", session_id: "s1", request_id: "r2" }]);
});

test("pane_close_detaches_and_leaves_the_pane_inert", () => {
  const h = harness();
  h.pane.open(80, 24, "attach");
  h.fresh();

  h.pane.close();
  expect(h.fresh()).toEqual([{ type: "detach", session_id: "s1" }]);

  h.pane.notice(scrollback("history", "r1"));
  h.pane.notice(output("late"));
  h.pane.notice(status("backoff"));
  h.pane.notice(status("ready"));
  h.pane.typed("a");
  h.pane.key({ label: "Esc", bytes: "\x1b" });
  h.pane.quick("yes");
  h.pane.measured(100, 30, "pane-resize");
  h.at(T0 + REQUEST_TIMEOUT * 10);
  h.pane.tick();
  h.pane.open(80, 24, "attach");
  h.pane.close();
  expect(h.fresh()).toEqual([]);
  expect(h.log).toEqual([]);
});

test("pane_an_empty_data_string_sends_nothing_and_does_not_arm_the_claim", () => {
  const h = harness();
  h.pane.open(80, 24, "attach");
  h.fresh();

  h.pane.typed("");
  expect(h.fresh()).toEqual([]);

  h.at(T0 + 1_000);
  h.pane.typed("a");
  expect(h.fresh()).toEqual([resize(80, 24, true), typed("a")]);
});
