// @vitest-environment node
import { mkdir, mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';
import { createServer } from 'vite';
import config from '../vite.config';

describe('development bridge updates', () => {
  it('invalidates a served frontend bridge module when its source changes', async () => {
    const temporary = await mkdtemp(join(tmpdir(), 'scull-vite-watch-'));
    const root = join(temporary, 'apps', 'desktop');
    const directory = join(root, 'src', 'bridge');
    await mkdir(directory, { recursive: true });
    const path = join(directory, 'confirmation.ts');
    const url = '/src/bridge/confirmation.ts';
    await writeFile(path, 'export const typedCount = null;\n');
    const server = await createServer({
      root, configFile: false, logLevel: 'silent',
      server: { port: 0, host: '127.0.0.1', watch: config.server?.watch },
    });
    try {
      const ready = new Promise<void>((resolve) => server.watcher.once('ready', resolve));
      await server.listen();
      await ready;
      const address = server.httpServer!.address();
      if (!address || typeof address === 'string') throw new Error('No test server port');
      const source = async () => (await fetch(`http://127.0.0.1:${address.port}${url}`)).text();
      expect(await source()).toMatch(/typedCount = null/);
      await writeFile(path, 'export const typedCount = 1;\n');
      await expect.poll(source, { timeout: 3000, interval: 50 }).toMatch(/typedCount = 1/);
    } finally {
      await server.close();
      await rm(temporary, { recursive: true, force: true });
    }
  });
});
