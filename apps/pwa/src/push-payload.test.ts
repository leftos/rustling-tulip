import { expect, test } from "vitest";

import { clickTarget, parsePush, shouldCloseNotification } from "./push-payload";

const ORIGIN = "https://tulip.example.ts.net";

const ROOT = `${ORIGIN}/`;

const FALLBACK = {
  title: "Rustling Tulip",
  body: "",
  navigate: ROOT,
  badge: null,
  tag: null,
};

/** The declarative payload the daemon sends, around one notification. */
function payload(notification: Record<string, unknown>): string {
  return JSON.stringify({ web_push: 8030, notification });
}

test("parsePush_reads_the_declarative_payload", () => {
  const text = payload({
    title: "Needs you",
    body: "Session rt-42 is waiting for input",
    navigate: `${ORIGIN}/s/rt-42`,
    app_badge: "2",
  });

  expect(parsePush(text, ORIGIN)).toEqual({
    title: "Needs you",
    body: "Session rt-42 is waiting for input",
    navigate: `${ORIGIN}/s/rt-42`,
    badge: 2,
    tag: "rt-42",
  });
});

test("parsePush_reads_a_badge_sent_as_a_string", () => {
  const text = payload({ title: "Needs you", app_badge: "3" });

  expect(parsePush(text, ORIGIN).badge).toBe(3);
});

test("parsePush_reads_a_badge_sent_as_a_number", () => {
  const text = payload({ title: "Needs you", app_badge: 3 });

  expect(parsePush(text, ORIGIN).badge).toBe(3);
});

test("parsePush_ignores_a_negative_badge", () => {
  const text = payload({ title: "Needs you", app_badge: "-1" });

  expect(parsePush(text, ORIGIN).badge).toBeNull();
});

test("parsePush_ignores_a_badge_that_is_not_a_number", () => {
  const text = payload({ title: "Needs you", app_badge: "x" });

  expect(parsePush(text, ORIGIN).badge).toBeNull();
});

test("parsePush_replaces_a_cross_origin_navigate_with_the_origin_root", () => {
  const text = payload({ title: "Needs you", navigate: "https://elsewhere.example/s/rt-42" });

  expect(parsePush(text, ORIGIN).navigate).toBe(ROOT);
});

test("parsePush_replaces_a_malformed_navigate_with_the_origin_root", () => {
  const text = payload({ title: "Needs you", navigate: "not a url" });

  expect(parsePush(text, ORIGIN).navigate).toBe(ROOT);
});

test("parsePush_gives_a_root_navigate_no_tag", () => {
  const text = payload({ title: "Needs you", navigate: ROOT });

  expect(parsePush(text, ORIGIN).tag).toBeNull();
});

test("parsePush_shows_the_fallback_notification_for_text_that_is_not_json", () => {
  expect(parsePush("when I grow up", ORIGIN)).toEqual(FALLBACK);
});

test("parsePush_shows_the_fallback_notification_for_no_data", () => {
  expect(parsePush(null, ORIGIN)).toEqual(FALLBACK);
});

test("parsePush_falls_back_to_its_own_title_alone", () => {
  const text = payload({ body: "Session rt-42 is waiting for input" });

  expect(parsePush(text, ORIGIN)).toEqual({
    title: "Rustling Tulip",
    body: "Session rt-42 is waiting for input",
    navigate: ROOT,
    badge: null,
    tag: null,
  });
});

test("parsePush_keeps_a_payload_whose_title_is_empty", () => {
  const text = payload({ title: "", navigate: `${ORIGIN}/s/rt-42`, app_badge: "2" });

  expect(parsePush(text, ORIGIN)).toEqual({
    title: "Rustling Tulip",
    body: "",
    navigate: `${ORIGIN}/s/rt-42`,
    badge: 2,
    tag: "rt-42",
  });
});

test("clickTarget_keeps_a_same_origin_url", () => {
  expect(clickTarget({ navigate: `${ORIGIN}/s/rt-42` }, ORIGIN)).toBe(`${ORIGIN}/s/rt-42`);
});

test("clickTarget_replaces_a_cross_origin_url_with_the_origin_root", () => {
  expect(clickTarget({ navigate: "https://elsewhere.example/s/rt-42" }, ORIGIN)).toBe(ROOT);
});

test("clickTarget_opens_the_origin_root_without_data", () => {
  expect(clickTarget(null, ORIGIN)).toBe(ROOT);
});

test("clickTarget_opens_the_origin_root_for_data_that_is_not_an_object", () => {
  expect(clickTarget("https://elsewhere.example/s/rt-42", ORIGIN)).toBe(ROOT);
});

test("shouldCloseNotification_closes_after_a_window_opened", () => {
  expect(shouldCloseNotification({ id: "rt-42" })).toBe(true);
});

test("shouldCloseNotification_keeps_the_notification_without_a_window", () => {
  expect(shouldCloseNotification(null)).toBe(false);
});
