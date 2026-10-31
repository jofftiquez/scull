// Overview drive switcher (P3-UI-12, D-22): each drive has its own wheel, its own forecast and
// its own quarantine. "All drives" is the default and matches design 01: every candidate, the
// forecast of the drive that fills first.
//
// Pure (no Vue components): which drives can be switched to, which candidates and free-space
// samples belong to a drive, and the one-line drive context under the switcher.
import { ref } from 'vue';
import type { Candidate, Drive, VolumeSampleView } from '../../bridge';
import { formatBytes, usedPercent } from '../../lib/format';
import { forecastFull, type Forecast } from './forecast';

/** "all", or a drive's mount point. Kept for the session (across screen visits). */
export const ALL_DRIVES = 'all';
export const driveChoice = ref<string>(ALL_DRIVES);

const isWin = (p: string) => /^(?:[A-Za-z]:[\\/]|\\\\)/.test(p);
const segs = (p: string) => p.split(isWin(p) ? /[\\/]+/ : /\/+/).filter(Boolean);

/** Whether `path` is the mount point or inside it (Windows: case-insensitive). */
export function onMount(path: string, mount: string): boolean {
  if (!mount || isWin(path) !== isWin(mount)) return false;
  const ps = segs(path);
  const ms = segs(mount);
  if (ms.length > ps.length) return false;
  const k = (s: string) => (isWin(path) ? s.toLowerCase() : s);
  return ms.every((s, i) => k(s) === k(ps[i]!));
}

/** The drive a path is on: the one with the longest matching mount point. */
export function driveOf(path: string, drives: readonly Drive[]): Drive | null {
  let best: Drive | null = null;
  for (const d of drives) {
    if (onMount(path, d.mount_point) && (!best || segs(d.mount_point).length > segs(best.mount_point).length)) best = d;
  }
  return best;
}

export interface DriveOption {
  /** `all` or the mount point. */
  id: string;
  label: string;
  /** "external", "network"... (`null` for the internal drives and All). */
  kind: string | null;
  drive: Drive | null;
  /** Candidate bytes on the drive (on-disk). */
  bytes: number;
  count: number;
  /** Used share of the drive, when known. */
  used: number | null;
}

/**
 * The switcher's options: All, then every drive that holds candidates or that scull scans
 * (D-22: external drives when connected; a network drive only once opted in; backups never),
 * home drive first. Empty when there is at most one drive to show: nothing to switch.
 */
export function driveOptions(drives: readonly Drive[], cands: readonly Candidate[]): DriveOption[] {
  const bytes = new Map<Drive, { bytes: number; count: number }>();
  for (const c of cands) {
    const d = driveOf(c.path, drives);
    if (!d) continue;
    const t = bytes.get(d) ?? { bytes: 0, count: 0 };
    t.bytes += c.sizes.allocated;
    t.count += 1;
    bytes.set(d, t);
  }
  const shown = drives.filter((d) => d.scan_policy !== 'never' && (d.scan_policy === 'scan' || bytes.has(d)));
  if (shown.length < 2) return [];
  const total = cands.reduce((a, c) => a + c.sizes.allocated, 0);
  const opts: DriveOption[] = shown.map((d) => ({
    id: d.mount_point,
    label: d.label,
    kind: d.kind === 'local' ? null : d.kind === 'removable' ? 'external' : d.kind,
    drive: d,
    bytes: bytes.get(d)?.bytes ?? 0,
    count: bytes.get(d)?.count ?? 0,
    used: usedPercent(d.total_bytes, d.free_bytes),
  }));
  opts.sort((a, b) => Number(b.drive!.holds_home) - Number(a.drive!.holds_home));
  return [{ id: ALL_DRIVES, label: 'All drives', kind: null, drive: null, bytes: total, count: cands.length, used: null }, ...opts];
}

/** The chosen drive, or `null` for All (also when the chosen drive is gone). */
export function chosenDrive(choice: string, options: readonly DriveOption[]): Drive | null {
  return options.find((o) => o.id === choice)?.drive ?? null;
}

/** Candidates on a drive (all of them for `null`). */
export function candidatesOn(cands: readonly Candidate[], drive: Drive | null, drives: readonly Drive[]): Candidate[] {
  if (!drive) return [...cands];
  return cands.filter((c) => driveOf(c.path, drives) === drive);
}

const norm = (p: string) => {
  const t = p.length > 1 ? p.replace(/[\\/]+$/, '') : p;
  return isWin(p) ? t.toLowerCase() : t;
};

/**
 * A drive's free-space samples. Samples are keyed by volume (mount point, or a volume GUID path
 * on Windows). When none match by mount point and every sample is of one volume, that volume is
 * the home drive's (history records the scanned roots' volume).
 */
export function samplesOf(drive: Drive, samples: readonly VolumeSampleView[], drives: readonly Drive[] = [drive]): VolumeSampleView[] {
  const mine = samples.filter((s) => norm(s.volume_id) === norm(drive.mount_point));
  if (mine.length || !drive.holds_home) return mine;
  const ids = [...new Set(samples.map((s) => s.volume_id))];
  const listed = ids.length === 1 && drives.some((d) => norm(d.mount_point) === norm(ids[0]!));
  return ids.length === 1 && !listed ? [...samples] : [];
}

export interface DriveForecast extends Forecast {
  /** The drive it is about (`null`: the volume could not be matched to a listed drive). */
  drive: Drive | null;
}

/**
 * The forecast to show: the chosen drive's own; for All, the drive that fills first (or, without
 * drives, the default volume of the samples).
 */
export function driveForecast(
  drive: Drive | null,
  drives: readonly Drive[],
  samples: readonly VolumeSampleView[],
  extraFree = 0,
): DriveForecast | null {
  if (drive) {
    const f = forecastFull(samplesOf(drive, samples, drives), extraFree);
    return f && { ...f, drive };
  }
  let best: DriveForecast | null = null;
  for (const d of drives) {
    if (d.scan_policy === 'never') continue;
    // Freed bytes (Quick Clean) are package caches: they come back to the home drive.
    const f = forecastFull(samplesOf(d, samples, drives), d.holds_home ? extraFree : 0);
    if (f && (!best || f.days < best.days)) best = { ...f, drive: d };
  }
  if (best) return best;
  const f = forecastFull(samples, extraFree);
  return f && { ...f, drive: null };
}

/** The line under the switcher: free space and where cleanups of this drive are quarantined. */
export function driveContext(drive: Drive | null, options: readonly DriveOption[]): string {
  if (!drive) {
    const n = options.length - 1;
    return `${n} drives · each keeps its own forecast and quarantine`;
  }
  const free = drive.free_bytes == null ? 'free space unknown' : `${formatBytes(drive.free_bytes)} free`;
  return `${drive.label} · ${free} · cleanups here are quarantined on ${drive.label}`;
}
