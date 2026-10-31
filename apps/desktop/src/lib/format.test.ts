import { describe, expect, it } from 'vitest';
import { formatBytes, usedPercent } from './format';

describe('formatBytes', () => {
  // Same cases as crates/scull-cli/src/output/bytes.rs (one engine, one way to print sizes).
  it.each([
    [0, '0 B'],
    [999, '999 B'],
    [1023, '1023 B'],
    [1024, '1.0 KiB'],
    [1536, '1.5 KiB'],
    [1024 * 1024 - 1, '1.0 MiB'],
    [Math.round(41.2 * 1024 ** 3), '41.2 GiB'],
    [5 * 1024 ** 4, '5.0 TiB'],
  ])('%d -> %s', (n, s) => {
    expect(formatBytes(n)).toBe(s);
  });

  it('never shows unknown as 0', () => {
    expect(formatBytes(null)).toBe('size unknown');
    expect(formatBytes(undefined)).toBe('size unknown');
    expect(formatBytes(Number.NaN)).toBe('size unknown');
    expect(formatBytes(-1)).toBe('size unknown');
  });
});

describe('usedPercent', () => {
  it('rounds the used share', () => {
    expect(usedPercent(494, 41.2)).toBe(92);
    expect(usedPercent(1000, 390)).toBe(61);
  });
  it('is null when a size is unknown or inconsistent', () => {
    expect(usedPercent(null, 1)).toBeNull();
    expect(usedPercent(10, null)).toBeNull();
    expect(usedPercent(0, 0)).toBeNull();
    expect(usedPercent(10, 11)).toBeNull();
  });
});
