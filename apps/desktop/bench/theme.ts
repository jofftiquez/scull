// Chart colors from design/tokens.json, the same source as the app's Tailwind theme. The D3
// pages read them as CSS custom properties (so a theme switch is a stylesheet change); ECharts
// draws to canvas/SVG attributes and needs resolved values passed through setOption.
import tokens from '../../../design/tokens.json';
import type { Ecosystem, Tier } from './data';

export const C = {
  ground: tokens.color.ground,
  surface: tokens.color.surface,
  ink: tokens.color.ink,
  line: tokens.color.line,
  text2: tokens.color.textSecondary,
  hint: tokens.color.textHint,
  accent: tokens.color.accent,
  violet: tokens.tier.S0_regenerable.fill,
  lilac: tokens.tier.S1_costly.fill,
  amber: tokens.tier.S2_uncertain.solid,
  amberLight: '#FFD08A',
  plum: tokens.tier.S3_protected.solid,
  plumLight: '#5A4772',
  categoryRing: tokens.color.categoryRing,
  fontSans: tokens.font.sans,
  fontMono: tokens.font.mono,
};

/** Solid tier colors (the pattern's base for S2/S3). */
export const TIER_SOLID: Record<Tier, string> = { S0: C.violet, S1: C.lilac, S2: C.amber, S3: C.plum };
/** Second color of the S2 stripes / S3 hatch. */
export const TIER_PATTERN: Record<Tier, string | null> = { S0: null, S1: null, S2: C.amberLight, S3: C.plumLight };
export const TIER_LABEL: Record<Tier, string> = {
  S0: tokens.tier.S0_regenerable.label,
  S1: tokens.tier.S1_costly.label,
  S2: tokens.tier.S2_uncertain.label,
  S3: tokens.tier.S3_protected.label,
};

/** Ecosystem series: ML uses the amber stripes (design 05), the rest solid. */
export const ECO_COLOR: Record<Ecosystem, string> = { ml: C.amber, js: C.ink, rust: C.violet, other: C.plumLight };

/** CSS custom properties the D3 renderer references (`var(--c-ink)`). */
export function cssVars(): string {
  return Object.entries(C)
    .filter(([k]) => !k.startsWith('font'))
    .map(([k, v]) => `--c-${k}: ${v};`)
    .join(' ');
}

/** The alternate palette used by the re-theme measurement (a dark ground). */
export const DARK: Partial<typeof C> = { ground: '#15121A', surface: '#211C27', ink: '#F7F5F2', line: '#3A3341', text2: '#C9C3CE' };
