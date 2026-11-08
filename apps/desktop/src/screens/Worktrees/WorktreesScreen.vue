<script setup lang="ts">
import ActionBar from '../../components/shell/ActionBar.vue';
import ActionConfirm from '../../components/shell/ActionConfirm.vue';
import { taskQueue, removedWorktrees } from '../../state/tasks';
import ActionFooter from '../../components/shell/ActionFooter.vue';
// Worktree removals use a reviewed one-item core plan. Git checks run again before removal;
// the exact folder is removed without force, and its branch is kept.
import { computed, nextTick, onMounted, ref, watch } from 'vue';
import { RouterLink } from 'vue-router';
import { toBridgeError, useBridge, type WorktreeReport, type WorktreesView } from '../../bridge';
import { usePlanBridge, type PlanPreview } from '../../bridge/plan';
import { formatBytes } from '../../lib/format';
import ScreenLayout from '../../components/shell/ScreenLayout.vue';
import VerdictBadge from '../../components/worktrees/VerdictBadge.vue';
import {
  branchOf,
  canRemove,
  checksOf,
  displayPath,
  filterCounts,
  matches,
  metaLine,
  repoCount,
  repoOf,
  removalBlock,
  sortBySize,
  verdictLine,
  verdictOf,
  verdictReason,
  type Filter,
  type Verdict,
} from '../../components/worktrees/worktrees';
import '../../components/review/review.css';
import { core } from '../../state/core';
import { usePagination } from '../../lib/pagination';
import ListPagination from '../../components/shell/ListPagination.vue';

const bridge = useBridge();
const plans = usePlanBridge();

const view = ref<WorktreesView | null>(null);
const loadError = ref('');
const loading = ref(true);
const filter = ref<Filter>('all');
const picked = ref<string | null>(null);
const removing = ref<string | null>(null);
const removeError = ref('');
const preparing = ref<string | null>(null);
const prepared = ref<{ worktree: WorktreeReport; plan: PlanPreview } | null>(null);
const removeButton = ref<HTMLElement | null>(null);

async function load() {
  loading.value = true;
  try {
    view.value = await bridge.worktrees();
    loadError.value = "";
  } catch (e) {
    loadError.value = toBridgeError(e).message;
  } finally { loading.value = false; }
}
onMounted(load);
watch(() => core.report, () => { void load(); });


const present = computed(() => sortBySize((view.value?.worktrees ?? []).filter((w) => !removedWorktrees.some((r) => r.path === w.path && r.scanId === (core.report?.scan_id ?? null)))));
const counts = computed(() => filterCounts(present.value));
const rows = computed(() => present.value.filter((w) => matches(w, filter.value)));
const { page, current: currentPage, rows: pageRows } = usePagination(rows);
watch(filter, () => { page.value = 1; picked.value = null; });
watch(page, () => { picked.value = null; });
const current = computed<WorktreeReport | null>(
  () => rows.value.find((w) => w.path === picked.value) ?? null,
);


const FILTERS: { key: Filter; label: string }[] = [
  { key: 'all', label: 'All' },
  { key: 'removable', label: 'Removable' },
  { key: 'review', label: 'Review' },
  { key: 'protected', label: 'Protected' },
  { key: 'agent', label: 'Agent' },
];

const BAR: Record<Verdict, string> = {
  removable: 'var(--tier-s1-fill)',
  review: 'var(--tier-s2-fill)',
  protected: 'repeating-linear-gradient(135deg, var(--color-tier-s3) 0 3px, var(--color-tier-s3-stripe) 3px 5px)',
};
const VERDICT_BOX: Record<Verdict, string> = {
  removable: 'bg-violet-tint text-ink',
  review: 'bg-amber-tint text-ink',
  protected: 'bg-tier-s3 text-tier-s3-text',
};

function pick(w: WorktreeReport) {
  if (!removing.value && !preparing.value) {
    picked.value = w.path;
    prepared.value = null;
  }
}

/** Arrow keys move through the list (it is one tab stop's worth of buttons). */
function onKey(e: KeyboardEvent, i: number) {
  const d = e.key === 'ArrowDown' ? 1 : e.key === 'ArrowUp' ? -1 : 0;
  if (!d) return;
  e.preventDefault();
  const next = pageRows.value[i + d];
  if (!next) return;
  pick(next);
  const el = (e.currentTarget as HTMLElement).closest('ul')?.querySelectorAll<HTMLButtonElement>('button[data-row]')[i + d];
  el?.focus();
}

async function prepareRemoval(w: WorktreeReport) {
  if (removing.value || preparing.value || !canRemove(w) || !w.candidate) return;
  preparing.value = w.path;
  removeError.value = '';
  try {
    const plan = await plans.preparePlan({ items: [w.candidate] });
    if (plan.items.length !== 1 || plan.items[0]?.candidate.id !== w.candidate || plan.items[0].candidate.path !== w.path) {
      throw new Error('The removal preview changed. Run Scan and review this worktree again.');
    }
    prepared.value = { worktree: w, plan };
  } catch (e) {
    removeError.value = toBridgeError(e).message;
  } finally { preparing.value = null; }
}

async function cancelRemoval() {
  prepared.value = null;
  await nextTick();
  removeButton.value?.querySelector('button')?.focus();
}

async function remove() {
  if (!prepared.value) return;
  const { worktree: w, plan } = prepared.value;
  if (removing.value || !canRemove(w) || !w.candidate) return;
  const candidate = w.candidate;
  const scanId = core.report?.scan_id ?? null;
  prepared.value = null;
  removing.value = w.path;
  removeError.value = '';
  try {
    await taskQueue.enqueue('Remove worktree', async () => {
      const result = await plans.executePlan({ plan_id: plan.plan_id, hash: plan.hash, items: [candidate], typed_count: 1 });
      const out = result.items.find((o) => o.item_id === candidate);
      if (!out || out.outcome === 'skipped') throw new Error(`Not removed: ${out?.reason ?? 'it changed since the scan'}. Run Scan to check the current state.`);
      removedWorktrees.push({ path: w.path, scanId });
      return result;
    }, (result) => ({ summary: `${formatBytes(result.measured_freed_bytes)} freed · branch kept`, details: [{ label: branchOf(w), path: w.path, status: 'completed', reason: 'Worktree removed. Its branch was kept.' }] }), branchOf(w));
    picked.value = null;
  } catch (e) { removeError.value = toBridgeError(e).message; }
  finally { removing.value = null; }
}

</script>

<template>
  <ScreenLayout :loading="loading && !view" loading-label="Loading worktrees…" aside-label="Worktree details" :detail-open="!!current" @close-details="picked = null">
    <header class="flex flex-col gap-3 px-2 py-2">
      <div class="flex flex-col gap-1">
        <h1 class="m-0 text-h2 font-semibold">Git Worktrees</h1>
        <!-- No counts until the view has loaded: "0 worktrees" while reading would be a false empty state. -->
        <span class="text-label text-text-secondary" data-testid="worktrees-summary" :aria-busy="!view && !loadError ? 'true' : undefined">
          <template v-if="view">
            {{ present.length }} worktree{{ present.length === 1 ? '' : 's' }} across {{ repoCount(present) }} repo{{ repoCount(present) === 1 ? '' : 's' }}

          </template>
          <template v-else-if="!loadError">Reading worktrees…</template>
        </span>
      </div>
      <div role="group" aria-label="Filter" class="flex flex-wrap gap-0.5">
        <button
          v-for="f in FILTERS"
          :key="f.key"
          type="button"
          :aria-pressed="filter === f.key"
          class="min-h-11 cursor-pointer rounded-button border-0 px-3.5 font-medium"
          :class="filter === f.key ? 'bg-ink text-on-ink' : 'bg-surface-raised text-ink hover:bg-line'"
          @click="filter = f.key"
        >
          {{ view ? `${f.label} ${counts[f.key]}` : f.label }}
        </button>
      </div>
    </header>

    <section v-if="loadError" role="alert" class="rounded-tile border border-accent-tint-border bg-accent-tint p-tile-padding">
      <h2 class="m-0 text-h3 font-semibold">Worktrees unavailable</h2>
      <p class="m-0 text-label text-text-strong-muted">{{ loadError }}</p>
    </section>

    <div v-else-if="view" class="flex flex-col gap-page-gap">
      <section aria-label="Worktree list" class="overflow-hidden rounded-tile bg-surface py-2">
        <p v-if="rows.length === 0" class="m-0 px-5 py-6 text-label text-text-secondary">
          <template v-if="core.scanning">Scanning for worktrees…</template>
          <template v-else-if="!view.roots.length">Scan to load worktrees. Detailed results are loaded for this app session.</template>
          <template v-else-if="view.cancelled">The scan stopped before Git worktrees were checked. Run Scan again.</template>
          <template v-else-if="view.signals_unavailable.some((s) => s.signal === 'git')">Git could not be checked. Make sure Git is installed, then run Scan again.</template>
          <template v-else>{{ present.length === 0 ? 'No Git worktrees found in the scanned folders. Check your scan folders in Settings.' : 'No worktree matches this filter.' }}</template>
        </p>
        <ul class="m-0 flex list-none flex-col p-0">
          <li v-for="(w, i) in pageRows" :key="w.path">
            <button
              type="button"
              data-row
              :aria-current="current?.path === w.path ? 'true' : undefined"
              class="mx-2 my-0.5 grid min-h-[72px] w-[calc(100%-16px)] cursor-pointer grid-cols-[6px_minmax(0,1fr)_auto] items-center gap-4 overflow-hidden rounded-[14px] border-0 py-3 pr-5 pl-0 text-left text-ink transition-[background] duration-200"
              :class="removing === w.path ? 'bg-violet-tint' : current?.path === w.path ? 'bg-surface-raised' : 'bg-surface hover:bg-surface-subtle'"
              :aria-busy="removing === w.path ? true : undefined"
              @click="pick(w)"
              @keydown="onKey($event, i)"
            >
              <span class="self-stretch rounded-[3px]" :style="{ background: BAR[verdictOf(w)] }" aria-hidden="true" />
              <span class="flex min-w-0 flex-col gap-1">
                <span class="font-mono font-medium [overflow-wrap:anywhere]">{{ branchOf(w) }}</span>
                <span class="text-caption text-text-secondary">{{ metaLine(w) }}</span>
              </span>
              <span class="flex flex-col items-end gap-1">
                <VerdictBadge :verdict="verdictOf(w)" />
                <span class="font-mono text-caption">{{ formatBytes(w.allocated) }}</span>
              </span>
            </button>
          </li>
        </ul>
        <div class="border-t border-line px-5 pt-4 pb-2 text-label text-text-secondary">
          <ListPagination :page="currentPage" :total="rows.length" @update:page="page = $event" />
          <template v-if="view.problems.length"> · {{ view.problems.length }} registration{{ view.problems.length === 1 ? '' : 's' }} couldn't be trusted (protected)</template>
        </div>
      </section>
    </div>
    <template v-if="current" #aside>
      <section v-if="current" aria-label="Worktree details" class="flex flex-col gap-5 rounded-tile bg-surface p-tile-padding">
        <div class="flex flex-col gap-1.5">
          <span class="text-label font-medium text-text-secondary">{{ repoOf(current) }}{{ current.kind === 'main' ? ' · main working tree' : '' }}</span>
          <h2 class="m-0 text-h2 font-semibold [overflow-wrap:anywhere]">{{ branchOf(current) }}</h2>
          <span class="font-mono text-caption text-text-secondary [overflow-wrap:anywhere]">{{ displayPath(current.path) }}</span>
        </div>
        <div class="flex flex-col gap-1 rounded-[14px] p-4" :class="VERDICT_BOX[verdictOf(current)]" data-testid="verdict">
          <span class="font-mono text-caption font-medium">{{ verdictLine(current) }}</span>
          <span class="text-label">{{ verdictReason(current) }}</span>
        </div>
        <p v-if="removalBlock(current)" class="m-0 text-label text-text-secondary" data-testid="removal-block">{{ removalBlock(current)!.detail }}</p>
        <div class="flex flex-col">
          <h3 class="m-0 pb-2 text-label font-medium text-text-secondary">Checks</h3>
          <ul class="m-0 list-none p-0" data-testid="checks">
            <li
              v-for="c in checksOf(current)"
              :key="c.label"
              class="grid grid-cols-[12px_minmax(0,1fr)_auto] items-center gap-3 border-t border-line py-[9px]"
            >
              <span
                aria-hidden="true"
                class="h-2.5 w-2.5 rounded-[4px]"
                :class="c.status === 'unknown' ? 'border border-dashed border-text-hint' : ''"
                :style="c.status === 'pass' ? 'background: var(--color-ink)' : c.status === 'fail' ? 'background: var(--tier-s2-fill)' : undefined"
              />
              <span class="flex flex-col">
                <span class="text-label">{{ c.label }}</span>
                <span v-if="c.note" class="text-caption text-text-secondary">{{ c.note }}</span>
              </span>
              <span class="font-mono text-[11px]" :class="c.status === 'fail' ? 'font-medium text-accent-text' : 'text-text-secondary'">{{ c.status }}</span>
            </li>
          </ul>
        </div>
        <ActionFooter panel="aside" aria-label="Worktree actions">
          <div ref="removeButton">
            <ActionBar :summary="`${branchOf(current)} · ${formatBytes(current.allocated)}`" :label="removing === current.path ? 'Queued / removing…' : preparing === current.path ? 'Preparing…' : 'Remove worktree'" tone="remove" :busy="!!removing || !!preparing" :disabled="!canRemove(current) || loading" :error="removeError" data-testid="remove" @action="prepareRemoval(current)">
              <RouterLink v-if="!current.inside_roots" to="/settings?tab=general" class="footer-secondary">Choose scan folders</RouterLink>
              <RouterLink v-if="current.decision.artifact_cleanup === 'allowed' && current.decision.artifacts.length" :to="{ path: '/cleanup', query: { worktree: current.path } }" class="footer-secondary">Review artifacts</RouterLink>
            </ActionBar>
          </div>
          <ActionConfirm v-if="prepared" title="Remove this worktree?" label="Remove worktree, keep branch" :count="1" tone="remove" data-testid="worktree-confirmation" @cancel="cancelRemoval" @confirm="remove">
            <strong>{{ branchOf(prepared.worktree) }}</strong><p class="font-mono text-caption">{{ prepared.worktree.path }}</p>
            <p>This removes the worktree folder. Its branch is kept. Removal cannot be undone by scull.</p>
          </ActionConfirm>
        </ActionFooter>
      </section>
    </template>
  </ScreenLayout>
</template>

<style scoped>
.footer-secondary { display:inline-flex; align-items:center; min-height:44px; padding:0 16px; border:1px solid var(--color-border); border-radius:var(--radius-button); color:var(--color-ink); text-decoration:none; font-size:13px; }
</style>
