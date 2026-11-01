// The Storage Explorer's folder tree (P3-UI-07, design 07 / 07b), built from what the core
// already reports: the drives (`drives()`) and the last scan (`ScanReport`). No extra
// filesystem walk: every size comes from the scan.
//
//   This Mac
//   ├── Macintosh HD · internal      a drive scull scans (D-22), sized by what was scanned on it
//   │   ├── code                     folders under the scan roots (home-relative on the home drive)
//   │   │   ├── rust-svc             a project (ScanReport.projects): its candidates, plus
//   │   │   │   ├── target           "source & other files" = project total - candidates (never touched)
//   │   │   │   └── source & other files
//   │   │   └── ...
//   └── Dev SSD · external
//
// A candidate is a leaf: nothing inside it is listed again (its size already counts it), and
// a project inside a candidate (a linked worktree's checkout) is not counted twice. Plain
// folders with a single child are merged into one tile ("work/payments"), like the design.
// Drives scull does not scan (backup never, network off by default; D-22) are listed apart.
import type { Candidate, Drive, ScanReport, Tier } from '../../bridge';
import { formatBytes } from '../../lib/format';
import { TIER_KEYS, TIERS } from '../overview/tiers';

export type TierBytes = Record<Tier, number>;
export type NodeKind = 'machine' | 'drive' | 'folder' | 'item' | 'group';

export interface ExNode {
  /** Unique in the tree (a path, or a synthetic key). */
  id: string;
  name: string;
  kind: NodeKind;
  /** Filesystem path; `null` for the machine and for groups. */
  path: string | null;
  bytes: number;
  tiers: TierBytes;
  /** An item's tier; `null` for containers. */
  tier: Tier | null;
  children: ExNode[];
  /** One-line explanation for an item (from its candidate's reasons). */
  note: string;
  /** The candidate behind an item, when there is one. */
  candidate: Candidate | null;
  drive: Drive | null;
  /** A project's folder (or a project with nothing but source, shown as an item). */
  project: boolean;
}

export interface UnscannedDrive {
  drive: Drive;
  why: string;
}

export interface Explorer {
  root: ExNode;
  /** Drives with nothing in the tree, and why (D-22). */
  unscanned: UnscannedDrive[];
}

/** Name of the "everything else in a project" leaf. */
export const REST_NAME = 'source & other files';
export const REST_NOTE = 'Source code, Git history and other project files. scull never touches them.';

const zero = (): TierBytes => ({ S0: 0, S1: 0, S2: 0, S3: 0 });

/* ---------------------------------------------------------------- paths */

const WIN_PATH = /^(?:[A-Za-z]:[\\/]|\\\\)/;

export function isWindowsPath(p: string): boolean {
  return WIN_PATH.test(p);
}

/** Path segments; Windows paths split on both separators. */
export function splitPath(p: string): string[] {
  return p.split(isWindowsPath(p) ? /[\\/]+/ : /\/+/).filter((s) => s.length > 0);
}

/** Segments of `path` below `base`, or `null` when `path` is not `base` or inside it. */
export function relativeSegments(path: string, base: string): string[] | null {
  const win = isWindowsPath(path);
  if (win !== isWindowsPath(base)) return null;
  const ps = splitPath(path);
  const bs = splitPath(base);
  if (bs.length > ps.length) return null;
  const k = (s: string) => (win ? s.toLowerCase() : s);
  for (let i = 0; i < bs.length; i++) if (k(ps[i]!) !== k(bs[i]!)) return null;
  return ps.slice(bs.length);
}

/** The user folder a path is in (`/Users/ada`, `/home/ada`, `C:\Users\ada`), if any. */
export function homeOf(path: string): string | null {
  const m = /^\/(?:Users|home)\/[^/]+(?=\/|$)/.exec(path) ?? /^[A-Za-z]:\\Users\\[^\\]+(?=\\|$)/i.exec(path);
  return m ? m[0] : null;
}

function joinPath(base: string, segs: readonly string[]): string {
  if (segs.length === 0) return base;
  if (isWindowsPath(base)) return `${base.replace(/[\\/]+$/, '')}\\${segs.join('\\')}`;
  return `${base.replace(/\/+$/, '')}/${segs.join('/')}`;
}

/* ---------------------------------------------------------------- tree */

interface Trie {
  name: string;
  path: string;
  children: Map<string, Trie>;
  leaf: Candidate | null;
  projectTotal: number | null;
}

const trie = (name: string, path: string): Trie => ({ name, path, children: new Map(), leaf: null, projectTotal: null });

/** Where a path lands: its drive, the base folder of the walk and the segments below it. */
interface Place {
  drive: Drive;
  base: string;
  segs: string[];
}

/** Drive suffix on the root tiles ("Dev SSD · external"). */
export function driveKindLabel(d: Drive): string {
  switch (d.kind) {
    case 'removable':
      return 'external';
    case 'local':
      return 'internal';
    default:
      return d.kind;
  }
}

/** Why a drive has nothing in the tree (D-22: external scanned when connected, network off
 * by default, backup never). */
export function unscannedWhy(d: Drive): string {
  if (d.scan_policy === 'never') return 'never scanned';
  if (d.scan_policy === 'off_by_default') return `${d.kind === 'network' ? 'network drive' : 'not scanned'} · off by default`;
  return 'nothing scanned here yet';
}

function capitalize(s: string): string {
  return s ? s[0]!.toUpperCase() + s.slice(1) : s;
}

function sentence(s: string): string {
  const t = capitalize(s.trim());
  return t && !/[.!?]$/.test(t) ? `${t}.` : t;
}

/** An item's explanation: its artifact, then the reason that set its tier. */
export function candidateNote(c: Candidate): string {
  const reason = c.reasons.find((r) => r.tier === c.tier) ?? c.reasons[0];
  const why = reason ? sentence(reason.message) : '';
  const what = c.artifact ? sentence(c.artifact) : '';
  if (!what) return why;
  if (!why || why.toLowerCase().startsWith(c.artifact.toLowerCase())) return why || what;
  return `${what} ${why}`;
}

function item(id: string, name: string, path: string | null, bytes: number, tier: Tier, note: string, extra: Partial<ExNode> = {}): ExNode {
  const tiers = zero();
  tiers[tier] = bytes;
  return { id, name, kind: 'item', path, bytes, tiers, tier, children: [], note, candidate: null, drive: null, project: false, ...extra };
}

function container(id: string, name: string, kind: NodeKind, path: string | null, children: ExNode[], extra: Partial<ExNode> = {}): ExNode {
  const tiers = zero();
  let bytes = 0;
  for (const c of children) {
    bytes += c.bytes;
    for (const t of TIER_KEYS) tiers[t] += c.tiers[t];
  }
  const sorted = [...children].sort((a, b) => b.bytes - a.bytes || a.name.localeCompare(b.name));
  return { id, name, kind, path, bytes, tiers, tier: null, children: sorted, note: '', candidate: null, drive: null, project: false, ...extra };
}

function fromTrie(t: Trie, drive: Drive): ExNode {
  const c = t.leaf;
  if (c) {
    return item(c.path, t.name, c.path, c.sizes.allocated, c.tier, candidateNote(c), { candidate: c, drive });
  }
  const kids = [...t.children.values()].map((k) => fromTrie(k, drive));
  const project = t.projectTotal !== null;
  const sum = kids.reduce((a, k) => a + k.bytes, 0);
  if (project && t.projectTotal! > sum) {
    kids.push(item(`${t.path}#rest`, REST_NAME, t.path, t.projectTotal! - sum, 'S3', REST_NOTE, { drive }));
  }
  if (project && kids.length === 1 && kids[0]!.name === REST_NAME) {
    // A project with nothing but source: one protected tile, named after the project.
    return { ...kids[0]!, id: t.path, name: t.name, project: true };
  }
  if (!project && kids.length === 1) {
    // A plain folder holding one thing: merge them into one tile ("work/payments").
    const only = kids[0]!;
    return { ...only, name: `${t.name}/${only.name}` };
  }
  return container(t.path, t.name, 'folder', t.path, kids, { drive, project });
}

/** Drives sorted for matching: longest mount point first. */
function byMountDepth(drives: readonly Drive[]): Drive[] {
  return [...drives].sort((a, b) => splitPath(b.mount_point).length - splitPath(a.mount_point).length);
}

const OTHER_DRIVE: Drive = {
  mount_point: '',
  label: 'Other locations',
  kind: 'local',
  scan_policy: 'scan',
  holds_home: false,
};

function place(path: string, drives: readonly Drive[]): Place | null {
  const drive = drives.find((d) => relativeSegments(path, d.mount_point) !== null);
  if (!drive) {
    const segs = splitPath(path);
    if (isWindowsPath(path)) return segs.length > 1 ? { drive: OTHER_DRIVE, base: `${segs[0]}\\`, segs: segs.slice(1) } : null;
    return segs.length ? { drive: OTHER_DRIVE, base: '/', segs } : null;
  }
  const home = drive.holds_home ? homeOf(path) : null;
  if (home && relativeSegments(home, drive.mount_point) !== null) {
    return { drive, base: home, segs: relativeSegments(path, home)! };
  }
  // On the home drive, folders outside the home folder start with the separator ("/opt"),
  // so they can't be mistaken for (or merged with) home-relative ones.
  const segs = relativeSegments(path, drive.mount_point)!;
  if (drive.holds_home && segs.length) segs[0] = `${isWindowsPath(path) ? '\\' : '/'}${segs[0]}`;
  return { drive, base: drive.mount_point, segs };
}

/** Walk (creating) the trie of `p`; `null` when the walk passes through a candidate. */
function walk(roots: Map<Drive, Trie>, p: Place): Trie | null {
  let t = roots.get(p.drive);
  if (!t) {
    t = trie(p.drive.label, p.drive.mount_point);
    roots.set(p.drive, t);
  }
  const win = isWindowsPath(p.base) || isWindowsPath(p.drive.mount_point);
  const real: string[] = [];
  for (const seg of p.segs) {
    if (t.leaf) return null;
    const clean = seg.replace(/^[\\/]/, '');
    real.push(clean);
    const key = win ? seg.toLowerCase() : seg;
    let next = t.children.get(key);
    if (!next) {
      next = trie(seg, joinPath(p.base, real));
      t.children.set(key, next);
    }
    t = next;
  }
  return t;
}

/**
 * The Explorer tree for a scan and the drives. `machine` names the root ("This Mac",
 * "This PC"). Without a report the tree is empty and every drive is listed as unscanned.
 */
export function buildExplorer(report: ScanReport | null, drives: readonly Drive[], machine = 'This Mac'): Explorer {
  const ordered = byMountDepth(drives);
  const roots = new Map<Drive, Trie>();

  // Candidates, shallowest first: an outer candidate swallows the ones inside it.
  const candidates = [...(report?.candidates ?? [])].sort((a, b) => splitPath(a.path).length - splitPath(b.path).length);
  for (const c of candidates) {
    const p = place(c.path, ordered);
    if (!p || p.segs.length === 0) continue;
    const t = walk(roots, p);
    if (!t || t.leaf) continue;
    t.leaf = c;
    t.children.clear();
  }
  // Projects: their total, minus their candidates, is the protected rest.
  for (const pr of report?.projects ?? []) {
    if (pr.total_allocated == null) continue;
    const p = place(pr.root, ordered);
    if (!p || p.segs.length === 0) continue;
    const t = walk(roots, p);
    if (!t || t.leaf) continue;
    t.projectTotal = pr.total_allocated;
  }

  const driveNodes: ExNode[] = [];
  for (const [drive, t] of roots) {
    const kids = [...t.children.values()].map((k) => fromTrie(k, drive));
    const node = container(`drive:${drive.mount_point}`, drive.label, 'drive', drive.mount_point || null, kids, { drive });
    if (node.bytes > 0) driveNodes.push(node);
  }
  const shown = new Set(driveNodes.map((n) => n.drive));
  const unscanned = drives.filter((d) => !shown.has(d)).map((drive) => ({ drive, why: unscannedWhy(drive) }));
  return { root: container('machine', machine, 'machine', null, driveNodes), unscanned };
}

/** "This Mac" / "This PC" / "This computer". */
export function machineName(os: string): string {
  return os === 'macos' ? 'This Mac' : os === 'windows' ? 'This PC' : 'This computer';
}

/* ---------------------------------------------------------------- navigation */

/** The nodes from the root along `ids` (unknown IDs stop the walk). */
export function trailOf(root: ExNode, ids: readonly string[]): ExNode[] {
  const out = [root];
  let node = root;
  for (const id of ids) {
    const next = node.children.find((c) => c.id === id && c.children.length > 0 && c.kind !== 'group');
    if (!next) break;
    out.push(next);
    node = next;
  }
  return out;
}

/** Whether clicking the tile opens it (folders, drives) rather than inspecting it. */
export function opens(n: ExNode): boolean {
  return (n.kind === 'drive' || n.kind === 'folder' || n.kind === 'machine') && n.children.length > 0;
}

/** Tiles under this share of the folder are grouped unless hidden folders are shown. */
export const SMALL_SHARE = 0.01;
/** At most this many tiles before the rest are grouped. */
export const MAX_TILES = 40;

/**
 * The tiles of a folder, biggest first. Unless `showAll`, items too small to draw (below
 * SMALL_SHARE of the folder, or past MAX_TILES) are folded into one "N smaller items" group,
 * which is inspected like a file group.
 */
export function visibleChildren(node: ExNode, showAll: boolean): ExNode[] {
  const kids = node.children.filter((c) => c.bytes > 0);
  if (showAll) return kids;
  const big: ExNode[] = [];
  const small: ExNode[] = [];
  for (const k of kids) {
    if (big.length < MAX_TILES - 1 && k.bytes >= node.bytes * SMALL_SHARE) big.push(k);
    else small.push(k);
  }
  if (small.length < 2) return kids;
  return [...big, container(`${node.id}#small`, `${small.length} smaller items`, 'group', null, small)];
}

/** The display path of a trail: drive and folder names joined with `/` ("Macintosh HD/code"). */
export function displayPath(trail: readonly ExNode[]): string {
  return trail.length <= 1 ? trail[0]!.name : trail.slice(1).map((n) => n.name).join('/');
}

export interface LargestRow {
  path: string;
  bytes: number;
  tier: Tier;
}

/** The biggest items inside a node, with display paths under `prefix`. */
export function largestInside(node: ExNode, prefix: string, n = 5): LargestRow[] {
  const out: LargestRow[] = [];
  const visit = (x: ExNode, p: string) => {
    if (x.kind === 'item') out.push({ path: p, bytes: x.bytes, tier: x.tier! });
    else for (const c of x.children) visit(c, `${p}/${c.name}`);
  };
  if (node.kind === 'item') visit(node, prefix);
  else for (const c of node.children) visit(c, `${prefix}/${c.name}`);
  return out.sort((a, b) => b.bytes - a.bytes).slice(0, n);
}

export function dominantTier(t: TierBytes): Tier | null {
  let best: Tier | null = null;
  for (const k of TIER_KEYS) if (t[k] > 0 && (best === null || t[k] > t[best])) best = k;
  return best;
}

export type DetailAction = 'why' | 'add' | 'review';

/** The aside's main button: explain protection, queue what can go, or review. */
export function detailAction(n: ExNode): DetailAction {
  const dom = n.tier ?? dominantTier(n.tiers);
  if (dom === 'S3') return 'why';
  return n.tiers.S0 + n.tiers.S1 > 0 ? 'add' : 'review';
}

export const ACTION_LABEL: Record<DetailAction, string> = {
  why: 'Why protected?',
  add: 'Add to cleanup',
  review: 'Review',
};

/** The aside's note under "Largest inside". */
export function detailNote(n: ExNode): string {
  if (n.note) return n.note;
  return n.tiers.S0 > 0 ? `${formatBytes(n.tiers.S0)} in here rebuilds itself.` : 'Nothing in here is safe to remove automatically.';
}

/** "This folder", "Folder", "Item · Regenerable", "Group". */
export function detailKind(n: ExNode, selected: boolean): string {
  if (!selected) return 'This folder';
  if (n.kind === 'item') return `Item · ${TIERS[n.tier!].label}`;
  return n.kind === 'group' ? 'Group' : 'Folder';
}

/** Why the protected bytes in a node are protected: distinct reasons, most common first. */
export function protectionReasons(n: ExNode, max = 4): string[] {
  const counts = new Map<string, number>();
  const visit = (x: ExNode) => {
    if (x.kind !== 'item') return x.children.forEach(visit);
    if (x.tier !== 'S3') return;
    const msgs = x.candidate ? x.candidate.reasons.filter((r) => r.tier === 'S3').map((r) => sentence(r.message)) : [x.note || REST_NOTE];
    for (const m of new Set(msgs)) counts.set(m, (counts.get(m) ?? 0) + x.bytes);
  };
  visit(n);
  return [...counts.entries()].sort((a, b) => b[1] - a[1]).slice(0, max).map(([m]) => m);
}

/** Candidate IDs in a node that a cleanup could take (S0 and S1), biggest first. */
export function cleanupCandidates(n: ExNode): Candidate[] {
  const out: Candidate[] = [];
  const visit = (x: ExNode) => {
    if (x.candidate && (x.tier === 'S0' || x.tier === 'S1')) out.push(x.candidate);
    x.children.forEach(visit);
  };
  visit(n);
  return out.sort((a, b) => b.sizes.allocated - a.sizes.allocated);
}

/** Tier shares of a node, in percent (sum 100 when it has bytes). */
export function tierShares(t: TierBytes): TierBytes {
  const total = TIER_KEYS.reduce((a, k) => a + t[k], 0);
  const out = zero();
  if (total > 0) for (const k of TIER_KEYS) out[k] = (t[k] / total) * 100;
  return out;
}
