// The design's illustrative machine (design/source/Main.dc.html: 16 items, 186.0 GB of
// development data) as a bridge, for comparing the Overview with the design mockups (screens 01).
//
// MOCK ONLY, OPT-IN: it wraps the mock bridge when the page URL carries `?scenario=design`
// (see useOverviewBridge). It never wraps the Tauri bridge, and the real app never sees these
// numbers. The default mock data stays the recorded real payloads (src/bridge/fixtures).
//
// The candidates are well-formed `Candidate` values, but their categories follow the design's
// wheel, not the core's rules (e.g. the design's "Source & Git" slice is filed under `other`;
// the core never lists source as a candidate).
import type {
  Action,
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
import { registerScanCandidates } from '../../bridge/planMock';

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

/** What a plan would do (D-28): delete at S0, quarantine at S1/S2, Docker through Docker itself, nothing at S3. */
function actionFor(id: string, tier: Tier): Action {
  if (id === 'docker') return { type: 'tool_command', tool: 'docker', args: ['builder', 'prune'] };
  if (tier === 'S0') return { type: 'delete' };
  if (tier === 'S3') return { type: 'none' };
  return { type: 'quarantine' };
}

function candidate([id, rel, category, tier, gib, artifact, project, why]: Row): Candidate {
  const path = `${HOME}/${rel}`;
  const bytes = Math.round(gib * GiB);
  return {
    action: actionFor(id, tier),
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

/** The design's external drive (design 07: Dev SSD), for the drive switcher (P3-UI-12). */
const SSD = '/Volumes/Dev SSD';
const SSD_ROWS: readonly Row[] = [
  ['ssd-win11', 'vm-images/win11.vhdx', 'large_file', 'S2', 63.5, 'Virtual machine disk', null, 'Virtual machine disk. Inspect only; scull never deletes VM disks.'],
  ['ssd-ubuntu', 'vm-images/ubuntu-24.qcow2', 'large_file', 'S2', 41.0, 'Virtual machine disk', null, 'Virtual machine disk. Inspect only.'],
  ['ssd-nm', 'projects-archive/node_modules', 'project_artifact', 'S0', 29.6, 'node_modules ×23', null, 'Dependencies of 23 projects untouched for 6+ months.'],
  ['ssd-target', 'projects-archive/target', 'project_artifact', 'S0', 18.6, 'target ×6', null, 'Rust build output in 6 archived projects.'],
];
export const DESIGN_SSD_CANDIDATES: readonly Candidate[] = SSD_ROWS.map((r) => ({ ...candidate(r), path: `${SSD}/${r[1]}` }));

/**
 * Wrap a mock bridge with the design's data. `stepMs` paces the simulated scan (9 steps).
 * `secondDrive` (`?scenario=design&drives=2`) adds the Dev SSD's candidates and free-space
 * samples, for the Overview drive switcher; design 01 itself has one drive's data.
 */
export function designScenarioBridge(base: ScullBridge, stepMs = 300, secondDrive = false): ScullBridge {
  let ops = 0;
  const all = secondDrive ? [...DESIGN_CANDIDATES, ...DESIGN_SSD_CANDIDATES] : DESIGN_CANDIDATES;
  const scanReport = (template: ScanReport): ScanReport => ({
    ...template,
    candidates: structuredClone(all) as Candidate[],
    entries_scanned: DESIGN_ENTRIES,
    roots: [`${HOME}/code`, `${HOME}/work`, `${HOME}/Library`, ...(secondDrive ? [SSD] : [])],
    projects: [],
    worktrees: [],
    totals: totals(all),
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
      const report = { ...scanReport(template), cancelled: Boolean(o.signal?.aborted) };
      registerScanCandidates(report.candidates);
      return report;
    },
    async caches(o?: OperationOptions): Promise<CachesReport> {
      const r = await base.caches(o);
      const cands = DESIGN_CANDIDATES.filter((c) => CACHE_IDS.has(c.id));
      registerScanCandidates(cands);
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
      if (secondDrive) {
        // Dev SSD: 1000 GB, 390 GB free, VM disks growing 14 GB a day (full in about 27 days).
        for (let i = 0; i < 8; i++) {
          volumes.push({ volume_id: SSD, total_bytes: 1000 * GiB, free_bytes: Math.round((390 + (7 - i) * 14) * GiB), sampled_at: s.now - (7 - i) * 86400 });
        }
      }
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

/** Whether the design scenario should include the second drive (`&drives=2`). */
export function wantsSecondDrive(loc: { search?: string; hash?: string } | undefined = globalThis.location): boolean {
  if (!loc) return false;
  const q = `${loc.search ?? ''}&${(loc.hash ?? '').split('?')[1] ?? ''}`;
  return /(^|[?&])drives=2(&|$)/.test(q);
}
