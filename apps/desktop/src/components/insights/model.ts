// Insights (design 05) from the history database's read-only series. Pure functions, unit
// tested: every figure the screen shows is computed here from recorded samples, and anything
// the history can't support is `null` (the tile then says so) rather than invented.
import type {
  Category,
  Forecast,
  ForecastConfidence,
  CategorySeries,
  FootprintSampleView,
  RegrowthEventView,
  VolumeSampleView,
  WorktreeReport,
} from '../../bridge';
import { isAgent, branchOf } from '../worktrees/worktrees';
import { categoryLabel } from '../overview/wheel';
import { daysLeft } from '../../lib/forecast';

export const DAY = 86_400;
const GiB = 1024 ** 3;

export type RangeId = '7d' | '30d' | '90d';
export const RANGES: readonly { id: RangeId; days: number; label: string }[] = [
  { id: '7d', days: 7, label: '7 days' },
  { id: '30d', days: 30, label: '30 days' },
  { id: '90d', days: 90, label: '90 days' },
];
export function rangeDays(id: RangeId): number {
  return RANGES.find((r) => r.id === id)?.days ?? 30;
}

/** The default reserve (`[proactive] reserve_gib`); the chart uses the forecast's own. */
export const FULL_RESERVE = 10 * GiB;
/** A rise in free space of at least this much between two samples counts as a cleanup. */
export const CLEANUP_MIN_RISE = 1 * GiB;
/** The forecast is drawn at most this far ahead. */
export const MAX_HORIZON_DAYS = 60;

export interface FreePoint {
  at: number;
  free: number;
}
export interface Cleanup extends FreePoint {
  /** Bytes free space rose by. */
  gained: number;
}
export interface BandPoint extends FreePoint {
  lo: number;
  hi: number;
}
export type Confidence = 'High' | 'Medium' | 'Low' | 'Unknown';

export interface FreeSpaceModel {
  points: FreePoint[];
  cleanups: Cleanup[];
  /** The core's forecast while free space is shrinking; `null` otherwise (see `state`). */
  trend: {
    /** Bytes per day lost (positive). */
    perDay: number;
    /** Days until the reserve (core `days_rounded`: at least 1). */
    days: number;
    confidence: Confidence;
  } | null;
  /** What the core concluded (`no_forecast` when it sent none for this volume). */
  state: Forecast['state'] | 'no_forecast';
  /** The reserve the forecast counts down to. */
  reserve: number;
  forecast: BandPoint[];
}

const LABEL: Record<ForecastConfidence, Confidence> = { high: 'High', medium: 'Medium', low: 'Low', unknown: 'Unknown' };

/**
 * Free-space history, cleanups and the forecast. The points and cleanups are what the disk
 * measured (a rise of at least CLEANUP_MIN_RISE between two samples marks a cleanup). The
 * trend, days and confidence are the core's (`forecast`, from the `forecasts` command; one
 * forecast source, src/lib/forecast.ts): the dashed line runs from its latest sample at its
 * fill rate to the reserve, and the band widens with its noise ratio.
 */
export function freeSpaceModel(samples: readonly VolumeSampleView[], forecast: Forecast | null = null): FreeSpaceModel {
  const points = [...samples]
    .sort((a, b) => a.sampled_at - b.sampled_at)
    .map((s) => ({ at: s.sampled_at, free: s.free_bytes }));
  const cleanups: Cleanup[] = [];
  for (let i = 1; i < points.length; i++) {
    const rise = points[i]!.free - points[i - 1]!.free;
    if (rise >= CLEANUP_MIN_RISE) cleanups.push({ ...points[i]!, gained: rise });
  }
  const reserve = forecast?.reserve_bytes ?? FULL_RESERVE;
  const state = forecast?.state ?? 'no_forecast';
  const d = daysLeft(forecast);
  if (!forecast || !d || d.belowReserve || forecast.as_of == null) {
    return { points, cleanups, trend: null, state, reserve, forecast: [] };
  }
  const perDay = d.perDay;
  const start = { at: forecast.as_of, free: forecast.free_bytes ?? 0 };
  // The band: ± the residual spread relative to the trend (noise ratio), growing with distance.
  const noise = forecast.noise_ratio ?? 0;
  const horizon = Math.min(MAX_HORIZON_DAYS, Math.max(1, Math.ceil((start.free - reserve) / perDay)));
  const band: BandPoint[] = [];
  for (let i = 0; i <= horizon; i++) {
    const free = Math.max(0, start.free - perDay * i);
    const spread = 2 * noise * perDay * i;
    band.push({ at: start.at + i * DAY, free, lo: Math.max(0, free - spread), hi: free + spread });
  }
  return { points, cleanups, trend: { perDay, days: d.days, confidence: LABEL[forecast.confidence] }, state, reserve, forecast: band };
}

/** The category that grew most over the window and its share of all growth. */
export function drivenBy(series: readonly CategorySeries[]): { category: Category; label: string; share: number } | null {
  const growth = series
    .map((s) => {
      const first = s.points[0];
      const last = s.points.at(-1);
      return { category: s.category, grew: first && last ? last.allocated_bytes - first.allocated_bytes : 0 };
    })
    .filter((g) => g.grew > 0);
  const total = growth.reduce((a, g) => a + g.grew, 0);
  const top = growth.sort((a, b) => b.grew - a.grew)[0];
  if (!top || total <= 0) return null;
  return { category: top.category, label: categoryLabel(top.category), share: top.grew / total };
}

export interface RegrowthModel {
  path: string;
  /** Last path component, with its parent ("shop-api/node_modules"). */
  name: string;
  artifact: string;
  /** Times it came back in the window. */
  count: number;
  /** Median days between going and coming back. */
  medianGapDays: number;
  /** Present / gone sequence for the bar strip, oldest first. */
  bars: ('present' | 'gone')[];
}

/** The artifact that came back most often (rebuilds only, not restores). */
export function regrowthModel(events: readonly RegrowthEventView[]): RegrowthModel | null {
  const byPath = new Map<string, RegrowthEventView[]>();
  for (const e of events) {
    if (e.same_item) continue;
    const list = byPath.get(e.path) ?? [];
    list.push(e);
    byPath.set(e.path, list);
  }
  const top = [...byPath.entries()].sort((a, b) => b[1].length - a[1].length || a[0].localeCompare(b[0]))[0];
  if (!top) return null;
  const [path, list] = top;
  const gaps = list.map((e) => (e.reappeared_at - e.disappeared_at) / DAY).sort((a, b) => a - b);
  const mid = Math.floor(gaps.length / 2);
  const medianGapDays = gaps.length % 2 ? gaps[mid]! : (gaps[mid - 1]! + gaps[mid]!) / 2;
  const bars: ('present' | 'gone')[] = ['present'];
  for (let i = 0; i < list.length; i++) bars.push('gone', 'present');
  const parts = path.split(/[\\/]/).filter(Boolean);
  return { path, name: parts.slice(-2).join('/'), artifact: list[0]!.artifact, count: list.length, medianGapDays, bars };
}

export interface MixModel {
  months: string[];
  groups: { key: string; label: string; values: number[]; share: number }[];
  /** Largest monthly total (bar heights are relative to it). */
  max: number;
}

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

/**
 * Bytes per category per calendar month (the last scan of each month), for the last `months`
 * months that have data. The three largest categories of the latest month are shown, the rest
 * are summed as "Other".
 */
export function mixModel(series: readonly CategorySeries[], months = 6): MixModel | null {
  const keyOf = (at: number) => {
    const d = new Date(at * 1000);
    return d.getUTCFullYear() * 12 + d.getUTCMonth();
  };
  const perMonth = new Map<number, Map<Category, { at: number; bytes: number }>>();
  for (const s of series) {
    for (const p of s.points) {
      const k = keyOf(p.at);
      const m = perMonth.get(k) ?? new Map();
      const prev = m.get(s.category);
      if (!prev || p.at >= prev.at) m.set(s.category, { at: p.at, bytes: p.allocated_bytes });
      perMonth.set(k, m);
    }
  }
  const keys = [...perMonth.keys()].sort((a, b) => a - b).slice(-months);
  if (keys.length === 0) return null;
  const latest = perMonth.get(keys.at(-1)!)!;
  const ranked = [...latest.entries()].sort((a, b) => b[1].bytes - a[1].bytes).map(([c]) => c);
  const shown: Category[] = ranked.filter((c) => c !== 'other').slice(0, 3);
  const valueOf = (k: number, c: Category) => perMonth.get(k)?.get(c)?.bytes ?? 0;
  const groups = shown.map((c) => ({ key: c as string, label: categoryLabel(c), values: keys.map((k) => valueOf(k, c)), share: 0 }));
  const all = new Set(series.map((s) => s.category));
  const rest = [...all].filter((c) => !shown.includes(c));
  if (rest.length) {
    groups.push({
      key: 'rest',
      label: 'Other',
      values: keys.map((k) => rest.reduce((a, c) => a + valueOf(k, c), 0)),
      share: 0,
    });
  }
  const totals = keys.map((_, i) => groups.reduce((a, g) => a + g.values[i]!, 0));
  const lastTotal = totals.at(-1) ?? 0;
  for (const g of groups) g.share = lastTotal > 0 ? g.values.at(-1)! / lastTotal : 0;
  return { months: keys.map((k) => MONTHS[k % 12]!), groups, max: Math.max(0, ...totals) };
}

export interface AgentModel {
  count: number;
  /** Bytes of agent worktrees idle for more than a day. */
  idleBytes: number;
  /** Agent worktrees by week of last activity, oldest first (8 weeks). */
  weeks: number[];
  /** Idle more than 7 days. */
  stale: number;
  /** By tool, from the branch prefix ("Claude Code 26"). */
  byTool: { tool: string; count: number }[];
}

const TOOL: Record<string, string> = {
  claude: 'Claude Code',
  codex: 'Codex',
  cursor: 'Cursor',
  copilot: 'Copilot',
  devin: 'Devin',
  agent: 'Other agents',
  agents: 'Other agents',
};

export function agentModel(worktrees: readonly WorktreeReport[], now: number): AgentModel | null {
  const agents = worktrees.filter(isAgent);
  if (agents.length === 0) return null;
  const weeks = new Array<number>(8).fill(0);
  let idleBytes = 0;
  let stale = 0;
  const tools = new Map<string, number>();
  for (const w of agents) {
    const a = w.state.activity;
    const newest = a.status === 'known' ? (a.value.newest_unix ?? null) : null;
    if (newest !== null) {
      const age = Math.max(0, now - newest);
      const week = Math.min(7, Math.floor(age / (7 * DAY)));
      weeks[7 - week]! += 1;
      if (age > DAY) idleBytes += w.allocated ?? 0;
      if (age > 7 * DAY) stale += 1;
    }
    const prefix = branchOf(w).split('/')[0]!.toLowerCase();
    const tool = TOOL[prefix] ?? 'Other agents';
    tools.set(tool, (tools.get(tool) ?? 0) + 1);
  }
  const byTool = [...tools.entries()].map(([tool, count]) => ({ tool, count })).sort((a, b) => b.count - a.count);
  return { count: agents.length, idleBytes, weeks, stale, byTool };
}

/** scull's latest footprint and the budget it is shown against. */
export const HISTORY_BUDGET = 1 * GiB;
export function footprintModel(samples: readonly FootprintSampleView[]): { bytes: number; share: number } | null {
  const last = [...samples].sort((a, b) => a.at - b.at).at(-1);
  if (!last) return null;
  return { bytes: last.total_bytes, share: Math.min(1, last.total_bytes / HISTORY_BUDGET) };
}

/** "Oct 9" (UTC, like the history's timestamps). */
export function shortDate(at: number): string {
  const d = new Date(at * 1000);
  return `${MONTHS[d.getUTCMonth()]} ${d.getUTCDate()}`;
}
