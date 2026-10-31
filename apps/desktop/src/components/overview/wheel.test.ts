import { describe, expect, it } from 'vitest';
import type { Candidate, Category, Tier } from '../../bridge';
import { MOCK_DATA } from '../../bridge/mock';
import { DESIGN_CANDIDATES } from './designScenario';
import { mostProtective, tierRank, TIER_KEYS } from './tiers';
import {
  buildWheelModel,
  collectCandidates,
  itemName,
  itemOpacity,
  labelPlacement,
  layoutWheel,
  matchesFilter,
  sectorPath,
} from './wheel';
import { shortPath, wheelCenter } from './center';

let n = 0;
function cand(category: Category, tier: Tier, bytes: number, extra: Partial<Candidate> = {}): Candidate {
  const id = `c${++n}`;
  return {
    action: { type: 'none' },
    artifact: `artifact ${id}`,
    category,
    evidence: [],
    flags: {},
    id,
    identity: { file: id, volume: 1 },
    path: `/Users/ada/${id}`,
    project: null,
    reasons: [],
    regeneration: { cost: { type: 'unknown' }, instructions: '' },
    rule: { id: 'test', version: 1 },
    sizes: { allocated: bytes, logical: bytes, reclaimable: { bytes, confidence: 'high', shared_bytes: 0 } },
    tier,
    ...extra,
  };
}

describe('tiers', () => {
  it('orders tiers by protection and picks the most protective', () => {
    expect(TIER_KEYS.map(tierRank)).toEqual([0, 1, 2, 3]);
    expect(mostProtective(['S0', 'S2', 'S1'])).toBe('S2');
    expect(mostProtective(['S3', 'S0'])).toBe('S3');
    expect(mostProtective([])).toBeNull();
  });
});

describe('wheel model', () => {
  it('groups candidates by category in wheel order, items largest first', () => {
    const m = buildWheelModel([
      cand('package_cache', 'S1', 10),
      cand('project_artifact', 'S0', 5),
      cand('project_artifact', 'S0', 50),
      cand('worktree', 'S2', 35),
    ]);
    expect(m.categories.map((c) => c.key)).toEqual(['project_artifact', 'package_cache', 'worktree']);
    expect(m.categories[0]!.items.map((i) => i.bytes)).toEqual([50, 5]);
    expect(m.total).toBe(100);
    expect(m.itemCount).toBe(4);
    expect(m.byTier.S0).toEqual({ bytes: 55, reclaimable: 55, count: 2 });
  });

  it('badges a category with its most protective tier, never a safe tier over S2/S3 bytes', () => {
    const m = buildWheelModel([
      cand('project_artifact', 'S0', 1_000_000),
      cand('project_artifact', 'S2', 1),
      cand('package_cache', 'S1', 500),
      cand('package_cache', 'S3', 1),
      cand('ml_cache', 'S0', 10),
      cand('ml_cache', 'S1', 10),
    ]);
    const tier = Object.fromEntries(m.categories.map((c) => [c.key, c.tier]));
    expect(tier).toEqual({ project_artifact: 'S2', package_cache: 'S3', ml_cache: 'S1' });
  });

  it('keeps the badge rule for every mix of tiers and sizes', () => {
    // Exhaustive over tier subsets; sizes skewed so the risky part is always tiny.
    for (let mask = 1; mask < 16; mask++) {
      const tiers = TIER_KEYS.filter((_, i) => mask & (1 << i));
      const cands = tiers.map((t, i) => cand('project_artifact', t, i === 0 ? 10 ** 9 : 1));
      const cat = buildWheelModel(cands).categories[0]!;
      const risky = cat.items.some((i) => i.tier === 'S2' || i.tier === 'S3');
      if (risky) expect(['S2', 'S3']).toContain(cat.tier);
      expect(tierRank(cat.tier)).toBe(Math.max(...tiers.map(tierRank)));
    }
  });

  it('folds tiny items into one group per tier, never across tiers', () => {
    const cands = [
      cand('project_artifact', 'S0', 10_000),
      ...Array.from({ length: 5 }, () => cand('project_artifact', 'S0', 1)),
      ...Array.from({ length: 3 }, () => cand('project_artifact', 'S2', 1)),
      cand('project_artifact', 'S3', 1),
    ];
    const items = buildWheelModel(cands).categories[0]!.items;
    expect(items.map((i) => [i.tier, i.count])).toEqual([
      ['S0', 1],
      ['S0', 5],
      ['S2', 3],
      ['S3', 1],
    ]);
    const s2 = items.find((i) => i.id === 'project_artifact:rest:S2')!;
    expect(s2.name).toBe('3 smaller items');
    expect(s2.candidateIds).toHaveLength(3);
    expect(items.reduce((a, i) => a + i.bytes, 0)).toBe(10_009);
  });

  it('counts zero-byte candidates but gives them no arc', () => {
    const m = buildWheelModel([cand('package_cache', 'S1', 0), cand('package_cache', 'S1', 7)]);
    expect(m.byTier.S1.count).toBe(2);
    expect(m.itemCount).toBe(1);
    expect(m.categories[0]!.items).toHaveLength(1);
  });

  it('merges scan and cache candidates once (by ID, then by path)', () => {
    const a = cand('package_cache', 'S1', 5);
    const b = cand('package_cache', 'S1', 5, { path: a.path });
    const c = cand('project_artifact', 'S0', 5);
    const scan = { ...MOCK_DATA.scanReport, candidates: [a, c] };
    const caches = { ...MOCK_DATA.cachesReport, candidates: [a, b] };
    expect(collectCandidates(scan, caches).map((x) => x.id)).toEqual([a.id, c.id]);
    expect(collectCandidates(null, null)).toEqual([]);
  });

  it('names items by project and folder, else by artifact', () => {
    expect(itemName(cand('project_artifact', 'S0', 1, { path: '/a/rust-svc/target', project: { ecosystem: 'rust', name: 'rust-svc', root: '/a/rust-svc' } }))).toBe('rust-svc · target');
    expect(itemName(cand('package_cache', 'S1', 1, { artifact: 'npm cache' }))).toBe('npm cache');
  });

  it('reads the recorded mock scan', () => {
    const m = buildWheelModel(collectCandidates(MOCK_DATA.scanReport, MOCK_DATA.cachesReport));
    expect(m.itemCount).toBe(MOCK_DATA.scanReport.candidates.length + MOCK_DATA.cachesReport.candidates.length);
    for (const c of m.categories) {
      if (c.items.some((i) => i.tier === 'S3')) expect(c.tier).toBe('S3');
    }
  });
});

describe('wheel geometry', () => {
  const arcs = layoutWheel(buildWheelModel(DESIGN_CANDIDATES));

  it('reproduces the design wheel (design/source/Main.dc.html) from the design data', () => {
    const build = arcs[0]!;
    expect(build.category.label).toBe('Build outputs');
    expect(build.d).toBe('M312.2 130.0A180 180 0 0 1 467.5 222.8L416.7 250.9A122 122 0 0 0 311.5 188.0Z');
    expect(build.items[0]!.d).toBe('M313.3 42.0A268 268 0 0 1 438.8 75.0L399.4 146.9A186 186 0 0 0 312.3 124.0Z');
    expect(build.label).toEqual({ x: 387.5, y: 180.4, rotate: 30.9 });
    const hf = arcs.flatMap((a) => a.items).find((a) => a.item.id === 'design-hf')!;
    expect(hf.d).toBe('M468.5 526.1A268 268 0 0 1 206.5 557.2L238.2 481.6A186 186 0 0 0 420.0 460.0Z');
  });

  it('turns lower-half labels so they never read upside down', () => {
    const rot = Object.fromEntries(arcs.map((a) => [a.category.label, a.label?.rotate ?? null]));
    expect(rot['Worktrees']).toBeCloseTo(296.8, 0);
    expect(rot['ML & models']).toBeCloseTo(6.5, 0);
    expect(rot['Containers']).toBeCloseTo(67.4, 0);
    // The design's unlabelled 8.2 GB slice is too short for its label.
    expect(rot['Large files']).toBeNull();
    expect(labelPlacement(0, 1, 'Caches')).toBeNull();
  });

  it('covers the full circle in order', () => {
    expect(arcs[0]!.start).toBe(0);
    expect(arcs.at(-1)!.end).toBeCloseTo(360, 6);
    for (let i = 1; i < arcs.length; i++) expect(arcs[i]!.start).toBeCloseTo(arcs[i - 1]!.end, 9);
    for (const a of arcs) {
      expect(a.items[0]!.start).toBeCloseTo(a.start, 9);
      expect(a.items.at(-1)!.end).toBeCloseTo(a.end, 9);
    }
  });

  it('uses the large-arc flag past 180° and keeps slivers visible', () => {
    expect(sectorPath(10, 20, 0, 300)).toContain(' 0 1 1 ');
    expect(sectorPath(10, 20, 0, 100)).toContain(' 0 0 1 ');
    const sliver = sectorPath(186, 268, 10, 10.5);
    expect(sliver).not.toMatch(/NaN/);
    expect(layoutWheel(buildWheelModel([]))).toEqual([]);
  });
});

describe('filter and selection', () => {
  const m = buildWheelModel(DESIGN_CANDIDATES);
  const items = m.categories.flatMap((c) => c.items);

  it('dims everything outside the tier filter', () => {
    expect(matchesFilter('S2', 'all')).toBe(true);
    expect(matchesFilter('S2', 'S0')).toBe(false);
    for (const it of items) expect(itemOpacity(it, { filter: 'S2', selected: null })).toBe(it.tier === 'S2' ? 1 : 0.14);
  });

  it('half-dims the rest while something is selected; a category keeps its own items', () => {
    const hf = items.find((i) => i.id === 'design-hf')!;
    const npm = items.find((i) => i.id === 'design-npm')!;
    expect(itemOpacity(hf, { filter: 'all', selected: 'design-hf' })).toBe(1);
    expect(itemOpacity(npm, { filter: 'all', selected: 'design-hf' })).toBe(0.5);
    expect(itemOpacity(npm, { filter: 'all', selected: 'package_cache' })).toBe(1);
    expect(itemOpacity(npm, { filter: 'all', selected: null, cleaned: new Set(['design-npm']) })).toBe(0.1);
  });

  it('says what the center shows', () => {
    expect(wheelCenter({ kind: 'idle', model: m, selected: null })).toMatchObject({ k: 'Development', v: '186.0 GiB', n: '7 categories · 16 items' });
    expect(wheelCenter({ kind: 'idle', model: m, selected: 'design-hf' })).toMatchObject({ k: 'Costly', v: '31.2 GiB', n: 'Hugging Face models' });
    expect(wheelCenter({ kind: 'idle', model: m, selected: 'worktree' })).toMatchObject({ k: 'Protected', n: 'Worktrees · 3 items' });
    expect(wheelCenter({ kind: 'idle', model: null, selected: null }).k).toBe('Not scanned');
    expect(wheelCenter({ kind: 'idle', model: buildWheelModel([]), selected: null }).k).toBe('Nothing found');
    expect(wheelCenter({ kind: 'scanning', entries: 276630, where: '/Users/ada/Library/Containers/Docker' })).toMatchObject({
      k: 'Scanning…',
      v: '276,630',
      n: '~/Library/Containers/Docker',
    });
    expect(shortPath('/home/ada/a/very/long/path/that/goes/on/and/on/forever/x')).toMatch(/^….{33}$/);
  });
});
