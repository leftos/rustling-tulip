import { invoke } from "@tauri-apps/api/core";
import {
  PROTOCOL_VERSION,
  type ClientMessage,
  type DaemonMessage,
} from "./types";

interface DaemonHandshake {
  protocol_version: number;
  port: number;
  auth_token: string;
  pid: number;
}

export interface DaemonClient {
  send(msg: ClientMessage): void;
  onMessage(cb: (msg: DaemonMessage) => void): () => void;
  onConnectionChange(cb: (state: ConnectionState) => void): () => void;
  state(): ConnectionState;
  close(): void;
}

export type ConnectionState =
  | { kind: "connecting" }
  | { kind: "open" }
  | { kind: "closed"; reason: string }
  | { kind: "auth_failed"; reason: string };

export async function ensureDaemonStarted(): Promise<DaemonHandshake> {
  return await invoke<DaemonHandshake>("ensure_daemon_started");
}

export async function pickDirectory(
  defaultPath?: string,
): Promise<string | null> {
  return await invoke<string | null>("pick_directory", {
    defaultPath: defaultPath ?? null,
  });
}

export function connectDaemon(handshake: DaemonHandshake): DaemonClient {
  const url = `ws://127.0.0.1:${handshake.port}/ws`;
  const ws = new WebSocket(url);
  const messageCbs = new Set<(msg: DaemonMessage) => void>();
  const stateCbs = new Set<(s: ConnectionState) => void>();
  let state: ConnectionState = { kind: "connecting" };

  const setState = (next: ConnectionState) => {
    state = next;
    for (const cb of stateCbs) cb(next);
  };

  ws.addEventListener("open", () => {
    ws.send(
      JSON.stringify({
        type: "hello",
        protocol_version: PROTOCOL_VERSION,
        auth_token: handshake.auth_token,
      } satisfies ClientMessage),
    );
    setState({ kind: "open" });
  });

  ws.addEventListener("message", (ev) => {
    let parsed: DaemonMessage;
    try {
      parsed = JSON.parse(ev.data) as DaemonMessage;
    } catch (err) {
      console.error("malformed daemon message", err);
      return;
    }
    if (parsed.type === "auth_failed") {
      setState({ kind: "auth_failed", reason: parsed.reason });
    }
    for (const cb of messageCbs) cb(parsed);
  });

  ws.addEventListener("close", (ev) => {
    if (state.kind !== "auth_failed") {
      setState({
        kind: "closed",
        reason: ev.reason || `code ${ev.code}`,
      });
    }
  });

  ws.addEventListener("error", () => {
    if (ws.readyState === WebSocket.CLOSED && state.kind !== "auth_failed") {
      setState({ kind: "closed", reason: "socket error" });
    }
  });

  return {
    send(msg) {
      if (ws.readyState !== WebSocket.OPEN) {
        console.warn("send while not open", msg.type);
        return;
      }
      ws.send(JSON.stringify(msg));
    },
    onMessage(cb) {
      messageCbs.add(cb);
      return () => messageCbs.delete(cb);
    },
    onConnectionChange(cb) {
      stateCbs.add(cb);
      cb(state);
      return () => stateCbs.delete(cb);
    },
    state() {
      return state;
    },
    close() {
      ws.close();
    },
  };
}

/**
 * Request the persisted scrollback for a session and resolve with the
 * decoded payload. Times out after 2 s if the daemon never answers (e.g.
 * because the session was already removed) — the terminal still attaches
 * after the promise resolves.
 *
 * Listens on the `rt:scrollback` window event (dispatched by App.tsx's
 * message router) so this works from any component without piercing the
 * client's onMessage subscription.
 */
export function loadScrollback(
  client: DaemonClient,
  sessionId: string,
): Promise<{ data_b64: string; truncated: boolean } | null> {
  return new Promise((resolve) => {
    const handler = (ev: Event) => {
      const detail = (ev as CustomEvent<DaemonMessage>).detail;
      if (detail.type !== "scrollback" || detail.session_id !== sessionId) {
        return;
      }
      cleanup();
      resolve({ data_b64: detail.data_b64, truncated: detail.truncated });
    };
    const timer = window.setTimeout(() => {
      cleanup();
      resolve(null);
    }, 2000);
    const cleanup = () => {
      window.removeEventListener("rt:scrollback", handler);
      window.clearTimeout(timer);
    };
    window.addEventListener("rt:scrollback", handler);
    client.send({ type: "load_scrollback", session_id: sessionId });
  });
}

export function bytesToBase64(bytes: Uint8Array): string {
  let bin = "";
  for (let i = 0; i < bytes.byteLength; i++) {
    bin += String.fromCharCode(bytes[i] ?? 0);
  }
  return btoa(bin);
}

export function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}
