import { describe, expect, it } from "vitest";
import { bestFitGridCols, gridArrangements } from "./grid";

const WIDE_16_9 = 16 / 9; // 1.78 — the common maximized-window shape
const WIDE_16_10 = 1.6; // common laptop panel
const STANDARD_4_3 = 4 / 3; // 1.33
const SQUARE = 1;
const ULTRAWIDE_21_9 = 21 / 9; // 2.33
const ULTRAWIDE_32_9 = 32 / 9; // 3.56

describe("bestFitGridCols", () => {
  it("suggests a clean 2x2 (not 3x2) for 4 panes on a widescreen container", () => {
    // Regression guard: a 3-wide layout yields squarer cells (1.19 vs 1.78
    // at 16:9), but the two phantom cells make 2x2 the better default. The
    // empty-cell penalty is tuned so this holds for any aspect wider than
    // ~1.5:1, including 16:10.
    expect(bestFitGridCols(4, WIDE_16_9)).toBe(2);
    expect(bestFitGridCols(4, WIDE_16_10)).toBe(2);
  });

  it("keeps the larger square grids clean at 16:9", () => {
    expect(bestFitGridCols(9, WIDE_16_9)).toBe(3); // 3x3, not 5x2
    expect(bestFitGridCols(16, WIDE_16_9)).toBe(4); // 4x4, not 6x3
  });

  it("spreads 4 panes across more columns on ultrawide containers", () => {
    // Clean N-wide strips beat 2x2 once cells would otherwise be very wide;
    // this branch is independent of the empty-cell penalty.
    expect(bestFitGridCols(4, ULTRAWIDE_21_9)).toBe(4);
    expect(bestFitGridCols(4, ULTRAWIDE_32_9)).toBe(4);
  });

  it("does not over-pack a tall/square container", () => {
    // Guards against raising the empty-cell penalty too far: 10 panes on a
    // 4:3 surface should stay 4x3, not collapse to a wide 5x2.
    expect(bestFitGridCols(10, STANDARD_4_3)).toBe(4);
    expect(bestFitGridCols(4, SQUARE)).toBe(2);
  });

  it("matches the expected column count across pane counts at 16:9", () => {
    const expected: Record<number, number> = {
      2: 2,
      3: 3,
      4: 2,
      5: 3,
      6: 3,
      7: 4,
      8: 4,
      9: 3,
      10: 5,
      12: 4,
    };
    for (const [n, cols] of Object.entries(expected)) {
      expect(bestFitGridCols(Number(n), WIDE_16_9)).toBe(cols);
    }
  });

  it("returns a single column for trivial pane counts", () => {
    expect(bestFitGridCols(1, WIDE_16_9)).toBe(1);
    expect(bestFitGridCols(0, WIDE_16_9)).toBe(1);
  });

  it("falls back to 16:9 for a non-positive or non-finite aspect", () => {
    expect(bestFitGridCols(4, 0)).toBe(bestFitGridCols(4, WIDE_16_9));
    expect(bestFitGridCols(4, Number.NaN)).toBe(bestFitGridCols(4, WIDE_16_9));
    expect(bestFitGridCols(4, Number.POSITIVE_INFINITY)).toBe(
      bestFitGridCols(4, WIDE_16_9),
    );
  });
});

describe("gridArrangements", () => {
  it("has no in-between shapes below 3 panes", () => {
    expect(gridArrangements(2)).toEqual([]);
    expect(gridArrangements(1)).toEqual([]);
  });

  it("enumerates column counts from 2 to N-1 with row-major rows", () => {
    expect(gridArrangements(4)).toEqual([
      { cols: 2, rows: 2 },
      { cols: 3, rows: 2 },
    ]);
    expect(gridArrangements(5)).toEqual([
      { cols: 2, rows: 3 },
      { cols: 3, rows: 2 },
      { cols: 4, rows: 2 },
    ]);
  });
});
