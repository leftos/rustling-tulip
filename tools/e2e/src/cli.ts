#!/usr/bin/env node
/**
 * Thin CLI client. Translates `rt-e2e <cmd> [args…]` into a JSON POST to the
 * long-lived host (`src/host.ts`) on `127.0.0.1:47999`.
 *
 * Subcommands:
 *   status                                  show host state
 *   screenshot [path]                       save PNG (default .tmp/screenshot.png)
 *   click <selector>                        WebDriver click
 *   type <selector> <text>                  WebDriver setValue
 *   wait <selector> [timeout_ms]            wait for element to exist
 *   eval <expression>                       run JS in the renderer, print result
 *   dump-html [path]                        save outerHTML (default .tmp/dump.html)
 *   ws-send <json>                          send a ClientMessage to the daemon
 *   ws-recent [limit]                       print recent DaemonMessage history
 *   shutdown                                stop driver, close ws, exit host
 *
 * Exit code 0 on `{ok: true}`, 1 otherwise.
 */
import { request } from "node:http";

import { HOST_PORT } from "./host.js";

interface RpcRequest {
  cmd: string;
  args: Record<string, unknown>;
}

function postJson(req: RpcRequest): Promise<{ status: number; body: unknown }> {
  return new Promise((resolveFn, rejectFn) => {
    const payload = Buffer.from(JSON.stringify(req), "utf8");
    const r = request(
      {
        hostname: "127.0.0.1",
        port: HOST_PORT,
        path: "/",
        method: "POST",
        headers: {
          "content-type": "application/json; charset=utf-8",
          "content-length": payload.byteLength,
        },
      },
      (res) => {
        const chunks: Buffer[] = [];
        res.on("data", (c: Buffer) => chunks.push(c));
        res.on("end", () => {
          const text = Buffer.concat(chunks).toString("utf8");
          let body: unknown;
          try {
            body = JSON.parse(text);
          } catch {
            body = text;
          }
          resolveFn({ status: res.statusCode ?? 0, body });
        });
      },
    );
    r.on("error", rejectFn);
    r.write(payload);
    r.end();
  });
}

function parsePositional(argv: string[]): RpcRequest | string {
  const [cmd, ...rest] = argv;
  if (!cmd) {
    return "missing subcommand. See `rt-e2e --help`.";
  }
  switch (cmd) {
    case "--help":
    case "-h":
    case "help":
      return "help";
    case "status":
      return { cmd: "status", args: {} };
    case "shutdown":
      return { cmd: "shutdown", args: {} };
    case "screenshot": {
      const args: Record<string, unknown> = {};
      if (rest[0]) args["path"] = rest[0];
      return { cmd: "screenshot", args };
    }
    case "click":
      if (!rest[0]) return "click requires <selector>";
      return { cmd: "click", args: { selector: rest[0] } };
    case "type":
      if (!rest[0] || rest[1] === undefined)
        return "type requires <selector> <text>";
      return { cmd: "type", args: { selector: rest[0], text: rest.slice(1).join(" ") } };
    case "wait": {
      if (!rest[0]) return "wait requires <selector> [timeout_ms]";
      const args: Record<string, unknown> = { selector: rest[0] };
      if (rest[1] !== undefined) args["timeout_ms"] = Number(rest[1]);
      return { cmd: "wait", args };
    }
    case "eval":
      if (!rest[0]) return "eval requires <expression>";
      return { cmd: "eval", args: { expression: rest.join(" ") } };
    case "dump-html": {
      const args: Record<string, unknown> = {};
      if (rest[0]) args["path"] = rest[0];
      return { cmd: "dump-html", args };
    }
    case "ws-send": {
      if (!rest[0]) return "ws-send requires <json>";
      let parsed: unknown;
      try {
        parsed = JSON.parse(rest.join(" "));
      } catch (err) {
        return `ws-send: invalid JSON: ${String(err)}`;
      }
      return { cmd: "ws-send", args: { message: parsed } };
    }
    case "ws-recent": {
      const args: Record<string, unknown> = {};
      if (rest[0]) args["limit"] = Number(rest[0]);
      return { cmd: "ws-recent", args };
    }
    default:
      return `unknown subcommand: ${cmd}`;
  }
}

const HELP = `rt-e2e — drive the running e2e host

  status
  screenshot [path]
  click <selector>
  type <selector> <text…>
  wait <selector> [timeout_ms]
  eval <expression>
  dump-html [path]
  ws-send <json>
  ws-recent [limit]
  shutdown
`;

async function main(): Promise<void> {
  const parsed = parsePositional(process.argv.slice(2));
  if (typeof parsed === "string") {
    if (parsed === "help") {
      process.stdout.write(HELP);
      process.exit(0);
    }
    process.stderr.write(`error: ${parsed}\n\n${HELP}`);
    process.exit(2);
  }
  let resp: { status: number; body: unknown };
  try {
    resp = await postJson(parsed);
  } catch (err) {
    process.stderr.write(
      `cannot reach host on http://127.0.0.1:${HOST_PORT} — is \`pnpm host\` running?\n` +
        `(${err instanceof Error ? err.message : String(err)})\n`,
    );
    process.exit(1);
  }
  process.stdout.write(`${JSON.stringify(resp.body, null, 2)}\n`);
  const ok =
    resp.status >= 200 &&
    resp.status < 300 &&
    typeof resp.body === "object" &&
    resp.body !== null &&
    (resp.body as { ok?: unknown }).ok === true;
  process.exit(ok ? 0 : 1);
}

void main();
