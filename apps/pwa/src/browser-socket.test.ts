import { expect, test } from "vitest";

import { openWebSocket } from "./browser-socket";
import {
  PROTOCOL_VERSION,
  SUPPORTED_PROTOCOL_VERSIONS,
  type ClientMessage,
  type DaemonMessage,
} from "./protocol";
import { createConnection } from "./socket";
import { memoryTokenStore } from "./token-store";

const WELCOME: DaemonMessage = {
  type: "welcome",
  protocol_version: PROTOCOL_VERSION,
  supported_versions: [...SUPPORTED_PROTOCOL_VERSIONS],
  features: [],
  daemon_id: "daemon-1",
  origin: "remote",
};

const LOAD_CHAT: ClientMessage = { type: "load_chat", session_id: "session-1" };

/** A stand-in for the browser's `WebSocket`: a mutable state, and the frames it was sent. */
class FakeBrowserSocket {
  static readonly opened: FakeBrowserSocket[] = [];

  readonly frames: string[] = [];
  /** `WebSocket.CONNECTING`, until a test says otherwise. */
  readyState = 0;
  onopen: ((event: unknown) => void) | null = null;
  onmessage: ((event: { data: unknown }) => void) | null = null;
  onclose: ((event: unknown) => void) | null = null;
  onerror: ((event: unknown) => void) | null = null;

  constructor(readonly url: string) {
    FakeBrowserSocket.opened.push(this);
  }

  send(data: string): void {
    this.frames.push(data);
  }

  close(): void {
    this.readyState = 3;
  }
}

// The wrapper reads only the members the fake has, so it stands in for the
// DOM type here.
const Fake = FakeBrowserSocket as unknown as new (url: string) => WebSocket;

function lastOpened(): FakeBrowserSocket {
  const socket = FakeBrowserSocket.opened.at(-1);
  if (!socket) {
    throw new Error("no socket was opened");
  }
  return socket;
}

async function flush(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
}

test("the_wrapper_reports_the_real_sockets_ready_state", () => {
  const wrapper = openWebSocket("wss://host.tail1234.ts.net:8787/ws", Fake);
  const socket = lastOpened();

  expect(wrapper.readyState).toBe(0);
  socket.readyState = 1;
  expect(wrapper.readyState).toBe(1);
  socket.readyState = 3;
  expect(wrapper.readyState).toBe(3);
});

test("a_connection_sends_through_the_real_wrapper_once_open", async () => {
  const connection = createConnection({
    url: "wss://host.tail1234.ts.net:8787/ws",
    openSocket: (url) => openWebSocket(url, Fake),
    setTimeout: (callback, delayMs) => setTimeout(callback, delayMs),
    clearTimeout: (handle) => {
      clearTimeout(handle);
    },
    visibility: { isHidden: () => false, onChange: () => () => undefined },
    tokenStore: memoryTokenStore("device-token"),
    reloadApp: () => undefined,
  });
  connection.start();
  await flush();
  const socket = lastOpened();
  socket.readyState = 1;
  socket.onopen?.({});
  socket.onmessage?.({ data: JSON.stringify(WELCOME) });

  expect(connection.send(LOAD_CHAT)).toBe(true);
  expect(JSON.parse(socket.frames.at(-1) ?? "null")).toEqual(LOAD_CHAT);
  connection.stop();
});
