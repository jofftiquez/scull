// Insights model (pure) and the screen over the mock bridge's recorded history.
import { describe, expect, it, vi } from 'vitest';
import { mockMonitor } from '../../monitor/mock';
import { flushPromises, mount } from '@vue/test-utils';
import { createMemoryHistory, createRouter } from 'vue-router';
import type { CategorySeries, Forecast, RegrowthEventView, VolumeSampleView, WorktreeReport } from '../../bridge';
import { steadyForecast } from '../../bridge/forecastMock';
import { BRIDGE_KEY } from '../../bridge';
import { createMockBridge, shiftHistory, MOCK_HISTORY } from '../../bridge/mock';
import InsightsScreen from '../../screens/Insights/InsightsScreen.vue';
import {
  CLEANUP_MIN_RISE,
  DAY,
  FULL_RESERVE,
  agentModel,
  drivenBy,
  footprintModel,
  freeSpaceModel,
  mixModel,
  regrowthModel,
} from './model';
import { createPendingInsightsActions } from './actions';

const GiB = 1024 ** 3;
const T0 = 1_790_000_000;

function samples(gibs: number[]): VolumeSampleView[] {
  return gibs.map((g, i) => ({ volume_id: '/', total_bytes: 494 * GiB, free_bytes: Math.round(g * GiB), sampled_at: T0 + i * DAY }));
}

describe('freeSpaceModel', () => {
  const last = T0 + 9 * DAY;
  const core = (over: Partial<Forecast> = {}): Forecast => ({
    ...steadyForecast({ volumeId: '/', totalGiB: 494, freeGiB: 38, perDayGiB: 3, asOf: last, confidence: 'medium' }),
    ...over,
  });

  it("finds cleanups as rises in free space; the trend, days and confidence are the core's", () => {
    // 50 → 41 GiB, a +12 GiB cleanup, then 3 GiB a day down to 38 GiB.
    const m = freeSpaceModel(samples([50, 47, 44, 41, 53, 50, 47, 44, 41, 38]), core());
    expect(m.cleanups).toHaveLength(1);
    expect(m.cleanups[0]!.gained).toBe(12 * GiB);
    expect(m.trend).toEqual({ perDay: 3 * GiB, days: 9, confidence: 'Medium' }); // (38 − 10) / 3 ≈ 9
    expect(m.state).toBe('filling');
    expect(m.reserve).toBe(FULL_RESERVE);
    // The dashed line starts at the core's latest sample and reaches the reserve.
    expect(m.forecast[0]).toMatchObject({ at: last, free: 38 * GiB });
    expect(m.forecast.at(-1)!.free).toBeLessThanOrEqual(FULL_RESERVE + 3 * GiB);
    expect(m.forecast.every((p) => p.lo <= p.free && p.free <= p.hi && p.lo >= 0)).toBe(true);
  });

  it("the band widens with the core's noise ratio; a steady line has none", () => {
    const steady = freeSpaceModel(samples([90, 87, 84]), core({ noise_ratio: 0 }));
    expect(steady.forecast.every((p) => Math.abs(p.hi - p.lo) < 1)).toBe(true);
    const noisy = freeSpaceModel(samples([90, 87, 84]), core({ noise_ratio: 0.2 }));
    const end = noisy.forecast.at(-1)!;
    expect(end.hi - end.lo).toBeGreaterThan(0);
    // A configured reserve moves the line's end.
    expect(freeSpaceModel(samples([90]), core({ reserve_bytes: 20 * GiB })).reserve).toBe(20 * GiB);
  });

  it('no trend without a filling forecast; the state says why; small rises are not cleanups', () => {
    expect(freeSpaceModel(samples([40, 41, 42]), null)).toMatchObject({ trend: null, state: 'no_forecast' });
    expect(freeSpaceModel(samples([40, 41]), core({ state: 'not_filling', days_until_threshold: null })).state).toBe('not_filling');
    expect(freeSpaceModel(samples([4]), core({ state: 'below_reserve', free_bytes: 4 * GiB })).trend).toBeNull();
    const small = freeSpaceModel(samples([40, 40 + (CLEANUP_MIN_RISE / GiB) * 0.5, 39]));
    expect(small.cleanups).toHaveLength(0);
  });
});

const series = (category: CategorySeries['category'], gibs: number[], start = T0, step = DAY): CategorySeries => ({
  category,
  points: gibs.map((g, i) => ({
    at: start + i * step,
    scan_id: `s${i}`,
    items: 1,
    logical_bytes: g * GiB,
    allocated_bytes: g * GiB,
    reclaimable_bytes: g * GiB,
  })),
});

describe('drivenBy and mixModel', () => {
  it('names the category that grew most and its share of all growth', () => {
    const d = drivenBy([series('ml_cache', [10, 17]), series('project_artifact', [20, 23]), series('package_cache', [9, 5])]);
    expect(d).toEqual({ category: 'ml_cache', label: 'ML & models', share: 0.7 });
    expect(drivenBy([series('ml_cache', [10, 9])])).toBeNull();
  });

  it('buckets by month (last scan wins), keeps the three largest and sums the rest as Other', () => {
    const month = 31 * DAY;
    const m = mixModel(
      [
        series('ml_cache', [10, 20, 30], T0, month),
        series('project_artifact', [40, 30, 20], T0, month),
        series('package_cache', [5, 5, 15], T0, month),
        series('worktree', [1, 1, 10], T0, month),
        series('container', [2, 2, 2], T0, month),
      ],
      6,
    )!;
    expect(m.months).toHaveLength(3);
    expect(m.groups.map((g) => g.key)).toEqual(['ml_cache', 'project_artifact', 'package_cache', 'rest']);
    expect(m.groups[3]!.values).toEqual([3 * GiB, 3 * GiB, 12 * GiB]);
    expect(m.groups.reduce((a, g) => a + g.share, 0)).toBeCloseTo(1);
    expect(m.max).toBe(77 * GiB);
    expect(mixModel([])).toBeNull();
  });
});

describe('regrowthModel', () => {
  const ev = (path: string, gone: number, back: number, same = false): RegrowthEventView => ({
    category: 'project_artifact',
    path,
    artifact: 'npm dependencies',
    previous_last_seen_at: gone - DAY,
    disappeared_at: gone,
    reappeared_at: back,
    same_item: same,
  });
  it('picks the path rebuilt most often; restores (same item) do not count', () => {
    const m = regrowthModel([
      ev('/u/code/shop-api/node_modules', T0, T0 + 2 * DAY),
      ev('/u/code/web/.next', T0, T0 + DAY, true),
      ev('/u/code/web/.next', T0 + 5 * DAY, T0 + 6 * DAY, true),
      ev('/u/code/shop-api/node_modules', T0 + 8 * DAY, T0 + 10 * DAY),
    ])!;
    expect(m.name).toBe('shop-api/node_modules');
    expect(m.count).toBe(2);
    expect(m.medianGapDays).toBe(2);
    expect(m.bars).toEqual(['present', 'gone', 'present', 'gone', 'present']);
    expect(regrowthModel([])).toBeNull();
  });
});

describe('agentModel and footprintModel', () => {
  const wt = (branch: string, idleDays: number, gib: number): WorktreeReport =>
    ({
      allocated: gib * GiB,
      kind: 'linked',
      path: `/u/wt/${branch}`,
      state: {
        head: { status: 'known', value: { kind: 'branch', name: branch } },
        activity: { status: 'known', value: { complete: true, entries_sampled: 1, newest_unix: T0 - idleDays * DAY } },
      },
    }) as unknown as WorktreeReport;
  it('counts agent worktrees by tool and week of last activity', () => {
    const m = agentModel([wt('claude/a', 0.5, 1), wt('claude/b', 3, 2), wt('codex/c', 20, 4), wt('main', 1, 9)], T0);
    if (m === null) throw new Error('expected agents');
    expect(m.count).toBe(3);
    expect(m.idleBytes).toBe(6 * GiB);
    expect(m.stale).toBe(1);
    expect(m.weeks.reduce((a, n) => a + n, 0)).toBe(3);
    expect(m.weeks[7]).toBe(2);
    expect(m.byTool).toEqual([
      { tool: 'Claude Code', count: 2 },
      { tool: 'Codex', count: 1 },
    ]);
    expect(agentModel([wt('main', 1, 1)], T0)).toBeNull();
  });
  it('reports the latest footprint against the budget', () => {
    const f = footprintModel([
      { at: 2, database_bytes: 0, wal_bytes: 0, backup_bytes: 0, damaged_bytes: 0, total_bytes: 256 * 1024 ** 2 },
      { at: 1, database_bytes: 0, wal_bytes: 0, backup_bytes: 0, damaged_bytes: 0, total_bytes: 1 },
    ])!;
    expect(f.bytes).toBe(256 * 1024 ** 2);
    expect(f.share).toBe(0.25);
    expect(footprintModel([])).toBeNull();
  });
});

describe('mock history', () => {
  it('shifts every timestamp so the latest is now', () => {
    const now = 2_000_000_000;
    const doc = shiftHistory(MOCK_HISTORY.freeSpace, now);
    expect(Math.max(...doc.samples.map((s) => s.sampled_at))).toBe(now);
    const d0 = MOCK_HISTORY.freeSpace.samples[1]!.sampled_at - MOCK_HISTORY.freeSpace.samples[0]!.sampled_at;
    expect(doc.samples[1]!.sampled_at - doc.samples[0]!.sampled_at).toBe(d0);
  });

  it('filters by the requested window', async () => {
    const b = createMockBridge();
    const now = Date.now() / 1000;
    const week = await b.freeSpaceHistory(now - 7 * DAY);
    const month = await b.freeSpaceHistory(now - 30 * DAY - 60);
    expect(week.samples.length).toBeLessThan(month.samples.length);
    expect(month.samples).toHaveLength(31);
    expect((await b.freeSpaceHistory(now - 7 * DAY, '/Volumes/Nope')).samples).toHaveLength(0);
  });
});

async function mountScreen(path = '/insights') {
  const router = createRouter({
    history: createMemoryHistory(),
    routes: [
      { path: '/insights', component: InsightsScreen },
      { path: '/cleanup', component: { template: '<div />' } },
      { path: '/settings', component: { template: '<div />' } },
    ],
  });
  await router.push(path);
  const w = mount(InsightsScreen, { global: { plugins: [router], provide: { [BRIDGE_KEY as symbol]: createMockBridge() } } });
  // The screen loads the range, then the six-month mix, worktrees and status: wait for the last.
  await vi.waitUntil(async () => {
    await flushPromises();
    return w.find('[data-testid="knows"]').text().includes('of history') && !w.find('[data-testid="mix"]').text().includes('appears after');
  }, { timeout: 10_000 });
  return w;
}

describe('InsightsScreen (mock bridge)', { timeout: 20_000 }, () => {
  it('renders the forecast, regrowth, mix and footprint from the recorded history', async () => {
    const w = await mountScreen();
    expect(w.find('[data-testid="forecast-chart"]').exists()).toBe(true);
    expect(w.findAll('[data-testid="cleanup-marker"]')).toHaveLength(2);
    expect(w.find('[data-testid="forecast-line"]').exists()).toBe(true);
    expect(w.find('[data-testid="forecast-summary"]').text()).toMatch(/≈ \d+ days/);
    expect(w.find('[data-testid="forecast-summary"]').text()).toContain('ML & models');
    expect(w.find('[data-testid="regrowth"]').text()).toContain('shop-api/node_modules came back 4 times');
    expect(w.find('[data-testid="mix"]').text()).toContain('ML & models');
    expect(w.find('[data-testid="knows"]').text()).toMatch(/of history, all on this computer/);
    // No source yet: honest empty states, not invented numbers.
    expect(w.find('[data-testid="duplication"]').text()).toContain("aren't measured yet");
    expect(w.find('[data-testid="accuracy"]').text()).toContain('After your first cleanups');
  });

  it('shows this week\'s digest and whether the weekly notification is on', async () => {
    mockMonitor.reset();
    const w = await mountScreen();
    await vi.waitFor(() => expect(w.find('[data-testid="digest"]').exists()).toBe(true));
    const card = w.get('[data-testid="digest"]').text();
    expect(card).toMatch(/41\.2 GiB free on \/ \(−24\.5 GiB this week\), about \d+ days until 10\.0 GiB free/);
    expect(card).toContain('Grew most: ML model caches');
    expect(w.get('[data-testid="digest-note"]').text()).toContain('The weekly digest is off');
    expect(card).toContain('Turn on the weekly digest');
    await mockMonitor.control.configure({ settings: { digest: { enabled: true } } });
    const on = await mountScreen();
    await vi.waitFor(() => expect(on.get('[data-testid="digest-note"]').text()).toMatch(/^Sent as a notification every Monday morning\. Next: /));
    mockMonitor.reset();
  });

  it('?demo=design shows the design values for the tiles without a source', async () => {
    const w = await mountScreen('/insights?demo=design');
    expect(w.find('[data-testid="duplication"]').text()).toContain('typescript@5.9');
    expect(w.find('[data-testid="accuracy"]').text()).toContain('median error 6%');
  });

  it('range buttons are pressed one at a time and reload the window', async () => {
    const w = await mountScreen();
    const buttons = w.findAll('[aria-label="Range"] button');
    expect(buttons.map((b) => b.attributes('aria-pressed'))).toEqual(['false', 'true', 'false']);
    await buttons[0]!.trigger('click');
    await flushPromises();
    expect(w.findAll('[aria-label="Range"] button').map((b) => b.attributes('aria-pressed'))).toEqual(['true', 'false', 'false']);
    expect(w.text()).toContain('last 7 days');
  });

  it('the forecast chart is keyboard operable and reads values out', async () => {
    const w = await mountScreen();
    const chart = w.find('[data-testid="forecast-chart"]');
    expect(chart.attributes('tabindex')).toBe('0');
    expect(chart.attributes('aria-label')).toMatch(/Free space went from/);
    await chart.trigger('keydown', { key: 'End' });
    expect(w.find('[data-testid="forecast-tooltip"]').text()).toContain('(forecast)');
    await chart.trigger('keydown', { key: 'Home' });
    expect(w.find('[data-testid="forecast-tooltip"]').text()).not.toContain('(forecast)');
    expect(w.find('table.sr-only caption').text()).toBe('Free space by day');
  });

  it('pending actions reject with a BridgeError instead of doing anything', async () => {
    const a = createPendingInsightsActions();
    await expect(a.neverSuggest('/x')).rejects.toThrow(/isn't available yet/);
    await expect(a.exportHistory()).rejects.toThrow(/isn't available yet/);
  });
});
