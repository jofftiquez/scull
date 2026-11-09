// Package-manager caches (read-only): where they are, how big, and the tool's own command that
// cleans them. Cleaning caches from the app comes later; `scull clean caches` does it today.
import { useEffect, useState } from 'react';
import type { CachesReport } from './bridge';
import { useBridges } from './bridgeContext';
import { formatBytes } from './lib/format';
import { tierReason } from './lib/plan';
import { Card, ErrorNote, Spinner, TierBadge } from './ui';

export function Caches() {
  const { core } = useBridges();
  const [report, setReport] = useState<CachesReport | null>(null);
  const [error, setError] = useState('');

  useEffect(() => {
    const ctl = new AbortController();
    core
      .caches({ signal: ctl.signal })
      .then(setReport)
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)));
    return () => ctl.abort();
  }, [core]);

  if (error) return <ErrorNote error={error} />;
  if (!report) return <Spinner label="Measuring caches…" />;
  const rows = [...report.candidates].sort((a, b) => b.sizes.reclaimable.bytes - a.sizes.reclaimable.bytes);
  return (
    <Card>
      <h2 className="m-0 text-h3 font-semibold">Package caches</h2>
      <p className="mt-1 text-text-secondary">
        Cleaned only with each tool's own command. Run <code className="font-mono">scull clean caches</code> in a
        terminal to clean them.
      </p>
      {rows.length === 0 ? (
        <p className="text-text-secondary">No package caches found.</p>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full border-collapse text-left text-caption">
            <thead className="text-text-tertiary">
              <tr>
                <th className="py-2 pr-4 font-medium">Cache</th>
                <th className="py-2 pr-4 font-medium">Tier</th>
                <th className="py-2 pr-4 text-right font-medium">Size</th>
                <th className="py-2 font-medium">Why</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((c) => (
                <tr key={c.id} className="border-t border-line align-top">
                  <td className="py-2 pr-4">
                    <div className="text-ink">{c.artifact}</div>
                    <div className="font-mono text-text-tertiary">{c.path}</div>
                  </td>
                  <td className="py-2 pr-4">
                    <TierBadge tier={c.tier} />
                  </td>
                  <td className="whitespace-nowrap py-2 pr-4 text-right font-mono">{formatBytes(c.sizes.reclaimable.bytes)}</td>
                  <td className="py-2 text-text-secondary">{tierReason(c)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Card>
  );
}
