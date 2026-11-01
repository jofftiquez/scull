// The Storage Explorer's actions that the core bridge does not have yet (P3-UI-07).
//
//   addToCleanup(ids)  put candidates in the Cleanup Review plan. Planned bridge command: the
//                      plan builder of the Phase 2 executor (`plan_add`, #67); the cleanup
//                      itself still goes through Cleanup Review and the core Executor (SAFE-10).
//   reveal(path)       show the folder in Finder / Explorer / Files.
//
// The desktop never deletes: in a plain browser the mock only records the call (it touches
// nothing), and inside Tauri the pending implementation rejects with a BridgeError until the
// commands exist. These calls stay out of `ScullBridge` so its read-only surface and its tests
// are unchanged.
import { BridgeError, type ScullBridge } from '../../bridge';

export interface ExplorerActions {
  readonly kind: 'mock' | 'pending';
  /** Resolves with how many candidates were queued. */
  addToCleanup(ids: readonly string[]): Promise<{ queued: number }>;
  reveal(path: string): Promise<void>;
}

export function createMockExplorerActions(): ExplorerActions & { calls: string[] } {
  const calls: string[] = [];
  return {
    kind: 'mock',
    calls,
    async addToCleanup(ids) {
      calls.push(`add ${ids.join(',')}`);
      return { queued: ids.length };
    },
    async reveal(path) {
      calls.push(`reveal ${path}`);
    },
  };
}

const unsupported = (what: string) =>
  new BridgeError({ kind: 'internal', message: `${what} from the Explorer arrives with the Phase 2 plan bridge; use Cleanup Review or \`scull clean\` for now.` });

export function createPendingExplorerActions(): ExplorerActions {
  return {
    kind: 'pending',
    addToCleanup: async () => Promise.reject(unsupported('Adding to a cleanup')),
    reveal: async () => Promise.reject(unsupported('Showing a folder')),
  };
}

export function explorerActionsFor(bridge: ScullBridge): ExplorerActions {
  return bridge.kind === 'mock' ? createMockExplorerActions() : createPendingExplorerActions();
}
