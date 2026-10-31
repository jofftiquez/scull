// Squarified treemap layout (Bruls, Huizing & van Wijk, 2000), pure TypeScript. No chart
// library: the Storage Explorer draws the rectangles itself as SVG (D-6 is undecided).
//
// Items are laid out biggest first. Each row (or column) runs along the shorter side of the
// space left, and grows while adding the next item keeps the row's worst aspect ratio from
// getting worse; then it is fixed and the rest of the space is filled the same way.

export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface Tile<T> extends Rect {
  item: T;
  value: number;
}

/** Worst aspect ratio (>= 1) of a row of `areas` laid along a side of length `side`. */
export function worstRatio(areas: readonly number[], side: number): number {
  if (areas.length === 0 || side <= 0) return Infinity;
  let sum = 0;
  let max = -Infinity;
  let min = Infinity;
  for (const a of areas) {
    sum += a;
    if (a > max) max = a;
    if (a < min) min = a;
  }
  if (sum <= 0 || min <= 0) return Infinity;
  const s2 = side * side;
  const sum2 = sum * sum;
  return Math.max((s2 * max) / sum2, sum2 / (s2 * min));
}

/** Place one row of areas along the shorter side of `r`; returns the rects and the space left. */
function placeRow(areas: readonly number[], r: Rect): { rects: Rect[]; rest: Rect } {
  const sum = areas.reduce((a, b) => a + b, 0);
  const rects: Rect[] = [];
  if (r.w >= r.h) {
    // Wide space: a column on the left, items stacked top to bottom.
    const colW = r.h > 0 ? sum / r.h : 0;
    let y = r.y;
    for (const a of areas) {
      const h = colW > 0 ? a / colW : 0;
      rects.push({ x: r.x, y, w: colW, h });
      y += h;
    }
    return { rects, rest: { x: r.x + colW, y: r.y, w: Math.max(0, r.w - colW), h: r.h } };
  }
  // Tall space: a row on top, items left to right.
  const rowH = r.w > 0 ? sum / r.w : 0;
  let x = r.x;
  for (const a of areas) {
    const w = rowH > 0 ? a / rowH : 0;
    rects.push({ x, y: r.y, w, h: rowH });
    x += w;
  }
  return { rects, rest: { x: r.x, y: r.y + rowH, w: r.w, h: Math.max(0, r.h - rowH) } };
}

/**
 * Lay out `items` in `rect`, area proportional to `value(item)`. Items with a value <= 0 (or
 * not finite) are left out. Tiles come back biggest first; ties keep their input order.
 */
export function squarify<T>(items: readonly T[], value: (item: T) => number, rect: Rect): Tile<T>[] {
  const valid = items
    .map((item, i) => ({ item, i, v: value(item) }))
    .filter((e) => Number.isFinite(e.v) && e.v > 0)
    .sort((a, b) => b.v - a.v || a.i - b.i);
  const total = valid.reduce((a, e) => a + e.v, 0);
  if (valid.length === 0 || rect.w <= 0 || rect.h <= 0 || total <= 0) return [];
  const scale = (rect.w * rect.h) / total;
  const areas = valid.map((e) => e.v * scale);

  const out: Tile<T>[] = [];
  let space: Rect = { ...rect };
  let row: number[] = [];
  const flush = () => {
    const { rects, rest } = placeRow(
      row.map((k) => areas[k]!),
      space,
    );
    rects.forEach((r, j) => {
      const e = valid[row[j]!]!;
      out.push({ ...r, item: e.item, value: e.v });
    });
    space = rest;
    row = [];
  };

  let i = 0;
  while (i < valid.length) {
    const side = Math.min(space.w, space.h);
    const current = row.map((k) => areas[k]!);
    if (row.length === 0 || worstRatio([...current, areas[i]!], side) <= worstRatio(current, side)) {
      row.push(i);
      i += 1;
    } else {
      flush();
    }
  }
  if (row.length) flush();
  return out;
}

/**
 * A treemap of `width` x `height` with `gap` pixels between tiles and none at the edges: the
 * layout runs on a box `gap` larger, then every tile is inset by half the gap.
 */
export function layoutTreemap<T>(
  items: readonly T[],
  value: (item: T) => number,
  width: number,
  height: number,
  gap = 0,
): Tile<T>[] {
  const half = gap / 2;
  return squarify(items, value, { x: -half, y: -half, w: width + gap, h: height + gap }).map((t) => ({
    ...t,
    x: t.x + half,
    y: t.y + half,
    w: Math.max(0, t.w - gap),
    h: Math.max(0, t.h - gap),
  }));
}
