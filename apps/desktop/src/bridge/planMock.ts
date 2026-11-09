// Mock plan bridge: an illustrative desktop plan from the design (03 Cleanup Review)
// built from real `Candidate` shapes, with simulated execution. Touches nothing on disk; the
// "execution" is timers. Used in a plain browser, in tests and for screenshots.
import type { Action, Candidate, Category, Confidence, Evidence, ItemId, ReasonCode, Tier } from './types';
import { BridgeError } from './types';
import type {
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

const extraCandidates = new Map<ItemId, PlanItem>();

/** Register candidates the mock may put in a plan when asked by ID (Git Worktrees). */
export function registerMockCandidates(items: PlanItem[]): void {
  for (const it of items) extraCandidates.set(it.candidate.id, it);
}

/** Candidates of the mock's last scan and caches report, by ID (the core plans from its last scan). */
const scanCandidates = new Map<ItemId, PlanItem>();

/** Remember what a mock scan (or caches report) returned, so a plan can be asked for by ID
 *  (the Overview's "Add to cleanup" basket). Registered items above take precedence. */
export function registerScanCandidates(candidates: readonly Candidate[]): void {
  for (const c of candidates) scanCandidates.set(c.id, { candidate: structuredClone(c) });
}

export function createMockPlanBridge(opts: MockPlanOptions = {}): PlanBridge {
  const latency = opts.latencyMs ?? 0;
  const stepMs = opts.stepMs ?? 700;
  const now = opts.now ?? (() => Date.now());
  const wait = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
  const iso = (ms: number) => new Date(ms).toISOString();
  let planSeq = 0;
  const plans = new Map<string, PlanPreview>();

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
        const pool = new Map([
          ...desktopItems(nowSecs).map((i) => [i.candidate.id, i] as const),
          ...scanCandidates,
          ...extraCandidates,
        ]);
        items = [];
        for (const id of new Set(req.items)) {
          const it = pool.get(id);
          if (!it) skipped.push({ label: id, reason: 'Not in the last scan · rescan to plan it' });
          // A plan never holds an S3 item (SAFE-11): it is listed as left out, never offered.
          else if (it.candidate.tier === 'S3') left_out.push({ label: it.candidate.path, tier: 'S3', reason: `${it.candidate.reasons[0]?.message ?? 'Protected'} · never offered` });
          else items.push(structuredClone(it));
        }
        if (items.length === 0 && left_out.length === 0) {
          throw new BridgeError({ kind: 'not_found', message: 'none of these items are in the last scan' });
        }
        if (items.length === 0) throw new BridgeError({ kind: 'safety_blocked', message: 'protected items are never planned' });
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
  };
}
