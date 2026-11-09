/// <reference types="vitest/config" />
import { resolve } from 'node:path';
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';

// Tauri expects a fixed dev port and a relative base for the bundled app.
// https://v2.tauri.app/start/frontend/vite/
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [react(), tailwindcss()],
  base: './',
  clearScreen: false,
  server: {
    port: Number(process.env.SCULL_DEV_PORT ?? 1420),
    strictPort: true,
    host: host || false,
    // Ignore the Rust bridge only; src/bridge must invalidate live command payloads.
    watch: { ignored: ['**/src-tauri/**', '**/apps/desktop/bridge/**', '**/docker/**', '**/e2e/**'] },
  },
  envPrefix: ['VITE_', 'TAURI_ENV_'],
  build: {
    target: 'es2022',
    // No source maps in the shipped bundle; Tauri serves files locally (no network at runtime).
    sourcemap: false,
    outDir: 'dist',
    // Two pages: the app, and the menu bar widget the tray opens (P4-UI-01).
    rolldownOptions: {
      input: { main: resolve(import.meta.dirname, 'index.html'), menubar: resolve(import.meta.dirname, 'menubar.html') },
    },
  },
  test: {
    environment: 'jsdom',
    include: ['src/**/*.test.{ts,tsx}', 'scripts/**/*.test.ts'],
    // Mock-bridge tests replay timed progress; on a loaded machine 5 s is too tight. Tests
    // must still wait on conditions, never sleeps.
    testTimeout: 20000,
    setupFiles: ['src/test/setup.ts'],
  },
});
