// The storage wheel (D-19): inner ring = categories, outer ring = items colored and patterned
// by safety tier. This module is pure (no Vue): it turns scan candidates into a model and the
// model into SVG geometry, so both are unit-tested.
//
// Arc sizes are on-disk (allocated) bytes: the wheel shows footprint. What a cleanup would
// free (reclaimable bytes) is reported next to it, never implied by area (PRD §14.3).
import type { CachesReport, Candidate, Category, ScanReport, Tier } from '../../bridge';
import type { TierFilter } from './tiers';
import { TIER_KEYS, mostProtective } from './tiers';

/** Category order around the wheel (clockwise from 12 o'clock) and inner-ring labels. */
export const CATEGORIES: readonly { key: Category; label: string }[] = [
  { key: 'project_artifact', label: 'Build outputs' },
  { key: 'package_cache', label: 'Caches' },
  { key: 'worktree', label: 'Worktrees' },
  { key: 'ml_cache', label: 'ML & models' },
  { key: 'container', label: 'Containers' },
  { key: 'large_file', label: 'Large files' },
  { key: 'other', label: 'Other' },
  { key: 'scull_data', label: 'scull data' },
];

export function categoryLabel(c: Category): string {
  return CATEGORIES.find((x) => x.key === c)?.label ?? c;
}

export interface WheelItem {
  /** Candidate ID, or `<category>:rest:<tier>` for a group of small items. */
  id: string;
  name: string;
  category: Category;
  tier: Tier;
  /** On-disk bytes (arc size). */
  bytes: number;
  /** Bytes a cleanup is expected to free. */
  reclaimable: number;
  /** Candidates this segment stands for (1, or more for a group of small items). */
  count: number;
  /** The candidate, for single items. */
  candidate: Candidate | null;
  /** IDs of the candidates behind the segment. */
  candidateIds: string[];
}

export interface WheelCategory {
  key: Category;
  label: string;
  bytes: number;
  reclaimable: number;
  count: number;
  /** Badge: the most protective tier inside (never a safe tier over S2/S3 bytes). */
  tier: Tier;
  items: WheelItem[];
}

export interface TierTotal {
  bytes: number;
  reclaimable: number;
  count: number;
}

export interface WheelModel {
  categories: WheelCategory[];
  total: number;
  /** Candidates drawn (zero-byte candidates are counted in `byTier` but have no arc). */
  itemCount: number;
  byTier: Record<Tier, TierTotal>;
}

/** Items smaller than this angle are folded into one "N smaller items" segment per tier. */
export const MIN_ITEM_DEG = 2.5;

/**
 * Candidates of a scan plus the package caches, each once. Caches found inside a scan root
 * appear in both reports; the scan's copy wins.
 */
export function collectCandidates(report?: ScanReport | null, caches?: CachesReport | null): Candidate[] {
  const out: Candidate[] = [];
  const ids = new Set<string>();
  const paths = new Set<string>();
  for (const c of [...(report?.candidates ?? []), ...(caches?.candidates ?? [])]) {
    if (ids.has(c.id) || paths.has(c.path)) continue;
    ids.add(c.id);
    paths.add(c.path);
    out.push(c);
  }
  return out;
}

/** "rust-svc · target", or the artifact name ("npm cache") outside a project. */
export function itemName(c: Candidate): string {
  const base = c.path.split(/[\\/]/).filter(Boolean).pop() ?? c.path;
  if (c.project) return `${c.project.name} · ${base}`;
  return c.artifact || base;
}

const emptyTotals = (): Record<Tier, TierTotal> => ({
  S0: { bytes: 0, reclaimable: 0, count: 0 },
  S1: { bytes: 0, reclaimable: 0, count: 0 },
  S2: { bytes: 0, reclaimable: 0, count: 0 },
  S3: { bytes: 0, reclaimable: 0, count: 0 },
});

export function buildWheelModel(candidates: readonly Candidate[], minItemDeg = MIN_ITEM_DEG): WheelModel {
  const byTier = emptyTotals();
  const groups = new Map<Category, Candidate[]>();
  let total = 0;
  let itemCount = 0;
  for (const c of candidates) {
    const bytes = Math.max(0, c.sizes.allocated);
    const t = byTier[c.tier];
    t.bytes += bytes;
    t.reclaimable += Math.max(0, c.sizes.reclaimable.bytes);
    t.count += 1;
    if (bytes === 0) continue;
    total += bytes;
    itemCount += 1;
    const list = groups.get(c.category) ?? [];
    list.push(c);
    groups.set(c.category, list);
  }

  const categories: WheelCategory[] = [];
  for (const { key, label } of CATEGORIES) {
    const list = groups.get(key);
    if (!list?.length) continue;
    list.sort((a, b) => b.sizes.allocated - a.sizes.allocated || a.path.localeCompare(b.path));
    const items: WheelItem[] = [];
    const small: Record<Tier, Candidate[]> = { S0: [], S1: [], S2: [], S3: [] };
    for (const c of list) {
      if ((c.sizes.allocated / total) * 360 >= minItemDeg) items.push(single(c));
      else small[c.tier].push(c);
    }
    // Small items are grouped per tier, never across tiers: a group is exactly one tier.
    for (const tier of TIER_KEYS) {
      const rest = small[tier];
      if (rest.length === 1) items.push(single(rest[0]!));
      else if (rest.length > 1) {
        items.push({
          id: `${key}:rest:${tier}`,
          name: `${rest.length} smaller items`,
          category: key,
          tier,
          bytes: rest.reduce((a, c) => a + c.sizes.allocated, 0),
          reclaimable: rest.reduce((a, c) => a + c.sizes.reclaimable.bytes, 0),
          count: rest.length,
          candidate: null,
          candidateIds: rest.map((c) => c.id),
        });
      }
    }
    categories.push({
      key,
      label,
      bytes: items.reduce((a, i) => a + i.bytes, 0),
      reclaimable: items.reduce((a, i) => a + i.reclaimable, 0),
      count: list.length,
      tier: mostProtective(items.map((i) => i.tier))!,
      items,
    });
  }
  return { categories, total, itemCount, byTier };
}

function single(c: Candidate): WheelItem {
  return {
    id: c.id,
    name: itemName(c),
    category: c.category,
    tier: c.tier,
    bytes: c.sizes.allocated,
    reclaimable: c.sizes.reclaimable.bytes,
    count: 1,
    candidate: c,
    candidateIds: [c.id],
  };
}

// ---- Geometry (viewBox 620 × 620, design/source/Main.dc.html) ----

export const GEOMETRY = {
  size: 620,
  c: 310,
  /** Dotted magenta ring, then the faint violet ring. */
  dotted: 300,
  faint: 286,
  categoryInner: 122,
  categoryOuter: 180,
  itemInner: 186,
  itemOuter: 268,
  /** Paper disc in the middle. */
  hub: 114,
  /** Category labels sit on this radius. */
  labelRadius: 151,
  /** Angular gap on each side of every segment, in degrees. */
  pad: 0.7,
  /** Radar arm length and the circumference the reveal mask sweeps (2π·140). */
  armLength: 274,
  maskRadius: 140,
  maskWidth: 282,
  maskLength: 880,
} as const;

/** Point at `deg` degrees clockwise from 12 o'clock. */
export function polar(r: number, deg: number, c = GEOMETRY.c): [number, number] {
  const a = (deg * Math.PI) / 180;
  return [c + r * Math.sin(a), c - r * Math.cos(a)];
}

const f = (n: number) => n.toFixed(1);

/** Annular sector from `a0` to `a1` degrees between radii `r0` < `r1`, with `pad` trimmed off each side. */
export function sectorPath(r0: number, r1: number, a0: number, a1: number, pad: number = GEOMETRY.pad): string {
  const span = a1 - a0;
  // Very thin slivers keep a visible sliver instead of disappearing under the padding.
  const p = span > pad * 2 + 0.4 ? pad : span * 0.15;
  const s = a0 + p;
  const e = a1 - p;
  const large = e - s > 180 ? 1 : 0;
  const [x0, y0] = polar(r1, s);
  const [x1, y1] = polar(r1, e);
  const [x2, y2] = polar(r0, e);
  const [x3, y3] = polar(r0, s);
  return (
    `M${f(x0)} ${f(y0)}A${r1} ${r1} 0 ${large} 1 ${f(x1)} ${f(y1)}` +
    `L${f(x2)} ${f(y2)}A${r0} ${r0} 0 ${large} 0 ${f(x3)} ${f(y3)}Z`
  );
}

export interface ItemArc {
  item: WheelItem;
  start: number;
  end: number;
  d: string;
}

export interface CategoryArc {
  category: WheelCategory;
  start: number;
  end: number;
  d: string;
  /** Label placement, or null when the arc is too short for the text. */
  label: { x: number; y: number; rotate: number } | null;
  items: ItemArc[];
}

/** Rough width of an 11px DM Sans label (for "does it fit" only). */
export function labelWidth(text: string): number {
  return text.length * 6.2;
}

/**
 * Tangential label at the arc's middle; text on the lower half is turned 180° so it never
 * reads upside down (as in the design: "ML & models" at 6.5°, "Worktrees" at 296.8°).
 */
export function labelPlacement(start: number, end: number, text: string): CategoryArc['label'] {
  const arc = (((end - start) * Math.PI) / 180) * GEOMETRY.labelRadius;
  if (labelWidth(text) + 16 > arc) return null;
  const mid = (start + end) / 2;
  const [x, y] = polar(GEOMETRY.labelRadius, mid);
  const rotate = mid > 90 && mid < 270 ? mid - 180 : mid;
  return { x: Number(f(x)), y: Number(f(y)), rotate: Number(f(((rotate % 360) + 360) % 360)) };
}

export function layoutWheel(model: WheelModel): CategoryArc[] {
  const g = GEOMETRY;
  if (model.total <= 0) return [];
  const out: CategoryArc[] = [];
  let acc = 0;
  for (const cat of model.categories) {
    const start = (acc / model.total) * 360;
    const items: ItemArc[] = [];
    for (const item of cat.items) {
      const a0 = (acc / model.total) * 360;
      acc += item.bytes;
      const a1 = (acc / model.total) * 360;
      items.push({ item, start: a0, end: a1, d: sectorPath(g.itemInner, g.itemOuter, a0, a1) });
    }
    const end = (acc / model.total) * 360;
    out.push({
      category: cat,
      start,
      end,
      d: sectorPath(g.categoryInner, g.categoryOuter, start, end),
      label: labelPlacement(start, end, cat.label),
      items,
    });
  }
  return out;
}

/** Whether an item passes the toolbar's tier filter. */
export function matchesFilter(tier: Tier, filter: TierFilter): boolean {
  return filter === 'all' || filter === tier;
}

/**
 * Segment opacity, as in the design: just-cleaned items fade to 0.1, items outside the tier
 * filter to 0.14, unselected items to 0.5 while something is selected.
 */
export function itemOpacity(
  item: WheelItem,
  s: { filter: TierFilter; selected: string | null; cleaned?: ReadonlySet<string> },
): number {
  if (s.cleaned?.has(item.id)) return 0.1;
  if (!matchesFilter(item.tier, s.filter)) return 0.14;
  if (s.selected && s.selected !== item.id && s.selected !== item.category) return 0.5;
  return 1;
}
