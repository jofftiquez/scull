// Onboarding discovery uses existing read-only reports; native context and folder selection
// are restricted app commands. Survey preferences are saved locally by prefs.ts.
import type { InjectionKey } from 'vue';
import type { ScullBridge } from '../../bridge';
import { createTauriOnboardingCommands, type OnboardingCommands } from '../../bridge/tauri';
import type { Detection, DetectedToolchain } from './model';

export interface OnboardingBridge extends OnboardingCommands {
  readonly kind: 'mock' | 'tauri';
  detect(): Promise<Detection>;
}

export const ONBOARDING_KEY: InjectionKey<OnboardingBridge> = Symbol('scull-onboarding');

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

export function createMockOnboardingBridge(opts: { latencyMs?: number } = {}): OnboardingBridge {
  return {
    kind: 'mock',
    context: async () => ({ home: MOCK_DETECTION.home }),
    pickFolders: async () => ['/Users/ada/Projects'],
    async detect() {
      if (opts.latencyMs) await new Promise<void>((r) => setTimeout(r, opts.latencyMs));
      return structuredClone(MOCK_DETECTION);
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

/** Reports are suggestions only. Home context comes from the native session. */
export function createPendingOnboardingBridge(bridge: ScullBridge, native = createTauriOnboardingCommands()): OnboardingBridge {
  return {
    kind: 'tauri',
    ...native,
    async detect() {
      const [projects, caches] = await Promise.allSettled([bridge.projects(), bridge.caches()]);
      const toolchains: DetectedToolchain[] = [];
      const add = (id: string, build: DetectedToolchain['build'], cache?: string, binary?: string) => {
        let t = toolchains.find((x) => x.id === id);
        if (!t) toolchains.push((t = { id, label: id, build, binaries: [], caches: [] }));
        if (cache && !t.caches.includes(cache)) t.caches.push(cache);
        if (binary && !t.binaries.includes(binary)) t.binaries.push(binary);
      };
      if (caches.status === 'fulfilled') {
        for (const c of caches.value.candidates) {
          const tool = c.action.type === 'tool_command' ? c.action.tool : c.rule.id.split('.')[0] ?? 'cache';
          add(['npm', 'pnpm', 'yarn', 'bun'].includes(tool) ? 'node' : tool, 'web', c.path, tool);
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
        home: '',
        toolchains,
        folders: roots.map((path) => ({ path, repos: counts.get(path) ?? null, suggested: true })),
        ai_clients: [],
      };
    },
  };
}

/** The mock in a plain browser, the derived probe inside Tauri. */
export function onboardingBridgeFor(bridge: ScullBridge): OnboardingBridge {
  return bridge.kind === 'mock' ? createMockOnboardingBridge() : createPendingOnboardingBridge(bridge);
}
