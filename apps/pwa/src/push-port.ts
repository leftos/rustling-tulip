import { base64urlToBytes } from "./base64url";
import { isInstalled } from "./installed";

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
 * What a browser step fails with when the page's service worker never becomes
 * active. The panel shows it as it is, like a browser step's own failure text.
 */
export const WORKER_NOT_READY =
  "The app's service worker didn't start; close and reopen the app, then try again.";

/** How long a browser step waits for the service worker to become active. */
export const WORKER_READY_MS = 10_000;

/** What {@link browserPushPort} needs from its surroundings: an active worker and a timer. */
export interface PushPortEnv {
  /** The page's active service worker; a page that never registers one leaves it unsettled. */
  ready(): Promise<ServiceWorkerRegistration>;
  /** Resolves after `ms` milliseconds. */
  sleep(ms: number): Promise<void>;
}

/** The page's own browser: its service worker and a real timer. */
export function browserPushEnv(): PushPortEnv {
  return {
    ready: () => navigator.serviceWorker.ready,
    sleep: (ms) =>
      new Promise<void>((resolve) => {
        setTimeout(resolve, ms);
      }),
  };
}

/** Whichever of the two settles {@link readyWithin}: the value, or the wait running out. */
type ReadyOutcome<T> = { readonly arrived: true; readonly value: T } | { readonly arrived: false };

/**
 * `ready` when it settles first, else a rejection with {@link WORKER_NOT_READY}
 * once `sleep(ms)` resolves, so a page whose service worker never activates
 * fails its browser step instead of leaving it out forever.
 */
export async function readyWithin<T>(
  ready: Promise<T>,
  ms: number,
  sleep: (ms: number) => Promise<void>,
): Promise<T> {
  const arrived: Promise<ReadyOutcome<T>> = ready.then((value) => ({ arrived: true, value }));
  const expired: Promise<ReadyOutcome<T>> = sleep(ms).then(() => ({ arrived: false }));
  const first = await Promise.race([arrived, expired]);
  if (!first.arrived) {
    throw new Error(WORKER_NOT_READY);
  }
  return first.value;
}

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

/**
 * The push API of the page's own browser, over its service worker and
 * `Notification`. `env` carries the worker and the timer, which a test scripts
 * so it can drive a worker that never activates.
 */
export function browserPushPort(env: PushPortEnv = browserPushEnv()): PushPort {
  return {
    supported: () => typeof PushManager !== "undefined" && typeof Notification !== "undefined",
    installed: () => isInstalled(navigator),
    permission: () => Notification.permission,
    requestPermission: async () => Notification.requestPermission(),
    current: async () => {
      const held = await subscription(env);
      return held === null ? null : keysOf(held);
    },
    subscribe: async (vapidPublicKey) => {
      const registration = await readyWithin(env.ready(), WORKER_READY_MS, env.sleep);
      const created = await registration.pushManager.subscribe({
        userVisibleOnly: true,
        applicationServerKey: base64urlToBytes(vapidPublicKey),
      });
      return keysOf(created);
    },
    unsubscribe: async () => {
      const held = await subscription(env);
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

/** The phone's subscription now, once its service worker is active. */
async function subscription(env: PushPortEnv): Promise<PushSubscription | null> {
  const registration = await readyWithin(env.ready(), WORKER_READY_MS, env.sleep);
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
