import { expect, test } from "vitest";

import { measureScroll, scrollAfterPaint, scrollAfterResize } from "./chat-scroll";

test("a chat at the bottom stays at the bottom when its box shrinks", () => {
  // The dock opens: the box loses 200 px of its 600, the rows stay 1000 high.
  const before = measureScroll({ scrollTop: 400, scrollHeight: 1000, clientHeight: 600 });
  expect(scrollAfterResize(before.atBottom, 600, 1000, 400)).toBe(600);
});

test("a chat at the bottom stays at the bottom when its box grows", () => {
  // The dock closes: the box gains its 200 px back.
  const before = measureScroll({ scrollTop: 600, scrollHeight: 1000, clientHeight: 400 });
  expect(scrollAfterResize(before.atBottom, 400, 1000, 600)).toBe(400);
  expect(scrollAfterResize(true, 200, 300, 600)).toBe(0);
});

test("a chat scrolled up keeps its place when its box shrinks", () => {
  const before = measureScroll({ scrollTop: 150, scrollHeight: 1000, clientHeight: 600 });
  expect(scrollAfterResize(before.atBottom, 600, 1000, 400)).toBeNull();
});

test("a chat whose box keeps its height is not scrolled", () => {
  // A work-line tick or a viewport scroll: nothing resized, so a reader
  // starting to drag up from the bottom is not pulled back.
  const before = measureScroll({ scrollTop: 395, scrollHeight: 1000, clientHeight: 600 });
  expect(before.atBottom).toBe(true);
  expect(scrollAfterResize(before.atBottom, 600, 1000, 600)).toBeNull();
});

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
