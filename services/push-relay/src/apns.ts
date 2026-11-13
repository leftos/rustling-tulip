// The APNs sender: the cached provider token, the mutable-content alert carrying
// the ciphertext, and the map from APNs' answer to the RFC 8030 status the daemon reads.

import { b64urlDecode, b64urlEncode } from "./vapid";

/** The part of a Workers KV namespace the relay uses (tests pass an in-memory one). */
export interface Kv {
  get(key: string): Promise<string | null>;
  put(key: string, value: string, options?: { expirationTtl?: number }): Promise<void>;
}

export interface ApnsEnv {
  RELAY_STATE: Kv;
  APNS_TOPIC: string;
  APNS_KEY_P8: string;
  APNS_KEY_ID: string;
  APNS_TEAM_ID: string;
}

export type Environment = "production" | "sandbox";

/** The only hosts the relay ever posts to; nothing in a request names a host. */
const HOSTS: Record<Environment, string> = {
  production: "api.push.apple.com",
  sandbox: "api.sandbox.push.apple.com",
};

const JWT_KEY = "apns-jwt";
/** A provider token is reused while under 50 minutes old (Apple refuses one older than an hour). */
const JWT_REUSE_SECONDS = 50 * 60;
/** The cached token's KV lifetime: past the reuse window, so a stale one is never served from KV alone. */
const JWT_KV_TTL_SECONDS = 60 * 60;

export interface ApnsPush {
  environment: Environment;
  token: string;
  body: Uint8Array;
  /** The daemon's request headers: `TTL`, `Urgency` and `Topic` are read. */
  headers: Headers;
}

export interface ApnsResult {
  status: number;
  retryAfter?: string;
  /** APNs' reason when the relay's own credential was refused (logged; the relay's fault). */
  relayFault?: string;
  /** The KV key whose write failed during the send (the push went on regardless). */
  kvError?: string;
}

const GONE_REASONS = new Set(["BadDeviceToken", "DeviceTokenNotForTopic"]);
const CREDENTIAL_REASONS = new Set(["InvalidProviderToken", "ExpiredProviderToken"]);
const STATUS_MAP = new Map<number, ApnsResult>([
  [200, { status: 201 }],
  [410, { status: 410 }],
  [413, { status: 413 }],
  [429, { status: 429, retryAfter: "60" }],
  [500, { status: 503, retryAfter: "60" }],
  [503, { status: 503, retryAfter: "60" }],
]);

/** The relay's answer to the daemon for APNs' `status` and `reason`. */
export function mapApnsAnswer(status: number, reason: string): ApnsResult {
  if (status === 400 && GONE_REASONS.has(reason)) return { status: 410 };
  if (status === 403 && CREDENTIAL_REASONS.has(reason)) return { status: 502, relayFault: reason };
  return STATUS_MAP.get(status) ?? { status: 502 };
}

function utf8b64(value: unknown): string {
  return b64urlEncode(new TextEncoder().encode(JSON.stringify(value)));
}

function pkcs8(pem: string): Uint8Array<ArrayBuffer> | null {
  const body = pem.replace(/-----[^-]+-----/g, "").replace(/\s+/g, "");
  return b64urlDecode(body.replace(/\+/g, "-").replace(/\//g, "_"));
}

async function mintProviderToken(env: ApnsEnv, nowSeconds: number): Promise<string> {
  const der = pkcs8(env.APNS_KEY_P8);
  if (der === null) throw new Error("APNS_KEY_P8 is not a PEM key");
  const key = await crypto.subtle.importKey("pkcs8", der, { name: "ECDSA", namedCurve: "P-256" }, false, ["sign"]);
  const input = `${utf8b64({ alg: "ES256", kid: env.APNS_KEY_ID })}.${utf8b64({ iss: env.APNS_TEAM_ID, iat: nowSeconds })}`;
  const signature = await crypto.subtle.sign({ name: "ECDSA", hash: "SHA-256" }, key, new TextEncoder().encode(input));
  return `${input}.${b64urlEncode(new Uint8Array(signature))}`;
}

function cachedToken(text: string | null, nowSeconds: number): string | null {
  if (text === null) return null;
  try {
    const cached = JSON.parse(text) as { token?: unknown; iat?: unknown };
    const age = typeof cached.iat === "number" ? nowSeconds - cached.iat : -1;
    return typeof cached.token === "string" && age >= 0 && age < JWT_REUSE_SECONDS ? cached.token : null;
  } catch {
    return null;
  }
}

/**
 * The APNs provider token: the one cached in KV while under 50 minutes old, else a new one,
 * cached. A failed cache write still returns the new token, with `cacheFailed` set.
 * Throws only when a token cannot be minted (an unusable `.p8`).
 */
export async function providerToken(env: ApnsEnv, nowSeconds: number): Promise<{ token: string; cacheFailed: boolean }> {
  const cached = cachedToken(await env.RELAY_STATE.get(JWT_KEY), nowSeconds);
  if (cached !== null) return { token: cached, cacheFailed: false };
  const token = await mintProviderToken(env, nowSeconds);
  try {
    await env.RELAY_STATE.put(JWT_KEY, JSON.stringify({ token, iat: nowSeconds }), { expirationTtl: JWT_KV_TTL_SECONDS });
  } catch {
    return { token, cacheFailed: true };
  }
  return { token, cacheFailed: false };
}

function apnsHeaders(env: ApnsEnv, push: ApnsPush, jwt: string, nowSeconds: number): Headers {
  const ttl = Number.parseInt(push.headers.get("TTL") ?? "", 10);
  const urgency = (push.headers.get("Urgency") ?? "").trim().toLowerCase();
  const headers = new Headers({
    authorization: `bearer ${jwt}`,
    "apns-topic": env.APNS_TOPIC,
    "apns-push-type": "alert",
    "apns-priority": urgency === "high" ? "10" : "5",
    "apns-expiration": String(Number.isFinite(ttl) && ttl > 0 ? nowSeconds + ttl : 0),
    "content-type": "application/json",
  });
  const topic = push.headers.get("Topic");
  if (topic !== null && topic !== "") headers.set("apns-collapse-id", topic);
  return headers;
}

function alertBody(ciphertext: Uint8Array): string {
  return JSON.stringify({
    aps: {
      alert: { "title-loc-key": "PUSH_FALLBACK_TITLE", "loc-key": "PUSH_FALLBACK_BODY" },
      "mutable-content": 1,
    },
    e: b64urlEncode(ciphertext),
  });
}

async function reasonOf(response: Response): Promise<string> {
  try {
    const parsed = (await response.json()) as { reason?: unknown };
    return typeof parsed.reason === "string" ? parsed.reason : "";
  } catch {
    return "";
  }
}

/**
 * Sends `push` to APNs as a mutable-content alert carrying the ciphertext in `e`, and maps
 * the answer. A provider key that cannot sign is the relay's fault (502); an APNs that cannot
 * be reached is answered 503 with `Retry-After`.
 */
export async function sendApns(env: ApnsEnv, push: ApnsPush, nowMs: number, fetcher: typeof fetch): Promise<ApnsResult> {
  const nowSeconds = Math.floor(nowMs / 1000);
  let jwt: { token: string; cacheFailed: boolean };
  try {
    jwt = await providerToken(env, nowSeconds);
  } catch {
    return { status: 502, relayFault: "ProviderKeyUnusable" };
  }
  const kvError = jwt.cacheFailed ? { kvError: JWT_KEY } : {};
  const url = `https://${HOSTS[push.environment]}/3/device/${push.token}`;
  let response: Response;
  try {
    const headers = apnsHeaders(env, push, jwt.token, nowSeconds);
    response = await fetcher(url, { method: "POST", headers, body: alertBody(push.body) });
  } catch {
    return { status: 503, retryAfter: "60", ...kvError };
  }
  return { ...mapApnsAnswer(response.status, response.status === 200 ? "" : await reasonOf(response)), ...kvError };
}
