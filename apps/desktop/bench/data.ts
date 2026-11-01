// Deterministic datasets for the chart benchmark (P3-CHART-01, D-6). Not shipped: nothing in
// src/ imports bench/. The Insights series follow design/source/Insights.dc.html; the hierarchy
// is a seeded 100K-leaf tree shaped like a scan (8 categories, Pareto item sizes, 4 tiers).

export type Tier = 'S0' | 'S1' | 'S2' | 'S3';

/** mulberry32: tiny seeded PRNG so every run draws the same data. */
export function rng(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

// ---- Insights (design 05) ----

export interface ForecastPoint {
  day: number;
  gb: number;
}
export interface BandPoint {
  day: number;
  gb: number;
  lo: number;
  hi: number;
}

/** 30 measured days (two cleanups), then a 9-day forecast with a widening confidence band. */
export const FORECAST: { measured: ForecastPoint[]; forecast: BandPoint[]; cleanups: number[]; floorGb: number } =
  (() => {
    const measured: ForecastPoint[] = [];
    let gb = 96;
    for (let day = 0; day <= 30; day++) {
      if (day === 8) gb += 22;
      if (day === 19) gb += 9;
      measured.push({ day, gb: Math.round(gb * 10) / 10 });
      gb -= 2.86; // 96 GB → 41.2 GB today, with +22 GB and +9 GB cleanups
    }
    const last = measured[measured.length - 1]!;
    const forecast: BandPoint[] = [];
    for (let i = 0; i <= 9; i++) {
      const v = last.gb - 3.5 * i;
      const spread = 0.9 * i;
      forecast.push({ day: 30 + i, gb: v, lo: v - spread, hi: v + spread });
    }
    return { measured, forecast, cleanups: [8, 19], floorGb: 10 };
  })();

/** shop-api/node_modules: installed (full) and cleaned (near zero), alternating. */
export const REGROWTH: { label: string; gb: number; state: 'installed' | 'cleaned' }[] = Array.from(
  { length: 9 },
  (_, i) => ({
    label: `Week ${Math.floor(i / 2) + 1}${i % 2 ? ' clean' : ''}`,
    gb: i % 2 ? 0.1 : 1.2,
    state: i % 2 ? 'cleaned' : 'installed',
  }),
);

/** Identical packages across projects. */
export const DUPLICATION: { pkg: string; copies: number; gb: number }[] = [
  { pkg: 'typescript@5.9', copies: 17, gb: 1.4 },
  { pkg: 'next@16.1', copies: 6, gb: 2.1 },
  { pkg: 'torch@2.9', copies: 3, gb: 3.9 },
];

export const ECOSYSTEMS = ['ml', 'js', 'rust', 'other'] as const;
export type Ecosystem = (typeof ECOSYSTEMS)[number];
export const ECOSYSTEM_LABEL: Record<Ecosystem, string> = {
  ml: 'ML / AI',
  js: 'JavaScript',
  rust: 'Rust',
  other: 'Other',
};

/** Development storage by ecosystem per month (GB). */
export const ECOSYSTEM_MIX: ({ month: string } & Record<Ecosystem, number>)[] = [
  { month: 'May', ml: 12, js: 41, rust: 27, other: 20 },
  { month: 'Jun', ml: 15, js: 39, rust: 27, other: 19 },
  { month: 'Jul', ml: 19, js: 35, rust: 27, other: 19 },
  { month: 'Aug', ml: 24, js: 31, rust: 26, other: 19 },
  { month: 'Sep', ml: 29, js: 27, rust: 25, other: 19 },
  { month: 'Oct', ml: 34, js: 24, rust: 24, other: 18 },
].map((m, i) => {
  // Totals grow 62% → 100% like the design's bar heights.
  const scale = [0.62, 0.66, 0.72, 0.8, 0.9, 1][i]! * 1.6;
  return {
    month: m.month,
    ml: m.ml * scale,
    js: m.js * scale,
    rust: m.rust * scale,
    other: m.other * scale,
  };
});

// ---- Hierarchy for the wheel / treemap (100K+ nodes) ----

export interface Leaf {
  name: string;
  value: number;
  tier: Tier;
}
export interface Category {
  name: string;
  children: Leaf[];
}
export interface Tree {
  name: string;
  children: Category[];
}

export const CATEGORY_NAMES = [
  'Build outputs',
  'Caches',
  'Worktrees',
  'ML & models',
  'Containers',
  'Large files',
  'Other',
  'scull data',
];

function pickTier(u: number): Tier {
  return u < 0.45 ? 'S0' : u < 0.7 ? 'S1' : u < 0.9 ? 'S2' : 'S3';
}

/** A seeded tree with `leaves` items spread over 8 categories (Pareto sizes). */
export function makeTree(leaves = 100_000, seed = 42): Tree {
  const r = rng(seed);
  const per = Math.ceil(leaves / CATEGORY_NAMES.length);
  let left = leaves;
  const children: Category[] = CATEGORY_NAMES.map((name, ci) => {
    const n = Math.min(per, left);
    left -= n;
    const items: Leaf[] = new Array(n);
    for (let i = 0; i < n; i++) {
      const size = Math.floor(4096 * Math.pow(1 - r(), -1 / 1.1)) * (ci + 1);
      items[i] = { name: `${name} item ${i}`, value: Math.min(size, 64 * 2 ** 30), tier: pickTier(r()) };
    }
    return { name, children: items };
  });
  return { name: 'root', children };
}

/** Number of nodes (root + categories + leaves). */
export function nodeCount(t: Tree): number {
  return 1 + t.children.reduce((a, c) => a + 1 + c.children.length, 0);
}

export const TIER_ORDER: readonly Tier[] = ['S0', 'S1', 'S2', 'S3'];

/**
 * The app's aggregation (src/components/overview/wheel.ts, MIN_ITEM_DEG): items under
 * `minDeg` of the whole wheel fold into one "N smaller items" segment per category and tier.
 */
export function aggregate(t: Tree, minDeg = 2.5): Tree {
  let total = 0;
  for (const c of t.children) for (const l of c.children) total += l.value;
  return {
    name: t.name,
    children: t.children.map((c) => {
      const kept: Leaf[] = [];
      const small: Record<Tier, { n: number; v: number }> = {
        S0: { n: 0, v: 0 },
        S1: { n: 0, v: 0 },
        S2: { n: 0, v: 0 },
        S3: { n: 0, v: 0 },
      };
      for (const l of c.children) {
        if ((l.value / total) * 360 >= minDeg) kept.push(l);
        else {
          small[l.tier].n += 1;
          small[l.tier].v += l.value;
        }
      }
      for (const tier of TIER_ORDER) {
        const s = small[tier];
        if (s.n > 0) kept.push({ name: `${c.name}: ${s.n} smaller items`, value: s.v, tier });
      }
      return { name: c.name, children: kept };
    }),
  };
}
