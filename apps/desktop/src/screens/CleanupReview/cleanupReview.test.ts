import { flushPromises, mount } from '@vue/test-utils';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { createMemoryHistory, createRouter } from 'vue-router';
import { BRIDGE_KEY } from '../../bridge';
import { createMockBridge } from '../../bridge/mock';
import { createUnavailablePlanBridge, PLAN_BRIDGE_KEY, type PlanBridge } from '../../bridge/plan';
import { createMockPlanBridge, MOCK_AGENT_PLAN_ID, registerScanCandidates } from '../../bridge/planMock';
import { cleanupBasket } from '../../components/overview/basket';
import { DESIGN_CANDIDATES } from '../../components/overview/designScenario';
import { routes } from '../../navigation';
import CleanupReviewScreen from './CleanupReviewScreen.vue';

async function mountAt(path: string, plans: PlanBridge = createMockPlanBridge()) {
  const router = createRouter({ history: createMemoryHistory(), routes });
  router.push(path);
  await router.isReady();
  const wrapper = mount(CleanupReviewScreen, {
    global: { plugins: [router], provide: { [BRIDGE_KEY as symbol]: createMockBridge(), [PLAN_BRIDGE_KEY as symbol]: plans } },
    attachTo: document.body,
  });
  await settle();
  return wrapper;
}

async function settle() {
  await flushPromises();
  await new Promise((r) => setTimeout(r, 20));
  await flushPromises();
}

afterEach(() => {
  vi.useRealTimers();
  cleanupBasket.clear();
  document.body.innerHTML = '';
});

describe('Cleanup Review (desktop plan)', () => {
  it('shows the plan with S0 preselected, S1/S2 unticked, S3 left out and unselectable', async () => {
    const w = await mountAt('/cleanup');
    const boxes = w.findAll<HTMLInputElement>('input[type="checkbox"][aria-label^="Include "]');
    expect(boxes).toHaveLength(6);
    const checked = boxes.filter((b) => b.element.checked).map((b) => b.attributes('aria-label'));
    expect(checked).toEqual([
      'Include ~/code/rust-svc/target',
      'Include ~/code/shop-api/node_modules',
      'Include ~/code/web-app/.next',
      'Include 31 worktree artifact folders',
    ]);
    // the S3 item is listed as protected, with no checkbox at all
    expect(w.text()).toContain('Protected · never offered');
    expect(w.text()).toContain('~/code/vault/target');
    expect(w.find('input[aria-label="Include ~/code/vault/target"]').exists()).toBe(false);
    expect(w.get('[data-testid="confirm"]').text()).toBe('Delete 4 items');
    expect(w.text()).toContain('Clean 4 items · 38.6 GiB predicted');
  });

  it('ticking an S2 item adds a quarantine to the plan', async () => {
    const w = await mountAt('/cleanup');
    await w.get('input[aria-label="Include ~/code/ml-lab/.venv"]').setValue(true);
    expect(w.get('[data-testid="confirm"]').text()).toBe('Delete 4 · quarantine 1');
    expect(w.text()).toContain('9.8 GiB');
  });

  it('every row opens all SAFE-01 preview fields', async () => {
    const w = await mountAt('/cleanup');
    await w.get('[data-testid="details-toggle"]').trigger('click');
    const d = w.get('[data-testid="item-details"]').text();
    for (const f of ['Exact path', 'Project', 'Artifact', 'Size', 'logical', 'allocated', 'reclaimable', 'confidence', 'Tier and reason', 'Regeneration', 'Evidence', 'Flags', 'In use', 'Action', 'Not restorable']) {
      expect(d).toContain(f);
    }
    expect(d).toContain('/Users/ada/code/rust-svc/target');
  });

  it('cleans the selection through the executePlan stub and shows the measured result', async () => {
    const plans = createMockPlanBridge({ stepMs: 0 });
    const exec = vi.spyOn(plans, 'executePlan');
    const w = await mountAt('/cleanup', plans);
    await w.get('[data-testid="confirm"]').trigger('click');
    await flushPromises();
    expect(exec).toHaveBeenCalledOnce();
    expect(exec.mock.calls[0]![0].items).toHaveLength(4);
    await vi.waitFor(async () => {
      await flushPromises();
      expect(w.find('[data-testid="done"]').exists()).toBe(true);
    }, { timeout: 10000, interval: 25 });
    expect(w.text()).toContain('freed');
    expect(w.text()).toContain('98% of the 38.6 GiB predicted');
    expect(w.get('[data-testid="confirm"]').text()).toBe('Done');
  });

  it('inside Tauri (no plan commands yet) it shows why there is nothing to review', async () => {
    const w = await mountAt('/cleanup', createUnavailablePlanBridge());
    expect(w.get('[role="alert"]').text()).toContain('not connected to the core yet');
    expect(w.find('[data-testid="confirm"]').exists()).toBe(false);
  });
});

describe('Cleanup Review (plan from the Overview basket)', () => {
  const rowIds = (w: Awaited<ReturnType<typeof mountAt>>) => w.findAll('[aria-label="Plan items"] [data-item]').map((r) => r.attributes('data-item'));

  it('with an empty basket it plans from the whole last scan', async () => {
    const plans = createMockPlanBridge();
    const prepare = vi.spyOn(plans, 'preparePlan');
    const w = await mountAt('/cleanup', plans);
    expect(prepare).toHaveBeenCalledWith({});
    expect(w.get('[data-testid="plan-line"]').text()).toContain('from the last scan');
  });

  it('with marked items it asks for exactly those and reviews only them', async () => {
    registerScanCandidates(DESIGN_CANDIDATES);
    for (const id of ['design-nm', 'design-target', 'design-npm']) cleanupBasket.add(id);
    const plans = createMockPlanBridge();
    const prepare = vi.spyOn(plans, 'preparePlan');
    const w = await mountAt('/cleanup', plans);
    expect(prepare).toHaveBeenCalledWith({ items: ['design-nm', 'design-target', 'design-npm'] });
    expect(rowIds(w)).toEqual(['design-nm', 'design-target', 'design-npm']);
    expect(w.get('[data-testid="plan-line"]').text()).toContain('from 3 items marked on the Overview');
    // S0 preselected, the S1 cache offered unticked; no scan-wide extras.
    expect(w.get('[data-testid="confirm"]').text()).toBe('Delete 2 items');
    expect(w.text()).not.toContain('Protected · never offered');
    expect(w.text()).not.toContain('Skipped at re-check');
  });

  it('an S3 ID in the request is left out, never selectable', async () => {
    registerScanCandidates(DESIGN_CANDIDATES);
    // The basket refuses S3 at add time; even if one got in, the plan never offers it.
    cleanupBasket.add('design-target');
    cleanupBasket.add('design-src');
    const w = await mountAt('/cleanup');
    expect(rowIds(w)).toEqual(['design-target']);
    expect(w.text()).toContain('Protected · never offered');
    expect(w.find('input[aria-label="Include ~/code"]').exists()).toBe(false);
  });

  it('cleaned items leave the basket', async () => {
    registerScanCandidates(DESIGN_CANDIDATES);
    for (const id of ['design-target', 'design-npm']) cleanupBasket.add(id);
    const plans = createMockPlanBridge({ stepMs: 0 });
    const exec = vi.spyOn(plans, 'executePlan');
    const w = await mountAt('/cleanup', plans);
    await w.get('[data-testid="confirm"]').trigger('click');
    await vi.waitFor(async () => {
      await flushPromises();
      expect(w.find('[data-testid="done"]').exists()).toBe(true);
    }, { timeout: 10000, interval: 25 });
    expect(exec.mock.calls[0]![0].items).toEqual(['design-target']);
    // Only what was cleaned is unmarked; the unticked cache stays in the basket.
    expect([...cleanupBasket]).toEqual(['design-npm']);
  });

  it('a stale basket says so and can fall back to the whole scan', async () => {
    cleanupBasket.add('gone-since-the-scan');
    const plans = createMockPlanBridge();
    const prepare = vi.spyOn(plans, 'preparePlan');
    const w = await mountAt('/cleanup', plans);
    expect(w.get('[role="alert"]').text()).toContain('none of these items are in the last scan');
    await w.get('[data-testid="plan-from-scan"]').trigger('click');
    await settle();
    expect(prepare).toHaveBeenLastCalledWith({});
    expect(cleanupBasket.size).toBe(0);
    expect(w.get('[data-testid="confirm"]').text()).toBe('Delete 4 items');
  });
});

describe('Cleanup Review (agent request mode)', () => {
  const url = `/cleanup?request=${MOCK_AGENT_PLAN_ID}`;

  it('needs every item reviewed and the typed code before approving', async () => {
    const plans = createMockPlanBridge();
    const approve = vi.spyOn(plans, 'approvePlan');
    const w = await mountAt(url, plans);
    expect(w.get('[data-testid="agent-pill"]').text()).toBe('Agent request');
    expect(w.get('[data-testid="code"]').text()).toBe('K7Q-4MP');
    const btn = w.get('[data-testid="approve"]');
    expect(btn.attributes('aria-disabled')).toBe('true');
    await btn.trigger('click');
    expect(approve).not.toHaveBeenCalled();

    for (const b of w.findAll('input[aria-label^="Reviewed "]')) await b.setValue(true);
    expect(w.text()).toContain('All 5 items reviewed');
    await w.get('[data-testid="code-input"]').setValue('k7q-4mx');
    expect(w.text()).toContain("That code doesn't match");
    await w.get('[data-testid="code-input"]').setValue('k7q4mp');
    expect(w.text()).toContain('Code matches');
    expect(w.get('[data-testid="approve"]').attributes('aria-disabled')).toBe('false');
    await w.get('[data-testid="approve"]').trigger('click');
    await flushPromises();
    expect(approve).toHaveBeenCalledWith(expect.objectContaining({ plan_id: MOCK_AGENT_PLAN_ID, hash: '9c41e07b5d2a18f3', code: 'k7q4mp' }));
    expect(w.get('[data-state]').attributes('data-state')).toBe('approved');
    expect(w.text()).toContain('Approved for one run');
  });

  it('deny closes the request with nothing changed', async () => {
    const w = await mountAt(url);
    await w.get('[data-testid="deny"]').trigger('click');
    await flushPromises();
    expect(w.get('[data-state]').attributes('data-state')).toBe('denied');
    expect(w.text()).toContain('You denied this request · nothing changed');
    expect(w.find('[data-testid="approve"]').exists()).toBe(false);
  });

  it('a target changing voids the plan (D-34)', async () => {
    const plans = createMockPlanBridge();
    const w = await mountAt(url, plans);
    plans.simulateTargetChanged(MOCK_AGENT_PLAN_ID);
    await flushPromises();
    expect(w.get('[data-state]').attributes('data-state')).toBe('changed');
    expect(w.get('[data-testid="fp-tag"]').text()).toBe('Void');
    expect(w.text()).toContain('This plan changed since it was prepared');
  });

  it('an unknown request says so', async () => {
    const w = await mountAt('/cleanup?request=nope');
    expect(w.get('[role="alert"]').text()).toContain('no agent request nope');
  });
});
