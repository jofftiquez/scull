// What the History screen shows (design 09), derived from the `history` bridge payload. Pure
// functions, so the wording and the figures are tested without rendering.
import type { AuditLineKind, HistoryItem, HistoryOperation, HistoryView, OperationKind } from '../../bridge';
import { formatBytes } from '../../lib/format';
import type { RestoreResult } from './bridge';

const DAY = 86400;
const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
const MONTHS_LONG = ['January', 'February', 'March', 'April', 'May', 'June', 'July', 'August', 'September', 'October', 'November', 'December'];

/** `/Users/ada/code/x` → `~/code/x` (macOS, Linux and Windows home folders). */
export function displayPath(p: string): string {
  return p.replace(/^\/(?:Users|home)\/[^/]+(?=\/|$)/, '~').replace(/^[A-Za-z]:\\Users\\[^\\]+(?=\\|$)/, '~');
}

const date = (t: number) => new Date(t * 1000);
const sameDay = (a: Date, b: Date) =>
  a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate();
const pad = (n: number) => String(n).padStart(2, '0');

/** `Today`, `Yesterday`, `Oct 28`. */
export function dayLabel(t: number, now: number): string {
  const d = date(t);
  const n = date(now);
  if (sameDay(d, n)) return 'Today';
  const y = date(now - DAY);
  if (sameDay(d, y)) return 'Yesterday';
  return `${MONTHS[d.getMonth()]} ${d.getDate()}`;
}

/** `Nov 4`. */
export function shortDate(t: number): string {
  const d = date(t);
  return `${MONTHS[d.getMonth()]} ${d.getDate()}`;
}

/** `16:12`. */
export function clock(t: number): string {
  const d = date(t);
  return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** `16:12:03`. */
export function clockSeconds(t: number): string {
  const d = date(t);
  return `${clock(t)}:${pad(d.getSeconds())}`;
}

/** The view with local restores applied: a restored item reads `restored`, with its target. */
export function withRestores(view: HistoryView, restored: ReadonlyMap<string, RestoreResult>): HistoryView {
  if (restored.size === 0) return view;
  return {
    ...view,
    operations: view.operations.map((op) => ({
      ...op,
      items: op.items.map((it) => {
        const r = restored.get(it.item_id);
        return r ? { ...it, outcome: 'restored' as const, restored_to: r.restored_to } : it;
      }),
    })),
  };
}

/** A quarantined item whose retention hasn't ended. */
export function isRestorable(it: HistoryItem, now: number): boolean {
  return it.outcome === 'quarantined' && (it.restorable_until == null || it.restorable_until > now);
}

const count = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

export const ICONS: Record<OperationKind, { text: string; bg: string; fg: string; label: string }> = {
  quick_clean: { text: 'QC', bg: 'var(--color-violet-tint)', fg: 'var(--color-violet-text)', label: 'Quick Clean' },
  cleanup: { text: 'CL', bg: 'var(--color-ink)', fg: 'var(--color-on-ink)', label: 'Cleanup' },
  worktrees: { text: 'WT', bg: 'var(--color-category-ring)', fg: 'var(--color-tier-s3-fg)', label: 'Worktrees' },
  agent: { text: 'AI', bg: 'var(--color-accent-tint)', fg: 'var(--color-accent-text)', label: 'Agent cleanup' },
};

/** `Quick Clean · 4 caches`, `Cleanup · 6 items`, `Worktrees · 7 removed`. */
export function title(op: HistoryOperation): string {
  const n = op.items.length;
  switch (op.kind) {
    case 'quick_clean':
      return `Quick Clean · ${count(n, 'cache')}`;
    case 'worktrees':
      return `Worktrees · ${n} removed`;
    case 'agent':
      return `Agent cleanup · ${count(n, 'item')}`;
    default:
      return `Cleanup · ${count(n, 'item')}`;
  }
}

/** The timeline's second line. */
export function subLine(op: HistoryOperation): string {
  if (op.summary) return op.summary;
  if (op.kind === 'quick_clean') return op.items.map((i) => i.label ?? displayPath(i.path)).join(', ');
  const deleted = op.items.filter((i) => i.outcome === 'removed').length;
  const quarantined = op.items.filter((i) => ['quarantined', 'restored', 'purged'].includes(i.outcome)).length;
  if (quarantined) {
    const until = op.items.find((i) => i.restorable_until != null)?.restorable_until;
    const days = until ? Math.round((until - op.started_at) / DAY) : null;
    return `${deleted} deleted, ${quarantined} quarantined${days ? ` for ${days} days` : ''}`;
  }
  return `${count(deleted, 'item')} deleted`;
}

export type TagTone = 'violet' | 'accent' | 'neutral';

/** The small pill under the freed size. */
export function tag(op: HistoryOperation, now: number): { text: string; tone: TagTone } {
  const restorable = op.items.filter((i) => isRestorable(i, now));
  if (restorable.length) {
    const until = Math.min(...restorable.map((i) => i.restorable_until ?? Infinity));
    const days = Number.isFinite(until) ? Math.max(1, Math.ceil((until - now) / DAY)) : null;
    return { text: days ? `restorable · ${count(days, 'day')}` : 'restorable', tone: 'violet' };
  }
  if (op.items.some((i) => i.outcome === 'restored') && !op.items.some((i) => i.outcome === 'quarantined')) {
    return { text: 'restored', tone: 'neutral' };
  }
  if (op.status === 'running') return { text: 'running', tone: 'neutral' };
  if (op.status !== 'completed') return { text: op.status.replace(/_/g, ' '), tone: 'accent' };
  if (op.kind === 'agent') return { text: 'human approved', tone: 'accent' };
  if (op.kind === 'quick_clean') return { text: 'not undoable', tone: 'neutral' };
  if (op.kind === 'worktrees') return { text: 'branches kept', tone: 'neutral' };
  if (op.items.some((i) => i.outcome === 'purged')) return { text: 'purged', tone: 'neutral' };
  if (op.delta_explanations.includes('local_snapshot')) return { text: 'snapshot held space', tone: 'neutral' };
  if (op.items.every((i) => i.outcome === 'removed' && i.tier === 'S0')) return { text: 'rebuilt by tools', tone: 'neutral' };
  return { text: 'deleted', tone: 'neutral' };
}

/** `You · Cleanup Review`. */
export function byLine(op: HistoryOperation): string {
  if (op.agent) return `${op.agent} prepared · you approved`;
  const who = op.approver === 'you' ? 'You' : op.approver;
  if (!op.origin) return who;
  return op.kind === 'quick_clean' ? `${who} · from the ${op.origin}` : `${who} · ${op.origin}`;
}

/** The detail panel's explanation. */
export function note(op: HistoryOperation, now: number): string {
  const restorable = op.items.filter((i) => isRestorable(i, now));
  if (restorable.length) {
    const until = Math.min(...restorable.map((i) => i.restorable_until ?? Infinity));
    return `Quarantined items sit on the same disk and can be restored${Number.isFinite(until) ? ` until ${shortDate(until)}` : ''}.`;
  }
  if (op.items.some((i) => i.outcome === 'restored') && !op.items.some((i) => i.outcome === 'quarantined')) {
    return 'Every quarantined item was restored. Nothing was overwritten: a taken path restores beside the original.';
  }
  if (op.kind === 'quick_clean') return 'Caches re-download on the next install, so there is nothing to restore.';
  if (op.kind === 'worktrees') {
    return 'Removed with git worktree remove. Every branch still exists; re-add any of them with git worktree add.';
  }
  if (op.kind === 'agent') {
    return 'The agent prepared the plan through MCP. Nothing ran until you approved it outside the chat.';
  }
  const purged = op.items.filter((i) => i.outcome === 'purged' && i.restorable_until != null);
  if (purged.length) return `Quarantine for this cleanup was purged on ${shortDate(Math.max(...purged.map((i) => i.restorable_until!)))}.`;
  if (op.delta_explanations.includes('local_snapshot')) {
    const held = op.predicted_bytes - (op.measured_delta_bytes ?? 0);
    return `A local snapshot held ${formatBytes(Math.max(0, held))} until it expired. scull now warns about snapshots before cleaning.`;
  }
  if (op.items.every((i) => i.outcome === 'removed' && i.tier === 'S0')) {
    return 'All regenerable; rebuilt automatically when you build.';
  }
  return '';
}

/** `+11.8 GiB` (measured), or the prediction when nothing was measured. */
export function freed(op: HistoryOperation): string {
  const m = op.measured_delta_bytes;
  if (m == null) return `~${formatBytes(op.predicted_bytes)}`;
  return `${m < 0 ? '−' : '+'}${formatBytes(Math.abs(m))}`;
}

export type ItemTone = 'restored' | 'attention' | 'muted';

/** An item's state line and tone. */
export function itemState(op: HistoryOperation, it: HistoryItem): { text: string; tone: ItemTone } {
  const why = it.reason ? ` · ${it.reason.replace(/_/g, ' ')}` : '';
  switch (it.outcome) {
    case 'removed':
      return { text: op.kind === 'worktrees' ? 'removed · branch kept' : 'deleted', tone: 'muted' };
    case 'quarantined':
      return { text: 'quarantined', tone: 'attention' };
    case 'restored':
      if (!it.restored_to || it.restored_to === it.path) return { text: 'restored to original path', tone: 'restored' };
      return { text: `path taken · restored beside it as ${it.restored_to.split(/[\\/]/).pop()}`, tone: 'restored' };
    case 'tool_run':
      return { text: 'cleaned', tone: 'muted' };
    case 'moved_to_trash':
      return { text: 'moved to the Trash', tone: 'muted' };
    case 'purged':
      return { text: 'purged', tone: 'muted' };
    case 'partial':
      return { text: `partly deleted${why}`, tone: 'attention' };
    case 'failed':
      return { text: `failed${why}`, tone: 'attention' };
    case 'refused':
      return { text: `refused by a safety check${why}`, tone: 'attention' };
    default:
      return { text: `skipped${why}`, tone: 'muted' };
  }
}

export const LOG_COLORS: Record<AuditLineKind, string> = {
  approval: 'var(--color-on-ink-soft)',
  check: 'var(--color-on-ink-subtle)',
  action: 'var(--color-on-ink-violet)',
  measured: 'var(--color-on-ink-magenta)',
  warning: 'var(--color-on-ink-amber)',
};

export interface Figures {
  monthLabel: string;
  freedThisMonth: number;
  operations: number;
  /** Measured / predicted, in percent (`null` without measurements). */
  accuracy: number | null;
  restorable: number;
}

/** The four tiles. */
export function figures(view: HistoryView): Figures {
  const n = date(view.now);
  const inMonth = (t: number) => {
    const d = date(t);
    return d.getFullYear() === n.getFullYear() && d.getMonth() === n.getMonth();
  };
  let freedThisMonth = 0;
  let measured = 0;
  let predicted = 0;
  let restorable = 0;
  for (const op of view.operations) {
    const m = op.measured_delta_bytes;
    if (m != null) {
      if (inMonth(op.started_at)) freedThisMonth += Math.max(0, m);
      measured += m;
      predicted += op.predicted_bytes;
    }
    restorable += op.items.filter((i) => isRestorable(i, view.now)).length;
  }
  return {
    monthLabel: MONTHS_LONG[n.getMonth()]!,
    freedThisMonth,
    operations: view.operations.length,
    accuracy: predicted > 0 ? Math.round((Math.max(0, measured) / predicted) * 100) : null,
    restorable,
  };
}
