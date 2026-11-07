import { expect, test } from "vitest";

import { introLineShown } from "./installed";

test("intro_line_is_hidden_in_the_installed_app", () => {
  expect(introLineShown(true)).toBe(false);
});

test("intro_line_shows_in_a_browser_tab", () => {
  // The pairing screen, or connecting: the install instruction stands.
  expect(introLineShown(false)).toBe(true);
});
