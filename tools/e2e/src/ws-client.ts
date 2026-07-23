import { setTimeout as delay } from "node:timers/promises";
import WebSocket from "ws";
import {
  PROTOCOL_VERSION,
  readClientId,
  readHandshake,
  type DaemonHandshake,
  waitForHandshake,
} from "./handshake.js";
import type { ClientMessage, DaemonMessage } from "./types.js";

export type MessageListener = (msg: DaemonMessage) => void;

/**
 * Side-channel WebSocket client used by the harness to send `ClientMessage`s
 * to the daemon directly (e.g. `add_repo`) without going through the UI.
 *
 * The Tauri app's own client at `apps/tauri-app/src/api.ts` is the production
 * path; this client coexists with it. The daemon broadcasts state to every
 * connected client, so changes made here propagate to the UI.
 */
export class DaemonWsClient {
  private socket: WebSocket | null = null;
  private listeners = new Set<MessageListener>();
  private welcomed = false;

  constructor(
    private readonly handshake: DaemonHandshake,
    private readonly clientId: string | null,
  ) {}

  static async open(opts: {
    waitTimeoutMs?: number;
  } = {}): Promise<DaemonWsClient> {
    const hs = opts.waitTimeoutMs
      ? await waitForHandshake({ timeoutMs: opts.waitTimeoutMs })
      : await readHandshake();
    // Share the UI's client_id so create_tab lands in the layout the UI
    // renders (tab layouts are keyed per client_id).
    const client = new DaemonWsClient(hs, await readClientId());
    await client.connect();
    return client;
  }

  get port(): number {
    return this.handshake.port;
  }

  private connect(): Promise<void> {
    return new Promise((resolve, reject) => {
      const url = `ws://127.0.0.1:${this.handshake.port}/ws`;
      const socket = new WebSocket(url);
      socket.on("open", () => {
        this.socket = socket;
        this.send({
          type: "hello",
          protocol_version: PROTOCOL_VERSION,
          auth_token: this.handshake.auth_token,
          ...(this.clientId ? { client_id: this.clientId } : {}),
        });
      });
      socket.on("message", (data: Buffer) => {
        let msg: DaemonMessage;
        try {
          msg = JSON.parse(data.toString("utf8")) as DaemonMessage;
        } catch (err) {
          reject(new Error(`malformed message from daemon: ${String(err)}`));
          return;
        }
        if (!this.welcomed) {
          if (msg.type === "welcome") {
            this.welcomed = true;
            resolve();
          } else if (msg.type === "auth_failed") {
            reject(
              new Error(
                `daemon rejected hello: ${(msg as { reason?: string }).reason ?? "(no reason)"}`,
              ),
            );
            return;
          }
        }
        for (const l of this.listeners) l(msg);
      });
      socket.on("error", (err: Error) => {
        if (!this.welcomed) reject(err);
      });
      socket.on("close", () => {
        this.socket = null;
      });
    });
  }

  send(msg: ClientMessage): void {
    if (!this.socket) {
      throw new Error("ws client not connected");
    }
    this.socket.send(JSON.stringify(msg));
  }

  onMessage(listener: MessageListener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  /**
   * Wait for a daemon message matching `predicate`. Resolves with the first
   * matching message; rejects on timeout.
   */
  waitFor<T extends DaemonMessage = DaemonMessage>(
    predicate: (msg: DaemonMessage) => msg is T,
    opts: { timeoutMs?: number } = {},
  ): Promise<T> {
    const timeoutMs = opts.timeoutMs ?? 5_000;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        unsubscribe();
        reject(new Error(`waitFor timed out after ${timeoutMs}ms`));
      }, timeoutMs);
      const unsubscribe = this.onMessage((msg) => {
        if (!predicate(msg)) return;
        clearTimeout(timer);
        unsubscribe();
        resolve(msg);
      });
    });
  }

  async close(): Promise<void> {
    if (!this.socket) return;
    const sock = this.socket;
    this.socket = null;
    sock.close();
    // Give the close handshake a moment so the daemon prunes us cleanly.
    await delay(50);
  }
}
