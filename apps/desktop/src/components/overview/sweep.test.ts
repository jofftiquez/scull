import { describe, expect, it } from 'vitest';
import { advanceSweep, SWEEP_HOLD, SWEEP_MS, sweepTarget } from './sweep';
import { prefersReducedMotion } from './motion';

describe('radar sweep', () => {
  it('follows entries / expected, held below a full turn until the scan ends', () => {
    expect(sweepTarget({ done: false, entries: 500, expected: 1000, elapsedMs: 0 })).toBeCloseTo(0.5);
    expect(sweepTarget({ done: false, entries: 5000, expected: 1000, elapsedMs: 0 })).toBe(SWEEP_HOLD);
    expect(sweepTarget({ done: true, entries: 0, expected: null, elapsedMs: 0 })).toBe(1);
  });

  it('creeps without an estimate so the arm never stalls', () => {
    const a = sweepTarget({ done: false, entries: 0, expected: null, elapsedMs: 1000 });
    const b = sweepTarget({ done: false, entries: 0, expected: null, elapsedMs: 5000 });
    expect(a).toBeGreaterThan(0);
    expect(b).toBeGreaterThan(a);
    expect(sweepTarget({ done: false, entries: 0, expected: null, elapsedMs: 1e9 })).toBeLessThanOrEqual(SWEEP_HOLD);
  });

  it('turns at most once per SWEEP_MS and never backwards', () => {
    expect(advanceSweep(0, 1, SWEEP_MS / 4, false)).toBeCloseTo(0.25);
    expect(advanceSweep(0, 0.1, SWEEP_MS, false)).toBe(0.1);
    expect(advanceSweep(0.6, 0.2, 16, false)).toBe(0.6);
    // A fast scan still takes the design's 3.4 s for the full turn.
    let p = 0;
    let t = 0;
    while (p < 1) {
      p = advanceSweep(p, 1, 16, false);
      t += 16;
    }
    expect(t).toBeGreaterThanOrEqual(SWEEP_MS);
    expect(t).toBeLessThan(SWEEP_MS + 32);
  });

  it('jumps straight to the target under reduced motion', () => {
    expect(advanceSweep(0, 1, 0, true)).toBe(1);
    expect(advanceSweep(0.2, 0.7, 1, true)).toBe(0.7);
  });

  it('reads prefers-reduced-motion', () => {
    const mm = (matches: boolean) => ({ matchMedia: () => ({ matches }) as MediaQueryList });
    expect(prefersReducedMotion(mm(true))).toBe(true);
    expect(prefersReducedMotion(mm(false))).toBe(false);
    expect(prefersReducedMotion({})).toBe(false);
  });
});
