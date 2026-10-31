// Radar-sweep scan (D-20; design 01b). The arm turns once per scan and the wheel is revealed
// behind it. A scan does not know its total in advance, so the arm follows an estimate:
//
//   - with an expected entry count (the previous scan's), entries / expected;
//   - otherwise a slow creep, so the arm keeps moving;
//
// capped below a full turn until the scan really ends. The arm never turns faster than one
// revolution per SWEEP_MS (the design's 3.4 s), so a fast scan still reads as a sweep, and it
// finishes the turn after the report arrives. Reduced motion: the arm jumps to the target.

/** One full revolution at full speed, in ms (design: 3400 ms). */
export const SWEEP_MS = 3400;
/** The arm stops here until the scan ends. */
export const SWEEP_HOLD = 0.97;
/** Creep time constant without an estimate, in ms. */
export const CREEP_MS = 6000;

export interface SweepInput {
  /** The scan has finished (report received, cancelled or failed). */
  done: boolean;
  /** Entries walked so far. */
  entries: number;
  /** Entries the last scan walked, when known. */
  expected: number | null;
  /** Time since the scan started, in ms. */
  elapsedMs: number;
}

/** Where the arm is heading, 0..1. */
export function sweepTarget(s: SweepInput): number {
  if (s.done) return 1;
  const creep = SWEEP_HOLD * (1 - Math.exp(-s.elapsedMs / CREEP_MS));
  const estimate = s.expected && s.expected > 0 ? s.entries / s.expected : 0;
  return Math.min(SWEEP_HOLD, Math.max(creep, estimate));
}

/** Move the arm toward `target` after `dtMs`; never backwards, at most one turn per SWEEP_MS. */
export function advanceSweep(current: number, target: number, dtMs: number, reducedMotion: boolean): number {
  if (target <= current) return current;
  if (reducedMotion) return target;
  return Math.min(target, current + Math.max(0, dtMs) / SWEEP_MS);
}
