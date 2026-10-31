// Pure rules and labels for showing a cleanup plan (PRD §10.2 preview fields) and selecting
// its items (D-28). No Vue, so they are unit-tested directly.
import type { Action, Candidate, Evidence, Flags, ItemId, Tier } from '../../bridge';
import type { PlanItem } from '../../bridge/plan';
import { formatBytes } from '../../lib/format';

/**
 * D-28 (enforced by `scull_core::Action::allowed_for`, mirrored here so the UI never offers
 * what the core would refuse): delete only at S0; tool and Git actions only at S0/S1; S2 only
 * quarantine or Trash; S3 gets no action.
 */
export function actionAllowed(tier: Tier, action: Action): boolean {
  switch (action.type) {
    case 'none':
      return true;
    case 'delete':
      return tier === 'S0';
    case 'tool_command':
    case 'git_worktree_remove':
    case 'git_worktree_prune':
      return tier === 'S0' || tier === 'S1';
    case 'quarantine':
    case 'trash':
      return tier !== 'S3';
  }
}

/** Whether the user may tick a candidate: never S3, never without an action, never an action its tier forbids. */
export function isSelectable(c: Candidate): boolean {
  return c.tier !== 'S3' && c.action.type !== 'none' && actionAllowed(c.tier, c.action);
}

/** The starting selection: the selectable S0 items (S1/S2 need an explicit tick). */
export function initialSelection(items: readonly PlanItem[]): Set<ItemId> {
  return new Set(items.filter((i) => i.candidate.tier === 'S0' && isSelectable(i.candidate)).map((i) => i.candidate.id));
}

/** Toggle an item, refusing anything that is not selectable. Returns a new set. */
export function toggleSelection(sel: ReadonlySet<ItemId>, item: PlanItem, on?: boolean): Set<ItemId> {
  const next = new Set(sel);
  const id = item.candidate.id;
  const want = on ?? !next.has(id);
  if (want && isSelectable(item.candidate)) next.add(id);
  else next.delete(id);
  return next;
}

/** Whether an action removes data with no undo in scull (rebuild with your tools instead). */
export function isIrreversible(a: Action): boolean {
  return a.type === 'delete' || a.type === 'tool_command' || a.type === 'git_worktree_remove' || a.type === 'git_worktree_prune';
}

export interface PlanTotals {
  count: number;
  /** Predicted reclaimable bytes of the selection. */
  bytes: number;
  deleteCount: number;
  deleteBytes: number;
  /** Quarantine and Trash: restorable, freed when purged. */
  quarantineCount: number;
  quarantineBytes: number;
}

/** Totals over the selected, selectable items. */
export function planTotals(items: readonly PlanItem[], sel: ReadonlySet<ItemId>): PlanTotals {
  const t: PlanTotals = { count: 0, bytes: 0, deleteCount: 0, deleteBytes: 0, quarantineCount: 0, quarantineBytes: 0 };
  for (const it of items) {
    const c = it.candidate;
    if (!sel.has(c.id) || !isSelectable(c)) continue;
    const b = c.sizes.reclaimable.bytes;
    t.count += 1;
    t.bytes += b;
    if (isIrreversible(c.action)) {
      t.deleteCount += 1;
      t.deleteBytes += b;
    } else {
      t.quarantineCount += 1;
      t.quarantineBytes += b;
    }
  }
  return t;
}

/** The confirm button: "Delete 4 items", "Delete 4 · quarantine 1", "Nothing selected". */
export function confirmLabel(t: PlanTotals): string {
  const items = (n: number) => `${n} ${n === 1 ? 'item' : 'items'}`;
  if (t.deleteCount && t.quarantineCount) return `Delete ${t.deleteCount} · quarantine ${t.quarantineCount}`;
  if (t.deleteCount) return `Delete ${items(t.deleteCount)}`;
  if (t.quarantineCount) return `Quarantine ${items(t.quarantineCount)}`;
  return 'Nothing selected';
}

/** `~/code/x` for paths under a home folder (macOS, Linux, Windows). */
export function displayPath(p: string): string {
  return p
    .replace(/^\/(Users|home)\/[^/]+(?=\/|$)/, '~')
    .replace(/^[A-Za-z]:\\Users\\[^\\]+(?=\\|$)/, '~');
}

/** The row title: the group label or the path. */
export function itemTitle(it: PlanItem): string {
  return it.label ?? displayPath(it.candidate.path);
}

const base = (p: string) => p.replace(/[\\/]+$/, '').split(/[\\/]/).pop() + (/[\\/]$/.test(p) ? '/' : '');

/** Evidence column: "Cargo.toml + Cargo.lock", "pyproject.toml, no lock", or the first note. */
export function evidenceSummary(c: Candidate): string {
  const markers = c.evidence.filter((e) => e.kind === 'marker').map((e) => base((e as { path: string }).path));
  const locks = c.evidence.filter((e) => e.kind === 'lockfile').map((e) => base((e as { path: string }).path));
  if (markers.length && !locks.length && c.reasons.some((r) => r.code === 'no_lockfile')) {
    return `${markers.join(' + ')}, no lock`;
  }
  if (markers.length || locks.length) return [...markers, ...locks].join(' + ');
  const note = c.evidence.find((e) => e.kind === 'note') as { text: string } | undefined;
  return note?.text ?? c.artifact;
}

/** Whole days since the newest observed change, or null. */
export function idleDays(c: Candidate, nowSecs = Date.now() / 1000): number | null {
  const lm = c.evidence.find((e) => e.kind === 'last_modified') as { newest_unix: number } | undefined;
  return lm ? Math.max(0, Math.floor((nowSecs - lm.newest_unix) / 86_400)) : null;
}

/** "about 4 min", "about 25 min", "about 1 h", "unknown". */
export function regenCost(c: Candidate, withNetwork = true): string {
  const cost = c.regeneration.cost;
  if (cost.type === 'unknown') return 'cost unknown';
  if (cost.type === 'not_regenerable') return 'not regenerable';
  const s = cost.seconds;
  const t = s < 90 ? `${Math.max(1, Math.round(s / 60))} min` : s < 5400 ? `${Math.round(s / 60)} min` : `${Math.round(s / 3600)} h`;
  return `about ${t}${withNetwork && cost.network ? ' · needs network' : ''}`;
}

/** Dependencies come back by reinstalling (npm ci, pip install, ...), outputs by rebuilding. */
export function isReinstall(c: Candidate): boolean {
  return /reinstall|\b(npm ci|npm install|pnpm install|yarn install|bun install|pip install|uv sync|poetry install)\b/i.test(
    c.regeneration.instructions ?? '',
  );
}

/** The sub-line under the path: project · idle · rebuild hint · note. */
export function itemSubline(it: PlanItem, nowSecs?: number): string {
  const c = it.candidate;
  const parts: string[] = [];
  if (c.project) parts.push(c.project.name);
  const idle = idleDays(c, nowSecs);
  if (idle != null) parts.push(`idle ${idle} ${idle === 1 ? 'day' : 'days'}`);
  if (it.note) parts.push(it.note);
  else if (c.regeneration.cost.type === 'estimate') {
    const verb = isReinstall(c) ? 'reinstall' : 'rebuild';
    parts.push(c.regeneration.instructions ? `${verb}: ${c.regeneration.instructions}, ${regenCost(c, false)}` : `${verb} ${regenCost(c, false)}`);
  } else if (c.regeneration.instructions) {
    parts.push(c.regeneration.instructions);
  }
  return parts.join(' · ');
}

export function actionLabel(a: Action): string {
  switch (a.type) {
    case 'none':
      return 'None';
    case 'delete':
      return 'Delete';
    case 'quarantine':
      return 'Quarantine';
    case 'trash':
      return 'Move to Trash';
    case 'tool_command':
      return `Run ${a.tool}`;
    case 'git_worktree_remove':
      return 'Remove worktree';
    case 'git_worktree_prune':
      return 'Prune registration';
  }
}

/** Undo column: how to get the item back. */
export function undoLabel(c: Candidate): string {
  switch (c.action.type) {
    case 'quarantine':
      return '7 days';
    case 'trash':
      return 'From Trash';
    case 'git_worktree_remove':
      return 'Branch kept';
    case 'none':
      return '—';
    default:
      if (c.regeneration.cost.type === 'not_regenerable') return 'None';
      return isReinstall(c) ? 'Reinstall' : 'Rebuild';
  }
}

/** Long restorability sentence for the details panel. */
export function restorability(c: Candidate): string {
  switch (c.action.type) {
    case 'quarantine':
      return 'Restorable for 7 days from History; space is freed when the quarantine is purged.';
    case 'trash':
      return 'Restorable from the system Trash until it is emptied.';
    case 'git_worktree_remove':
      return 'Not restorable by scull; the branch and its commits are kept.';
    case 'none':
      return 'No action: scull will not touch it.';
    default:
      return 'Not restorable: rebuild it with your tools.';
  }
}

export const TIER_LABEL: Record<Tier, string> = {
  S0: 'S0 Regenerable',
  S1: 'S1 Costly',
  S2: 'S2 Uncertain',
  S3: 'S3 Protected',
};

/** The reason that set the tier (the first of the most protective ones). */
export function tierReason(c: Candidate): string {
  const r = c.reasons.find((x) => x.tier === c.tier) ?? c.reasons[0];
  return r?.message ?? 'No reason recorded';
}

const yn = (v: boolean | null | undefined) => (v == null ? 'unknown' : v ? 'yes' : 'no');

/** Every SAFE-01 risk flag; `null` reads "unknown", never "no". */
export function flagList(f: Flags): { label: string; value: string; warn: boolean }[] {
  const rows: [string, boolean | null | undefined][] = [
    ['In use', f.active],
    ['Hardlinked', f.hardlinked],
    ['Crosses a mount', f.cross_mount],
    ['Symlink', f.symlink],
    ['Cloud placeholder', f.cloud_placeholder],
  ];
  return rows.map(([label, v]) => ({ label, value: yn(v), warn: v !== false }));
}

/** True flags (and unknown in-use) as short chips for the row. */
export function flagChips(f: Flags): string[] {
  const out: string[] = [];
  if (f.active) out.push('in use');
  else if (f.active == null) out.push('in use: unknown');
  if (f.hardlinked) out.push('hardlinked');
  if (f.cross_mount) out.push('crosses mount');
  if (f.symlink) out.push('symlink');
  if (f.cloud_placeholder) out.push('cloud placeholder');
  return out;
}

/** One evidence fact as a sentence. */
export function describeEvidence(e: Evidence, nowSecs = Date.now() / 1000): string {
  switch (e.kind) {
    case 'marker':
      return `Marker ${displayPath(e.path)}`;
    case 'lockfile':
      return `Lockfile ${displayPath(e.path)}`;
    case 'known_artifact':
      return `Known artifact: ${e.artifact}`;
    case 'git_tracked':
      return `${e.files} file${e.files === 1 ? '' : 's'} tracked by Git`;
    case 'protected_root':
      return `Inside protected root ${displayPath(e.root)}`;
    case 'user_exclusion':
      return `Excluded by your rule ${e.rule}`;
    case 'protect_marker':
      return `Protect marker ${displayPath(e.path)}`;
    case 'in_use':
      return `In use (${e.signal}${e.by ? `, ${e.by}` : ''})`;
    case 'last_modified': {
      const d = Math.max(0, Math.floor((nowSecs - e.newest_unix) / 86_400));
      return `Last changed ${d} day${d === 1 ? '' : 's'} ago${e.sampled ? ' (sampled)' : ''}`;
    }
    case 'crosses_mount':
      return 'Crosses a mount point';
    case 'link_escape':
      return `Link points outside: ${displayPath(e.target)}`;
    case 'cloud_sync':
      return `Synced by ${e.provider}`;
    case 'hardlinked':
      return `${formatBytes(e.shared_bytes)} shared with other links`;
    case 'signal_unavailable':
      return `${e.signal} unavailable: ${e.reason}`;
    case 'note':
      return e.text;
  }
}

/** `7f3a·c21e`: the first 8 hex digits of a plan ID or hash, as the design shows them. */
export function shortId(id: string): string {
  return `${id.slice(0, 4)}·${id.slice(4, 8)}`;
}

/** 24-hour wall clock, `10:42:06`. */
export function clock(t: string | number): string {
  const d = new Date(t);
  const p = (n: number) => String(n).padStart(2, '0');
  return `${d.getHours()}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/** "2 min", "24 h", "3 days" for a positive duration in ms. */
export function duration(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s} s`;
  if (s < 3600) return `${Math.round(s / 60)} min`;
  if (s < 2 * 86_400) return `${Math.round(s / 3600)} h`;
  return `${Math.round(s / 86_400)} days`;
}

/** `13:40` for a countdown in seconds. */
export function mmss(secs: number): string {
  return `${Math.floor(secs / 60)}:${String(secs % 60).padStart(2, '0')}`;
}
