/**
 * Long-lived "driver host". Owns one WebDriver session and one side-channel
 * WS connection to the daemon, and accepts JSON commands over a local HTTP
 * socket on `127.0.0.1:47999`.
 *
 * Usage: `pnpm host` (or `pnpm host:build` to force a debug rebuild first).
 * Drive it via `pnpm exec rt-e2e <subcommand>` from another terminal.
 */
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { mkdir, writeFile } from "node:fs/promises";
import { dirname, isAbsolute, resolve } from "node:path";

import {
  startDriver,
  stopDriver,
  type DriverHandle,
  REPO_ROOT,
} from "./driver.js";
import { DaemonWsClient } from "./ws-client.js";
import type { ClientMessage } from "./types.js";

export const HOST_PORT = 47999;

interface HostState {
  driver: DriverHandle | null;
  ws: DaemonWsClient | null;
  recentDaemonMessages: { ts: number; payload: unknown }[];
}

const MAX_RECENT_MESSAGES = 100;

const state: HostState = {
  driver: null,
  ws: null,
  recentDaemonMessages: [],
};

interface JsonRequest<T = unknown> {
  cmd: string;
  args?: T;
}

type Handler = (
  args: Record<string, unknown>,
) => Promise<unknown>;

const handlers: Record<string, Handler> = {
  status: async () => ({
    driver: state.driver !== null,
    ws: state.ws !== null,
    ws_port: state.ws?.port ?? null,
    recent_daemon_messages: state.recentDaemonMessages.length,
    pid: process.pid,
  }),

  start: async (args) => {
    if (state.driver) {
      return { ok: true, note: "already started" };
    }
    const forceFrontend = Boolean(args["force_build"]);
    const env = (args["env"] as Record<string, string> | undefined) ?? {};
    state.driver = await startDriver({
      forceFrontend,
      env,
      sessionTimeoutMs: 30_000,
    });
    // WS side-channel is best-effort: when the WebView is broken and never
    // calls `ensure_daemon_started`, no daemon comes up and the connect
    // would crash the host. We log the failure and keep the WebDriver
    // session alive so the operator can still diagnose via screenshots/eval.
    try {
      state.ws = await DaemonWsClient.open({ waitTimeoutMs: 8_000 });
      state.ws.onMessage((msg) => {
        state.recentDaemonMessages.push({ ts: Date.now(), payload: msg });
        if (state.recentDaemonMessages.length > MAX_RECENT_MESSAGES) {
          state.recentDaemonMessages.splice(
            0,
            state.recentDaemonMessages.length - MAX_RECENT_MESSAGES,
          );
        }
      });
    } catch (err) {
      // eslint-disable-next-line no-console
      console.warn(
        `[host] ws connect failed (continuing without side-channel): ${
          err instanceof Error ? err.message : String(err)
        }`,
      );
      state.ws = null;
    }
    return { ok: true, ws_port: state.ws?.port ?? null };
  },

  stop: async () => {
    if (state.ws) {
      await state.ws.close();
      state.ws = null;
    }
    if (state.driver) {
      await stopDriver(state.driver);
      state.driver = null;
    }
    state.recentDaemonMessages.length = 0;
    return { ok: true };
  },

  shutdown: async () => {
    void (async () => {
      try {
        await handlers["stop"]?.({});
      } finally {
        setTimeout(() => process.exit(0), 50);
      }
    })();
    return { ok: true, note: "shutting down" };
  },

  screenshot: async (args) => {
    requireDriver();
    const target = String(args["path"] ?? ".tmp/screenshot.png");
    const abs = isAbsolute(target) ? target : resolve(REPO_ROOT, target);
    await mkdir(dirname(abs), { recursive: true });
    const png = await state.driver!.browser.takeScreenshot();
    await writeFile(abs, Buffer.from(png, "base64"));
    return { ok: true, path: abs };
  },

  click: async (args) => {
    requireDriver();
    const selector = String(args["selector"] ?? "");
    if (!selector) throw new Error("missing 'selector'");
    const el = await state.driver!.browser.$(selector);
    await el.click();
    return { ok: true };
  },

  type: async (args) => {
    requireDriver();
    const selector = String(args["selector"] ?? "");
    const text = String(args["text"] ?? "");
    if (!selector) throw new Error("missing 'selector'");
    const el = await state.driver!.browser.$(selector);
    await el.setValue(text);
    return { ok: true };
  },

  wait: async (args) => {
    requireDriver();
    const selector = String(args["selector"] ?? "");
    const timeout = Number(args["timeout_ms"] ?? 5_000);
    if (!selector) throw new Error("missing 'selector'");
    const el = await state.driver!.browser.$(selector);
    await el.waitForExist({ timeout });
    return { ok: true };
  },

  eval: async (args) => {
    requireDriver();
    const expr = String(args["expression"] ?? "");
    if (!expr) throw new Error("missing 'expression'");
    // WebDriver `executeScript` returns the value of the script.
    // Wrap in a return statement so callers can pass plain expressions.
    const wrapped = `return (function () { return (${expr}); })();`;
    const result: unknown = await state.driver!.browser.execute(wrapped);
    return { ok: true, result };
  },

  "dump-html": async (args) => {
    requireDriver();
    const target = String(args["path"] ?? ".tmp/dump.html");
    const abs = isAbsolute(target) ? target : resolve(REPO_ROOT, target);
    await mkdir(dirname(abs), { recursive: true });
    const html = await state.driver!.browser.execute(
      "return document.documentElement.outerHTML;",
    );
    await writeFile(abs, String(html), "utf8");
    return { ok: true, path: abs };
  },

  "ws-send": async (args) => {
    requireWs();
    const message = args["message"] as ClientMessage | undefined;
    if (!message || typeof message !== "object" || typeof message.type !== "string") {
      throw new Error("missing or malformed 'message' (expected ClientMessage)");
    }
    state.ws!.send(message);
    return { ok: true };
  },

  "ws-recent": async (args) => {
    const limit = Number(args["limit"] ?? 20);
    const slice = state.recentDaemonMessages.slice(-limit);
    return { ok: true, messages: slice };
  },
};

function requireDriver(): void {
  if (!state.driver) throw new Error("driver not started; call /start first");
}
function requireWs(): void {
  if (!state.ws) throw new Error("ws not connected; call /start first");
}

async function readBody(req: IncomingMessage): Promise<string> {
  const chunks: Buffer[] = [];
  for await (const chunk of req) {
    chunks.push(chunk as Buffer);
  }
  return Buffer.concat(chunks).toString("utf8");
}

function sendJson(res: ServerResponse, status: number, body: unknown): void {
  const payload = JSON.stringify(body);
  res.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
    "content-length": Buffer.byteLength(payload),
  });
  res.end(payload);
}

async function handleRequest(
  req: IncomingMessage,
  res: ServerResponse,
): Promise<void> {
  if (req.method !== "POST") {
    sendJson(res, 405, { ok: false, error: "POST only" });
    return;
  }
  let body: JsonRequest<Record<string, unknown>>;
  try {
    body = JSON.parse(await readBody(req)) as JsonRequest<
      Record<string, unknown>
    >;
  } catch (err) {
    sendJson(res, 400, {
      ok: false,
      error: `bad request body: ${err instanceof Error ? err.message : String(err)}`,
    });
    return;
  }
  const handler = handlers[body.cmd];
  if (!handler) {
    sendJson(res, 404, { ok: false, error: `unknown cmd: ${body.cmd}` });
    return;
  }
  try {
    const result = await handler(body.args ?? {});
    sendJson(res, 200, result);
  } catch (err) {
    sendJson(res, 500, {
      ok: false,
      error: err instanceof Error ? err.message : String(err),
    });
  }
}

async function main(): Promise<void> {
  const forceBuild = process.argv.includes("--build");
  // eslint-disable-next-line no-console
  console.log(`[host] starting (pid ${process.pid})…`);

  // Auto-start the driver+ws on boot so the user only has one command.
  await handlers["start"]?.({ force_build: forceBuild });

  const server = createServer((req, res) => {
    void handleRequest(req, res);
  });
  server.listen(HOST_PORT, "127.0.0.1", () => {
    // eslint-disable-next-line no-console
    console.log(`[host] listening on http://127.0.0.1:${HOST_PORT}`);
    // eslint-disable-next-line no-console
    console.log(`[host] ws connected to daemon on port ${state.ws?.port}`);
    // eslint-disable-next-line no-console
    console.log("[host] use `pnpm exec rt-e2e <subcommand>` from another terminal.");
  });

  const shutdown = async (): Promise<void> => {
    // eslint-disable-next-line no-console
    console.log("[host] shutting down…");
    try {
      await handlers["stop"]?.({});
    } catch (err) {
      // eslint-disable-next-line no-console
      console.error("[host] shutdown error:", err);
    }
    server.close(() => process.exit(0));
  };

  process.on("SIGINT", () => void shutdown());
  process.on("SIGTERM", () => void shutdown());
  process.on("SIGBREAK", () => void shutdown());
}

void main();
