import { expect, test } from "vitest";

import { backoffDelayMs } from "./backoff";

// The native client's numbers (`apps/native/src/connection.rs`): 500 ms
// doubling, capped at 10 s, no jitter.
test("backoff_delay_doubles_from_500_ms_and_caps_at_10_s", () => {
  const table: readonly (readonly [number, number])[] = [
    [0, 500],
    [1, 1000],
    [4, 8000],
    [5, 10_000],
    [30, 10_000],
  ];

  for (const [attempt, expected] of table) {
    expect(backoffDelayMs(attempt)).toBe(expected);
  }
});
