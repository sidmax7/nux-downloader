// Block-grid geometry and bucketing for the per-file canvas.

export interface Cell {
  /** 0–100, averaged over the blocks the cell covers. */
  done: number;
  /** The link that delivered most of the cell's blocks. */
  link: number | null;
}

export interface Block {
  done: number;
  link: number | null;
}

/** The most cells ever drawn; a file has at most 4096 blocks. */
export const MAX_CELLS = 4096;

/** Merges blocks into at most `limit` cells, keeping order. */
export function bucketBlocks(blocks: Block[], limit: number): Cell[] {
  if (limit < 1) return [];
  if (blocks.length <= limit) return blocks.map((b) => ({ ...b }));
  const cells: Cell[] = [];
  for (let i = 0; i < limit; i += 1) {
    const from = Math.floor((i * blocks.length) / limit);
    const to = Math.max(from + 1, Math.floor(((i + 1) * blocks.length) / limit));
    let sum = 0;
    const votes = new Map<number | null, number>();
    for (let j = from; j < to; j += 1) {
      sum += blocks[j].done;
      votes.set(blocks[j].link, (votes.get(blocks[j].link) ?? 0) + 1);
    }
    let link: number | null = null;
    let best = 0;
    for (const [candidate, count] of votes) {
      if (candidate !== null && count > best) {
        best = count;
        link = candidate;
      }
    }
    cells.push({ done: Math.round(sum / (to - from)), link });
  }
  return cells;
}

export interface Geometry {
  columns: number;
  rows: number;
  cell: number;
}

/** Fits `count` square cells (plus a gap) into `width` pixels. */
export function gridGeometry(count: number, width: number, cell = 10, gap = 2): Geometry {
  const step = cell + gap;
  const columns = Math.max(1, Math.floor((width + gap) / step));
  return { columns, rows: Math.max(1, Math.ceil(count / columns)), cell };
}

/** How many cells fit when the grid may use `maxRows` rows. */
export function cellBudget(width: number, maxRows: number, cell = 10, gap = 2): number {
  const { columns } = gridGeometry(0, width, cell, gap);
  return Math.min(MAX_CELLS, columns * maxRows);
}
