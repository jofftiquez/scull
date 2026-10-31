// Drives the radar sweep (sweep.ts) frame by frame while a scan runs.
import { onBeforeUnmount, ref, type Ref } from 'vue';
import { advanceSweep, sweepTarget } from './sweep';
import { nextFrame } from './motion';

export interface RadarSweep {
  /** Arm position, 0..1 (1 = whole wheel revealed). */
  progress: Ref<number>;
  /** The sweep is running (the scan, then the rest of the turn). */
  sweeping: Ref<boolean>;
  /** Bumps when a sweep completes (the center's bounce keys on it). */
  completed: Ref<number>;
  /** Sweep while `scan` runs. `entries()` reads live progress; `expected` is the last scan's
   * entry count, if any. Resolves when the arm has finished its turn. */
  run(scan: () => Promise<unknown>, entries: () => number, expected: number | null): Promise<void>;
}

export function useRadarSweep(reduced: Ref<boolean>): RadarSweep {
  const progress = ref(1);
  const sweeping = ref(false);
  const completed = ref(0);
  let stop: (() => void) | null = null;
  let finish: (() => void) | null = null;

  async function run(scan: () => Promise<unknown>, entries: () => number, expected: number | null) {
    if (sweeping.value) return;
    sweeping.value = true;
    progress.value = 0;
    const t0 = performance.now();
    let last = t0;
    let done = false;
    const turned = new Promise<void>((resolve) => {
      finish = resolve;
      const tick = (now: number) => {
        const target = sweepTarget({ done, entries: entries(), expected, elapsedMs: now - t0 });
        progress.value = advanceSweep(progress.value, target, now - last, reduced.value);
        last = now;
        if (done && progress.value >= 1) {
          stop = null;
          finish = null;
          sweeping.value = false;
          completed.value += 1;
          resolve();
          return;
        }
        stop = nextFrame(tick);
      };
      stop = nextFrame(tick);
    });
    try {
      await scan();
    } catch {
      // The scan reports its own error (header); the sweep just completes.
    } finally {
      done = true;
    }
    await turned;
  }

  onBeforeUnmount(() => {
    stop?.();
    finish?.();
    stop = null;
    finish = null;
    sweeping.value = false;
    progress.value = 1;
  });

  return { progress, sweeping, completed, run };
}
