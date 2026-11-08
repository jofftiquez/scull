import type { DesktopCacheProgress } from '../../bridge/generated';
import type { TaskProgressUpdate } from '../../state/tasks';
import { toolName } from './model';

/** Counts are completed tools, never guessed percentages inside a command. */
export function cacheTaskProgress(event: DesktopCacheProgress): TaskProgressUpdate {
  const tool = event.tool ? toolName(event.tool) : undefined;
  const update: TaskProgressUpdate = { completed: event.completed, total: event.total, current: event.kind === 'preparing' ? 'Preparing cleanup…' : tool ? `${event.kind === 'finished' || event.kind === 'skipped' ? 'Finished' : 'Cleaning'} ${tool}` : undefined };
  if (event.kind === 'started') update.log = { stream: event.command ? 'command' : 'system', text: event.command ?? `Starting ${tool ?? 'cache cleanup'}`, tool };
  else if (event.kind === 'output') update.log = { stream: event.stream ?? 'stdout', text: event.line ?? '', tool };
  else if (event.kind === 'output_truncated') update.log = { stream: 'system', text: 'Output limit reached; further command output was omitted.', tool };
  else if (event.kind === 'skipped') update.log = { stream: 'system', text: `Skipped: ${event.reason ?? 'the safety check refused this cache'}`, tool };
  else if (event.kind === 'finished') update.log = { stream: 'system', text: event.succeeded ? 'Command completed.' : 'Command failed. See results for the reason.', tool };
  return update;
}
