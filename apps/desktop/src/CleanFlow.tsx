// The one cleanup flow (#144): pick folders → scan → review the plan (S0 preselected, S1/S2
// opt-in, S3 never) → confirm → execute → result. Every rule comes from the core: the plan is
// prepared and re-checked there, and execution revalidates each item before touching it.
import { useEffect, useMemo, useRef, useState } from 'react';
import type { ItemId, ProgressEvent, Tier } from './bridge';
import type { ExecutionResult, ItemOutcome, PlanItem, PlanPreview } from './bridge/plan';
import { useBridges } from './bridgeContext';
import { formatBytes } from './lib/format';
import {
  confirmLabel,
  initialSelection,
  isSelectable,
  itemSubline,
  itemTitle,
  planTotals,
  toggleSelection,
  typedConfirmCount,
  undoLabel,
} from './lib/plan';
import { Button, Card, ErrorNote, Spinner, TierBadge } from './ui';

type Phase = 'idle' | 'scanning' | 'review' | 'running' | 'done';

const TIERS: Tier[] = ['S0', 'S1', 'S2'];

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export function CleanFlow() {
  const { core, plans } = useBridges();
  const [roots, setRoots] = useState<string[] | null>(null);
  const [extra, setExtra] = useState('');
  const [phase, setPhase] = useState<Phase>('idle');
  const [progress, setProgress] = useState<ProgressEvent | null>(null);
  const [plan, setPlan] = useState<PlanPreview | null>(null);
  const [sel, setSel] = useState<Set<ItemId>>(new Set());
  const [confirming, setConfirming] = useState(false);
  const [outcomes, setOutcomes] = useState<Record<string, ItemOutcome>>({});
  const [result, setResult] = useState<ExecutionResult | null>(null);
  const [error, setError] = useState('');
  const abort = useRef<AbortController | null>(null);

  useEffect(() => {
    core
      .settings()
      .then((s) => setRoots(s.config.scan?.roots ?? []))
      .catch((e: unknown) => {
        setRoots([]);
        setError(message(e));
      });
  }, [core]);

  async function scan(only?: string) {
    setError('');
    setPlan(null);
    setResult(null);
    setOutcomes({});
    setPhase('scanning');
    abort.current = new AbortController();
    try {
      const report = await core.scan(only ? { roots: [only] } : {}, {
        onProgress: setProgress,
        signal: abort.current.signal,
      });
      if (report.cancelled) {
        setPhase('idle');
        return;
      }
      const p = await plans.preparePlan({});
      setPlan(p);
      setSel(initialSelection(p.items));
      setPhase('review');
    } catch (e) {
      setError(message(e));
      setPhase('idle');
    } finally {
      setProgress(null);
    }
  }

  const items = plan?.items ?? [];
  const totals = useMemo(() => planTotals(items, sel), [items, sel]);
  const chosen = items.filter((i) => sel.has(i.candidate.id) && isSelectable(i.candidate));
  const typedRequired = typedConfirmCount(items, sel);

  async function execute(typed?: number) {
    if (!plan) return;
    setConfirming(false);
    setError('');
    setPhase('running');
    try {
      const r = await plans.executePlan(
        {
          plan_id: plan.plan_id,
          hash: plan.hash,
          items: chosen.map((i) => i.candidate.id),
          ...(typed ? { typed_count: typed } : {}),
        },
        {
          onEvent: (e) => {
            if (e.kind === 'item_finished') setOutcomes((o) => ({ ...o, [e.outcome.item_id]: e.outcome }));
          },
        },
      );
      setResult(r);
      setPhase('done');
    } catch (e) {
      setError(message(e));
      setPhase('review');
    }
  }

  return (
    <div className="flex flex-col gap-5">
      {error && <ErrorNote error={error} />}

      {(phase === 'idle' || phase === 'scanning') && (
        <Card>
          <h2 className="m-0 text-h3 font-semibold">Folders to scan</h2>
          {roots === null ? (
            <Spinner label="Reading settings…" />
          ) : roots.length > 0 ? (
            <ul className="my-3 flex flex-col gap-1 p-0 font-mono text-caption text-text-secondary" aria-label="Scan folders">
              {roots.map((r) => (
                <li key={r} className="list-none">
                  {r}
                </li>
              ))}
            </ul>
          ) : (
            <p className="my-3 text-text-secondary">
              No folders configured yet. Run <code className="font-mono">scull init</code> in a terminal, or scan one folder below.
            </p>
          )}
          <div className="flex flex-wrap items-center gap-3">
            <Button variant="primary" disabled={phase === 'scanning' || !roots?.length} onClick={() => void scan()}>
              Scan
            </Button>
            <span className="text-text-tertiary">or</span>
            <form
              className="flex flex-1 items-center gap-2"
              onSubmit={(e) => {
                e.preventDefault();
                if (extra.trim()) void scan(extra.trim());
              }}
            >
              <label className="sr-only" htmlFor="folder">
                Folder to scan
              </label>
              <input
                id="folder"
                value={extra}
                onChange={(e) => setExtra(e.target.value)}
                placeholder="/path/to/a/folder"
                className="min-w-0 flex-1 rounded-button border border-border bg-surface px-3 py-2 font-mono text-caption"
              />
              <Button type="submit" disabled={phase === 'scanning' || !extra.trim()}>
                Scan folder
              </Button>
            </form>
          </div>
          {phase === 'scanning' && (
            <div className="mt-4 flex items-center gap-4">
              <Spinner
                label={
                  progress?.phase === 'scanning'
                    ? `Scanning… ${(progress.entries ?? 0).toLocaleString()} entries`
                    : 'Analysing…'
                }
              />
              <Button variant="ghost" onClick={() => abort.current?.abort()}>
                Cancel
              </Button>
            </div>
          )}
        </Card>
      )}

      {plan && phase !== 'idle' && phase !== 'scanning' && (
        <>
          <div className="flex flex-wrap items-baseline justify-between gap-3">
            <h2 className="m-0 text-h2 font-semibold">
              {phase === 'done' ? 'Done' : items.length === 0 ? 'Nothing to clean' : 'Review the cleanup'}
            </h2>
            <Button variant="ghost" disabled={phase === 'running'} onClick={() => setPhase('idle')}>
              Scan again
            </Button>
          </div>

          {result && <ResultSummary result={result} items={items} />}

          {TIERS.map((tier) => {
            const rows = items.filter((i) => i.candidate.tier === tier);
            if (rows.length === 0) return null;
            return (
              <Card key={tier}>
                <h3 className="m-0 mb-3 flex items-center gap-3 text-h3 font-semibold">
                  <TierBadge tier={tier} />
                  <span className="text-label font-normal text-text-secondary">
                    {tier === 'S0' ? 'regenerable, selected' : tier === 'S1' ? 'costly to recreate, opt in' : 'uncertain, opt in per item'}
                  </span>
                </h3>
                <ul className="m-0 flex flex-col divide-y divide-line p-0">
                  {rows.map((it) => (
                    <Row
                      key={it.candidate.id}
                      item={it}
                      checked={sel.has(it.candidate.id)}
                      disabled={phase !== 'review'}
                      outcome={outcomes[it.candidate.id]}
                      onToggle={(on) => setSel((s) => toggleSelection(s, it, on))}
                    />
                  ))}
                </ul>
              </Card>
            );
          })}

          {plan.left_out.length > 0 && (
            <details className="rounded-card border border-line bg-surface-subtle px-5 py-3">
              <summary className="cursor-pointer text-text-secondary">
                {plan.left_out.length} protected item(s) left out (never cleaned)
              </summary>
              <ul className="mb-0 mt-2 flex flex-col gap-1 pl-4 text-caption text-text-secondary">
                {plan.left_out.map((l) => (
                  <li key={l.label}>
                    <span className="font-mono">{l.label}</span>: {l.reason}
                  </li>
                ))}
              </ul>
            </details>
          )}

          {phase !== 'done' && items.length > 0 && (
            <div className="sticky bottom-0 flex flex-wrap items-center justify-between gap-3 rounded-card border border-line bg-surface px-5 py-4">
              <span className="text-text-secondary">
                {totals.count} selected · <strong className="text-ink">{formatBytes(totals.bytes)}</strong>
                {totals.quarantineCount > 0 && ` · ${totals.quarantineCount} restorable (quarantine)`}
              </span>
              {phase === 'running' ? (
                <Spinner label={`Cleaning… ${Object.keys(outcomes).length} of ${chosen.length}`} />
              ) : (
                <Button variant="primary" disabled={totals.count === 0} onClick={() => setConfirming(true)}>
                  {confirmLabel(totals)}
                </Button>
              )}
            </div>
          )}
        </>
      )}

      {confirming && (
        <ConfirmDialog
          summary={[
            `${totals.count} item(s), ${formatBytes(totals.bytes)}.`,
            totals.deleteCount > 0 && `${totals.deleteCount} deleted (regenerate them with your tools).`,
            totals.quarantineCount > 0 && `${totals.quarantineCount} moved to quarantine (restorable).`,
          ]
            .filter(Boolean)
            .join(' ')}
          typedRequired={typedRequired}
          onCancel={() => setConfirming(false)}
          onConfirm={(typed) => void execute(typed)}
        />
      )}
    </div>
  );
}

function Row({
  item,
  checked,
  disabled,
  outcome,
  onToggle,
}: {
  item: PlanItem;
  checked: boolean;
  disabled: boolean;
  outcome: ItemOutcome | undefined;
  onToggle: (on: boolean) => void;
}) {
  const c = item.candidate;
  const id = `item-${c.id}`;
  const selectable = isSelectable(c);
  return (
    <li className="flex list-none items-start gap-3 py-3">
      <input
        id={id}
        type="checkbox"
        className="mt-1 h-4 w-4 accent-[var(--color-accent)]"
        checked={checked && selectable}
        disabled={disabled || !selectable}
        onChange={(e) => onToggle(e.target.checked)}
      />
      <label htmlFor={id} className="flex min-w-0 flex-1 flex-col gap-0.5">
        <span className="truncate font-mono text-caption text-ink">{itemTitle(item)}</span>
        <span className="text-caption text-text-tertiary">
          {itemSubline(item)} · {undoLabel(c)}
        </span>
      </label>
      <span className="whitespace-nowrap font-mono text-caption">{formatBytes(c.sizes.reclaimable.bytes)}</span>
      {outcome && (
        <span
          className={`whitespace-nowrap text-caption ${outcome.outcome === 'skipped' ? 'text-amber-text' : 'text-tier-s0-fg'}`}
          title={outcome.reason}
        >
          {outcome.outcome}
        </span>
      )}
    </li>
  );
}

function ResultSummary({ result, items }: { result: ExecutionResult; items: PlanItem[] }) {
  const done = result.items.filter((o) => o.outcome !== 'skipped');
  const skipped = result.items.filter((o) => o.outcome === 'skipped');
  const title = (id: ItemId) => {
    const it = items.find((i) => i.candidate.id === id);
    return it ? itemTitle(it) : id;
  };
  return (
    <Card className="border-tier-s0">
      <p className="m-0 text-h3 font-semibold" role="status">
        {done.length} of {result.items.length} item(s) cleaned · {formatBytes(result.measured_freed_bytes)} freed
      </p>
      {result.moved_bytes > 0 && (
        <p className="mb-0 mt-2 text-text-secondary">
          {formatBytes(result.moved_bytes)} moved to quarantine or the Trash. Restore with{' '}
          <code className="font-mono">scull restore</code>.
        </p>
      )}
      {skipped.length > 0 && (
        <ul className="mb-0 mt-3 flex flex-col gap-1 pl-4 text-caption text-amber-text">
          {skipped.map((o) => (
            <li key={o.item_id}>
              Skipped <span className="font-mono">{title(o.item_id)}</span>
              {o.reason ? `: ${o.reason}` : ''}
            </li>
          ))}
        </ul>
      )}
    </Card>
  );
}

function ConfirmDialog({
  summary,
  typedRequired,
  onCancel,
  onConfirm,
}: {
  summary: string;
  typedRequired: number;
  onCancel: () => void;
  onConfirm: (typed?: number) => void;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  const [typed, setTyped] = useState('');
  useEffect(() => {
    ref.current?.showModal();
  }, []);
  const ok = typedRequired === 0 || typed.trim() === String(typedRequired);
  return (
    <dialog
      ref={ref}
      aria-labelledby="confirm-title"
      onCancel={onCancel}
      className="m-auto max-w-lg rounded-card border border-line bg-surface p-6 text-ink backdrop:bg-ink/40"
    >
      <h2 id="confirm-title" className="m-0 text-h3 font-semibold">
        Clean these items?
      </h2>
      <p className="text-text-secondary">{summary}</p>
      <p className="text-caption text-text-tertiary">
        Each item is checked again just before it is touched; anything that changed is skipped.
      </p>
      {typedRequired > 0 && (
        <label className="mt-3 flex flex-col gap-1 text-label">
          {typedRequired} item(s) can't be undone by scull (S1/S2). Type {typedRequired} to confirm them:
          <input
            autoFocus
            inputMode="numeric"
            value={typed}
            onChange={(e) => setTyped(e.target.value)}
            className="rounded-button border border-border bg-surface px-3 py-2 font-mono"
          />
        </label>
      )}
      <div className="mt-5 flex justify-end gap-3">
        <Button variant="ghost" onClick={onCancel}>
          Cancel
        </Button>
        <Button variant="primary" disabled={!ok} onClick={() => onConfirm(typedRequired ? typedRequired : undefined)}>
          Clean
        </Button>
      </div>
    </dialog>
  );
}

