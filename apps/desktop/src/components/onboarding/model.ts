// First-run onboarding (PRD §14.7, ONB-01…07; design 02). Pure logic: the answers, their
// detected defaults, the live summary, the scan-root rules (CFG-04) and the first-scan state
// machine. The screen (screens/Onboarding) only renders this.
//
// Answer IDs are the CLI's (`scull init`, crates/scull-cli/src/cmd/init.rs), so a profile made
// here and one made by `scull init` read the same.

/** "What do you build?" (ONB-02 #1), in the design's order. */
export const BUILDS = [
  { id: 'web', label: 'Web / JS' },
  { id: 'rust', label: 'Rust' },
  { id: 'python', label: 'Python' },
  { id: 'ml', label: 'ML / AI' },
  { id: 'mobile', label: 'Mobile' },
  { id: 'game', label: 'Game dev' },
  { id: 'native', label: 'C / C++ / embedded' },
  { id: 'infra', label: 'Infra / DevOps' },
  { id: 'other', label: 'Something else' },
] as const;
export type BuildId = (typeof BUILDS)[number]['id'];

export const AGENTS = [
  { id: 'none', label: 'None' },
  { id: 'sometimes', label: 'Sometimes' },
  { id: 'daily', label: 'Daily' },
] as const;
export type AgentUse = (typeof AGENTS)[number]['id'];

export const PRIORITIES = [
  { id: 'free-space', label: 'Free space now', hint: 'Opens on Quick Clean and the safest wins' },
  { id: 'tidy', label: 'Keep it tidy automatically', hint: 'Sets up forecasts, staleness alerts and budgets' },
  { id: 'understand', label: 'Just understand my disk', hint: 'Opens on the storage map; no suggestions pushed' },
] as const;
export type Priority = (typeof PRIORITIES)[number]['id'];

/** Caution never changes tiers or safety checks (ONB-07); only suggestions and alerts. */
export const CAUTION = [
  { id: 'cautious', label: 'Very cautious', hint: 'Nothing preselected · longer idle windows' },
  { id: 'balanced', label: 'Balanced', hint: 'Regenerable items preselected for review' },
  { id: 'expert', label: 'I know what I’m doing', hint: 'Shorter idle windows · fewer reminders' },
] as const;
export type Caution = (typeof CAUTION)[number]['id'];

/** A toolchain the metadata-only probe found (same fields as `scull init`'s `Toolchain`). */
export interface DetectedToolchain {
  /** `rust`, `node`, `python`, `ml`, `containers`, `go`, `jvm`, `dotnet`, `mobile`, `native`, `game`. */
  id: string;
  label: string;
  /** The "What do you build?" answer it suggests. */
  build: BuildId;
  /** Binaries found on PATH (absolute). */
  binaries: string[];
  /** Known cache folders that exist (absolute, not sized). */
  caches: string[];
}

/** A folder that probably holds projects. */
export interface DetectedFolder {
  /** Absolute path. */
  path: string;
  /** Git repositories directly inside, when counted. */
  repos: number | null;
  /** A well-known project folder (`~/code`, `~/Projects`, ...); others are offered unticked. */
  suggested: boolean;
}

/** What the probe found (ONB-01). Metadata only: nothing is opened, sized or scanned. */
export interface Detection {
  /** The home directory (absolute). */
  home: string;
  toolchains: DetectedToolchain[];
  folders: DetectedFolder[];
  /** `claude-code`, `codex`, `cursor`. */
  ai_clients: string[];
}

export interface Answers {
  builds: BuildId[];
  /** Absolute scan roots, in the order shown. */
  roots: string[];
  /** Broad roots (CFG-04) the user explicitly confirmed. */
  confirmedBroad: string[];
  agents: AgentUse;
  connectAgents: boolean;
  priority: Priority;
  caution: Caution;
}

/** The local profile (ONB-03); same fields as `scull init`'s `profile.toml`. */
export interface Profile {
  version: 1;
  builds: BuildId[];
  roots: string[];
  agents: AgentUse;
  priority: Priority;
  caution: Caution;
  detected_toolchains: string[];
  detected_ai_clients: string[];
  /** Connect scull's MCP server to the detected clients, read-only. */
  connect_agents: boolean;
}

const AI_LABELS: Record<string, string> = { 'claude-code': 'Claude Code', codex: 'Codex', cursor: 'Cursor' };

/** "Claude Code", "Codex", or the ID itself. */
export function aiClientLabel(id: string): string {
  return AI_LABELS[id] ?? id;
}

/** "A", "A and B", "A, B and C". */
export function andList(items: readonly string[]): string {
  if (items.length <= 1) return items.join('');
  return `${items.slice(0, -1).join(', ')} and ${items[items.length - 1]}`;
}

// ---- Paths and scan roots (CFG-04) ----

function isWindowsPath(p: string): boolean {
  return /^[A-Za-z]:[\\/]/.test(p) || p.startsWith('\\\\');
}

function parts(p: string): string[] {
  const win = isWindowsPath(p);
  const segs = p.split(win ? /[\\/]+/ : /\/+/).filter((s) => s.length > 0);
  return win ? segs.map((s) => s.toLowerCase()) : segs;
}

/** Absolute on any OS: `/x`, `C:\x`, `\\server\share`. */
export function isAbsolute(p: string): boolean {
  return p.startsWith('/') || isWindowsPath(p);
}

/** `~/code` for a folder under home, the path itself otherwise. */
export function displayPath(path: string, home: string): string {
  const h = parts(home);
  const p = parts(path);
  if (h.length === 0 || p.length < h.length || !h.every((s, i) => s === p[i])) return path;
  if (p.length === h.length) return '~';
  const sep = isWindowsPath(path) ? '\\' : '/';
  return `~${sep}${path.split(/[\\/]+/).filter(Boolean).slice(h.length).join(sep)}`;
}

/** Why a root is broad (CFG-04), or `null`: a filesystem root, the home folder or a parent of it. */
export function broadRootConcern(path: string, home: string): string | null {
  const p = parts(path);
  const h = parts(home);
  const bareWin = /^[A-Za-z]:[\\/]?$/.test(path);
  if (p.length === 0 || bareWin || (isWindowsPath(path) && !path.startsWith('\\\\') && p.length === 1)) {
    return 'is a filesystem root';
  }
  if (h.length > 0 && p.length === h.length && h.every((s, i) => s === p[i])) return 'is your home folder';
  if (h.length > p.length && p.every((s, i) => s === h[i])) return 'contains your home folder';
  return null;
}

export type RootCheck = { ok: true; path: string; broad: string | null } | { ok: false; error: string };

/** Validate a typed folder: expands a leading `~`, rejects relative paths and `..`. */
export function checkRoot(input: string, home: string): RootCheck {
  let p = input.trim();
  if (p.length === 0) return { ok: false, error: 'Type a folder path.' };
  if (p === '~' || p.startsWith('~/') || p.startsWith('~\\')) {
    const sep = isWindowsPath(home) ? '\\' : '/';
    p = p === '~' ? home : `${home.replace(/[\\/]+$/, '')}${sep}${p.slice(2)}`;
  }
  if (/^[A-Za-z]:$/.test(p)) p += '\\';
  if (!isAbsolute(p)) return { ok: false, error: 'Use a full path, like /Users/you/code.' };
  if (p.split(/[\\/]+/).includes('..')) return { ok: false, error: 'Remove “..” from the path.' };
  if (p.length > 1) p = p.replace(/[\\/]+$/, '') || p;
  if (/^[A-Za-z]:$/.test(p)) p += '\\';
  return { ok: true, path: p, broad: broadRootConcern(p, home) };
}

// ---- Defaults and summary ----

/** Detected defaults (ONB-01): builds from toolchains; suggested folders ticked unless broad. */
export function defaultAnswers(d: Detection): Answers {
  const builds: BuildId[] = [];
  for (const t of d.toolchains) if (!builds.includes(t.build)) builds.push(t.build);
  if (builds.length === 0) builds.push('other');
  const roots = d.folders.filter((f) => f.suggested && broadRootConcern(f.path, d.home) === null).map((f) => f.path);
  const agents: AgentUse = d.ai_clients.length > 0 ? 'daily' : 'none';
  return {
    builds: BUILDS.map((b) => b.id).filter((id) => builds.includes(id)),
    roots,
    confirmedBroad: [],
    agents,
    connectAgents: d.ai_clients.length > 0,
    priority: 'free-space',
    caution: 'balanced',
  };
}

/** Roots to scan: the ticked ones, minus broad roots that were never confirmed. */
export function scanRoots(a: Answers, home: string): string[] {
  return a.roots.filter((r) => broadRootConcern(r, home) === null || a.confirmedBroad.includes(r));
}

export function answeredCount(a: Answers, home: string): number {
  return [a.builds.length > 0, scanRoots(a, home).length > 0, true, true, true].filter(Boolean).length;
}

export const IDLE_WINDOW: Record<Caution, string> = { cautious: '30 days', balanced: '14 days', expert: '7 days' };

export interface SummaryRow {
  k: string;
  v: string;
}

/** "Here's what scull will focus on" (ONB-03), updated live as answers change. */
export function summary(a: Answers, d: Detection): SummaryRow[] {
  const roots = scanRoots(a, d.home).map((r) => displayPath(r, d.home));
  const labels = BUILDS.filter((b) => a.builds.includes(b.id)).map((b) => b.label);
  const clients = d.ai_clients.map(aiClientLabel);
  const open = { 'free-space': 'Quick Clean + safest wins', tidy: 'Alerts, forecast and budgets', understand: 'Storage map' }[
    a.priority
  ];
  const agents =
    a.agents === 'none'
      ? 'Off'
      : 'Agent worktrees watched' + (a.connectAgents && clients.length > 0 ? ` · ${clients.join(' + ')} connected` : '');
  return [
    { k: 'Scan', v: roots.length ? roots.join(', ') : 'Choose at least one folder' },
    { k: 'Show first', v: labels.length ? labels.slice(0, 4).join(' · ') : 'Everything, by size' },
    { k: 'Agents', v: agents },
    { k: 'Open on', v: open },
    { k: 'Stale after', v: `Idle ${IDLE_WINDOW[a.caution]}` },
    { k: 'Checks', v: a.priority === 'understand' ? 'Manual' : 'Every 6 h · alert at ~14 days to full' },
  ];
}

export function toProfile(a: Answers, d: Detection): Profile {
  return {
    version: 1,
    builds: [...a.builds],
    roots: scanRoots(a, d.home),
    agents: a.agents,
    priority: a.priority,
    caution: a.caution,
    detected_toolchains: d.toolchains.map((t) => t.id),
    detected_ai_clients: [...d.ai_clients],
    connect_agents: a.agents !== 'none' && a.connectAgents && d.ai_clients.length > 0,
  };
}

/** Short names of what was found for a toolchain row: binaries, else cache folder names. */
export function toolchainDetail(t: DetectedToolchain): string {
  const base = (p: string) => p.split(/[\\/]+/).filter(Boolean).pop() ?? p;
  const names = t.binaries.map((b) =>
    base(b)
      .replace(/\.(exe|cmd|bat|com)$/i, '')
      .replace(/\.app$/i, '')
      .toLowerCase(),
  );
  const list = names.length > 0 ? names : t.caches.map((c) => base(c).replace(/^\./, ''));
  return [...new Set(list)].slice(0, 3).join(' · ');
}

/** "Mac", "PC", "computer": used in "Found on this Mac" and "Everything stays on this Mac". */
export function machineNoun(os: string | undefined): string {
  return os === 'macos' ? 'Mac' : os === 'windows' ? 'PC' : 'computer';
}

// ---- First scan (design 02 → 02b) ----

export type ScanPhase = 'idle' | 'running' | 'done' | 'cancelled' | 'failed';

export interface ScanResult {
  entries: number;
  projects: number;
  worktrees: number;
  /** S0 bytes: what the design calls "safely reclaimable". */
  safeBytes: number;
}

export interface FirstScan {
  phase: ScanPhase;
  /** Entries counted so far, summed over roots. */
  entries: number;
  /** What the scan is doing ("~/code/…", "classifying"). */
  activity: string;
  result: ScanResult | null;
  error: string | null;
}

export function initialScan(): FirstScan {
  return { phase: 'idle', entries: 0, activity: '', result: null, error: null };
}

/** Allowed transitions of the first-scan state machine. */
const NEXT: Record<ScanPhase, readonly ScanPhase[]> = {
  idle: ['running'],
  running: ['done', 'cancelled', 'failed'],
  done: [],
  cancelled: ['running'],
  failed: ['running'],
};

export function canMove(from: ScanPhase, to: ScanPhase): boolean {
  return NEXT[from].includes(to);
}

/**
 * Progress bar fill, 0–1. The core reports entries but no total, so while running the bar
 * eases towards 90% over a few seconds (the design's 3.2 s ease-out) and fills on completion.
 */
export function scanFill(phase: ScanPhase, elapsedMs: number): number {
  if (phase === 'idle') return 0;
  if (phase === 'done') return 1;
  const t = Math.min(1, Math.max(0, elapsedMs) / 3200);
  return 0.9 * (1 - (1 - t) ** 2);
}

/** Per-root entry counters → total (the core counts entries per root). */
export function totalEntries(perRoot: Record<string, number>): number {
  return Object.values(perRoot).reduce((a, b) => a + b, 0);
}

/** First open (ONB): no scan was ever recorded. The app then starts on onboarding. */
export function needsOnboarding(status: { state: string } | null): boolean {
  return status !== null && (status.state === 'no_scan' || status.state === 'no_database');
}
