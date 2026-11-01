import { flushPromises, mount } from '@vue/test-utils';
import { describe, expect, it, vi } from 'vitest';
import { createMemoryHistory, createRouter } from 'vue-router';
import { BRIDGE_KEY } from '../../bridge';
import { createMockBridge } from '../../bridge/mock';
import { createUnavailablePlanBridge, PLAN_BRIDGE_KEY, type PlanBridge } from '../../bridge/plan';
import { createMockPlanBridge } from '../../bridge/planMock';
import { routes } from '../../navigation';
import WorktreesScreen from './WorktreesScreen.vue';

async function settle() {
  await flushPromises();
  await new Promise((r) => setTimeout(r, 20));
  await flushPromises();
}

async function mountScreen(plans: PlanBridge = createMockPlanBridge({ stepMs: 0 })) {
  const router = createRouter({ history: createMemoryHistory(), routes });
  router.push('/worktrees');
  await router.isReady();
  const w = mount(WorktreesScreen, {
    global: { plugins: [router], provide: { [BRIDGE_KEY as symbol]: createMockBridge(), [PLAN_BRIDGE_KEY as symbol]: plans } },
  });
  await settle();
  return w;
}

const rowFor = (w: Awaited<ReturnType<typeof mountScreen>>, branch: string) =>
  w.findAll('button[data-row]').find((b) => b.text().includes(branch))!;

describe('Git Worktrees screen', () => {
  it('lists every worktree with its verdict, the largest removable selected', async () => {
    const w = await mountScreen();
    expect(w.findAll('button[data-row]')).toHaveLength(8);
    expect(w.get('[aria-current="true"]').text()).toContain('agent/fix-flaky-tests-7');
    expect(w.get('[data-testid="verdict"]').text()).toContain('Removable · S1');
    expect(w.findAll('[data-testid="checks"] li')).toHaveLength(7);
    expect(w.get('[data-testid="remove"]').text()).toBe('Remove worktree, keep branch · 2.4 GiB');
    expect(w.get('[data-testid="remove"]').attributes('disabled')).toBeUndefined();
  });

  it('blocks whole-worktree removal for review and protected worktrees', async () => {
    const w = await mountScreen();
    for (const b of ['feat/search-v2', 'fix/payments-hotfix', 'main']) {
      await rowFor(w, b).trigger('click');
      expect(w.get('[data-testid="remove"]').text()).toBe('Whole-worktree removal blocked');
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
    await w.get('[data-testid="remove"]').trigger('click');
    await settle();
    expect(prep).toHaveBeenCalledOnce();
    expect(prep.mock.calls[0]![0].items).toHaveLength(1);
    expect(exec).toHaveBeenCalledOnce();
    await vi.waitFor(async () => {
      await settle();
      expect(w.findAll('button[data-row]')).toHaveLength(7);
    }, { timeout: 10000, interval: 25 });
    expect(w.findAll('button[data-row]').some((b) => b.text().includes('agent/fix-flaky-tests-7'))).toBe(false);
    expect(w.get('[data-testid="toast"]').text()).toContain('Removed agent/fix-flaky-tests-7 · branch kept');
    expect(w.get('[aria-current="true"]').text()).toContain('agent/refactor-cart-12');
    expect(w.get('[data-testid="worktrees-summary"]').text()).toMatch(/^7 worktrees/);
  });

  it('inside Tauri the stub refuses and nothing disappears', async () => {
    const w = await mountScreen(createUnavailablePlanBridge());
    await w.get('[data-testid="remove"]').trigger('click');
    await settle();
    expect(w.get('[role="alert"]').text()).toContain('not connected to the core yet');
    expect(w.findAll('button[data-row]')).toHaveLength(8);
  });
});
