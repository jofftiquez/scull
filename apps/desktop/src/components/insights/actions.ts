// The Insights screen's actions that the core bridge does not have yet (P3-UI-10).
//
//   neverSuggest(path)   add a path to the never-suggest list (config, CFG-05); planned bridge
//                        command `never_suggest`.
//   exportHistory()      export the history database ("What scull knows", DATA-04); planned
//                        bridge command `export_history`.
//
// Neither deletes anything. In a plain browser the mock records the call; inside Tauri the
// pending implementation rejects with a BridgeError until the commands exist. They stay out of
// `ScullBridge`, so its read-only surface is unchanged.
import { BridgeError, type ScullBridge } from '../../bridge';

export interface InsightsActions {
  readonly kind: 'mock' | 'pending';
  neverSuggest(path: string): Promise<void>;
  exportHistory(): Promise<void>;
}

export function createMockInsightsActions(): InsightsActions & { calls: string[] } {
  const calls: string[] = [];
  return {
    kind: 'mock',
    calls,
    async neverSuggest(path) {
      calls.push(`never ${path}`);
    },
    async exportHistory() {
      calls.push('export');
    },
  };
}

const pending = (what: string) =>
  new BridgeError({ kind: 'internal', message: `${what} from the app isn't available yet; use \`scull data export\` and the config file for now.` });

export function createPendingInsightsActions(): InsightsActions {
  return {
    kind: 'pending',
    neverSuggest: () => Promise.reject(pending('Never-suggest')),
    exportHistory: () => Promise.reject(pending('History export')),
  };
}

export function insightsActionsFor(bridge: ScullBridge): InsightsActions {
  return bridge.kind === 'mock' ? createMockInsightsActions() : createPendingInsightsActions();
}
