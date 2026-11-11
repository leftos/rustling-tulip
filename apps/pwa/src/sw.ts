import { clickTarget, parsePush, shouldCloseNotification } from "./push-payload";
import { MANIFEST_URL, PAGE_URL, shouldStorePage, strategyFor } from "./sw-strategy";

// The WebWorker lib types `self` as the base `WorkerGlobalScope`; this worker
// is a service worker, whose scope adds `skipWaiting` and `clients`.
declare const self: ServiceWorkerGlobalScope;

// The WebWorker lib leaves out two things this worker needs: the Badging API,
// which iOS before 18.4 sets the badge only through, and the notification
// option that makes a same-tag replacement alert again.
declare global {
  interface NotificationOptions {
    renotify?: boolean;
  }
  interface WorkerNavigator {
    setAppBadge(contents?: number): Promise<void>;
  }
}

// The shell cache: the page, its manifest and the build's hashed assets,
// precached on install so an installed app opens styled and scripted while the
// phone is offline.
const CACHE_NAME = "rt-shell-v1";
const CACHE_PREFIX = "rt-shell-";

// The build's asset list. `vite.config.ts`'s `rt-precache` plugin rewrites this
// literal in place with the JSON of the build's hashed assets; a worker served
// unbuilt, as under `vite dev`, still carries the sentinel.
const PRECACHE_JSON = "__RT_PRECACHE__";

self.addEventListener("install", (event: ExtendableEvent) => {
  event.waitUntil(
    (async () => {
      const cache = await caches.open(CACHE_NAME);
      await cache.addAll([PAGE_URL, MANIFEST_URL, ...buildAssets()]);
      await self.skipWaiting();
    })(),
  );
});

self.addEventListener("activate", (event: ExtendableEvent) => {
  event.waitUntil(
    (async () => {
      const names = await caches.keys();
      await Promise.all(
        names
          .filter((name) => name.startsWith(CACHE_PREFIX) && name !== CACHE_NAME)
          .map((name) => caches.delete(name)),
      );
      await self.clients.claim();
    })(),
  );
});

self.addEventListener("fetch", (event: FetchEvent) => {
  const { request } = event;
  if (request.method !== "GET") {
    return;
  }
  const strategy = strategyFor(new URL(request.url), request.mode, self.location.origin);
  if (strategy === "network") {
    return;
  }
  event.respondWith(strategy === "cache-first" ? cacheFirst(request) : networkFirstPage(request));
});

// A push must show a notification: a push the user never sees is a session
// that waits for them unannounced. iOS 18.4 and later show a declarative push
// themselves only while no service worker is installed; once this one is, they
// deliver it to its `push` handler too, so the notification shown here is the
// one shown on every version. iOS 16.4 to 18.3 sets the badge only from here.
self.addEventListener("push", (event: PushEvent) => {
  event.waitUntil(
    (async () => {
      const shown = parsePush(event.data?.text() ?? null, self.location.origin);
      const options: NotificationOptions = {
        body: shown.body,
        data: { navigate: shown.navigate },
      };
      if (shown.tag !== null) {
        options.tag = shown.tag;
        // A newer push about the same session replaces the older notification
        // and alerts again, so it never arrives unannounced.
        options.renotify = true;
      }
      await self.registration.showNotification(shown.title, options);
      if (shown.badge !== null && "setAppBadge" in self.navigator) {
        try {
          await self.navigator.setAppBadge(shown.badge);
        } catch (error) {
          // The badge is best effort: the notification has already shown, so a
          // refusal here must not fail the push.
          console.warn("setAppBadge failed", error);
        }
      }
    })(),
  );
});

// A tap opens the session the push named: an open window of this app is
// focused and navigated to it, else a new one is opened. The notification
// closes only once that open has produced a window, so a tap that fails leaves
// it there to tap again.
self.addEventListener("notificationclick", (event: NotificationEvent) => {
  event.waitUntil(
    (async () => {
      const url = clickTarget(event.notification.data, self.location.origin);
      // `type: "window"` answers with window clients, which carry `focus` and
      // `navigate`; the lib types them as plain clients.
      const windows = (await self.clients.matchAll({
        type: "window",
        includeUncontrolled: true,
      })) as readonly WindowClient[];
      const client = windows[0];
      const navigated = client === undefined ? null : await focusAndNavigate(client, url);
      const opened = navigated ?? (await self.clients.openWindow(url));
      if (shouldCloseNotification(opened)) {
        event.notification.close();
      }
    })(),
  );
});

// The build's hashed assets, parsed from the literal the build wrote into this
// script; a worker served unbuilt has none.
function buildAssets(): string[] {
  if (!PRECACHE_JSON.startsWith("[")) {
    return [];
  }
  return JSON.parse(PRECACHE_JSON) as string[];
}

// A hashed asset never changes under its name, so the first copy is the one
// served from then on.
async function cacheFirst(request: Request): Promise<Response> {
  const cache = await caches.open(CACHE_NAME);
  const cached = await cache.match(request);
  if (cached !== undefined) {
    return cached;
  }
  const response = await fetch(request);
  if (response.ok) {
    await cache.put(request, response.clone());
  }
  return response;
}

// A navigation goes to the network and falls back to the precached page when
// the network is unreachable. A fresh same-origin page replaces the cached one,
// so the offline fallback is the newest page the phone has seen.
async function networkFirstPage(request: Request): Promise<Response> {
  try {
    const response = await fetch(request);
    if (shouldStorePage(response)) {
      const cache = await caches.open(CACHE_NAME);
      await cache.put(PAGE_URL, response.clone());
    }
    return response;
  } catch (error) {
    const cache = await caches.open(CACHE_NAME);
    const cached = await cache.match(PAGE_URL);
    if (cached !== undefined) {
      return cached;
    }
    throw error;
  }
}

// Focus comes first: it needs the click's window-interaction allowance, which a
// page load can outlast. A null answer means no window took the navigation, as
// an uncontrolled page on a first install or after a hard reload does not.
async function focusAndNavigate(client: WindowClient, url: string): Promise<WindowClient | null> {
  try {
    await client.focus();
    return await client.navigate(url);
  } catch (error) {
    // The window refused the focus or the navigation; its caller opens a new one instead.
    console.warn("opening the session in the open window failed; opening a new one", error);
    return null;
  }
}
