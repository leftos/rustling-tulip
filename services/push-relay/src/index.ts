// The push relay (docs/plans/push-relay.md): to a daemon, one more RFC 8030 Web Push
// service; each accepted push goes on to APNs as a mutable-content alert carrying the
// ciphertext, so only this relay holds the Apple key.

import { sendApns, type ApnsEnv, type Environment, type Kv } from "./apns";
import { sha256Hex, verifyVapid } from "./vapid";

/** The part of a Rate Limiting binding the relay uses (tests pass stubs). */
export interface Limiter {
  limit(options: { key: string }): Promise<{ success: boolean }>;
}

export interface Env extends ApnsEnv {
  DENY_LIST: Kv;
  RELAY_STATE: Kv;
  TOKEN_LIMITER: Limiter;
  SENDER_LIMITER: Limiter;
  IP_LIMITER: Limiter;
  RELAY_ORIGIN: string;
  RELAY_ENABLED: string;
}

export interface Deps {
  /** Milliseconds since the Unix epoch. */
  now: () => number;
  fetch: typeof fetch;
}

const MAX_BODY_BYTES = 3072;
const MAX_TOKEN_CHARS = 512;
const DAILY_CAP = 300;
const DAY_COUNTER_TTL_SECONDS = 2 * 24 * 3600;
const PLATFORM = "apns";

type Route = { kind: "health" } | { kind: "apns"; environment: Environment; token: string };

/** What the push's log line carries besides its status: never a token, body or header value. */
interface LogContext {
  sender: string | null;
  relayFault?: string;
  /** What KV writes failed while the push went on: `day`, `apns-jwt`. */
  kvErrors: string[];
}

function answer(status: number, reason: string, retryAfter?: string): Response {
  const headers = new Headers({ "content-type": "text/plain; charset=utf-8" });
  if (retryAfter !== undefined) headers.set("Retry-After", retryAfter);
  return new Response(reason, { status, headers });
}

function route(request: Request): Route | Response {
  const path = new URL(request.url).pathname;
  if (path === "/health") return request.method === "GET" ? { kind: "health" } : answer(405, "method not allowed");
  const match = /^\/v1\/apns\/([^/]+)\/([^/]*)$/.exec(path);
  const environment = match?.[1];
  if (environment !== "production" && environment !== "sandbox") return answer(404, "not found");
  if (request.method !== "POST") return answer(405, "method not allowed");
  return { kind: "apns", environment, token: match?.[2] ?? "" };
}

function validToken(token: string): boolean {
  return token.length > 0 && token.length <= MAX_TOKEN_CHARS && /^[0-9a-fA-F]+$/.test(token);
}

async function denied(env: Env, entry: string): Promise<boolean> {
  return (await env.DENY_LIST.get(entry)) !== null;
}

async function readBody(request: Request): Promise<Uint8Array | Response> {
  const encoding = (request.headers.get("Content-Encoding") ?? "").trim().toLowerCase();
  if (encoding !== "aes128gcm") return answer(400, "content encoding is not aes128gcm");
  const body = new Uint8Array(await request.arrayBuffer());
  if (body.length === 0) return answer(400, "empty body");
  if (body.length > MAX_BODY_BYTES) return answer(413, "body over 3072 bytes");
  return body;
}

async function limited(request: Request, env: Env, senderHash: string, tokenHash: string): Promise<boolean> {
  const ip = request.headers.get("CF-Connecting-IP") ?? "unknown";
  const checks: [Limiter, string][] = [
    [env.IP_LIMITER, ip],
    [env.SENDER_LIMITER, senderHash],
    [env.TOKEN_LIMITER, tokenHash],
  ];
  for (const [limiter, key] of checks) {
    if (!(await limiter.limit({ key })).success) return true;
  }
  return false;
}

/**
 * Counts the push against the token's UTC-day cap; a 429 once the day's 300 are spent. A failed
 * counter write (KV takes one write per key a second) is noted in `ctx` and lets the push through.
 */
async function dailyCap(env: Env, tokenHash: string, nowMs: number, ctx: LogContext): Promise<Response | null> {
  const now = new Date(nowMs);
  const key = `day:${tokenHash}:${now.toISOString().slice(0, 10)}`;
  const count = Number.parseInt((await env.RELAY_STATE.get(key)) ?? "0", 10) || 0;
  if (count >= DAILY_CAP) {
    const midnight = Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate() + 1);
    return answer(429, "daily cap for this device reached", String(Math.ceil((midnight - nowMs) / 1000)));
  }
  try {
    await env.RELAY_STATE.put(key, String(count + 1), { expirationTtl: DAY_COUNTER_TTL_SECONDS });
  } catch {
    ctx.kvErrors.push("day");
  }
  return null;
}

/** The checks after the sender is known, in the designed order, then the APNs send. */
async function admit(request: Request, env: Env, deps: Deps, push: Route & { kind: "apns" }, ctx: LogContext): Promise<Response> {
  const body = await readBody(request);
  if (body instanceof Response) return body;
  // Hashed lowercased, so case variants of one hex token share its deny entry, limit and counter.
  const tokenHash = await sha256Hex(new TextEncoder().encode(push.token.toLowerCase()));
  if (await denied(env, `token:${tokenHash}`)) return answer(410, "device token is denied");
  if (await limited(request, env, ctx.sender ?? "", tokenHash)) return answer(429, "rate limited", "60");
  const capped = await dailyCap(env, tokenHash, deps.now(), ctx);
  if (capped !== null) return capped;
  const result = await sendApns(env, { ...push, body, headers: request.headers }, deps.now(), deps.fetch);
  if (result.relayFault !== undefined) ctx.relayFault = result.relayFault;
  if (result.kvError !== undefined) ctx.kvErrors.push(result.kvError);
  return answer(result.status, result.status === 201 ? "" : "push not delivered", result.retryAfter);
}

async function relayPush(request: Request, env: Env, deps: Deps, push: Route & { kind: "apns" }, ctx: LogContext): Promise<Response> {
  if (!validToken(push.token)) return answer(400, "device token is not hexadecimal");
  const vapid = await verifyVapid(request.headers.get("Authorization"), env.RELAY_ORIGIN, Math.floor(deps.now() / 1000));
  if (!vapid.ok) return answer(403, vapid.reason);
  const senderHash = await sha256Hex(vapid.key);
  ctx.sender = senderHash;
  if (await denied(env, `sender:${senderHash}`)) return answer(403, "sender is denied");
  return admit(request, env, deps, push, ctx);
}

function logPush(status: number, ctx: LogContext): void {
  const line: Record<string, unknown> = { status, platform: PLATFORM, sender: ctx.sender?.slice(0, 8) ?? null };
  if (ctx.relayFault !== undefined) line.relay_fault = ctx.relayFault;
  if (ctx.kvErrors.length > 0) line.kv_error = ctx.kvErrors.join(",");
  console.log(JSON.stringify(line));
}

/** Answers one request; `deps` carries the clock and the outbound `fetch` so tests pass their own. */
export async function handle(request: Request, env: Env, deps: Deps): Promise<Response> {
  if (env.RELAY_ENABLED !== "true") return answer(503, "relay disabled", "300");
  const matched = route(request);
  if (matched instanceof Response) return matched;
  if (matched.kind === "health") return answer(200, "ok");
  const ctx: LogContext = { sender: null, kvErrors: [] };
  const response = await relayPush(request, env, deps, matched, ctx);
  logPush(response.status, ctx);
  return response;
}

export default {
  fetch(request: Request, env: Env): Promise<Response> {
    return handle(request, env, { now: Date.now, fetch: globalThis.fetch.bind(globalThis) });
  },
};
