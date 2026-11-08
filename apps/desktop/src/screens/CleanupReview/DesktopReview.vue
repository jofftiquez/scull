<script setup lang="ts">
import LoadingState from '../../components/shell/LoadingState.vue';
import ActionBar from '../../components/shell/ActionBar.vue';
import ActionConfirm from '../../components/shell/ActionConfirm.vue';
import { taskQueue } from '../../state/tasks';
import ActionFooter from '../../components/shell/ActionFooter.vue';
// Cleanup Review, the user's own plan (design 03 / 03b): review → running → done.
//
// The plan covers the items marked on the Overview ("Add to cleanup", basket.ts) when there are
// any, else the whole last scan. Either way the core decides what is in it: S3 items are never
// planned, and items that changed since the scan are skipped.
//
// Nothing changes until the user confirms. Confirming calls the plan bridge's `executePlan`
// through the audited core Executor. The browser preview simulates the result (SAFE-10).
import { computed, onMounted, ref } from 'vue';
import { RouterLink, useRoute } from 'vue-router';
import { toBridgeError, useBridge, type ItemId } from '../../bridge';
import { usePlanBridge, type ExecutionResult, type ItemOutcome, type PlanPreview } from '../../bridge/plan';
import { formatBytes } from '../../lib/format';
import { rescan } from '../../state/core';
import PlanRow from '../../components/review/PlanRow.vue';
import SafetyChecks from '../../components/review/SafetyChecks.vue';
import TierBadge from '../../components/review/TierBadge.vue';
import ReviewHeader from './ReviewHeader.vue';
import { downloadJson } from '../../components/review/download';
import { basketPlanRequest, cleanupBasket, removeFromCleanup } from '../../components/overview/basket';
import {
  displayPath,
  duration,
  initialSelection,
  isSelectable,
  isIrreversible,
  itemTitle,
  planTotals,
  shortId,
  toggleSelection,
  undoLabel,
} from '../../components/review/plan';
import '../../components/review/review.css';
import { usePagination } from '../../lib/pagination';
import ListPagination from '../../components/shell/ListPagination.vue';

const bridge = useBridge();
const plans = usePlanBridge();
const route = useRoute();
const projectRoot = typeof route.query.project === 'string' ? route.query.project : null;
const worktreeRoot = typeof route.query.worktree === 'string' ? route.query.worktree : null;
const scopeRoot = projectRoot ?? worktreeRoot;

type Phase = 'loading' | 'review' | 'running' | 'done' | 'error';
const phase = ref<Phase>('loading');
const plan = ref<PlanPreview | null>(null);
const sel = ref<Set<ItemId>>(new Set());
const outcomes = ref<Record<ItemId, ItemOutcome>>({});
const active = ref<ItemId | null>(null);
const gone = ref<Set<ItemId>>(new Set());
const result = ref<ExecutionResult | null>(null);
const error = ref('');
const confirming = ref(false);
/** How many Overview-marked items the plan was asked for (0: planned from the whole last scan). */
const marked = ref(0);
/** The selection as it was when cleaning started (totals stay put while rows sweep out). */
const chosen = ref<ItemId[]>([]);


async function load() {
  confirming.value = false;
  phase.value = 'loading';
  error.value = '';
  outcomes.value = {};
  gone.value = new Set();
  result.value = null;
  active.value = null;
  const req = scopeRoot ? {} : basketPlanRequest();
  marked.value = req.items?.length ?? 0;
  try {
    let p = await plans.preparePlan(req);
    if (scopeRoot) {
      const root = scopeRoot.replace(/[\\/]$/, '');
      const inside = (path: string) => path.startsWith(`${root}/`) || path.startsWith(`${root}\\`);
      p = { ...p, items: p.items.filter((it) => it.candidate.tier === 'S0' && ((projectRoot && it.candidate.project?.root === projectRoot) || inside(it.candidate.path))), left_out: p.left_out.filter((it) => inside(it.label)), skipped: p.skipped.filter((it) => inside(it.label)) };
    }
    plan.value = p;
    sel.value = initialSelection(p.items);
    phase.value = 'review';
  } catch (e) {
    error.value = toBridgeError(e).message;
    phase.value = 'error';
  }
}
onMounted(load);
const items = computed(() => plan.value?.items ?? []);
const typedRequired = computed(() => items.value.filter((it) => sel.value.has(it.candidate.id) && isSelectable(it.candidate) && it.candidate.tier !== 'S0' && isIrreversible(it.candidate.action)).length);
const running = computed(() => phase.value === 'running');
const done = computed(() => phase.value === 'done');
const totals = computed(() =>
  planTotals(items.value, phase.value === 'review' || phase.value === 'loading' ? sel.value : new Set(chosen.value)),
);
const visibleItems = computed(() => items.value.filter((i) => !gone.value.has(i.candidate.id)));

const source = computed(() =>
  scopeRoot ? `for ${scopeRoot}` : marked.value ? `from ${marked.value} ${marked.value === 1 ? 'item' : 'items'} marked on the Overview` : 'from the last scan',
);
const subline = computed(() => {
  const p = plan.value;
  if (!p) return `preparing a plan ${source.value} · nothing changes until you confirm`;
  const now = Date.now();
  return [
    `plan ${shortId(p.plan_id)}`,
    source.value,
    `created ${duration(now - Date.parse(p.created_at))} ago`,
    `expires in ${duration(Date.parse(p.expires_at) - now)}`,
    done.value ? 'cleaned' : 'nothing changes until you confirm',
  ].join(' · ');
});

function toggle(id: ItemId, on: boolean) {
  if (phase.value !== 'review') return;
  const it = items.value.find((i) => i.candidate.id === id);
  if (it) sel.value = toggleSelection(sel.value, it, on);
}

async function start() {
  const p = plan.value;
  if (!p || phase.value !== 'review' || totals.value.count === 0) return;
  const ids = items.value.filter((i) => sel.value.has(i.candidate.id) && isSelectable(i.candidate)).map((i) => i.candidate.id);
  const count = typedRequired.value;
  chosen.value = ids;
  confirming.value = false;
  phase.value = 'running';
  error.value = '';
  try {
    const r = await taskQueue.enqueue('Clean files', async (update) => {
      const result = await plans.executePlan(
        { plan_id: p.plan_id, hash: p.hash, items: ids, ...(count ? { typed_count: count } : {}) },
        { onEvent: (e) => {
          if (e.kind === 'item_started') active.value = e.item_id;
          else {
            const id = e.outcome.item_id;
            outcomes.value = { ...outcomes.value, [id]: e.outcome };
            update(Object.keys(outcomes.value).length / ids.length);
            if (active.value === id) active.value = null;
            if (e.outcome.outcome !== 'skipped') setTimeout(() => (gone.value = new Set([...gone.value, id])), 560);
          }
        } },
      );
      removeFromCleanup(result.items.filter((o) => o.outcome !== 'skipped').map((o) => o.item_id));
      return result;
    }, (result) => ({
      summary: `${result.items.filter((o) => o.outcome !== 'skipped').length} of ${ids.length} items completed · ${formatBytes(result.measured_freed_bytes)} freed${result.moved_bytes ? ` · ${formatBytes(result.moved_bytes)} moved to quarantine / Trash` : ''}`,
      partial: result.items.length !== ids.length || result.items.some((o) => o.outcome === 'skipped'),
      failed: !result.items.some((o) => o.outcome !== 'skipped'),
      details: result.items.map((o) => ({ label: p.items.find((i) => i.candidate.id === o.item_id)?.candidate.artifact ?? 'File cleanup', path: p.items.find((i) => i.candidate.id === o.item_id)?.candidate.path, status: o.outcome === 'skipped' ? 'skipped' as const : 'completed' as const, reason: o.reason ?? (o.outcome === 'quarantined' ? 'Moved to quarantine; available to restore in History.' : o.outcome === 'trashed' ? 'Moved to Trash.' : `${formatBytes(o.freed_bytes)} freed`) })),
    }), `${ids.length} selected items`);

    result.value = r;
    phase.value = 'done';
  } catch (e) { error.value = toBridgeError(e).message; phase.value = 'review'; }
  finally { active.value = null; }
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
const doneCount = computed(() => result.value?.items.filter((o) => o.outcome !== 'skipped').length ?? 0);
const skippedCount = computed(() => result.value?.items.filter((o) => o.outcome === 'skipped').length ?? 0);

function exportPlan() {
  const p = plan.value;
  if (p) downloadJson(`scull-plan-${p.plan_id}.json`, { ...p, selected: [...sel.value] });
}

/** Drop the Overview marks and plan from the whole last scan instead. */
async function planFromScan() {
  cleanupBasket.clear();
  await load();
}

async function doRescan() {
  await rescan(bridge);
  await load();
}

const btn = 'inline-flex h-11 cursor-pointer items-center rounded-button border border-border bg-surface px-5 font-medium text-ink hover:border-ink disabled:cursor-not-allowed disabled:opacity-60';
const { page, current: currentPage, rows: pageItems } = usePagination(visibleItems);
</script>

<template>
  <ReviewHeader :subline="subline">
    <button type="button" :class="btn" :disabled="!plan" @click="exportPlan">Export plan</button>
    <button type="button" :class="btn" :disabled="running" @click="doRescan">Rescan</button>
  </ReviewHeader>

  <LoadingState v-if="phase === 'loading'" label="Preparing cleanup review…" />
  <div v-else class="grid grid-cols-12 gap-page-gap">
    <section v-if="done && result" aria-label="Cleanup results" class="col-span-12 rounded-tile bg-surface p-5" data-testid="done">
      <h2 class="m-0 text-h3 font-semibold">{{ skippedCount ? 'Cleanup finished with skipped items' : 'Cleanup complete' }}</h2>
      <p class="m-0 mt-2 text-label">{{ formatBytes(measured) }} freed · {{ doneCount }} items completed<span v-if="result.moved_bytes"> · {{ formatBytes(result.moved_bytes) }} moved to quarantine / Trash</span><span v-if="skippedCount"> · {{ skippedCount }} skipped</span></p>
    </section>

    <section
      v-if="phase === 'error'"
      role="alert"
      class="tile col-span-12 flex flex-col gap-2 rounded-tile border border-accent-tint-border bg-accent-tint p-tile-padding"
    >
      <h2 class="m-0 text-h3 font-semibold">No plan to review</h2>
      <p class="m-0 text-label text-text-strong-muted">{{ error }}</p>
      <div v-if="marked">
        <button type="button" :class="btn" data-testid="plan-from-scan" @click="planFromScan">Plan from the whole scan instead</button>
      </div>
    </section>

    <template v-if="plan">
      <div class="col-span-12 flex flex-wrap items-center justify-between gap-3 px-2 text-label">
        <strong>{{ totals.count }} selected · {{ formatBytes(totals.bytes) }} predicted</strong>
        <span class="text-text-secondary">Delete {{ formatBytes(totals.deleteBytes) }} · Quarantine {{ formatBytes(totals.quarantineBytes) }} · {{ plan.skipped.length }} skipped</span>
      </div>

      <section aria-label="Plan items" class="tile col-span-12 overflow-hidden rounded-tile bg-surface pt-2">
        <div class="overflow-x-auto">
          <div role="list" class="min-w-[1000px]">
            <div class="plan-grid py-2 text-label font-medium text-text-secondary" aria-hidden="true">
              <span></span><span>Item</span><span>Tier</span><span>Evidence</span><span>Action</span>
              <span class="pr-4 text-right">Predicted</span><span>Undo</span><span></span>
            </div>
            <PlanRow
              v-for="it in pageItems"
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
        <ListPagination :page="currentPage" :total="visibleItems.length" @update:page="page = $event" />
      </section>

      <details class="col-span-12 rounded-tile bg-surface p-5"><summary class="min-h-touch cursor-pointer font-medium">Safety checks and estimate details</summary><SafetyChecks /><p v-for="n in plan.estimate_notes" :key="n" class="text-label text-text-secondary">{{ n }}</p></details>
      <ActionFooter aria-label="Cleanup actions">
        <ActionBar :summary="done ? `${doneCount} completed · ${skippedCount} skipped` : `${totals.count} selected · ${formatBytes(totals.bytes)} predicted`" :label="done ? 'Review again' : running ? 'Queued / cleaning…' : 'Review cleanup'" :busy="running" :disabled="!done && (phase !== 'review' || !totals.count)" :error="error" data-testid="confirm" @action="done ? load() : confirming = true">
          <RouterLink v-if="!running" to="/" class="inline-flex min-h-12 items-center rounded-button border border-border px-4 text-ink no-underline">{{ done ? 'Overview' : 'Cancel' }}</RouterLink>
        </ActionBar>
      </ActionFooter>
      <ActionConfirm v-if="confirming" :title="`Clean ${totals.count} selected items?`" label="Confirm cleanup" :count="typedRequired" @cancel="confirming = false" @confirm="start">
        <p>{{ formatBytes(totals.deleteBytes) }} selected for deletion · {{ formatBytes(totals.quarantineBytes) }} for quarantine.</p>
        <p>Deleted items cannot be undone by scull. Quarantined items can be restored for 7 days. Worktree branches are kept.</p>
      </ActionConfirm>
    </template>
  </div>
</template>

