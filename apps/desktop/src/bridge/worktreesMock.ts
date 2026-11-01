// The design's illustrative worktrees (design 04), in the REAL `WorktreeReport` shape, for the
// mock bridge. Like MOCK_DRIVES, they stand in for a machine with many agent worktrees; the
// recorded fixture (a generated tree) only has two. Removable ones also get a candidate that the
// mock plan bridge can put in a plan (`git_worktree_remove`), so removal can be shown end to end
// without touching anything.
import type { PlanItem } from './plan';
import { mockCandidate, registerMockCandidates } from './planMock';
import type { Evidence, LockState, MergeStatus, ReasonCode, Tier, WorktreeReport, WorktreesView } from './types';

const GiB = 1024 ** 3;
const HOME = '/Users/ada';
const NOW = Math.floor(Date.now() / 1000);

interface Spec {
  n: number;
  branch: string;
  repo: string;
  path: string;
  tier: Tier;
  gib: number;
  artifacts: string[];
  uniqueIgnored?: string[];
  tracked?: number;
  untracked?: number;
  unpushed?: number;
  upstream?: string | null;
  merge: MergeStatus;
  lock?: LockState;
  idleHours: number;
  recent?: boolean;
  reasons: [ReasonCode, string][];
}

const known = <T>(value: T) => ({ status: 'known' as const, value });
const list = (paths: string[]) => known({ paths, total: paths.length, truncated: false });

function build(s: Spec): { report: WorktreeReport; item: PlanItem | null } {
  const id = `b0a7${s.n.toString(16).padStart(28, '0')}`;
  const oid = `${s.n}e1c9a0`.padEnd(40, '7');
  const common = `${HOME}/code/${s.repo}/.git`;
  const removable = s.tier === 'S0' || s.tier === 'S1';
  const evidence: Evidence[] = [{ kind: 'last_modified', newest_unix: NOW - s.idleHours * 3600, sampled: false }];
  const report: WorktreeReport = {
    allocated: Math.round(s.gib * GiB),
    candidate: removable ? id : null,
    common_dir: common,
    inside_roots: true,
    kind: 'linked',
    path: s.path,
    decision: {
      action: { type: removable ? 'git_worktree_remove' : s.tier === 'S2' ? 'quarantine' : 'none' },
      artifact_cleanup: 'allowed',
      artifacts: s.artifacts.map((p) => ({ path: p, rule: p.startsWith('target') ? 'rust.target' : p.startsWith('.next') ? 'node.next' : 'node.node_modules' })),
      evidence,
      reasons: s.reasons.map(([code, message]) => ({ code, message, tier: s.tier })),
      tier: s.tier,
      unique_ignored: s.uniqueIgnored ?? [],
    },
    state: {
      activity: known({ complete: true, entries_sampled: 1200, newest_path: null, newest_unix: NOW - s.idleHours * 3600 }),
      git_dir: `${common}/worktrees/${s.path.split('/').pop()}`,
      head: known({ kind: 'branch' as const, name: `refs/heads/${s.branch}`, oid }),
      head_reachability: known({ kind: 'reachable_from' as const, refs: [`refs/remotes/origin/${s.branch}`], total: 1 }),
      ignored: list([...s.artifacts, ...(s.uniqueIgnored ?? [])]),
      kind: 'linked',
      lock: s.lock ?? { state: 'unlocked' },
      merge: s.merge,
      missing: false,
      name: s.path.split('/').pop(),
      path: s.path,
      repo: { bare: false, common_dir: common, main_worktree: `${HOME}/code/${s.repo}` },
      submodules: known([]),
      tracked_changes: list(Array.from({ length: s.tracked ?? 0 }, (_, i) => `src/file${i}.rs`)),
      unpushed: known({ count: s.unpushed ?? 0, oids: Array.from({ length: s.unpushed ?? 0 }, () => oid), truncated: false }),
      untracked: list(Array.from({ length: s.untracked ?? 0 }, (_, i) => `notes${i}.md`)),
      upstream: known(s.upstream === null ? null : { name: s.upstream ?? `origin/${s.branch}`, exists: true, ahead: s.unpushed ?? 0, behind: 0 }),
    },
  };
  if (s.recent) evidence.push({ kind: 'in_use', signal: 'modified within 24 hours', by: null });
  const item: PlanItem | null = removable
    ? {
        candidate: mockCandidate(
          {
            id,
            path: s.path,
            artifact: `Git worktree (${s.branch})`,
            category: 'worktree',
            rule: 'git.worktree',
            tier: s.tier,
            action: 'git_worktree_remove',
            gib: s.gib,
            idleDays: Math.floor(s.idleHours / 24),
            reasons: s.reasons,
            regen: [`git worktree add ${s.path} ${s.branch}`, 30],
          },
          NOW,
        ),
        label: `worktree ${s.branch}`,
      }
    : null;
  return { report, item };
}

const SPECS: Spec[] = [
  {
    n: 1, branch: 'agent/refactor-cart-12', repo: 'shop-api', path: `${HOME}/code/.worktrees/shop-api/agent-refactor-cart-12`,
    tier: 'S1', gib: 1.2, artifacts: ['node_modules/'], idleHours: 21 * 24,
    merge: { method: 'ancestor_of_default', default_ref: 'refs/remotes/origin/main' },
    reasons: [['worktree_clean', 'Clean, unlocked, and every commit is merged into main and pushed. Removal keeps the branch.']],
  },
  {
    n: 2, branch: 'agent/fix-flaky-tests-7', repo: 'rust-svc', path: `${HOME}/code/.worktrees/rust-svc/agent-fix-flaky-tests-7`,
    tier: 'S1', gib: 2.4, artifacts: ['target/'], idleHours: 16 * 24,
    merge: { method: 'ancestor_of_default', default_ref: 'refs/remotes/origin/main' },
    reasons: [['worktree_clean', 'Clean, unlocked, merged and pushed. Most of its size is a regenerable target/ folder.']],
  },
  {
    n: 3, branch: 'feat/search-v2', repo: 'web-app', path: `${HOME}/code/web-app-search-v2`,
    tier: 'S2', gib: 0.6, artifacts: ['.next/'], idleHours: 33 * 24,
    merge: { method: 'patch_id_only', commits: 4, default_ref: 'refs/remotes/origin/main' },
    reasons: [['merge_unproven', 'Looks squash-merged, but scull can’t prove it without asking GitHub. Review before removing.']],
  },
  {
    n: 4, branch: 'agent/migrate-db-4', repo: 'shop-api', path: `${HOME}/code/.worktrees/shop-api/agent-migrate-db-4`,
    tier: 'S2', gib: 0.4, artifacts: ['node_modules/'], uniqueIgnored: ['.env', 'dev.sqlite'], idleHours: 18 * 24,
    merge: { method: 'ancestor_of_default', default_ref: 'refs/remotes/origin/main' },
    reasons: [['worktree_unique_ignored', 'Holds ignored files that aren’t build artifacts. They exist nowhere else.']],
  },
  {
    n: 5, branch: 'fix/payments-hotfix', repo: 'payments', path: `${HOME}/work/payments-hotfix`,
    tier: 'S3', gib: 1.1, artifacts: ['node_modules/'], unpushed: 3, idleHours: 2, recent: true,
    merge: { method: 'not_merged', checked: ['refs/remotes/origin/main'] },
    reasons: [['unpushed_commits', 'Has 3 commits that exist only on this machine. scull will never remove it.']],
  },
  {
    n: 6, branch: 'agent/perf-probe-22', repo: 'rust-svc', path: `${HOME}/code/.worktrees/rust-svc/agent-perf-probe-22`,
    tier: 'S3', gib: 2.0, artifacts: ['target/'], idleHours: 3 * 24, lock: { state: 'locked', reason: 'benchmark running' },
    merge: { method: 'not_merged', checked: ['refs/remotes/origin/main'] },
    reasons: [['worktree_locked', 'Locked by its owner: “benchmark running”. Locked worktrees are never removed.']],
  },
];

const BUILT = SPECS.map(build);

/** The design's six worktrees. */
export const DESIGN_WORKTREES: readonly WorktreeReport[] = BUILT.map((b) => b.report);

registerMockCandidates(BUILT.flatMap((b) => (b.item ? [b.item] : [])));

/** The recorded view plus the design's worktrees (first). */
export function withDesignWorktrees(v: WorktreesView): WorktreesView {
  return { ...v, worktrees: [...DESIGN_WORKTREES, ...v.worktrees] };
}
