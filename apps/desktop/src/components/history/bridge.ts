// The History screen's one mutating action, which the core bridge does not have (P3-UI-09).
//
//   restoreQuarantined(op, item)  move a quarantined item back through the core (P2-EXEC-06):
//                                 the recorded path when it is free, otherwise the first free
//                                 sibling `<name>.scull-restored-<n>`. Never overwrites, and the
//                                 DB manifest (not the UI) decides what is restored.
//
// The desktop never moves files itself (SAFE-10). In a plain browser the mock only animates (it
// touches nothing); inside Tauri `restore_quarantined` restores through the core Executor under
// the execution lock (P3-EXEC-01; main window only), audited with the desktop channel. This stays
// out of `ScullBridge`, whose read-only surface and tests are unchanged.
import { BridgeError, toBridgeError, type HistoryItem, type HistoryOperation, type ScullBridge } from '../../bridge';
import type { DesktopRestore } from '../../bridge/generated';
import { realTransport, type TauriTransport } from '../../bridge/tauri';
import { CLEANUP_COMMANDS } from '../../bridge/tauriPlans';

/** Where an item went back to. */
export interface RestoreResult {
  /** The path it was restored to. */
  restored_to: string;
  /** `true` when the original path was taken and a `.scull-restored-<n>` sibling was used. */
  sibling: boolean;
}

export interface HistoryActions {
  readonly kind: 'mock' | 'pending' | 'tauri';
  restoreQuarantined(op: HistoryOperation, item: HistoryItem): Promise<RestoreResult>;
}

/** The design's restore takes 1.1 s (design/source/History.dc.html). */
export const RESTORE_MS = 1100;

/** Suffix of a sibling restore path (scull-cleaner `RESTORED_SUFFIX`). */
export const RESTORED_SUFFIX = '.scull-restored-';

/** The path a restore lands on: the original if free, else the first free sibling. */
export function restoreTarget(path: string, taken: (p: string) => boolean): RestoreResult {
  if (!taken(path)) return { restored_to: path, sibling: false };
  for (let n = 1; n <= 999; n++) {
    const p = `${path}${RESTORED_SUFFIX}${n}`;
    if (!taken(p)) return { restored_to: p, sibling: true };
  }
  throw new BridgeError({ kind: 'safety_blocked', message: 'no free name to restore to; nothing was moved' });
}

export function createMockHistoryActions(
  opts: { durationMs?: number; occupied?: Iterable<string> } = {},
): HistoryActions & { calls: string[] } {
  const duration = opts.durationMs ?? RESTORE_MS;
  const occupied = new Set(opts.occupied ?? []);
  const calls: string[] = [];
  return {
    kind: 'mock',
    calls,
    async restoreQuarantined(op, item) {
      calls.push(`restore ${op.operation_id} ${item.item_id}`);
      if (item.outcome !== 'quarantined') {
        throw new BridgeError({ kind: 'usage', message: 'only quarantined items can be restored' });
      }
      if (item.restorable_until != null && item.restorable_until * 1000 < Date.now()) {
        throw new BridgeError({ kind: 'not_found', message: 'the quarantine for this item has ended' });
      }
      await new Promise((r) => setTimeout(r, duration));
      const result = restoreTarget(item.path, (p) => occupied.has(p));
      occupied.add(result.restored_to);
      return result;
    },
  };
}

export function createPendingHistoryActions(): HistoryActions {
  return {
    kind: 'pending',
    restoreQuarantined: async () =>
      Promise.reject(
        new BridgeError({
          kind: 'unsupported_platform',
          message: 'Restoring from the app arrives with the Phase 2 executor bridge; nothing was moved.',
        }),
      ),
  };
}

/** Inside Tauri: the core restores it (the manifest of record decides; never overwrites). */
export function createTauriHistoryActions(t: TauriTransport = realTransport): HistoryActions {
  return {
    kind: 'tauri',
    async restoreQuarantined(op, item) {
      try {
        const r = await t.invoke<DesktopRestore>(CLEANUP_COMMANDS.restore, {
          operationId: op.operation_id,
          itemId: item.item_id,
        });
        return { restored_to: r.restored_to, sibling: r.sibling };
      } catch (e) {
        throw toBridgeError(e);
      }
    },
  };
}

export function historyActionsFor(bridge: ScullBridge): HistoryActions {
  // The mock's machine already has something at ~/code/proto/Library again, so restoring it
  // shows the sibling restore (`Library.scull-restored-1`).
  return bridge.kind === 'mock'
    ? createMockHistoryActions({ occupied: ['/Users/ada/code/proto/Library'] })
    : createTauriHistoryActions();
}
