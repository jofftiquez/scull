import type { ScanParams } from '../../bridge';
import { AGENTS, BUILDS, CAUTION, PRIORITIES, broadRootConcern, checkRoot, scanRoots, type Answers } from './model';

export interface SavedSetup {
  version: 1;
  answers: Answers;
  home: string;
}

type Store = Pick<Storage, 'getItem' | 'setItem'>;
const KEY = 'scull.onboarding.setup';

function storage(): Store | null {
  try {
    return globalThis.localStorage ?? null;
  } catch {
    return null;
  }
}

function validate(raw: unknown): SavedSetup | null {
  if (!raw || typeof raw !== 'object') return null;
  const s = raw as Record<string, unknown>;
  if (s.version !== 1 || typeof s.home !== 'string' || !s.home || !checkRoot(s.home, '').ok) return null;
  if (!s.answers || typeof s.answers !== 'object') return null;
  const a = s.answers as Record<string, unknown>;
  const paths = (v: unknown): v is string[] =>
    Array.isArray(v) && v.every((p: unknown) => typeof p === 'string' && checkRoot(p, '').ok);
  if (
    !Array.isArray(a.builds) || !a.builds.length || !a.builds.every((v: unknown) => BUILDS.some((b) => b.id === v)) ||
    !paths(a.roots) || !paths(a.confirmedBroad) || a.connectAgents !== false ||
    !AGENTS.some((v) => v.id === a.agents) || !PRIORITIES.some((v) => v.id === a.priority) || !CAUTION.some((v) => v.id === a.caution)
  ) return null;
  const answers = a as unknown as Answers;
  return {
    version: 1,
    home: s.home,
    answers: {
      builds: [...new Set(answers.builds)],
      roots: [...new Set(answers.roots)],
      confirmedBroad: [...new Set(answers.confirmedBroad)].filter((p) => answers.roots.includes(p) && broadRootConcern(p, s.home as string) !== null),
      agents: answers.agents,
      connectAgents: false,
      priority: answers.priority,
      caution: answers.caution,
    },
  };
}

export function loadSetup(store: Store | null = storage()): SavedSetup | null {
  try {
    const raw = store?.getItem(KEY);
    return raw ? validate(JSON.parse(raw)) : null;
  } catch {
    return null;
  }
}

export function saveSetup(setup: SavedSetup, store: Store | null = storage()): { saved: boolean; reason?: string } {
  try {
    const valid = validate(setup);
    if (!valid || !store) throw new Error('Setup cannot be remembered on this device.');
    store.setItem(KEY, JSON.stringify(valid));
    return { saved: true };
  } catch {
    return { saved: false, reason: 'Your choices could not be remembered. Try again, or scan once without saving.' };
  }
}

export function savedScanParams(setup: SavedSetup | null): ScanParams | null {
  const valid = validate(setup);
  if (!valid) return null;
  const roots = scanRoots(valid.answers, valid.home);
  return roots.length ? { roots, allow_broad_roots: roots.some((r) => valid.answers.confirmedBroad.includes(r)) } : null;
}
