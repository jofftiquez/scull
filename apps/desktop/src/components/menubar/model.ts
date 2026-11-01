// The menu bar widget (design 11, D-23; P4-UI-01): free space with its ring, the forecast, what
// is safe to reclaim (the ready-to-review plan `scull check` prepared, PRO-05), Quick Clean of
// package caches, the latest alerts and when the machine was last checked.
//
// Pure functions over bridge payloads, so every figure is unit-tested. Nothing here (or in the
// widget) deletes anything: Quick Clean is the Overview's refused stub (quickClean.ts), and every
// other control opens the main window.
import type { Alert, AlertKind, CachesReport, DesktopStatus, Drive, FreeSpaceHistory, Severity } from '../../bridge';
import { freeSpaceModel, FULL_RESERVE } from '../insights/model';
import { easeOutCubic, quickCleanTools, type QuickCleanTool } from '../overview/quickClean';
import { formatBytes } from '../../lib/format';

/** Ring circumference (design: r = 38, 2πr ≈ 238.8). */
export const RING_C = 2 * Math.PI * 38;
/** Quick Clean animation length (design 11: 2200 ms). */
export const CLEAN_MS = 2200;
/** How long "Checking…" spins at least (design: 1400 ms), so the check reads as one. */
export const CHECK_MIN_MS = 1400;

/** Main-window routes the widget opens (the Tauri side allows exactly these). */
export type MainRoute = '/' | '/cleanup' | '/caches' | '/worktrees' | '/insights' | '/settings';
export const MAIN_ROUTES: readonly MainRoute[] = ['/', '/cleanup', '/caches', '/worktrees', '/insights', '/settings'];

export interface MenuBarData {
  drives: Drive[] | null;
  status: DesktopStatus | null;
  caches: CachesReport | null;
  history: FreeSpaceHistory | null;
}

export interface VolumeFigures {
  label: string;
  free: number;
  total: number;
}

export interface Reclaim {
  bytes: number;
  /** `plan`: the ready-to-review plan; `scan`: the last scan's S0 total (no plan yet). */
  source: 'plan' | 'scan';
  items: number | null;
  /** `scull clean --plan <file>` when there is a plan. */
  reviewCommand: string | null;
}

export type AlertTone = 'critical' | 'warning' | 'info';

export interface AlertRow {
  key: string;
  message: string;
  tone: AlertTone;
  route: MainRoute;
}

export interface MenuBarModel {
  volume: VolumeFigures | null;
  /** Bytes per day the volume loses; `null` when there is no trend (never guessed). */
  perDay: number | null;
  reclaim: Reclaim | null;
  tools: QuickCleanTool[];
  alerts: AlertRow[];
  /** Seconds since the last free-space sample or scan, `null` when never checked. */
  checkedAgo: number | null;
}

/** The volume the widget shows: the drive holding home (live), else the latest `/` sample. */
export function pickVolume(drives: readonly Drive[] | null, status: DesktopStatus | null): VolumeFigures | null {
  const home = drives?.find((d) => d.holds_home && d.total_bytes && d.free_bytes != null);
  if (home) return { label: home.label, free: home.free_bytes!, total: home.total_bytes! };
  const vols = status?.volumes ?? [];
  const v = vols.find((s) => s.volume_id === '/') ?? [...vols].sort((a, b) => b.sampled_at - a.sampled_at)[0];
  return v && v.total_bytes > 0 ? { label: v.volume_id, free: v.free_bytes, total: v.total_bytes } : null;
}

/** The plan first; without one, the last scan's safe (S0) bytes; nothing when both are 0. */
export function pickReclaim(status: DesktopStatus | null): Reclaim | null {
  const plan = status?.review_plan;
  if (plan && plan.reclaimable_bytes > 0) {
    return { bytes: plan.reclaimable_bytes, source: 'plan', items: plan.items, reviewCommand: plan.review_command };
  }
  const safe = status?.last_scan?.safe_reclaimable_bytes ?? 0;
  return safe > 0 ? { bytes: safe, source: 'scan', items: null, reviewCommand: null } : null;
}

const ROUTE_OF: Record<AlertKind, MainRoute> = {
  worktree_ttl_expired: '/worktrees',
  stale: '/cleanup',
  budget_exceeded: '/insights',
  growth_spike: '/insights',
  disk_full_forecast: '/insights',
  low_space: '/insights',
  free_space_drop: '/insights',
};
const TONE_OF: Record<Severity, AlertTone> = { critical: 'critical', warning: 'warning', info: 'info' };

export function alertRows(alerts: readonly Alert[]): AlertRow[] {
  return alerts.map((a) => ({ key: a.key, message: a.message, tone: TONE_OF[a.severity], route: ROUTE_OF[a.kind] ?? '/' }));
}

/** Seconds since the machine was last measured: the newest of the volume samples and the scan. */
export function checkedAgo(status: DesktopStatus | null): number | null {
  if (!status) return null;
  const ages: number[] = status.volumes.map((v) => Math.max(0, status.now - v.sampled_at));
  if (status.last_scan) ages.push(status.last_scan.age_secs);
  return ages.length ? Math.min(...ages) : null;
}

export function menuBarModel(d: MenuBarData): MenuBarModel {
  const trend = d.history ? freeSpaceModel(d.history.samples).trend : null;
  return {
    volume: pickVolume(d.drives, d.status),
    perDay: trend?.perDay ?? null,
    reclaim: pickReclaim(d.status),
    tools: d.caches ? quickCleanTools(d.caches.candidates) : [],
    alerts: alertRows(d.status?.alerts ?? []),
    checkedAgo: checkedAgo(d.status),
  };
}

/** Days until the volume is full (FULL_RESERVE left), from its trend; at least 1. */
export function daysLeft(free: number, perDay: number | null): number | null {
  if (perDay === null || !(perDay > 0)) return null;
  return Math.max(1, Math.round((free - FULL_RESERVE) / perDay));
}

export interface Ring {
  usedPct: number;
  usedDash: string;
  recDash: string;
  recOff: string;
  label: string;
}

/** The ring (design): used space in dark violet, the reclaimable share of it in violet at its end. */
export function ring(free: number, total: number, reclaim: number): Ring {
  const used = Math.max(0, total - free);
  const usedLen = (used / total) * RING_C;
  const recLen = (Math.min(reclaim, used) / total) * RING_C;
  const usedPct = Math.round((used / total) * 100);
  return {
    usedPct,
    usedDash: usedLen.toFixed(1),
    recDash: Math.max(0, recLen - 1).toFixed(1),
    recOff: (-(usedLen - recLen)).toFixed(1),
    label: `Disk ${usedPct}% used, ${formatBytes(free)} free of ${formatBytes(total)}`,
  };
}

export type CleanPhase = 'idle' | 'cleaning' | 'done' | 'unavailable';

export interface CleanFrame {
  /** Bytes freed so far (eased). */
  freed: number;
  /** Cache bytes still shown. */
  remaining: number;
  label: string;
  /** Shimmer fill, 0..100 (cleaning only). */
  fill: number;
}

/** One frame of the widget's Quick Clean button (design 11 / 11b). `p` is 0..1 while cleaning. */
export function cleanFrame(total: number, phase: CleanPhase, p: number, freedTotal = total): CleanFrame {
  const e = phase === 'done' ? 1 : phase === 'cleaning' ? easeOutCubic(Math.min(1, Math.max(0, p))) : 0;
  const freed = freedTotal * e;
  const label =
    phase === 'done'
      ? `+${formatBytes(freedTotal)} freed ✓`
      : phase === 'cleaning'
        ? `Cleaning… ${Math.round(e * 100)}%`
        : `Quick Clean · ${formatBytes(total)}`;
  return { freed, remaining: Math.max(0, total - freed), label, fill: phase === 'cleaning' ? Math.round(e * 100) : 0 };
}

/** "npm · pnpm · Bun · Yarn · re-downloadable" (design), from the cache tools found. */
export function toolNames(tools: readonly QuickCleanTool[]): string {
  const names = [...new Set(tools.map((t) => t.label))];
  return [...names, 're-downloadable'].join(' · ');
}

/** Particles of the done burst (design 11: 14, four colors, keyframes scull-b0..b9). */
export const BURST = Array.from({ length: 14 }, (_, i) => ({
  size: [6, 8, 7][i % 3]!,
  radius: i % 3 === 0 ? '2px' : '50%',
  color: ['#D6268A', '#7C4DFF', '#15121A', '#FFB547'][i % 4]!,
  animation: `scull-b${i % 10} ${650 + (i % 4) * 80}ms ${i >= 10 ? 100 : 0}ms cubic-bezier(.15,.7,.3,1) both`,
}));
