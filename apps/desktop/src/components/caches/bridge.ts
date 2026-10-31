// The Caches screen's Quick Clean calls that the core bridge does not have yet (P3-UI-08).
//
//   plan(report)       the Quick Clean preview: per tool the resolved binary, the prune and wipe
//                      arguments, and the tools that will be skipped, with reasons (QC-03…05).
//                      Planned bridge command: `quick_clean_plan` (scull-cleaner
//                      `QuickClean::plan`).
//   run(choices, ...)  run the chosen tools one after another, each with its own command and no
//                      shell, revalidating each first; orphaned caches the user confirmed go
//                      through the Executor (D-36). Planned command: `quick_clean_run`.
//
// The desktop never deletes and never starts a process. In a plain browser the mock only
// animates; inside Tauri the pending implementation says Quick Clean isn't available from the
// app yet. These calls stay out of `ScullBridge`, so its read-only surface (SAFE-10) and its
// tests are unchanged.
import type { CachesReport, ScullBridge } from '../../bridge';
import {
  NOT_UNDOABLE,
  TOOL_COMMANDS,
  runDuration,
  toolGroup,
  toolName,
  toolOf,
  type Mode,
  type PlannedCache,
  type QuickCleanPreview,
  type RunResult,
  type SkippedCache,
} from './model';

export interface RunChoice {
  item_id: string;
  mode: Mode;
}

export interface CacheActions {
  readonly kind: 'mock' | 'pending';
  plan(report: CachesReport): Promise<QuickCleanPreview>;
  /** `onProgress(index, fraction)`: the index (in `choices`) of the running tool, 0–1. */
  run(preview: QuickCleanPreview, choices: RunChoice[], onProgress?: (index: number, fraction: number) => void): Promise<RunResult[]>;
}

/** Where the mock machine's tools resolve (trusted directories, SAFE-12). */
export const MOCK_BINARIES: Record<string, string> = {
  npm: '/opt/homebrew/bin/npm',
  pnpm: '/opt/homebrew/bin/pnpm',
  yarn: '/opt/homebrew/bin/yarn',
  uv: '/opt/homebrew/bin/uv',
  go: '/opt/homebrew/bin/go',
  brew: '/opt/homebrew/bin/brew',
  bun: '/Users/ada/.bun/bin/bun',
  docker: '/usr/local/bin/docker',
};

/** The preview a report leads to, given where binaries resolve (`null`: not resolved). */
export function previewFrom(report: CachesReport, binaries: Record<string, string> | null, unavailable: string | null): QuickCleanPreview {
  const tools: PlannedCache[] = [];
  const skipped: SkippedCache[] = [];
  for (const c of report.candidates) {
    const tool = toolOf(c);
    const base = { tool, name: toolName(tool), group: toolGroup(tool) };
    if (c.tier === 'S2' || c.tier === 'S3') {
      skipped.push({ ...base, path: c.path, bytes: c.sizes.reclaimable.bytes, reason: { kind: 'not_eligible', tier: c.tier } });
      continue;
    }
    const cmds = TOOL_COMMANDS[tool] ?? { prune: null, wipe: c.action.type === 'tool_command' ? c.action.args : null };
    const binary = binaries ? (binaries[tool] ?? null) : null;
    const orphan = binaries !== null && binary === null;
    tools.push({
      ...base,
      item_id: c.id,
      cache: c.path,
      bytes: c.sizes.reclaimable.bytes,
      runner: orphan ? 'executor' : 'tool',
      binary,
      prune: orphan ? null : cmds.prune,
      wipe: orphan ? [] : cmds.wipe,
      notes: tool === 'pnpm' ? ['pnpm has no wipe: projects link into its store'] : [],
    });
  }
  for (const r of report.refused) {
    const tool = r.rule.replace(/^cache\./, '');
    skipped.push({ tool, name: toolName(tool), group: toolGroup(tool), path: r.path ?? null, bytes: null, reason: { kind: 'refused', detail: r.reason } });
  }
  return { tools, skipped, unavailable };
}

/** Simulated run: the design's timing (650 ms per tool + 300 ms), nothing touched. */
export function createMockCacheActions(opts: { msPerTool?: number; stepMs?: number } = {}): CacheActions & { runs: RunChoice[][] } {
  const runs: RunChoice[][] = [];
  return {
    kind: 'mock',
    runs,
    async plan(report) {
      return previewFrom(report, MOCK_BINARIES, null);
    },
    async run(preview, choices, onProgress) {
      runs.push(structuredClone(choices));
      const n = choices.length;
      const dur = opts.msPerTool === undefined ? runDuration(n) : opts.msPerTool * n;
      const t0 = Date.now();
      for (;;) {
        const e = dur <= 0 ? 1 : Math.min(1, (Date.now() - t0) / dur);
        const index = Math.min(n - 1, Math.floor(e * n));
        onProgress?.(index, e >= 1 ? 1 : e * n - index);
        if (e >= 1) break;
        await new Promise((r) => setTimeout(r, opts.stepMs ?? 40));
      }
      return choices.map((ch) => {
        const t = preview.tools.find((x) => x.item_id === ch.item_id)!;
        const pnpm = t.tool === 'pnpm';
        return {
          tool: t.tool,
          item_id: t.item_id,
          ok: true,
          freed_bytes: Math.round(t.bytes * (pnpm ? 0.94 : 0.98)),
          explanation: pnpm ? 'Projects still link some packages from the store, so prune kept them (QC-06).' : null,
          error: null,
        };
      });
    },
  };
}

export const PENDING_REASON =
  'Quick Clean from the app arrives with the quick_clean_plan and quick_clean_run bridge commands. `scull clean caches` works now.';

export function createPendingCacheActions(): CacheActions {
  return {
    kind: 'pending',
    async plan(report) {
      return previewFrom(report, null, PENDING_REASON);
    },
    async run() {
      throw new Error(PENDING_REASON);
    },
  };
}

export function cacheActionsFor(bridge: ScullBridge): CacheActions {
  return bridge.kind === 'mock' ? createMockCacheActions() : createPendingCacheActions();
}

const GiB = 1024 ** 3;
const g = (x: number) => Math.round(x * GiB);

/**
 * The design's machine (design 08), as a preview, plus the skipped states the issue asks for:
 * a Windows `.cmd` shim (#218), a cache in use, and an orphaned cache left for manual removal.
 * Used only with the mock bridge and `#/caches?demo=design`; the recorded fixture has a single
 * npm cache, classified S2 (in-use probes are not available in that build), so it is skipped.
 */
export function designPreview(): { preview: QuickCleanPreview; preselected: string[] } {
  const t = (
    tool: string,
    cache: string,
    gb: number,
    runner: 'tool' | 'executor' = 'tool',
    name = toolName(tool),
    group = toolGroup(tool),
  ): PlannedCache => ({
    tool,
    name,
    group,
    item_id: `demo-${tool}`,
    cache,
    bytes: g(gb),
    runner,
    binary: runner === 'tool' ? (MOCK_BINARIES[tool] ?? null) : null,
    prune: runner === 'tool' ? (TOOL_COMMANDS[tool]?.prune ?? null) : null,
    wipe: runner === 'tool' ? (TOOL_COMMANDS[tool]?.wipe ?? null) : [],
    notes: [],
  });
  return {
    preselected: ['demo-npm', 'demo-pnpm', 'demo-bun', 'demo-yarn'],
    preview: {
      unavailable: null,
      tools: [
        t('npm', '/Users/ada/.npm', 6.1),
        t('pnpm', '/Users/ada/Library/pnpm/store', 4.8),
        t('uv', '/Users/ada/.cache/uv', 3.1),
        t('bun', '/Users/ada/.bun/install/cache', 2.6),
        t('yarn', '/Users/ada/Library/Caches/Yarn', 1.4),
        t('go', '/Users/ada/go/pkg/mod', 1.8),
        t('docker', '/Users/ada/Library/Containers/com.docker.docker', 18.0),
        t('brew', '/Users/ada/Library/Caches/Homebrew', 2.3),
        t('playwright', '/Users/ada/Library/Caches/ms-playwright', 1.9, 'executor'),
        t('cypress', '/Users/ada/Library/Caches/Cypress', 0.8, 'executor'),
      ],
      skipped: [
        {
          tool: 'pnpm',
          name: 'pnpm store (Windows)',
          group: 'Package managers',
          path: 'C:\\Users\\ada\\AppData\\Local\\pnpm\\store',
          bytes: g(1.2),
          reason: { kind: 'shell_shim_only', shim: 'C:\\Users\\ada\\AppData\\Roaming\\npm\\pnpm.cmd' },
        },
        {
          tool: 'huggingface',
          name: 'Hugging Face',
          group: 'Models',
          path: '/Users/ada/.cache/huggingface',
          bytes: g(7.4),
          reason: { kind: 'in_use', by: ['python3 (pid 4121) has a file open'] },
        },
        {
          tool: 'gradle',
          name: 'Gradle',
          group: 'Package managers',
          path: '/Users/ada/.gradle/caches',
          bytes: g(0.6),
          reason: { kind: 'not_installed', manual_removal: '~/.gradle/caches' },
        },
      ],
    },
  };
}

export { NOT_UNDOABLE };
