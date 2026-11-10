import { afterEach, beforeEach, expect, test, vi } from "vitest";

import {
  FEATURE_RESUME_SESSION,
  PROTOCOL_VERSION,
  SUPPORTED_PROTOCOL_VERSIONS,
  type ClientMessage,
  type DaemonMessage,
} from "./protocol";
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
  features: [],
  daemon_id: "daemon-1",
  origin: "remote",
};

const LOAD_CHAT: ClientMessage = { type: "load_chat", session_id: "session-1" };

// --- the driver -------------------------------------------------------------

class FakeSocket implements WebSocketLike {
  readonly frames: string[] = [];
  closed = false;
  /** `WebSocket.OPEN`, until a test says otherwise. */
  readyState = 1;
  /** Make the next writes throw, as a socket closing under the write does. */
  throwOnSend = false;
  onopen: ((event: unknown) => void) | null = null;
  onmessage: ((event: { data: unknown }) => void) | null = null;
  onclose: ((event: unknown) => void) | null = null;
  onerror: ((event: unknown) => void) | null = null;

  send(data: string): void {
    if (this.throwOnSend) {
      throw new Error("the socket is closing");
    }
    this.frames.push(data);
  }

  close(): void {
    this.closed = true;
    this.readyState = 3;
  }

  open(): void {
    this.onopen?.({});
  }

  receive(message: unknown): void {
    this.onmessage?.({ data: JSON.stringify(message) });
  }

  serverClose(): void {
    this.onclose?.({});
  }
}

function fakeVisibility(): {
  source: { isHidden(): boolean; onChange(callback: () => void): () => void };
  hide(): void;
} {
  let hidden = false;
  const callbacks = new Set<() => void>();
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
      for (const callback of callbacks) {
        callback();
      }
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
}

function harness(options: HarnessOptions = {}): Harness {
  const sockets: FakeSocket[] = [];
  const notices: ConnectionNotice[] = [];
  const visibility = fakeVisibility();
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

function lastStatus(notices: ConnectionNotice[]): string | undefined {
  return notices.flatMap((notice) => (notice.kind === "status" ? [notice.state] : [])).at(-1);
}

async function flush(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
}

interface Reached {
  connection: Connection;
  sockets: FakeSocket[];
  socket: FakeSocket;
  notices: ConnectionNotice[];
  visibility: ReturnType<typeof fakeVisibility>;
}

async function reachConnecting(): Promise<Reached> {
  const { connection, sockets, notices, visibility } = harness();
  connection.start();
  await flush();
  return { connection, sockets, socket: socketAt(sockets, 0), notices, visibility };
}

async function reachHandshaking(): Promise<Reached> {
  const reached = await reachConnecting();
  reached.socket.open();
  return reached;
}

async function reachReady(): Promise<Reached> {
  const reached = await reachHandshaking();
  reached.socket.receive(WELCOME);
  return reached;
}

async function reachBackoff(): Promise<Reached> {
  const reached = await reachReady();
  reached.socket.serverClose();
  return reached;
}

async function reachHidden(): Promise<Reached> {
  const reached = await reachReady();
  reached.visibility.hide();
  return reached;
}

async function reachNotApproved(): Promise<Reached> {
  const reached = await reachReady();
  reached.socket.receive({ type: "device_pair_failed", reason: "token expired" });
  return reached;
}

async function reachUnpairedReady(): Promise<Reached> {
  const { connection, sockets, notices, visibility } = harness({ tokenStore: memoryTokenStore() });
  connection.start();
  await flush();
  const socket = socketAt(sockets, 0);
  socket.open();
  socket.receive(WELCOME);
  return { connection, sockets, socket, notices, visibility };
}

async function reachPairing(): Promise<Reached> {
  const reached = await reachUnpairedReady();
  reached.socket.receive({ type: "device_pair_failed", reason: "device not approved" });
  return reached;
}

const NOT_READY: readonly (readonly [string, () => Promise<Reached>])[] = [
  ["connecting", reachConnecting],
  ["handshaking", reachHandshaking],
  ["backoff", reachBackoff],
  ["hidden", reachHidden],
  ["notApproved", reachNotApproved],
  ["pairing", reachPairing],
];

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

// --- the send path ----------------------------------------------------------

test("paired_is_true_only_on_a_ready_connection_that_presented_a_token", async () => {
  const ready = await reachReady();
  expect(ready.connection.paired()).toBe(true);

  const unpaired = await reachUnpairedReady();
  expect(lastStatus(unpaired.notices)).toBe("ready");
  expect(unpaired.connection.paired()).toBe(false);

  for (const [state, reach] of NOT_READY) {
    const reached = await reach();
    expect(reached.connection.paired(), state).toBe(false);
  }
});

test("supports_is_true_only_on_a_ready_connection_whose_host_lists_the_feature", async () => {
  for (const [state, reach] of NOT_READY) {
    const reached = await reach();
    expect(reached.connection.supports(FEATURE_RESUME_SESSION), state).toBe(false);
  }

  const without = await reachReady();
  expect(lastStatus(without.notices)).toBe("ready");
  expect(without.connection.supports(FEATURE_RESUME_SESSION)).toBe(false);

  const listing = await reachHandshaking();
  listing.socket.receive({ ...WELCOME, features: [FEATURE_RESUME_SESSION] });
  expect(lastStatus(listing.notices)).toBe("ready");
  expect(listing.connection.supports(FEATURE_RESUME_SESSION)).toBe(true);
});

test("send_while_ready_writes_the_frame_and_answers_true", async () => {
  const reached = await reachReady();
  expect(lastStatus(reached.notices)).toBe("ready");

  expect(reached.connection.send(LOAD_CHAT)).toBe(true);
  expect(JSON.parse(reached.socket.frames.at(-1) ?? "null")).toEqual(LOAD_CHAT);
});

test("send_outside_ready_writes_nothing_and_answers_false", async () => {
  for (const [state, reach] of NOT_READY) {
    const reached = await reach();
    const frames = [...reached.socket.frames];

    expect(lastStatus(reached.notices)).toBe(state);
    expect(reached.connection.send(LOAD_CHAT)).toBe(false);
    expect(reached.socket.frames).toEqual(frames);
  }
});

test("send_on_an_unpaired_ready_connection_writes_nothing_and_answers_false", async () => {
  const reached = await reachUnpairedReady();
  expect(lastStatus(reached.notices)).toBe("ready");
  const frames = [...reached.socket.frames];

  expect(reached.connection.send(LOAD_CHAT)).toBe(false);
  expect(reached.socket.frames).toEqual(frames);
});

test("send_after_stop_writes_nothing_and_answers_false", async () => {
  const reached = await reachReady();
  const frames = [...reached.socket.frames];
  reached.connection.stop();

  expect(reached.connection.send(LOAD_CHAT)).toBe(false);
  expect(reached.socket.frames).toEqual(frames);
});

test("send_on_a_closing_socket_writes_nothing_and_answers_false", async () => {
  const reached = await reachReady();
  const frames = [...reached.socket.frames];
  reached.socket.readyState = 2;

  expect(reached.connection.send(LOAD_CHAT)).toBe(false);
  expect(reached.socket.frames).toEqual(frames);
});

test("send_whose_write_throws_answers_false_and_says_so", async () => {
  const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
  const reached = await reachReady();
  reached.socket.throwOnSend = true;

  expect(reached.connection.send(LOAD_CHAT)).toBe(false);
  expect(warn).toHaveBeenCalled();
});

test("send_while_connecting_is_not_replayed_once_ready", async () => {
  const reached = await reachConnecting();
  expect(reached.connection.send(LOAD_CHAT)).toBe(false);

  reached.socket.open();
  reached.socket.receive(WELCOME);
  expect(lastStatus(reached.notices)).toBe("ready");

  expect(reached.socket.frames).not.toContain(JSON.stringify(LOAD_CHAT));
});
