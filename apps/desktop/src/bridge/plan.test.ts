import { describe, expect, it } from 'vitest';
import { createMockBridge } from './mock';
import { planBridgeFor } from './plan';
import { createMockPlanBridge } from './planMock';
import { actionAllowed } from '../lib/plan';

describe('plan bridge', () => {
  it('goes with the core bridge: mock with mock, Tauri with Tauri', () => {
    const m = createMockBridge();
    expect(planBridgeFor(m).kind).toBe('mock');
    expect(planBridgeFor(m)).toBe(planBridgeFor(m));
    const t = planBridgeFor({ ...m, kind: 'tauri' });
    expect(t.kind).toBe('tauri');
  });

  it('mock plans are internally consistent (D-28, S3 never carries an action)', async () => {
    const plan = await createMockPlanBridge().preparePlan({});
    for (const it of plan.items) {
      expect(actionAllowed(it.candidate.tier, it.candidate.action)).toBe(true);
      expect(it.candidate.tier).not.toBe('S3');
    }
    for (const l of plan.left_out) expect(l.tier).toBe('S3');
  });

  it('a plan asked for unknown IDs is refused (not found)', async () => {
    await expect(createMockPlanBridge().preparePlan({ items: ['nope'] })).rejects.toMatchObject({ kind: 'not_found' });
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
});
