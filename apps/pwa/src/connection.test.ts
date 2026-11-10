import { afterEach, beforeEach, expect, test, vi } from "vitest";

import { backoffDelayMs } from "./backoff";
import {
  WELCOME_TIMEOUT_MS,
  step,
  type ConnectionEffect,
  type ConnectionState,
} from "./connection-machine";
import { CONNECTION_DROPPED, DEVICE_NOT_APPROVED, ENTER_THE_CODE, SAVE_FAILED } from "./pairing";
import { PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS, type DaemonMessage } from "./protocol";
import {
  createConnection,
  type Connection,
  type ConnectionNotice,
  type WebSocketLike,
} from "./socket";
import { memoryTokenStore, type TokenStore } from "./token-store";

const DEVICE_TOKEN = "device-token";

const WELCOME: DaemonMessage = {
  type: "welcome",
  protocol_version: PROTOCOL_VERSION,
  supported_versions: [...SUPPORTED_PROTOCOL_VERSIONS],
  features: ["device_pairing", "a_feature_from_a_newer_daemon"],
  daemon_id: "daemon-1",
  origin: "remote",
};

const READY: ConnectionState = { name: "ready", attempt: 0, features: new Set(), daemonId: null };

function setTimerEffects(effects: ConnectionEffect[]): ConnectionEffect[] {
  return effects.filter((effect) => effect.type === "setTimer");
}

function deliveredMessages(notices: ConnectionNotice[]): DaemonMessage[] {
  return notices.flatMap((notice) => (notice.kind === "message" ? [notice.message] : []));
}

function lastStatus(notices: ConnectionNotice[]): string | undefined {
  return notices.flatMap((notice) => (notice.kind === "status" ? [notice.state] : [])).at(-1);
}

// --- the pure reducer -------------------------------------------------------

test("machine_start_goes_connecting_and_opens", () => {
  const { state, effects } = step({ name: "idle" }, { type: "start" });

  expect(state).toEqual({ name: "connecting", attempt: 0 });
  expect(effects).toEqual([
    { type: "clearTimer" },
    { type: "open" },
    { type: "status", state: "connecting" },
  ]);
});

test("machine_start_clears_a_pending_timer", () => {
  const { state, effects } = step({ name: "backoff", attempt: 4 }, { type: "start" });

  expect(state).toEqual({ name: "connecting", attempt: 0 });
  expect(effects).toEqual([
    { type: "clearTimer" },
    { type: "open" },
    { type: "status", state: "connecting" },
  ]);
});

test("machine_opened_sends_the_hello_with_the_token", () => {
  const { state, effects } = step(
    { name: "connecting", attempt: 2 },
    { type: "opened", token: DEVICE_TOKEN },
  );

  expect(state).toEqual({ name: "handshaking", attempt: 2 });
  expect(effects).toEqual([
    {
      type: "send",
      message: {
        type: "hello",
        protocol_version: PROTOCOL_VERSION,
        protocol_versions: [...SUPPORTED_PROTOCOL_VERSIONS],
        auth_token: DEVICE_TOKEN,
        client_kind: "phone",
      },
    },
    { type: "setTimer", delayMs: WELCOME_TIMEOUT_MS },
    { type: "status", state: "handshaking" },
  ]);
});

test("machine_welcome_makes_ready_and_keeps_features", () => {
  const { state, effects } = step(
    { name: "handshaking", attempt: 3 },
    { type: "message", message: WELCOME },
  );

  expect(state).toEqual({
    name: "ready",
    attempt: 0,
    features: new Set(["device_pairing", "a_feature_from_a_newer_daemon"]),
    daemonId: "daemon-1",
  });
  expect(effects).toEqual([
    { type: "clearTimer" },
    { type: "deliver", message: WELCOME },
    { type: "status", state: "ready" },
  ]);
});

test("machine_welcome_timeout_closes_the_socket_and_backs_off", () => {
  const { state, effects } = step({ name: "handshaking", attempt: 0 }, { type: "timer" });

  expect(state).toEqual({ name: "backoff", attempt: 1 });
  expect(effects).toEqual([
    { type: "close" },
    { type: "setTimer", delayMs: backoffDelayMs(0) },
    { type: "status", state: "backoff" },
  ]);
});

test("machine_close_backs_off_with_growing_delays", () => {
  const first = step({ name: "connecting", attempt: 0 }, { type: "closed" });
  expect(first.state).toEqual({ name: "backoff", attempt: 1 });
  expect(first.effects).toEqual([
    { type: "setTimer", delayMs: 500 },
    { type: "status", state: "backoff" },
  ]);

  const retried = step(first.state, { type: "timer" });
  expect(retried.state).toEqual({ name: "connecting", attempt: 1 });

  const second = step(retried.state, { type: "closed" });
  expect(second.state).toEqual({ name: "backoff", attempt: 2 });
  expect(second.effects).toEqual([
    { type: "setTimer", delayMs: 1000 },
    { type: "status", state: "backoff" },
  ]);
});

test("machine_timer_in_backoff_reconnects", () => {
  const { state, effects } = step({ name: "backoff", attempt: 2 }, { type: "timer" });

  expect(state).toEqual({ name: "connecting", attempt: 2 });
  expect(effects).toEqual([{ type: "open" }, { type: "status", state: "connecting" }]);
});

// A connection that presented a token (a revoked or unknown one) and is
// refused stays refused: only a new token redials.
test("machine_device_pair_failed_stops_redialling", () => {
  const refused = step(READY, {
    type: "message",
    message: { type: "device_pair_failed", reason: "remote access is off" },
  });

  expect(refused.state).toEqual({ name: "notApproved" });
  expect(refused.effects).toEqual([{ type: "close" }, { type: "status", state: "notApproved" }]);

  // Neither the closed socket nor a timer left over from the handshake redials.
  expect(step(refused.state, { type: "closed" })).toEqual({ state: refused.state, effects: [] });
  expect(step(refused.state, { type: "timer" })).toEqual({ state: refused.state, effects: [] });
  expect(setTimerEffects(refused.effects)).toEqual([]);
});

test("machine_device_pair_failed_during_handshaking_clears_the_welcome_timer", () => {
  const { state, effects } = step(
    { name: "handshaking", attempt: 0, unpaired: true },
    { type: "message", message: { type: "device_pair_failed", reason: "device not approved" } },
  );

  // A no-token connection keeps its socket: it is the pairing channel.
  expect(state).toEqual({ name: "pairing", connected: true });
  expect(effects).toEqual([{ type: "clearTimer" }, { type: "status", state: "pairing" }]);
});

test("machine_token_changed_redials_after_not_approved", () => {
  const states: ConnectionState[] = [
    { name: "notApproved" },
    { name: "pairing", connected: false },
  ];
  for (const from of states) {
    const { state, effects } = step(from, { type: "tokenChanged" });

    expect(state).toEqual({ name: "connecting", attempt: 0 });
    expect(effects).toEqual([
      { type: "clearTimer" },
      { type: "open" },
      { type: "status", state: "connecting" },
    ]);
  }
});

test("machine_token_changed_while_hidden_does_not_dial", () => {
  const state: ConnectionState = { name: "hidden" };

  expect(step(state, { type: "tokenChanged" })).toEqual({ state, effects: [] });
});

test("machine_hidden_closes_and_sets_no_timer", () => {
  const { state, effects } = step(READY, { type: "hiddenEvent" });

  expect(state).toEqual({ name: "hidden" });
  expect(effects).toEqual([
    { type: "clearTimer" },
    { type: "close" },
    { type: "status", state: "hidden" },
  ]);
  expect(setTimerEffects(effects)).toEqual([]);
});

test("machine_not_approved_stays_put_when_hidden", () => {
  const state: ConnectionState = { name: "notApproved" };

  expect(step(state, { type: "hiddenEvent" })).toEqual({ state, effects: [] });

  // A pairing channel closes its socket when hidden, loses its request, and
  // stays in `pairing`: neither the next `visible` nor a timer redials.
  const hidden = step({ name: "pairing", connected: true }, { type: "hiddenEvent" });
  expect(hidden).toEqual({
    state: { name: "pairing", connected: false },
    effects: [{ type: "close" }, { type: "pairingLost" }],
  });
  expect(step(hidden.state, { type: "visible" })).toEqual({ state: hidden.state, effects: [] });
  expect(step(hidden.state, { type: "timer" })).toEqual({ state: hidden.state, effects: [] });
  expect(step(hidden.state, { type: "hiddenEvent" })).toEqual({
    state: hidden.state,
    effects: [],
  });
});

test("machine_token_changed_on_an_open_pairing_channel_loses_its_request", () => {
  const { state, effects } = step({ name: "pairing", connected: true }, { type: "tokenChanged" });

  expect(state).toEqual({ name: "connecting", attempt: 0 });
  expect(effects).toContainEqual({ type: "open" });
  expect(effects).toContainEqual({ type: "pairingLost" });
});

test("machine_token_changed_drops_a_tap_dial_and_says_so", () => {
  const states: ConnectionState[] = [
    { name: "connecting", attempt: 0, ask: true },
    { name: "handshaking", attempt: 0, unpaired: true, ask: true },
    { ...READY, unpaired: true, ask: true },
  ];
  for (const from of states) {
    const { state, effects } = step(from, { type: "tokenChanged" });

    expect(state).toEqual({ name: "connecting", attempt: 0 });
    expect(effects).toContainEqual({ type: "pairingLost" });
  }
});

test("machine_stored_token_refused_as_not_approved_is_dropped_for_the_visit", () => {
  const refused = step(READY, {
    type: "message",
    message: { type: "device_pair_failed", reason: "device not approved" },
  });

  expect(refused.state).toEqual({ name: "notApproved" });
  expect(refused.effects).toEqual([
    { type: "close" },
    { type: "dropToken" },
    { type: "status", state: "notApproved" },
  ]);
  expect(setTimerEffects(refused.effects)).toEqual([]);
});

test("machine_opened_without_a_token_marks_the_handshake_unpaired", () => {
  for (const token of [undefined, ""]) {
    const { state } = step({ name: "connecting", attempt: 0 }, { type: "opened", token });

    expect(state).toEqual({ name: "handshaking", attempt: 0, unpaired: true });
  }
});

test("machine_no_token_welcome_then_not_approved_opens_the_pairing_channel", () => {
  const ready = step(
    { name: "handshaking", attempt: 0, unpaired: true },
    { type: "message", message: WELCOME },
  );
  expect(ready.state).toEqual({
    name: "ready",
    attempt: 0,
    features: new Set(["device_pairing", "a_feature_from_a_newer_daemon"]),
    daemonId: "daemon-1",
    unpaired: true,
  });

  const pairing = step(ready.state, {
    type: "message",
    message: { type: "device_pair_failed", reason: "device not approved" },
  });
  expect(pairing).toEqual({
    state: { name: "pairing", connected: true },
    effects: [{ type: "status", state: "pairing" }],
  });
});

test("machine_pairing_delivers_pairing_messages_and_drops_the_rest", () => {
  const state: ConnectionState = { name: "pairing", connected: true };
  const pending = { type: "device_pair_pending", request_id: "req-1", expires_in_secs: 300 };

  expect(step(state, { type: "message", message: pending })).toEqual({
    state,
    effects: [{ type: "deliver", message: pending }],
  });
  expect(step(state, { type: "message", message: { type: "repos", repos: [] } })).toEqual({
    state,
    effects: [],
  });
});

test("machine_pairing_close_loses_the_request_and_does_not_redial", () => {
  const { state, effects } = step({ name: "pairing", connected: true }, { type: "closed" });

  expect(state).toEqual({ name: "pairing", connected: false });
  expect(effects).toEqual([{ type: "pairingLost" }]);
  expect(setTimerEffects(effects)).toEqual([]);
});

test("machine_ask_pair_sends_on_an_open_channel_and_redials_a_closed_one", () => {
  const open: ConnectionState = { name: "pairing", connected: true };
  expect(step(open, { type: "askPair" })).toEqual({
    state: open,
    effects: [{ type: "send", message: { type: "device_pair_request" } }],
  });

  const redial = step({ name: "pairing", connected: false }, { type: "askPair" });
  expect(redial.state).toEqual({ name: "connecting", attempt: 0, ask: true });
  expect(redial.effects).toEqual([
    { type: "clearTimer" },
    { type: "open" },
    { type: "status", state: "connecting" },
  ]);
});

test("machine_ask_pair_outside_pairing_does_nothing", () => {
  const states: ConnectionState[] = [READY, { name: "notApproved" }, { name: "hidden" }];
  for (const state of states) {
    expect(step(state, { type: "askPair" })).toEqual({ state, effects: [] });
    expect(step(state, { type: "redeem", requestId: "req-1", code: "123456" })).toEqual({
      state,
      effects: [],
    });
  }
});

test("machine_redeem_sends_only_on_an_open_channel", () => {
  const open: ConnectionState = { name: "pairing", connected: true };
  expect(step(open, { type: "redeem", requestId: "req-1", code: "123456" })).toEqual({
    state: open,
    effects: [
      {
        type: "send",
        message: { type: "device_pair_redeem", request_id: "req-1", code: "123456" },
      },
    ],
  });

  const closed: ConnectionState = { name: "pairing", connected: false };
  expect(step(closed, { type: "redeem", requestId: "req-1", code: "123456" })).toEqual({
    state: closed,
    effects: [],
  });
});

test("machine_a_lost_ask_dial_says_so", () => {
  const connecting = step({ name: "connecting", attempt: 0, ask: true }, { type: "closed" });
  expect(connecting.state).toEqual({ name: "backoff", attempt: 1 });
  expect(connecting.effects).toContainEqual({ type: "pairingLost" });

  const hidden = step({ name: "handshaking", attempt: 0, ask: true }, { type: "hiddenEvent" });
  expect(hidden.state).toEqual({ name: "hidden" });
  expect(hidden.effects).toContainEqual({ type: "pairingLost" });
});

test("machine_visible_reconnects_at_once_with_a_fresh_attempt", () => {
  const fromBackoff = step({ name: "backoff", attempt: 5 }, { type: "visible" });
  expect(fromBackoff.state).toEqual({ name: "connecting", attempt: 0 });
  expect(fromBackoff.effects).toEqual([
    { type: "clearTimer" },
    { type: "open" },
    { type: "status", state: "connecting" },
  ]);

  const fromHidden = step({ name: "hidden" }, { type: "visible" });
  expect(fromHidden.state).toEqual({ name: "connecting", attempt: 0 });
});

test("machine_unknown_message_in_ready_is_delivered_not_thrown", () => {
  const message = { type: "a_message_from_a_newer_daemon", payload: 1 };
  const { state, effects } = step(READY, { type: "message", message });

  expect(state).toBe(READY);
  expect(effects).toEqual([{ type: "deliver", message }]);
});

test("machine_frame_without_a_type_in_ready_is_ignored", () => {
  expect(step(READY, { type: "message", message: 42 })).toEqual({ state: READY, effects: [] });
  expect(step(READY, { type: "message", message: null })).toEqual({ state: READY, effects: [] });
});

test("machine_non_welcome_during_handshaking_is_ignored", () => {
  const state: ConnectionState = { name: "handshaking", attempt: 0 };

  expect(step(state, { type: "message", message: { type: "repos", repos: [] } })).toEqual({
    state,
    effects: [],
  });
});

// --- the driver -------------------------------------------------------------

class FakeSocket implements WebSocketLike {
  readonly frames: string[] = [];
  closed = false;
  /** `WebSocket.OPEN`. */
  readyState = 1;
  onopen: ((event: unknown) => void) | null = null;
  onmessage: ((event: { data: unknown }) => void) | null = null;
  onclose: ((event: unknown) => void) | null = null;
  onerror: ((event: unknown) => void) | null = null;

  send(data: string): void {
    this.frames.push(data);
  }

  close(): void {
    this.closed = true;
  }

  open(): void {
    this.onopen?.({});
  }

  receive(message: unknown): void {
    this.onmessage?.({ data: JSON.stringify(message) });
  }

  receiveText(text: string): void {
    this.onmessage?.({ data: text });
  }

  serverClose(): void {
    this.onclose?.({});
  }
}

function fakeVisibility(hiddenAtStart = false): {
  source: { isHidden(): boolean; onChange(callback: () => void): () => void };
  hide(): void;
  show(): void;
} {
  let hidden = hiddenAtStart;
  const callbacks = new Set<() => void>();
  const fire = (): void => {
    for (const callback of callbacks) {
      callback();
    }
  };
  return {
    source: {
      isHidden: () => hidden,
      onChange: (callback) => {
        callbacks.add(callback);
        return () => {
          callbacks.delete(callback);
        };
      },
    },
    hide: () => {
      hidden = true;
      fire();
    },
    show: () => {
      hidden = false;
      fire();
    },
  };
}

interface Harness {
  connection: Connection;
  sockets: FakeSocket[];
  notices: ConnectionNotice[];
  visibility: ReturnType<typeof fakeVisibility>;
}

interface HarnessOptions {
  readonly tokenStore?: TokenStore;
  readonly hiddenAtStart?: boolean;
  readonly reloadApp?: () => void;
}

function harness(options: HarnessOptions = {}): Harness {
  const sockets: FakeSocket[] = [];
  const notices: ConnectionNotice[] = [];
  const visibility = fakeVisibility(options.hiddenAtStart ?? false);
  const connection = createConnection({
    url: "wss://host.tail1234.ts.net:8787/ws",
    openSocket: () => {
      const socket = new FakeSocket();
      sockets.push(socket);
      return socket;
    },
    setTimeout: (callback, delayMs) => setTimeout(callback, delayMs),
    clearTimeout: (handle) => {
      clearTimeout(handle);
    },
    visibility: visibility.source,
    tokenStore: options.tokenStore ?? memoryTokenStore(DEVICE_TOKEN),
    reloadApp: options.reloadApp ?? (() => undefined),
  });
  connection.subscribe((notice) => {
    notices.push(notice);
  });
  return { connection, sockets, notices, visibility };
}

function socketAt(sockets: FakeSocket[], index: number): FakeSocket {
  const socket = sockets[index];
  if (!socket) {
    throw new Error(`no socket was opened at ${index}`);
  }
  return socket;
}

async function flush(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

test("connection_cycle_reconnects_after_the_backoff", async () => {
  const { connection, sockets, notices } = harness();

  connection.start();
  await flush();
  const first = socketAt(sockets, 0);
  expect(first.frames).toEqual([]);

  first.open();
  expect(JSON.parse(first.frames[0] ?? "null")).toEqual({
    type: "hello",
    protocol_version: PROTOCOL_VERSION,
    protocol_versions: [...SUPPORTED_PROTOCOL_VERSIONS],
    auth_token: DEVICE_TOKEN,
    client_kind: "phone",
  });

  first.receive(WELCOME);
  expect(lastStatus(notices)).toBe("ready");
  expect(deliveredMessages(notices)).toEqual([WELCOME]);

  first.serverClose();
  expect(lastStatus(notices)).toBe("backoff");

  await vi.advanceTimersByTimeAsync(backoffDelayMs(0));
  await flush();
  expect(sockets).toHaveLength(2);

  const second = socketAt(sockets, 1);
  second.open();
  second.receive(WELCOME);
  expect(lastStatus(notices)).toBe("ready");
});

test("connection_drops_an_unparseable_frame_and_stays_ready", async () => {
  const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
  const { connection, sockets, notices } = harness();

  connection.start();
  await flush();
  const socket = socketAt(sockets, 0);
  socket.open();
  socket.receive(WELCOME);
  const before = notices.length;

  socket.receiveText("{not json");

  expect(notices).toHaveLength(before);
  expect(lastStatus(notices)).toBe("ready");
  expect(warn).toHaveBeenCalled();
});

test("reload_app reloads the page once", async () => {
  const reloadApp = vi.fn();
  const { connection, sockets } = harness({ reloadApp });

  connection.start();
  await flush();
  const socket = socketAt(sockets, 0);
  socket.open();
  socket.receive(WELCOME);

  // The host may say it twice; one page visit reloads once.
  socket.receive({ type: "reload_app" });
  socket.receive({ type: "reload_app" });

  expect(reloadApp).toHaveBeenCalledOnce();
});

test("connection_ignores_a_stale_socket_close", async () => {
  const { connection, sockets, notices } = harness();

  connection.start();
  await flush();
  const first = socketAt(sockets, 0);
  first.open();
  first.receive(WELCOME);
  first.serverClose();

  await vi.advanceTimersByTimeAsync(backoffDelayMs(0));
  await flush();
  const second = socketAt(sockets, 1);
  second.open();
  second.receive(WELCOME);
  const before = notices.length;

  // The replaced socket's own close arrives after the reconnect.
  first.serverClose();

  expect(notices).toHaveLength(before);
  expect(lastStatus(notices)).toBe("ready");
});

test("connection_goes_hidden_without_a_socket_and_reconnects_when_visible", async () => {
  const { connection, sockets, notices, visibility } = harness();

  connection.start();
  await flush();
  const first = socketAt(sockets, 0);
  first.open();
  first.receive(WELCOME);

  visibility.hide();
  expect(lastStatus(notices)).toBe("hidden");
  expect(first.closed).toBe(true);

  await vi.advanceTimersByTimeAsync(WELCOME_TIMEOUT_MS * 10);
  expect(sockets).toHaveLength(1);

  visibility.show();
  await flush();
  expect(sockets).toHaveLength(2);
  const second = socketAt(sockets, 1);
  second.open();
  second.receive(WELCOME);
  expect(lastStatus(notices)).toBe("ready");
});

test("connection_backs_off_when_the_device_token_cannot_be_read", async () => {
  const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
  const tokenStore: TokenStore = {
    get: () => Promise.reject(new Error("no stored token")),
    set: () => Promise.resolve(),
    clear: () => Promise.resolve(),
  };
  const { connection, sockets, notices } = harness({ tokenStore });

  connection.start();
  await flush();

  expect(sockets).toHaveLength(0);
  expect(lastStatus(notices)).toBe("backoff");
  expect(warn).toHaveBeenCalled();

  // The retry after the backoff asks the store again, and backs off again.
  await vi.advanceTimersByTimeAsync(backoffDelayMs(0));
  await flush();
  expect(sockets).toHaveLength(0);
  expect(lastStatus(notices)).toBe("backoff");
});

test("connection_started_hidden_holds_no_socket_and_dials_when_visible", async () => {
  const { connection, sockets, notices, visibility } = harness({ hiddenAtStart: true });

  connection.start();
  await flush();
  expect(sockets).toHaveLength(0);
  expect(lastStatus(notices)).toBe("hidden");

  await vi.advanceTimersByTimeAsync(WELCOME_TIMEOUT_MS * 10);
  expect(sockets).toHaveLength(0);

  visibility.show();
  await flush();
  expect(sockets).toHaveLength(1);
});

test("connection_does_not_dial_on_a_token_change_while_hidden", async () => {
  const { connection, sockets, notices, visibility } = harness();

  connection.start();
  await flush();
  const socket = socketAt(sockets, 0);
  socket.open();
  socket.receive(WELCOME);
  visibility.hide();

  connection.tokenChanged();
  await flush();

  expect(sockets).toHaveLength(1);
  expect(lastStatus(notices)).toBe("hidden");
});

// --- pairing over the no-token connection -----------------------------------

const NOT_APPROVED = { type: "device_pair_failed", reason: DEVICE_NOT_APPROVED };
const PENDING = { type: "device_pair_pending", request_id: "req-1", expires_in_secs: 300 };
const APPROVED = { type: "device_pair_decided", request_id: "req-1", outcome: "approved" };

function sent(socket: FakeSocket): unknown[] {
  return socket.frames.map((frame) => JSON.parse(frame) as unknown);
}

function pairRequests(socket: FakeSocket): unknown[] {
  return sent(socket).filter((frame) => {
    return (frame as { type?: unknown }).type === "device_pair_request";
  });
}

/** Start a phone with no token and bring it to the pairing channel. */
async function pairingChannel(tokenStore: TokenStore = memoryTokenStore()): Promise<
  Harness & {
    socket: FakeSocket;
  }
> {
  const h = harness({ tokenStore });
  h.connection.start();
  await flush();
  const socket = socketAt(h.sockets, 0);
  socket.open();
  socket.receive(WELCOME);
  socket.receive(NOT_APPROVED);
  return { ...h, socket };
}

test("connection_no_token_lands_in_pairing_with_the_socket_open", async () => {
  const { socket, notices, connection } = await pairingChannel();

  expect(lastStatus(notices)).toBe("pairing");
  expect(socket.closed).toBe(false);
  expect(sent(socket).map((frame) => (frame as { type: string }).type)).toEqual(["hello"]);
  expect(connection.pairingState()).toEqual({
    phase: "idle",
    requestId: null,
    expiresInSecs: null,
    error: null,
  });
});

test("connection_ask_to_pair_sends_one_request_per_tap_until_it_ends", async () => {
  const { socket, connection } = await pairingChannel();

  connection.askToPair();
  expect(pairRequests(socket)).toHaveLength(1);
  expect(connection.pairingState().phase).toBe("asking");

  connection.askToPair();
  expect(pairRequests(socket)).toHaveLength(1);

  socket.receive(PENDING);
  expect(connection.pairingState().phase).toBe("pending");
  connection.askToPair();
  expect(pairRequests(socket)).toHaveLength(1);
});

test("connection_ask_to_pair_redials_a_closed_channel_and_asks_after_not_approved", async () => {
  const { socket, sockets, notices, connection } = await pairingChannel();

  // The daemon's idle close: nothing redials by itself.
  socket.serverClose();
  expect(lastStatus(notices)).toBe("pairing");
  await vi.advanceTimersByTimeAsync(WELCOME_TIMEOUT_MS * 10);
  expect(sockets).toHaveLength(1);

  connection.askToPair();
  await flush();
  expect(sockets).toHaveLength(2);
  const second = socketAt(sockets, 1);
  second.open();
  second.receive(WELCOME);
  expect(pairRequests(second)).toEqual([]);

  second.receive(NOT_APPROVED);
  expect(pairRequests(second)).toEqual([{ type: "device_pair_request" }]);
  expect(lastStatus(notices)).toBe("pairing");
});

test("connection_approved_request_redeems_with_its_request_id", async () => {
  const { socket, connection } = await pairingChannel();
  connection.askToPair();
  socket.receive(PENDING);
  socket.receive(APPROVED);
  expect(connection.pairingState()).toEqual({
    phase: "approved",
    requestId: "req-1",
    expiresInSecs: 300,
    error: null,
  });

  connection.redeem("123456");

  expect(sent(socket).at(-1)).toEqual({
    type: "device_pair_redeem",
    request_id: "req-1",
    code: "123456",
  });
});

test("connection_redeem_refuses_a_code_that_is_not_six_digits", async () => {
  const { socket, connection } = await pairingChannel();
  connection.askToPair();
  socket.receive(PENDING);
  const before = socket.frames.length;

  connection.redeem("12a");

  expect(socket.frames).toHaveLength(before);
  expect(connection.pairingState().error).toBe(ENTER_THE_CODE);
});

test("connection_device_token_is_stored_and_the_redial_presents_it", async () => {
  const tokenStore = memoryTokenStore();
  const set = vi.spyOn(tokenStore, "set");
  const { socket, sockets, connection } = await pairingChannel(tokenStore);
  connection.askToPair();
  socket.receive(PENDING);
  socket.receive(APPROVED);
  connection.redeem("123456");

  socket.receive({ type: "device_token", device_id: "device-1", token: "new-token" });
  socket.serverClose();
  await flush();

  expect(set).toHaveBeenCalledWith("new-token");
  expect(connection.pairingState().phase).toBe("paired");
  expect(sockets).toHaveLength(2);
  const second = socketAt(sockets, 1);
  second.open();
  expect(sent(second)[0]).toMatchObject({ type: "hello", auth_token: "new-token" });
});

test("connection_failed_save_still_redials_with_the_token", async () => {
  vi.spyOn(console, "warn").mockImplementation(() => undefined);
  const tokenStore: TokenStore = {
    get: () => Promise.resolve(undefined),
    set: () => Promise.reject(new Error("quota exceeded")),
    clear: () => Promise.resolve(),
  };
  const { socket, sockets, connection } = await pairingChannel(tokenStore);
  connection.askToPair();
  socket.receive(PENDING);
  socket.receive(APPROVED);
  connection.redeem("123456");

  socket.receive({ type: "device_token", device_id: "device-1", token: "new-token" });
  await flush();
  await flush();

  expect(connection.pairingState().error).toBe(SAVE_FAILED);
  expect(sockets).toHaveLength(2);
  const second = socketAt(sockets, 1);
  second.open();
  expect(sent(second)[0]).toMatchObject({ type: "hello", auth_token: "new-token" });
});

test("connection_denied_or_expired_decision_is_denied_in_its_own_words", async () => {
  for (const outcome of ["denied", "expired"]) {
    const { socket, connection } = await pairingChannel();
    connection.askToPair();
    socket.receive(PENDING);

    socket.receive({ type: "device_pair_decided", request_id: "req-1", outcome });

    expect(connection.pairingState()).toEqual({
      phase: "denied",
      requestId: null,
      expiresInSecs: null,
      error: outcome,
    });
    connection.stop();
  }
});

test("connection_wrong_code_is_shown_verbatim_and_keeps_the_socket", async () => {
  const { socket, notices, connection } = await pairingChannel();
  connection.askToPair();
  socket.receive(PENDING);
  socket.receive(APPROVED);
  connection.redeem("000000");

  socket.receive({ type: "device_pair_failed", reason: "That code is not right." });

  expect(connection.pairingState().error).toBe("That code is not right.");
  expect(connection.pairingState().requestId).toBe("req-1");
  expect(socket.closed).toBe(false);
  expect(lastStatus(notices)).toBe("pairing");
});

test("connection_reason_the_daemon_closes_on_is_shown_and_the_close_stays_in_pairing", async () => {
  const reason = "Too many wrong codes; ask again from the device.";
  const { socket, sockets, notices, connection } = await pairingChannel();
  connection.askToPair();
  socket.receive(PENDING);
  socket.receive(APPROVED);
  connection.redeem("000000");

  socket.receive({ type: "device_pair_failed", reason });
  expect(socket.closed).toBe(false);
  socket.serverClose();
  await vi.advanceTimersByTimeAsync(WELCOME_TIMEOUT_MS * 10);

  expect(connection.pairingState().error).toBe(reason);
  expect(lastStatus(notices)).toBe("pairing");
  expect(sockets).toHaveLength(1);
});

test("connection_close_while_pending_drops_the_request_and_does_not_redial", async () => {
  const { socket, sockets, connection, visibility } = await pairingChannel();
  connection.askToPair();
  socket.receive(PENDING);

  socket.serverClose();
  await vi.advanceTimersByTimeAsync(WELCOME_TIMEOUT_MS * 10);

  expect(connection.pairingState()).toEqual({
    phase: "idle",
    requestId: null,
    expiresInSecs: null,
    error: CONNECTION_DROPPED,
  });
  expect(sockets).toHaveLength(1);

  visibility.hide();
  visibility.show();
  await flush();
  expect(sockets).toHaveLength(1);
});

test("connection_token_change_mid_request_drops_the_dead_request", async () => {
  const { socket, sockets, connection } = await pairingChannel();
  connection.askToPair();
  socket.receive(PENDING);

  connection.tokenChanged();
  await flush();

  expect(connection.pairingState().requestId).toBeNull();
  expect(connection.pairingState().error).toBe(CONNECTION_DROPPED);
  expect(sockets).toHaveLength(2);
  const second = socketAt(sockets, 1);
  connection.redeem("123456");
  expect(second.frames).toEqual([]);
});

/** A paired phone whose stored token the daemon refuses on the first socket. */
async function refusedStoredToken(
  reason: string,
  tokenStore: TokenStore = memoryTokenStore(DEVICE_TOKEN),
): Promise<Harness & { first: FakeSocket }> {
  const h = harness({ tokenStore });
  h.connection.start();
  await flush();
  const first = socketAt(h.sockets, 0);
  first.open();
  first.receive(WELCOME);
  first.receive({ type: "device_pair_failed", reason });
  await flush();
  await flush();
  return { ...h, first };
}

test("connection_refused_stored_token_is_kept_and_the_visit_lands_in_pairing", async () => {
  const tokenStore = memoryTokenStore(DEVICE_TOKEN);
  const clear = vi.spyOn(tokenStore, "clear");
  const { first, sockets, notices } = await refusedStoredToken(DEVICE_NOT_APPROVED, tokenStore);

  expect(first.closed).toBe(true);
  expect(clear).not.toHaveBeenCalled();
  expect(await tokenStore.get()).toBe(DEVICE_TOKEN);
  expect(sockets).toHaveLength(2);
  const second = socketAt(sockets, 1);
  second.open();
  expect(sent(second)[0]).toMatchObject({ type: "hello", auth_token: "" });
  second.receive(WELCOME);
  second.receive(NOT_APPROVED);
  expect(lastStatus(notices)).toBe("pairing");
  expect(second.closed).toBe(false);
});

test("connection_refusal_for_another_reason_is_not_approved_and_keeps_the_token", async () => {
  const tokenStore = memoryTokenStore(DEVICE_TOKEN);
  const clear = vi.spyOn(tokenStore, "clear");
  const { first, sockets, notices } = await refusedStoredToken("remote access is off", tokenStore);

  expect(lastStatus(notices)).toBe("notApproved");
  expect(first.closed).toBe(true);
  expect(clear).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(WELCOME_TIMEOUT_MS * 10);
  expect(sockets).toHaveLength(1);
});

test("connection_dropped_token_is_not_presented_again_in_the_same_visit", async () => {
  const tokenStore = memoryTokenStore(DEVICE_TOKEN);
  const { connection, sockets } = await refusedStoredToken(DEVICE_NOT_APPROVED, tokenStore);

  // Whatever dials next in this visit presents no token, though the store
  // still holds it: no refuse-and-redial loop.
  connection.tokenChanged();
  await flush();

  const last = socketAt(sockets, sockets.length - 1);
  expect(sockets).toHaveLength(3);
  last.open();
  expect(sent(last)[0]).toMatchObject({ type: "hello", auth_token: "" });
});

test("connection_fresh_page_presents_the_kept_token_again", async () => {
  const tokenStore = memoryTokenStore(DEVICE_TOKEN);
  const refusedVisit = await refusedStoredToken(DEVICE_NOT_APPROVED, tokenStore);
  refusedVisit.connection.stop();

  const relaunched = harness({ tokenStore });
  relaunched.connection.start();
  await flush();
  const socket = socketAt(relaunched.sockets, 0);
  socket.open();

  expect(sent(socket)[0]).toMatchObject({ type: "hello", auth_token: DEVICE_TOKEN });
});

test("connection_forget_with_a_failed_clear_still_redials_without_the_token", async () => {
  const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
  const tokenStore: TokenStore = {
    get: () => Promise.resolve(DEVICE_TOKEN),
    set: () => Promise.resolve(),
    clear: () => Promise.reject(new Error("database gone")),
  };
  const { connection, sockets } = harness({ tokenStore });
  connection.start();
  await flush();
  const first = socketAt(sockets, 0);
  first.open();
  first.receive(WELCOME);

  connection.forget();
  await flush();
  await flush();

  expect(warn).toHaveBeenCalled();
  expect(sockets).toHaveLength(2);
  const second = socketAt(sockets, 1);
  second.open();
  expect(sent(second)[0]).toMatchObject({ type: "hello", auth_token: "" });
});

test("connection_forget_clears_the_token_and_redials_without_it", async () => {
  const tokenStore = memoryTokenStore(DEVICE_TOKEN);
  const clear = vi.spyOn(tokenStore, "clear");
  const { connection, sockets, notices } = harness({ tokenStore });
  connection.start();
  await flush();
  const first = socketAt(sockets, 0);
  first.open();
  first.receive(WELCOME);
  expect(lastStatus(notices)).toBe("ready");

  connection.forget();
  await flush();
  await flush();

  expect(clear).toHaveBeenCalledOnce();
  expect(first.closed).toBe(true);
  expect(sockets).toHaveLength(2);
  const second = socketAt(sockets, 1);
  second.open();
  expect(sent(second)[0]).toMatchObject({ type: "hello", auth_token: "" });
  second.receive(WELCOME);
  second.receive(NOT_APPROVED);
  expect(lastStatus(notices)).toBe("pairing");
});

test("connection_dial_after_a_forget_reads_the_store_again", async () => {
  const tokenStore = memoryTokenStore();
  const { socket, sockets, connection } = await pairingChannel(tokenStore);
  connection.askToPair();
  socket.receive(PENDING);
  socket.receive(APPROVED);
  connection.redeem("123456");
  socket.receive({ type: "device_token", device_id: "device-1", token: "new-token" });
  await flush();

  connection.forget();
  await flush();
  await flush();
  await tokenStore.set("stored-later");
  connection.tokenChanged();
  await flush();

  const last = socketAt(sockets, sockets.length - 1);
  last.open();
  expect(sent(last)[0]).toMatchObject({ type: "hello", auth_token: "stored-later" });
});

test("connection_pairing_state_changes_are_notified", async () => {
  const { socket, notices, connection } = await pairingChannel();

  connection.askToPair();
  socket.receive(PENDING);

  const phases = notices.flatMap((notice) =>
    notice.kind === "pairing" ? [notice.pairing.phase] : [],
  );
  expect(phases).toEqual(["asking", "pending"]);
});
