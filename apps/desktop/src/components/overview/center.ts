// What the middle of the wheel says (design 01, 01b, 01c).
import { formatBytes } from '../../lib/format';
import { TIERS } from './tiers';
import type { WheelModel } from './wheel';

export interface WheelCenter {
  /** Small line above the figure ("Development", "Costly", "Scanning…"). */
  k: string;
  kColor: string;
  /** The figure ("186.0 GiB", "276,630"). */
  v: string;
  /** Line under the figure (name or path). */
  n: string;
}

const ACCENT = '#B81E74';

/** Shorten a path for the center line: home becomes "~", long paths keep their tail. */
export function shortPath(p: string, home: string | null = null, max = 34): string {
  let s = p;
  if (home && (s === home || s.startsWith(`${home}/`) || s.startsWith(`${home}\\`))) s = `~${s.slice(home.length)}`;
  else s = s.replace(/^\/(Users|home)\/[^/]+/, '~');
  return s.length > max ? `…${s.slice(s.length - max + 1)}` : s;
}

export type CenterInput =
  | { kind: 'scanning'; entries: number; where: string }
  | { kind: 'freed'; bytes: number; what: string }
  | { kind: 'idle'; model: WheelModel | null; selected: string | null };

export function wheelCenter(i: CenterInput): WheelCenter {
  if (i.kind === 'scanning') {
    return { k: 'Scanning…', kColor: ACCENT, v: i.entries.toLocaleString('en-US'), n: shortPath(i.where) };
  }
  if (i.kind === 'freed') return { k: 'Just freed', kColor: ACCENT, v: `+${formatBytes(i.bytes)}`, n: i.what };
  const m = i.model;
  if (!m) return { k: 'Not scanned', kColor: ACCENT, v: '—', n: 'Rescan to fill the wheel' };
  if (i.selected) {
    for (const c of m.categories) {
      if (c.key === i.selected) {
        const t = TIERS[c.tier];
        return { k: t.label, kColor: t.text, v: formatBytes(c.bytes), n: `${c.label} · ${c.count} item${c.count === 1 ? '' : 's'}` };
      }
      const it = c.items.find((x) => x.id === i.selected);
      if (it) {
        const t = TIERS[it.tier];
        return { k: t.label, kColor: t.text, v: formatBytes(it.bytes), n: it.name };
      }
    }
  }
  if (m.total === 0) return { k: 'Nothing found', kColor: ACCENT, v: formatBytes(0), n: 'no cleanup candidates' };
  const nc = m.categories.length;
  return {
    k: 'Development',
    kColor: ACCENT,
    v: formatBytes(m.total),
    n: `${nc} categor${nc === 1 ? 'y' : 'ies'} · ${m.itemCount} item${m.itemCount === 1 ? '' : 's'}`,
  };
}
