// Sentences of the Overview's right sidebar, built from the scan (never from fixed copy).
import type { Candidate } from '../../bridge';
import { formatBytes } from '../../lib/format';
import { categoryLabel, type WheelCategory, type WheelItem } from './wheel';
import { TIERS } from './tiers';

const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;

/** "3 projects hold 31.9 GiB of build output; 1 worktree holds 6.7 GiB of artifacts. All of it rebuilds itself." */
export function reclaimSummary(cands: readonly Candidate[]): string {
  const safe = cands.filter((c) => c.tier === 'S0' && c.sizes.reclaimable.bytes > 0);
  if (safe.length === 0) {
    return 'Nothing is regenerable right now. Costly and uncertain items wait in Cleanup Review.';
  }
  const sum = (xs: Candidate[]) => xs.reduce((a, c) => a + c.sizes.reclaimable.bytes, 0);
  const parts: string[] = [];
  const builds = safe.filter((c) => c.category === 'project_artifact');
  if (builds.length) {
    const projects = new Set(builds.map((c) => c.project?.root ?? c.path)).size;
    parts.push(`${plural(projects, 'project holds', 'projects hold')} ${formatBytes(sum(builds))} of build output`);
  }
  const wts = safe.filter((c) => c.category === 'worktree');
  if (wts.length) parts.push(`${plural(wts.length, 'worktree holds', 'worktrees hold')} ${formatBytes(sum(wts))} of artifacts`);
  const rest = safe.filter((c) => c.category !== 'project_artifact' && c.category !== 'worktree');
  if (rest.length) parts.push(`${plural(rest.length, 'other item holds', 'other items hold')} ${formatBytes(sum(rest))}`);
  const s = parts.join('; ');
  return `${s[0]!.toUpperCase()}${s.slice(1)}. All of it rebuilds itself.`;
}

/** Why an item has its tier: the reasons at that tier, else the regeneration note. */
export function itemReason(item: WheelItem): string {
  const c = item.candidate;
  if (!c) {
    return `${plural(item.count, 'smaller item', 'smaller items')} in ${categoryLabel(item.category)}, all ${TIERS[item.tier].label.toLowerCase()}. Select the category for its total.`;
  }
  const at = c.reasons.filter((r) => r.tier === c.tier).map((r) => r.message);
  const text = at.length ? at.join(' ') : c.regeneration.instructions;
  return text ? `${text[0]!.toUpperCase()}${text.slice(1)}${/[.!?]$/.test(text) ? '' : '.'}` : '';
}

export function categoryReason(c: WheelCategory): string {
  const tiers = new Set(c.items.map((i) => i.tier));
  const mixed = tiers.size > 1 ? ' It mixes tiers, so it carries the most protective one.' : '';
  return `${plural(c.count, 'item', 'items')}, ${formatBytes(c.reclaimable)} reclaimable.${mixed}`;
}

/** "38.6 GiB" -> ["38.6", "GiB"]. */
export function splitBytes(bytes: number): [string, string] {
  const [v = '', u = ''] = formatBytes(bytes).split(' ');
  return [v, u];
}
