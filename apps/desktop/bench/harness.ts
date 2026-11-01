// Shared page harness: builds the scenario's data, times the render, and records hover latency
// for bench/run.mjs. Each library page (echarts-*.ts, d3.ts, baseline.ts) passes a Renderer.
import { aggregate, makeTree, nodeCount, type Tree } from './data';
import { cssVars } from './theme';

export type Scenario = 'insights' | 'wheel-full' | 'wheel-agg' | 'treemap-full' | 'treemap-agg';

export interface Handle {
  /** Element the driver hovers over (its bounding box). */
  hover: Element;
  /** Resolves when entry animations are done (immediately when there are none). */
  settled: Promise<void>;
  /** Apply the alternate palette; resolves after the change is painted. */
  retheme(): Promise<void>;
}

export interface Renderer {
  name: string;
  insights(root: HTMLElement, reduced: boolean): Handle;
  wheel(root: HTMLElement, tree: Tree, reduced: boolean): Handle;
  treemap(root: HTMLElement, tree: Tree, reduced: boolean): Handle;
}

export interface BenchResult {
  lib: string;
  scenario: Scenario;
  reduced: boolean;
  nodes: number;
  /** Building the dataset (same code for every library). */
  dataMs: number;
  /** The synchronous render call. */
  renderCallMs: number;
  /** Render call → first painted frame. */
  firstFrameMs: number;
  /** Navigation start → first painted frame (includes parsing the bundle). */
  sinceNavMs: number;
  /** Render call → entry animations finished. */
  settledMs: number;
  /** Alternate palette applied and painted. */
  rethemeMs?: number;
  domNodes: number;
  /** Per hover: event handler time and event → next painted frame. */
  hovers: { handlerMs: number; frameMs: number }[];
}

declare global {
  interface Window {
    __bench?: BenchResult;
    __benchRetheme?: () => Promise<number>;
  }
}

/** Resolves after the next frame has been produced (rAF, then a task after it). */
export function nextFrame(): Promise<void> {
  return new Promise((resolve) => {
    requestAnimationFrame(() => {
      const ch = new MessageChannel();
      ch.port1.onmessage = () => resolve();
      ch.port2.postMessage(0);
    });
  });
}

export async function runBench(renderer: Renderer | null, libName = renderer?.name ?? 'baseline'): Promise<void> {
  const params = new URLSearchParams(location.search);
  const scenario = (params.get('scenario') ?? 'insights') as Scenario;
  const leaves = Number(params.get('leaves') ?? 100_000);
  const reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
  document.documentElement.setAttribute('style', cssVars());
  const root = document.getElementById('root')!;

  const d0 = performance.now();
  let tree: Tree | null = null;
  if (scenario !== 'insights') {
    tree = makeTree(leaves);
    if (scenario.endsWith('-agg')) tree = aggregate(tree);
  }
  const dataMs = performance.now() - d0;

  const t0 = performance.now();
  let handle: Handle | null = null;
  if (renderer) {
    if (scenario === 'insights') handle = renderer.insights(root, reduced);
    else if (scenario.startsWith('wheel')) handle = renderer.wheel(root, tree!, reduced);
    else handle = renderer.treemap(root, tree!, reduced);
  }
  const t1 = performance.now();
  await nextFrame();
  const t2 = performance.now();
  if (handle) await handle.settled;
  const t3 = performance.now();

  const result: BenchResult = {
    lib: libName,
    scenario,
    reduced,
    nodes: tree ? nodeCount(tree) : 0,
    dataMs,
    renderCallMs: t1 - t0,
    firstFrameMs: t2 - t0,
    sinceNavMs: t2,
    settledMs: t3 - t0,
    domNodes: document.getElementsByTagName('*').length,
    hovers: [],
  };

  // Hover latency: one mouse move fires pointermove then mousemove. The pointermove capture
  // listener runs before any library handler (D3 listens to pointer events, zrender to mouse
  // events), the mousemove bubble listener after all of them; the frame time is measured to the
  // next painted frame.
  let start = 0;
  addEventListener('pointermove', () => (start = performance.now()), { capture: true });
  addEventListener('mousemove', () => {
    const handlerMs = performance.now() - start;
    const s = start;
    void nextFrame().then(() => result.hovers.push({ handlerMs, frameMs: performance.now() - s }));
  });

  if (handle) {
    const h = handle;
    h.hover.setAttribute('data-bench-hover', '');
    window.__benchRetheme = async () => {
      const r0 = performance.now();
      await h.retheme();
      result.rethemeMs = performance.now() - r0;
      return result.rethemeMs;
    };
  }
  window.__bench = result;
}
