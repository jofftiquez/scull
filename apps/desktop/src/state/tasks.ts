import { reactive } from 'vue';
import { toBridgeError } from '../bridge/types';

export interface TaskDetail {
  label: string;
  status: 'completed' | 'failed' | 'skipped';
  path?: string;
  reason?: string;
}
export interface TaskLog { stream: 'stdout' | 'stderr' | 'system' | 'command'; text: string; tool?: string }
export interface TaskProgressUpdate { completed: number; total: number; current?: string; log?: TaskLog }
export type TaskReporter = (update: number | TaskProgressUpdate) => void;
export const TASK_LOG_LIMIT = 65536;
export const TASK_LOG_LINES = 400;

function reportProgress(task: ActionTask, update: number | TaskProgressUpdate) {
  if (typeof update === 'number') { task.progress = Math.max(0, Math.min(1, update)); return; }
  task.startedAt ??= Date.now();
  task.total = Math.max(0, update.total);
  task.completed = Math.max(0, Math.min(task.total, update.completed));
  task.progress = task.total ? task.completed / task.total : null;
  task.current = update.current;
  if (update.log) {
    const text = update.log.text.slice(0, TASK_LOG_LIMIT);
    const logs = task.logs ??= [];
    logs.push({ ...update.log, text });
    if (text.length < update.log.text.length) task.logsTruncated = true;
    let size = logs.reduce((n, line) => n + line.text.length, 0);
    while (logs.length > TASK_LOG_LINES || size > TASK_LOG_LIMIT) {
      size -= logs.shift()!.text.length; task.logsTruncated = true;
    }
  }
}

export interface TaskResult { summary: string; partial?: boolean; failed?: boolean; details?: TaskDetail[] }
export interface ActionTask {
  id: number;
  label: string;
  target?: string;
  details?: TaskDetail[];
  status: 'pending' | 'running' | 'succeeded' | 'partial' | 'failed' | 'cancelled';
  summary: string;
  progress: number | null;
  completed?: number;
  total?: number;
  current?: string;
  logs?: TaskLog[];
  logsTruncated?: boolean;
  startedAt?: number;
  celebrated: boolean;
}

export function createTaskQueue() {
  const state = reactive({ tasks: [] as ActionTask[], externalBusy: false, admissionError: '' });
  const jobs = new Map<number, () => Promise<void>>();
  let serial = 0;
  let draining = false;
  let probe: () => Promise<boolean> = async () => false;
  async function checkBusy() {
    try { state.externalBusy = await probe(); state.admissionError = ''; }
    catch (e) { state.admissionError = toBridgeError(e).message; throw e; }
    return state.externalBusy;
  }
  async function drain() {
    if (draining) return;
    draining = true;
    try {
      for (const task of state.tasks) {
        if (task.status !== 'pending') continue;
        while (task.status === 'pending') {
          let busy = true;
          try { busy = await checkBusy(); } catch { /* Unknown admission stays queued. */ }
          if (!busy) break;
          await new Promise((resolve) => setTimeout(resolve, 1000));
        }
        if (task.status !== 'pending') continue;
        task.status = 'running'; task.startedAt = Date.now();
        await jobs.get(task.id)?.();
        jobs.delete(task.id);
      }
    } finally { draining = false; }
  }
  function enqueue<T>(label: string, run: (progress: TaskReporter) => Promise<T>, describe: (result: T) => TaskResult, target?: string): Promise<T> {
    const id = ++serial;
    state.tasks.push({ id, label, target, status: 'pending', summary: '', progress: null, celebrated: false });
    const task = state.tasks[state.tasks.length - 1]!;
    const promise = new Promise<T>((resolve, reject) => {
      jobs.set(id, async () => {
        try {
          const value = await run((update) => reportProgress(task, update));
          const result = describe(value);
          task.status = result.failed ? 'failed' : result.partial ? 'partial' : 'succeeded';
          task.summary = result.summary;
          task.details = result.details;
          resolve(value);
        } catch (e) {
          task.status = 'failed'; task.summary = toBridgeError(e).message;
          task.details = [{ label: 'The action could not finish', status: 'failed', reason: task.summary }];
          reject(e);
        }
      });
      cancellations.set(id, () => { task.status = 'cancelled'; task.summary = 'Cancelled before starting.'; jobs.delete(id); reject(new Error(task.summary)); });
    });
    void drain();
    return promise.finally(() => cancellations.delete(id));
  }
  const cancellations = new Map<number, () => void>();
  return { state, enqueue, checkBusy, setProbe: (fn: () => Promise<boolean>) => { probe = fn; }, cancel: (id: number) => { if (state.tasks.find((t) => t.id === id)?.status === 'pending') cancellations.get(id)?.(); } };
}

// Keep in-flight closures and results when a development component hot-reloads.
const previousQueue = import.meta.hot?.data.taskQueue as ReturnType<typeof createTaskQueue> | undefined;
export const taskQueue = previousQueue ?? createTaskQueue();
if (previousQueue && import.meta.hot?.data.taskVersion !== 4) {
  const enqueue = taskQueue.enqueue;
  taskQueue.enqueue = function<T>(label: string, run: (progress: TaskReporter) => Promise<T>, describe: (value: T) => TaskResult, target?: string) {
    const task = { value: undefined as ActionTask | undefined };
    const promise = enqueue(label, (legacyProgress) => run((update) => {
      if (typeof update === 'number') legacyProgress(update);
      else if (task.value) reportProgress(task.value, update);
    }), (value) => {
      const result = describe(value);
      if (task.value) {
        task.value.details = result.details;
        if (result.failed) queueMicrotask(() => { if (task.value) task.value.status = 'failed'; });
      }
      return result;
    });
    task.value = taskQueue.state.tasks.at(-1);
    if (task.value) task.value.target = target;
    return promise;
  };
}
export const removedWorktrees = import.meta.hot?.data.removedWorktrees as { path: string; scanId: string | null }[] | undefined ?? reactive<{ path: string; scanId: string | null }[]>([]);
if (import.meta.hot) import.meta.hot.dispose((data) => {
  data.taskQueue = taskQueue; data.taskVersion = 4; data.removedWorktrees = removedWorktrees;
});
