// Chart benchmark build (P3-CHART-01). Not part of the app: the app's `vite build` never sees
// these pages and nothing in src/ imports bench/. Run with `node bench/bench.mjs` (docs/CHARTS.md).
import { resolve } from 'node:path';
import { defineConfig } from 'vite';

export const PAGES = ['baseline', 'd3', 'echarts-canvas', 'echarts-svg'];

export default defineConfig({
  root: import.meta.dirname,
  base: './',
  logLevel: 'warn',
  build: {
    target: 'es2022',
    outDir: resolve(import.meta.dirname, 'dist'),
    emptyOutDir: true,
    manifest: true,
    sourcemap: false,
    rollupOptions: {
      input: Object.fromEntries(PAGES.map((p) => [p, resolve(import.meta.dirname, `${p}.html`)])),
    },
  },
});
