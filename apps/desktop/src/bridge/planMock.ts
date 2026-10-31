// Mock plan bridge: illustrative plans from the design (03 Cleanup Review, 12 MCP approval)
// built from real `Candidate` shapes, with simulated execution. Touches nothing on disk; the
// "execution" is timers. Used in a plain browser, in tests and for screenshots.
import type { Action, Candidate, Category, Confidence, Evidence, ItemId, ReasonCode, Tier } from './types';
import { BridgeError } from './types';
import type {
  ApprovalEvent,
  ApprovalRequest,
  ExecuteOptions,
  ExecutionResult,
  ItemOutcome,
  PlanBridge,
  PlanItem,
  PlanPreview,
} from './plan';

const GiB = 1024 ** 3;
const DAY = 86_400;
const HOME = '/Users/ada';

export interface MockPlanOptions {
  /** Latency per call in ms (0 in tests). */
  latencyMs?: number;
  /** Time per executed item in ms (design: 700). */
  stepMs?: number;
  /** Delay between approval and the agent starting to run the plan (design: 2600). */
  agentDelayMs?: number;
  /** Clock, injectable for tests. */
  now?: () => number;
}

interface CandSpec {
  id: string;
  path: string;
  artifact: string;
  category?: Category;
  project?: [name: string, ecosystem: string, root: string];
  markers?: string[];
  locks?: string[];
  notes?: string[];
  idleDays?: number;
  tier: Tier;
  action: Action['type'];
  gib: number;
  confidence?: Confidence;
  reasons: [ReasonCode, string][];
  regen: [instructions: string | null, seconds: number | null, network?: boolean];
  rule: string;
  inUse?: boolean | null;
  hardlinked?: boolean | null;
}

/** A complete candidate (every SAFE-01 field) from a short spec. */
export function mockCandidate(s: CandSpec, nowSecs: number): Candidate {
  const evidence: Evidence[] = [
    ...(s.markers ?? []).map((m): Evidence => ({ kind: 'marker', path: m })),
    ...(s.locks ?? []).map((l): Evidence => ({ kind: 'lockfile', path: l })),
    { kind: 'known_artifact', artifact: s.artifact },
    ...(s.notes ?? []).map((t): Evidence => ({ kind: 'note', text: t })),
  ];
  if (s.idleDays != null) {
    evidence.push({ kind: 'last_modified', newest_unix: nowSecs - s.idleDays * DAY, sampled: false });
  }
  const allocated = Math.round(s.gib * GiB);
  const action = (s.action === 'tool_command' ? { type: 'tool_command', tool: 'npm', args: [] } : { type: s.action }) as Action;
  return {
    id: s.id,
    path: s.path,
    artifact: s.artifact,
    category: s.category ?? 'project_artifact',
    evidence,
    flags: {
      active: s.inUse === undefined ? false : s.inUse,
      cloud_placeholder: false,
      cross_mount: false,
      hardlinked: s.hardlinked === undefined ? false : s.hardlinked,
      symlink: false,
    },
    identity: { volume: 16777220, file: s.id.slice(0, 8) },
    project: s.project ? { name: s.project[0], ecosystem: s.project[1], root: s.project[2] } : null,
    reasons: s.reasons.map(([code, message]) => ({ code, message, tier: s.tier })),
    regeneration: {
      instructions: s.regen[0],
      cost:
        s.regen[1] == null
          ? { type: 'unknown' }
          : { type: 'estimate', seconds: s.regen[1], network: s.regen[2] ?? false },
    },
    rule: { id: s.rule, version: 1 },
    sizes: {
      allocated,
      logical: Math.round(allocated * 0.94),
      reclaimable: { bytes: allocated, confidence: s.confidence ?? 'high', shared_bytes: 0 },
    },
    tier: s.tier,
    action,
  };
}

const ids = (n: number) => n.toString(16).padStart(32, '0').replace(/^0{4}/, 'a7c1');

/** Design 03: the user's own plan from the last scan. */
function desktopItems(now: number): PlanItem[] {
  const c = (s: CandSpec) => mockCandidate(s, now);
  return [
    {
      candidate: c({
        id: ids(1), path: `${HOME}/code/rust-svc/target`, artifact: 'Cargo build output', rule: 'rust.target',
        project: ['rust-svc', 'rust', `${HOME}/code/rust-svc`], markers: ['Cargo.toml'], locks: ['Cargo.lock'],
        idleDays: 6, tier: 'S0', action: 'delete', gib: 15.2,
        reasons: [['regenerable', 'Build output with a lockfile: cargo rebuilds it exactly']],
        regen: ['cargo build', 240],
      }),
    },
    {
      candidate: c({
        id: ids(2), path: `${HOME}/code/shop-api/node_modules`, artifact: 'Installed npm dependencies', rule: 'node.node_modules',
        project: ['shop-api', 'node', `${HOME}/code/shop-api`], markers: ['package.json'], locks: ['package-lock.json'],
        idleDays: 3, tier: 'S0', action: 'delete', gib: 12.4,
        reasons: [['regenerable', 'Dependencies pinned by package-lock.json: npm ci reinstalls them']],
        regen: ['npm ci', 60, true],
      }),
      note: 'regrew 4× this month',
    },
    {
      candidate: c({
        id: ids(3), path: `${HOME}/code/web-app/.next`, artifact: 'Next.js build output', rule: 'node.next',
        project: ['web-app', 'node', `${HOME}/code/web-app`], markers: ['next.config.js'], locks: ['pnpm-lock.yaml'],
        idleDays: 9, tier: 'S0', action: 'delete', gib: 4.3,
        reasons: [['regenerable', 'Build output: next build recreates it']],
        regen: ['next build', 90],
      }),
    },
    {
      candidate: c({
        id: ids(4), path: `${HOME}/code/.worktrees`, artifact: 'Artifacts inside merged worktrees', rule: 'git.worktree_artifacts',
        notes: ['Markers + lockfiles in each'], idleDays: 14, tier: 'S0', action: 'delete', gib: 6.7, confidence: 'medium',
        reasons: [['regenerable', 'node_modules and target folders of merged, clean worktrees; the worktrees are kept']],
        regen: ['Rebuild inside the worktree if you use it again', null],
      }),
      note: 'node_modules and target inside merged worktrees · worktrees kept',
      label: '31 worktree artifact folders',
      members: 31,
    },
    {
      candidate: c({
        id: ids(5), path: `${HOME}/code/kart-game/Library`, artifact: 'Unity import cache', rule: 'unity.library',
        project: ['kart-game', 'unity', `${HOME}/code/kart-game`], markers: ['ProjectSettings/'],
        idleDays: 21, tier: 'S1', action: 'quarantine', gib: 8.2,
        reasons: [['costly_to_regenerate', 'Unity re-imports every asset: about 25 min']],
        regen: ['Unity re-import', 1500],
      }),
    },
    {
      candidate: c({
        id: ids(6), path: `${HOME}/code/ml-lab/.venv`, artifact: 'Python virtual environment', rule: 'python.venv',
        project: ['ml-lab', 'python', `${HOME}/code/ml-lab`], markers: ['pyproject.toml'],
        idleDays: 40, tier: 'S2', action: 'quarantine', gib: 9.8, confidence: 'medium',
        reasons: [['no_lockfile', 'No lockfile: may not be reproducible']],
        regen: [null, null],
      }),
      note: 'no lockfile, may not be reproducible',
    },
  ];
}

/** Design 12: what Claude Code asked for. */
function agentItems(now: number): PlanItem[] {
  const c = (s: CandSpec) => mockCandidate(s, now);
  return [
    {
      candidate: c({
        id: ids(11), path: `${HOME}/code/shop-api/node_modules`, artifact: 'Installed npm dependencies', rule: 'node.node_modules',
        project: ['shop-api', 'node', `${HOME}/code/shop-api`], markers: ['package.json'], locks: ['package-lock.json'],
        idleDays: 3, tier: 'S0', action: 'delete', gib: 12.4,
        reasons: [['regenerable', 'Dependencies pinned by package-lock.json: npm ci reinstalls them']],
        regen: ['npm ci', 60, true],
      }),
    },
    {
      candidate: c({
        id: ids(12), path: `${HOME}/code/shop-api/.turbo`, artifact: 'Turborepo build cache', rule: 'node.turbo',
        project: ['shop-api', 'node', `${HOME}/code/shop-api`], markers: ['turbo.json'], locks: ['package-lock.json'],
        tier: 'S0', action: 'delete', gib: 1.9,
        reasons: [['regenerable', 'Build cache: refilled on the next turbo run']],
        regen: ['Refilled on the next turbo run', 30],
      }),
      note: 'build cache',
    },
    {
      candidate: c({
        id: ids(13), path: `${HOME}/code/.worktrees/shop-api`, artifact: 'Artifacts inside merged worktrees', rule: 'git.worktree_artifacts',
        notes: ['Markers + lockfiles in each'], tier: 'S0', action: 'delete', gib: 4.8,
        reasons: [['regenerable', 'agent/fix-auth-91, -92, -94 · merged · worktrees kept']],
        regen: ['Rebuild inside the worktree if you use it again', null],
      }),
      label: '3 worktree artifact folders',
      members: 3,
      note: 'agent/fix-auth-91, -92, -94 · merged',
    },
    {
      candidate: c({
        id: ids(14), path: `${HOME}/code/kart-game/Library`, artifact: 'Unity import cache', rule: 'unity.library',
        project: ['kart-game', 'unity', `${HOME}/code/kart-game`], markers: ['ProjectSettings/'],
        idleDays: 21, tier: 'S1', action: 'quarantine', gib: 8.2,
        reasons: [['costly_to_regenerate', 'Unity re-imports every asset: about 25 min']],
        regen: ['Unity re-import', 1500],
      }),
    },
    {
      candidate: c({
        id: ids(15), path: `${HOME}/code/ml-lab/.venv`, artifact: 'Python virtual environment', rule: 'python.venv',
        project: ['ml-lab', 'python', `${HOME}/code/ml-lab`], markers: ['pyproject.toml'],
        idleDays: 40, tier: 'S2', action: 'quarantine', gib: 9.8, confidence: 'medium',
        reasons: [['no_lockfile', 'No lockfile: may not be reproducible']],
        regen: [null, null],
      }),
      note: 'no lockfile, may not be reproducible',
    },
  ];
}

/** The ID of the mock agent request (design 12). */
export const MOCK_AGENT_PLAN_ID = '9c41e07b';

/** Extra candidates other screens can ask a plan for (e.g. a worktree removal). */
const extraCandidates = new Map<ItemId, PlanItem>();

/** Register candidates the mock may put in a plan when asked by ID (Git Worktrees). */
export function registerMockCandidates(items: PlanItem[]): void {
  for (const it of items) extraCandidates.set(it.candidate.id, it);
}

type AgentStatus = 'pending' | 'approved' | 'running' | 'done' | 'denied' | 'revoked' | 'void' | 'expired';

export interface MockPlanBridge extends PlanBridge {
  /** Simulate a target of a plan changing on disk (D-34: an MCP plan is voided). */
  simulateTargetChanged(planId: string, itemId?: ItemId): void;
}

export function createMockPlanBridge(opts: MockPlanOptions = {}): MockPlanBridge {
  const latency = opts.latencyMs ?? 0;
  const stepMs = opts.stepMs ?? 700;
  const agentDelay = opts.agentDelayMs ?? 2600;
  const now = opts.now ?? (() => Date.now());
  const wait = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
  const iso = (ms: number) => new Date(ms).toISOString();
  let planSeq = 0;
  const plans = new Map<string, PlanPreview>();

  const agent = {
    status: 'pending' as AgentStatus,
    listeners: new Set<(e: ApprovalEvent) => void>(),
    timers: [] as ReturnType<typeof setTimeout>[],
    request: null as ApprovalRequest | null,
  };
  const emit = (e: ApprovalEvent) => agent.listeners.forEach((l) => l(structuredClone(e)));

  function agentRequest(): ApprovalRequest {
    if (agent.request) return agent.request;
    const t = now();
    const nowSecs = Math.floor(t / 1000);
    const plan: PlanPreview = {
      plan_id: MOCK_AGENT_PLAN_ID,
      hash: '9c41e07b5d2a18f3',
      created_at: iso(t - 78_000),
      expires_at: iso(t - 78_000 + 15 * 60_000),
      source: {
        kind: 'mcp',
        client: 'Claude Code',
        initials: 'CC',
        tool: 'prepare_cleanup',
        pid: 51022,
        command: 'claude',
        cwd: `${HOME}/code/shop-api`,
        note: 'Freeing space before the e2e run. These are build outputs and two old environments I found on this machine.',
      },
      items: agentItems(nowSecs),
      skipped: [],
      left_out: [
        {
          label: 'worktree agent/fix-auth-93',
          tier: 'S3',
          reason: 'Claude Code asked for it · 2 uncommitted files · protected work is never offered',
        },
      ],
      estimate_notes: [],
    };
    plans.set(plan.plan_id, plan);
    agent.request = { plan, code: 'K7Q-4MP' };
    return agent.request;
  }

  const normalize = (c: string) => c.toUpperCase().replace(/[^A-Z0-9]/g, '');

  function outcomeFor(it: PlanItem, ratio = 0.982): ItemOutcome {
    const c = it.candidate;
    const bytes = c.sizes.reclaimable.bytes;
    const t = c.action.type;
    if (t === 'quarantine' || t === 'trash') {
      return { item_id: c.id, outcome: t === 'trash' ? 'trashed' : 'quarantined', freed_bytes: 0, moved_bytes: bytes };
    }
    if (t === 'git_worktree_remove') {
      return { item_id: c.id, outcome: 'removed', freed_bytes: Math.round(bytes * ratio), moved_bytes: 0 };
    }
    return { item_id: c.id, outcome: 'deleted', freed_bytes: Math.round(bytes * ratio), moved_bytes: 0 };
  }

  function resultOf(planId: string, items: PlanItem[], outcomes: ItemOutcome[]): ExecutionResult {
    return {
      plan_id: planId,
      items: outcomes,
      predicted_bytes: items.reduce((a, b) => a + b.candidate.sizes.reclaimable.bytes, 0),
      measured_freed_bytes: outcomes.reduce((a, o) => a + o.freed_bytes, 0),
      moved_bytes: outcomes.reduce((a, o) => a + o.moved_bytes, 0),
      finished_at: iso(now()),
    };
  }

  function runAgent() {
    const req = agentRequest();
    const later = (ms: number, fn: () => void) => agent.timers.push(setTimeout(fn, ms));
    later(agentDelay, () => {
      if (agent.status !== 'approved') return;
      agent.status = 'running';
      emit({ kind: 'agent_started', at: iso(now()) });
      const outcomes: ItemOutcome[] = [];
      req.plan.items.forEach((it, k) => {
        later(k * stepMs, () => emit({ kind: 'item_started', item_id: it.candidate.id }));
        later((k + 1) * stepMs, () => {
          const o = outcomeFor(it);
          outcomes.push(o);
          emit({ kind: 'item_finished', outcome: o });
        });
      });
      later(req.plan.items.length * stepMs + 300, () => {
        agent.status = 'done';
        emit({ kind: 'finished', result: resultOf(req.plan.plan_id, req.plan.items, outcomes) });
      });
    });
  }

  const requirePending = (planId: string) => {
    if (planId !== MOCK_AGENT_PLAN_ID) throw new BridgeError({ kind: 'not_found', message: `no plan ${planId}` });
    const exp = Date.parse(agentRequest().plan.expires_at);
    if (now() >= exp) agent.status = 'expired';
    if (agent.status === 'expired') throw new BridgeError({ kind: 'plan_expired', message: 'the plan expired' });
    if (agent.status === 'void') throw new BridgeError({ kind: 'plan_invalidated', message: 'a target changed' });
  };

  return {
    kind: 'mock',

    async preparePlan(req) {
      await wait(latency);
      const t = now();
      const nowSecs = Math.floor(t / 1000);
      let items: PlanItem[];
      let left_out: PlanPreview['left_out'] = [];
      let skipped: PlanPreview['skipped'] = [];
      let estimate_notes: string[] = [];
      if (req.items && req.items.length > 0) {
        const pool = new Map([...desktopItems(nowSecs).map((i) => [i.candidate.id, i] as const), ...extraCandidates]);
        items = req.items.map((id) => pool.get(id)).filter((i): i is PlanItem => i !== undefined);
        if (items.length === 0) throw new BridgeError({ kind: 'not_found', message: 'none of these items are in the last scan' });
      } else {
        items = desktopItems(nowSecs);
        skipped = [
          { label: `${HOME}/work/payments/target`, reason: 'In use: cargo build is running (pid 48213)' },
          { label: 'worktree agent/fix-auth-93', reason: 'New untracked file since scan: notes.md · now protected' },
          { label: `${HOME}/code/old-site/node_modules`, reason: 'Changed since scan: folder was replaced · identity mismatch' },
        ];
        left_out = [
          { label: `${HOME}/code/vault/target`, tier: 'S3', reason: 'Protected by a .scull-protect marker · never offered' },
        ];
        estimate_notes = [
          "Two pnpm projects are linked to the shared store, so deleting their node_modules frees almost nothing. They're left out of this estimate. Prune the store from Quick Clean instead.",
        ];
      }
      const plan: PlanPreview = {
        plan_id: `7f3ac21e${(++planSeq).toString(16).padStart(8, '0')}`,
        hash: `7f3ac21e${'0'.repeat(7)}${planSeq}`,
        created_at: iso(t - 120_000),
        expires_at: iso(t - 120_000 + 24 * 3600_000),
        source: { kind: 'desktop' },
        items,
        skipped,
        left_out,
        estimate_notes,
      };
      plans.set(plan.plan_id, plan);
      return structuredClone(plan);
    },

    async executePlan(req, o: ExecuteOptions = {}) {
      const plan = plans.get(req.plan_id);
      if (!plan || plan.hash !== req.hash) {
        throw new BridgeError({ kind: 'plan_tampered', message: 'the plan does not match what was reviewed' });
      }
      const chosen = plan.items.filter((i) => req.items.includes(i.candidate.id));
      // A plan only ever narrows (SAFE-11): an ID that is not in the plan, or an S3 item, is refused.
      if (chosen.length !== req.items.length || chosen.some((i) => i.candidate.tier === 'S3')) {
        throw new BridgeError({ kind: 'safety_blocked', message: 'items outside the reviewed plan' });
      }
      const outcomes: ItemOutcome[] = [];
      for (const it of chosen) {
        if (o.signal?.aborted) break;
        o.onEvent?.({ kind: 'item_started', item_id: it.candidate.id });
        await wait(stepMs);
        const out = outcomeFor(it);
        outcomes.push(out);
        o.onEvent?.({ kind: 'item_finished', outcome: structuredClone(out) });
      }
      await wait(Math.max(latency, stepMs > 0 ? 400 : 0));
      return resultOf(plan.plan_id, chosen, outcomes);
    },

    async approvalRequest(planId) {
      await wait(latency);
      if (planId !== MOCK_AGENT_PLAN_ID) return null;
      return structuredClone(agentRequest());
    },

    async approvePlan(req) {
      requirePending(req.plan_id);
      const r = agentRequest();
      if (agent.status !== 'pending') throw new BridgeError({ kind: 'usage', message: `plan is ${agent.status}` });
      if (req.hash !== r.plan.hash) throw new BridgeError({ kind: 'plan_tampered', message: 'fingerprint mismatch' });
      if (normalize(req.code) !== normalize(r.code)) throw new BridgeError({ kind: 'usage', message: 'wrong code' });
      agent.status = 'approved';
      runAgent();
      return { approved_at: iso(now()) };
    },

    async denyPlan(req) {
      requirePending(req.plan_id);
      if (agent.status !== 'pending') throw new BridgeError({ kind: 'usage', message: `plan is ${agent.status}` });
      agent.status = 'denied';
      return { denied_at: iso(now()) };
    },

    async revokeApproval(req) {
      requirePending(req.plan_id);
      if (agent.status !== 'approved') throw new BridgeError({ kind: 'usage', message: 'already started' });
      agent.status = 'revoked';
      agent.timers.forEach(clearTimeout);
      agent.timers = [];
      return { revoked_at: iso(now()) };
    },

    watchApproval(_planId, onEvent) {
      agent.listeners.add(onEvent);
      return () => agent.listeners.delete(onEvent);
    },

    simulateTargetChanged(planId, itemId) {
      if (planId !== MOCK_AGENT_PLAN_ID) return;
      if (agent.status !== 'pending' && agent.status !== 'approved') return;
      agent.status = 'void';
      agent.timers.forEach(clearTimeout);
      agent.timers = [];
      const id = itemId ?? agentRequest().plan.items[0]!.candidate.id;
      emit({
        kind: 'invalidated',
        item_id: id,
        reason: 'Changed 40 s ago: npm install wrote 214 files · plan no longer matches the disk',
        at: iso(now()),
      });
    },
  };
}
