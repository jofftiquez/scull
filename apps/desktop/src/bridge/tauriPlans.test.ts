// The Tauri plan bridge over a fake transport: command names and arguments, and payload
// mapping for the desktop's own plans.
import { describe, expect, it } from 'vitest';
import type { Candidate } from './generated';
import type { TauriTransport } from './tauri';
import { CLEANUP_COMMANDS, createTauriPlanBridge } from './tauriPlans';

const T0 = 1_900_000_000; // Unix seconds
const iso = (s: number) => new Date(s * 1000).toISOString();
const HASH = 'a'.repeat(64);

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
