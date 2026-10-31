// Formatting shared by every screen. Byte counts use the CLI's exact rules
// (crates/scull-cli/src/output/bytes.rs: binary units, one decimal above 1 KiB), so the same
// number reads the same in the CLI and the app (PRD principle 6).

const UNITS = ['KiB', 'MiB', 'GiB', 'TiB', 'PiB', 'EiB'] as const;

/** `0 B`, `1023 B`, `1.0 KiB`, `41.2 GiB`. `null` (unknown) reads "size unknown", never 0. */
export function formatBytes(bytes: number | null | undefined): string {
  if (bytes === null || bytes === undefined || !Number.isFinite(bytes) || bytes < 0) {
    return 'size unknown';
  }
  if (bytes < 1024) return `${Math.floor(bytes)} B`;
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1023.95 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(1)} ${UNITS[unit]}`;
}

/** Used share of a volume, 0–100, or `null` when either size is unknown. */
export function usedPercent(total: number | null, free: number | null): number | null {
  if (total === null || free === null || total <= 0 || free < 0 || free > total) return null;
  return Math.round(((total - free) / total) * 100);
}
