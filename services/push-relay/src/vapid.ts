// RFC 8292 VAPID: the `Authorization: vapid t=<jwt>, k=<key>` header a daemon
// sends, and the ES256 check of its JWT against the `k=` point, with WebCrypto only.

/** How far ahead of now a JWT's `exp` may lie (RFC 8292 §2: no more than 24 hours). */
const MAX_EXP_AHEAD_SECONDS = 24 * 3600;

const ES256 = { name: "ECDSA", hash: "SHA-256" } as const;

export function b64urlEncode(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** The bytes `text` (base64url, padding optional) encodes, or null when it is not base64url. */
export function b64urlDecode(text: string): Uint8Array<ArrayBuffer> | null {
  if (!/^[A-Za-z0-9_-]*={0,2}$/.test(text)) return null;
  const base64 = text.replace(/=+$/, "").replace(/-/g, "+").replace(/_/g, "/");
  if (base64.length % 4 === 1) return null;
  const binary = atob(base64 + "=".repeat((4 - (base64.length % 4)) % 4));
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) out[i] = binary.charCodeAt(i);
  return out;
}

/** Lower-case hex of SHA-256 over `bytes`. */
export async function sha256Hex(bytes: Uint8Array): Promise<string> {
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", new Uint8Array(bytes)));
  return Array.from(digest, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

/**
 * The `t` and `k` parameters of a `vapid` Authorization header (RFC 8292 §3), in
 * any order and with any whitespace around them, or null for anything else.
 */
export function parseAuthorization(header: string | null): { t: string; k: string } | null {
  const match = header === null ? null : /^\s*vapid\s+(\S.*)$/i.exec(header);
  if (!match) return null;
  const params = new Map<string, string>();
  for (const part of (match[1] ?? "").split(",")) {
    const eq = part.indexOf("=");
    const name = part.slice(0, eq).trim().toLowerCase();
    const value = part.slice(eq + 1).trim();
    if (eq < 0 || value === "" || params.has(name)) return null;
    params.set(name, value);
  }
  const t = params.get("t");
  const k = params.get("k");
  return t && k ? { t, k } : null;
}

export type Verdict = { ok: true; key: Uint8Array<ArrayBuffer> } | { ok: false; reason: string };

interface Claims {
  aud: unknown;
  exp: unknown;
}

function decodeJson(part: string): unknown {
  const bytes = b64urlDecode(part);
  if (bytes === null) return null;
  try {
    return JSON.parse(new TextDecoder().decode(bytes)) as unknown;
  } catch {
    return null;
  }
}

async function signatureVerifies(key: Uint8Array<ArrayBuffer>, jwt: string): Promise<boolean> {
  const parts = jwt.split(".");
  const signature = b64urlDecode(parts[2] ?? "");
  if (parts.length !== 3 || signature === null || signature.length !== 64) return false;
  try {
    const publicKey = await crypto.subtle.importKey("raw", key, { name: "ECDSA", namedCurve: "P-256" }, false, ["verify"]);
    const signed = new TextEncoder().encode(`${parts[0]}.${parts[1]}`);
    return await crypto.subtle.verify(ES256, publicKey, signature, signed);
  } catch {
    return false;
  }
}

/** Why `claims` are refused at `nowSeconds` by a relay whose origin is `origin`, or null when they pass. */
function claimsFault(claims: Claims, origin: string, nowSeconds: number): string | null {
  if (claims.aud !== origin) return "vapid jwt audience is not this relay";
  if (typeof claims.exp !== "number") return "vapid jwt has no expiry";
  if (claims.exp <= nowSeconds) return "vapid jwt expired";
  if (claims.exp > nowSeconds + MAX_EXP_AHEAD_SECONDS) return "vapid jwt expires more than a day ahead";
  return null;
}

/**
 * Checks a push's `Authorization` header: its JWT must verify ES256 against its own
 * `k=` point, name `origin` as `aud`, and expire after `nowSeconds` but within a day.
 */
export async function verifyVapid(header: string | null, origin: string, nowSeconds: number): Promise<Verdict> {
  const auth = parseAuthorization(header);
  if (auth === null) return { ok: false, reason: "vapid authorization missing or malformed" };
  const key = b64urlDecode(auth.k);
  if (key === null || key.length !== 65 || key[0] !== 4) return { ok: false, reason: "vapid key is not a P-256 point" };
  const header64 = decodeJson(auth.t.split(".")[0] ?? "") as { alg?: unknown } | null;
  if (header64?.alg !== "ES256") return { ok: false, reason: "vapid jwt is not ES256" };
  if (!(await signatureVerifies(key, auth.t))) return { ok: false, reason: "vapid jwt does not verify" };
  const claims = decodeJson(auth.t.split(".")[1] ?? "") as Claims | null;
  const fault = claims === null ? "vapid jwt claims are not JSON" : claimsFault(claims, origin, nowSeconds);
  return fault === null ? { ok: true, key } : { ok: false, reason: fault };
}
