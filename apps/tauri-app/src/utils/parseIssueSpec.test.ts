import { describe, expect, it } from "vitest";
import { parseIssueSpec } from "./parseIssueSpec";

describe("parseIssueSpec", () => {
  it("expands mixed ranges and singles in order", () => {
    expect(parseIssueSpec("123-127, 131, 134-136")).toEqual({
      ok: true,
      issues: [123, 124, 125, 126, 127, 131, 134, 135, 136],
    });
  });

  it("parses a single number", () => {
    expect(parseIssueSpec("42")).toEqual({ ok: true, issues: [42] });
  });

  it("preserves first-appearance order and dedupes overlaps", () => {
    expect(parseIssueSpec("131, 123-127, 125-130")).toEqual({
      ok: true,
      issues: [131, 123, 124, 125, 126, 127, 128, 129, 130],
    });
  });

  it("tolerates whitespace and trailing commas", () => {
    expect(parseIssueSpec("  7 - 9 ,, 12 ,")).toEqual({
      ok: true,
      issues: [7, 8, 9, 12],
    });
  });

  it("rejects malformed specs", () => {
    for (const bad of ["abc", "12-", "-12", "12-9", "0", "", "  ,  "]) {
      expect(parseIssueSpec(bad).ok).toBe(false);
    }
  });

  it("caps runaway ranges", () => {
    expect(parseIssueSpec("1-100000").ok).toBe(false);
  });
});
