import { expect, test } from "vitest";

import { measureScroll, scrollAfterPaint } from "./chat-scroll";

test("measureScroll reads the bottom within AT_BOTTOM_PX", () => {
  const justAbove = measureScroll({ scrollTop: 192, scrollHeight: 500, clientHeight: 300 });
  expect(justAbove.atBottom).toBe(false);
  expect(justAbove.top).toBe(192);

  const atBottom = measureScroll({ scrollTop: 193, scrollHeight: 500, clientHeight: 300 });
  expect(atBottom.atBottom).toBe(true);
  expect(atBottom.top).toBe(193);
});

test("a position kept across the toggle comes back", () => {
  expect(scrollAfterPaint({ top: 120, atBottom: false }, { top: 0, atBottom: true }, 900)).toBe(
    120,
  );
});

test("a chat kept at the bottom returns to the newest row", () => {
  expect(scrollAfterPaint({ top: 600, atBottom: true }, { top: 0, atBottom: true }, 900)).toBe(900);
});

test("with nothing kept, only a chat at the bottom follows the newest row", () => {
  expect(scrollAfterPaint(null, { top: 0, atBottom: true }, 900)).toBe(900);
  expect(scrollAfterPaint(null, { top: 50, atBottom: false }, 900)).toBeNull();
});
