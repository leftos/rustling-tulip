import { defineConfig, type Plugin } from "vite";

import { injectPrecache, precacheUrls } from "./src/precache.ts";

// The service worker's install precaches the page, the manifest and this
// build's hashed assets, so an installed app opens styled and scripted with no
// network. The list comes from the build's own output, so the worker fetches no
// manifest at run time.
function rtPrecache(): Plugin {
  return {
    name: "rt-precache",
    apply: "build",
    enforce: "post",
    generateBundle(_options, bundle) {
      const worker = bundle["sw.js"];
      if (worker !== undefined && worker.type === "chunk") {
        try {
          worker.code = injectPrecache(worker.code, precacheUrls(Object.keys(bundle)));
        } catch (error) {
          this.error(error instanceof Error ? error.message : String(error));
        }
      } else {
        this.error("the build has no sw.js chunk to carry the precache list");
      }
    },
  };
}

// Root-absolute URLs: the daemon serves the app at the origin root and answers
// a deep link such as `/s/abc123` with the page at that URL, where a relative
// URL would resolve under `/s/`.
export default defineConfig({
  base: "/",
  plugins: [rtPrecache()],
  build: {
    // No inline module-preload polyfill: the daemon serves the page under
    // `script-src 'self'`, which forbids an inline script.
    modulePreload: { polyfill: false },
    rollupOptions: {
      // The service worker is a second entry, emitted under a stable name so
      // the page's registration and the daemon's no-cache rule find it.
      input: { main: "index.html", sw: "src/sw.ts" },
      output: {
        entryFileNames: (chunk) => (chunk.name === "sw" ? "sw.js" : "assets/[name]-[hash].js"),
      },
    },
  },
});
