import { flushPromises, mount } from '@vue/test-utils';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { createMemoryHistory, createRouter } from 'vue-router';
import { BRIDGE_KEY } from '../../bridge';
import { createMockBridge, MOCK_DATA } from '../../bridge/mock';
import { core, resetCore } from '../../state/core';
import { createUnavailablePlanBridge, PLAN_BRIDGE_KEY, type PlanBridge } from '../../bridge/plan';
import { createMockPlanBridge } from '../../bridge/planMock';
import { routes } from '../../navigation';
import { taskQueue, removedWorktrees } from '../../state/tasks';
import WorktreesScreen from './WorktreesScreen.vue';

async function settle() {
  await flushPromises();
  await new Promise((r) => setTimeout(r, 20));
  await flushPromises();
}

async function mountScreen(plans: PlanBridge = createMockPlanBridge({ stepMs: 0 }), bridge = createMockBridge()) {
  const router = createRouter({ history: createMemoryHistory(), routes });
  router.push('/worktrees');
  await router.isReady();
  const w = mount(WorktreesScreen, {
    global: { plugins: [router], provide: { [BRIDGE_KEY as symbol]: bridge, [PLAN_BRIDGE_KEY as symbol]: plans } },
  });
  await settle();
  return w;
}

const rowFor = (w: Awaited<ReturnType<typeof mountScreen>>, branch: string) =>
  w.findAll('button[data-row]').find((b) => b.text().includes(branch))!;

describe('Git Worktrees screen', () => {
  beforeEach(() => { removedWorktrees.splice(0); taskQueue.state.tasks.splice(0); });
  it('loads results when a scan finishes while this page is open', async () => {
    resetCore();
    const bridge = createMockBridge();
    const get = vi.spyOn(bridge, 'worktrees');
    get.mockResolvedValueOnce({ roots: [], cancelled: false, worktrees: [], problems: [], signals_unavailable: [] });
    const w = await mountScreen(undefined, bridge);
    expect(w.text()).toContain('Scan to load worktrees');
    core.report = structuredClone(MOCK_DATA.scanReport);
    await settle();
    expect(w.findAll('button[data-row]')).toHaveLength(8);
    expect(w.find('aside').exists()).toBe(false);
    w.unmount();
    resetCore();
  });
  it('claims no counts while the worktrees are still being read', async () => {
    const bridge = createMockBridge();
    let resolve!: () => void;
    const gate = new Promise<void>((r) => (resolve = r));
    const real = bridge.worktrees.bind(bridge);
    bridge.worktrees = async () => {
      await gate;
      return real();
    };
    const w = await mountScreen(undefined, bridge);
    expect(w.get('[data-testid="loading-state"]').text()).toContain('Loading worktrees');
    expect(w.get('[data-testid="main-scroll"]').attributes('aria-busy')).toBe('true');
    expect(w.find('[data-testid="worktrees-summary"]').exists()).toBe(false);
    expect(w.findAll('button[data-row]')).toHaveLength(0);
    resolve();
    await settle();
    expect(w.get('[data-testid="worktrees-summary"]').text()).toMatch(/^8 worktrees across/);
    expect(w.get('[data-testid="main-scroll"]').attributes('aria-busy')).toBe('false');
    expect(w.findAll('button[data-row]')).toHaveLength(8);
  });

  it('lists worktrees full width and opens details only after selection', async () => {
    const w = await mountScreen();
    expect(w.findAll('button[data-row]')).toHaveLength(8);
    expect(w.find('aside').exists()).toBe(false);
    await rowFor(w, 'agent/fix-flaky-tests-7').trigger('click');
    expect(w.get('[aria-current="true"]').text()).toContain('agent/fix-flaky-tests-7');
    expect(w.get('[data-testid="verdict"]').text()).toContain('Removable · S1');
    expect(w.findAll('[data-testid="checks"] li')).toHaveLength(7);
    expect(w.get('[data-testid="remove"]').text()).toBe('Remove worktree');
    expect(w.get('[data-testid="remove"]').attributes('disabled')).toBeUndefined();
  });

  it('blocks whole-worktree removal for review and protected worktrees', async () => {
    const w = await mountScreen();
    for (const b of ['feat/search-v2', 'fix/payments-hotfix', 'main']) {
      await rowFor(w, b).trigger('click');
      expect(w.get('[data-testid="removal-block"]').text().length).toBeGreaterThan(20);
      expect(w.get('[data-testid="remove"]').attributes('disabled')).toBeDefined();
    }
  });

  it('filters', async () => {
    const w = await mountScreen();
    const removable = w.findAll('[role="group"] button').find((b) => b.text().startsWith('Removable'))!;
    expect(removable.text()).toBe('Removable 2');
    await removable.trigger('click');
    expect(removable.attributes('aria-pressed')).toBe('true');
    expect(w.findAll('button[data-row]')).toHaveLength(2);
  });

  it('removes through a one-item plan (preparePlan + executePlan stubs), then toasts', async () => {
    const plans = createMockPlanBridge({ stepMs: 0 });
    const prep = vi.spyOn(plans, 'preparePlan');
    const exec = vi.spyOn(plans, 'executePlan');
    const w = await mountScreen(plans);
    await rowFor(w, 'agent/fix-flaky-tests-7').trigger('click');
    await w.get('[data-testid="remove"]').trigger('click');
    await settle();
    expect(prep).toHaveBeenCalledOnce();
    expect(prep.mock.calls[0]![0].items).toHaveLength(1);
    expect(exec).not.toHaveBeenCalled();
    expect(w.get('[data-testid="confirm-action"]').attributes('disabled')).toBeDefined();
    await w.get('input[aria-label="Confirm irreversible item count"]').setValue('1');
    await w.get('[data-testid="confirm-action"]').trigger('click');
    await settle();
    expect(exec).toHaveBeenCalledOnce();
    expect(exec.mock.calls[0]![0].typed_count).toBe(1);
    await vi.waitFor(async () => {
      await settle();
      expect(w.findAll('button[data-row]')).toHaveLength(7);
    }, { timeout: 10000, interval: 25 });
    expect(w.findAll('button[data-row]').some((b) => b.text().includes('agent/fix-flaky-tests-7'))).toBe(false);
    expect(taskQueue.state.tasks.at(-1)?.status).toBe('succeeded');
    expect(taskQueue.state.tasks.at(-1)?.summary).toContain('branch kept');
    expect(w.find('aside').exists()).toBe(false);
    expect(w.get('[data-testid="worktrees-summary"]').text()).toMatch(/^7 worktrees/);
  });

  it('inside Tauri the stub refuses and nothing disappears', async () => {
    const w = await mountScreen(createUnavailablePlanBridge());
    await rowFor(w, 'agent/fix-flaky-tests-7').trigger('click');
    await w.get('[data-testid="remove"]').trigger('click');
    await settle();
    expect(w.get('[role="alert"]').text()).toContain('not connected to the core yet');
    expect(w.findAll('button[data-row]')).toHaveLength(8);
  });
});
