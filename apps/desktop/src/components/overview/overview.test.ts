import { flushPromises, mount } from '@vue/test-utils';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { createMemoryHistory, createRouter } from 'vue-router';
import App from '../../App.vue';
import { BRIDGE_KEY, type ScullBridge } from '../../bridge';
import { createMockBridge } from '../../bridge/mock';
import { routes } from '../../navigation';
import { core, resetCore } from '../../state/core';
import { until } from '../../test/until';
import StorageWheel from './StorageWheel.vue';
import TierToolbar from './TierToolbar.vue';
import { cleanupBasket } from './basket';
import { DESIGN_CANDIDATES, designScenarioBridge, wantsDesignScenario } from './designScenario';
import { overviewBridge } from './overviewBridge';
import { buildWheelModel, layoutWheel } from './wheel';

function reduceMotion(on: boolean) {
  vi.stubGlobal('matchMedia', (q: string) => ({
    matches: on && q.includes('reduce'),
    media: q,
    addEventListener: () => {},
    removeEventListener: () => {},
  }));
}

async function mountOverview(bridge: ScullBridge = designScenarioBridge(createMockBridge(), 0)) {
  const router = createRouter({ history: createMemoryHistory(), routes });
  router.push('/');
  await router.isReady();
  const wrapper = mount(App, {
    attachTo: document.body,
    global: { plugins: [router], provide: { [BRIDGE_KEY as symbol]: bridge } },
  });
  await flushPromises();
  return wrapper;
}

/** The Overview decides asynchronously whether to auto-scan (not on first run, CFG-04), so wait
 *  for the scan report itself, then for the sweep to finish. */
async function scanned(w: Awaited<ReturnType<typeof mountOverview>>) {
  await until(() => core.report !== null);
  await until(() => w.get('[data-testid="wheel"]').attributes('aria-busy') === 'false');
}

beforeEach(() => {
  resetCore();
  cleanupBasket.clear();
  reduceMotion(true);
});
afterEach(() => {
  vi.unstubAllGlobals();
  document.body.innerHTML = '';
});

describe('tier toolbar', () => {
  it('offers All, the four tiers and Clear, with the pressed state', async () => {
    const w = mount(TierToolbar, { props: { modelValue: 'all' } });
    const buttons = w.findAll('button');
    expect(buttons.map((b) => b.text())).toEqual(['All', 'Regenerable', 'Costly', 'Uncertain', 'Protected', 'Clear']);
    expect(w.get('[data-filter="all"]').attributes('aria-pressed')).toBe('true');
    await w.get('[data-filter="S2"]').trigger('click');
    expect(w.emitted('update:modelValue')).toEqual([['S2']]);
    await w.get('[data-filter="clear"]').trigger('click');
    expect(w.emitted('clear')).toHaveLength(1);
  });
});

describe('storage wheel', () => {
  const arcs = layoutWheel(buildWheelModel(DESIGN_CANDIDATES));
  const center = { k: 'Development', kColor: 'var(--color-accent-text)', v: '186.0 GiB', n: '' };

  it('patterns uncertain and protected items, solid fills for the others', () => {
    const w = mount(StorageWheel, { props: { arcs, filter: 'all', selected: null, center, sweep: 1, scanning: false } });
    for (const p of w.findAll('path[data-kind="item"]')) {
      const fill = p.attributes('fill')!;
      const tier = p.attributes('data-tier');
      if (tier === 'S2' || tier === 'S3') expect(fill).toMatch(/^url\(#/);
      else expect(fill).toMatch(/^var\(--color-tier-s[01]-swatch\)$/);
    }
    expect(w.findAll('path[data-kind="category"]')).toHaveLength(arcs.length);
  });

  it('filters by tier and reveals up to the arm', async () => {
    const w = mount(StorageWheel, { props: { arcs, filter: 'S0', selected: null, center, sweep: 0.5, scanning: true } });
    for (const p of w.findAll('path[data-kind="item"]')) {
      expect(p.attributes('opacity')).toBe(p.attributes('data-tier') === 'S0' ? '1' : '0.14');
    }
    expect(w.get('[data-testid="reveal"]').attributes('stroke-dasharray')).toBe('440.0 880');
    expect(w.get('[data-testid="arm"]').attributes('style')).toContain('opacity: 1');
    await w.setProps({ scanning: false, sweep: 1 });
    expect(w.get('[data-testid="arm"]').attributes('style')).toContain('opacity: 0');
  });

  it('is one tab stop; arrows move, Enter selects, Shift+Enter adds', async () => {
    const w = mount(StorageWheel, { attachTo: document.body, props: { arcs, filter: 'all', selected: null, center, sweep: 1, scanning: false } });
    const focusable = w.findAll('[tabindex="0"]');
    expect(focusable).toHaveLength(1);
    await focusable[0]!.trigger('keydown', { key: 'ArrowRight' });
    await flushPromises();
    const second = document.activeElement as SVGElement;
    expect(second.getAttribute('data-seg')).toBe('design-target');
    second.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    second.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', shiftKey: true, bubbles: true }));
    expect(w.emitted('select')).toEqual([
      ['design-target', false],
      ['design-target', true],
    ]);
    w.unmount();
  });
});

describe('overview screen', () => {
  it('scans on first visit and draws the wheel from the result', async () => {
    const w = await mountOverview();
    await scanned(w);
    expect(w.findAll('path[data-kind="item"]')).toHaveLength(16);
    expect(w.get('[data-testid="center"]').text()).toContain('186.0 GiB');
    expect(w.find('[data-testid="show-regenerable"]').exists()).toBe(false);
    w.unmount();
  });

  it('selects, filters and clears (Esc)', async () => {
    const w = await mountOverview();
    await scanned(w);
    await w.get('[data-seg="design-hf"]').trigger('click');
    expect(w.get('[data-testid="center"]').text()).toContain('Hugging Face models');
    expect(w.get('[data-testid="selected"]').text()).toContain('Hugging Face models');
    await w.get('[data-filter="S0"]').trigger('click');
    expect(w.get('[data-filter="S0"]').attributes('aria-pressed')).toBe('true');
    expect(w.get('[data-seg="design-hf"]').attributes('opacity')).toBe('0.14');
    await w.get('[data-seg="design-hf"]').trigger('click');
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
    await flushPromises();
    expect(w.get('[data-filter="all"]').attributes('aria-pressed')).toBe('true');
    expect(w.get('[data-testid="center"]').text()).toContain('Development');
    w.unmount();
  });

  it('shift-click marks an item for cleanup, never a protected one', async () => {
    const w = await mountOverview();
    await scanned(w);
    await w.get('[data-seg="design-npm"]').trigger('click', { shiftKey: true });
    expect(cleanupBasket.has('design-npm')).toBe(true);
    expect(w.get('[data-testid="announce"]').text()).toContain('Added npm cache to cleanup');
    await w.get('[data-seg="design-src"]').trigger('click', { shiftKey: true });
    expect(cleanupBasket.has('design-src')).toBe(false);
    expect(w.get('[data-testid="announce"]').text()).toContain('protected');
    w.unmount();
  });

  it('reduced motion: the sweep ends as soon as the scan does', async () => {
    const w = await mountOverview(designScenarioBridge(createMockBridge(), 0));
    await vi.waitFor(() => expect(core.scanning).toBe(false));
    await scanned(w);
    expect(w.get('[data-testid="reveal"]').attributes('stroke-dasharray')).toBe('880.0 880');
    w.unmount();
  });

  it('with motion, the arm takes its time after a fast scan', async () => {
    reduceMotion(false);
    const w = await mountOverview(designScenarioBridge(createMockBridge(), 0));
    await vi.waitFor(() => expect(core.report).not.toBeNull());
    // The report is in, but the arm is still turning (one turn = 3.4 s).
    expect(w.get('[data-testid="rescan"]').text()).toBe('Scanning…');
    expect(w.get('[data-testid="ring"]').classes()).toContain('spinning');
    w.unmount();
  });

  it('never offers an idle Rescan while mount is still deciding to run the first scan', async () => {
    const mock = createMockBridge();
    let release!: () => void;
    const gate = new Promise<void>((r) => (release = r));
    const status = mock.status.bind(mock);
    const bridge: ScullBridge = {
      ...mock,
      status: async () => {
        await gate;
        return status();
      },
    };
    const w = await mountOverview(designScenarioBridge(bridge, 0));
    const rescan = w.get('[data-testid="rescan"]');
    expect(core.scanning).toBe(false);
    expect(core.report).toBeNull();
    expect(rescan.text()).not.toBe('Scan');
    expect(rescan.attributes('disabled')).toBeDefined();
    release();
    await scanned(w);
    expect(rescan.attributes('disabled')).toBeUndefined();
    w.unmount();
  });

  it('uses the design scenario only with the mock bridge and ?scenario=design', () => {
    const mock = createMockBridge();
    expect(overviewBridge(mock, false)).toBe(mock);
    expect(overviewBridge(mock, true)).not.toBe(mock);
    expect(overviewBridge(mock, true)).toBe(overviewBridge(mock, true));
    const tauriLike = { ...mock, kind: 'tauri' as const };
    expect(overviewBridge(tauriLike, true)).toBe(tauriLike);
    expect(wantsDesignScenario({ search: '?scenario=design', hash: '#/' })).toBe(true);
    expect(wantsDesignScenario({ search: '', hash: '#/?scenario=design' })).toBe(true);
    expect(wantsDesignScenario({ search: '?scenario=designer', hash: '' })).toBe(false);
  });
});
