// Design scenarios only (`?scenario=design`, `?demo=design`): a core-shaped forecast for a disk
// losing a steady amount a day, so the illustrative machines show the design's "full in about
// 9 days". The app never computes forecasts: real ones come from the core (`forecasts`).
import type { Forecast, ForecastsView } from './types';

const GiB = 1024 ** 3;
const DAY = 86_400;
export const DEFAULT_RESERVE = 10 * GiB;

export interface Steady {
  volumeId: string;
  totalGiB: number;
  freeGiB: number;
  perDayGiB: number;
  /** When the latest sample was taken (Unix seconds). */
  asOf: number;
  confidence?: Forecast['confidence'];
}

/** What the core reports for a steady decline: (free − reserve) / rate days from the sample. */
export function steadyForecast(s: Steady, reserve = DEFAULT_RESERVE): Forecast {
  const free = Math.round(s.freeGiB * GiB);
  const rate = s.perDayGiB * GiB;
  const days = (free - reserve) / rate;
  return {
    volume_id: s.volumeId,
    state: free <= reserve ? 'below_reserve' : 'filling',
    confidence: s.confidence ?? 'high',
    as_of: s.asOf,
    free_bytes: free,
    total_bytes: Math.round(s.totalGiB * GiB),
    reserve_bytes: reserve,
    fill_bytes_per_day: rate,
    days_until_threshold: free <= reserve ? 0 : days,
    threshold_at: free <= reserve ? null : Math.round(s.asOf + days * DAY),
    samples_used: 20,
    span_secs: 14 * DAY,
    outliers_ignored: 0,
    jumps_ignored: 0,
    noise_ratio: 0.02,
  };
}

export function forecastsView(now: number, forecasts: Forecast[]): ForecastsView {
  return { state: 'ok', detail: null, now, reserve_bytes: DEFAULT_RESERVE, alert_days: 14, forecasts };
}
