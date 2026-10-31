import { flushPromises, mount } from '@vue/test-utils';
import { describe, expect, it, vi } from 'vitest';
import { createMemoryHistory, createRouter } from 'vue-router';
import { BRIDGE_KEY, type HistoryView, type ScullBridge } from '../../bridge';
import { createMockBridge } from '../../bridge/mock';
import { createMockHistoryActions, createPendingHistoryActions, type HistoryActions } from '../../components/history/bridge';
import { routes } from '../../navigation';
import HistoryScreen from './HistoryScreen.vue';

async function mountScreen(opts: { actions?: HistoryActions; bridge?: ScullBridge; query?: string } = {}) {
  const router = createRouter({ history: createMemoryHistory(), routes });
  router.push(`/history${opts.query ?? ''}`);
  await router.isReady();
  const w = mount(HistoryScreen, {
    props: { actions: opts.actions ?? createMockHistoryActions({ durationMs: 0, occupied: ['/Users/ada/code/proto/Library'] }) },
    global: { plugins: [router], provide: { [BRIDGE_KEY as symbol]: opts.bridge ?? createMockBridge() } },
  });
  await flushPromises();
  return w;
}

type W = Awaited<ReturnType<typeof mountScreen>>;
const restoreButtons = (w: W) => w.findAll('[data-testid="restore"]');
const states = (w: W) => w.findAll('[data-testid="item-state"]').map((s) => s.text());

describe('History screen', () => {
  it('lists every operation, figures on top, the restorable one selected', async () => {
    const w = await mountScreen();
    await vi.waitFor(() => expect(w.findAll('button[data-row]')).toHaveLength(7));
    expect(w.get('[aria-current="true"]').text()).toContain('Cleanup · 4 items');
    expect(w.get('[data-testid="figures"]').text()).toContain('Operations7');
    expect(w.get('[data-testid="restorable"]').text()).toBe('2');
    expect(restoreButtons(w)).toHaveLength(2);
    expect(w.get('[data-testid="log"]').text()).toContain('plan 7f3a·c21e approved · desktop');
    expect(w.get('[data-testid="chain"]').text()).toContain('hash chain verified');
  });

  it('selects another operation from the timeline', async () => {
    const w = await mountScreen();
    await vi.waitFor(() => expect(w.findAll('button[data-row]')).toHaveLength(7));
    await w.findAll('button[data-row]')[0]!.trigger('click');
    expect(w.get('[data-testid="details"] h2').text()).toBe('Quick Clean · 4 caches');
    expect(w.get('[data-testid="note"]').text()).toMatch(/nothing to restore/);
    expect(restoreButtons(w)).toHaveLength(0);
    await w.findAll('button[data-row]')[0]!.trigger('keydown', { key: 'ArrowDown' });
    expect(w.get('[data-testid="details"] h2').text()).toBe('Cleanup · 4 items');
  });

  it('restores to the original path (09b): state, counter and toast', async () => {
    const actions = createMockHistoryActions({ durationMs: 0, occupied: [] });
    const w = await mountScreen({ actions });
    await vi.waitFor(() => expect(restoreButtons(w)).toHaveLength(2));
    await restoreButtons(w)[0]!.trigger('click');
    await vi.waitFor(() => expect(w.find('[data-testid="toast"]').exists()).toBe(true));
    expect(w.get('[data-testid="toast"]').text()).toContain('Restored ~/code/old-ml/.venv');
    expect(w.get('[data-testid="toast"]').text()).toContain('2.4 GiB');
    expect(states(w)[2]).toBe('restored to original path');
    expect(restoreButtons(w)).toHaveLength(1);
    expect(w.get('[data-testid="restorable"]').text()).toBe('1');
    expect(actions.calls).toEqual(['restore op-oct28 oct28-3']);
  });

  it('never overwrites: a taken path restores beside the original', async () => {
    const w = await mountScreen();
    await vi.waitFor(() => expect(restoreButtons(w)).toHaveLength(2));
    await restoreButtons(w)[1]!.trigger('click');
    await vi.waitFor(() => expect(states(w)[3]).toBe('path taken · restored beside it as Library.scull-restored-1'));
    expect(w.get('[data-testid="toast"]').text()).toContain('as Library.scull-restored-1');
  });

  it('shows "Restoring…" while the restore runs and blocks a second one', async () => {
    let release!: () => void;
    const actions: HistoryActions = {
      kind: 'mock',
      restoreQuarantined: () =>
        new Promise((r) => {
          release = () => r({ restored_to: '/Users/ada/code/old-ml/.venv', sibling: false });
        }),
    };
    const w = await mountScreen({ actions });
    await vi.waitFor(() => expect(restoreButtons(w)).toHaveLength(2));
    await restoreButtons(w)[0]!.trigger('click');
    expect(restoreButtons(w)[0]!.text()).toBe('Restoring…');
    expect(restoreButtons(w)[1]!.attributes('disabled')).toBeDefined();
    expect(states(w)[2]).toBe('restoring…');
    release();
    await vi.waitFor(() => expect(states(w)[2]).toBe('restored to original path'));
  });

  it('inside Tauri the stub refuses: error shown, nothing restored', async () => {
    const w = await mountScreen({ actions: createPendingHistoryActions() });
    await vi.waitFor(() => expect(restoreButtons(w)).toHaveLength(2));
    await restoreButtons(w)[0]!.trigger('click');
    await vi.waitFor(() => expect(w.find('[data-testid="restore-error"]').exists()).toBe(true));
    expect(w.get('[data-testid="restore-error"]').text()).toMatch(/nothing was moved/);
    expect(restoreButtons(w)).toHaveLength(2);
    expect(w.find('[data-testid="toast"]').exists()).toBe(false);
  });

  it('empty history (what this build records): explains, no details panel', async () => {
    const w = await mountScreen({ query: '?demo=empty' });
    await vi.waitFor(() => expect(w.find('[data-testid="empty"]').exists()).toBe(true));
    expect(w.get('[data-testid="empty"]').text()).toContain('No cleanups yet');
    expect(w.find('[data-testid="details"]').exists()).toBe(false);
    expect(w.get('[data-testid="export"]').attributes('disabled')).toBeDefined();
  });

  it('an unreadable database is explained and left alone', async () => {
    const unreadable: HistoryView = {
      state: 'unavailable',
      detail: 'schema version 9 (this build reads 2); run `scull status` in a terminal',
      now: 0,
      audit_api: false,
      operations: [],
    };
    const bridge = { ...createMockBridge(), history: async () => unreadable };
    const w = await mountScreen({ bridge });
    await vi.waitFor(() => expect(w.find('[data-testid="empty"]').exists()).toBe(true));
    expect(w.get('[data-testid="empty"]').text()).toContain('schema version 9');
    expect(w.get('[data-testid="empty"]').text()).toContain('Nothing was changed.');
  });

  it('a bridge error is shown', async () => {
    const bridge = {
      ...createMockBridge(),
      history: async () => Promise.reject({ kind: 'io', message: 'disk unplugged' }),
    };
    const w = await mountScreen({ bridge });
    await vi.waitFor(() => expect(w.find('[role="alert"]').exists()).toBe(true));
    expect(w.get('[role="alert"]').text()).toContain('disk unplugged');
  });
});
