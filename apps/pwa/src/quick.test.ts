import { expect, test } from "vitest";

import { quickRepliesFor, quickReplyBytes } from "./quick";

test("quick_agent_groups_are_an_agent_sessions_shipped_set", () => {
  expect(quickRepliesFor(true)).toEqual([
    { title: "confirm", items: ["yes", "no"] },
    {
      title: "common",
      items: ["continue", "commit and push", "retry", "skip", "drastically simplify"],
    },
  ]);
});

test("quick_shell_groups_are_the_y_n_pair", () => {
  expect(quickRepliesFor(false)).toEqual([{ title: "confirm", items: ["y", "n"] }]);
});

test("quick_replies_are_sent_with_a_carriage_return", () => {
  for (const isAgent of [true, false]) {
    for (const group of quickRepliesFor(isAgent)) {
      for (const item of group.items) {
        expect(quickReplyBytes(item)).toBe(`${item}\r`);
      }
    }
  }
});
