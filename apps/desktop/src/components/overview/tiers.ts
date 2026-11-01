// Safety tiers as the Overview draws them (D-16, design/html/Main.html). A tier is never
// encoded by color alone: every place that shows one also carries its label, and areas use
// the stripe (uncertain) and hatch (protected) patterns.
import type { Tier } from '../../bridge';

export interface TierLook {
  key: Tier;
  /** Toolbar and center label ("Regenerable"). */
  label: string;
  /** Lower-case chip label ("regenerable"), as in the "By safety tier" list. */
  chip: string;
  hint: string;
  /** Solid swatch (toolbar dots, legend). */
  swatch: string;
  /** Text color for the tier name on the paper ground (WCAG AA at 13px semibold). */
  text: string;
  /** Badge colors (selected item card). */
  badgeBg: string;
  badgeFg: string;
  badgeBorder: string;
}

// Center-label colors: the design's #7C4DFF (4.4:1) and #A86500 (4.3:1) fall just short of
// AA for 13px text on the paper ground, so they use violet-text (#6B3FD9) and a darker amber
// (#9A5C00, 4.9:1). The other two are the design's values.
export const TIERS: Record<Tier, TierLook> = {
  S0: {
    key: 'S0',
    label: 'Regenerable',
    chip: 'regenerable',
    hint: 'Rebuilds itself',
    swatch: '#7C4DFF',
    text: '#6B3FD9',
    badgeBg: '#7C4DFF',
    badgeFg: '#FFFFFF',
    badgeBorder: '#7C4DFF',
  },
  S1: {
    key: 'S1',
    label: 'Costly',
    chip: 'costly',
    hint: 'Re-download or long build',
    swatch: '#DCCBFF',
    text: '#6B4FB3',
    badgeBg: '#DCCBFF',
    badgeFg: '#15121A',
    badgeBorder: '#DCCBFF',
  },
  S2: {
    key: 'S2',
    label: 'Uncertain',
    chip: 'uncertain',
    hint: 'Look before it goes',
    swatch: '#FFB547',
    text: '#9A5C00',
    badgeBg: 'repeating-linear-gradient(135deg, #FFB547 0 4px, #FFD08A 4px 8px)',
    badgeFg: '#15121A',
    badgeBorder: '#FFB547',
  },
  S3: {
    key: 'S3',
    label: 'Protected',
    chip: 'protected',
    hint: 'Never touched',
    swatch: '#5A4772',
    text: '#3B2A4F',
    badgeBg: '#3B2A4F',
    badgeFg: '#FFFFFF',
    badgeBorder: '#3B2A4F',
  },
};

export const TIER_KEYS: readonly Tier[] = ['S0', 'S1', 'S2', 'S3'];

/** Ordering by protection: S0 (regenerable) < S1 < S2 < S3 (protected). */
export function tierRank(t: Tier): number {
  return TIER_KEYS.indexOf(t);
}

/**
 * The most protective tier of a group (a parent's badge). A group holding any uncertain or
 * protected bytes is never shown in a "safe" tier. `null` for an empty group.
 */
export function mostProtective(tiers: Iterable<Tier>): Tier | null {
  let best: Tier | null = null;
  for (const t of tiers) if (best === null || tierRank(t) > tierRank(best)) best = t;
  return best;
}

/** Toolbar filter: one tier, or everything. */
export type TierFilter = 'all' | Tier;
