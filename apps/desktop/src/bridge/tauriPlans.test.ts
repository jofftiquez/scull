// The Tauri plan bridge (P4-MCP-02) over a fake transport: command names and arguments, payload
// mapping, and status polling turned into the dialog's events.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { AgentApprovalRequest, AgentPlanStatus, Candidate } from './generated';
import type { ApprovalEvent } from './plan';
import type { TauriTransport } from './tauri';
import { APPROVAL_COMMANDS, CLEANUP_COMMANDS, OPEN_REQUEST_EVENT, createTauriPlanBridge, eventsFor, initialsOf } from './tauriPlans';
import { BridgeError } from './types';

const T0 = 1_900_000_000; // Unix seconds
const iso = (s: number) => new Date(s * 1000).toISOString();
const HASH = 'a'.repeat(64);
const PLAN = 'plan_01JTEST';

function candidate(id: string): Candidate {
  return {
    action: { type: 'delete' },
    artifact: 'Cargo build output',
    category: 'project_artifact',
    evidence: [],
    flags: {},
    id,
    identity: { file: '1', volume: 7 },
    path: `/home/dev/code/${id}/target`,
    reasons: [],
    regeneration: { cost: { kind: 'unknown' } },
    rule: { id: 'rust.target', version: 1 },
    sizes: { allocated: 1024, logical: 1000, reclaimable: { bytes: 1000, confidence: 'high' } },
    tier: 'S0',
  } as unknown as Candidate;
}

const opened: AgentApprovalRequest = {
  plan_id: PLAN,
  content_hash: HASH,
  created_at: T0 - 60,
  expires_at: T0 + 840,
  items: [
    { candidate: candidate('a1'), from_last_scan: true },
    { candidate: candidate('b2'), from_last_scan: false },
  ],
  code: 'K7Q-4MP',
  attempts_left: 3,
};

function status(over: Partial<AgentPlanStatus>): AgentPlanStatus {
  return {
    plan_id: PLAN,
    state: 'awaiting_approval',
    plan_expires_at: T0 + 840,
    approved_at: null,
    channel: null,
    used_at: null,
    denied_at: null,
    operation: null,
    ...over,
  };
}

interface Fake extends TauriTransport {
  calls: { cmd: string; args?: Record<string, unknown> }[];
  answers: Map<string, (args?: Record<string, unknown>) => unknown>;
  emit(event: string, payload: unknown): void;
}

function fake(): Fake {
  const handlers = new Map<string, ((p: unknown) => void)[]>();
  const f: Fake = {
    calls: [],
    answers: new Map(),
    async invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
      f.calls.push({ cmd, args });
      const a = f.answers.get(cmd);
      if (!a) throw { kind: 'internal', message: `no answer for ${cmd}` };
      return a(args) as T;
    },
    async listen<T>(event: string, handler: (payload: T) => void) {
      const list = handlers.get(event) ?? [];
      list.push(handler as (p: unknown) => void);
      handlers.set(event, list);
      return () => handlers.set(event, (handlers.get(event) ?? []).filter((h) => h !== handler));
    },
    emit(event, payload) {
      for (const h of handlers.get(event) ?? []) h(payload);
    },
  };
  return f;
}

describe('Tauri plan bridge: commands', () => {
  it('lists waiting agent plans', async () => {
    const t = fake();
    t.answers.set(APPROVAL_COMMANDS.requests, () => ({
      requests: [{ plan_id: PLAN, content_hash: HASH, created_at: T0 - 60, expires_at: T0 + 840, item_count: 2, reclaimable_bytes: 2000 }],
    }));
    const r = await createTauriPlanBridge({ transport: t }).approvalRequests();
    expect(r).toEqual([{ plan_id: PLAN, hash: HASH, created_at: iso(T0 - 60), expires_at: iso(T0 + 840), item_count: 2, reclaimable_bytes: 2000 }]);
  });

  it('opens a request: the plan as recorded, the code for display, no invented client', async () => {
    const t = fake();
    t.answers.set(APPROVAL_COMMANDS.open, () => opened);
    const r = (await createTauriPlanBridge({ transport: t }).approvalRequest(PLAN))!;
    expect(t.calls).toEqual([{ cmd: 'open_approval_request', args: { planId: PLAN } }]);
    expect(r.code).toBe('K7Q-4MP');
    expect(r.plan).toMatchObject({ plan_id: PLAN, hash: HASH, created_at: iso(T0 - 60), expires_at: iso(T0 + 840) });
    expect(r.plan.source).toEqual({ kind: 'mcp', tool: 'prepare_cleanup' });
    expect(r.plan.items.map((i) => [i.candidate.id, i.from_last_scan, i.note])).toEqual([
      ['a1', true, undefined],
      ['b2', false, 'as recorded in the plan · not in your last scan'],
    ]);
  });

  it("shows the client's own name (unverified) when the core recorded one", async () => {
    const t = fake();
    t.answers.set(APPROVAL_COMMANDS.open, () => ({ ...opened, client: 'Claude Code 2.1' }));
    t.answers.set(APPROVAL_COMMANDS.requests, () => ({
      requests: [{ plan_id: PLAN, content_hash: HASH, created_at: T0, expires_at: T0 + 900, item_count: 1, reclaimable_bytes: 1, client: 'Claude Code 2.1' }],
    }));
    const b = createTauriPlanBridge({ transport: t });
    const r = (await b.approvalRequest(PLAN))!;
    expect(r.plan.source).toEqual({ kind: 'mcp', tool: 'prepare_cleanup', client: 'Claude Code 2.1', initials: 'CC' });
    expect((await b.approvalRequests())[0]!.client).toBe('Claude Code 2.1');
    expect(initialsOf('cursor')).toBe('CU');
  });

  it('an unknown plan opens as "no request"; other refusals are errors', async () => {
    const t = fake();
    t.answers.set(APPROVAL_COMMANDS.open, (a) => {
      throw a?.planId === 'plan_gone' ? { kind: 'not_found', message: 'no plan' } : { kind: 'plan_expired', message: 'expired' };
    });
    const b = createTauriPlanBridge({ transport: t });
    expect(await b.approvalRequest('plan_gone')).toBeNull();
    await expect(b.approvalRequest(PLAN)).rejects.toMatchObject({ kind: 'plan_expired' });
  });

  it('approve sends the reviewed hash and the typed code; deny and revoke send the plan', async () => {
    const t = fake();
    t.answers.set(APPROVAL_COMMANDS.approve, () => ({ plan_id: PLAN, approved_at: T0, approval_expires_at: T0 + 840 }));
    t.answers.set(APPROVAL_COMMANDS.deny, () => ({ plan_id: PLAN, denied_at: T0 + 1 }));
    t.answers.set(APPROVAL_COMMANDS.revoke, () => ({ plan_id: PLAN, revoked_at: T0 + 2 }));
    const b = createTauriPlanBridge({ transport: t });
    expect(await b.approvePlan({ plan_id: PLAN, hash: HASH, code: 'k7q4mp' })).toEqual({ approved_at: iso(T0) });
    expect(await b.denyPlan({ plan_id: PLAN })).toEqual({ denied_at: iso(T0 + 1) });
    expect(await b.revokeApproval({ plan_id: PLAN })).toEqual({ revoked_at: iso(T0 + 2) });
    expect(t.calls).toEqual([
      { cmd: 'approve_agent_plan', args: { planId: PLAN, contentHash: HASH, code: 'k7q4mp' } },
      { cmd: 'deny_agent_plan', args: { planId: PLAN } },
      { cmd: 'revoke_agent_approval', args: { planId: PLAN } },
    ]);
  });

  it('a refused approval comes back as a BridgeError with the core message', async () => {
    const t = fake();
    t.answers.set(APPROVAL_COMMANDS.approve, () => {
      throw { kind: 'safety_blocked', message: 'the confirmation code does not match; nothing was approved (2 tries left)' };
    });
    const e = await createTauriPlanBridge({ transport: t })
      .approvePlan({ plan_id: PLAN, hash: HASH, code: 'AAAAAA' })
      .catch((x: unknown) => x);
    expect(e).toBeInstanceOf(BridgeError);
    expect(e).toMatchObject({ kind: 'safety_blocked', message: expect.stringContaining('2 tries left') });
  });

  it("the desktop's own plans: prepare from the last scan, execute exactly the confirmed items", async () => {
    const t = fake();
    t.answers.set(CLEANUP_COMMANDS.prepare, () => ({
      plan_id: 'plan_01JDESK',
      content_hash: HASH,
      created_at: T0,
      expires_at: T0 + 3600,
      items: [candidate('a1'), candidate('b2')],
      left_out: [{ path: '/home/dev/code/vault/target', tier: 'S3', reason: 'protected: never cleaned' }],
    }));
    t.answers.set(CLEANUP_COMMANDS.execute, () => ({
      plan_id: 'plan_01JSUB',
      operation_id: 'op_01JRUN',
      items: [{ item_id: 'a1', outcome: 'deleted', reason: null, freed_bytes: 1000, moved_bytes: 0 }],
      predicted_bytes: 1000,
      measured_delta_bytes: 980,
      moved_bytes: 0,
      cancelled: false,
      finished_at: T0 + 5,
    }));
    const b = createTauriPlanBridge({ transport: t });
    const p = await b.preparePlan({ items: ['a1', 'b2'] });
    expect(p).toMatchObject({ plan_id: 'plan_01JDESK', hash: HASH, source: { kind: 'desktop' }, expires_at: iso(T0 + 3600) });
    expect(p.items.map((i) => i.candidate.id)).toEqual(['a1', 'b2']);
    expect(p.left_out).toEqual([{ label: '/home/dev/code/vault/target', tier: 'S3', reason: 'protected: never cleaned' }]);
    const events: string[] = [];
    const r = await b.executePlan({ plan_id: p.plan_id, hash: p.hash, items: ['a1'] }, { onEvent: (e) => events.push(e.kind) });
    expect(t.calls).toEqual([
      { cmd: 'prepare_desktop_plan', args: { items: ['a1', 'b2'] } },
      { cmd: 'execute_desktop_plan', args: { planId: 'plan_01JDESK', contentHash: HASH, items: ['a1'], typedCount: null } },
    ]);
    expect(r).toMatchObject({ plan_id: 'plan_01JSUB', measured_freed_bytes: 980, finished_at: iso(T0 + 5) });
    expect(r.items).toEqual([{ item_id: 'a1', outcome: 'deleted', freed_bytes: 1000, moved_bytes: 0 }]);
    expect(events).toEqual(['item_finished']);
    await b.executePlan({ plan_id: p.plan_id, hash: p.hash, items: ['a1'], typed_count: 1 });
    expect(t.calls.at(-1)).toEqual({ cmd: 'execute_desktop_plan', args: { planId: 'plan_01JDESK', contentHash: HASH, items: ['a1'], typedCount: 1 } });
  });

  it('a refused run (elevated, lock held, plan changed) comes back as the core error', async () => {
    const t = fake();
    t.answers.set(CLEANUP_COMMANDS.execute, () => {
      throw { kind: 'safety_blocked', message: 'scull is running as root/administrator … Nothing was changed.' };
    });
    await expect(
      createTauriPlanBridge({ transport: t }).executePlan({ plan_id: 'plan_x', hash: HASH, items: ['a1'] }),
    ).rejects.toMatchObject({ kind: 'safety_blocked' });
  });
});

describe('Tauri plan bridge: status polling', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  const op = (items: number, finished: boolean) => ({
    started_at: T0 + 10,
    finished_at: finished ? T0 + 20 : null,
    items: [
      { item_id: 'a1', outcome: 'removed' as const, reason: null, freed_bytes: 1000, moved_bytes: 0 },
      { item_id: 'b2', outcome: 'skipped' as const, reason: 'changed', freed_bytes: 0, moved_bytes: 0 },
    ].slice(0, items),
    predicted_bytes: 2000,
    moved_bytes: 0,
    freed_bytes: 1000,
    measured_delta_bytes: finished ? 990 : null,
  });

  it('approved elsewhere, agent running, item outcomes, finished; then it stops asking', async () => {
    const t = fake();
    const seq = [
      status({}),
      status({ state: 'approved', approved_at: T0 + 5, channel: 'terminal' }),
      status({ state: 'running', approved_at: T0 + 5, channel: 'terminal', used_at: T0 + 10, operation: op(1, false) }),
      status({ state: 'executed', approved_at: T0 + 5, channel: 'terminal', used_at: T0 + 10, operation: op(2, true) }),
    ];
    let i = 0;
    t.answers.set(APPROVAL_COMMANDS.status, () => seq[Math.min(i++, seq.length - 1)]);
    const events: ApprovalEvent[] = [];
    createTauriPlanBridge({ transport: t, pollMs: 100 }).watchApproval(PLAN, (e) => events.push(e));
    await vi.advanceTimersByTimeAsync(1000);
    expect(events.map((e) => e.kind)).toEqual(['approved', 'agent_started', 'item_finished', 'item_finished', 'finished']);
    expect(events[0]).toEqual({ kind: 'approved', at: iso(T0 + 5), channel: 'terminal' });
    expect(events[3]).toEqual({ kind: 'item_finished', outcome: { item_id: 'b2', outcome: 'skipped', freed_bytes: 0, moved_bytes: 0, reason: 'changed' } });
    expect(events[4]).toMatchObject({ kind: 'finished', result: { measured_freed_bytes: 990, predicted_bytes: 2000, finished_at: iso(T0 + 20) } });
    expect(t.calls.filter((c) => c.cmd === 'agent_plan_status')).toHaveLength(4);
    expect(t.calls[0]!.args).toEqual({ planId: PLAN });
  });

  it('voided, denied, revoked and expired each close the dialog once', () => {
    for (const [st, kind] of [
      ['voided', 'invalidated'],
      ['denied', 'denied'],
      ['revoked', 'revoked'],
      ['expired', 'expired'],
    ] as const) {
      const seen = { state: 'awaiting_approval' as AgentPlanStatus['state'], started: false, items: 0 };
      const s = status({ state: st, used_at: T0 + 3, denied_at: T0 + 3 });
      expect(eventsFor(s, seen).map((e) => e.kind)).toEqual([kind]);
      expect(eventsFor(s, seen)).toEqual([]);
    }
  });

  it('a failed read is retried; unsubscribing stops the polling', async () => {
    const t = fake();
    let n = 0;
    t.answers.set(APPROVAL_COMMANDS.status, () => {
      n += 1;
      if (n === 1) throw { kind: 'io', message: 'database is busy' };
      return status({});
    });
    const stop = createTauriPlanBridge({ transport: t, pollMs: 100 }).watchApproval(PLAN, () => undefined);
    await vi.advanceTimersByTimeAsync(350);
    expect(n).toBe(3);
    stop();
    await vi.advanceTimersByTimeAsync(1000);
    expect(n).toBe(3);
  });
});

describe('Tauri plan bridge: scull://approve links', () => {
  it('the launch link and later ones open the request; unsubscribing stops them', async () => {
    const t = fake();
    t.answers.set(APPROVAL_COMMANDS.takeDeepLink, () => PLAN);
    const opened: string[] = [];
    const stop = createTauriPlanBridge({ transport: t }).onOpenRequest((id) => opened.push(id));
    await vi.waitFor(() => expect(opened).toEqual([PLAN]));
    t.emit(OPEN_REQUEST_EVENT, 'plan_01JNEXT');
    expect(opened).toEqual([PLAN, 'plan_01JNEXT']);
    stop();
    t.emit(OPEN_REQUEST_EVENT, 'plan_01JLATER');
    expect(opened).toEqual([PLAN, 'plan_01JNEXT']);
  });
});
