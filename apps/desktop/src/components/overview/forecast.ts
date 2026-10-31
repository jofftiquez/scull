// "Disk full in about N days" (design 01 forecast alert). A straight-line fit of the free-space
// samples the history database recorded (status().volumes). No forecast without at least two
// samples an hour apart, or when free space is not shrinking: the alert then stays hidden
// rather than guessing (PRD principle: never fabricate).
import type { VolumeSampleView } from '../../bridge';

const DAY = 86400;
/** "Full" means less than this left (the OS and apps need room to work). */
export const FULL_RESERVE = 10 * 1024 ** 3;
/** The alert shows from this many days out. */
export const ALERT_DAYS = 30;
/** Only samples from the last 30 days count. */
const WINDOW = 30 * DAY;

export interface Forecast {
  days: number;
  volumeId: string;
  free: number;
  /** Bytes per day the volume loses. */
  perDay: number;
}

/** The volume to forecast: `/` (or the first volume) of the latest samples. */
function pickVolume(samples: readonly VolumeSampleView[]): string | null {
  if (samples.some((s) => s.volume_id === '/')) return '/';
  const latest = [...samples].sort((a, b) => b.sampled_at - a.sampled_at)[0];
  return latest?.volume_id ?? null;
}

/**
 * Days until the volume is full, from its samples. `extraFree` adds bytes freed since the last
 * sample (e.g. right after a cleanup).
 */
export function forecastFull(samples: readonly VolumeSampleView[], extraFree = 0): Forecast | null {
  const id = pickVolume(samples);
  if (id === null) return null;
  const vol = samples.filter((s) => s.volume_id === id).sort((a, b) => a.sampled_at - b.sampled_at);
  const last = vol.at(-1);
  if (!last) return null;
  const pts = vol.filter((s) => s.sampled_at >= last.sampled_at - WINDOW);
  if (pts.length < 2 || last.sampled_at - pts[0]!.sampled_at < 3600) return null;
  // Least squares slope of free bytes over time.
  const n = pts.length;
  const mx = pts.reduce((a, s) => a + s.sampled_at, 0) / n;
  const my = pts.reduce((a, s) => a + s.free_bytes, 0) / n;
  let num = 0;
  let den = 0;
  for (const s of pts) {
    num += (s.sampled_at - mx) * (s.free_bytes - my);
    den += (s.sampled_at - mx) ** 2;
  }
  const slope = num / den; // bytes per second (negative when filling up)
  if (!(slope < 0)) return null;
  const perDay = -slope * DAY;
  const free = last.free_bytes + extraFree;
  return { days: Math.max(1, Math.round((free - FULL_RESERVE) / perDay)), volumeId: id, free, perDay };
}

/** Ring fill of the alert (design: 113 = the r=18 circumference, full at 30 days). */
export function daysDash(days: number): string {
  return Math.min(113, (days / ALERT_DAYS) * 113).toFixed(1);
}
