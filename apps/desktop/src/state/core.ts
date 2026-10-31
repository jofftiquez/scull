// What the shell knows from the core: the last status, a running scan (progress, cancel) and
// the figures next to nav items. Screens read `core` and call `rescan` / `cancelScan`.
import { markRaw, reactive } from 'vue';
import type { DesktopStatus, ProgressEvent, ScanReport, ScullBridge, TierBreakdown } from '../bridge';
import { toBridgeError } from '../bridge';
import { formatBytes } from '../lib/format';
import { NOT_SCANNED, shell } from './shell';

export interface CoreState {
  status: DesktopStatus | null;
  scanning: boolean;
  progress: ProgressEvent | null;
  /** The last operation's error, shown in the header. */
  error: string | null;
  /** The last scan report of this session (a cancelled one only until a complete one exists),
   * kept raw (not deeply reactive): the Overview draws its wheel from it. */
  report: ScanReport | null;
}

export const core = reactive<CoreState>({ status: null, scanning: false, progress: null, error: null, report: null });

let controller: AbortController | null = null;

const total = (t: TierBreakdown) => t.s0 + t.s1 + t.s2 + t.s3;

/** "4 min ago", "just now", "3 h ago", "2 days ago". */
export function ago(secs: number): string {
  if (secs < 60) return 'just now';
  if (secs < 3600) return `${Math.floor(secs / 60)} min ago`;
  if (secs < 86400) return `${Math.floor(secs / 3600)} h ago`;
  const d = Math.floor(secs / 86400);
  return `${d} day${d === 1 ? '' : 's'} ago`;
}

/** The header's freshness line (design 01), from the status or a running scan. */
export function statusLine(s: CoreState): string {
  if (s.scanning) {
    const p = s.progress;
    if (p?.phase === 'scanning') {
      return `scanning · ${(p.entries ?? 0).toLocaleString('en-US')} entries · ${formatBytes(p.allocated_bytes ?? 0)} found`;
    }
    if (p?.phase === 'analyzing') return `analysing · ${p.what ?? ''}`;
    return 'scanning…';
  }
  if (s.error) return `scan failed · ${s.error}`;
  const last = s.status?.last_scan;
  if (!last) return NOT_SCANNED;
  const parts = [`scanned ${ago(last.age_secs)}`];
  if (last.roots != null) parts.push(`${last.roots} folder${last.roots === 1 ? '' : 's'}`);
  parts.push(`${formatBytes(last.reclaimable_bytes)} reclaimable`);
  if (last.status === 'partial') parts.push('partial');
  return parts.join(' · ');
}

function publish() {
  shell.status = statusLine(core);
}

/** Read status, projects, worktrees and caches; fill the header and nav figures. */
export async function refresh(bridge: ScullBridge): Promise<void> {
  try {
    core.status = await bridge.status();
  } catch (e) {
    core.error = toBridgeError(e).message;
  }
  publish();
  const [projects, worktrees, caches] = await Promise.allSettled([
    bridge.projects(),
    bridge.worktrees(),
    bridge.caches(),
  ]);
  const counts = { ...shell.counts };
  if (projects.status === 'fulfilled' && projects.value.projects.length > 0) {
    counts.projects = String(projects.value.projects.length);
  }
  if (worktrees.status === 'fulfilled' && worktrees.value.worktrees.length > 0) {
    counts.worktrees = String(worktrees.value.worktrees.length);
  }
  if (caches.status === 'fulfilled' && caches.value.candidates.length > 0) {
    counts.caches = formatBytes(total(caches.value.totals.reclaimable));
  }
  shell.counts = counts;
}

/** Scan the configured roots with live progress in the header. */
export async function rescan(bridge: ScullBridge): Promise<void> {
  if (core.scanning) return;
  controller = new AbortController();
  core.scanning = true;
  core.error = null;
  core.progress = null;
  publish();
  try {
    const report = await bridge.scan(
      {},
      {
        signal: controller.signal,
        onProgress: (p) => {
          core.progress = p;
          publish();
        },
      },
    );
    if (!report.cancelled || !core.report) core.report = markRaw(report);
  } catch (e) {
    core.error = toBridgeError(e).message;
  } finally {
    core.scanning = false;
    core.progress = null;
    controller = null;
  }
  await refresh(bridge);
}

/** Cancel the running scan (it resolves as `cancelled`). */
export function cancelScan(): void {
  controller?.abort();
}
