// One forecast source (#36), on the desktop side: the recorded core payloads (the `forecasts`,
// `free_space_history` and `weekly_digest` answers of one history database, recorded by
// apps/desktop/bridge/tests/insights.rs) give the same days left and confidence on every
// surface: the Insights tile, the Overview alert, the menu bar widget and the digest card.
// The Rust side checks the CLI, the digest, the tray monitor and the bridge against each other
// (crates/scull-cli/tests/forecast_surfaces.rs).
import { describe, expect, it } from 'vitest';
import { createMockBridge, MOCK_DRIVES } from '../bridge/mock';
import { steadyForecast } from '../bridge/forecastMock';
import { freeSpaceModel } from '../components/insights/model';
import { driveForecast } from '../components/overview/drives';
import { daysLeft as widgetDays, pickForecast } from '../components/menubar/model';
import { confident, daysLeft, forecastOf, forecastOfDrive } from './forecast';

const GiB = 1024 ** 3;
const DAY = 86_400;

describe('every desktop surface shows the core forecast', () => {
  it('Insights tile, Overview alert, menu bar and digest agree on days and confidence', async () => {
    const b = createMockBridge();
    const now = Date.now() / 1000;
    const [view, history, digest] = await Promise.all([b.forecasts(), b.freeSpaceHistory(now - 30 * DAY), b.weeklyDigest()]);
    const core = forecastOf(view.forecasts, history.volume_id)!;
    expect(core.state).toBe('filling');
    const coreDays = Math.max(1, Math.round(core.days_until_threshold!));

    const insights = freeSpaceModel(history.samples, core).trend!;
    const overview = driveForecast(null, MOCK_DRIVES, view.forecasts)!;
    const menubar = widgetDays(pickForecast(MOCK_DRIVES, view));
    const digestDays = digest.digest!.volumes.find((v) => v.volume_id === core.volume_id)!.days_left;

    expect({ insights: insights.days, overview: overview.days, menubar, digest: digestDays }).toEqual({
      insights: coreDays,
      overview: coreDays,
      menubar: coreDays,
      digest: coreDays,
    });
    expect(insights.confidence.toLowerCase()).toBe(core.confidence);
    expect(overview.confidence).toBe(core.confidence);
    expect(digest.digest!.lines[0]).toContain(`about ${coreDays} days until 10.0 GiB free`);
    // The same rate as the core, too (the chart's dashed line and "Rate").
    expect(insights.perDay).toBe(core.fill_bytes_per_day);
  });
});

describe('daysLeft (reads the core, never fits)', () => {
  const at = 1_900_000_000;
  const f = steadyForecast({ volumeId: '/', totalGiB: 494, freeGiB: 41.2, perDayGiB: 3.5, asOf: at });

  it("rounds the core's days like Forecast::days_rounded and adds releases at the fill rate", () => {
    expect(daysLeft(f)).toMatchObject({ days: 9, perDay: 3.5 * GiB, actionable: true, belowReserve: false });
    expect(daysLeft(f, 14.9 * GiB)!.days).toBe(13);
    expect(daysLeft({ ...f, days_until_threshold: 0.2 })!.days).toBe(1);
  });

  it('below the reserve is 0 days (a release can lift it back above)', () => {
    const low = steadyForecast({ volumeId: '/', totalGiB: 494, freeGiB: 4, perDayGiB: 3.5, asOf: at });
    expect(daysLeft(low)).toMatchObject({ days: 0, belowReserve: true, actionable: true });
    expect(daysLeft(low, 13 * GiB)).toMatchObject({ days: 2, belowReserve: false });
  });

  it('nothing without a filling forecast; actionable needs medium confidence', () => {
    expect(daysLeft(null)).toBeNull();
    expect(daysLeft({ ...f, state: 'not_enough_history', fill_bytes_per_day: null, days_until_threshold: null })).toBeNull();
    expect(daysLeft({ ...f, state: 'not_filling' })).toBeNull();
    expect(daysLeft({ ...f, confidence: 'low' })!.actionable).toBe(false);
    expect([confident('unknown'), confident('low'), confident('medium'), confident('high')]).toEqual([false, false, true, true]);
  });

  it('matches drives by mount point; a lone unknown volume is the home drive', () => {
    const [hd, ssd] = MOCK_DRIVES;
    expect(forecastOf([f], '/')).toBe(f);
    expect(forecastOfDrive(ssd!, [f], MOCK_DRIVES)).toBeNull();
    const guid = { ...f, volume_id: '\\\\?\\Volume{1}\\' };
    expect(forecastOfDrive(hd!, [guid], MOCK_DRIVES)).toBe(guid);
    expect(forecastOfDrive(hd!, [guid, f], MOCK_DRIVES)).toBe(f);
  });
});
