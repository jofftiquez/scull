// Plan bridge STUBS: the calls the Cleanup Review and Git Worktrees screens need to prepare,
// approve and execute a cleanup plan. They do NOT exist in the core bridge yet.
//
// SAFE-10: the desktop never deletes. Execution will go through the core Executor behind a
// reviewed plan (`scull_cleaner::Executor`, Phase 2), reached over a Tauri command that the
// core bridge does not have yet. Until then:
//   - in a plain browser (mock bridge) `createMockPlanBridge` answers with illustrative data
//     and simulated progress, touching nothing;
//   - inside Tauri `createUnavailablePlanBridge` rejects every call with `unsupported_platform`
//     so the screens show their error state instead of pretending to clean.
//
// The payload types below are LOCAL and provisional: they mirror the core's plan model (PRD
// §10.2, §10.4, §13.3) and reuse the generated `Candidate` for every preview field. When the
// core exposes plans over the bridge, replace them with the generated types.
//
// This is deliberately a separate interface from `ScullBridge`, which stays read-only.
import type { InjectionKey } from 'vue';
import { inject } from 'vue';
import type { Candidate, ItemId, ScullBridge, Tier } from './types';
import { BridgeError } from './types';
import { useBridge } from './index';
import { createMockPlanBridge } from './planMock';

/** Who prepared a plan. Desktop plans are the user's own; MCP plans come from an agent. */
export type PlanSource =
  | { kind: 'desktop' }
  | {
      kind: 'mcp';
      /** Client name as reported by the MCP client (not verified). */
      client: string;
      /** Two letters for the avatar. */
      initials: string;
      /** MCP tool that prepared the plan. */
      tool: string;
      /** `scull mcp serve` process. */
      pid: number;
      /** Command that started the server, and where. */
      command: string;
      cwd: string;
      /** The agent's free-text note, shown as-is (escaped by Vue). */
      note: string;
    };

/** One plan item: a full candidate (every SAFE-01 preview field) plus display hints. */
export interface PlanItem {
  candidate: Candidate;
  /** Replaces the path for grouped items, e.g. "31 worktree artifact folders". */
  label?: string;
  /** Number of filesystem items the row stands for (grouped items). */
  members?: number;
  /** Extra context for the sub-line, e.g. "regrew 4× this month". */
  note?: string;
}

/** An item dropped when the plan was re-checked (in use, changed since scan). */
export interface SkippedItem {
  label: string;
  reason: string;
}

/** Something scull left out of the plan on purpose (S3 protected). Never selectable. */
export interface LeftOutItem {
  label: string;
  tier: Tier;
  reason: string;
}

export interface PlanPreview {
  plan_id: string;
  /** Content hash; approval and execution are bound to it. Lowercase hex. */
  hash: string;
  created_at: string;
  expires_at: string;
  source: PlanSource;
  items: PlanItem[];
  skipped: SkippedItem[];
  left_out: LeftOutItem[];
  /** Caveats about the estimate (e.g. pnpm store links). */
  estimate_notes: string[];
}

/** An agent's request waiting for the human (PRD §13.3). */
export interface ApprovalRequest {
  plan: PlanPreview;
  /** Short code the human types to approve. Shown only here; the agent never sees it. */
  code: string;
}

export type ItemOutcomeKind = 'deleted' | 'quarantined' | 'trashed' | 'removed' | 'skipped';

export interface ItemOutcome {
  item_id: ItemId;
  outcome: ItemOutcomeKind;
  /** Free-space delta measured for this item (0 for quarantine: freed when purged). */
  freed_bytes: number;
  /** Bytes moved to quarantine/Trash. */
  moved_bytes: number;
  /** Why it was skipped (SAFE-05 re-check). */
  reason?: string;
}

export interface ExecutionResult {
  plan_id: string;
  items: ItemOutcome[];
  predicted_bytes: number;
  /** Free space after minus before (PRD: predicted vs measured). */
  measured_freed_bytes: number;
  moved_bytes: number;
  finished_at: string;
}

export type ExecutionEvent =
  | { kind: 'item_started'; item_id: ItemId }
  | { kind: 'item_finished'; outcome: ItemOutcome };

/** Events about an agent plan after it was shown (from the core's approval broker). */
export type ApprovalEvent =
  | { kind: 'agent_started'; at: string }
  | { kind: 'item_started'; item_id: ItemId }
  | { kind: 'item_finished'; outcome: ItemOutcome }
  | { kind: 'finished'; result: ExecutionResult }
  /** D-34: any target of an MCP plan changed, so the whole plan is void. */
  | { kind: 'invalidated'; item_id: ItemId; reason: string; at: string };

export interface ExecuteOptions {
  onEvent?: (e: ExecutionEvent) => void;
  signal?: AbortSignal;
}

export interface PlanBridge {
  readonly kind: 'mock' | 'unavailable';
  /** Build a desktop plan from the last scan (dry run: nothing changes). */
  preparePlan(req: { items?: ItemId[] }): Promise<PlanPreview>;
  /** Execute the selected items of a reviewed plan through the core Executor. */
  executePlan(req: { plan_id: string; hash: string; items: ItemId[] }, opts?: ExecuteOptions): Promise<ExecutionResult>;
  /** The agent request for a plan ID (MCP), or null when there is none. */
  approvalRequest(planId: string): Promise<ApprovalRequest | null>;
  /** Record the human's approval (single use, bound to the hash). */
  approvePlan(req: { plan_id: string; hash: string; code: string }): Promise<{ approved_at: string }>;
  denyPlan(req: { plan_id: string }): Promise<{ denied_at: string }>;
  /** Take an approval back before the agent used it. */
  revokeApproval(req: { plan_id: string }): Promise<{ revoked_at: string }>;
  /** Subscribe to broker events for a plan. Returns the unsubscribe function. */
  watchApproval(planId: string, onEvent: (e: ApprovalEvent) => void): () => void;
}

/** Inside Tauri: the core has no plan commands yet, so every call fails clearly. */
export function createUnavailablePlanBridge(): PlanBridge {
  const no = (): never => {
    throw new BridgeError({
      kind: 'unsupported_platform',
      message: 'Cleanup plans are not connected to the core yet; nothing was changed.',
    });
  };
  return {
    kind: 'unavailable',
    preparePlan: async () => no(),
    executePlan: async () => no(),
    approvalRequest: async () => no(),
    approvePlan: async () => no(),
    denyPlan: async () => no(),
    revokeApproval: async () => no(),
    watchApproval: () => () => undefined,
  };
}

const forBridge = new WeakMap<ScullBridge, PlanBridge>();

/** The plan bridge that goes with a core bridge (mock with mock, unavailable with Tauri). */
export function planBridgeFor(bridge: ScullBridge): PlanBridge {
  let p = forBridge.get(bridge);
  if (!p) {
    p = bridge.kind === 'mock' ? createMockPlanBridge({ latencyMs: 120 }) : createUnavailablePlanBridge();
    forBridge.set(bridge, p);
    // Development and E2E builds only (`VITE_SCULL_E2E=1`, apps/desktop/e2e/run.sh): lets
    // screenshot scripts and E2E tests drive the mock (e.g. a target changing). A normal
    // `vite build` drops this branch.
    if (p.kind === 'mock' && (import.meta.env.DEV || import.meta.env.VITE_SCULL_E2E === '1')) {
      (globalThis as { __scullMockPlans?: PlanBridge }).__scullMockPlans = p;
    }
  }
  return p;
}

export const PLAN_BRIDGE_KEY: InjectionKey<PlanBridge> = Symbol('scull-plan-bridge');

/** The provided plan bridge (tests), else the one that goes with the app's core bridge. */
export function usePlanBridge(): PlanBridge {
  return inject(PLAN_BRIDGE_KEY, null) ?? planBridgeFor(useBridge());
}
