import { afterEach, beforeEach, expect, test, vi } from "vitest";

import { backoffDelayMs } from "./backoff";
import {
  WELCOME_TIMEOUT_MS,
  step,
  type ConnectionEffect,
  type ConnectionState,
} from "./connection-machine";
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

test("machine_device_pair_failed_stops_redialling", () => {
  const refused = step(READY, {
    type: "message",
    message: { type: "device_pair_failed", reason: "device not approved" },
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
    { name: "handshaking", attempt: 0 },
    { type: "message", message: { type: "device_pair_failed", reason: "device not approved" } },
  );

  expect(state).toEqual({ name: "notApproved" });
  expect(effects).toEqual([
    { type: "clearTimer" },
    { type: "close" },
    { type: "status", state: "notApproved" },
  ]);
});

test("machine_token_changed_redials_after_not_approved", () => {
  const { state, effects } = step({ name: "notApproved" }, { type: "tokenChanged" });

  expect(state).toEqual({ name: "connecting", attempt: 0 });
  expect(effects).toEqual([
    { type: "clearTimer" },
    { type: "open" },
    { type: "status", state: "connecting" },
  ]);
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
  const tokenStore: TokenStore = { get: () => Promise.reject(new Error("no stored token")) };
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
