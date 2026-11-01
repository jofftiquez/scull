import { describe, expect, it } from 'vitest';
import type { Action, Candidate, Tier } from '../../bridge';
import type { PlanItem } from '../../bridge/plan';
import { mockCandidate } from '../../bridge/planMock';
import {
  actionAllowed,
  confirmLabel,
  displayPath,
  evidenceSummary,
  flagChips,
  flagList,
  initialSelection,
  isSelectable,
  itemSubline,
  planTotals,
  toggleSelection,
  undoLabel,
} from './plan';

const GiB = 1024 ** 3;
const NOW = 1_800_000_000;
let seq = 0;

function item(tier: Tier, action: Action['type'], gib = 1, extra: Partial<Parameters<typeof mockCandidate>[0]> = {}): PlanItem {
  const id = (++seq).toString(16).padStart(32, '0');
  return {
    candidate: mockCandidate(
      {
        id,
        path: `/Users/ada/code/p${seq}/target`,
        artifact: 'Cargo build output',
        rule: 'rust.target',
        tier,
        action,
        gib,
        reasons: [['regenerable', 'because']],
        regen: ['cargo build', 240],
        ...extra,
      },
      NOW,
    ),
  };
}

describe('D-28 action rules', () => {
  const all: Action['type'][] = ['delete', 'quarantine', 'trash', 'git_worktree_remove', 'git_worktree_prune', 'none'];
  const allowed = (t: Tier) => all.filter((a) => actionAllowed(t, { type: a } as Action));
  it('delete only at S0; Git/tool actions at S0/S1; S2 only quarantine/Trash; S3 nothing', () => {
    expect(allowed('S0')).toEqual(all);
    expect(allowed('S1')).toEqual(['quarantine', 'trash', 'git_worktree_remove', 'git_worktree_prune', 'none']);
    expect(allowed('S2')).toEqual(['quarantine', 'trash', 'none']);
    expect(allowed('S3')).toEqual(['none']);
    expect(actionAllowed('S1', { type: 'tool_command', tool: 'npm', args: [] })).toBe(true);
    expect(actionAllowed('S2', { type: 'tool_command', tool: 'npm', args: [] })).toBe(false);
  });
});

describe('selection rules', () => {
  const s0 = item('S0', 'delete', 15.2);
  const s0b = item('S0', 'delete', 12.4);
  const s1 = item('S1', 'quarantine', 8.2);
  const s2 = item('S2', 'quarantine', 9.8);
  const s3 = item('S3', 'none', 3);
  const items = [s0, s0b, s1, s2, s3];

  it('S3 is never selectable, whatever action it carries', () => {
    expect(isSelectable(s3.candidate)).toBe(false);
    const forged: Candidate = { ...s3.candidate, action: { type: 'quarantine' } };
    expect(isSelectable(forged)).toBe(false);
    expect(toggleSelection(new Set(), s3, true).has(s3.candidate.id)).toBe(false);
  });

  it('an action the tier forbids is not selectable (S2 delete, S1 delete)', () => {
    expect(isSelectable(item('S2', 'delete').candidate)).toBe(false);
    expect(isSelectable(item('S1', 'delete').candidate)).toBe(false);
    expect(isSelectable(item('S2', 'git_worktree_remove').candidate)).toBe(false);
    expect(isSelectable(item('S0', 'none').candidate)).toBe(false);
  });

  it('S0 is preselected; S1 and S2 wait for an explicit tick', () => {
    const sel = initialSelection(items);
    expect([...sel].sort()).toEqual([s0.candidate.id, s0b.candidate.id].sort());
    const withS2 = toggleSelection(sel, s2);
    expect(withS2.has(s2.candidate.id)).toBe(true);
    expect(sel.has(s2.candidate.id)).toBe(false); // returns a new set
    expect(toggleSelection(withS2, s2).has(s2.candidate.id)).toBe(false);
  });

  it('a forbidden S0 item is not preselected', () => {
    const bad = item('S0', 'none');
    expect(initialSelection([bad]).size).toBe(0);
  });

  it('totals split delete vs quarantine and ignore unselectable ids', () => {
    const sel = new Set([s0.candidate.id, s0b.candidate.id, s2.candidate.id, s3.candidate.id]);
    const t = planTotals(items, sel);
    expect(t.count).toBe(3);
    expect(t.deleteCount).toBe(2);
    expect(t.quarantineCount).toBe(1);
    expect(t.deleteBytes).toBe(Math.round(15.2 * GiB) + Math.round(12.4 * GiB));
    expect(t.quarantineBytes).toBe(Math.round(9.8 * GiB));
    expect(t.bytes).toBe(t.deleteBytes + t.quarantineBytes);
  });

  it('confirm label follows the selection', () => {
    expect(confirmLabel(planTotals(items, initialSelection(items)))).toBe('Delete 2 items');
    expect(confirmLabel(planTotals(items, new Set([s0.candidate.id])))).toBe('Delete 1 item');
    expect(confirmLabel(planTotals(items, new Set([s0.candidate.id, s1.candidate.id])))).toBe('Delete 1 · quarantine 1');
    expect(confirmLabel(planTotals(items, new Set([s1.candidate.id, s2.candidate.id])))).toBe('Quarantine 2 items');
    expect(confirmLabel(planTotals(items, new Set([s3.candidate.id])))).toBe('Nothing selected');
  });
});

describe('preview fields', () => {
  it('shortens home paths on every OS', () => {
    expect(displayPath('/Users/ada/code/x')).toBe('~/code/x');
    expect(displayPath('/home/ada/code/x')).toBe('~/code/x');
    expect(displayPath('C:\\Users\\ada\\code')).toBe('~\\code');
    expect(displayPath('/opt/data')).toBe('/opt/data');
  });

  it('summarises evidence like the design', () => {
    const a = item('S0', 'delete', 1, { markers: ['/x/Cargo.toml'], locks: ['/x/Cargo.lock'] });
    expect(evidenceSummary(a.candidate)).toBe('Cargo.toml + Cargo.lock');
    const b = item('S2', 'quarantine', 1, { markers: ['/x/pyproject.toml'], reasons: [['no_lockfile', 'no lock']] });
    expect(evidenceSummary(b.candidate)).toBe('pyproject.toml, no lock');
    const c = item('S0', 'delete', 1, { notes: ['Markers + lockfiles in each'] });
    expect(evidenceSummary(c.candidate)).toBe('Markers + lockfiles in each');
  });

  it('sub-line: project, idle days, regeneration and cost', () => {
    const a = item('S0', 'delete', 1, { project: ['rust-svc', 'rust', '/x'], idleDays: 6 });
    expect(itemSubline(a, NOW)).toBe('rust-svc · idle 6 days · rebuild: cargo build, about 4 min');
    const b = item('S0', 'delete', 1, { project: ['shop-api', 'node', '/x'], regen: ['npm ci', 60, true] });
    expect(itemSubline(b, NOW)).toBe('shop-api · reinstall: npm ci, about 1 min');
  });

  it('undo/restorability per action', () => {
    expect(undoLabel(item('S0', 'delete').candidate)).toBe('Rebuild');
    expect(undoLabel(item('S0', 'delete', 1, { regen: ['npm ci', 60] }).candidate)).toBe('Reinstall');
    expect(undoLabel(item('S1', 'quarantine').candidate)).toBe('7 days');
    expect(undoLabel(item('S2', 'trash').candidate)).toBe('From Trash');
  });

  it('unknown flags read "unknown", never "no"', () => {
    const c = item('S0', 'delete', 1, { inUse: null, hardlinked: true }).candidate;
    const rows = Object.fromEntries(flagList(c.flags).map((f) => [f.label, f.value]));
    expect(rows['In use']).toBe('unknown');
    expect(rows['Hardlinked']).toBe('yes');
    expect(rows['Symlink']).toBe('no');
    expect(flagChips(c.flags)).toEqual(['in use: unknown', 'hardlinked']);
  });
});
