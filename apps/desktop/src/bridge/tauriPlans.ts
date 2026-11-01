// The plan bridge inside Tauri.
//
// * AI agent plans (P4-MCP-02) through the core's approval broker: the human approves, denies or
//   revokes here; the agent executes an approved plan itself, through `scull mcp serve`.
// * The desktop's own plans (P3-EXEC-01): `preparePlan` records a plan from the app's last scan;
//   `executePlan` runs exactly the items the human confirmed through the core Executor (lock,
//   revalidation, D-28, audit with the desktop channel; apps/desktop/bridge/src/cleanup.rs).
//   The webview never touches a file (SAFE-10: deletion only through the Executor).
//
// The confirmation code arrives with the opened request for DISPLAY ONLY: the core keeps the
// challenge in its own memory and checks the code the human typed (`approvePlan` sends it
// back). It is never stored or logged here.
//
// The core reports a plan's state, not a stream: `watchApproval` polls `agent_plan_status` and
// turns changes into the dialog's events (approved elsewhere, agent started, item outcomes,
// finished, voided, denied, revoked, expired).
import type {
  DesktopPlan,
  DesktopRun,
  RunItemOutcome,
  AgentApprovalRequest,
  AgentApproved,
  AgentDenied,
  AgentItemOutcome,
  AgentItemResult,
  AgentOperation,
  AgentPlanStatus,
  AgentRequests,
  AgentRevoked,
} from './generated';
import type {
  AgentRequestSummary,
  ApprovalEvent,
  ApprovalRequest,
  ExecutionResult,
  ItemOutcome,
  ItemOutcomeKind,
  PlanBridge,
  PlanPreview,
} from './plan';
import { realTransport, type TauriTransport } from './tauri';
import { toBridgeError } from './types';

/** The Tauri commands and event (apps/desktop/src-tauri/src/lib.rs). */
export const APPROVAL_COMMANDS = {
  requests: 'approval_requests',
  open: 'open_approval_request',
  approve: 'approve_agent_plan',
  deny: 'deny_agent_plan',
  revoke: 'revoke_agent_approval',
  status: 'agent_plan_status',
  takeDeepLink: 'take_deep_link',
} as const;
export const OPEN_REQUEST_EVENT = 'scull://open-request';

/** The cleanup commands (P3-EXEC-01; main window only). */
export const CLEANUP_COMMANDS = {
  prepare: 'prepare_desktop_plan',
  execute: 'execute_desktop_plan',
  restore: 'restore_quarantined',
  quickClean: 'quick_clean',
} as const;

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

/** How often an open dialog asks the core where its plan stands. */
export const STATUS_POLL_MS = 1000;

const iso = (secs: number) => new Date(secs * 1000).toISOString();

/** The dialog's outcome kinds (deleted / quarantined / trashed / skipped). */
export function outcomeKind(o: AgentItemOutcome): ItemOutcomeKind {
  switch (o) {
    case 'removed':
    case 'tool_run':
      return 'deleted';
    case 'quarantined':
      return 'quarantined';
    case 'moved_to_trash':
      return 'trashed';
    default:
      return 'skipped';
  }
}

const OUTCOME_REASON: Partial<Record<AgentItemOutcome, string>> = {
  partial: 'only partly cleaned',
  failed: 'cleaning it failed',
  refused: 'refused by the re-check',
  unknown: 'outcome unknown to this version',
};

export function itemOutcome(r: AgentItemResult): ItemOutcome {
  const kind = outcomeKind(r.outcome);
  const reason = r.reason ?? OUTCOME_REASON[r.outcome];
  return {
    item_id: r.item_id,
    outcome: kind,
    freed_bytes: r.freed_bytes,
    moved_bytes: r.moved_bytes,
    ...(kind === 'skipped' && reason ? { reason } : {}),
  };
}

export function executionResult(planId: string, op: AgentOperation): ExecutionResult {
  return {
    plan_id: planId,
    items: op.items.map(itemOutcome),
    predicted_bytes: op.predicted_bytes,
    measured_freed_bytes: Math.max(0, op.measured_delta_bytes ?? op.freed_bytes),
    moved_bytes: op.moved_bytes,
    finished_at: iso(op.finished_at ?? op.started_at),
  };
}

/** The opened request as the dialog shows it. */
export function approvalRequestOf(r: AgentApprovalRequest): ApprovalRequest {
  const plan: PlanPreview = {
    plan_id: r.plan_id,
    hash: r.content_hash,
    created_at: iso(r.created_at),
    expires_at: iso(r.expires_at),
    // The client's name as it gave itself (sanitized by the core, unverified, #78); the core
    // records no process or note.
    source: r.client
      ? { kind: 'mcp', tool: 'prepare_cleanup', client: r.client, initials: initialsOf(r.client) }
      : { kind: 'mcp', tool: 'prepare_cleanup' },
    items: r.items.map((i) => ({
      candidate: i.candidate,
      from_last_scan: i.from_last_scan,
      ...(i.from_last_scan ? {} : { note: 'as recorded in the plan · not in your last scan' }),
    })),
    skipped: [],
    left_out: [],
    estimate_notes: [],
  };
  return { plan, code: r.code };
}

export function requestSummaryOf(r: AgentRequests['requests'][number]): AgentRequestSummary {
  return {
    plan_id: r.plan_id,
    hash: r.content_hash,
    created_at: iso(r.created_at),
    expires_at: iso(r.expires_at),
    item_count: r.item_count,
    reclaimable_bytes: r.reclaimable_bytes,
    ...(r.client ? { client: r.client } : {}),
  };
}

/** Two letters for the avatar: "Claude Code 2.1" → "CC". */
export function initialsOf(client: string): string {
  const words = client.split(/\s+/).filter((w) => /^[A-Za-z]/.test(w));
  const letters = (words.length > 1 ? words.slice(0, 2).map((w) => w[0]) : [client.slice(0, 2)]).join('');
  return letters.toUpperCase() || 'AI';
}

/** What a status poll adds to what the dialog was already told. */
interface Seen {
  state: AgentPlanStatus['state'];
  started: boolean;
  items: number;
}

const VOIDED_REASON = 'A target changed on disk before the plan ran, so the whole plan is void (D-34).';

/** The events that take the dialog from `seen` to `s`; updates `seen`. */
export function eventsFor(s: AgentPlanStatus, seen: Seen): ApprovalEvent[] {
  const out: ApprovalEvent[] = [];
  const op = s.operation ?? null;
  if (s.state === seen.state && s.state !== 'running') return out;
  switch (s.state) {
    case 'approved':
      out.push({ kind: 'approved', at: iso(s.approved_at ?? 0), channel: s.channel ?? 'unknown' });
      break;
    case 'running':
    case 'executed':
      if (!op) break;
      if (seen.state === 'awaiting_approval' && s.approved_at != null) {
        out.push({ kind: 'approved', at: iso(s.approved_at), channel: s.channel ?? 'unknown' });
      }
      if (!seen.started) {
        out.push({ kind: 'agent_started', at: iso(op.started_at) });
        seen.started = true;
      }
      for (const r of op.items.slice(seen.items)) out.push({ kind: 'item_finished', outcome: itemOutcome(r) });
      seen.items = op.items.length;
      if (s.state === 'executed') out.push({ kind: 'finished', result: executionResult(s.plan_id, op) });
      break;
    case 'voided':
      out.push({ kind: 'invalidated', item_id: '', reason: VOIDED_REASON, at: iso(s.used_at ?? 0) });
      break;
    case 'denied':
      out.push({ kind: 'denied', at: iso(s.denied_at ?? 0) });
      break;
    case 'revoked':
      out.push({ kind: 'revoked', at: iso(s.used_at ?? 0) });
      break;
    case 'expired':
      out.push({ kind: 'expired', at: iso(s.plan_expires_at) });
      break;
    default:
      break;
  }
  seen.state = s.state;
  return out;
}

const FINAL: ReadonlySet<AgentPlanStatus['state']> = new Set(['executed', 'voided', 'denied', 'revoked', 'expired']);

export interface TauriPlanOptions {
  transport?: TauriTransport;
  pollMs?: number;
}

export function createTauriPlanBridge(opts: TauriPlanOptions = {}): PlanBridge {
  const t = opts.transport ?? realTransport;
  const pollMs = opts.pollMs ?? STATUS_POLL_MS;
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
          typedCount: null,
        }),
      );
      for (const outcome of r.items) o.onEvent?.({ kind: 'item_finished', outcome });
      return r;
    },

    async approvalRequests() {
      const r = await call<AgentRequests>(APPROVAL_COMMANDS.requests);
      return r.requests.map(requestSummaryOf);
    },

    async approvalRequest(planId) {
      try {
        return approvalRequestOf(await call<AgentApprovalRequest>(APPROVAL_COMMANDS.open, { planId }));
      } catch (e) {
        const err = toBridgeError(e);
        if (err.kind === 'not_found') return null;
        throw err;
      }
    },

    async approvePlan(req) {
      const r = await call<AgentApproved>(APPROVAL_COMMANDS.approve, {
        planId: req.plan_id,
        contentHash: req.hash,
        code: req.code,
      });
      return { approved_at: iso(r.approved_at) };
    },

    async denyPlan(req) {
      const r = await call<AgentDenied>(APPROVAL_COMMANDS.deny, { planId: req.plan_id });
      return { denied_at: iso(r.denied_at) };
    },

    async revokeApproval(req) {
      const r = await call<AgentRevoked>(APPROVAL_COMMANDS.revoke, { planId: req.plan_id });
      return { revoked_at: iso(r.revoked_at) };
    },

    watchApproval(planId, onEvent) {
      const seen: Seen = { state: 'awaiting_approval', started: false, items: 0 };
      let stopped = false;
      let timer: ReturnType<typeof setTimeout> | undefined;
      const poll = async () => {
        if (stopped) return;
        try {
          const s = await call<AgentPlanStatus>(APPROVAL_COMMANDS.status, { planId });
          if (stopped) return;
          for (const e of eventsFor(s, seen)) onEvent(e);
          if (FINAL.has(s.state)) return;
        } catch {
          // A transient read failure (the database busy, ...): ask again next time.
        }
        if (!stopped) timer = setTimeout(() => void poll(), pollMs);
      };
      timer = setTimeout(() => void poll(), pollMs);
      return () => {
        stopped = true;
        clearTimeout(timer);
      };
    },

    onOpenRequest(onOpen) {
      let stopped = false;
      let unlisten: (() => void) | undefined;
      void call<string | null>(APPROVAL_COMMANDS.takeDeepLink)
        .then((id) => {
          if (id && !stopped) onOpen(id);
        })
        .catch(() => undefined);
      void t
        .listen<string>(OPEN_REQUEST_EVENT, (id) => {
          if (!stopped && typeof id === 'string') onOpen(id);
        })
        .then((u) => {
          if (stopped) u();
          else unlisten = u;
        })
        .catch(() => undefined);
      return () => {
        stopped = true;
        unlisten?.();
      };
    },
  };
}
