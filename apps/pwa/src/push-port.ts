import { base64urlToBytes } from "./base64url";

/**
 * What a push subscription gives the page: its endpoint and the two keys,
 * base64url as `PushSubscription.toJSON()` returns them. The daemon's
 * `register_push` carries them unchanged.
 */
export interface PushKeys {
  readonly endpoint: string;
  readonly p256dh: string;
  readonly auth: string;
}

/** The browser's notification permission, as the panel shows it. */
export type PushPermission = "default" | "granted" | "denied";

/**
 * The phone's push side behind one seam, like `TokenStore` and `ScannerEnv`:
 * the page runs {@link browserPushPort}, and a test hands the panel
 * {@link memoryPushPort}. Subscribing needs the user's tap, so the port never
 * subscribes on its own.
 */
export interface PushPort {
  /** Whether this browser has the push API at all. */
  supported(): boolean;
  /** Whether the page runs from the Home Screen, which iOS requires for push. */
  installed(): boolean;
  permission(): PushPermission;
  /** Asks for the permission; iOS needs this inside the tap's own handler. */
  requestPermission(): Promise<PushPermission>;
  /** The subscription the phone holds; null when it holds none. */
  current(): Promise<PushKeys | null>;
  /** Subscribes to push for `vapidPublicKey`; needs the same tap. */
  subscribe(vapidPublicKey: string): Promise<PushKeys>;
  /** Drops the phone's subscription; safe with none held. */
  unsubscribe(): Promise<void>;
}

/** The push API of the page's own browser, over its service worker and `Notification`. */
export function browserPushPort(): PushPort {
  return {
    supported: () => typeof PushManager !== "undefined" && typeof Notification !== "undefined",
    installed: () => isInstalled(navigator),
    permission: () => Notification.permission,
    requestPermission: async () => Notification.requestPermission(),
    current: async () => {
      const held = await subscription();
      return held === null ? null : keysOf(held);
    },
    subscribe: async (vapidPublicKey) => {
      const registration = await navigator.serviceWorker.ready;
      const created = await registration.pushManager.subscribe({
        userVisibleOnly: true,
        applicationServerKey: base64urlToBytes(vapidPublicKey),
      });
      return keysOf(created);
    },
    unsubscribe: async () => {
      const held = await subscription();
      if (held !== null) {
        await held.unsubscribe();
      }
    },
  };
}

/** What a test starts {@link memoryPushPort} with. */
export interface MemoryPushOptions {
  readonly supported?: boolean;
  readonly installed?: boolean;
  /** Starts undecided; `requestPermission` answers `granted`. */
  readonly permission?: PushPermission;
  /** The subscription the phone starts with; null when it holds none. */
  readonly keys?: PushKeys | null;
  /** The subscription `subscribe` makes; {@link FAKE_KEYS} by default. */
  readonly subscribed?: PushKeys;
}

/** What a test reads back from {@link memoryPushPort}. */
export interface MemoryPushPort extends PushPort {
  /** Every call the page made, in order. */
  readonly calls: readonly string[];
  /** The VAPID key each `subscribe` was handed, in order. */
  readonly asked: readonly string[];
  /** The subscription the phone holds now. */
  held(): PushKeys | null;
}

/** The subscription a memory port makes when a test names none. */
export const FAKE_KEYS: PushKeys = {
  endpoint: "https://push.example.invalid/one",
  p256dh: "p256dh-key",
  auth: "auth-secret",
};

/** A port held in memory only: nothing outlives the page, and a test reads every call. */
export function memoryPushPort(options: MemoryPushOptions = {}): MemoryPushPort {
  let permission: PushPermission = options.permission ?? "granted";
  let keys: PushKeys | null = options.keys ?? null;
  const calls: string[] = [];
  const asked: string[] = [];
  const subscribed = options.subscribed ?? FAKE_KEYS;
  return {
    supported: () => options.supported ?? true,
    installed: () => options.installed ?? true,
    permission: () => permission,
    requestPermission: async () => {
      calls.push("requestPermission");
      if (permission === "default") {
        permission = "granted";
      }
      return permission;
    },
    current: async () => {
      calls.push("current");
      return keys;
    },
    subscribe: async (vapidPublicKey) => {
      calls.push("subscribe");
      asked.push(vapidPublicKey);
      keys = subscribed;
      return subscribed;
    },
    unsubscribe: async () => {
      calls.push("unsubscribe");
      keys = null;
    },
    calls,
    asked,
    held: () => keys,
  };
}

/** Whether the page runs from the Home Screen: iOS reports `standalone`, other browsers the display mode. */
function isInstalled(where: Navigator): boolean {
  const ios = where as Navigator & { readonly standalone?: boolean };
  return ios.standalone === true || matchMedia("(display-mode: standalone)").matches;
}

/** The phone's subscription now, once its service worker is active. */
async function subscription(): Promise<PushSubscription | null> {
  const registration = await navigator.serviceWorker.ready;
  return registration.pushManager.getSubscription();
}

/** The endpoint and keys of `subscription`, which the wire carries unchanged. */
function keysOf(subscription: PushSubscription): PushKeys {
  const json = subscription.toJSON();
  const keys = json.keys ?? {};
  const p256dh = keys["p256dh"];
  const auth = keys["auth"];
  if (json.endpoint === undefined || p256dh === undefined || auth === undefined) {
    throw new Error("the push subscription carries no endpoint or keys");
  }
  return { endpoint: json.endpoint, p256dh, auth };
}
