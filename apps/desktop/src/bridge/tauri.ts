// The real bridge: Tauri commands implemented in apps/desktop/src-tauri (thin wrappers over
// scull-desktop-bridge, which calls the read-only scull crates). The only module allowed to
// import @tauri-apps/api (enforced by eslint `no-restricted-imports`).
import { invoke } from '@tauri-apps/api/core';
import type { AppInfo, Drive, ScullBridge } from './types';

export function createTauriBridge(): ScullBridge {
  return {
    kind: 'tauri',
    appInfo: () => invoke<AppInfo>('app_info'),
    drives: () => invoke<Drive[]>('drives'),
  };
}
