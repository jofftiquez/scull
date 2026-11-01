// Days left, from the core's disk-full forecast (PRO-01; one forecast source, #36).
//
// The desktop does not fit its own trend. Every surface that shows days left (the Insights tile
// and chart, the Overview alert, the menu bar widget) reads the `forecasts` bridge command,
// computed by scull_proactive::forecast with the configured policy: the same method, window and
// reserve as `scull check`, `scull status`, `scull insights`, the MCP server and the weekly
// digest. This module only reads those results:
//
// * days are the core's `days_until_threshold`, rounded like `Forecast::days_rounded` (at least
//   1 while filling, 0 below the reserve);
// * bytes freed after the latest sample (Quick Clean) are a known release: they push the date
//   back by `released / fill rate` days, the same as a Release in the core;
// * `actionable` is the core's `is_actionable` rule (filling with at least medium confidence, or
//   below the reserve): alerts (Overview, menu bar) show only those; the Insights tile shows any
//   forecast with its confidence.
import type { Drive, Forecast, ForecastConfidence } from '../bridge';

export interface DaysLeft {
  volumeId: string;
  /** Whole days until the reserve (0: already below it). */
  days: number;
  /** Bytes lost per day by the trend (positive while filling). */
  perDay: number;
  confidence: ForecastConfidence;
  /** Free bytes the forecast counts from (the latest sample, plus any release). */
  free: number;
  /** The reserve it counts down to. */
  reserve: number;
  belowReserve: boolean;
  /** May an alert or status line show it (core `Forecast::is_actionable`). */
  actionable: boolean;
}

const ORDER: Record<ForecastConfidence, number> = { unknown: 0, low: 1, medium: 2, high: 3 };

/** Whether `c` is at least medium (alerts and status lines need that). */
export function confident(c: ForecastConfidence): boolean {
  return ORDER[c] >= ORDER.medium;
}

/** Days left from a core forecast, with `released` bytes freed since its latest sample. `null`
 *  when there is nothing to say (not enough history, not filling). */
export function daysLeft(f: Forecast | null | undefined, released = 0): DaysLeft | null {
  if (!f) return null;
  const rate = f.fill_bytes_per_day ?? 0;
  const free = (f.free_bytes ?? 0) + released;
  const base = { volumeId: f.volume_id, confidence: f.confidence, reserve: f.reserve_bytes, free };
  if (f.state === 'below_reserve') {
    if (free <= f.reserve_bytes || !(rate > 0)) {
      return { ...base, days: 0, perDay: Math.max(0, rate), belowReserve: true, actionable: true };
    }
    // A release lifted it above the reserve: count from the release.
    const days = Math.max(1, Math.round((free - f.reserve_bytes) / rate));
    return { ...base, days, perDay: rate, belowReserve: false, actionable: confident(f.confidence) };
  }
  if (f.state !== 'filling' || !(rate > 0) || f.days_until_threshold == null) return null;
  const days = Math.max(1, Math.round(f.days_until_threshold + released / rate));
  return { ...base, days, perDay: rate, belowReserve: false, actionable: confident(f.confidence) };
}

const norm = (p: string) => p.replace(/[\\/]+$/, '') || p;

/** The forecast of `volumeId` (mount point), if the core has one. */
export function forecastOf(forecasts: readonly Forecast[], volumeId: string | null | undefined): Forecast | null {
  if (!volumeId) return null;
  return forecasts.find((f) => norm(f.volume_id) === norm(volumeId)) ?? null;
}

/**
 * A drive's forecast. Forecasts are keyed by volume (mount point, or a volume GUID path on
 * Windows). When none matches by mount point and there is a single forecast of a volume no
 * listed drive has, it belongs to the home drive (history records the scanned roots' volume).
 */
export function forecastOfDrive(drive: Drive, forecasts: readonly Forecast[], drives: readonly Drive[] = [drive]): Forecast | null {
  const mine = forecastOf(forecasts, drive.mount_point);
  if (mine || !drive.holds_home || forecasts.length !== 1) return mine;
  const only = forecasts[0]!;
  return drives.some((d) => norm(d.mount_point) === norm(only.volume_id)) ? null : only;
}
