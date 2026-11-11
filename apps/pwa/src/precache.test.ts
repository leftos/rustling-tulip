import { expect, test } from "vitest";

import { injectPrecache, precacheUrls } from "./precache";

const SENTINEL = "__RT_PRECACHE__";

test("precacheUrls_keeps_assets_sorted_and_root_absolute", () => {
  const fileNames = [
    "sw.js",
    "index.html",
    "assets/main-b2.js",
    "assets/main-a1.css",
    "assets/main-b2.js.map",
    "manifest.webmanifest",
  ];

  expect(precacheUrls(fileNames)).toEqual(["/assets/main-a1.css", "/assets/main-b2.js"]);
});

test("precacheUrls_of_an_empty_bundle_is_empty", () => {
  expect(precacheUrls([])).toEqual([]);
});

test("injectPrecache_replaces_the_double_quoted_sentinel", () => {
  const code = `const P="${SENTINEL}";`;

  const injected = injectPrecache(code, ["/assets/a.js"]);

  expect(injected).toBe('const P="[\\"/assets/a.js\\"]";');
  expect(JSON.parse(JSON.parse(injected.slice("const P=".length, -1)))).toEqual(["/assets/a.js"]);
});

test("injectPrecache_replaces_the_single_quoted_sentinel", () => {
  const code = `const P='${SENTINEL}';`;

  const injected = injectPrecache(code, ["/assets/a.js"]);

  expect(injected).toBe('const P="[\\"/assets/a.js\\"]";');
});

test("injectPrecache_replaces_the_backtick_quoted_sentinel", () => {
  const code = `const P=\`${SENTINEL}\`;`;

  const injected = injectPrecache(code, ["/assets/a.js"]);

  expect(injected).toBe('const P="[\\"/assets/a.js\\"]";');
});

test("injectPrecache_throws_when_two_quote_forms_hold_a_sentinel_each", () => {
  const code = `const A="${SENTINEL}";const B=\`${SENTINEL}\`;`;

  expect(() => injectPrecache(code, ["/assets/a.js"])).toThrow(
    "the service worker holds the precache sentinel 2 times, not once",
  );
});

test("injectPrecache_throws_when_the_sentinel_is_absent", () => {
  expect(() => injectPrecache("const P=[];", ["/assets/a.js"])).toThrow(
    "the service worker holds the precache sentinel 0 times, not once",
  );
});

test("injectPrecache_throws_when_the_sentinel_is_there_twice", () => {
  const code = `const A="${SENTINEL}";const B="${SENTINEL}";`;

  expect(() => injectPrecache(code, ["/assets/a.js"])).toThrow(
    "the service worker holds the precache sentinel 2 times, not once",
  );
});
