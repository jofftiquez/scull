import { describe, expect, it } from 'vitest';
import type { ExecutionResult } from '../../bridge/plan';
import {
  approvalReducer,
  canApprove,
  codeStatus,
  initialApproval,
  normalizeCode,
  secondsLeft,
  viewOf,
  type ApprovalAction,
  type ApprovalState,
} from './approval';

const T0 = 1_800_000_000_000;
const EXP = T0 + 15 * 60_000;
const IDS = ['a', 'b', 'c'];
const start = () => initialApproval({ itemIds: IDS, code: 'K7Q-4MP', expiresAt: EXP, now: T0 });
const run = (s: ApprovalState, ...acts: ApprovalAction[]) => acts.reduce(approvalReducer, s);
const reviewAll = (s: ApprovalState) => run(s, ...IDS.map((id): ApprovalAction => ({ type: 'review', id, on: true })));
const ready = () => run(reviewAll(start()), { type: 'code', value: 'k7q4mp' });
const result: ExecutionResult = {
  plan_id: 'p', items: [], predicted_bytes: 10, measured_freed_bytes: 9, moved_bytes: 0, finished_at: new Date(T0).toISOString(),
};

describe('approval mode state machine', () => {
  it('starts pending, with the full countdown', () => {
    const s = start();
    expect(s.phase).toBe('pending');
    expect(viewOf(s)).toBe('pending');
    expect(secondsLeft(s)).toBe(900);
  });

  it('an already expired plan opens expired', () => {
    expect(initialApproval({ itemIds: IDS, code: 'X', expiresAt: T0, now: T0 }).phase).toBe('expired');
  });

  it('normalizes the typed code (case, dash, spaces)', () => {
    expect(normalizeCode(' k7q-4mp ')).toBe('K7Q4MP');
    const s = start();
    expect(codeStatus(run(s, { type: 'code', value: 'K7' }))).toBe('typing');
    expect(codeStatus(run(s, { type: 'code', value: 'K7Q4MX' }))).toBe('mismatch');
    expect(codeStatus(run(s, { type: 'code', value: 'k7q 4mp' }))).toBe('match');
    expect(codeStatus(s)).toBe('empty');
  });

  it('is ready only with every item reviewed AND the right code', () => {
    expect(canApprove(run(start(), { type: 'code', value: 'K7Q-4MP' }))).toBe(false);
    expect(canApprove(reviewAll(start()))).toBe(false);
    const r = ready();
    expect(canApprove(r)).toBe(true);
    expect(viewOf(r)).toBe('ready');
    // un-reviewing one item takes it back to pending
    expect(viewOf(run(r, { type: 'review', id: 'b', on: false }))).toBe('pending');
  });

  it('approve while not ready shakes and stays pending', () => {
    const s = run(reviewAll(start()), { type: 'code', value: 'WRONG1' }, { type: 'approve', at: T0 + 1000 });
    expect(s.phase).toBe('pending');
    expect(s.shake).toBe(true);
    expect(run(s, { type: 'code', value: 'K' }).shake).toBe(false);
  });

  it('pending → approved → running → executed', () => {
    let s = run(ready(), { type: 'approve', at: T0 + 5000 });
    expect(s.phase).toBe('approved');
    expect(s.approvedAt).toBe(T0 + 5000);
    s = run(s, { type: 'agent_started', at: T0 + 8000 }, { type: 'item_started', id: 'a' });
    expect(s.phase).toBe('running');
    expect(s.active).toBe('a');
    s = run(s, { type: 'item_finished', outcome: { item_id: 'a', outcome: 'deleted', freed_bytes: 5, moved_bytes: 0 } });
    expect(s.active).toBeNull();
    expect(s.outcomes.a?.freed_bytes).toBe(5);
    s = run(s, { type: 'finished', result, at: T0 + 9000 });
    expect(s.phase).toBe('executed');
    expect(s.result).toEqual(result);
  });

  it('review and code are locked once the request leaves pending', () => {
    const s = run(ready(), { type: 'approve', at: T0 + 1 });
    expect(run(s, { type: 'review', id: 'a', on: false }).reviewed.a).toBe(true);
    expect(run(s, { type: 'code', value: '' }).code).toBe('k7q4mp');
  });

  it('approved elsewhere (e.g. scull approve in a terminal) moves a pending dialog to approved', () => {
    const s = run(start(), { type: 'approved_elsewhere', at: T0 + 5_000, channel: 'terminal' });
    expect(s.phase).toBe('approved');
    expect(s.approvedAt).toBe(T0 + 5_000);
    expect(s.approvedVia).toBe('terminal');
    // Only from pending: an approval made here, or a closed request, is left alone.
    const mine = run(ready(), { type: 'approve', at: T0 + 1_000 });
    expect(run(mine, { type: 'approved_elsewhere', at: T0 + 2_000, channel: 'desktop' })).toBe(mine);
    const denied = run(start(), { type: 'deny', at: T0 + 1_000 });
    expect(run(denied, { type: 'approved_elsewhere', at: T0 + 2_000, channel: 'terminal' })).toBe(denied);
  });

  it('deny closes a pending request; revoke closes an approved one', () => {
    expect(run(start(), { type: 'deny', at: T0 + 1 }).phase).toBe('denied');
    expect(run(ready(), { type: 'approve', at: T0 + 1 }, { type: 'revoke', at: T0 + 2 }).phase).toBe('revoked');
    // revoke is impossible once the agent started
    const running = run(ready(), { type: 'approve', at: T0 + 1 }, { type: 'agent_started', at: T0 + 2 });
    expect(run(running, { type: 'revoke', at: T0 + 3 }).phase).toBe('running');
    // deny is impossible after approval
    expect(run(ready(), { type: 'approve', at: T0 + 1 }, { type: 'deny', at: T0 + 2 }).phase).toBe('approved');
  });

  it('expires at the deadline, pending or approved-but-unused, never while running', () => {
    expect(run(start(), { type: 'tick', now: EXP - 1 }).phase).toBe('pending');
    const e = run(start(), { type: 'tick', now: EXP });
    expect(e.phase).toBe('expired');
    expect(e.closedAt).toBe(EXP);
    expect(secondsLeft(e)).toBe(0);
    expect(run(ready(), { type: 'approve', at: T0 + 1 }, { type: 'tick', now: EXP + 5 }).phase).toBe('expired');
    const running = run(ready(), { type: 'approve', at: T0 + 1 }, { type: 'agent_started', at: T0 + 2 });
    expect(run(running, { type: 'tick', now: EXP + 5 }).phase).toBe('running');
    // a ready request can't be approved after the deadline even before the tick lands
    expect(run(ready(), { type: 'approve', at: EXP }).phase).toBe('pending');
  });

  it('D-34: a changed target voids the whole agent plan (pending or approved)', () => {
    const c = run(start(), { type: 'invalidated', id: 'b', reason: 'changed', at: T0 + 3 });
    expect(c.phase).toBe('changed');
    expect(c.changed).toEqual({ id: 'b', reason: 'changed' });
    expect(run(ready(), { type: 'approve', at: T0 + 1 }, { type: 'invalidated', id: 'a', reason: 'x', at: T0 + 2 }).phase).toBe('changed');
    // while running, the executor skips the item instead (per-item outcome)
    const running = run(ready(), { type: 'approve', at: T0 + 1 }, { type: 'agent_started', at: T0 + 2 });
    expect(run(running, { type: 'invalidated', id: 'a', reason: 'x', at: T0 + 3 }).phase).toBe('running');
  });

  it('closed states are terminal', () => {
    for (const closed of [
      run(start(), { type: 'deny', at: T0 + 1 }),
      run(start(), { type: 'tick', now: EXP }),
      run(start(), { type: 'invalidated', id: 'a', reason: 'x', at: T0 + 1 }),
    ]) {
      const after = run(
        closed,
        { type: 'approve', at: T0 + 2 },
        { type: 'agent_started', at: T0 + 3 },
        { type: 'finished', result, at: T0 + 4 },
        { type: 'review', id: 'a', on: true },
      );
      expect(after.phase).toBe(closed.phase);
    }
  });
});
