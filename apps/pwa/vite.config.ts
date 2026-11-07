import { defineConfig } from "vite";

// Root-absolute URLs: the daemon serves the app at the origin root and answers
// a deep link such as `/s/abc123` with the page at that URL, where a relative
// URL would resolve under `/s/`.
export default defineConfig({
  base: "/",
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
