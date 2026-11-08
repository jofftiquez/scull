// The native Caches screen uses the core planner and audited runner. Only the browser
// fixture simulates cleaning; the webview never starts processes or deletes files.
import { toRaw } from 'vue';
import type { CachesReport, ScullBridge } from '../../bridge';
import type { DesktopCachePreview, DesktopQuickClean } from '../../bridge/generated';
import { realTransport, type TauriTransport } from '../../bridge/tauri';
import { runCacheCommand, type CacheProgressHandler } from '../../bridge/cacheProgress';
import { CLEANUP_COMMANDS } from '../../bridge/tauriPlans';
import { toBridgeError } from '../../bridge/types';
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
  readonly kind: 'mock' | 'tauri';
  plan(report?: CachesReport): Promise<QuickCleanPreview>;
  /** `onProgress(index, fraction)`: the index (in `choices`) of the running tool, 0–1. */
  run(preview: QuickCleanPreview, choices: RunChoice[], onProgress?: (index: number, fraction: number) => void, onEvent?: CacheProgressHandler): Promise<RunResult[]>;
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
      if (!report) throw new Error('A cache report is required for the browser preview.');
      return previewFrom(report, MOCK_BINARIES, null);
    },
    async run(preview, choices, onProgress, onEvent) {
      runs.push(structuredClone(choices));
      const n = choices.length;
      const dur = opts.msPerTool === undefined ? runDuration(n) : opts.msPerTool * n;
      const t0 = Date.now();
      const opId = 'mock-cache-run';
      let last = -1;
      onEvent?.({ op_id: opId, kind: 'preparing', completed: 0, total: n });
      for (;;) {
        const e = dur <= 0 ? 1 : Math.min(1, (Date.now() - t0) / dur);
        const index = Math.min(n - 1, Math.floor(e * n));
        while (last < index) {
          if (last >= 0) { const previous = preview.tools.find((t) => t.item_id === choices[last]?.item_id)!; onEvent?.({ op_id: opId, kind: 'finished', item_id: previous.item_id, tool: previous.tool, succeeded: true, completed: last + 1, total: n }); }
          last++;
          const tool = preview.tools.find((t) => t.item_id === choices[last]?.item_id)!;
          onEvent?.({ op_id: opId, kind: 'started', item_id: tool.item_id, tool: tool.tool, command: `${tool.binary ?? tool.tool} ${(choices[last]?.mode === 'prune' ? tool.prune : tool.wipe)?.join(' ') ?? ''}`, completed: last, total: n });
          onEvent?.({ op_id: opId, kind: 'output', item_id: tool.item_id, tool: tool.tool, stream: 'stdout', line: 'Simulated cleanup output (browser preview).', completed: last, total: n });
        }
        onProgress?.(index, e >= 1 ? 1 : e * n - index);
        if (e >= 1) break;
        await new Promise((r) => setTimeout(r, opts.stepMs ?? 40));
      }
      const final = preview.tools.find((t) => t.item_id === choices.at(-1)?.item_id);
      if (final) onEvent?.({ op_id: opId, kind: 'finished', item_id: final.item_id, tool: final.tool, succeeded: true, completed: n, total: n });
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

/** Native preview commands are kept with their displayed preview for exact mode validation. */
export function createTauriCacheActions(t: TauriTransport = realTransport): CacheActions {
  const plans = new WeakMap<QuickCleanPreview, DesktopCachePreview>();
  const call = async <T>(command: string, args?: Record<string, unknown>): Promise<T> => {
    try {
      return await t.invoke<T>(command, args);
    } catch (error) {
      throw toBridgeError(error);
    }
  };
  return {
    kind: 'tauri',
    async plan() {
      const native = await call<DesktopCachePreview>(CLEANUP_COMMANDS.quickCleanPlan);
      const preview: QuickCleanPreview = {
        unavailable: null,
        tools: native.tools.map((tool) => ({
          tool: tool.tool,
          name: tool.name,
          group: toolGroup(tool.tool),
          item_id: tool.item_id,
          cache: tool.cache,
          bytes: tool.bytes,
          runner: 'tool',
          binary: (tool.prune ?? tool.wipe)?.binary ?? null,
          prune: tool.prune?.args ?? null,
          wipe: tool.wipe?.args ?? null,
          commands: { prune: tool.prune, wipe: tool.wipe },
          notes: tool.notes,
        })),
        skipped: native.skipped.map((skip) => ({
          tool: skip.tool,
          name: skip.name,
          group: toolGroup(skip.tool),
          path: skip.path ?? null,
          bytes: skip.bytes ?? null,
          reason: { kind: 'native', detail: skip.reason },
        })),
      };
      plans.set(preview, native);
      return preview;
    },
    async run(preview, choices, onProgress, onEvent) {
      const native = plans.get(toRaw(preview));
      if (!native) throw new Error('Refresh the cache preview before cleaning.');
      if (choices.length === 0) return [];
      const request = choices.map((choice) => {
        const tool = native.tools.find((tool) => tool.item_id === choice.item_id);
        const command = tool?.[choice.mode];
        if (!command) throw new Error('This cache mode is unavailable. Refresh the preview.');
        return { ...choice, command };
      });
      onProgress?.(0, 0);
      const result = await runCacheCommand<DesktopQuickClean>(CLEANUP_COMMANDS.quickCleanRun, { choices: request }, onEvent, t);
      onProgress?.(choices.length - 1, 1);
      return choices.map((choice) => {
        const tool = native.tools.find((tool) => tool.item_id === choice.item_id)!;
        const ran = result.tools.find((ran) => ran.item_id === choice.item_id);
        const skipped = result.skipped.find((skip) => skip.tool === tool.tool);
        return {
          tool: tool.tool,
          item_id: choice.item_id,
          ok: ran?.succeeded ?? false,
          freed_bytes: Math.max(0, ran?.cache_bytes_freed ?? 0),
          explanation: ran?.explanation ?? (ran?.cache_bytes_freed == null && ran?.succeeded ? 'The command succeeded; the freed size could not be measured.' : null),
          error: ran?.succeeded ? null : (skipped?.reason ?? ran?.explanation ?? 'The tool did not finish successfully. See History for details.'),
        };
      });
    },
  };
}

export function cacheActionsFor(bridge: ScullBridge): CacheActions {
  return bridge.kind === 'mock' ? createMockCacheActions() : createTauriCacheActions();
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
