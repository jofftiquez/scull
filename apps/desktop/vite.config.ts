/// <reference types="vitest/config" />
import { defineConfig } from 'vite';
import vue from '@vitejs/plugin-vue';
import tailwindcss from '@tailwindcss/vite';

// Tauri expects a fixed dev port and a relative base for the bundled app.
// https://v2.tauri.app/start/frontend/vite/
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [vue(), tailwindcss()],
  base: './',
  clearScreen: false,
  server: {
    port: Number(process.env.SCULL_DEV_PORT ?? 1420),
    strictPort: true,
    host: host || false,
    watch: { ignored: ['**/src-tauri/**', '**/bridge/**', '**/docker/**'] },
  },
  envPrefix: ['VITE_', 'TAURI_ENV_'],
  build: {
    target: 'es2022',
    // No source maps in the shipped bundle; Tauri serves files locally (no network at runtime).
    sourcemap: false,
    outDir: 'dist',
  },
  test: {
    environment: 'jsdom',
    include: ['src/**/*.test.ts', 'scripts/**/*.test.ts'],
  },
});
