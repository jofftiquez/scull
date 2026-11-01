import { describe, expect, it } from 'vitest';
import { createMockBridge } from './mock';
import { createUnavailablePlanBridge, planBridgeFor, wantsAgentRequest } from './plan';
import { createMockPlanBridge, MOCK_AGENT_PLAN_ID, registerScanCandidates } from './planMock';
import { DESIGN_CANDIDATES } from '../components/overview/designScenario';
import { BridgeError } from './types';
import { actionAllowed } from '../components/review/plan';

describe('plan bridge stubs', () => {
  it('goes with the core bridge: mock with mock, the approval broker with Tauri', async () => {
    const m = createMockBridge();
    expect(planBridgeFor(m).kind).toBe('mock');
    expect(planBridgeFor(m)).toBe(planBridgeFor(m));
    const t = planBridgeFor({ ...m, kind: 'tauri' });
    expect(t.kind).toBe('tauri');
    // Inside Tauri the desktop's own plans still refuse: the desktop never executes (SAFE-10).
    await expect(t.preparePlan({})).rejects.toMatchObject({ kind: 'unsupported_platform' });
    await expect(t.executePlan({ plan_id: 'x', hash: 'y', items: [] })).rejects.toMatchObject({ kind: 'unsupported_platform' });
  });

  it('the mock announces its agent request only when asked (?agent=request)', async () => {
    expect(await createMockPlanBridge().approvalRequests()).toEqual([]);
    const r = await createMockPlanBridge({ announceAgentRequest: true }).approvalRequests();
    expect(r).toHaveLength(1);
    expect(r[0]).toMatchObject({ plan_id: MOCK_AGENT_PLAN_ID, client: 'Claude Code', item_count: 5 });
    expect(wantsAgentRequest({ location: { search: '', hash: '#/?agent=request' } })).toBe(true);
    expect(wantsAgentRequest({ location: { search: '?agent=request', hash: '#/' } })).toBe(true);
    expect(wantsAgentRequest({ location: { search: '?agent=requests', hash: '' } })).toBe(false);
  });

  it('the unavailable bridge refuses everything (the desktop never deletes itself)', async () => {
    const u = createUnavailablePlanBridge();
    await expect(u.preparePlan({})).rejects.toBeInstanceOf(BridgeError);
    await expect(u.executePlan({ plan_id: 'x', hash: 'y', items: [] })).rejects.toMatchObject({ kind: 'unsupported_platform' });
    await expect(u.approvePlan({ plan_id: 'x', hash: 'y', code: 'z' })).rejects.toBeInstanceOf(BridgeError);
  });

  it('mock plans are internally consistent (D-28, S3 never carries an action)', async () => {
    const p = createMockPlanBridge();
    const plans = [await p.preparePlan({}), (await p.approvalRequest(MOCK_AGENT_PLAN_ID))!.plan];
    for (const plan of plans) {
      for (const it of plan.items) {
        expect(actionAllowed(it.candidate.tier, it.candidate.action)).toBe(true);
        expect(it.candidate.tier).not.toBe('S3');
      }
      for (const l of plan.left_out) expect(l.tier).toBe('S3');
    }
  });

  it('a plan asked for by ID holds exactly those scan items; S3 is left out, unknown IDs skipped', async () => {
    registerScanCandidates(DESIGN_CANDIDATES);
    const p = createMockPlanBridge();
    const plan = await p.preparePlan({ items: ['design-target', 'design-npm', 'design-wtun', 'design-gone', 'design-target'] });
    expect(plan.items.map((i) => i.candidate.id)).toEqual(['design-target', 'design-npm']);
    expect(plan.left_out).toEqual([expect.objectContaining({ label: '/Users/ada/code/.worktrees/unpushed', tier: 'S3' })]);
    expect(plan.skipped).toEqual([expect.objectContaining({ label: 'design-gone' })]);
    for (const it of plan.items) expect(actionAllowed(it.candidate.tier, it.candidate.action)).toBe(true);
    // Only protected items: no plan at all. Nothing known: not found.
    await expect(p.preparePlan({ items: ['design-src', 'design-wtun'] })).rejects.toMatchObject({ kind: 'safety_blocked' });
    await expect(p.preparePlan({ items: ['nope'] })).rejects.toMatchObject({ kind: 'not_found' });
  });

  it('the mock core bridge registers what its scan returned, so its IDs can be planned', async () => {
    const report = await createMockBridge().scan({});
    const plannable = report.candidates.filter((c) => c.tier !== 'S3');
    expect(plannable.length).toBeGreaterThan(0);
    const plan = await createMockPlanBridge().preparePlan({ items: plannable.map((c) => c.id) });
    expect(plan.items.map((i) => i.candidate.id)).toEqual(plannable.map((c) => c.id));
  });

  it('executePlan only narrows: unknown items or a changed hash are refused', async () => {
    const p = createMockPlanBridge({ stepMs: 0 });
    const plan = await p.preparePlan({});
    const ids = plan.items.map((i) => i.candidate.id);
    await expect(p.executePlan({ plan_id: plan.plan_id, hash: 'tampered', items: ids })).rejects.toMatchObject({ kind: 'plan_tampered' });
    await expect(p.executePlan({ plan_id: plan.plan_id, hash: plan.hash, items: [...ids, 'f'.repeat(32)] })).rejects.toMatchObject({
      kind: 'safety_blocked',
    });
    const events: string[] = [];
    const r = await p.executePlan({ plan_id: plan.plan_id, hash: plan.hash, items: ids.slice(0, 2) }, { onEvent: (e) => events.push(e.kind) });
    expect(events).toEqual(['item_started', 'item_finished', 'item_started', 'item_finished']);
    expect(r.items).toHaveLength(2);
    expect(r.measured_freed_bytes).toBeLessThanOrEqual(r.predicted_bytes);
  });

  it('agent approval: wrong code or hash refused; one approval only; deny after approve refused', async () => {
    const p = createMockPlanBridge({ agentDelayMs: 10_000 });
    const req = (await p.approvalRequest(MOCK_AGENT_PLAN_ID))!;
    expect(await p.approvalRequest('other')).toBeNull();
    await expect(p.approvePlan({ plan_id: req.plan.plan_id, hash: req.plan.hash, code: 'AAA-AAA' })).rejects.toBeInstanceOf(BridgeError);
    await expect(p.approvePlan({ plan_id: req.plan.plan_id, hash: 'x', code: req.code })).rejects.toMatchObject({ kind: 'plan_tampered' });
    await p.approvePlan({ plan_id: req.plan.plan_id, hash: req.plan.hash, code: 'k7q4mp' });
    await expect(p.approvePlan({ plan_id: req.plan.plan_id, hash: req.plan.hash, code: req.code })).rejects.toBeInstanceOf(BridgeError);
    await expect(p.denyPlan({ plan_id: req.plan.plan_id })).rejects.toBeInstanceOf(BridgeError);
    await p.revokeApproval({ plan_id: req.plan.plan_id });
  });
});
