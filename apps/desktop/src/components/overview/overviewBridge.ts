// The bridge the Overview reads: the app's, or, with the mock bridge and `?scenario=design`
// in the URL, the design's illustrative machine (designScenario.ts) for design comparison.
import { useBridge, type ScullBridge } from '../../bridge';
import { designScenarioBridge, wantsDesignScenario, wantsSecondDrive } from './designScenario';

const wrapped = new WeakMap<ScullBridge, ScullBridge>();

export function overviewBridge(base: ScullBridge, wantDesign = wantsDesignScenario()): ScullBridge {
  if (base.kind !== 'mock' || !wantDesign) return base;
  let b = wrapped.get(base);
  if (!b) {
    b = designScenarioBridge(base, 300, wantsSecondDrive());
    wrapped.set(base, b);
  }
  return b;
}

export function useOverviewBridge(): ScullBridge {
  return overviewBridge(useBridge());
}
