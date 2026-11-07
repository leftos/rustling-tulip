// Reading the declarative Web Push payload the daemon sends
// (`docs/plans/mobile-app.md` "Push"):
// `{"web_push": 8030, "notification": {"title", "body", "navigate", "app_badge"}}`.
// Pure, so the reading is unit-testable without a worker; `sw.ts` is the glue
// that shows what this returns.

/** The notification the worker shows for one push. */
export interface ShownPush {
  /** Its title; never empty, since the fallback notification has one. */
  readonly title: string;
  /** Its body, empty when the push names none. */
  readonly body: string;
  /** The same-origin page a tap opens; the origin root when none is named. */
  readonly navigate: string;
  /** The Needs you count for the app badge; null when the push carries none. */
  readonly badge: number | null;
  /** The session this push is about, so a newer push replaces its older
   * notification; null when the push names no single session. */
  readonly tag: string | null;
}

/** The title a push shows when it cannot read one of its own: a push must show
 * a notification, so no push lowers the badge or passes unseen. */
const FALLBACK_TITLE = "Rustling Tulip";

/** The session a `/s/{id}` route names, as `parseRoute` reads it. */
const SESSION_PATH = /^\/s\/([^/]+)$/;

/**
 * The notification for one push: `text` is the push's data, `origin` the
 * worker's origin. A push that is not JSON or carries no notification shows the
 * fallback notification; a push that names no title shows this app's own, and
 * the rest of its payload still shows around it.
 */
export function parsePush(text: string | null, origin: string): ShownPush {
  const notification = declarativeNotification(text);
  if (notification === null) {
    return fallbackPush(origin);
  }
  const title = stringField(notification, "title");
  const target = sameOriginUrl(notification["navigate"], origin);
  return {
    title: title === null || title === "" ? FALLBACK_TITLE : title,
    body: stringField(notification, "body") ?? "",
    navigate: target?.href ?? rootUrl(origin),
    badge: parseBadge(notification["app_badge"]),
    tag: target === null ? null : sessionTag(target),
  };
}

/**
 * The URL a tap opens: the `navigate` the worker stored in the notification's
 * `data`, kept only when it is a same-origin URL, else the origin root.
 */
export function clickTarget(data: unknown, origin: string): string {
  const navigate = isRecord(data) ? data["navigate"] : undefined;
  return sameOriginUrl(navigate, origin)?.href ?? rootUrl(origin);
}

/**
 * Whether the notification a tap opened may close: only once an open resolved
 * to a window. One that resolved nothing (`null`) or threw leaves it there for
 * another tap.
 */
export function shouldCloseNotification(opened: unknown): boolean {
  return opened !== null && opened !== undefined;
}

/** The `notification` object of the declarative payload; null when `text` is
 * not JSON or carries none. */
function declarativeNotification(text: string | null): Readonly<Record<string, unknown>> | null {
  if (text === null) {
    return null;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    // A push that is not JSON is not one this app sent; it still shows.
    return null;
  }
  if (!isRecord(parsed)) {
    return null;
  }
  const notification = parsed["notification"];
  return isRecord(notification) ? notification : null;
}

function isRecord(value: unknown): value is Readonly<Record<string, unknown>> {
  return typeof value === "object" && value !== null;
}

/** `field[name]` when it is a string; null for anything else. */
function stringField(field: Readonly<Record<string, unknown>>, name: string): string | null {
  const value = field[name];
  return typeof value === "string" ? value : null;
}

/** `value` as a URL only when it parses and belongs to `origin`: a push can
 * never send the user to another site. */
function sameOriginUrl(value: unknown, origin: string): URL | null {
  const url = parseUrl(value);
  return url !== null && url.origin === origin ? url : null;
}

function parseUrl(value: unknown): URL | null {
  if (typeof value !== "string") {
    return null;
  }
  try {
    return new URL(value);
  } catch {
    return null;
  }
}

/** `value` as a badge count: a whole number that is not negative, sent as a
 * number or as a string of digits; null for anything else. */
function parseBadge(value: unknown): number | null {
  if (typeof value === "number") {
    return Number.isInteger(value) && value >= 0 ? value : null;
  }
  if (typeof value === "string" && /^\d+$/.test(value)) {
    const badge = Number.parseInt(value, 10);
    return Number.isSafeInteger(badge) ? badge : null;
  }
  return null;
}

/** The session `url` names; null when it names none. */
function sessionTag(url: URL): string | null {
  const raw = SESSION_PATH.exec(url.pathname)?.[1];
  if (raw === undefined) {
    return null;
  }
  try {
    return decodeURIComponent(raw);
  } catch {
    // A malformed escape names no session this app can open.
    return null;
  }
}

/** The notification that shows for a push this app cannot read. */
function fallbackPush(origin: string): ShownPush {
  return { title: FALLBACK_TITLE, body: "", navigate: rootUrl(origin), badge: null, tag: null };
}

function rootUrl(origin: string): string {
  return `${origin}/`;
}
