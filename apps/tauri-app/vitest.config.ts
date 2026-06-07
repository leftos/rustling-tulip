import { defineConfig } from "vitest/config";

// Pure-logic unit tests only — no DOM, no React. Kept separate from
// vite.config.ts so the dev-server config (port, fs allowlist, HMR) never
// leaks into the test run.
export default defineConfig({
  test: {
    include: ["src/**/*.test.ts"],
    environment: "node",
  },
});
