// Mock/real parity: the mock bridge answers with data of exactly the real bridge's shape.
// Everything is validated against apps/desktop/schemas (generated from the Rust payload types;
// the shared ones equal the CLI's schemas, checked by apps/desktop/bridge/tests/schemas.rs).
import { readdirSync, readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import Ajv2020 from 'ajv/dist/2020.js';
import { describe, expect, it } from 'vitest';
import { renderTypes, TYPES_PATH } from '../../scripts/gen-types.mjs';
import { createMockBridge, MOCK_DATA } from './mock';
import { BridgeError, COMMANDS } from './types';

const SCHEMAS = resolve(__dirname, '../../schemas');
const FIXTURES = resolve(__dirname, 'fixtures');

const ajv = new Ajv2020({ allErrors: true, strict: false, validateFormats: false });
const validators = new Map<string, ReturnType<typeof ajv.compile>>();
function validator(stem: string) {
  let v = validators.get(stem);
  if (!v) {
    const schema = JSON.parse(readFileSync(resolve(SCHEMAS, `${stem}.schema.json`), 'utf8'));
    v = ajv.compile(schema);
    validators.set(stem, v);
  }
  return v;
}
function expectValid(stem: string, data: unknown) {
  const v = validator(stem);
  const ok = v(data);
  expect(ok ? [] : v.errors, `${stem} payload`).toEqual([]);
}

describe('generated types', () => {
  it('src/bridge/generated.ts is up to date with apps/desktop/schemas (run `pnpm types`)', async () => {
    expect(readFileSync(TYPES_PATH, 'utf8')).toBe(await renderTypes());
  });
});

describe('recorded real payloads (fixtures)', () => {
  const schemaOf: Record<string, string> = {
    'scan-report': 'scan-report',
    projects: 'projects',
    worktrees: 'worktrees',
    'caches-report': 'caches-report',
    status: 'status',
    capabilities: 'capabilities',
  };
  for (const file of readdirSync(FIXTURES).filter((f) => f.endsWith('.json'))) {
    const stem = file.replace(/\.json$/, '');
    it(`${file} validates against its schema`, () => {
      const data = JSON.parse(readFileSync(resolve(FIXTURES, file), 'utf8'));
      if (stem === 'progress-events') {
        expect(data.length).toBeGreaterThan(0);
        for (const e of data) expectValid('progress-event', e);
      } else {
        expect(schemaOf[stem], `schema for ${file}`).toBeDefined();
        expectValid(schemaOf[stem]!, data);
      }
    });
  }

  it('carries no machine paths', () => {
    for (const file of readdirSync(FIXTURES)) {
      const text = readFileSync(resolve(FIXTURES, file), 'utf8');
      expect(text).not.toMatch(/\/tmp\/|\/home\/|scull-fx-|\\\\Users\\\\/);
    }
  });
});

describe('mock bridge answers in the real shapes', () => {
  const b = createMockBridge();

  it('every call validates', async () => {
    expectValid('app-info', await b.appInfo());
    expectValid('drives', await b.drives());
    expectValid('scan-report', await b.scan({}));
    expectValid('projects', await b.projects());
    expectValid('worktrees', await b.worktrees());
    expectValid('caches-report', await b.caches());
    expectValid('status', await b.status());
    expectValid('capabilities', await b.capabilities());
    expectValid('history', await b.history());
  });

  it('streams progress events that validate, tagged with one operation ID', async () => {
    const events: unknown[] = [];
    await b.scan({}, { onProgress: (e) => events.push(e) });
    expect(events.length).toBe(MOCK_DATA.progressEvents.length);
    const ids = new Set(events.map((e) => (e as { op_id: string }).op_id));
    expect(ids.size).toBe(1);
    for (const e of events) expectValid('progress-event', e);
  });

  it('cancels like the real one: the report comes back marked cancelled', async () => {
    const ctl = new AbortController();
    ctl.abort();
    const r = await b.scan({}, { signal: ctl.signal });
    expect(r.cancelled).toBe(true);
    expectValid('scan-report', r);
  });

  it('rejects with a ScullError-shaped BridgeError', async () => {
    const err = await b.scan({ roots: ['relative'] }).catch((e) => e);
    expect(err).toBeInstanceOf(BridgeError);
    expectValid('error', (err as BridgeError).error);
  });

  it('implements every command of the real bridge', () => {
    for (const method of Object.keys(COMMANDS)) {
      expect(typeof (b as unknown as Record<string, unknown>)[method]).toBe('function');
    }
  });
});
