import { expect, test } from "vitest";

import { SW_SCOPE, SW_URL } from "./paths";

test("paths_sw_url_is_root_absolute", () => {
  expect(SW_URL).toBe("/sw.js");
});

test("paths_sw_scope_is_the_origin_root", () => {
  expect(SW_SCOPE).toBe("/");
});
