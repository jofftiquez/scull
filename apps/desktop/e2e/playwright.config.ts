// Playwright E2E for the desktop app's key flows (P3-E2E-01), against the Vite server with the
// mock bridge. Run it through `e2e/run.sh`, which builds and serves inside the official
// Playwright container (the browsers come from the image; nothing is downloaded on install).
//
//   E2E_SERVER=preview (default)  serve the E2E build in e2e/.dist (`vite preview`)
//   E2E_SERVER=dev                the Vite dev server
//   E2E_BASE_URL=http://…         use a server that is already running (no webServer)
//
// The mock bridge touches nothing on disk, and every request outside the app's own origin is
// aborted (see fixtures.ts), so a run can't delete, move or download anything.
import { defineConfig, devices } from '@playwright/test';

const port = Number(process.env.E2E_PORT ?? 4317);
const server = process.env.E2E_SERVER === 'dev' ? 'dev' : 'preview';
const external = process.env.E2E_BASE_URL;
const vite = 'node node_modules/vite/bin/vite.js';

export default defineConfig({
  testDir: '.',
  testMatch: '*.spec.ts',
  outputDir: 'results',
  fullyParallel: true,
  forbidOnly: Boolean(process.env.CI),
  // Flows must pass on their own: a retry would hide a flaky wait.
  retries: 0,
  workers: Number(process.env.E2E_WORKERS ?? 3),
  timeout: 60_000,
  expect: { timeout: 15_000 },
  reporter: [['list'], ['html', { outputFolder: 'report', open: 'never' }]],
  use: {
    ...devices['Desktop Chrome'],
    baseURL: external ?? `http://localhost:${port}`,
    // The design's frame (the design mockups).
    viewport: { width: 1440, height: 900 },
    locale: 'en-US',
    timezoneId: 'UTC',
    reducedMotion: 'no-preference',
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },
  projects: [{ name: 'chromium' }],
  webServer: external
    ? undefined
    : {
        command:
          server === 'dev'
            ? `${vite} --port ${port} --strictPort`
            : `${vite} preview --outDir e2e/.dist --port ${port} --strictPort`,
        cwd: '..',
        url: `http://localhost:${port}/`,
        reuseExistingServer: false,
        timeout: 60_000,
        stdout: 'ignore',
        stderr: 'pipe',
      },
});
