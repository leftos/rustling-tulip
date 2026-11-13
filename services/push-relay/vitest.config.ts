import { cloudflareTest } from "@cloudflare/vitest-plugin";
import { defineConfig } from "vitest/config";

// Configured as https://developers.cloudflare.com/workers/testing/vitest-integration/configuration/ shows.
// The tests pass their own KV and rate-limit stubs and a mocked APNs `fetch` in `env` and `deps`.
export default defineConfig({
  plugins: [
    cloudflareTest({
      wrangler: { configPath: "./wrangler.jsonc" },
    }),
  ],
});
