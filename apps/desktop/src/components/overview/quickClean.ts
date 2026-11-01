// Quick Clean on the Overview (design 01 / 01c; D-20): pick package caches, confirm, watch the
// chips pop one by one while the total counts down, then a burst and the freed figure.
//
// THE DESKTOP NEVER DELETES (yet). There is no bridge method that cleans anything (the bridge
// is read-only by design, SAFE-10). `quickCleanPreview` is the stub the button calls:
//   - with the mock bridge it resolves a SIMULATED result (nothing exists to delete; the UI
//     marks the result "simulated");
//   - with the Tauri bridge it rejects with QUICK_CLEAN_UNAVAILABLE, so the real app can never
//     show a success that did not happen.
// When the core grows a reviewed Quick Clean command (P2-QC-01 runner behind a plan), replace
// the stub with that bridge call and keep the animation.
import type { Candidate, ScullBridge } from '../../bridge';
import { formatBytes } from '../../lib/format';

/** Cleaning animation length (design: 2600 ms). */
export const CLEAN_MS = 2600;

export interface QuickCleanTool {
  id: string;
  /** "npm", "pnpm", "Bun", "Yarn". */
  label: string;
  bytes: number;
  candidate: Candidate;
}

const NAMES: Record<string, string> = {
  npm: 'npm',
  pnpm: 'pnpm',
  yarn: 'Yarn',
  bun: 'Bun',
  pip: 'pip',
  uv: 'uv',
  cargo: 'Cargo',
  go: 'Go',
  gradle: 'Gradle',
  maven: 'Maven',
  composer: 'Composer',
  nuget: 'NuGet',
  cocoapods: 'CocoaPods',
  homebrew: 'Homebrew',
  conda: 'conda',
  deno: 'Deno',
};

/** Short chip name: from the rule ("npm.cache" -> "npm"), else the artifact without " cache". */
export function toolLabel(c: Candidate): string {
  const key = c.rule.id.split('.')[0]!.toLowerCase();
  return NAMES[key] ?? c.artifact.replace(/\s+(cache|store)$/i, '');
}

/** Quick Clean candidates: package caches, largest first. Protected caches are never offered. */
export function quickCleanTools(cands: readonly Candidate[]): QuickCleanTool[] {
  return cands
    .filter((c) => c.category === 'package_cache' && c.tier !== 'S3')
    .map((c) => ({ id: c.id, label: toolLabel(c), bytes: c.sizes.reclaimable.bytes, candidate: c }))
    .sort((a, b) => b.bytes - a.bytes);
}

export type ChipState = 'off' | 'on' | 'active' | 'done';
export type Phase = 'idle' | 'confirm' | 'cleaning' | 'done' | 'unavailable';

export interface QuickCleanFrame {
  chips: { tool: QuickCleanTool; state: ChipState; label: string }[];
  /** Bytes freed so far (animated). */
  freed: number;
  /** Bytes still shown in the header figure (counts down). */
  remaining: number;
  /** Chosen total. */
  total: number;
  chosen: number;
  button: string;
  /** Progress fill, 0..100. */
  fill: number;
  cleaned: string[];
}

export const easeOutCubic = (x: number) => 1 - Math.pow(1 - x, 3);

/**
 * One frame of the Quick Clean card, as in the design: chips finish in order, each over an
 * equal share of the eased progress `p` (0..1 during `cleaning`).
 */
export function quickCleanFrame(tools: readonly QuickCleanTool[], off: ReadonlySet<string>, phase: Phase, p: number): QuickCleanFrame {
  const e = phase === 'cleaning' ? easeOutCubic(Math.min(1, Math.max(0, p))) : phase === 'done' ? 1 : 0;
  const chosen = tools.filter((t) => !off.has(t.id));
  const n = chosen.length;
  const total = chosen.reduce((a, t) => a + t.bytes, 0);
  const freed = total * e;
  let active = '';
  const cleaned: string[] = [];
  const chips = tools.map((tool) => {
    const on = !off.has(tool.id);
    const k = chosen.indexOf(tool);
    let state: ChipState = on ? 'on' : 'off';
    if (on && (phase === 'cleaning' || phase === 'done')) {
      if (phase === 'done' || e >= (k + 1) / n - 0.001) {
        state = 'done';
        cleaned.push(tool.id);
      } else if (e >= k / n) {
        state = 'active';
        active = tool.label;
      }
    }
    return { tool, state, label: state === 'done' ? `${tool.label} ✓` : `${tool.label} · ${formatBytes(tool.bytes)}` };
  });
  let button = n === 0 ? 'Pick caches to clean' : `Clean ${n} ${n === 1 ? 'cache' : 'caches'}`;
  if (phase === 'confirm') button = `Clean ${formatBytes(total)} now`;
  if (phase === 'cleaning') button = `Cleaning ${active}…`;
  if (phase === 'done') button = 'Done';
  return { chips, freed, remaining: total - freed, total, chosen: n, button, fill: Math.round(e * 100), cleaned };
}

export interface QuickCleanResult {
  /** Bytes the cleaned caches held. */
  freed: number;
  /** Measured change in free space. */
  measured: number;
  /** True when nothing really ran (mock bridge). */
  simulated: boolean;
}

export const QUICK_CLEAN_UNAVAILABLE =
  'Quick Clean is not available in the desktop app yet: the app never deletes until the reviewed cleanup engine lands. Nothing was deleted.';

/**
 * STUB (see the header comment). Never deletes, moves or modifies anything, in any bridge.
 * Mock: a simulated result (measured = 98% of the expected bytes, as in the design).
 * Tauri: rejects with QUICK_CLEAN_UNAVAILABLE.
 */
export async function quickCleanPreview(bridge: ScullBridge, tools: readonly QuickCleanTool[]): Promise<QuickCleanResult> {
  if (bridge.kind !== 'mock') throw new Error(QUICK_CLEAN_UNAVAILABLE);
  const freed = tools.reduce((a, t) => a + t.bytes, 0);
  return { freed, measured: Math.round(freed * 0.98), simulated: true };
}

/** Particles of the burst (design: 20, five colors, keyframes scull-b0..b9). */
export const PARTICLES = Array.from({ length: 20 }, (_, i) => ({
  size: [8, 10, 12, 9][i % 4]!,
  radius: i % 3 === 0 ? '3px' : '50%',
  color: [
    'var(--color-accent)',
    'var(--color-on-fill)',
    'var(--color-violet-light)',
    'var(--color-tier-s2)',
    'var(--color-tier-s1)',
  ][i % 5]!,
  animation: `scull-b${i % 10} ${700 + (i % 5) * 90}ms ${i >= 10 ? 120 : 0}ms cubic-bezier(.15,.7,.3,1) both`,
}));
