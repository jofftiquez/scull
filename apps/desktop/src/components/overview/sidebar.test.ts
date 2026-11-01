import { flushPromises, mount } from '@vue/test-utils';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { createMemoryHistory, createRouter } from 'vue-router';
import App from '../../App.vue';
import { BRIDGE_KEY, type ScullBridge } from '../../bridge';
import { createMockBridge, MOCK_DATA } from '../../bridge/mock';
import { routes } from '../../navigation';
import { core, resetCore } from '../../state/core';
import { shell } from '../../state/shell';
import QuickCleanCard from './QuickCleanCard.vue';
import { DESIGN_CANDIDATES, designScenarioBridge } from './designScenario';
import { daysDash } from './forecast';
import {
  QUICK_CLEAN_UNAVAILABLE,
  quickCleanFrame,
  quickCleanPreview,
  quickCleanTools,
  toolLabel,
} from './quickClean';
import { itemReason, reclaimSummary, splitBytes } from './summary';
import { buildWheelModel } from './wheel';
import { until } from '../../test/until';

const GiB = 1024 ** 3;
const tools = quickCleanTools(DESIGN_CANDIDATES);

function reduceMotion(on: boolean) {
  vi.stubGlobal('matchMedia', (q: string) => ({
    matches: on && q.includes('reduce'),
    media: q,
    addEventListener: () => {},
    removeEventListener: () => {},
  }));
}

beforeEach(() => {
  resetCore();
  shell.wink = 0;
  reduceMotion(true);
});
afterEach(() => {
  vi.unstubAllGlobals();
  document.body.innerHTML = '';
});

describe('quick clean model', () => {
  it('offers package caches only (never protected ones), largest first, with short names', () => {
    expect(tools.map((t) => t.label)).toEqual(['npm', 'pnpm', 'Bun', 'Yarn']);
    const s3 = { ...DESIGN_CANDIDATES.find((c) => c.id === 'design-npm')!, tier: 'S3' as const };
    expect(quickCleanTools([s3])).toEqual([]);
    expect(toolLabel({ ...s3, rule: { id: 'weird.thing', version: 1 }, artifact: 'Weird cache' })).toBe('Weird');
  });

  it('pops chips in order while the total counts down', () => {
    const idle = quickCleanFrame(tools, new Set(), 'idle', 0);
    expect(idle.button).toBe('Clean 4 caches');
    expect(idle.remaining).toBe(idle.total);
    expect(quickCleanFrame(tools, new Set(), 'confirm', 0).button).toBe('Clean 14.9 GiB now');
    const mid = quickCleanFrame(tools, new Set(), 'cleaning', 0.2);
    // ease-out cubic: 0.2 -> 0.488, so chips 0 and 1 are done, chip 2 is active.
    expect(mid.chips.map((c) => c.state)).toEqual(['done', 'active', 'on', 'on']);
    expect(mid.button).toBe('Cleaning pnpm…');
    expect(mid.remaining).toBeLessThan(idle.total);
    expect(mid.chips[0]!.label).toBe('npm ✓');
    const done = quickCleanFrame(tools, new Set(), 'done', 1);
    expect(done.cleaned).toHaveLength(4);
    expect(done.remaining).toBe(0);
    expect(done.fill).toBe(100);
  });

  it('leaves chips that are switched off alone', () => {
    const off = new Set([tools[0]!.id]);
    const f = quickCleanFrame(tools, off, 'done', 1);
    expect(f.chips[0]!.state).toBe('off');
    expect(f.cleaned).not.toContain(tools[0]!.id);
    expect(quickCleanFrame(tools, new Set(tools.map((t) => t.id)), 'idle', 0).button).toBe('Pick caches to clean');
  });

  it('the stub simulates with the mock bridge and refuses with the real one', async () => {
    const mock = createMockBridge();
    const r = await quickCleanPreview(mock, tools);
    expect(r.simulated).toBe(true);
    expect(r.freed).toBe(tools.reduce((a, t) => a + t.bytes, 0));
    await expect(quickCleanPreview({ ...mock, kind: 'tauri' }, tools)).rejects.toThrow(QUICK_CLEAN_UNAVAILABLE);
  });
});

describe('forecast alert ring', () => {
  it('fills over 30 days (design)', () => {
    expect(daysDash(9)).toBe('33.9');
    expect(daysDash(90)).toBe('113.0');
  });
});

describe('sidebar copy', () => {
  it('summarises what is safe from the scan', () => {
    expect(reclaimSummary(DESIGN_CANDIDATES)).toBe(
      '3 projects hold 31.9 GiB of build output; 1 worktree holds 6.7 GiB of artifacts. All of it rebuilds itself.',
    );
    expect(reclaimSummary(MOCK_DATA.scanReport.candidates)).toMatch(/^Nothing is regenerable/);
    expect(splitBytes(38.6 * GiB)).toEqual(['38.6', 'GiB']);
  });

  it('explains an item with the reasons at its tier', () => {
    const hf = buildWheelModel(DESIGN_CANDIDATES).categories.flatMap((c) => c.items).find((i) => i.id === 'design-hf')!;
    expect(itemReason(hf)).toBe('14 models. 3 unused for 90+ days hold 11.8 GB. Re-downloading all is about 31 GB.');
  });
});

describe('quick clean card', () => {
  it('idle -> confirm -> done (reduced motion: no animation, no particles)', async () => {
    const w = mount(QuickCleanCard, { props: { tools, bridge: createMockBridge(), reduced: true } });
    expect(w.findAll('[data-chip]').map((c) => c.text())).toEqual(['npm · 6.1 GiB', 'pnpm · 4.8 GiB', 'Bun · 2.6 GiB', 'Yarn · 1.4 GiB']);
    await w.get('[data-testid="qc-button"]').trigger('click');
    expect(w.get('[data-testid="qc-confirm"]').text()).toContain('Not undoable');
    await w.get('[data-testid="qc-button"]').trigger('click');
    await flushPromises();
    const done = w.get('[data-testid="qc-done"]');
    expect(done.text()).toContain('+14.9 GiB');
    expect(done.text()).toContain('freed · measured 14.6 GiB');
    expect(done.text()).toContain('simulated');
    expect(w.findAll('.particle')).toHaveLength(0);
    expect(w.get('[data-testid="qc-total"]').text()).toBe('0 B');
    const last = w.emitted('state')!.at(-1)![0] as { phase: string; cleaned: string[] };
    expect(last.phase).toBe('done');
    expect(last.cleaned).toHaveLength(4);
    await w.get('[data-testid="qc-nice"]').trigger('click');
    expect(w.find('[data-testid="qc-done"]').exists()).toBe(false);
  });

  it('with motion: chips pop over 2.6 s and the burst has 20 particles', async () => {
    vi.useFakeTimers();
    try {
      const w = mount(QuickCleanCard, { props: { tools, bridge: createMockBridge(), reduced: false } });
      await w.get('[data-testid="qc-button"]').trigger('click');
      await w.get('[data-testid="qc-button"]').trigger('click');
      await flushPromises();
      await vi.advanceTimersByTimeAsync(800);
      expect(w.get('[data-testid="qc-button"]').text()).toMatch(/^Cleaning /);
      expect(w.findAll('[data-chip="done"]').length).toBeGreaterThan(0);
      await vi.advanceTimersByTimeAsync(2000);
      expect(w.find('[data-testid="qc-done"]').exists()).toBe(true);
      expect(w.findAll('.particle')).toHaveLength(20);
    } finally {
      vi.useRealTimers();
    }
  });

  it('the real app shows "not available" and deletes nothing', async () => {
    const bridge: ScullBridge = { ...createMockBridge(), kind: 'tauri' };
    const w = mount(QuickCleanCard, { props: { tools, bridge, reduced: true } });
    await w.get('[data-testid="qc-button"]').trigger('click');
    await w.get('[data-testid="qc-button"]').trigger('click');
    await flushPromises();
    expect(w.get('[data-testid="qc-unavailable"]').text()).toContain('Nothing was deleted');
    expect(w.find('[data-testid="qc-done"]').exists()).toBe(false);
  });
});

describe('overview sidebar', () => {
  async function mountOverview(bridge: ScullBridge) {
    const router = createRouter({ history: createMemoryHistory(), routes });
    router.push('/');
    await router.isReady();
    const w = mount(App, { attachTo: document.body, global: { plugins: [router], provide: { [BRIDGE_KEY as symbol]: bridge } } });
    await flushPromises();
    // The Overview decides asynchronously whether to auto-scan (never on first run, CFG-04):
    // wait for the report itself, then for the sweep to finish.
    await until(() => core.report !== null);
    await until(() => w.get('[data-testid="rescan"]').text() === 'Rescan');
    await flushPromises();
    return w;
  }

  it('fills alert, hero, detail, quick clean and tiers from the bridge; Quick Clean done winks the logo', async () => {
    const w = await mountOverview(designScenarioBridge(createMockBridge(), 0));
    expect(w.get('[data-testid="forecast"]').text()).toContain('Macintosh HD full in about 9 days.');
    expect(w.get('[data-testid="hero"]').text()).toContain('38.6');
    expect(w.get('[data-testid="tiers"]').text()).toContain('186.0 GiB');
    expect(w.get('[data-testid="selected"]').text()).toContain('Nothing selected');
    await w.get('[data-seg="design-hf"]').trigger('click');
    expect(w.get('[data-testid="badge"]').text()).toBe('Costly');
    expect(w.get('[data-testid="detail-action"]').text()).toBe('Add to cleanup');
    await w.get('[data-testid="qc-button"]').trigger('click');
    await w.get('[data-testid="qc-button"]').trigger('click');
    await flushPromises();
    expect(w.get('[data-testid="center"]').text()).toContain('Just freed');
    expect(w.get('[data-testid="forecast"]').text()).toContain('13 days');
    expect(w.get('[data-seg="design-npm"]').attributes('opacity')).toBe('0.1');
    expect(shell.wink).toBe(1);
    w.unmount();
  });

  it("with the recorded data: the core's recorded forecast, nothing regenerable", async () => {
    const w = await mountOverview(createMockBridge());
    // The same days as the Insights tile, the menu bar and the digest (src/lib/forecast.test.ts).
    await vi.waitFor(() => expect(w.get('[data-testid="forecast"]').text()).toContain('Macintosh HD full in about 11 days.'));
    expect(w.get('[data-testid="hero"]').text()).toContain('Nothing is regenerable');
    w.unmount();
  });
});
