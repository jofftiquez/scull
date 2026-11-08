import { describe, expect, it, vi } from 'vitest';
import { runCacheCommand, CACHE_PROGRESS_EVENT } from './cacheProgress';
import type { DesktopCacheProgress } from './generated';
import type { TauriTransport } from './tauri';

describe('cache command stream', () => {
  it('subscribes before invoking, filters operation IDs and delivers output before completion', async () => {
    const received: DesktopCacheProgress[] = [];
    const stop = vi.fn();
    let handler!: (event: DesktopCacheProgress) => void;
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    const transport: TauriTransport = {
      listen: vi.fn(async (_name, callback) => { handler = callback as typeof handler; return stop; }),
      invoke: async <T,>(_command: string, args?: Record<string, unknown>) => {
        expect(handler).toBeTypeOf('function');
        const op_id = String(args?.opId);
        handler({ op_id: 'another-operation', kind: 'output', line: 'ignore', completed: 0, total: 2 });
        handler({ op_id, kind: 'started', tool: 'bun', completed: 0, total: 2 });
        handler({ op_id, kind: 'output', tool: 'bun', stream: 'stdout', line: 'live output', completed: 0, total: 2 });
        await gate;
        handler({ op_id, kind: 'finished', tool: 'bun', succeeded: true, completed: 1, total: 2 });
        return 'done' as T;
      },
    };
    const result = runCacheCommand('quick_clean_run', { choices: [] }, (event) => received.push(event), transport);
    await vi.waitFor(() => expect(received.map((e) => e.kind)).toEqual(['started', 'output']));
    expect(stop).not.toHaveBeenCalled();
    expect(transport.listen).toHaveBeenCalledWith(CACHE_PROGRESS_EVENT, expect.any(Function));
    release(); expect(await result).toBe('done');
    expect(received.at(-1)?.completed).toBe(1); expect(stop).toHaveBeenCalledOnce();
  });
  it('unsubscribes when native validation or execution fails', async () => {
    const stop = vi.fn();
    const transport: TauriTransport = { listen: async () => stop, invoke: async () => { throw new Error('refused'); } };
    await expect(runCacheCommand('quick_clean_run', {}, () => {}, transport)).rejects.toMatchObject({ message: 'refused' });
    expect(stop).toHaveBeenCalledOnce();
  });
});
