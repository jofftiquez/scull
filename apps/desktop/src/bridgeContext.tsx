// The bridges the app talks to, provided once at the root: the core's (scan, caches, settings)
// and the plan bridge (prepare/execute). Tauri's inside the app, the mock in a browser (E2E).
import { createContext, useContext } from 'react';
import type { ReactNode } from 'react';
import type { ScullBridge } from './bridge';
import type { PlanBridge } from './bridge/plan';

export interface Bridges {
  core: ScullBridge;
  plans: PlanBridge;
}

const BridgeContext = createContext<Bridges | null>(null);

export function BridgeProvider({ value, children }: { value: Bridges; children: ReactNode }) {
  return <BridgeContext.Provider value={value}>{children}</BridgeContext.Provider>;
}

export function useBridges(): Bridges {
  const b = useContext(BridgeContext);
  if (!b) throw new Error('no bridges provided');
  return b;
}
