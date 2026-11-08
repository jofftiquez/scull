import { flushPromises, mount } from '@vue/test-utils';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { createMemoryHistory, createRouter } from 'vue-router';
import { BRIDGE_KEY, type ScullBridge } from '../../bridge';
import { createMockBridge } from '../../bridge/mock';
import { core, resetCore } from '../../state/core';
import OverviewScreen from './OverviewScreen.vue';
import ExplorerScreen from '../Explorer/ExplorerScreen.vue';
import HistoryScreen from '../History/HistoryScreen.vue';
import InsightsScreen from '../Insights/InsightsScreen.vue';
import SettingsScreen from '../Settings/SettingsScreen.vue';

async function mountScreen(component: typeof OverviewScreen, bridge: ScullBridge) {
  const router = createRouter({ history: createMemoryHistory(), routes: [{ path: '/', component }, { path: '/settings', component: { template: '<div />' } }] });
  await router.push('/');
  return mount(component, { global: { plugins: [router], provide: { [BRIDGE_KEY as symbol]: bridge } } });
}

afterEach(resetCore);

describe('screen loading states', () => {
  it.each([
    { label: 'Overview', component: OverviewScreen, method: 'status', ready: '[data-testid="wheel-tile"]' },
    { label: 'Explorer', component: ExplorerScreen, method: 'drives', ready: 'section[aria-label="Folder map"]' },
    { label: 'History', component: HistoryScreen, method: 'history', ready: '[data-testid="figures"]' },
    { label: 'Insights', component: InsightsScreen, method: 'weeklyDigest', ready: '[data-testid="knows"]' },
    { label: 'Settings', component: SettingsScreen, method: 'monitorStatus', ready: '[role="tablist"]' },
  ] as const)('keeps $label in the shared loading state until initial reads finish', async ({ component, method, ready }) => {
    resetCore();
    const base = createMockBridge();
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    const bridge = { ...base, [method]: async () => { await gate; return base[method](); } } as ScullBridge;
    const w = await mountScreen(component, bridge);
    await flushPromises();
    expect(w.get('[data-testid="main-scroll"]').attributes('aria-busy')).toBe('true');
    expect(w.get('[data-testid="loading-state"]').attributes('role')).toBe('status');
    expect(w.find(ready).exists()).toBe(false);
    expect(w.find('[data-testid="header"]').exists()).toBe(true);
    release();
    await vi.waitFor(() => expect(w.find('[data-testid="loading-state"]').exists()).toBe(false));
    expect(w.find(ready).exists()).toBe(true);
    w.unmount();
  });

  it('reveals the Overview scan visualization even while its initial status read is pending', async () => {
    resetCore();
    const base = createMockBridge();
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    const w = await mountScreen(OverviewScreen, { ...base, status: async () => { await gate; return base.status(); } });
    expect(w.find('[data-testid="loading-state"]').exists()).toBe(true);
    core.scanning = true;
    await flushPromises();
    expect(w.find('[data-testid="loading-state"]').exists()).toBe(false);
    expect(w.find('[data-testid="wheel-tile"]').exists()).toBe(true);
    core.scanning = false;
    release();
    await flushPromises();
    w.unmount();
  });

  it('keeps Insights charts visible during a range query and ignores stale replies', async () => {
    const base = createMockBridge();
    const releases: (() => void)[] = [];
    let hold = false;
    const bridge: ScullBridge = { ...base, freeSpaceHistory: async (since) => {
      if (!hold) return base.freeSpaceHistory(since);
      return new Promise((resolve) => { releases.push(() => resolve({ state: 'ok', detail: null, volume_id: '/', since, samples: [] })); });
    } };
    const w = await mountScreen(InsightsScreen, bridge);
    await vi.waitFor(() => expect(w.find('[data-testid="knows"]').exists()).toBe(true));
    expect(w.get('[data-testid="main-footer"]').text()).toContain('Export history');
    expect(w.get('[data-testid="main-footer"] a').attributes('href')).toBe('/settings?tab=data');
    expect(w.get('[data-testid="knows"]').find('button').exists()).toBe(false);
    hold = true;
    const ranges = w.findAll('[aria-label="Range"] button');
    await ranges[0]!.trigger('click');
    expect(w.find('[data-testid="forecast-chart"]').exists()).toBe(true);
    expect(w.text()).toContain('Updating storage trends…');
    hold = false;
    await ranges[2]!.trigger('click');
    await vi.waitFor(() => expect(w.text()).toContain('last 90 days'));
    const chart = w.get('[data-testid="forecast-chart"]').attributes('aria-label');
    releases[0]!();
    await flushPromises();
    expect(w.get('[data-testid="forecast-chart"]').attributes('aria-label')).toBe(chart);
    expect(w.text()).not.toContain('Updating storage trends…');
    w.unmount();
  });
});
