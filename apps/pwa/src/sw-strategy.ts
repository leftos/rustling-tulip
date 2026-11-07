// The app is served from the daemon's origin root (`base: "/"` in
// `vite.config.ts`), and a deep link such as `/s/abc123` is answered with the
// page at that URL. These two live here rather than in `paths.ts` so that
// `sw.ts`, which is the only importer, bundles them into its own script: the
// service worker stays a self-contained classic worker.
export const PAGE_URL = "/";
export const MANIFEST_URL = "/manifest.webmanifest";

/// The caching strategy the service worker uses for a request. Pure, so the
/// routing decision is unit-testable without a worker.
export type Strategy = "network-first-page" | "cache-first" | "network";

/// Where the build puts its hashed assets (`vite.config.ts`).
const ASSET_PREFIX = "/assets/";

/// A request to another origin is never served from this app's cache; a
/// same-origin navigation is network-first with the cached page as fallback;
/// a hashed asset is cache-first; everything else goes to the network.
export function strategyFor(url: URL, mode: RequestMode, origin: string): Strategy {
  if (url.origin !== origin) {
    return "network";
  }
  if (mode === "navigate") {
    return "network-first-page";
  }
  if (url.pathname.startsWith(ASSET_PREFIX)) {
    return "cache-first";
  }
  return "network";
}

/// Whether a navigation's network answer may replace the cached page: a
/// same-origin `200` only, so an error page or another origin's answer never
/// becomes the page served while the phone is offline.
export function shouldStorePage(response: { ok: boolean; type: ResponseType }): boolean {
  return response.ok && response.type === "basic";
}
