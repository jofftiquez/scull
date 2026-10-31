// The design's illustrative machine (design/source/Main.dc.html: 16 items, 186.0 GB of
// development data) as a bridge, for comparing the Overview with design/screens/01*.png.
//
// MOCK ONLY, OPT-IN: it wraps the mock bridge when the page URL carries `?scenario=design`
// (see useOverviewBridge). It never wraps the Tauri bridge, and the real app never sees these
// numbers. The default mock data stays the recorded real payloads (src/bridge/fixtures).
//
// The candidates are well-formed `Candidate` values, but their categories follow the design's
// wheel, not the core's rules (e.g. the design's "Source & Git" slice is filed under `other`;
// the core never lists source as a candidate).
import type {
  CachesReport,
  Candidate,
  Category,
  DesktopStatus,
  OperationOptions,
  ProgressEvent,
  ReasonCode,
  ScanReport,
  ScullBridge,
  Tier,
  Totals,
} from '../../bridge';

const GiB = 1024 ** 3;
const HOME = '/Users/ada';

type Row = [id: string, path: string, category: Category, tier: Tier, gib: number, artifact: string, project: string | null, why: string];

/** The design's ITEMS, in its order, with paths under the imagined home. */
const ROWS: readonly Row[] = [
  ['target', 'code/rust-svc/target', 'project_artifact', 'S0', 15.2, 'Cargo build output', 'rust-svc', 'Rust build output. Rebuilt by cargo build in about 4 minutes.'],
  ['nm', 'code/shop-api/node_modules', 'project_artifact', 'S0', 12.4, 'node_modules', 'shop-api', 'npm dependencies with a lockfile. npm install brings them back. Regrew 4× this month.'],
  ['next', 'code/web-app/.next', 'project_artifact', 'S0', 4.3, 'Next.js build output', 'web-app', 'Next.js build output. Rebuilt by next build.'],
  ['npm', '.npm/_cacache', 'package_cache', 'S1', 6.1, 'npm cache', null, 'Global npm cache. Packages re-download on next install.'],
  ['pnpm', 'Library/pnpm/store', 'package_cache', 'S1', 4.8, 'pnpm store', null, 'Shared store. Only unused packages are pruned; linked projects keep theirs.'],
  ['bun', '.bun/install/cache', 'package_cache', 'S1', 2.6, 'Bun cache', null, 'Global Bun cache. Re-downloads on next install.'],
  ['yarn', 'Library/Caches/Yarn', 'package_cache', 'S1', 1.4, 'Yarn cache', null, 'Global Yarn cache. Re-downloads on next install.'],
  ['wtun', 'code/.worktrees/unpushed', 'worktree', 'S3', 14.3, 'Unpushed worktrees', null, '3 worktrees with commits that exist only on this Mac. Never touched.'],
  ['wtart', 'code/.worktrees/artifacts', 'worktree', 'S0', 6.7, 'Worktree artifacts', null, 'node_modules and target inside 31 merged worktrees. The worktrees stay.'],
  ['wtign', 'code/.worktrees/ignored', 'worktree', 'S2', 6.1, 'Worktree ignored files', null, '.env files and local databases in 18 worktrees. They exist nowhere else.'],
  ['hf', '.cache/huggingface', 'ml_cache', 'S1', 31.2, 'Hugging Face models', null, '14 models. 3 unused for 90+ days hold 11.8 GB. Re-downloading all is about 31 GB.'],
  ['venv', 'code/ml-lab/.venv', 'ml_cache', 'S2', 9.8, 'Python virtualenv', 'ml-lab', 'No lockfile, so it may not be reproducible. Review before removing.'],
  ['data', 'code/ml-lab/data', 'ml_cache', 'S2', 3.9, 'Datasets', 'ml-lab', 'Datasets with no recorded source. Review needed.'],
  ['docker', 'Library/Containers/com.docker.docker', 'container', 'S1', 18.0, 'Docker build cache', null, 'Cleaned through Docker itself, never by deleting files directly.'],
  ['kart', 'code/kart-game/Library', 'large_file', 'S1', 8.2, 'Unity Library', 'kart-game', 'Unity Library folder. Re-import takes about 25 minutes.'],
  ['src', 'code', 'other', 'S3', 41.0, 'Source & Git history', null, 'Your source code and Git history. scull never touches it.'],
];

const CODE: Record<Tier, ReasonCode> = { S0: 'regenerable', S1: 'costly_to_regenerate', S2: 'no_lockfile', S3: 'git_tracked' };

function candidate([id, rel, category, tier, gib, artifact, project, why]: Row): Candidate {
  const path = `${HOME}/${rel}`;
  const bytes = Math.round(gib * GiB);
  return {
    action: { type: 'none' },
    artifact,
    category,
    evidence: [],
    flags: {},
    id: `design-${id}`,
    identity: { file: id, volume: 1 },
    path,
    project: project ? { ecosystem: 'node', name: project, root: `${HOME}/code/${project}` } : null,
    reasons: [{ code: CODE[tier], message: why, tier }],
    regeneration: { cost: { type: 'unknown' }, instructions: why },
    rule: { id: `design.${id}`, version: 1 },
    sizes: { allocated: bytes, logical: bytes, reclaimable: { bytes, confidence: 'high', shared_bytes: 0 } },
    tier,
  };
}

export const DESIGN_CANDIDATES: readonly Candidate[] = ROWS.map(candidate);
const CACHE_IDS = new Set(['design-npm', 'design-pnpm', 'design-bun', 'design-yarn']);
/** Entries the design's scan walks (412,880). */
export const DESIGN_ENTRIES = 412880;
const FOLDERS = ['~/code/rust-svc', '~/code/shop-api', '~/.cache/npm', '~/code/.worktrees', '~/.cache/huggingface', '~/code/ml-lab', '~/Library/Containers/Docker', '~/code/kart-game', '~/work'];

function totals(cands: readonly Candidate[]): Totals {
  const z = () => ({ s0: 0, s1: 0, s2: 0, s3: 0 });
  const t = { allocated: z(), count: z(), logical: z(), reclaimable: z(), worktrees: { allocated: z(), count: z(), reclaimable: z() } };
  for (const c of cands) {
    const k = c.tier.toLowerCase() as 's0';
    t.allocated[k] += c.sizes.allocated;
    t.logical[k] += c.sizes.logical;
    t.reclaimable[k] += c.sizes.reclaimable.bytes;
    t.count[k] += 1;
  }
  return t;
}

const wait = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

/** Wrap a mock bridge with the design's data. `stepMs` paces the simulated scan (9 steps). */
export function designScenarioBridge(base: ScullBridge, stepMs = 300): ScullBridge {
  let ops = 0;
  const scanReport = (template: ScanReport): ScanReport => ({
    ...template,
    candidates: structuredClone(DESIGN_CANDIDATES) as Candidate[],
    entries_scanned: DESIGN_ENTRIES,
    roots: [`${HOME}/code`, `${HOME}/work`, `${HOME}/Library`],
    projects: [],
    worktrees: [],
    totals: totals(DESIGN_CANDIDATES),
  });
  return {
    ...base,
    async scan(params, o: OperationOptions = {}) {
      const pending = base.scan(params, {});
      const opId = `design-${++ops}`;
      for (let i = 1; i <= FOLDERS.length && !o.signal?.aborted; i++) {
        await wait(stepMs);
        const e: ProgressEvent = {
          op_id: opId,
          phase: 'scanning',
          entries: Math.round((DESIGN_ENTRIES * i) / FOLDERS.length),
          allocated_bytes: Math.round((186 * GiB * i) / FOLDERS.length),
          root: `${HOME}/code`,
          current: FOLDERS[i - 1]!,
          what: null,
        };
        o.onProgress?.(e);
      }
      const template = await pending;
      return { ...scanReport(template), cancelled: Boolean(o.signal?.aborted) };
    },
    async caches(o?: OperationOptions): Promise<CachesReport> {
      const r = await base.caches(o);
      const cands = DESIGN_CANDIDATES.filter((c) => CACHE_IDS.has(c.id));
      return { ...r, candidates: structuredClone(cands) as Candidate[], totals: totals(cands) };
    },
    async status(): Promise<DesktopStatus> {
      const s = await base.status();
      const reclaimable = DESIGN_CANDIDATES.reduce((a, c) => a + c.sizes.reclaimable.bytes, 0);
      const safe = DESIGN_CANDIDATES.filter((c) => c.tier === 'S0').reduce((a, c) => a + c.sizes.reclaimable.bytes, 0);
      // The design's disk: 494 GB, 41.2 GB free, losing 3.5 GB a day (full in about 9 days).
      const volumes = Array.from({ length: 8 }, (_, i) => ({
        volume_id: '/',
        total_bytes: 494 * GiB,
        free_bytes: Math.round((41.2 + (7 - i) * 3.5) * GiB),
        sampled_at: s.now - (7 - i) * 86400,
      }));
      return {
        ...s,
        volumes,
        last_scan: s.last_scan && {
          ...s.last_scan,
          roots: 3,
          candidates: DESIGN_CANDIDATES.length,
          reclaimable_bytes: reclaimable,
          safe_reclaimable_bytes: safe,
        },
      };
    },
  };
}

/** Whether the page asked for the design scenario (`?scenario=design`). */
export function wantsDesignScenario(loc: { search?: string; hash?: string } | undefined = globalThis.location): boolean {
  if (!loc) return false;
  const q = `${loc.search ?? ''}&${(loc.hash ?? '').split('?')[1] ?? ''}`;
  return /(^|[?&])scenario=design(&|$)/.test(q);
}
