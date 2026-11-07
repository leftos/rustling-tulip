import { expect, test } from "vitest";

import { MANIFEST_URL, PAGE_URL, shouldStorePage, strategyFor } from "./sw-strategy";

const ORIGIN = "https://tulip.example.ts.net";

test("strategy_navigation_is_network_first_page", () => {
  const page = new URL(`${ORIGIN}/s/abc123`);

  expect(strategyFor(page, "navigate", ORIGIN)).toBe("network-first-page");
});

test("strategy_assets_are_cache_first", () => {
  const asset = new URL(`${ORIGIN}/assets/index-abc123.js`);

  expect(strategyFor(asset, "same-origin", ORIGIN)).toBe("cache-first");
});

test("strategy_other_same_origin_goes_to_network", () => {
  const manifest = new URL(`${ORIGIN}/manifest.webmanifest`);

  expect(strategyFor(manifest, "same-origin", ORIGIN)).toBe("network");
});

test("strategy_cross_origin_goes_to_network", () => {
  const health = new URL("https://elsewhere.example/s/abc123");

  expect(strategyFor(health, "navigate", ORIGIN)).toBe("network");
});

test("strategy_assets_path_on_another_origin_goes_to_network", () => {
  const asset = new URL("https://elsewhere.example/assets/index-abc123.js");

  expect(strategyFor(asset, "same-origin", ORIGIN)).toBe("network");
});

test("shouldStorePage_keeps_a_successful_same_origin_page", () => {
  expect(shouldStorePage({ ok: true, type: "basic" })).toBe(true);
});

test("shouldStorePage_rejects_an_error_page", () => {
  expect(shouldStorePage({ ok: false, type: "basic" })).toBe(false);
});

test("shouldStorePage_rejects_another_origin_answer", () => {
  expect(shouldStorePage({ ok: true, type: "opaque" })).toBe(false);
});

test("shell_page_url_is_the_origin_root", () => {
  expect(PAGE_URL).toBe("/");
});

test("shell_manifest_url_is_root_absolute", () => {
  expect(MANIFEST_URL).toBe("/manifest.webmanifest");
});
