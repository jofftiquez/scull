// The Settings screen's writes, which the core bridge does not have (P3-UI-11).
//
//   save(key, value)       change one setting. Config keys (`scan.roots`, `mcp.redact_home`, ...)
//                          will be written to config.toml by a core command (the same
//                          validation as the CLI); `app.*` keys are app preferences (schedule,
//                          menu bar, alerts, ...) that have no config key yet.
//   connectAgent / disconnectAgent   MCP client registration (`scull agent install`, §17).
//   exportData / deleteHistory       `scull data export` / `scull data purge`.
//
// The desktop never writes files itself. In a plain browser the mock keeps changes in memory
// (it touches nothing); inside Tauri the pending implementation rejects with a BridgeError, so
// the screen reverts the control and says why. This stays out of `ScullBridge`, whose read-only
// surface and tests are unchanged.
import { BridgeError, type ScullBridge } from '../../bridge';

export type SettingValue = boolean | number | string | string[];

export interface SettingsWriter {
  readonly kind: 'mock' | 'pending';
  save(key: string, value: SettingValue): Promise<void>;
  connectAgent(client: string): Promise<void>;
  disconnectAgent(client: string): Promise<void>;
  exportData(): Promise<void>;
  deleteHistory(): Promise<void>;
}

/** Keys the app may never change: file-only by design (PRD §13.2: destructive MCP tools need an
 * explicit line in config.toml and the launch flag). */
export const FILE_ONLY_KEYS: ReadonlySet<string> = new Set(['mcp.destructive']);

const fileOnly = (key: string) =>
  new BridgeError({
    kind: 'safety_blocked',
    message: `${key} can only be set in config.toml (and needs the --allow-destructive launch flag); the app never turns it on.`,
  });

export function createMockSettingsWriter(): SettingsWriter & { calls: string[]; values: Map<string, SettingValue> } {
  const calls: string[] = [];
  const values = new Map<string, SettingValue>();
  return {
    kind: 'mock',
    calls,
    values,
    async save(key, value) {
      if (FILE_ONLY_KEYS.has(key)) throw fileOnly(key);
      calls.push(`save ${key}=${JSON.stringify(value)}`);
      values.set(key, value);
    },
    async connectAgent(client) {
      calls.push(`connect ${client}`);
    },
    async disconnectAgent(client) {
      calls.push(`disconnect ${client}`);
    },
    async exportData() {
      calls.push('export');
    },
    async deleteHistory() {
      calls.push('delete-history');
    },
  };
}

const pending = (what: string) =>
  Promise.reject(
    new BridgeError({
      kind: 'unsupported_platform',
      message: `${what} from the app arrives with the core config command; nothing was changed. Use config.toml or the CLI for now.`,
    }),
  );

export function createPendingSettingsWriter(): SettingsWriter {
  return {
    kind: 'pending',
    save: async (key) => (FILE_ONLY_KEYS.has(key) ? Promise.reject(fileOnly(key)) : pending('Changing settings')),
    connectAgent: async () => pending('Connecting an agent'),
    disconnectAgent: async () => pending('Disconnecting an agent'),
    exportData: async () => pending('Exporting your data'),
    deleteHistory: async () => pending('Deleting history'),
  };
}

export function settingsWriterFor(bridge: ScullBridge): SettingsWriter {
  return bridge.kind === 'mock' ? createMockSettingsWriter() : createPendingSettingsWriter();
}
