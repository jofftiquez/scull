// Small UI primitives on the design tokens (design/tokens.json → styles/theme.css). Native
// elements only: buttons, checkboxes and <dialog> bring keyboard and screen-reader behaviour.
import type { ButtonHTMLAttributes, ReactNode } from 'react';
import type { Tier } from './bridge';
import { TIER_LABEL } from './lib/plan';

type Variant = 'primary' | 'secondary' | 'ghost';

const VARIANTS: Record<Variant, string> = {
  primary: 'bg-ink text-on-ink hover:bg-ink-raised disabled:opacity-40',
  secondary: 'border border-border bg-surface text-ink hover:bg-surface-raised disabled:opacity-40',
  ghost: 'text-text-secondary hover:text-ink hover:bg-surface-raised disabled:opacity-40',
};

export function Button({
  variant = 'secondary',
  className = '',
  ...rest
}: ButtonHTMLAttributes<HTMLButtonElement> & { variant?: Variant }) {
  return (
    <button
      type="button"
      className={`inline-flex items-center gap-2 rounded-button px-4 py-2 text-label font-medium transition-colors disabled:cursor-not-allowed ${VARIANTS[variant]} ${className}`}
      {...rest}
    />
  );
}

/** A tier chip: color plus the tier's name (never color alone). */
export function TierBadge({ tier }: { tier: Tier }) {
  const style: Record<Tier, string> = {
    S0: 'bg-tier-s0 text-tier-s0-text',
    S1: 'bg-tier-s1 text-tier-s1-text',
    S2: 'bg-tier-s2 text-tier-s2-text',
    S3: 'bg-tier-s3 text-tier-s3-text',
  };
  return (
    <span className={`inline-block whitespace-nowrap rounded-chip px-2 py-0.5 font-mono text-caption ${style[tier]}`}>
      {TIER_LABEL[tier]}
    </span>
  );
}

export function Spinner({ label }: { label: string }) {
  return (
    <span role="status" className="inline-flex items-center gap-2 text-text-secondary">
      <span
        aria-hidden
        className="inline-block h-4 w-4 rounded-full border-2 border-line border-t-accent"
        style={{ animation: 'scull-spin 0.8s linear infinite' }}
      />
      {label}
    </span>
  );
}

export function Card({ children, className = '' }: { children: ReactNode; className?: string }) {
  return <section className={`rounded-card border border-line bg-surface p-5 ${className}`}>{children}</section>;
}

/** An error from the core, in its own words (the same messages as the CLI). */
export function ErrorNote({ error }: { error: string }) {
  return (
    <p role="alert" className="rounded-card border border-amber-tint bg-amber-tint-subtle px-4 py-3 text-amber-text">
      {error}
    </p>
  );
}
