// State machine of the agent-request (MCP approval) mode of Cleanup Review (design 12,
// PRD §13.3, D-34). Pure: the screen feeds it user actions, bridge answers, broker events and
// clock ticks.
//
//   pending ──approve (all reviewed + code)──▶ approved ──agent_started──▶ running ──finished──▶ executed
//      │ approved_elsewhere ▶ approved (e.g. `scull approve` in a terminal)
//      │ deny ▶ denied        │ revoke ▶ revoked
//      │ tick ≥ expiry ▶ expired (pending or approved-but-unused)
//      └ invalidated ▶ changed (pending or approved: D-34 voids the WHOLE agent plan)
//
// "ready" is not a phase: it is `pending` with every item reviewed and the code typed.
import type { ItemId } from '../../bridge';
import type { ExecutionResult, ItemOutcome } from '../../bridge/plan';

export type ApprovalPhase = 'pending' | 'approved' | 'running' | 'executed' | 'expired' | 'denied' | 'revoked' | 'changed';

/** The design's state names (for the UI and screenshots). */
export type ApprovalView = 'pending' | 'ready' | 'approved' | 'running' | 'executed' | 'expired' | 'denied' | 'revoked' | 'changed';

export interface ApprovalState {
  phase: ApprovalPhase;
  itemIds: ItemId[];
  reviewed: Record<ItemId, boolean>;
  code: string;
  expectedCode: string;
  /** Epoch ms. */
  expiresAt: number;
  now: number;
  approvedAt: number | null;
  /** Where it was approved, when not in this dialog (`terminal`, ...). */
  approvedVia: string | null;
  startedAt: number | null;
  closedAt: number | null;
  active: ItemId | null;
  outcomes: Record<ItemId, ItemOutcome>;
  changed: { id: ItemId; reason: string } | null;
  result: ExecutionResult | null;
  /** Approve was pressed while not ready: the code field shakes. */
  shake: boolean;
}

export type ApprovalAction =
  | { type: 'review'; id: ItemId; on: boolean }
  | { type: 'code'; value: string }
  | { type: 'approve'; at: number }
  /** Approved on another human channel while this dialog was open (no code typed here). */
  | { type: 'approved_elsewhere'; at: number; channel: string }
  | { type: 'deny'; at: number }
  | { type: 'revoke'; at: number }
  | { type: 'tick'; now: number }
  | { type: 'agent_started'; at: number }
  | { type: 'item_started'; id: ItemId }
  | { type: 'item_finished'; outcome: ItemOutcome }
  | { type: 'finished'; result: ExecutionResult; at: number }
  | { type: 'invalidated'; id: ItemId; reason: string; at: number };

export function initialApproval(p: { itemIds: ItemId[]; code: string; expiresAt: number; now: number }): ApprovalState {
  return {
    phase: p.now >= p.expiresAt ? 'expired' : 'pending',
    itemIds: [...p.itemIds],
    reviewed: {},
    code: '',
    expectedCode: p.code,
    expiresAt: p.expiresAt,
    now: p.now,
    approvedAt: null,
    approvedVia: null,
    startedAt: null,
    closedAt: p.now >= p.expiresAt ? p.expiresAt : null,
    active: null,
    outcomes: {},
    changed: null,
    result: null,
    shake: false,
  };
}

/** Case, spaces and dashes don't matter: "k7q4mp" matches "K7Q-4MP". */
export function normalizeCode(v: string): string {
  return v.toUpperCase().replace(/[^A-Z0-9]/g, '');
}

export function reviewedCount(s: ApprovalState): number {
  return s.itemIds.filter((id) => s.reviewed[id]).length;
}

export function allReviewed(s: ApprovalState): boolean {
  return s.itemIds.length > 0 && reviewedCount(s) === s.itemIds.length;
}

export type CodeStatus = 'empty' | 'typing' | 'mismatch' | 'match';

export function codeStatus(s: ApprovalState): CodeStatus {
  const typed = normalizeCode(s.code);
  const want = normalizeCode(s.expectedCode);
  if (!typed) return 'empty';
  if (typed === want) return 'match';
  return typed.length >= want.length ? 'mismatch' : 'typing';
}

export function canApprove(s: ApprovalState): boolean {
  return s.phase === 'pending' && s.now < s.expiresAt && allReviewed(s) && codeStatus(s) === 'match';
}

export function viewOf(s: ApprovalState): ApprovalView {
  if (s.phase === 'pending') return canApprove(s) ? 'ready' : 'pending';
  return s.phase;
}

/** Seconds left to decide (0 once closed). */
export function secondsLeft(s: ApprovalState): number {
  if (s.phase !== 'pending' && s.phase !== 'approved') return 0;
  return Math.max(0, Math.ceil((s.expiresAt - s.now) / 1000));
}

export function isClosed(s: ApprovalState): boolean {
  return s.phase === 'expired' || s.phase === 'denied' || s.phase === 'revoked' || s.phase === 'changed';
}

export function approvalReducer(s: ApprovalState, a: ApprovalAction): ApprovalState {
  const pending = s.phase === 'pending';
  switch (a.type) {
    case 'review':
      if (!pending || !s.itemIds.includes(a.id)) return s;
      return { ...s, reviewed: { ...s.reviewed, [a.id]: a.on }, shake: false };
    case 'code':
      if (!pending) return s;
      return { ...s, code: a.value.slice(0, 9), shake: false };
    case 'approve':
      if (!pending) return s;
      if (!canApprove({ ...s, now: a.at })) return { ...s, shake: true };
      return { ...s, phase: 'approved', approvedAt: a.at, now: a.at, shake: false };
    case 'approved_elsewhere':
      if (!pending) return s;
      return { ...s, phase: 'approved', approvedAt: a.at, approvedVia: a.channel, now: Math.max(s.now, a.at), shake: false };
    case 'deny':
      if (!pending) return s;
      return { ...s, phase: 'denied', closedAt: a.at, now: a.at };
    case 'revoke':
      if (s.phase !== 'approved') return s;
      return { ...s, phase: 'revoked', closedAt: a.at, now: a.at };
    case 'tick': {
      const next = { ...s, now: a.now };
      // An unused approval lapses too; a running plan finishes (each item is re-checked).
      if ((pending || s.phase === 'approved') && a.now >= s.expiresAt) {
        return { ...next, phase: 'expired', closedAt: s.expiresAt };
      }
      return next;
    }
    case 'agent_started':
      if (s.phase !== 'approved') return s;
      return { ...s, phase: 'running', startedAt: a.at };
    case 'item_started':
      if (s.phase !== 'running') return s;
      return { ...s, active: a.id };
    case 'item_finished':
      if (s.phase !== 'running') return s;
      return {
        ...s,
        outcomes: { ...s.outcomes, [a.outcome.item_id]: a.outcome },
        active: s.active === a.outcome.item_id ? null : s.active,
      };
    case 'finished':
      if (s.phase !== 'running') return s;
      return { ...s, phase: 'executed', result: a.result, active: null, closedAt: a.at };
    case 'invalidated':
      // D-34: an agent plan is voided entirely before it runs; while running, the executor's
      // per-item re-check skips the item instead (reported as a skipped outcome).
      if (!pending && s.phase !== 'approved') return s;
      return { ...s, phase: 'changed', changed: { id: a.id, reason: a.reason }, closedAt: a.at };
  }
}
