// The Settings screen's writes, which the core bridge does not have (P3-UI-11).
//
//   save(key, value)       change one setting. Config keys (`scan.roots`, `mcp.redact_home`, ...)
//                          are written to config.toml by a validated native command (the same
//                          validation as the CLI); `app.*` keys are app preferences (schedule,
//                          menu bar, alerts, ...) that have no config key yet.
//   connectAgent / disconnectAgent   MCP client registration (`scull agent install`, §17).
//   exportData / deleteHistory       `scull data export` / `scull data purge`.
//
// The desktop never writes files itself. In a plain browser the mock keeps changes in memory
// (it touches nothing); inside Tauri supported config keys reach the core/platform writer.
// Unsupported app preferences and actions still reject, so the screen reverts the control.
// This stays out of `ScullBridge`, whose read-only surface is unchanged.
import { BridgeError, toBridgeError, type ScullBridge } from '../../bridge';
import { realTransport, type TauriTransport } from '../../bridge/tauri';

export type SettingValue = boolean | number | string | string[];

export interface SettingsWriter {
  readonly kind: 'mock' | 'pending' | 'native';
  save(key: string, value: SettingValue): Promise<void>;
  connectAgent(client: string): Promise<void>;
  disconnectAgent(client: string): Promise<void>;
  exportData(): Promise<void>;
  deleteHistory(): Promise<void>;
}

/** Keys the app may never change: file-only by design (PRD §13.2: destructive MCP tools need an
 * explicit line in config.toml and the launch flag). */
export const FILE_ONLY_KEYS: ReadonlySet<string> = new Set(['mcp.destructive']);

export const SETTINGS_COMMANDS = {
  save: 'save_setting',
} as const;

/** Existing configuration controls with a native consumer. App preferences use their own controls. */
export const EDITABLE_CONFIG_KEYS: ReadonlySet<string> = new Set([
  'scan.roots',
  'scan.protect',
  'scan.activity_window',
  'mcp.enabled',
  'mcp.redact_home',
  'mcp.hash_names',
]);

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
      message: `${what} is not available in the app yet; nothing was changed.`,
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
  return bridge.kind === 'mock' ? createMockSettingsWriter() : createNativeSettingsWriter();
}

export function createNativeSettingsWriter(t: TauriTransport = realTransport): SettingsWriter {
  return {
    ...createPendingSettingsWriter(),
    kind: 'native',
    async save(key, value) {
      if (FILE_ONLY_KEYS.has(key)) throw fileOnly(key);
      if (!EDITABLE_CONFIG_KEYS.has(key)) return pending(`Changing ${key}`);
      try {
        await t.invoke(SETTINGS_COMMANDS.save, { key, value });
      } catch (e) {
        throw toBridgeError(e);
      }
    },
  };
}
