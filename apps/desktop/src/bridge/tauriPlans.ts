// The plan bridge inside Tauri: the desktop's own plans (P3-EXEC-01). `preparePlan` records a
// plan from the app's last scan; `executePlan` runs exactly the items the human confirmed
// through the core Executor (lock, revalidation, D-28, audit with the desktop channel;
// apps/desktop/bridge/src/cleanup.rs). The webview never touches a file (SAFE-10: deletion
// only through the Executor).
import type { DesktopPlan, DesktopRun, RunItemOutcome } from './generated';
import type { ExecutionResult, ItemOutcome, ItemOutcomeKind, PlanBridge, PlanPreview } from './plan';
import { realTransport, type TauriTransport } from './tauri';
import { toBridgeError } from './types';

/** The cleanup commands (apps/desktop/src-tauri/src/lib.rs). */
export const CLEANUP_COMMANDS = {
  executionBusy: 'execution_busy',
  prepare: 'prepare_desktop_plan',
  execute: 'execute_desktop_plan',
} as const;

const iso = (secs: number) => new Date(secs * 1000).toISOString();

const RUN_OUTCOME: Record<RunItemOutcome, ItemOutcomeKind> = {
  deleted: 'deleted',
  quarantined: 'quarantined',
  trashed: 'trashed',
  skipped: 'skipped',
};

/** A recorded desktop plan as Cleanup Review shows it. */
export function desktopPlanOf(p: DesktopPlan): PlanPreview {
  return {
    plan_id: p.plan_id,
    hash: p.content_hash,
    created_at: iso(p.created_at),
    expires_at: iso(p.expires_at),
    source: { kind: 'desktop' },
    items: p.items.map((candidate) => ({ candidate })),
    skipped: [],
    left_out: p.left_out.map((l) => ({ label: l.path, tier: l.tier, reason: l.reason })),
    estimate_notes: [],
  };
}

/** A finished run as Cleanup Review shows it. */
export function desktopRunOf(r: DesktopRun): ExecutionResult {
  const items: ItemOutcome[] = r.items.map((i) => ({
    item_id: i.item_id,
    outcome: RUN_OUTCOME[i.outcome],
    freed_bytes: i.freed_bytes,
    moved_bytes: i.moved_bytes,
    ...(i.reason ? { reason: i.reason } : {}),
  }));
  return {
    plan_id: r.plan_id,
    items,
    predicted_bytes: r.predicted_bytes,
    measured_freed_bytes: Math.max(0, r.measured_delta_bytes ?? items.reduce((a, i) => a + i.freed_bytes, 0)),
    moved_bytes: r.moved_bytes,
    finished_at: iso(r.finished_at),
  };
}


export interface TauriPlanOptions {
  transport?: TauriTransport;
}

export function createTauriPlanBridge(opts: TauriPlanOptions = {}): PlanBridge {
  const t = opts.transport ?? realTransport;
  const call = async <T>(cmd: string, args?: Record<string, unknown>): Promise<T> => {
    try {
      return await t.invoke<T>(cmd, args);
    } catch (e) {
      throw toBridgeError(e);
    }
  };

  return {
    kind: 'tauri',

    async preparePlan(req) {
      return desktopPlanOf(await call<DesktopPlan>(CLEANUP_COMMANDS.prepare, { items: req.items ?? null }));
    },

    async executePlan(req, o = {}) {
      // Sent only from the Confirm button: the reviewed plan, its hash and the chosen items.
      const r = desktopRunOf(
        await call<DesktopRun>(CLEANUP_COMMANDS.execute, {
          planId: req.plan_id,
          contentHash: req.hash,
          items: req.items,
          typedCount: req.typed_count ?? null,
        }),
      );
      for (const outcome of r.items) o.onEvent?.({ kind: 'item_finished', outcome });
      return r;
    },
  };
}
