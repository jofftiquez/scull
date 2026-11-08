// Caches screen (P3-UI-08; design 08, PRD §10.8 QC-01…08): pure logic. The Quick Clean
// preview's shape mirrors scull-cleaner's `QuickCleanPlan` (PlannedTool / SkippedTool /
// SkipReason), so the planned bridge command can return it unchanged.
import type { Candidate, Tier } from '../../bridge';
import type { CacheCleanCommand } from '../../bridge/generated';

/** scull-detectors' NOT_UNDOABLE_NOTE (shown with every Quick Clean, QC-04). */
export const NOT_UNDOABLE = 'Not undoable. Packages re-download on next install; offline installs may fail until then.';

export type Mode = 'prune' | 'wipe';
export type Group = 'Package managers' | 'Models' | 'Containers & system' | 'Test browsers';
export const GROUPS: readonly Group[] = ['Package managers', 'Models', 'Containers & system', 'Test browsers'];

/** Why a tool will not run (scull-cleaner `SkipReason`). */
export type SkipReason =
  | { kind: 'native'; detail: string }
  | { kind: 'refused'; detail: string }
  | { kind: 'not_eligible'; tier: Tier }
  | { kind: 'in_use'; by: string[] }
  | { kind: 'activity_unknown'; why: string[] }
  | { kind: 'link'; path: string }
  | { kind: 'other_volume'; anchor: string }
  | { kind: 'unrecognized' }
  /** No trusted binary and orphan removal not allowed: left for manual removal. */
  | { kind: 'not_installed'; manual_removal: string }
  /** Only a `.cmd` / `.bat` / `.ps1` shim: needs a shell, which scull never uses (SAFE-12, #218). */
  | { kind: 'shell_shim_only'; shim: string }
  | { kind: 'wrong_version'; reported: string }
  | { kind: 'elevated' };

/** The reason as a sentence (same wording as scull-cleaner's `Display`). */
export function skipText(r: SkipReason): string {
  switch (r.kind) {
    case 'native':
      return r.detail;
    case 'refused':
      return `location refused: ${r.detail}`;
    case 'not_eligible':
      return `classified ${r.tier}: tool commands are allowed at S0/S1 only`;
    case 'in_use':
      return `in use (${r.by.join('; ')})`;
    case 'activity_unknown':
      return `could not check whether it is in use (${r.why.join('; ')})`;
    case 'link':
      return `${r.path} is a symlink or junction: its target can't be verified`;
    case 'other_volume':
      return `on a different volume than ${r.anchor}`;
    case 'unrecognized':
      return "contents don't match the tool's cache layout";
    case 'not_installed':
      return `tool not installed (no trusted binary); ${r.manual_removal} left in place for manual removal`;
    case 'shell_shim_only':
      return `only ${r.shim} found, which needs a shell to run (Windows shims: #218)`;
    case 'wrong_version':
      return `binary version doesn't match this cache: ${r.reported}`;
    case 'elevated':
      return 'refusing to run tools while elevated (SAFE-14)';
  }
}

/** A cache Quick Clean can clean (scull-cleaner `PlannedTool`). */
export interface PlannedCache {
  tool: string;
  name: string;
  group: Group;
  item_id: string;
  /** The cache folder. */
  cache: string;
  bytes: number;
  /**
   * `tool`: the tool's own command, run from its resolved binary without a shell (QC-03).
   * `executor`: an orphaned cache (no trusted binary) that scull removes itself through the
   * Executor (D-36, provisional): needs a per-tool confirmation.
   */
  runner: 'tool' | 'executor';
  /** Resolved binary: canonical absolute path from a trusted directory (SAFE-12). */
  binary: string | null;
  /** Exact native commands for each mode, including the scoped environment. */
  commands?: Partial<Record<Mode, CacheCleanCommand | null>>;
  /** Arguments of the prune and the wipe; `null` when the tool has none. */
  prune: string[] | null;
  wipe: string[] | null;
  notes: string[];
}

/** A cache that won't be cleaned (scull-cleaner `SkippedTool`). */
export interface SkippedCache {
  tool: string;
  name: string;
  group: Group;
  path: string | null;
  bytes: number | null;
  reason: SkipReason;
}

/** The Quick Clean preview (scull-cleaner `QuickCleanPlan`). */
export interface QuickCleanPreview {
  tools: PlannedCache[];
  skipped: SkippedCache[];
  /** Set when planning is unavailable. */
  unavailable: string | null;
}

const TOOL_NAMES: Record<string, [string, Group]> = {
  npm: ['npm', 'Package managers'],
  pnpm: ['pnpm store', 'Package managers'],
  yarn: ['Yarn', 'Package managers'],
  bun: ['Bun', 'Package managers'],
  uv: ['uv', 'Package managers'],
  pip: ['pip', 'Package managers'],
  go: ['Go modules', 'Package managers'],
  cargo: ['Cargo registry', 'Package managers'],
  gradle: ['Gradle', 'Package managers'],
  maven: ['Maven', 'Package managers'],
  nuget: ['NuGet', 'Package managers'],
  huggingface: ['Hugging Face', 'Models'],
  ollama: ['Ollama models', 'Models'],
  docker: ['Docker build cache', 'Containers & system'],
  brew: ['Homebrew', 'Containers & system'],
  homebrew: ['Homebrew', 'Containers & system'],
  playwright: ['Playwright browsers', 'Test browsers'],
  cypress: ['Cypress binaries', 'Test browsers'],
};

/** Tool ID of a cache candidate: its tool command's tool, else the rule's prefix (`npm.cache`). */
export function toolOf(c: Pick<Candidate, 'action' | 'rule'>): string {
  if (c.action.type === 'tool_command') return c.action.tool;
  const id = c.rule.id;
  return (id.startsWith('cache.') ? id.slice(6) : id.split('.')[0]) ?? id;
}

export function toolName(tool: string): string {
  return TOOL_NAMES[tool]?.[0] ?? tool;
}

export function toolGroup(tool: string): Group {
  return TOOL_NAMES[tool]?.[1] ?? 'Package managers';
}

/** Each tool's prune and wipe (PRD QC-03 table). pnpm has no wipe: projects link into its store. */
export const TOOL_COMMANDS: Record<string, { prune: string[] | null; wipe: string[] | null }> = {
  npm: { prune: ['cache', 'verify'], wipe: ['cache', 'clean', '--force'] },
  pnpm: { prune: ['store', 'prune'], wipe: null },
  bun: { prune: null, wipe: ['pm', 'cache', 'rm'] },
  yarn: { prune: null, wipe: ['cache', 'clean'] },
  uv: { prune: ['cache', 'prune'], wipe: ['cache', 'clean'] },
  pip: { prune: null, wipe: ['cache', 'purge'] },
  go: { prune: null, wipe: ['clean', '-modcache'] },
  brew: { prune: ['cleanup'], wipe: ['cleanup', '--prune=all'] },
  docker: { prune: ['builder', 'prune'], wipe: ['builder', 'prune', '--all'] },
};

/** Prune when the tool has one (the safer default, QC-03), else wipe. */
export function defaultMode(p: Pick<PlannedCache, 'prune' | 'wipe'>): Mode {
  return p.prune ? 'prune' : 'wipe';
}

/** What the row shows as its command: the resolved binary and the arguments, or the executor. */
export function commandLine(p: PlannedCache, mode: Mode): string {
  if (p.runner === 'executor') return 'removed safely by scull';
  const command = p.commands?.[mode];
  if (command) return [...command.env.map(({ name, value }) => `${name}=${value}`), command.binary, ...command.args].join(' ');
  const args = (mode === 'prune' ? (p.prune ?? p.wipe) : (p.wipe ?? p.prune)) ?? [];
  return [p.binary ?? p.tool, ...args].join(' ');
}

/** `~/.npm`, `~\AppData\…`: home folders shortened. */
export function shortPath(path: string): string {
  return path.replace(/^\/(?:Users|home)\/[^/]+(?=\/|$)/, '~').replace(/^[A-Za-z]:\\Users\\[^\\]+(?=\\|$)/, '~');
}

// ---- Selection ----

export interface Choice {
  on: boolean;
  mode: Mode;
}

/**
 * Initial choices: every tool-run cache ticked with its default mode. Orphaned caches
 * (executor) start unticked: removing one needs a per-tool confirmation (D-36).
 */
export function initialChoices(tools: readonly PlannedCache[]): Record<string, Choice> {
  return Object.fromEntries(tools.map((t) => [t.item_id, { on: t.runner === 'tool', mode: defaultMode(t) }]));
}

export function selected(tools: readonly PlannedCache[], choices: Record<string, Choice>): PlannedCache[] {
  return tools.filter((t) => choices[t.item_id]?.on);
}

export function selectedBytes(tools: readonly PlannedCache[], choices: Record<string, Choice>): number {
  return selected(tools, choices).reduce((a, t) => a + t.bytes, 0);
}

export function groupTotals(
  tools: readonly PlannedCache[],
  skipped: readonly SkippedCache[],
): { group: Group; bytes: number; count: number }[] {
  return GROUPS.map((group) => {
    const t = tools.filter((x) => x.group === group);
    const s = skipped.filter((x) => x.group === group);
    return {
      group,
      bytes: t.reduce((a, x) => a + x.bytes, 0) + s.reduce((a, x) => a + (x.bytes ?? 0), 0),
      count: t.length + s.length,
    };
  }).filter((g) => g.count > 0);
}

// ---- Sequential run (state machine) ----

export type RunPhase = 'idle' | 'running' | 'done';
export type RowState = 'idle' | 'queued' | 'active' | 'done' | 'failed';

/** The design runs tools one after another: 650 ms each plus 300 ms (Caches.dc.html). */
export function runDuration(n: number): number {
  return 650 * n + 300;
}

/**
 * Row state and progress (0–100) of the k-th of n selected tools when the whole run is at
 * `e` (0–1): earlier tools are done, the current one fills, later ones wait.
 */
export function rowProgress(k: number, n: number, e: number): { state: RowState; pct: number } {
  if (k < 0 || n === 0) return { state: 'idle', pct: 0 };
  if (e >= (k + 1) / n) return { state: 'done', pct: 100 };
  if (e >= k / n) return { state: 'active', pct: Math.round((e * n - k) * 100) };
  return { state: 'queued', pct: 0 };
}

/** Run progress as the tools report it: index of the running tool and its own fraction. */
export function overall(index: number, fraction: number, n: number): number {
  return n === 0 ? 1 : Math.min(1, (index + Math.min(1, Math.max(0, fraction))) / n);
}

export interface RunResult {
  tool: string;
  item_id: string;
  ok: boolean;
  /** Bytes the cache shrank by (measured). */
  freed_bytes: number;
  /** pnpm: why a prune freed less (QC-06). */
  explanation: string | null;
  error: string | null;
}

/** "+X freed from N caches · measured Y". `measured` is the freed bytes the tools reported. */
export function runSummary(results: readonly RunResult[], planned: number): { count: number; planned: number; measured: number } {
  const ok = results.filter((r) => r.ok);
  return { count: ok.length, planned, measured: ok.reduce((a, r) => a + r.freed_bytes, 0) };
}

/** Burst particles (design 08b): 20 pieces in the duotone, staggered. */
export function particles(): { size: number; round: boolean; color: string; anim: string }[] {
  const colors = ['var(--color-accent)', 'var(--color-on-ink)', 'var(--color-violet-light)', 'var(--color-tier-s2)', 'var(--color-tier-s1)'];
  return Array.from({ length: 20 }, (_, i) => ({
    size: [8, 10, 12, 9][i % 4]!,
    round: i % 3 !== 0,
    color: colors[i % 5]!,
    anim: `cache-b${i % 10} ${750 + (i % 5) * 90}ms ${i >= 10 ? 120 : 0}ms cubic-bezier(.15,.7,.3,1) both`,
  }));
}
