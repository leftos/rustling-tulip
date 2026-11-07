import { expect, test } from "vitest";

import { sessionBox } from "./viewport";

/** The layout viewport's height, `innerHeight`, which the keyboard leaves alone on iOS. */
const LAYOUT = 844;

test("session_box_fills_the_screen_with_the_keyboard_closed", () => {
  // The visual viewport is the whole layout viewport.
  expect(sessionBox({ height: 844, offsetTop: 0 }, LAYOUT)).toEqual({
    top: 0,
    height: 844,
    keyboardOpen: false,
  });
});

test("session_box_shrinks_to_the_space_above_the_keyboard", () => {
  expect(sessionBox({ height: 508, offsetTop: 0 }, LAYOUT)).toEqual({
    top: 0,
    height: 508,
    keyboardOpen: true,
  });
});

test("session_box_follows_a_panned_visual_viewport", () => {
  expect(sessionBox({ height: 508, offsetTop: 200 }, LAYOUT)).toEqual({
    top: 200,
    height: 508,
    keyboardOpen: true,
  });
});

test("session_box_reads_a_fully_panned_page_as_keyboard_open", () => {
  // iOS pans all the way down to show the composer: the bottom edge meets innerHeight.
  expect(sessionBox({ height: 508, offsetTop: 336 }, LAYOUT)).toEqual({
    top: 336,
    height: 508,
    keyboardOpen: true,
  });
});

test("session_box_rounds_its_edges_so_no_gap_opens", () => {
  // Both edges round to the nearest pixel, and the height is the space between them.
  expect(sessionBox({ height: 507.6, offsetTop: 0.4 }, LAYOUT)).toMatchObject({
    top: 0,
    height: 508,
  });
  expect(sessionBox({ height: 507.4, offsetTop: 335.6 }, LAYOUT)).toMatchObject({
    top: 336,
    height: 507,
  });
  expect(sessionBox({ height: 508.5, offsetTop: 0 }, LAYOUT)).toMatchObject({
    top: 0,
    height: 509,
  });
});

test("session_box_reads_a_sub_pixel_shortfall_as_keyboard_closed", () => {
  expect(sessionBox({ height: 843.5, offsetTop: 0 }, LAYOUT).keyboardOpen).toBe(false);
});
