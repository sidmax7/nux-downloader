import { describe, expect, it } from "vitest";
import { MAX_CELLS, bucketBlocks, cellBudget, gridGeometry } from "./grid";

const blocks = (n: number, done = 100, link: number | null = 1) =>
  Array.from({ length: n }, () => ({ done, link }));

describe("bucketBlocks", () => {
  it("keeps blocks as they are when they fit", () => {
    expect(bucketBlocks(blocks(5), 10)).toHaveLength(5);
  });

  it("merges to the limit, in order", () => {
    const input = [...blocks(50, 0, null), ...blocks(50, 100, 2)];
    const cells = bucketBlocks(input, 10);
    expect(cells).toHaveLength(10);
    expect(cells.slice(0, 5).every((c) => c.done === 0)).toBe(true);
    expect(cells.slice(5).every((c) => c.done === 100 && c.link === 2)).toBe(true);
  });

  it("averages progress inside a cell", () => {
    const input = [...blocks(2, 0, null), ...blocks(2, 100, 3)];
    expect(bucketBlocks(input, 1)).toEqual([{ done: 50, link: 3 }]);
  });

  it("picks the link that delivered most", () => {
    const input = [
      { done: 100, link: 1 },
      { done: 100, link: 2 },
      { done: 100, link: 2 },
    ];
    expect(bucketBlocks(input, 1)[0].link).toBe(2);
  });

  it("copes with a zero limit and no blocks", () => {
    expect(bucketBlocks(blocks(3), 0)).toEqual([]);
    expect(bucketBlocks([], 5)).toEqual([]);
  });

  it("handles the largest file (4096 blocks) into a small budget", () => {
    expect(bucketBlocks(blocks(4096), 100)).toHaveLength(100);
  });
});

describe("gridGeometry", () => {
  it("fits columns into the width", () => {
    // cell 10 + gap 2 = 12 per step; 100px fits 8 columns (96px + 4 spare)
    expect(gridGeometry(20, 100)).toEqual({ columns: 8, rows: 3, cell: 10 });
  });

  it("always has one column and one row", () => {
    expect(gridGeometry(0, 3)).toEqual({ columns: 1, rows: 1, cell: 10 });
  });

  it("limits the budget by rows and by the block maximum", () => {
    expect(cellBudget(120, 2)).toBe(20);
    expect(cellBudget(100000, 1000)).toBe(MAX_CELLS);
  });
});
