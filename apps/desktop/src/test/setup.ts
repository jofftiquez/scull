// Test setup: a more forgiving default for `vi.waitFor`. Screen tests mount the whole app with
// animated mocks; on a loaded machine (parallel builds, CI runners) the 1 s default flakes.
// Tests still wait on conditions; an explicit `timeout` in a call always wins.
import { vi } from 'vitest';

type WaitFor = typeof vi.waitFor;
const original: WaitFor = vi.waitFor.bind(vi);
const patched = ((callback: Parameters<WaitFor>[0], options?: Parameters<WaitFor>[1]) => {
  const opts = typeof options === 'number' ? { timeout: options } : { timeout: 5000, interval: 25, ...(options ?? {}) };
  return original(callback, opts);
}) as WaitFor;
vi.waitFor = patched;
