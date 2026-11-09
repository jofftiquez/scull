// Plan bridge: prepare and execute the desktop's own cleanup plan.
//
// SAFE-10: desktop plans execute through the audited native Executor. The browser mock
// simulates outcomes without touching files.
//
// The payload types below are LOCAL and provisional: they mirror the core's plan model (PRD
// §10.2, §10.4, §13.3) and reuse the generated `Candidate` for every preview field. When the
// core exposes plans over the bridge, replace them with the generated types.
//
// This is deliberately a separate interface from `ScullBridge`, which stays read-only.
import type { Candidate, ItemId, ScullBridge, Tier } from './types';
import { createMockPlanBridge } from './planMock';
import { createTauriPlanBridge } from './tauriPlans';

/** Who prepared a plan. Desktop plans are the user's own; MCP plans come from an agent. */
export type PlanSource =
  | { kind: 'desktop' }
  | {
      kind: 'mcp';
      /** Client name as reported by the MCP client (not verified). Absent when the plan doesn't
       *  record it (the core's plans don't yet). */
      client?: string;
      /** Two letters for the avatar. */
      initials?: string;
      /** MCP tool that prepared the plan. */
      tool: string;
      /** `scull mcp serve` process, when recorded. */
      pid?: number;
      /** Command that started the server, and where, when recorded. */
      command?: string;
      cwd?: string;
      /** The agent's free-text note, shown as-is (escaped by Vue), when there is one. */
      note?: string;
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
  /** Agent plans: `false` when this app's last scan didn't find the item unchanged, so only
   *  what the plan recorded is known (no evidence; flags unknown). */
  from_last_scan?: boolean;
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

export interface ExecuteOptions {
  onEvent?: (e: ExecutionEvent) => void;
  signal?: AbortSignal;
}

export interface PlanBridge {
  readonly kind: 'mock' | 'tauri';
  /** Build a desktop plan from the last scan, or from just `items` of it; S3 items are never
   *  planned. Dry run: nothing changes. */
  preparePlan(req: { items?: ItemId[] }): Promise<PlanPreview>;
  /** Execute the selected items of a reviewed plan through the core Executor. */
  executePlan(req: { plan_id: string; hash: string; items: ItemId[]; typed_count?: number }, opts?: ExecuteOptions): Promise<ExecutionResult>;
}

const forBridge = new WeakMap<ScullBridge, PlanBridge>();

/** The plan bridge that goes with a core bridge (mock with mock, Tauri with Tauri). */
export function planBridgeFor(bridge: ScullBridge): PlanBridge {
  let p = forBridge.get(bridge);
  if (!p) {
    p = bridge.kind === 'mock' ? createMockPlanBridge({ latencyMs: 120 }) : createTauriPlanBridge();
    forBridge.set(bridge, p);
  }
  return p;
}
