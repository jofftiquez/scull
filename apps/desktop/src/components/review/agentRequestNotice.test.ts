// The agent request notification (design 12b): shown by scull when an AI agent's plan waits for
// the human; Review opens it, Later hides it; never an Approve or Deny button.
import { flushPromises, mount } from '@vue/test-utils';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { createMemoryHistory, createRouter } from 'vue-router';
import { BRIDGE_KEY } from '../../bridge';
import { createMockBridge } from '../../bridge/mock';
import { createUnavailablePlanBridge, PLAN_BRIDGE_KEY, type AgentRequestSummary, type PlanBridge } from '../../bridge/plan';
import { routes } from '../../navigation';
import AgentRequestNotice from './AgentRequestNotice.vue';

const now = Date.now();
const request = (id: string, over: Partial<AgentRequestSummary> = {}): AgentRequestSummary => ({
  plan_id: id,
  hash: 'c'.repeat(64),
  created_at: new Date(now - 60_000).toISOString(),
  expires_at: new Date(now + 14 * 60_000).toISOString(),
  item_count: 5,
  reclaimable_bytes: 37.1 * 2 ** 30,
  ...over,
});

function bridgeWith(requests: AgentRequestSummary[]): PlanBridge {
  return { ...createUnavailablePlanBridge(), approvalRequests: async () => requests };
}

async function mountAt(path: string, plans: PlanBridge) {
  const router = createRouter({ history: createMemoryHistory(), routes });
  router.push(path);
  await router.isReady();
  const w = mount(AgentRequestNotice, {
    props: { pollMs: 60_000 },
    global: { plugins: [router], provide: { [BRIDGE_KEY as symbol]: createMockBridge(), [PLAN_BRIDGE_KEY as symbol]: plans } },
  });
  await flushPromises();
  return { w, router };
}

afterEach(() => {
  document.body.innerHTML = '';
});

describe('agent request notification', () => {
  it('says who asks, how much, how long is left; Review opens the request', async () => {
    const { w, router } = await mountAt('/', bridgeWith([request('plan_01JABCDEF', { client: 'Claude Code' })]));
    const n = w.get('[data-testid="agent-request-notice"]');
    expect(n.attributes('role')).toBe('region');
    expect(n.text()).toContain('Claude Code wants to clean 37.1 GiB');
    expect(n.text()).toContain('5 items · plan 01JA·BCDE · decide within 14 min');
    expect(n.text()).toContain('Nothing runs until you approve in scull.');
    // It can't approve or deny anything.
    expect(n.findAll('button').map((b) => b.text())).toEqual(['Review', 'Later']);
    await w.get('[data-testid="agent-request-review"]').trigger('click');
    // The Cleanup Review screen is loaded on demand.
    await vi.waitFor(() => expect(router.currentRoute.value.fullPath).toBe('/cleanup?request=plan_01JABCDEF'));
    expect(w.find('[data-testid="agent-request-notice"]').exists()).toBe(false);
  });

  it('without a client name it says "An AI agent"; Later hides it and shows the next one', async () => {
    const { w } = await mountAt('/', bridgeWith([request('plan_01JONE'), request('plan_01JTWO')]));
    expect(w.text()).toContain('An AI agent wants to clean');
    expect(w.text()).toContain('+1 more');
    await w.get('[data-testid="agent-request-later"]').trigger('click');
    expect(w.text()).toContain('plan 01JT·WO');
    await w.get('[data-testid="agent-request-later"]').trigger('click');
    expect(w.find('[data-testid="agent-request-notice"]').exists()).toBe(false);
  });

  it('is hidden while that request is open, and with nothing waiting or expired', async () => {
    expect((await mountAt('/cleanup?request=plan_01JOPEN', bridgeWith([request('plan_01JOPEN')]))).w.text()).toBe('');
    expect((await mountAt('/', bridgeWith([]))).w.text()).toBe('');
    const gone = request('plan_01JOLD', { expires_at: new Date(now - 1000).toISOString() });
    expect((await mountAt('/', bridgeWith([gone]))).w.text()).toBe('');
    // A failing bridge shows nothing.
    const failing = { ...createUnavailablePlanBridge(), approvalRequests: async () => Promise.reject(new Error('busy')) };
    expect((await mountAt('/', failing)).w.text()).toBe('');
  });
});
