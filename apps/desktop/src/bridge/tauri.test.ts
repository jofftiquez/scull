// The Tauri bridge over a fake transport: command names and arguments, progress filtering by
// operation ID, cancellation, and errors as BridgeError(ScullError).
import { describe, expect, it } from 'vitest';
import { MONITOR_COMMANDS, MONITOR_EVENT, createTauriBridge, createTauriMonitorControl, newOpId, type TauriTransport } from './tauri';
import { BridgeError, CANCEL_COMMAND, COMMANDS, PROGRESS_EVENT, type ProgressEvent } from './types';
import { MOCK_DATA } from './mock';

interface Call {
  cmd: string;
  args?: Record<string, unknown>;
}

function fake(answer: (c: Call, emit: (p: ProgressEvent) => void) => unknown = () => ({})) {
  const calls: Call[] = [];
  const handlers = new Map<string, Set<(p: unknown) => void>>();
  const emit = (p: ProgressEvent) => handlers.get(PROGRESS_EVENT)?.forEach((h) => h(p));
  const t: TauriTransport = {
    async invoke<T>(cmd: string, args?: Record<string, unknown>) {
      const c = { cmd, args };
      calls.push(c);
      const out = answer(c, emit);
      if (out instanceof Error) throw out;
      return (await out) as T;
    },
    async listen<T>(event: string, handler: (p: T) => void) {
      const set = handlers.get(event) ?? new Set();
      set.add(handler as (p: unknown) => void);
      handlers.set(event, set);
      return () => set.delete(handler as (p: unknown) => void);
    },
  };
  return { t, calls, handlers };
}

describe('tauri bridge', () => {
  it('calls the commands by their Rust names', async () => {
    const { t, calls } = fake();
    const b = createTauriBridge(t);
    await b.appInfo();
    await b.drives();
    await b.projects();
    await b.worktrees();
    await b.status();
    await b.capabilities();
    expect(calls.map((c) => c.cmd)).toEqual([
      COMMANDS.appInfo,
      COMMANDS.drives,
      COMMANDS.projects,
      COMMANDS.worktrees,
      COMMANDS.status,
      COMMANDS.capabilities,
    ]);
  });

  it('history commands send the window start (and the volume) as Tauri arguments', async () => {
    const { t, calls } = fake();
    const b = createTauriBridge(t);
    await b.freeSpaceHistory(100);
    await b.freeSpaceHistory(100, '/Volumes/Dev SSD');
    await b.rollupHistory(200);
    await b.regrowth(300);
    await b.footprintHistory(400);
    expect(calls).toEqual([
      { cmd: COMMANDS.freeSpaceHistory, args: { since: 100, volumeId: null } },
      { cmd: COMMANDS.freeSpaceHistory, args: { since: 100, volumeId: '/Volumes/Dev SSD' } },
      { cmd: COMMANDS.rollupHistory, args: { since: 200 } },
      { cmd: COMMANDS.regrowth, args: { since: 300 } },
      { cmd: COMMANDS.footprintHistory, args: { since: 400 } },
    ]);
  });

  it('digest and monitor status are plain read commands', async () => {
    const { t, calls } = fake();
    const b = createTauriBridge(t);
    await b.weeklyDigest();
    await b.monitorStatus();
    expect(calls).toEqual([
      { cmd: 'weekly_digest', args: undefined },
      { cmd: 'monitor_status', args: undefined },
    ]);
  });

  it('monitor control: configure, pause, send now and snapshots', async () => {
    const { t, calls, handlers } = fake();
    const c = createTauriMonitorControl(t);
    await c.configure({ settings: { interval_secs: 900 }, memory: null });
    await c.pause(3600);
    await c.pause(null);
    await c.sendDigestNow();
    expect(calls).toEqual([
      { cmd: MONITOR_COMMANDS.configure, args: { config: { settings: { interval_secs: 900 }, memory: null } } },
      { cmd: MONITOR_COMMANDS.pause, args: { secs: 3600 } },
      { cmd: MONITOR_COMMANDS.pause, args: { secs: null } },
      { cmd: MONITOR_COMMANDS.sendDigestNow, args: undefined },
    ]);
    const got: unknown[] = [];
    const stop = await c.onSnapshot((s) => got.push(s));
    handlers.get(MONITOR_EVENT)?.forEach((h) => h({ settings: {}, memory: { last_check: 5 } }));
    stop();
    handlers.get(MONITOR_EVENT)?.forEach((h) => h({ settings: {}, memory: {} }));
    expect(got).toEqual([{ settings: {}, memory: { last_check: 5 } }]);
    const { t: bad } = fake(() => new Error('no monitor'));
    const err = await createTauriMonitorControl(bad).pause(60).catch((e) => e);
    expect(err).toBeInstanceOf(BridgeError);
  });

  it('scan sends params and an operation ID, and only forwards its own progress', async () => {
    const { t, calls, handlers } = fake((c, emit) => {
      if (c.cmd === 'scan') {
        const opId = c.args?.opId as string;
        emit({ ...MOCK_DATA.progressEvents[0]!, op_id: 'someone-else' });
        emit({ ...MOCK_DATA.progressEvents[0]!, op_id: opId });
        return MOCK_DATA.scanReport;
      }
      return {};
    });
    const got: ProgressEvent[] = [];
    const r = await createTauriBridge(t).scan({ roots: ['/Users/ada/code'] }, { onProgress: (p) => got.push(p) });
    expect(r.scan_id).toBe(MOCK_DATA.scanReport.scan_id);
    const scan = calls.find((c) => c.cmd === 'scan')!;
    expect(scan.args?.params).toEqual({ roots: ['/Users/ada/code'] });
    expect(scan.args?.opId).toMatch(/^[A-Za-z0-9_-]{1,64}$/);
    expect(got).toHaveLength(1);
    expect(got[0]!.op_id).toBe(scan.args?.opId);
    // The listener is removed when the operation ends.
    expect(handlers.get(PROGRESS_EVENT)?.size).toBe(0);
  });

  it('aborting sends cancel_operation with the same ID', async () => {
    const ctl = new AbortController();
    let resolveScan: (v: unknown) => void = () => undefined;
    const { t, calls } = fake((c) => {
      if (c.cmd === 'caches') return new Promise((r) => (resolveScan = r));
      if (c.cmd === CANCEL_COMMAND) {
        resolveScan({ ...MOCK_DATA.cachesReport, cancelled: true });
        return true;
      }
      return {};
    });
    const pending = createTauriBridge(t).caches({ signal: ctl.signal });
    await Promise.resolve();
    ctl.abort();
    const r = await pending;
    expect(r.cancelled).toBe(true);
    const op = calls.find((c) => c.cmd === 'caches')!.args?.opId;
    expect(calls.find((c) => c.cmd === CANCEL_COMMAND)?.args).toEqual({ opId: op });
  });

  it('an already-aborted signal cancels at once (the core remembers early cancels)', async () => {
    const ctl = new AbortController();
    ctl.abort();
    const { t, calls } = fake((c) => (c.cmd === 'scan' ? MOCK_DATA.scanReport : true));
    await createTauriBridge(t).scan({}, { signal: ctl.signal });
    const cancel = calls.findIndex((c) => c.cmd === CANCEL_COMMAND);
    const scan = calls.findIndex((c) => c.cmd === 'scan');
    expect(cancel).toBeGreaterThanOrEqual(0);
    expect(calls[cancel]!.args?.opId).toBe(calls[scan]!.args?.opId);
  });

  it('errors arrive as BridgeError carrying the ScullError', async () => {
    const { t } = fake(() => {
      throw { kind: 'safety_blocked', message: 'scan root is the home folder', path: '/Users/ada' };
    });
    const err = await createTauriBridge(t)
      .scan({ roots: ['/Users/ada'] })
      .catch((e) => e);
    expect(err).toBeInstanceOf(BridgeError);
    expect((err as BridgeError).kind).toBe('safety_blocked');
    expect((err as BridgeError).error.path).toBe('/Users/ada');

    const { t: t2 } = fake(() => new Error('ipc closed'));
    const err2 = await createTauriBridge(t2).status().catch((e) => e);
    expect((err2 as BridgeError).kind).toBe('internal');
  });

  it('operation IDs are unique and valid', () => {
    const ids = new Set(Array.from({ length: 50 }, newOpId));
    expect(ids.size).toBe(50);
    for (const id of ids) expect(id).toMatch(/^[A-Za-z0-9_-]{1,64}$/);
  });
});
