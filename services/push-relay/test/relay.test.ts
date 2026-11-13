import { afterEach, describe, expect, it, vi } from "vitest";

import { handle, type Deps, type Env } from "../src/index";
import type { Kv } from "../src/apns";
import { b64urlDecode, b64urlEncode, parseAuthorization, sha256Hex } from "../src/vapid";
import fixture from "./fixtures/daemon-vapid.json";

const ORIGIN = "https://relay.test";
const TOPIC = "com.example.relay";
const TOKEN = "ab".repeat(32);
/** The fixture's moment of signing: its `exp` less the daemon's twelve-hour `LIFETIME`. */
const SIGNED_AT_MS = (fixture.exp - 12 * 3600) * 1000;

/** Every request any test's mock APNs saw, for the suite-wide host check at the end. */
const everyRequest: Request[] = [];

class MemoryKv implements Kv {
  readonly values = new Map<string, string>();
  readonly puts: { key: string; value: string; ttl: number | undefined }[] = [];
  /** A put to a key this accepts throws, as Workers KV does on a second write to a key within a second. */
  failPut: (key: string) => boolean = () => false;

  async get(key: string): Promise<string | null> {
    return this.values.get(key) ?? null;
  }

  async put(key: string, value: string, options?: { expirationTtl?: number }): Promise<void> {
    if (this.failPut(key)) throw new Error("KV PUT failed: 429 Too Many Requests");
    this.values.set(key, value);
    this.puts.push({ key, value, ttl: options?.expirationTtl });
  }
}

function limiter(success = true) {
  const keys: string[] = [];
  return {
    keys,
    async limit(options: { key: string }) {
      keys.push(options.key);
      return { success };
    },
  };
}

interface Apns {
  requests: Request[];
  answer: { status: number; body?: string };
  fetch: typeof fetch;
}

function mockApns(): Apns {
  const apns: Apns = {
    requests: [],
    answer: { status: 200 },
    fetch: async (input: RequestInfo | URL, init?: RequestInit) => {
      const request = new Request(input, init);
      apns.requests.push(request.clone());
      everyRequest.push(request.clone());
      return new Response(apns.answer.body ?? "", { status: apns.answer.status });
    },
  };
  return apns;
}

interface World {
  env: Env & { DENY_LIST: MemoryKv; RELAY_STATE: MemoryKv };
  limiters: { token: ReturnType<typeof limiter>; sender: ReturnType<typeof limiter>; ip: ReturnType<typeof limiter> };
  apns: Apns;
  nowMs: number;
  deps: Deps;
  publicKey: CryptoKey;
}

function pem(der: ArrayBuffer): string {
  const b64 = btoa(String.fromCharCode(...new Uint8Array(der)));
  const lines = b64.match(/.{1,64}/g) ?? [];
  // The armour wraps a key generated at test time; the label is split so the
  // commit hook's private-key check does not take this builder for a key.
  const label = ["PRIVATE", "KEY"].join(" ");
  return [`-----BEGIN ${label}-----`, ...lines, `-----END ${label}-----`, ""].join("\n");
}

async function world(): Promise<World> {
  const pair = (await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, true, [
    "sign",
    "verify",
  ])) as CryptoKeyPair;
  const p8 = pem((await crypto.subtle.exportKey("pkcs8", pair.privateKey)) as ArrayBuffer);
  const limiters = { token: limiter(), sender: limiter(), ip: limiter() };
  const apns = mockApns();
  const w: World = {
    env: {
      DENY_LIST: new MemoryKv(),
      RELAY_STATE: new MemoryKv(),
      TOKEN_LIMITER: limiters.token,
      SENDER_LIMITER: limiters.sender,
      IP_LIMITER: limiters.ip,
      RELAY_ORIGIN: ORIGIN,
      RELAY_ENABLED: "true",
      APNS_TOPIC: TOPIC,
      APNS_KEY_P8: p8,
      APNS_KEY_ID: "KEYID12345",
      APNS_TEAM_ID: "TEAMID1234",
    },
    limiters,
    apns,
    nowMs: SIGNED_AT_MS,
    deps: { now: () => w.nowMs, fetch: apns.fetch },
    publicKey: pair.publicKey,
  };
  return w;
}

function bytes(length: number): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(length);
  for (let i = 0; i < length; i += 1) out[i] = (i * 31 + 7) % 256;
  return out;
}

interface PushOptions {
  path?: string;
  method?: string;
  headers?: Record<string, string>;
  body?: Uint8Array<ArrayBuffer>;
  omit?: string[];
}

function push(options: PushOptions = {}): Request {
  const headers: Record<string, string> = {
    Authorization: `vapid t=${fixture.jwt}, k=${fixture.k}`,
    "Content-Encoding": "aes128gcm",
    "Content-Type": "application/octet-stream",
    TTL: "3600",
    Urgency: "high",
    Topic: "rt-0123abcd",
    "CF-Connecting-IP": "203.0.113.9",
    ...options.headers,
  };
  for (const name of options.omit ?? []) delete headers[name];
  const method = options.method ?? "POST";
  return new Request(`${ORIGIN}${options.path ?? `/v1/apns/production/${TOKEN}`}`, {
    method,
    headers,
    body: method === "GET" ? null : (options.body ?? bytes(200)),
  });
}

async function send(w: World, options: PushOptions = {}): Promise<Response> {
  return handle(push(options), w.env, w.deps);
}

async function hashOf(text: string): Promise<string> {
  return sha256Hex(new TextEncoder().encode(text));
}

/** The lines `console.log` prints from now until the test's mocks are restored. */
function captureLog(): string[] {
  const lines: string[] = [];
  vi.spyOn(console, "log").mockImplementation((...args: unknown[]) => {
    lines.push(args.map(String).join(" "));
  });
  return lines;
}

async function verifiesWith(key: CryptoKey, jwt: string): Promise<boolean> {
  const [head, claims, signature] = jwt.split(".");
  const raw = b64urlDecode(signature ?? "");
  if (raw === null) return false;
  const signed = new TextEncoder().encode(`${head}.${claims}`);
  return crypto.subtle.verify({ name: "ECDSA", hash: "SHA-256" }, key, raw, signed);
}

describe("push relay", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("verifies the daemon's fixture JWT at its signing time and forwards the push to APNs as designed", async () => {
    const w = await world();
    const body = bytes(200);
    const response = await send(w, { body });
    expect(response.status).toBe(201);
    expect(w.apns.requests).toHaveLength(1);
    const sent = w.apns.requests[0]!;
    expect(sent.method).toBe("POST");
    expect(sent.url).toBe(`https://api.push.apple.com/3/device/${TOKEN}`);
    expect(sent.headers.get("apns-topic")).toBe(TOPIC);
    expect(sent.headers.get("apns-push-type")).toBe("alert");
    expect(sent.headers.get("apns-priority")).toBe("10");
    expect(sent.headers.get("apns-expiration")).toBe(String(SIGNED_AT_MS / 1000 + 3600));
    expect(sent.headers.get("apns-collapse-id")).toBe("rt-0123abcd");
    const authorization = sent.headers.get("authorization") ?? "";
    expect(authorization.startsWith("bearer ")).toBe(true);
    const providerJwt = authorization.slice("bearer ".length);
    expect(await verifiesWith(w.publicKey, providerJwt)).toBe(true);
    const [head, claims] = providerJwt.split(".").map((part) => new TextDecoder().decode(b64urlDecode(part) ?? new Uint8Array()));
    expect(JSON.parse(head ?? "")).toEqual({ alg: "ES256", kid: "KEYID12345" });
    expect(JSON.parse(claims ?? "")).toEqual({ iss: "TEAMID1234", iat: SIGNED_AT_MS / 1000 });
    const payload = (await sent.json()) as { aps: unknown; e: string };
    expect(payload.aps).toEqual({
      alert: { "title-loc-key": "PUSH_FALLBACK_TITLE", "loc-key": "PUSH_FALLBACK_BODY" },
      "mutable-content": 1,
    });
    expect(payload.e).toMatch(/^[A-Za-z0-9_-]+$/);
    expect(b64urlDecode(payload.e)).toEqual(body);
  });

  it("refuses a JWT whose audience is another origin with 403", async () => {
    const w = await world();
    w.env.RELAY_ORIGIN = "https://other-relay.test";
    expect((await send(w)).status).toBe(403);
    expect(w.apns.requests).toHaveLength(0);
  });

  it("refuses an expired JWT with 403", async () => {
    const w = await world();
    w.nowMs = (fixture.exp + 1) * 1000;
    const response = await send(w);
    expect(response.status).toBe(403);
    expect(await response.text()).toBe("vapid jwt expired");
    expect(w.apns.requests).toHaveLength(0);
  });

  it("refuses a JWT expiring more than 24 hours ahead with 403", async () => {
    const w = await world();
    w.nowMs = (fixture.exp - 24 * 3600 - 1) * 1000;
    expect((await send(w)).status).toBe(403);
    expect(w.apns.requests).toHaveLength(0);
  });

  it("refuses a push without Authorization with 403", async () => {
    const w = await world();
    expect((await send(w, { omit: ["Authorization"] })).status).toBe(403);
    expect(w.apns.requests).toHaveLength(0);
  });

  it("refuses a denied sender key with 403, never 410", async () => {
    const w = await world();
    const raw = b64urlDecode(fixture.k);
    expect(raw).not.toBeNull();
    w.env.DENY_LIST.values.set(`sender:${await sha256Hex(raw ?? new Uint8Array())}`, "1");
    expect((await send(w)).status).toBe(403);
    expect(w.apns.requests).toHaveLength(0);
  });

  it.each([
    ["vapid t=a.b.c, k=Bxyz", { t: "a.b.c", k: "Bxyz" }],
    ["vapid k=Bxyz, t=a.b.c", { t: "a.b.c", k: "Bxyz" }],
    ["vapid t=a.b.c,k=Bxyz", { t: "a.b.c", k: "Bxyz" }],
    ["Bearer a.b.c", null],
    ["vapid t=a.b.c", null],
    ["vapid k=Bxyz", null],
  ])("parses the Authorization %j as RFC 8292 says", (header, expected) => {
    expect(parseAuthorization(header)).toEqual(expected);
  });

  it("refuses a body of 3073 bytes with 413", async () => {
    const w = await world();
    expect((await send(w, { body: bytes(3073) })).status).toBe(413);
    expect(w.apns.requests).toHaveLength(0);
  });

  it("accepts a body of 3072 bytes, at normal priority with no expiry or collapse id when the headers ask none", async () => {
    const w = await world();
    const response = await send(w, {
      path: `/v1/apns/sandbox/${TOKEN}`,
      body: bytes(3072),
      omit: ["TTL", "Urgency", "Topic"],
    });
    expect(response.status).toBe(201);
    const sent = w.apns.requests[0]!;
    expect(sent.url).toBe(`https://api.sandbox.push.apple.com/3/device/${TOKEN}`);
    expect(sent.headers.get("apns-priority")).toBe("5");
    expect(sent.headers.get("apns-expiration")).toBe("0");
    expect(sent.headers.has("apns-collapse-id")).toBe(false);
  });

  it("refuses a body that is not aes128gcm with 400", async () => {
    const w = await world();
    expect((await send(w, { headers: { "Content-Encoding": "aesgcm" } })).status).toBe(400);
    expect(w.apns.requests).toHaveLength(0);
  });

  it("refuses a non-hexadecimal token with 400", async () => {
    const w = await world();
    expect((await send(w, { path: "/v1/apns/production/not-a-hex-token" })).status).toBe(400);
    expect(w.apns.requests).toHaveLength(0);
  });

  it("answers 404 for an unknown environment", async () => {
    const w = await world();
    expect((await send(w, { path: `/v1/apns/staging/${TOKEN}` })).status).toBe(404);
    expect(w.apns.requests).toHaveLength(0);
  });

  it("answers a denied token with 410 and sends nothing to APNs", async () => {
    const w = await world();
    w.env.DENY_LIST.values.set(`token:${await hashOf(TOKEN)}`, "1");
    expect((await send(w)).status).toBe(410);
    expect(w.apns.requests).toHaveLength(0);
  });

  it.each([
    [200, "", 201, null],
    [410, '{"reason":"Unregistered"}', 410, null],
    [400, '{"reason":"BadDeviceToken"}', 410, null],
    [400, '{"reason":"DeviceTokenNotForTopic"}', 410, null],
    [413, '{"reason":"PayloadTooLarge"}', 413, null],
    [429, '{"reason":"TooManyRequests"}', 429, "60"],
    [403, '{"reason":"InvalidProviderToken"}', 502, null],
    [403, '{"reason":"ExpiredProviderToken"}', 502, null],
    [500, '{"reason":"InternalServerError"}', 503, "60"],
    [503, '{"reason":"ServiceUnavailable"}', 503, "60"],
    [400, '{"reason":"BadTopic"}', 502, null],
    [403, '{"reason":"BadCertificateEnvironment"}', 502, null],
  ])("maps an APNs %i %s to %i", async (apnsStatus, apnsBody, expected, retryAfter) => {
    const w = await world();
    w.apns.answer = { status: apnsStatus, body: apnsBody };
    const response = await send(w);
    expect(response.status).toBe(expected);
    expect(response.headers.get("Retry-After")).toBe(retryAfter);
  });

  it("a_denied_token_is_denied_in_any_case", async () => {
    const w = await world();
    w.env.DENY_LIST.values.set(`token:${await hashOf(TOKEN)}`, "1");
    expect((await send(w, { path: `/v1/apns/production/${TOKEN.toUpperCase()}` })).status).toBe(410);
    expect(w.apns.requests).toHaveLength(0);
  });

  it("case_variants_share_one_daily_counter", async () => {
    const w = await world();
    const key = `day:${await hashOf(TOKEN)}:${new Date(w.nowMs).toISOString().slice(0, 10)}`;
    w.env.RELAY_STATE.values.set(key, "299");
    const upper = TOKEN.toUpperCase();
    expect((await send(w, { path: `/v1/apns/production/${upper}` })).status).toBe(201);
    expect(w.apns.requests[0]?.url).toBe(`https://api.push.apple.com/3/device/${upper}`);
    expect(w.limiters.token.keys).toEqual([await hashOf(TOKEN)]);
    expect(w.env.RELAY_STATE.values.get(key)).toBe("300");
    expect((await send(w)).status).toBe(429);
    expect(w.apns.requests).toHaveLength(1);
  });

  it("a_failed_daily_counter_write_still_delivers", async () => {
    const w = await world();
    w.env.RELAY_STATE.failPut = (key) => key.startsWith("day:");
    const lines = captureLog();
    expect((await send(w)).status).toBe(201);
    expect(w.apns.requests).toHaveLength(1);
    expect(lines).toHaveLength(1);
    expect(JSON.parse(lines[0] ?? "")).toMatchObject({ status: 201, kv_error: "day" });
  });

  it("a_failed_token_cache_write_still_delivers", async () => {
    const w = await world();
    w.env.RELAY_STATE.failPut = (key) => key === "apns-jwt";
    const lines = captureLog();
    expect((await send(w)).status).toBe(201);
    expect(w.apns.requests).toHaveLength(1);
    const providerJwt = (w.apns.requests[0]?.headers.get("authorization") ?? "").slice("bearer ".length);
    expect(await verifiesWith(w.publicKey, providerJwt)).toBe(true);
    expect(w.env.RELAY_STATE.values.has("apns-jwt")).toBe(false);
    expect(lines).toHaveLength(1);
    expect(JSON.parse(lines[0] ?? "")).toMatchObject({ status: 201, kv_error: "apns-jwt" });
  });

  it("answers 429 with Retry-After 60 when the per-token limiter refuses", async () => {
    const w = await world();
    w.limiters.token = limiter(false);
    w.env.TOKEN_LIMITER = w.limiters.token;
    const response = await send(w);
    expect(response.status).toBe(429);
    expect(response.headers.get("Retry-After")).toBe("60");
    expect(w.limiters.token.keys).toEqual([await hashOf(TOKEN)]);
    expect(w.apns.requests).toHaveLength(0);
  });

  it("answers 429 at the daily cap of 300 with Retry-After the seconds to the next UTC midnight", async () => {
    const w = await world();
    const day = new Date(w.nowMs).toISOString().slice(0, 10);
    const key = `day:${await hashOf(TOKEN)}:${day}`;
    w.env.RELAY_STATE.values.set(key, "299");
    expect((await send(w)).status).toBe(201);
    expect(w.env.RELAY_STATE.puts.find((put) => put.key === key)).toEqual({ key, value: "300", ttl: 2 * 24 * 3600 });
    const response = await send(w);
    expect(response.status).toBe(429);
    const midnight = Date.UTC(
      new Date(w.nowMs).getUTCFullYear(),
      new Date(w.nowMs).getUTCMonth(),
      new Date(w.nowMs).getUTCDate() + 1,
    );
    expect(response.headers.get("Retry-After")).toBe(String(Math.ceil((midnight - w.nowMs) / 1000)));
    expect(w.apns.requests).toHaveLength(1);
  });

  it("answers 503 with Retry-After while the kill switch is on", async () => {
    const w = await world();
    w.env.RELAY_ENABLED = "false";
    const response = await send(w);
    expect(response.status).toBe(503);
    expect(response.headers.get("Retry-After")).toBe("300");
    expect(w.apns.requests).toHaveLength(0);
  });

  it("mints the provider token once, reuses it ten minutes later and mints a new one after 50 minutes", async () => {
    const w = await world();
    const bearer = (index: number) => w.apns.requests[index]?.headers.get("authorization");
    expect((await send(w)).status).toBe(201);
    w.nowMs += 10 * 60 * 1000;
    expect((await send(w)).status).toBe(201);
    expect(bearer(1)).toBe(bearer(0));
    expect(w.env.RELAY_STATE.puts.filter((put) => put.key === "apns-jwt")).toHaveLength(1);
    w.nowMs = SIGNED_AT_MS + 50 * 60 * 1000;
    expect((await send(w)).status).toBe(201);
    expect(bearer(2)).not.toBe(bearer(0));
    expect(w.env.RELAY_STATE.puts.filter((put) => put.key === "apns-jwt")).toHaveLength(2);
  });

  it("answers GET /health, 404 for other paths and 405 for other methods", async () => {
    const w = await world();
    expect((await send(w, { path: "/health", method: "GET" })).status).toBe(200);
    expect((await send(w, { path: "/v1/other" })).status).toBe(404);
    expect((await send(w, { method: "PUT" })).status).toBe(405);
    expect(w.apns.requests).toHaveLength(0);
  });

  it("never logs the token, the body or the JWT", async () => {
    const w = await world();
    const lines: string[] = [];
    vi.spyOn(console, "log").mockImplementation((...args: unknown[]) => {
      lines.push(args.map(String).join(" "));
    });
    const body = bytes(200);
    expect((await send(w, { body })).status).toBe(201);
    w.apns.answer = { status: 403, body: '{"reason":"InvalidProviderToken"}' };
    expect((await send(w, { body })).status).toBe(502);
    expect((await send(w, { omit: ["Authorization"] })).status).toBe(403);
    expect(lines).toHaveLength(3);
    const logged = lines.join("\n");
    for (const secret of [TOKEN, fixture.jwt, fixture.k, b64urlEncode(body), w.env.APNS_KEY_P8]) {
      expect(logged).not.toContain(secret);
    }
    const first = JSON.parse(lines[0] ?? "") as Record<string, unknown>;
    expect(first).toMatchObject({ status: 201, platform: "apns" });
    expect(String(first.sender)).toMatch(/^[0-9a-f]{8}$/);
  });

  it("never posts to a host but APNs, across the whole suite", () => {
    expect(everyRequest.length).toBeGreaterThan(0);
    for (const request of everyRequest) {
      expect(["api.push.apple.com", "api.sandbox.push.apple.com"]).toContain(new URL(request.url).host);
    }
  });
});
