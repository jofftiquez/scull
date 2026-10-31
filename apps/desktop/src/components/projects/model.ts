// Projects screen (P3-UI-06; design 06): pure logic over the core's ProjectsView. Ecosystem
// filters, sorting, tier bars, the per-project detail and the clean state machine.
import type { ProjectSummary, Tier, TierBreakdown } from '../../bridge';

export const TIER_LABEL: Record<Tier, string> = { S0: 'Regenerable', S1: 'Costly', S2: 'Uncertain', S3: 'Protected' };
export const TIERS: readonly Tier[] = ['S0', 'S1', 'S2', 'S3'];
const KEY: Record<Tier, keyof TierBreakdown> = { S0: 's0', S1: 's1', S2: 's2', S3: 's3' };

/** Display names of the core's ecosystem IDs (scull-engine PROJECT_MARKERS, detectors). */
const ECO_LABEL: Record<string, string> = {
  node: 'JS/TS',
  deno: 'Deno',
  rust: 'Rust',
  python: 'Python',
  ml: 'ML',
  go: 'Go',
  java: 'Java',
  ruby: 'Ruby',
  php: 'PHP',
  swift: 'Swift',
  cmake: 'C/C++',
  dart: 'Dart',
  elixir: 'Elixir',
  unity: 'Game',
  unreal: 'Game',
  godot: 'Game',
  docker: 'Docker',
};

export function ecosystemLabel(id: string): string {
  return ECO_LABEL[id] ?? id.charAt(0).toUpperCase() + id.slice(1);
}

/** Avatar tone and initials per ecosystem (design 06). */
export function avatar(p: Pick<ProjectSummary, 'ecosystems'>): { initials: string; tone: 'js' | 'rust' | 'ml' | 'game' | 'other' } {
  const e = p.ecosystems[0] ?? '';
  const initials: Record<string, string> = { node: 'JS', deno: 'JS', rust: 'RS', python: 'PY', ml: 'ML', go: 'GO', unity: 'UN' };
  const tone = ['node', 'deno'].includes(e)
    ? 'js'
    : e === 'rust'
      ? 'rust'
      : ['python', 'ml'].includes(e)
        ? 'ml'
        : ['unity', 'unreal', 'godot'].includes(e)
          ? 'game'
          : 'other';
  return { initials: initials[e] ?? (e.slice(0, 2).toUpperCase() || '··'), tone };
}

export const ALL = 'all';

export interface Filter {
  id: string;
  label: string;
  count: number;
}

/** "All", then one filter per ecosystem label present, biggest first. Labels that map to the
 * same name (unity/unreal → Game) share one filter. */
export function filters(projects: readonly ProjectSummary[]): Filter[] {
  const by = new Map<string, { size: number; count: number }>();
  for (const p of projects) {
    for (const label of new Set(p.ecosystems.map(ecosystemLabel))) {
      const f = by.get(label) ?? { size: 0, count: 0 };
      f.size += p.total_allocated ?? 0;
      f.count += 1;
      by.set(label, f);
    }
  }
  const rest = [...by.entries()]
    .sort((a, b) => b[1].size - a[1].size || a[0].localeCompare(b[0]))
    .map(([label, f]) => ({ id: label, label, count: f.count }));
  return [{ id: ALL, label: 'All', count: projects.length }, ...rest];
}

export function matches(p: ProjectSummary, filter: string): boolean {
  return filter === ALL || p.ecosystems.some((e) => ecosystemLabel(e) === filter);
}

const sum = (t: TierBreakdown) => t.s0 + t.s1 + t.s2 + t.s3;

/** Bytes of the project: measured total, else what its candidates add up to, else unknown. */
export function projectSize(p: ProjectSummary): number | null {
  if (p.total_allocated != null) return p.total_allocated;
  const s = sum(p.allocated);
  return s > 0 ? s : null;
}

/** Biggest first; unknown sizes last; then by name. */
export function sortBySize(list: readonly ProjectSummary[]): ProjectSummary[] {
  return [...list].sort((a, b) => {
    const x = projectSize(a);
    const y = projectSize(b);
    if (x === null && y !== null) return 1;
    if (y === null && x !== null) return -1;
    return (y ?? 0) - (x ?? 0) || a.name.localeCompare(b.name);
  });
}

export interface BarSegment {
  tier: Tier;
  pct: number;
}

/** Tier bar: each tier's share of the project's size (the rest is files that aren't candidates). */
export function tierBar(p: ProjectSummary): BarSegment[] {
  const total = Math.max(projectSize(p) ?? 0, sum(p.allocated));
  return TIERS.map((tier) => ({ tier, pct: total > 0 ? (p.allocated[KEY[tier]] / total) * 100 : 0 }));
}

export interface InsideRow {
  tier: Tier | null;
  label: string;
  bytes: number;
}

/** "What's inside": bytes per tier, then files that are not candidates (never offered). */
export function inside(p: ProjectSummary): InsideRow[] {
  const rows: InsideRow[] = TIERS.filter((t) => p.allocated[KEY[t]] > 0).map((t) => ({
    tier: t,
    label: TIER_LABEL[t],
    bytes: p.allocated[KEY[t]],
  }));
  const size = projectSize(p);
  const rest = size === null ? 0 : size - sum(p.allocated);
  if (rest > 0) rows.push({ tier: null, label: 'Everything else', bytes: rest });
  return rows;
}

/** What the clean button may remove here: S0 bytes only (regenerable, PRD §8.3). */
export function regenerable(p: ProjectSummary): number {
  return p.reclaimable.s0;
}

/**
 * The project as it reads while (`fraction` 0–1) and after (`1`) its regenerable bytes go:
 * S0 shrinks with the design's ease-out cubic, and the size shrinks with it.
 */
export function shrink(p: ProjectSummary, fraction: number): ProjectSummary {
  const f = Math.min(1, Math.max(0, fraction));
  const k = (1 - f) ** 3; // what is left: 1 - easeOutCubic(f)
  const gone = p.allocated.s0 - Math.round(p.allocated.s0 * k);
  return {
    ...p,
    allocated: { ...p.allocated, s0: p.allocated.s0 - gone },
    reclaimable: { ...p.reclaimable, s0: Math.round(p.reclaimable.s0 * k) },
    total_allocated: p.total_allocated == null ? p.total_allocated : Math.max(0, p.total_allocated - gone),
    badge: f >= 1 && p.allocated.s1 + p.allocated.s2 + p.allocated.s3 === 0 ? null : p.badge,
  };
}

/** `~/code/api` for paths under a home folder (/Users/x, /home/x, C:\Users\x). */
export function shortPath(path: string): string {
  return path
    .replace(/^\/(?:Users|home)\/[^/]+(?=\/|$)/, '~')
    .replace(/^[A-Za-z]:\\Users\\[^\\]+(?=\\|$)/, '~');
}

/** "3 days ago" from Unix seconds; unmeasured reads "unknown". */
export function lastActivity(mtime: number | null | undefined, nowSecs: number): string {
  if (mtime == null) return 'unknown';
  const s = Math.max(0, nowSecs - mtime);
  if (s < 3600) return s < 120 ? 'just now' : `${Math.floor(s / 60)} min ago`;
  if (s < 86400) {
    const h = Math.floor(s / 3600);
    return `${h} hour${h === 1 ? '' : 's'} ago`;
  }
  const d = Math.floor(s / 86400);
  return `${d} day${d === 1 ? '' : 's'} ago`;
}

// ---- Per-project clean (state machine) ----

export type CleanPhase = 'idle' | 'cleaning' | 'cleaned' | 'failed';

export interface CleanState {
  phase: CleanPhase;
  /** Root of the project being or last cleaned. */
  root: string | null;
  fraction: number;
  freed: number;
  error: string | null;
}

export function idleClean(): CleanState {
  return { phase: 'idle', root: null, fraction: 0, freed: 0, error: null };
}

/** Only one project cleans at a time, and only when it has regenerable bytes. */
export function canClean(state: CleanState, p: ProjectSummary | null): boolean {
  return !!p && state.phase !== 'cleaning' && regenerable(p) > 0;
}
