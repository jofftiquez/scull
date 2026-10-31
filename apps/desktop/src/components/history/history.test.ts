import { describe, expect, it } from 'vitest';
import { createMockBridge } from '../../bridge/mock';
import { designHistory } from '../../bridge/historyMock';
import type { HistoryOperation } from '../../bridge';
import {
  RESTORED_SUFFIX,
  createMockHistoryActions,
  createPendingHistoryActions,
  historyActionsFor,
  restoreTarget,
} from './bridge';
import {
  byLine,
  dayLabel,
  displayPath,
  figures,
  freed,
  isRestorable,
  itemState,
  note,
  subLine,
  tag,
  title,
  withRestores,
} from './model';

// Oct 31 2026, 18:00 local: the design's "today".
const NOW = Math.floor(new Date(2026, 9, 31, 18, 0, 0).getTime() / 1000);
const view = designHistory(NOW);
const op = (id: string) => view.operations.find((o) => o.operation_id === `op-${id}`)!;

describe('history model', () => {
  it('shortens home paths on every OS', () => {
    expect(displayPath('/Users/ada/code/x')).toBe('~/code/x');
    expect(displayPath('/home/ada/.npm')).toBe('~/.npm');
    expect(displayPath('C:\\Users\\ada\\code')).toBe('~\\code');
    expect(displayPath('/opt/data')).toBe('/opt/data');
  });

  it('labels days like the design', () => {
    expect(dayLabel(op('today').started_at, NOW)).toBe('Today');
    expect(dayLabel(op('oct28').started_at, NOW)).toBe('Oct 28');
    expect(dayLabel(NOW - 86400, NOW)).toBe('Yesterday');
  });

  it('titles, sub-lines and tags follow design 09', () => {
    const rows = view.operations.map((o) => [title(o), subLine(o), tag(o, NOW).text]);
    expect(rows).toEqual([
      ['Quick Clean · 4 caches', 'npm, pnpm prune, Bun, Yarn', 'not undoable'],
      ['Cleanup · 4 items', '2 deleted, 2 quarantined for 7 days', 'restorable · 4 days'],
      ['Cleanup · 9 items', 'Build outputs in 5 projects', 'rebuilt by tools'],
      ['Worktrees · 7 removed', 'Merged agent worktrees · branches kept', 'branches kept'],
      ['Cleanup · 14 items', 'Build outputs and old caches', 'purged'],
      ['Cleanup · 5 items', 'Freed less than predicted', 'snapshot held space'],
      ['Agent cleanup · 3 items', 'Prepared by Claude Code · approved in desktop dialog', 'human approved'],
    ]);
    expect(tag(op('oct28'), NOW).tone).toBe('violet');
    expect(tag(op('sep14'), NOW).tone).toBe('accent');
    expect(freed(op('oct28'))).toBe('+11.8 GiB');
  });

  it('explains each operation', () => {
    expect(byLine(op('today'))).toBe('You · from the Overview');
    expect(byLine(op('oct28'))).toBe('You · Cleanup Review');
    expect(byLine(op('sep14'))).toBe('Claude Code prepared · you approved');
    expect(note(op('oct28'), NOW)).toBe('Quarantined items sit on the same disk and can be restored until Nov 4.');
    expect(note(op('oct9'), NOW)).toBe('Quarantine for this cleanup was purged on Oct 16.');
    expect(note(op('today'), NOW)).toMatch(/nothing to restore/);
    expect(note(op('sep28'), NOW)).toMatch(/^A local snapshot held 4\.3 GiB/);
  });

  it('computes the four figures from the operations', () => {
    const f = figures(view);
    expect(f.monthLabel).toBe('October');
    expect(f.operations).toBe(7);
    expect(f.restorable).toBe(2);
    expect(f.accuracy).toBe(91);
    // Oct 31, 28, 20, 15, 9: 14.6 + 11.8 + 9.0 + 3.1 + 21.9 GiB.
    expect(f.freedThisMonth / 1024 ** 3).toBeCloseTo(60.4, 1);
  });

  it('a quarantine that ended is no longer restorable', () => {
    const it = op('oct28').items.find((i) => i.outcome === 'quarantined')!;
    expect(isRestorable(it, NOW)).toBe(true);
    expect(isRestorable(it, it.restorable_until! + 1)).toBe(false);
  });

  it('applies restores: original path or a sibling, never the same path twice', () => {
    const [venv, lib] = op('oct28').items.filter((i) => i.outcome === 'quarantined');
    const restored = withRestores(
      view,
      new Map([
        [venv!.item_id, { restored_to: venv!.path, sibling: false }],
        [lib!.item_id, { restored_to: `${lib!.path}${RESTORED_SUFFIX}1`, sibling: true }],
      ]),
    );
    const o = restored.operations.find((x) => x.operation_id === 'op-oct28')!;
    const states = o.items.map((i) => itemState(o, i).text);
    expect(states).toEqual([
      'deleted',
      'deleted',
      'restored to original path',
      'path taken · restored beside it as Library.scull-restored-1',
    ]);
    expect(tag(o, NOW).text).toBe('restored');
    expect(figures(restored).restorable).toBe(0);
    // The input is not mutated.
    expect(op('oct28').items[2]!.outcome).toBe('quarantined');
  });

  it('describes failures, refusals and skips with their reason', () => {
    const base = op('oct20');
    const it = { ...base.items[0]!, reason: 'in_use' };
    const o: HistoryOperation = { ...base, status: 'completed_with_errors' };
    expect(itemState(o, { ...it, outcome: 'refused' }).text).toBe('refused by a safety check · in use');
    expect(itemState(o, { ...it, outcome: 'failed' }).tone).toBe('attention');
    expect(itemState(o, { ...it, outcome: 'skipped' }).text).toBe('skipped · in use');
    expect(tag(o, NOW)).toEqual({ text: 'completed with errors', tone: 'accent' });
  });
});

describe('restore stub (screen-local, never on ScullBridge)', () => {
  it('restores to the original path when free, else the first free sibling', () => {
    expect(restoreTarget('/a/x', () => false)).toEqual({ restored_to: '/a/x', sibling: false });
    const taken = new Set(['/a/x', '/a/x.scull-restored-1']);
    expect(restoreTarget('/a/x', (p) => taken.has(p))).toEqual({ restored_to: '/a/x.scull-restored-2', sibling: true });
  });

  it('the mock never restores the same path twice and refuses non-quarantined items', async () => {
    const a = createMockHistoryActions({ durationMs: 0, occupied: ['/Users/ada/code/proto/Library'] });
    const o = op('oct28');
    const [venv, lib] = o.items.filter((i) => i.outcome === 'quarantined');
    expect(await a.restoreQuarantined(o, venv!)).toEqual({ restored_to: venv!.path, sibling: false });
    expect(await a.restoreQuarantined(o, lib!)).toEqual({ restored_to: `${lib!.path}.scull-restored-1`, sibling: true });
    await expect(a.restoreQuarantined(o, o.items[0]!)).rejects.toMatchObject({ kind: 'usage' });
    expect(a.calls).toHaveLength(3);
  });

  it('inside Tauri the pending stub refuses and moves nothing', async () => {
    const o = op('oct28');
    await expect(createPendingHistoryActions().restoreQuarantined(o, o.items[2]!)).rejects.toMatchObject({
      kind: 'unsupported_platform',
    });
    expect(historyActionsFor(createMockBridge()).kind).toBe('mock');
    expect(historyActionsFor({ ...createMockBridge(), kind: 'tauri' }).kind).toBe('pending');
  });

  it('ScullBridge stays read-only: no restore on it', () => {
    expect(Object.keys(createMockBridge())).not.toContain('restoreQuarantined');
    expect(Object.keys(createMockBridge()).some((k) => /restore/i.test(k))).toBe(false);
  });
});
