// Quick Clean on the Overview (design 01 / 01c; D-20): pick package caches, confirm, watch the
// chips pop one by one while the total counts down, then a burst and the freed figure.
//
// `quickCleanPreview` runs it after the card's confirmation:
//   - with the mock bridge it resolves a SIMULATED result (nothing exists to delete; the UI
//     marks the result "simulated");
//   - inside Tauri it calls `quick_clean` (P3-EXEC-01): the core runs each ticked tool's own
//     cleanup command (trusted locations, private sandbox) as one audited operation. The webview
//     never touches a file (SAFE-10). Only the main window may call it; the menu bar widget
//     opens the main window instead.
import { toBridgeError, type Candidate, type ScullBridge } from '../../bridge';
import type { DesktopQuickClean } from '../../bridge/generated';
import { runCacheCommand, type CacheProgressHandler } from '../../bridge/cacheProgress';
import { realTransport, type TauriTransport } from '../../bridge/tauri';
import { CLEANUP_COMMANDS } from '../../bridge/tauriPlans';
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
  cleaned?: string[];
  issues?: string[];
  /** Bytes the cleaned caches held. */
  freed: number;
  /** Measured change in free space. */
  measured: number;
  /** True when nothing really ran (mock bridge). */
  simulated: boolean;
}

/**
 * Run Quick Clean on the ticked chips, after the card's confirmation.
 * Mock: a simulated result (measured = 98% of the expected bytes, as in the design); touches
 * nothing. Tauri: `quick_clean` (P3-EXEC-01, main window only): each tool's own command, found
 * only in trusted locations and run in a private sandbox, as one audited operation.
 */
export async function quickCleanPreview(
  bridge: ScullBridge,
  tools: readonly QuickCleanTool[],
  t: TauriTransport = realTransport,
  onProgress?: CacheProgressHandler,
): Promise<QuickCleanResult> {
  if (bridge.kind === 'mock') {
    const freed = tools.reduce((a, x) => a + x.bytes, 0);
    return { freed, measured: Math.round(freed * 0.98), simulated: true, cleaned: tools.map((t) => t.id) };
  }
  let r: DesktopQuickClean;
  try {
    r = await runCacheCommand<DesktopQuickClean>(CLEANUP_COMMANDS.quickClean, { items: tools.map((x) => x.id) }, onProgress, t);
  } catch (e) {
    throw toBridgeError(e);
  }
  if (!r.operation_id) {
    const why = r.skipped.map((s) => `${s.tool}: ${s.reason}`).join('; ');
    throw new Error(`Nothing was run${why ? ` (${why})` : ''}. Nothing was deleted.`);
  }
  const freed = r.tools.reduce((a, x) => a + Math.max(0, x.cache_bytes_freed ?? 0), 0);
  const issues = [...r.skipped.map((s) => `${s.tool}: ${s.reason}`), ...r.tools.filter((t) => !t.succeeded).map((t) => `${t.tool}: ${t.explanation ?? 'command failed'}`)];
  if (r.tools.length + r.skipped.length !== tools.length) issues.push('Not every selected cache reported a result. Check History.');
  return { freed, measured: Math.max(0, r.measured_delta_bytes ?? freed), simulated: false,
    cleaned: r.tools.filter((t) => t.succeeded && t.item_id && (t.cache_bytes_freed ?? 0) >= (tools.find((x) => x.id === t.item_id)?.bytes ?? Infinity)).map((t) => t.item_id!),
    ...(issues.length ? { issues } : {}),
  };
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
