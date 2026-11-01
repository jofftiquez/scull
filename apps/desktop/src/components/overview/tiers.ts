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

// Colors are the theme's CSS variables (design/tokens.json, light and dark; P5-UI-01), so
// every use follows the theme. Center labels use the tier `fg` colors: the design's #7C4DFF
// (4.4:1) and #A86500 (4.3:1) fall just short of AA for 13px text on the paper ground, so the
// light theme uses violet-text (#6B3FD9) and a darker amber (#9A5C00, 4.9:1).
export const TIERS: Record<Tier, TierLook> = {
  S0: {
    key: 'S0',
    label: 'Regenerable',
    chip: 'regenerable',
    hint: 'Rebuilds itself',
    swatch: 'var(--color-tier-s0-swatch)',
    text: 'var(--color-tier-s0-fg)',
    badgeBg: 'var(--tier-s0-fill)',
    badgeFg: 'var(--color-tier-s0-text)',
    badgeBorder: 'var(--color-tier-s0)',
  },
  S1: {
    key: 'S1',
    label: 'Costly',
    chip: 'costly',
    hint: 'Re-download or long build',
    swatch: 'var(--color-tier-s1-swatch)',
    text: 'var(--color-tier-s1-fg)',
    badgeBg: 'var(--tier-s1-fill)',
    badgeFg: 'var(--color-tier-s1-text)',
    badgeBorder: 'var(--color-tier-s1)',
  },
  S2: {
    key: 'S2',
    label: 'Uncertain',
    chip: 'uncertain',
    hint: 'Look before it goes',
    swatch: 'var(--color-tier-s2-swatch)',
    text: 'var(--color-tier-s2-fg)',
    badgeBg: 'var(--tier-s2-fill)',
    badgeFg: 'var(--color-tier-s2-text)',
    badgeBorder: 'var(--color-tier-s2)',
  },
  S3: {
    key: 'S3',
    label: 'Protected',
    chip: 'protected',
    hint: 'Never touched',
    swatch: 'var(--color-tier-s3-swatch)',
    text: 'var(--color-tier-s3-fg)',
    badgeBg: 'var(--color-tier-s3)',
    badgeFg: 'var(--color-tier-s3-text)',
    badgeBorder: 'var(--color-tier-s3)',
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
