import { MANIFEST_URL, PAGE_URL, shouldStorePage, strategyFor } from "./sw-strategy";

// The WebWorker lib types `self` as the base `WorkerGlobalScope`; this worker
// is a service worker, whose scope adds `skipWaiting` and `clients`.
declare const self: ServiceWorkerGlobalScope;

// The shell cache: the page and its manifest, precached on install so an
// installed app opens while the phone is offline.
const CACHE_NAME = "rt-shell-v1";
const CACHE_PREFIX = "rt-shell-";

self.addEventListener("install", (event: ExtendableEvent) => {
  event.waitUntil(
    (async () => {
      const cache = await caches.open(CACHE_NAME);
      await cache.addAll([PAGE_URL, MANIFEST_URL]);
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
