import { describe, expect, it } from 'vitest';
import type { WorktreeReport } from '../../bridge';
import { MOCK_DATA } from '../../bridge/mock';
import { DESIGN_WORKTREES } from '../../bridge/worktreesMock';
import { branchOf, canRemove, checksOf, filterCounts, isAgent, matches, metaLine, removalBlock, repoOf, sortBySize, verdictLine, verdictOf } from './worktrees';

const byBranch = (b: string) => DESIGN_WORKTREES.find((w) => branchOf(w) === b)!;
const recorded = MOCK_DATA.worktrees.worktrees.filter((w) => !DESIGN_WORKTREES.includes(w));
const status = (w: WorktreeReport) => Object.fromEntries(checksOf(w).map((c) => [c.label.replace(/ into .*/, ' into'), c.status]));

describe('worktree verdicts', () => {
  it('explains the actual removal block and the next step without weakening the gate', () => {
    const removable = byBranch('agent/refactor-cart-12');
    expect(removalBlock(removable)).toBeNull();
    expect(removalBlock({ ...removable, kind: 'main' })?.title).toBe('Main working tree');
    expect(removalBlock({ ...removable, inside_roots: false })?.detail).toContain('scan folders');
    expect(removalBlock(byBranch('fix/payments-hotfix'))?.detail).toContain('push');
    expect(removalBlock(byBranch('agent/perf-probe-22'))?.detail).toContain('locked');
    expect(canRemove(byBranch('fix/payments-hotfix'))).toBe(false);
  });
  it('follow the core tier; removal only where D-28 allows a Git action', () => {
    expect(verdictOf(byBranch('agent/refactor-cart-12'))).toBe('removable');
    expect(verdictLine(byBranch('agent/refactor-cart-12'))).toBe('Removable · S1');
    expect(verdictOf(byBranch('feat/search-v2'))).toBe('review');
    expect(canRemove(byBranch('feat/search-v2'))).toBe(false);
    expect(verdictOf(byBranch('fix/payments-hotfix'))).toBe('protected');
    expect(canRemove(byBranch('agent/perf-probe-22'))).toBe(false);
  });

  it('never offers removal of the main working tree, an S2 worktree, or one outside the roots', () => {
    const main = recorded.find((w) => w.kind === 'main')!;
    expect(verdictOf(main)).toBe('protected');
    expect(canRemove(main)).toBe(false);
    const linked = recorded.find((w) => w.kind === 'linked')!;
    expect(linked.decision.tier).toBe('S2');
    expect(canRemove(linked)).toBe(false);
    const forged: WorktreeReport = { ...linked, decision: { ...linked.decision, action: { type: 'git_worktree_remove' } } };
    expect(canRemove(forged)).toBe(false); // S2 + git action is refused (D-28)
    const outside: WorktreeReport = { ...byBranch('agent/refactor-cart-12'), inside_roots: false };
    expect(canRemove(outside)).toBe(false);
    expect(verdictOf(outside)).toBe('protected');
  });
});

describe('checks', () => {
  it('a removable worktree passes all seven', () => {
    const c = checksOf(byBranch('agent/refactor-cart-12'));
    expect(c).toHaveLength(7);
    expect(c.every((x) => x.status === 'pass')).toBe(true);
    expect(c.find((x) => x.label === 'Ignored files are artifacts only')?.note).toBe('node_modules');
  });

  it('report the reason for review or protection', () => {
    expect(status(byBranch('feat/search-v2'))['Merged into']).toBe('fail');
    expect(checksOf(byBranch('feat/search-v2')).find((x) => x.label.startsWith('Merged'))?.note).toContain('squash');
    const db = checksOf(byBranch('agent/migrate-db-4')).find((x) => x.label === 'Ignored files are artifacts only')!;
    expect(db).toMatchObject({ status: 'fail', note: '.env and dev.sqlite are unique' });
    const hot = checksOf(byBranch('fix/payments-hotfix'));
    expect(hot.find((x) => x.label === 'All commits on a pushed ref')).toMatchObject({ status: 'fail', note: '3 unpushed commits' });
    expect(hot.find((x) => x.label === 'Not in use')?.status).toBe('fail');
    expect(checksOf(byBranch('agent/perf-probe-22')).find((x) => x.label === 'Not locked')).toMatchObject({ status: 'fail', note: 'benchmark running' });
  });

  it('a signal the core could not observe reads "unknown", never "pass"', () => {
    const linked = recorded.find((w) => w.kind === 'linked')!;
    expect(checksOf(linked).find((x) => x.label === 'Not in use')?.status).toBe('unknown');
  });
});

describe('list', () => {
  const all = MOCK_DATA.worktrees.worktrees;
  it('filters and counts', () => {
    const c = filterCounts(all);
    expect(c.all).toBe(all.length);
    expect(c.removable + c.review + c.protected).toBe(all.length);
    expect(c.removable).toBe(2);
    expect(c.agent).toBe(all.filter(isAgent).length);
    expect(all.filter((w) => matches(w, 'removable')).every(canRemove)).toBe(true);
  });

  it('sorts removable first, then review, then protected, largest first', () => {
    const order = sortBySize(all).map(verdictOf);
    expect(order).toEqual([...order].sort((a, b) => ['removable', 'review', 'protected'].indexOf(a) - ['removable', 'review', 'protected'].indexOf(b)));
    expect(branchOf(sortBySize(all)[0]!)).toBe('agent/fix-flaky-tests-7');
  });

  it('names the repo and the activity', () => {
    const w = byBranch('agent/perf-probe-22');
    expect(repoOf(w)).toBe('rust-svc');
    expect(metaLine(w)).toBe('rust-svc · agent branch · locked');
    expect(metaLine(byBranch('feat/search-v2'))).toMatch(/^web-app · idle 33 days$/);
  });
});
