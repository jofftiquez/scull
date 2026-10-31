// Onboarding's two calls that the core bridge does not have yet (P3-UI-03).
//
//   detect()       the metadata-only probe of `scull init` (ONB-01): toolchains, project
//                  folders, AI-client configs. Planned bridge command: `onboarding_detect`.
//   saveProfile()  writes the profile and `scan.roots` like `scull init` does (ONB-03, CFG-05).
//                  Planned bridge command: `onboarding_save_profile`.
//
// Neither deletes anything; they stay out of `ScullBridge` until the Rust commands exist, so
// the core bridge's command table and parity tests are unchanged. In a plain browser the mock
// answers with the design's illustrative machine. Inside Tauri, detection is derived from the
// existing read-only commands (projects, caches) and saving reports that it is not available
// yet, so onboarding still works end to end without writing anything.
import type { ScullBridge } from '../../bridge';
import type { Detection, DetectedToolchain, Profile } from './model';

export interface SaveResult {
  saved: boolean;
  /** Why it was not saved (the command does not exist yet). */
  reason?: string;
}

export interface OnboardingBridge {
  readonly kind: 'mock' | 'pending';
  detect(): Promise<Detection>;
  saveProfile(profile: Profile): Promise<SaveResult>;
}

/** The design's machine (design/source/Onboarding.dc.html), under the mock's /Users/ada. */
export const MOCK_DETECTION: Detection = {
  home: '/Users/ada',
  toolchains: [
    { id: 'rust', label: 'Rust', build: 'rust', binaries: ['/Users/ada/.cargo/bin/cargo'], caches: ['/Users/ada/.cargo/registry'] },
    {
      id: 'node',
      label: 'Node',
      build: 'web',
      binaries: ['/opt/homebrew/bin/npm', '/opt/homebrew/bin/pnpm', '/Users/ada/.bun/bin/bun'],
      caches: ['/Users/ada/.npm'],
    },
    { id: 'python', label: 'Python', build: 'python', binaries: ['/opt/homebrew/bin/uv', '/opt/homebrew/bin/conda'], caches: [] },
    {
      id: 'ml',
      label: 'ML models',
      build: 'ml',
      binaries: ['/usr/local/bin/ollama'],
      caches: ['/Users/ada/.cache/huggingface'],
    },
    { id: 'containers', label: 'Containers', build: 'infra', binaries: ['/usr/local/bin/docker'], caches: [] },
    { id: 'game', label: 'Game engine', build: 'game', binaries: ['/Applications/Unity Hub.app'], caches: [] },
  ],
  folders: [
    { path: '/Users/ada/code', repos: 32, suggested: true },
    { path: '/Users/ada/work', repos: 9, suggested: true },
    { path: '/Users/ada/Downloads/experiments', repos: 3, suggested: false },
  ],
  ai_clients: ['claude-code', 'codex'],
};

export function createMockOnboardingBridge(opts: { latencyMs?: number } = {}): OnboardingBridge & { saved: Profile[] } {
  const wait = () => new Promise<void>((r) => setTimeout(r, opts.latencyMs ?? 0));
  const saved: Profile[] = [];
  return {
    kind: 'mock',
    saved,
    async detect() {
      await wait();
      return structuredClone(MOCK_DETECTION);
    },
    async saveProfile(profile) {
      await wait();
      saved.push(structuredClone(profile));
      return { saved: true };
    },
  };
}

const ECOSYSTEM_BUILD: Record<string, DetectedToolchain['build']> = {
  rust: 'rust',
  node: 'web',
  python: 'python',
  go: 'other',
  java: 'other',
  dotnet: 'other',
  swift: 'mobile',
  flutter: 'mobile',
  cpp: 'native',
};

function parent(p: string): string {
  const i = Math.max(p.lastIndexOf('/'), p.lastIndexOf('\\'));
  return i > 0 ? p.slice(0, i) : p;
}

/**
 * Inside Tauri until the probe command exists: what the last scan and the cache report show.
 * The home folder is guessed from the cache paths (they live under it) and is only used to
 * shorten paths and to recognise broad roots.
 */
export function createPendingOnboardingBridge(bridge: ScullBridge): OnboardingBridge {
  return {
    kind: 'pending',
    async detect() {
      const [projects, caches] = await Promise.allSettled([bridge.projects(), bridge.caches()]);
      const toolchains: DetectedToolchain[] = [];
      const add = (id: string, build: DetectedToolchain['build'], cache?: string, binary?: string) => {
        let t = toolchains.find((x) => x.id === id);
        if (!t) toolchains.push((t = { id, label: id, build, binaries: [], caches: [] }));
        if (cache && !t.caches.includes(cache)) t.caches.push(cache);
        if (binary && !t.binaries.includes(binary)) t.binaries.push(binary);
      };
      let home = '';
      if (caches.status === 'fulfilled') {
        for (const c of caches.value.candidates) {
          const tool = c.action.type === 'tool_command' ? c.action.tool : c.rule.id.split('.')[0] ?? 'cache';
          add(['npm', 'pnpm', 'yarn', 'bun'].includes(tool) ? 'node' : tool, 'web', c.path, tool);
          const m = /^(.*?[\\/](?:Users|home)[\\/][^\\/]+)/.exec(c.path) ?? /^([A-Za-z]:\\Users\\[^\\]+)/.exec(c.path);
          if (m?.[1]) home = m[1];
        }
      }
      const counts = new Map<string, number>();
      if (projects.status === 'fulfilled') {
        for (const p of projects.value.projects) {
          for (const e of p.ecosystems) add(e, ECOSYSTEM_BUILD[e] ?? 'other');
          if (p.git_worktree) counts.set(parent(p.root), (counts.get(parent(p.root)) ?? 0) + 1);
        }
      }
      const roots = projects.status === 'fulfilled' ? projects.value.roots : [];
      return {
        home,
        toolchains,
        folders: roots.map((path) => ({ path, repos: counts.get(path) ?? null, suggested: true })),
        ai_clients: [],
      };
    },
    async saveProfile() {
      return {
        saved: false,
        reason: 'Saving the profile from the app arrives with the onboarding_save_profile command; run `scull init` to write it now.',
      };
    },
  };
}

/** The mock in a plain browser, the derived probe inside Tauri. */
export function onboardingBridgeFor(bridge: ScullBridge): OnboardingBridge {
  return bridge.kind === 'mock' ? createMockOnboardingBridge({ latencyMs: 120 }) : createPendingOnboardingBridge(bridge);
}
