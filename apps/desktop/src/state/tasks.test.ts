import { describe, expect, it, vi } from 'vitest';
import { createTaskQueue, TASK_LOG_LIMIT, TASK_LOG_LINES } from './tasks';
const summary = () => ({ summary: 'Finished' });
const deferred = () => { let resolve!: () => void; const promise = new Promise<void>((r) => { resolve = r; }); return { promise, resolve }; };
describe('application action queue', () => {
  it('serializes confirmed jobs, reports real progress, and retains results independently of screens', async () => {
    const q = createTaskQueue(); const gate = deferred(); const order: number[] = [];
    const first = q.enqueue('First', async (progress) => { order.push(1); progress(.25); await gate.promise; return 1; }, summary);
    const second = q.enqueue('Second', async () => { order.push(2); return 2; }, summary);
    await vi.waitFor(() => expect(q.state.tasks[0]?.status).toBe('running'));
    expect(order).toEqual([1]); expect(q.state.tasks[0]?.progress).toBe(.25); expect(q.state.tasks[1]?.status).toBe('pending');
    gate.resolve(); expect(await first).toBe(1); expect(await second).toBe(2); expect(order).toEqual([1, 2]);
    expect(q.state.tasks.map((t) => t.status)).toEqual(['succeeded', 'succeeded']);
  });
  it('waits behind a native lock and can cancel without running', async () => {
    const q = createTaskQueue(); q.setProbe(async () => true); const run = vi.fn();
    const result = q.enqueue('Pending', run, summary).catch((e: Error) => e.message);
    await vi.waitFor(() => expect(q.state.externalBusy).toBe(true));
    q.cancel(q.state.tasks[0]!.id); expect(await result).toContain('Cancelled before starting'); expect(run).not.toHaveBeenCalled();
  });
  it('does not execute when lock status is unknown and exposes the error', async () => {
    const q = createTaskQueue(); q.setProbe(async () => { throw new Error('lock unreadable'); }); const run = vi.fn();
    const result = q.enqueue('Pending', run, summary).catch(() => undefined);
    await vi.waitFor(() => expect(q.state.admissionError).toBe('lock unreadable'));
    expect(run).not.toHaveBeenCalled(); q.cancel(q.state.tasks[0]!.id); await result;
  });
  it('continues after failures and distinguishes partial outcomes from success', async () => {
    const q = createTaskQueue(); const failure = q.enqueue('Fail', async () => { throw new Error('refused'); }, summary).catch(() => undefined);
    const partial = q.enqueue('Partial', async () => 1, () => ({ summary: 'One skipped', partial: true }));
    await failure; await partial; expect(q.state.tasks.map((t) => t.status)).toEqual(['failed', 'partial']); expect(q.state.tasks[0]?.summary).toBe('refused');
  });
  it('labels zero completed results as failed and retains per-item reasons', async () => {
    const q = createTaskQueue();
    await q.enqueue('Clean caches', async () => 0, () => ({ summary: '0 of 2 caches cleaned', partial: true, failed: true, details: [{ label: 'Bun', status: 'failed', reason: 'Command exited with code 1.' }] }), '2 selected caches');
    expect(q.state.tasks[0]).toMatchObject({ status: 'failed', target: '2 selected caches', details: [{ label: 'Bun', reason: 'Command exited with code 1.' }] });
  });

  it('retains live counts and output while a task is still running, with bounded logs', async () => {
    const q = createTaskQueue(); const gate = deferred();
    const run = q.enqueue('Clean caches', async (report) => {
      report({ completed: 1, total: 3, current: 'Cleaning Bun', log: { stream: 'stdout', text: 'working' } });
      await gate.promise;
      for (let i = 0; i < TASK_LOG_LINES + 20; i++) report({ completed: 2, total: 3, log: { stream: 'stdout', text: `${i}: ${'x'.repeat(200)}` } });
    }, summary);
    await vi.waitFor(() => expect(q.state.tasks[0]?.completed).toBe(1));
    const task = q.state.tasks[0]!;
    expect(task).toMatchObject({ status: 'running', total: 3, current: 'Cleaning Bun', logs: [{ text: 'working' }] });
    expect(task.progress).toBeCloseTo(1 / 3);
    gate.resolve(); await run;
    expect(task.logsTruncated).toBe(true);
    expect(task.logs!.length).toBeLessThanOrEqual(TASK_LOG_LINES);
    expect(task.logs!.reduce((n, line) => n + line.text.length, 0)).toBeLessThanOrEqual(TASK_LOG_LIMIT);
  });

});
