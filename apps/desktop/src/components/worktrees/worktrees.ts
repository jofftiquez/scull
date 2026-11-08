// Pure view model for Git Worktrees (design 04): verdict, the seven checks, filters. Derived
// from the core's `WorktreeReport` (GIT-02 state + GIT-03 decision); nothing here decides
// safety on its own: the verdict is the core's tier, and removal is offered only when D-28
// allows `git worktree remove` at that tier.
import type { Tier, WorktreeReport } from '../../bridge';
import { actionAllowed, displayPath } from '../review/plan';

export type Verdict = 'removable' | 'review' | 'protected';

export const VERDICT_LABEL: Record<Verdict, string> = { removable: 'Removable', review: 'Review', protected: 'Protected' };

export function verdictOf(r: WorktreeReport): Verdict {
  const t = r.decision.tier;
  if (t === 'S3' || r.kind !== 'linked' || !r.inside_roots) return 'protected';
  if (canRemove(r)) return 'removable';
  return 'review';
}

/** Whole-worktree removal is offered only for a linked worktree whose tier allows a Git action (S0/S1). */
export function canRemove(r: WorktreeReport): boolean {
  return (
    r.kind === 'linked' &&
    r.inside_roots &&
    !!r.candidate &&
    r.decision.action.type === 'git_worktree_remove' &&
    actionAllowed(r.decision.tier, r.decision.action)
  );
}

/** Explain a core refusal beside the disabled action, with a practical next step. */
export function removalBlock(r: WorktreeReport): { title: string; detail: string } | null {
  if (canRemove(r)) return null;
  if (r.kind === 'main') return { title: 'Main working tree', detail: 'This is the main checkout. Only linked worktrees can be removed here.' };
  if (!r.inside_roots) return { title: 'Outside scan folders', detail: 'Add this worktree folder to your scan folders in Settings, then run Scan so its contents can be checked.' };
  if (r.state.lock.state === 'locked') return { title: 'Worktree is locked', detail: `Keep it locked while it is needed${r.state.lock.reason ? ` (${r.state.lock.reason})` : ''}. Unlock it in Git only when that work is finished, then scan again.` };
  const checks = checksOf(r).filter((c) => c.status !== 'pass');
  const next: string[] = [];
  if (checks.some((c) => /uncommitted|untracked/.test(c.label))) next.push('Commit or save any local files you need.');
  if (checks.some((c) => /pushed|Merged/.test(c.label))) next.push('Merge or push the commits you want to keep.');
  if (checks.some((c) => /Ignored/.test(c.label))) next.push('Move unique ignored files somewhere safe.');
  if (checks.some((c) => /in use/.test(c.label))) next.push('Close processes using this folder.');
  const reason = verdictReason(r);
  return { title: 'Needs review before removal', detail: `${reason}. ${next.join(' ')} Run Scan again after resolving these checks.` };
}

/** "Removable · S1". */
export function verdictLine(r: WorktreeReport): string {
  return `${VERDICT_LABEL[verdictOf(r)]} · ${r.decision.tier}`;
}

/** The reason that set the tier. */
export function verdictReason(r: WorktreeReport): string {
  const d = r.decision;
  return (d.reasons.find((x) => x.tier === d.tier) ?? d.reasons[0])?.message ?? 'No reason recorded';
}

export function branchOf(r: WorktreeReport): string {
  const h = r.state.head;
  if (h.status === 'known') {
    return h.value.kind === 'branch' ? h.value.name.replace(/^refs\/heads\//, '') : `detached ${h.value.oid.slice(0, 7)}`;
  }
  return r.state.name ?? r.path.split(/[\\/]/).pop() ?? r.path;
}

export function repoOf(r: WorktreeReport): string {
  const main = r.state.repo.main_worktree;
  if (main) return main.split(/[\\/]/).filter(Boolean).pop() ?? main;
  return r.common_dir.replace(/[\\/]\.git[\\/]?$/, '').split(/[\\/]/).filter(Boolean).pop() ?? r.common_dir;
}

/** Agent-made worktrees, recognised by their branch prefix (agent/, claude/, codex/, cursor/). */
export function isAgent(r: WorktreeReport): boolean {
  return /^(agent|agents|claude|codex|cursor|copilot|devin)\//i.test(branchOf(r));
}

const lastPart = (s: string) => s.replace(/[\\/]+$/, '').split(/[\\/]/).pop() ?? s;

/** "shop-api · agent branch · idle 21 days" / "· locked" / "· active 2 hours ago". */
export function metaLine(r: WorktreeReport, nowSecs = Date.now() / 1000): string {
  const parts = [repoOf(r)];
  if (r.kind === 'main') parts.push('main working tree');
  else if (isAgent(r)) parts.push('agent branch');
  if (r.state.lock.state === 'locked') parts.push('locked');
  else if (r.state.activity.status === 'known' && r.state.activity.value.newest_unix != null) {
    const s = Math.max(0, nowSecs - r.state.activity.value.newest_unix);
    if (s < 86_400) {
      const h = Math.max(1, Math.round(s / 3600));
      parts.push(`active ${h} hour${h === 1 ? '' : 's'} ago`);
    } else {
      const d = Math.floor(s / 86_400);
      parts.push(`idle ${d} day${d === 1 ? '' : 's'}`);
    }
  }
  return parts.join(' · ');
}

export type CheckStatus = 'pass' | 'fail' | 'unknown';
export interface Check {
  label: string;
  note: string;
  status: CheckStatus;
}

const count = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

/** The seven checks of design 04, from the core's observations. Unknown is never "pass". */
export function checksOf(r: WorktreeReport): Check[] {
  const s = r.state;
  const d = r.decision;
  const out: Check[] = [];
  const obsList = (o: WorktreeReport['state']['untracked'], label: string, what: string) => {
    if (o.status === 'unknown') out.push({ label, note: o.reason, status: 'unknown' });
    else if (o.value.total === 0) out.push({ label, note: '', status: 'pass' });
    else out.push({ label, note: count(o.value.total, what), status: 'fail' });
  };
  obsList(s.tracked_changes, 'No uncommitted changes', 'changed file');
  obsList(s.untracked, 'No untracked files', 'untracked file');

  const arts = d.artifacts.map((a) => lastPart(a.path)).join(', ');
  if (d.unique_ignored.length) {
    const u = d.unique_ignored.map(lastPart);
    out.push({ label: 'Ignored files are artifacts only', note: `${u.join(' and ')} ${u.length === 1 ? 'is' : 'are'} unique`, status: 'fail' });
  } else if (s.ignored.status === 'unknown') {
    out.push({ label: 'Ignored files are artifacts only', note: s.ignored.reason, status: 'unknown' });
  } else {
    out.push({ label: 'Ignored files are artifacts only', note: arts, status: 'pass' });
  }

  const up = s.upstream.status === 'known' ? s.upstream.value : null;
  if (s.unpushed.status === 'unknown') out.push({ label: 'All commits on a pushed ref', note: s.unpushed.reason, status: 'unknown' });
  else if (s.unpushed.value.count > 0) {
    out.push({ label: 'All commits on a pushed ref', note: count(s.unpushed.value.count, 'unpushed commit'), status: 'fail' });
  } else out.push({ label: 'All commits on a pushed ref', note: up?.name ?? '', status: 'pass' });

  const m = s.merge;
  const shortRef = (ref: string) => ref.replace(/^refs\/(remotes\/[^/]+|heads)\//, '');
  switch (m.method) {
    case 'ancestor_of_default':
      out.push({ label: `Merged into ${shortRef(m.default_ref)}`, note: 'every commit is an ancestor', status: 'pass' });
      break;
    case 'reachable_from_pushed_ref':
      out.push({ label: 'Merged or pushed', note: `reachable from ${m.refs.map(shortRef).join(', ')}`, status: 'pass' });
      break;
    case 'no_commits':
      out.push({ label: 'Merged into main', note: 'no commits of its own', status: 'pass' });
      break;
    case 'patch_id_only':
      out.push({ label: `Merged into ${shortRef(m.default_ref)}`, note: 'patch matches a squash commit, unproven', status: 'fail' });
      break;
    case 'not_merged':
      out.push({ label: 'Merged into main', note: `not merged into ${m.checked.map(shortRef).join(', ') || 'the default branch'}`, status: 'fail' });
      break;
    case 'unknown':
      out.push({ label: 'Merged into main', note: m.reason, status: 'unknown' });
      break;
  }

  out.push(
    s.lock.state === 'locked'
      ? { label: 'Not locked', note: s.lock.reason ?? 'locked', status: 'fail' }
      : { label: 'Not locked', note: '', status: 'pass' },
  );

  const inUse = d.evidence.find((e) => e.kind === 'in_use') as { signal: string; by?: string | null } | undefined;
  const noSignal = d.evidence.find((e) => e.kind === 'signal_unavailable' && /in.?use/i.test(e.signal)) as { reason: string } | undefined;
  if (inUse) out.push({ label: 'Not in use', note: inUse.by ? `${inUse.signal} (${inUse.by})` : inUse.signal, status: 'fail' });
  else if (noSignal) out.push({ label: 'Not in use', note: noSignal.reason, status: 'unknown' });
  else out.push({ label: 'Not in use', note: 'no process inside', status: 'pass' });
  return out;
}

export type Filter = 'all' | Verdict | 'agent';

export function matches(r: WorktreeReport, f: Filter): boolean {
  return f === 'all' ? true : f === 'agent' ? isAgent(r) : verdictOf(r) === f;
}

export function filterCounts(rs: readonly WorktreeReport[]): Record<Filter, number> {
  const c: Record<Filter, number> = { all: rs.length, removable: 0, review: 0, protected: 0, agent: 0 };
  for (const r of rs) {
    c[verdictOf(r)] += 1;
    if (isAgent(r)) c.agent += 1;
  }
  return c;
}

const RANK: Record<Verdict, number> = { removable: 0, review: 1, protected: 2 };

/** By what can be reclaimed (design 04): removable, then review, then protected; largest first in each; unknown sizes last. */
export function sortBySize(rs: readonly WorktreeReport[]): WorktreeReport[] {
  return [...rs].sort((a, b) => RANK[verdictOf(a)] - RANK[verdictOf(b)] || (b.allocated ?? -1) - (a.allocated ?? -1));
}

export function repoCount(rs: readonly WorktreeReport[]): number {
  return new Set(rs.map((r) => r.common_dir)).size;
}

export const TIER_OF_VERDICT: Record<Verdict, Tier> = { removable: 'S1', review: 'S2', protected: 'S3' };

export { displayPath };
