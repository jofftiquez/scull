<script setup lang="ts">
// Cleanup Review, the user's own plan (design 03 / 03b): review → running → done.
//
// Nothing changes until the user confirms. Confirming calls the plan bridge's `executePlan`
// STUB (src/bridge/plan.ts): in the browser the mock simulates it; inside Tauri it rejects,
// because the desktop never deletes and the core Executor is not connected yet (SAFE-10).
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';
import { RouterLink } from 'vue-router';
import { toBridgeError, useBridge, type ItemId } from '../../bridge';
import { usePlanBridge, type ExecutionResult, type ItemOutcome, type PlanPreview } from '../../bridge/plan';
import { formatBytes } from '../../lib/format';
import { rescan } from '../../state/core';
import PlanRow from '../../components/review/PlanRow.vue';
import SafetyChecks from '../../components/review/SafetyChecks.vue';
import StatTile from '../../components/review/StatTile.vue';
import SuccessMark from '../../components/review/SuccessMark.vue';
import TierBadge from '../../components/review/TierBadge.vue';
import ReviewHeader from './ReviewHeader.vue';
import { downloadJson } from '../../components/review/download';
import {
  confirmLabel,
  displayPath,
  duration,
  initialSelection,
  isSelectable,
  itemTitle,
  planTotals,
  shortId,
  toggleSelection,
  undoLabel,
} from '../../components/review/plan';
import '../../components/review/review.css';

const bridge = useBridge();
const plans = usePlanBridge();

type Phase = 'loading' | 'review' | 'running' | 'done' | 'error';
const phase = ref<Phase>('loading');
const plan = ref<PlanPreview | null>(null);
const sel = ref<Set<ItemId>>(new Set());
const outcomes = ref<Record<ItemId, ItemOutcome>>({});
const active = ref<ItemId | null>(null);
const gone = ref<Set<ItemId>>(new Set());
const result = ref<ExecutionResult | null>(null);
const error = ref('');
/** The selection as it was when cleaning started (totals stay put while rows sweep out). */
const chosen = ref<ItemId[]>([]);
const timers: ReturnType<typeof setTimeout>[] = [];
let abort: AbortController | null = null;

async function load() {
  phase.value = 'loading';
  error.value = '';
  outcomes.value = {};
  gone.value = new Set();
  result.value = null;
  active.value = null;
  try {
    const p = await plans.preparePlan({});
    plan.value = p;
    sel.value = initialSelection(p.items);
    phase.value = 'review';
  } catch (e) {
    error.value = toBridgeError(e).message;
    phase.value = 'error';
  }
}
onMounted(load);
onBeforeUnmount(() => {
  timers.forEach(clearTimeout);
  abort?.abort();
});

const items = computed(() => plan.value?.items ?? []);
const running = computed(() => phase.value === 'running');
const done = computed(() => phase.value === 'done');
const totals = computed(() =>
  planTotals(items.value, phase.value === 'review' || phase.value === 'loading' ? sel.value : new Set(chosen.value)),
);
const finishedBytes = computed(() =>
  Object.values(outcomes.value).reduce((a, o) => a + o.freed_bytes + o.moved_bytes, 0),
);
const progress = computed(() => {
  if (phase.value === 'done') return 1;
  if (!running.value || chosen.value.length === 0) return 0;
  return Object.keys(outcomes.value).length / chosen.value.length;
});
const visibleItems = computed(() => items.value.filter((i) => !gone.value.has(i.candidate.id)));
const activeItem = computed(() => items.value.find((i) => i.candidate.id === active.value));

const subline = computed(() => {
  const p = plan.value;
  if (!p) return 'preparing a plan from the last scan · nothing changes until you confirm';
  const now = Date.now();
  return [
    `plan ${shortId(p.plan_id)}`,
    `created ${duration(now - Date.parse(p.created_at))} ago`,
    `expires in ${duration(Date.parse(p.expires_at) - now)}`,
    done.value ? 'cleaned' : 'nothing changes until you confirm',
  ].join(' · ');
});

const confirmTitle = computed(() => {
  if (running.value) return activeItem.value ? `Cleaning ${itemTitle(activeItem.value)}…` : 'Cleaning…';
  if (done.value) return 'Cleanup complete';
  const t = totals.value;
  return t.count ? `Clean ${t.count} ${t.count === 1 ? 'item' : 'items'} · ${formatBytes(t.bytes)} predicted` : 'Select items to clean';
});
const buttonLabel = computed(() => {
  if (running.value) return `${Math.round(progress.value * 100)}% · cleaning`;
  if (done.value) return 'Done';
  return confirmLabel(totals.value);
});

function toggle(id: ItemId, on: boolean) {
  if (phase.value !== 'review') return;
  const it = items.value.find((i) => i.candidate.id === id);
  if (it) sel.value = toggleSelection(sel.value, it, on);
}

async function start() {
  const p = plan.value;
  if (!p || phase.value !== 'review' || totals.value.count === 0) return;
  chosen.value = items.value.filter((i) => sel.value.has(i.candidate.id) && isSelectable(i.candidate)).map((i) => i.candidate.id);
  phase.value = 'running';
  abort = new AbortController();
  try {
    const r = await plans.executePlan(
      { plan_id: p.plan_id, hash: p.hash, items: chosen.value },
      {
        signal: abort.signal,
        onEvent: (e) => {
          if (e.kind === 'item_started') active.value = e.item_id;
          else {
            const id = e.outcome.item_id;
            outcomes.value = { ...outcomes.value, [id]: e.outcome };
            if (active.value === id) active.value = null;
            // The row sweeps out (520ms), then leaves the list.
            if (e.outcome.outcome !== 'skipped') {
              timers.push(setTimeout(() => (gone.value = new Set([...gone.value, id])), 560));
            }
          }
        },
      },
    );
    result.value = r;
    timers.push(setTimeout(() => (phase.value = 'done'), 420));
  } catch (e) {
    error.value = toBridgeError(e).message;
    phase.value = 'review';
  } finally {
    active.value = null;
  }
}

function rowState(id: ItemId): Record<string, boolean> {
  const o = outcomes.value[id];
  return {
    'row-active': active.value === id,
    'row-sweep': !!o && o.outcome !== 'skipped',
    'bg-surface-subtle': !sel.value.has(id) && active.value !== id,
  };
}

const measured = computed(() => result.value?.measured_freed_bytes ?? 0);
const predictedFreed = computed(() => {
  const r = result.value;
  if (!r) return 0;
  const freedIds = new Set(r.items.filter((o) => o.outcome === 'deleted' || o.outcome === 'removed').map((o) => o.item_id));
  return items.value.filter((i) => freedIds.has(i.candidate.id)).reduce((a, i) => a + i.candidate.sizes.reclaimable.bytes, 0);
});
const accuracy = computed(() => (predictedFreed.value > 0 ? Math.round((measured.value / predictedFreed.value) * 100) : null));
const doneCount = computed(() => result.value?.items.filter((o) => o.outcome !== 'skipped').length ?? 0);
const quarantined = computed(() => result.value?.items.filter((o) => o.outcome === 'quarantined' || o.outcome === 'trashed').length ?? 0);

function exportPlan() {
  const p = plan.value;
  if (p) downloadJson(`scull-plan-${p.plan_id}.json`, { ...p, selected: [...sel.value] });
}

async function doRescan() {
  await rescan(bridge);
  await load();
}

const btn = 'inline-flex h-11 cursor-pointer items-center rounded-button border border-border bg-surface px-5 font-medium text-ink hover:border-ink disabled:cursor-not-allowed disabled:opacity-60';
</script>

<template>
  <ReviewHeader :subline="subline">
    <button type="button" :class="btn" :disabled="!plan" @click="exportPlan">Export plan</button>
    <button type="button" :class="btn" :disabled="running" @click="doRescan">Rescan</button>
  </ReviewHeader>

  <div class="grid grid-cols-12 gap-page-gap">
    <section
      v-if="done && result"
      aria-label="Cleanup complete"
      aria-live="polite"
      class="tile relative col-span-12 flex flex-wrap items-center gap-7 overflow-hidden rounded-tile bg-surface p-8"
      style="animation: scull-rise 320ms ease-out"
      data-testid="done"
    >
      <SuccessMark burst />
      <div class="flex flex-[1_1_320px] flex-col gap-1">
        <span class="text-[48px] leading-none font-bold tracking-[-0.03em]" style="animation: scull-bounce 640ms 200ms cubic-bezier(.2,.9,.3,1.3) both">
          {{ formatBytes(measured) }} freed
        </span>
        <span class="text-[16px]" style="animation: scull-rise 420ms 700ms ease-out both">
          Measured <strong>{{ formatBytes(measured) }}</strong>
          <template v-if="accuracy !== null"> · {{ accuracy }}% of the {{ formatBytes(predictedFreed) }} predicted</template>
          <template v-if="result.moved_bytes"> · <strong>{{ formatBytes(result.moved_bytes) }}</strong> moved to quarantine</template>
          · {{ doneCount }} {{ doneCount === 1 ? 'item' : 'items' }}
        </span>
        <span class="text-label text-text-secondary" style="animation: scull-rise 420ms 900ms ease-out both">
          {{
            quarantined
              ? `${quarantined} quarantined item${quarantined > 1 ? 's' : ''} can be restored for 7 days from History.`
              : 'Deleted items are rebuilt by your tools the next time you build.'
          }}
        </span>
      </div>
      <div class="flex flex-wrap gap-2" style="animation: scull-rise 420ms 1000ms ease-out both">
        <button type="button" class="min-h-12 cursor-pointer rounded-button border border-border bg-surface px-5 font-medium text-ink" @click="load">
          Review again
        </button>
        <RouterLink to="/" class="inline-flex min-h-12 items-center rounded-button bg-ink px-[22px] font-semibold text-on-ink no-underline hover:text-on-ink">
          Back to overview
        </RouterLink>
      </div>
    </section>

    <section
      v-if="phase === 'error'"
      role="alert"
      class="tile col-span-12 flex flex-col gap-2 rounded-tile border border-accent-tint-border bg-accent-tint p-tile-padding"
    >
      <h2 class="m-0 text-h3 font-semibold">No plan to review</h2>
      <p class="m-0 text-label text-text-strong-muted">{{ error }}</p>
    </section>

    <template v-if="plan">
      <StatTile
        :label="running || done ? (totals.quarantineCount ? 'Cleaned so far' : 'Freed so far') : 'Selected'"
        :value="formatBytes(running || done ? finishedBytes : totals.bytes)"
        :sub="done ? 'All done' : running ? `of ${formatBytes(totals.bytes)}` : `${totals.count} ${totals.count === 1 ? 'item' : 'items'} · predicted`"
      />
      <StatTile label="Delete · S0" :value="formatBytes(totals.deleteBytes)" sub="Freed now · rebuilt by your tools" tone="accent" />
      <StatTile label="Quarantine · S1/S2" :value="formatBytes(totals.quarantineBytes)" sub="7-day undo · freed when purged" />
      <StatTile label="Skipped" :value="String(plan.skipped.length)" sub="In use or changed since scan" />

      <section aria-label="Plan items" class="tile col-span-12 overflow-hidden rounded-tile bg-surface pt-2">
        <div class="overflow-x-auto">
          <div role="list" class="min-w-[1000px]">
            <div class="plan-grid py-2 text-label font-medium text-text-secondary" aria-hidden="true">
              <span></span><span>Item</span><span>Tier</span><span>Evidence</span><span>Action</span>
              <span class="pr-4 text-right">Predicted</span><span>Undo</span><span></span>
            </div>
            <PlanRow
              v-for="it in visibleItems"
              :key="it.candidate.id"
              :item="it"
              grid="plan-grid"
              :row-class="rowState(it.candidate.id)"
            >
              <template #lead>
                <label class="flex h-16 w-14 cursor-pointer items-center justify-center" :class="{ 'cursor-not-allowed': !isSelectable(it.candidate) }">
                  <input
                    type="checkbox"
                    class="m-0 h-[18px] w-[18px] accent-ink"
                    :checked="sel.has(it.candidate.id)"
                    :disabled="phase !== 'review' || !isSelectable(it.candidate)"
                    :aria-label="`Include ${itemTitle(it)}`"
                    @change="toggle(it.candidate.id, ($event.target as HTMLInputElement).checked)"
                  />
                </label>
              </template>
              <template #trail>
                <template v-if="outcomes[it.candidate.id]?.outcome === 'skipped'">Skipped: {{ outcomes[it.candidate.id]?.reason }}</template>
                <template v-else>{{ undoLabel(it.candidate) }}</template>
              </template>
            </PlanRow>

            <template v-if="plan.left_out.length">
              <div role="listitem"><h2 class="m-0 border-t border-line px-4 pt-4 pb-2 text-label font-medium text-text-secondary">Protected · never offered</h2></div>
              <div
                v-for="l in plan.left_out"
                :key="l.label"
                role="listitem"
                class="grid min-h-[52px] grid-cols-[56px_minmax(280px,2.4fr)_132px_minmax(320px,3fr)] items-center border-t border-line text-text-secondary"
              >
                <span></span>
                <span class="font-mono text-label">{{ displayPath(l.label) }}</span>
                <span><TierBadge :tier="l.tier" /></span>
                <span class="pr-4 text-label">{{ l.reason }}</span>
              </div>
            </template>

            <template v-if="plan.skipped.length">
              <div role="listitem"><h2 class="m-0 border-t border-line px-4 pt-4 pb-2 text-label font-medium text-text-secondary">Skipped at re-check · not in this plan</h2></div>
              <div
                v-for="s in plan.skipped"
                :key="s.label"
                role="listitem"
                class="grid min-h-[52px] grid-cols-[56px_minmax(280px,2.4fr)_minmax(320px,3fr)] items-center border-t border-line text-text-secondary"
              >
                <span></span>
                <span class="font-mono text-label">{{ displayPath(s.label) }}</span>
                <span class="pr-4 text-label">{{ s.reason }}</span>
              </div>
            </template>
          </div>
        </div>
      </section>

      <section aria-label="How much you will really get back" class="tile col-span-12 flex flex-col gap-2.5 rounded-tile bg-surface p-tile-padding lg:col-span-6">
        <h2 class="m-0 text-label font-medium text-text-secondary">What you'll really get back</h2>
        <p class="m-0">scull measures free space before and after, and shows the difference next to this prediction.</p>
        <p v-for="n in plan.estimate_notes" :key="n" class="m-0 text-label text-text-secondary">{{ n }}</p>
      </section>
      <SafetyChecks />

      <section
        aria-label="Confirm"
        class="tile col-span-12 flex flex-wrap items-center justify-between gap-4 rounded-tile border border-accent-tint-border bg-accent-tint p-tile-padding text-ink"
      >
        <div class="flex flex-col gap-1">
          <h2 class="m-0 text-h3 font-semibold" aria-live="polite">{{ confirmTitle }}</h2>
          <span class="text-label text-text-strong-muted">
            Deleted S0 items can't be undone; rebuild them with your tools. Quarantined items can be restored for 7 days.
          </span>
          <span v-if="error && phase === 'review'" role="alert" class="text-label text-accent-text">{{ error }}</span>
        </div>
        <div class="flex flex-wrap gap-2">
          <RouterLink
            v-if="!running"
            to="/"
            class="inline-flex min-h-12 items-center rounded-button border border-text-strong-muted px-5 font-medium text-ink no-underline"
          >Cancel</RouterLink>
          <button
            type="button"
            class="relative min-h-12 min-w-[220px] overflow-hidden rounded-button border-0 px-6 font-semibold transition-[background] duration-200"
            :class="running || done ? 'cursor-default bg-surface-raised text-ink' : totals.count ? 'cursor-pointer bg-ink text-on-ink' : 'cursor-not-allowed bg-surface-raised text-text-secondary'"
            :disabled="phase !== 'review' || totals.count === 0"
            data-testid="confirm"
            @click="start"
          >
            <span
              aria-hidden="true"
              class="striped-fill absolute inset-y-0 left-0 rounded-button transition-[width] duration-700 ease-linear"
              :style="{ width: `${Math.round(progress * 100)}%` }"
            />
            <span class="relative">{{ buttonLabel }}</span>
          </button>
        </div>
      </section>
    </template>
  </div>
</template>

