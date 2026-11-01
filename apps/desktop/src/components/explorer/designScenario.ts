// The design's illustrative machine for the Storage Explorer (design/source/Explorer.dc.html:
// Macintosh HD 186.0 GB + Dev SSD 262.8 GB of development data), as a real-shaped ScanReport.
//
// MOCK ONLY, OPT-IN: the Explorer uses it only with the mock bridge and `#/explorer?demo=design`
// (like the Projects screen), to compare with design/screens/07*.png. The real app never sees
// these numbers; the default mock data stays the recorded payloads (src/bridge/fixtures).
//
// The design splits a project's protected part into ".git" and "src"; the core reports a
// project's total size, so here (as in the real app) it is one "source & other files" tile.
import type { Candidate, ProjectSummary, ReasonCode, ScanReport, Tier } from '../../bridge';
import { MOCK_DATA } from '../../bridge/mock';

const GiB = 1024 ** 3;
const HOME = '/Users/ada';
const SSD = '/Volumes/Dev SSD';
const g = (x: number) => Math.round(x * GiB);

const CODE: Record<Tier, ReasonCode> = { S0: 'regenerable', S1: 'costly_to_regenerate', S2: 'no_lockfile', S3: 'git_tracked' };

type Row = [path: string, tier: Tier, gib: number, why: string];

const CANDIDATES: readonly Row[] = [
  [`${HOME}/code/.worktrees/unpushed worktrees`, 'S3', 14.3, 'Commits that exist only on this Mac.'],
  [`${HOME}/code/.worktrees/worktree artifacts`, 'S0', 6.7, 'node_modules and target in merged worktrees.'],
  [`${HOME}/code/.worktrees/ignored files`, 'S2', 6.1, '.env files and local databases.'],
  [`${HOME}/code/rust-svc/target`, 'S0', 15.2, 'Rust build output.'],
  [`${HOME}/code/shop-api/node_modules`, 'S0', 12.4, 'npm dependencies with a lockfile.'],
  [`${HOME}/code/shop-api/dist`, 'S0', 0.4, 'Build output.'],
  [`${HOME}/code/ml-lab/.venv`, 'S2', 9.8, 'No lockfile.'],
  [`${HOME}/code/ml-lab/data`, 'S2', 3.9, 'No recorded source.'],
  [`${HOME}/code/kart-game/Library`, 'S1', 8.2, 'Unity re-import takes about 25 minutes.'],
  [`${HOME}/.cache/huggingface`, 'S1', 31.2, '14 models; 3 unused for 90+ days.'],
  [`${HOME}/.cache/npm`, 'S1', 6.1, 'Packages re-download on next install.'],
  [`${HOME}/.cache/pnpm store`, 'S1', 4.8, 'Only unused packages are pruned.'],
  [`${HOME}/.cache/uv`, 'S1', 3.1, 'Packages re-download on next install.'],
  [`${HOME}/.cache/other`, 'S2', 2.9, 'Caches without a known owner.'],
  [`${HOME}/.cache/bun`, 'S1', 2.6, 'Packages re-download on next install.'],
  [`${HOME}/.cache/yarn`, 'S1', 1.4, 'Packages re-download on next install.'],
  [`${HOME}/Docker build cache`, 'S1', 18.0, 'Cleaned through Docker itself.'],
  [`${SSD}/vm-images/win11.vhdx`, 'S2', 63.5, 'Virtual machine disk. Inspect only; scull never deletes VM disks.'],
  [`${SSD}/vm-images/ubuntu-24.qcow2`, 'S2', 41.0, 'Virtual machine disk. Inspect only.'],
  [`${SSD}/projects-archive/node_modules ×23`, 'S0', 29.6, 'Dependencies of 23 projects untouched for 6+ months.'],
  [`${SSD}/projects-archive/target ×6`, 'S0', 18.6, 'Rust build output in 6 archived projects.'],
  [`${SSD}/datasets`, 'S3', 88.0, 'Your data. Never touched.'],
];

/** Projects and their total size (candidates + source). */
const PROJECTS: readonly [root: string, gib: number][] = [
  [`${HOME}/code/rust-svc`, 24.6],
  [`${HOME}/code/shop-api`, 21.3],
  [`${HOME}/code/ml-lab`, 19.2],
  [`${HOME}/code/kart-game`, 12.5],
  [`${HOME}/work/payments`, 11.2],
  [`${SSD}/projects-archive`, 70.3],
];

function candidate([path, tier, gib, why]: Row, i: number): Candidate {
  const bytes = g(gib);
  return {
    action: { type: 'none' },
    artifact: '',
    category: 'project_artifact',
    evidence: [],
    flags: {},
    id: `explorer-design-${i}`,
    identity: { file: `e${i}`, volume: 1 },
    path,
    project: null,
    reasons: [{ code: CODE[tier], message: why, tier }],
    regeneration: { cost: { type: 'unknown' }, instructions: why },
    rule: { id: `design.explorer.${i}`, version: 1 },
    sizes: { allocated: bytes, logical: bytes, reclaimable: { bytes, confidence: 'high', shared_bytes: 0 } },
    tier,
  };
}

function project([root, gib]: readonly [string, number]): ProjectSummary {
  const z = () => ({ s0: 0, s1: 0, s2: 0, s3: 0 });
  return {
    allocated: z(),
    badge: 'S3',
    candidates: 0,
    ecosystems: [],
    git_worktree: null,
    markers: [],
    name: root.split('/').at(-1)!,
    newest_mtime: null,
    reclaimable: z(),
    root,
    total_allocated: g(gib),
  };
}

/** The design's machine as a scan report (the recorded report's other fields). */
export function designExplorerReport(): ScanReport {
  return {
    ...structuredClone(MOCK_DATA.scanReport),
    candidates: CANDIDATES.map(candidate),
    projects: PROJECTS.map(project),
    roots: [`${HOME}/code`, `${HOME}/.cache`, `${HOME}/work`, SSD],
    worktrees: [],
  };
}
