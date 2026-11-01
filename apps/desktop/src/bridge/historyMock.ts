// The design's illustrative operation history (design 09), in the REAL `HistoryView` shape, for
// the mock bridge. Like MOCK_DRIVES it stands in for a machine with a few months of cleanups:
// this build of the core has no Executor, so the real `history` command can only answer with an
// empty list. Times are relative to now (the design's "Today 16:12", "Oct 28 10:41", ... on
// Oct 31), so the quarantine countdown always reads like the design.
import type { AuditLineKind, HistoryItem, HistoryOperation, HistoryView, ItemOutcome, OperationKind, Tier } from './types';

const GiB = 1024 ** 3;
const g = (x: number) => Math.round(x * GiB);
const HOME = '/Users/ada';
const DAY = 86400;

/** Unix seconds `daysAgo` days before `now`, at local `hh:mm:ss`. */
function at(now: number, daysAgo: number, hh: number, mm: number, ss = 0): number {
  const d = new Date(now * 1000);
  d.setDate(d.getDate() - daysAgo);
  d.setHours(hh, mm, ss, 0);
  return Math.floor(d.getTime() / 1000);
}

type ItemSpec = [path: string, gib: number, outcome: ItemOutcome, extra?: Partial<HistoryItem>];
type LineSpec = [hms: [number, number, number], kind: AuditLineKind, text: string];

interface OpSpec {
  id: string;
  daysAgo: number;
  hm: [number, number];
  kind: OperationKind;
  origin: string;
  agent?: string;
  summary?: string;
  predicted: number;
  measured: number;
  explanations?: string[];
  method?: string;
  items: ItemSpec[];
  log: LineSpec[];
}

/** `paths` sharing `gib` (weights 1, 0.9, 0.8, ... so sizes differ), all with `outcome`. */
function spread(paths: string[], gib: number, outcome: ItemOutcome, extra?: Partial<HistoryItem>): ItemSpec[] {
  const w = paths.map((_, i) => Math.max(0.2, 1 - i * 0.1));
  const sum = w.reduce((a, b) => a + b, 0);
  return paths.map((p, i) => [p, (gib * w[i]!) / sum, outcome, extra]);
}

const action = (o: ItemOutcome, kind: OperationKind): string =>
  kind === 'quick_clean' ? 'tool' : kind === 'worktrees' ? 'worktree_remove' : o === 'quarantined' ? 'quarantine' : 'delete';

function build(now: number, s: OpSpec): HistoryOperation {
  const started = at(now, s.daysAgo, s.hm[0], s.hm[1]);
  const items = s.items.map(([path, gib, outcome, extra], i): HistoryItem => {
    const bytes = g(gib);
    const moved = outcome === 'quarantined' || outcome === 'restored';
    return {
      item_id: `${s.id}-${i + 1}`,
      path: path.startsWith('~') ? HOME + path.slice(1) : path,
      label: null,
      action: action(outcome, s.kind),
      tier: (moved || outcome === 'purged' ? 'S1' : 'S0') as Tier,
      outcome,
      reason: null,
      predicted_bytes: bytes,
      freed_bytes: moved ? null : bytes,
      moved_bytes: moved ? bytes : null,
      restored_to: null,
      ...extra,
      // Quarantined: restorable for 7 days; purged: when that ended.
      restorable_until: outcome === 'quarantined' || outcome === 'purged' ? started + 7 * DAY : null,
    };
  });
  const log = s.log.map(([[h, m, sec], kind, text]) => ({ at: at(now, s.daysAgo, h, m, sec), kind, text }));
  return {
    operation_id: `op-${s.id}`,
    plan_id: `plan-${s.id}`,
    kind: s.kind,
    approver: s.agent ? `${s.agent} prepared · you approved` : 'you',
    method: s.method ?? 'desktop',
    origin: s.origin,
    agent: s.agent ?? null,
    summary: s.summary ?? null,
    started_at: started,
    finished_at: log.length ? log[log.length - 1]!.at : started,
    status: 'completed',
    predicted_bytes: g(s.predicted),
    measured_delta_bytes: g(s.measured),
    delta_explanations: s.explanations ?? [],
    items,
    log,
    chain: { intact: true, rows: items.length + 2, problems: [] },
  };
}

const SPECS: OpSpec[] = [
  {
    id: 'today',
    daysAgo: 0,
    hm: [16, 12],
    kind: 'quick_clean',
    origin: 'Overview',
    predicted: 14.9,
    measured: 14.6,
    items: [
      ['~/.npm', 6.1, 'tool_run', { label: 'npm' }],
      ['~/Library/pnpm/store', 4.8, 'tool_run', { label: 'pnpm prune' }],
      ['~/.bun/install/cache', 2.6, 'tool_run', { label: 'Bun' }],
      ['~/Library/Caches/Yarn', 1.4, 'tool_run', { label: 'Yarn' }],
    ],
    log: [
      [[16, 12, 3], 'approval', 'plan 9b21·04ac approved · desktop'],
      [[16, 12, 3], 'check', 'resolved npm → /opt/homebrew/bin/npm'],
      [[16, 12, 4], 'action', 'npm cache clean --force · ok'],
      [[16, 12, 5], 'action', 'pnpm store prune · ok'],
      [[16, 12, 6], 'measured', 'measured +14.6 GiB free'],
    ],
  },
  {
    id: 'oct28',
    daysAgo: 3,
    hm: [10, 41],
    kind: 'cleanup',
    origin: 'Cleanup Review',
    predicted: 12.1,
    measured: 11.8,
    items: [
      ['~/code/old-site/node_modules', 2.9, 'removed'],
      ['~/code/rust-svc/target', 4.4, 'removed'],
      ['~/code/old-ml/.venv', 2.4, 'quarantined'],
      ['~/code/proto/Library', 2.4, 'quarantined'],
    ],
    log: [
      [[10, 41, 12], 'approval', 'plan 7f3a·c21e approved · desktop'],
      [[10, 41, 12], 'check', 're-check 4/4 ok'],
      [[10, 41, 13], 'action', 'deleted 2 S0 items'],
      [[10, 41, 14], 'action', 'quarantined 2 items · same volume'],
      [[10, 41, 15], 'measured', 'measured +11.8 GiB free'],
    ],
  },
  {
    id: 'oct20',
    daysAgo: 11,
    hm: [18, 5],
    kind: 'cleanup',
    origin: 'Cleanup Review',
    summary: 'Build outputs in 5 projects',
    predicted: 9.4,
    measured: 9.0,
    items: spread(['~/code/web-app/.next', '~/code/web-app/node_modules', '~/code/shop-api/dist', '~/code/shop-api/node_modules', '~/code/rust-svc/target/debug', '~/code/cli-tools/bin', '~/code/kart-game/Library', '~/code/kart-game/Temp', '~/code/ml-lab/.pytest_cache'], 9.4, 'removed'),
    log: [
      [[18, 5, 40], 'approval', 'plan 51c0·7e19 approved · desktop'],
      [[18, 5, 44], 'measured', 'measured +9.0 GiB free'],
    ],
  },
  {
    id: 'oct15',
    daysAgo: 16,
    hm: [9, 30],
    kind: 'worktrees',
    origin: 'Git Worktrees',
    summary: 'Merged agent worktrees · branches kept',
    predicted: 3.2,
    measured: 3.1,
    items: spread(['agent/fix-login-3', 'agent/docs-sweep-1', 'agent/bump-deps-9', 'agent/lint-fixes-2', 'agent/cache-layer-5', 'agent/retry-http-8', 'agent/test-split-4'].map((b) => `~/code/.worktrees/shop-api/${b.replace('/', '-')}`), 3.2, 'removed'),
    log: [
      [[9, 30, 2], 'action', 'git worktree remove × 7 · ok'],
      [[9, 30, 3], 'measured', 'measured +3.1 GiB free'],
    ],
  },
  {
    id: 'oct9',
    daysAgo: 22,
    hm: [14, 22],
    kind: 'cleanup',
    origin: 'Cleanup Review',
    summary: 'Build outputs and old caches',
    predicted: 22.8,
    measured: 21.9,
    items: [
      ...spread(['~/code/old-api/node_modules', '~/code/old-api/dist', '~/code/site-2023/node_modules', '~/code/site-2023/.next', '~/code/rust-svc/target', '~/code/payments/target', '~/code/kart-game/Library', '~/code/web-app/node_modules', '~/code/ml-lab/.venv', '~/code/cli-tools/vendor'], 19.6, 'removed'),
      ...spread(['~/code/proto-a/build', '~/code/proto-b/build', '~/code/experiments/.venv', '~/code/notes-app/node_modules'], 3.2, 'purged', { restorable_until: 0 }),
    ],
    log: [
      [[14, 22, 10], 'approval', 'plan 2a77·bb10 approved · desktop'],
      [[14, 22, 19], 'measured', 'measured +21.9 GiB free'],
    ],
  },
  {
    id: 'sep28',
    daysAgo: 33,
    hm: [11, 3],
    kind: 'cleanup',
    origin: 'Cleanup Review',
    summary: 'Freed less than predicted',
    predicted: 7.1,
    measured: 2.8,
    explanations: ['local_snapshot'],
    items: spread(['~/code/web-app/node_modules', '~/code/shop-api/node_modules', '~/code/rust-svc/target', '~/code/ml-lab/.mypy_cache', '~/code/cli-tools/dist'], 7.1, 'removed'),
    log: [
      [[11, 3, 30], 'measured', 'measured +2.8 GiB free'],
      [[11, 3, 30], 'warning', 'warning: local snapshot present'],
    ],
  },
  {
    id: 'sep14',
    daysAgo: 47,
    hm: [20, 47],
    kind: 'agent',
    origin: 'MCP',
    agent: 'Claude Code',
    method: 'desktop_dialog',
    summary: 'Prepared by Claude Code · approved in desktop dialog',
    predicted: 12.6,
    measured: 11.9,
    items: spread(['~/code/rust-svc/target', '~/code/payments/target', '~/code/shop-api/node_modules'], 12.6, 'removed'),
    log: [
      [[20, 46, 58], 'approval', 'mcp prepare_cleanup · plan c901·11de'],
      [[20, 47, 31], 'approval', 'approved in desktop dialog · code 4-1-7'],
      [[20, 47, 35], 'measured', 'measured +11.9 GiB free'],
    ],
  },
];

/** The design's seven operations, newest first. */
export function designHistory(now = Math.floor(Date.now() / 1000)): HistoryView {
  return { state: 'ok', detail: null, now, audit_api: true, operations: SPECS.map((s) => build(now, s)) };
}
