// The Projects screen's actions that the core bridge does not have yet (P3-UI-06).
//
//   cleanRegenerable(root)  remove the project's S0 (regenerable) candidates through the core
//                           Executor behind a reviewed plan (SAFE-10). Planned bridge commands:
//                           `plan_project_clean` + `execute_plan` (Phase 2 executor, #67).
//   neverSuggest(root)      add the project to the never-suggest list (config, CFG-05).
//   reveal(root)            show the folder in Finder / Explorer / Files.
//
// The desktop never deletes: in a plain browser the mock only animates (it touches nothing),
// and inside Tauri the pending implementation rejects with a BridgeError until the commands
// exist. These calls stay out of `ScullBridge` so its read-only surface and its tests are
// unchanged.
import { BridgeError, type ProjectsView, type ScullBridge } from '../../bridge';

export interface ProjectActions {
  readonly kind: 'mock' | 'pending';
  /** Resolves with the bytes freed; `onProgress` gets 0–1. */
  cleanRegenerable(root: string, bytes: number, onProgress?: (fraction: number) => void): Promise<{ freed_bytes: number }>;
  neverSuggest(root: string): Promise<void>;
  reveal(root: string): Promise<void>;
}

/** The design's clean takes 1.8 s (design/source/Projects.dc.html). */
export const CLEAN_MS = 1800;

export function createMockProjectActions(opts: { durationMs?: number; stepMs?: number } = {}): ProjectActions & {
  calls: string[];
} {
  const duration = opts.durationMs ?? CLEAN_MS;
  const step = opts.stepMs ?? 40;
  const calls: string[] = [];
  return {
    kind: 'mock',
    calls,
    async cleanRegenerable(root, bytes, onProgress) {
      calls.push(`clean ${root}`);
      const t0 = Date.now();
      for (;;) {
        const f = duration <= 0 ? 1 : Math.min(1, (Date.now() - t0) / duration);
        onProgress?.(f);
        if (f >= 1) break;
        await new Promise((r) => setTimeout(r, step));
      }
      return { freed_bytes: bytes };
    },
    async neverSuggest(root) {
      calls.push(`never ${root}`);
    },
    async reveal(root) {
      calls.push(`reveal ${root}`);
    },
  };
}

const unsupported = (what: string) =>
  new BridgeError({ kind: 'internal', message: `${what} from the app arrives with the Phase 2 executor bridge; use \`scull clean\` for now.` });

export function createPendingProjectActions(): ProjectActions {
  return {
    kind: 'pending',
    cleanRegenerable: async () => Promise.reject(unsupported('Cleaning')),
    neverSuggest: async () => Promise.reject(unsupported('“Never suggest”')),
    reveal: async () => Promise.reject(unsupported('Showing a folder')),
  };
}

export function projectActionsFor(bridge: ScullBridge): ProjectActions {
  return bridge.kind === 'mock' ? createMockProjectActions() : createPendingProjectActions();
}

const GiB = 1024 ** 3;
const g = (x: number) => Math.round(x * GiB);
const DAY = 86400;

/**
 * The design's eight projects (design 06), as a ProjectsView. Used only with the mock bridge
 * and `#/projects?demo=design`, to show the design states; the recorded fixture has no
 * regenerable bytes, because in-use probes are not available in that build.
 */
export function designProjects(now = Math.floor(Date.now() / 1000)): ProjectsView {
  const p = (
    name: string,
    root: string,
    ecosystems: string[],
    days: number,
    t: [number, number, number, number],
    markers: string[],
  ) => {
    const allocated = { s0: g(t[0]), s1: g(t[1]), s2: g(t[2]), s3: g(t[3]) };
    const badge = t[3] > 0 ? 'S3' : t[2] > 0 ? 'S2' : t[1] > 0 ? 'S1' : t[0] > 0 ? 'S0' : null;
    return {
      name,
      root,
      ecosystems,
      markers,
      newest_mtime: now - Math.round(days * DAY),
      allocated,
      reclaimable: { ...allocated, s3: 0 },
      badge: badge as ProjectsView['projects'][number]['badge'],
      candidates: t.filter((x) => x > 0).length,
      git_worktree: root,
      total_allocated: g(t[0] + t[1] + t[2] + t[3]),
    };
  };
  return {
    cancelled: false,
    issues: [],
    roots: ['/Users/ada/code', '/Users/ada/work'],
    signals_unavailable: [],
    projects: [
      p('shop-api', '/Users/ada/code/shop-api', ['node', 'docker'], 3, [12.8, 0, 0, 8.5], ['package.json', 'Dockerfile']),
      p('rust-svc', '/Users/ada/code/rust-svc', ['rust'], 6, [15.2, 0, 0, 9.4], ['Cargo.toml']),
      p('ml-lab', '/Users/ada/code/ml-lab', ['python', 'ml'], 2, [0, 0, 13.7, 5.5], ['pyproject.toml']),
      p('kart-game', '/Users/ada/code/kart-game', ['unity'], 41, [0, 8.2, 0, 4.3], ['ProjectSettings']),
      p('payments', '/Users/ada/work/payments', ['rust'], 1 / 12, [0, 0, 0, 11.2], ['Cargo.toml']),
      p('web-app', '/Users/ada/code/web-app', ['node'], 9, [4.3, 1.2, 0, 4.3], ['package.json']),
      p('old-site', '/Users/ada/code/old-site', ['node'], 214, [2.9, 0, 0, 0.5], ['package.json']),
      p('cli-tools', '/Users/ada/code/cli-tools', ['go'], 63, [0.6, 0, 0, 1.3], ['go.mod']),
    ],
  };
}
