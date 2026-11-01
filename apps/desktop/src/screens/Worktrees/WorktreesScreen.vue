<script setup lang="ts">
// Git Worktrees (design 04 / 04b): every worktree of the last scan with its checks and the
// core's verdict. Removal ("git worktree remove", branch kept) is offered only for Removable
// worktrees and goes through a one-item plan: `preparePlan` + `executePlan`, the plan bridge
// STUBS (src/bridge/plan.ts). The desktop never removes anything itself (SAFE-10); inside Tauri
// the stub refuses and the error is shown.
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';
import { RouterLink } from 'vue-router';
import { toBridgeError, useBridge, type WorktreeReport, type WorktreesView } from '../../bridge';
import { usePlanBridge } from '../../bridge/plan';
import { formatBytes } from '../../lib/format';
import ScreenLayout from '../../components/shell/ScreenLayout.vue';
import RemovalToast from '../../components/worktrees/RemovalToast.vue';
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
  sortBySize,
  verdictLine,
  verdictOf,
  verdictReason,
  type Filter,
  type Verdict,
} from '../../components/worktrees/worktrees';
import '../../components/review/review.css';

const bridge = useBridge();
const plans = usePlanBridge();

const view = ref<WorktreesView | null>(null);
const loadError = ref('');
const filter = ref<Filter>('all');
const picked = ref<string | null>(null);
const removed = ref<Set<string>>(new Set());
const removing = ref<string | null>(null);
const removeError = ref('');
const toast = ref<{ text: string; amount: string } | null>(null);
const timers: ReturnType<typeof setTimeout>[] = [];

onMounted(async () => {
  try {
    view.value = await bridge.worktrees();
  } catch (e) {
    loadError.value = toBridgeError(e).message;
  }
});
onBeforeUnmount(() => timers.forEach(clearTimeout));

const present = computed(() => sortBySize((view.value?.worktrees ?? []).filter((w) => !removed.value.has(w.path))));
const counts = computed(() => filterCounts(present.value));
const rows = computed(() => present.value.filter((w) => matches(w, filter.value)));
const current = computed<WorktreeReport | null>(
  () => rows.value.find((w) => w.path === picked.value) ?? rows.value[0] ?? null,
);
const totalBytes = computed(() => present.value.reduce((a, w) => a + (w.allocated ?? 0), 0));

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
  if (!removing.value) picked.value = w.path;
}

/** Arrow keys move through the list (it is one tab stop's worth of buttons). */
function onKey(e: KeyboardEvent, i: number) {
  const d = e.key === 'ArrowDown' ? 1 : e.key === 'ArrowUp' ? -1 : 0;
  if (!d) return;
  e.preventDefault();
  const next = rows.value[i + d];
  if (!next) return;
  pick(next);
  const el = (e.currentTarget as HTMLElement).closest('ul')?.querySelectorAll<HTMLButtonElement>('button[data-row]')[i + d];
  el?.focus();
}

async function remove(w: WorktreeReport) {
  if (removing.value || !canRemove(w) || !w.candidate) return;
  removing.value = w.path;
  removeError.value = '';
  toast.value = null;
  const started = Date.now();
  try {
    const plan = await plans.preparePlan({ items: [w.candidate] });
    const result = await plans.executePlan({ plan_id: plan.plan_id, hash: plan.hash, items: [w.candidate] });
    const out = result.items.find((o) => o.item_id === w.candidate);
    if (!out || out.outcome === 'skipped') {
      removeError.value = `Not removed: ${out?.reason ?? 'it changed since the scan'}. Nothing was touched.`;
      return;
    }
    // Let the row's sweep (560ms after 260ms) finish before it leaves the list.
    await new Promise((r) => setTimeout(r, Math.max(0, 860 - (Date.now() - started))));
    removed.value = new Set([...removed.value, w.path]);
    const rest = rows.value;
    picked.value = (rest.find((x) => verdictOf(x) === 'removable') ?? rest[0])?.path ?? null;
    toast.value = { text: `Removed ${branchOf(w)} · branch kept`, amount: `+${formatBytes(result.measured_freed_bytes)}` };
    timers.push(setTimeout(() => (toast.value = null), 4200));
  } catch (e) {
    removeError.value = toBridgeError(e).message;
  } finally {
    removing.value = null;
  }
}

const artifactNames = (w: WorktreeReport) => w.decision.artifacts.map((a) => a.path.replace(/\/$/, '')).join(', ');
</script>

<template>
  <ScreenLayout>
    <header class="flex flex-col gap-4 rounded-tile bg-surface px-6 py-5">
      <div class="flex flex-col gap-1">
        <h1 class="m-0 text-h1 font-semibold tracking-[-0.02em]">Git Worktrees</h1>
        <span class="text-label text-text-secondary" data-testid="worktrees-summary">
          {{ present.length }} worktree{{ present.length === 1 ? '' : 's' }} across {{ repoCount(present) }} repo{{ repoCount(present) === 1 ? '' : 's' }}
          · {{ formatBytes(totalBytes) }} · found through Git metadata, never by folder name · no fetch performed
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
          {{ f.label }} {{ counts[f.key] }}
        </button>
      </div>
    </header>

    <section v-if="loadError" role="alert" class="rounded-tile border border-accent-tint-border bg-accent-tint p-tile-padding">
      <h2 class="m-0 text-h3 font-semibold">Worktrees unavailable</h2>
      <p class="m-0 text-label text-text-strong-muted">{{ loadError }}</p>
    </section>

    <div v-else-if="view" class="grid grid-cols-12 items-start gap-page-gap">
      <section aria-label="Worktree list" class="tile col-span-12 overflow-hidden rounded-tile bg-surface py-2 lg:col-span-7">
        <p v-if="rows.length === 0" class="m-0 px-5 py-6 text-label text-text-secondary">
          {{ present.length === 0 ? 'No Git worktrees in the scanned folders.' : 'No worktree matches this filter.' }}
        </p>
        <ul class="m-0 flex list-none flex-col p-0">
          <li v-for="(w, i) in rows" :key="w.path">
            <button
              type="button"
              data-row
              :aria-current="current?.path === w.path ? 'true' : undefined"
              class="mx-2 my-0.5 grid min-h-[72px] w-[calc(100%-16px)] cursor-pointer grid-cols-[6px_minmax(0,1fr)_auto] items-center gap-4 overflow-hidden rounded-[14px] border-0 py-3 pr-5 pl-0 text-left text-ink transition-[background] duration-200"
              :class="removing === w.path ? 'bg-violet-tint' : current?.path === w.path ? 'bg-surface-raised' : 'bg-surface hover:bg-surface-subtle'"
              :style="removing === w.path ? 'animation: scull-collapse 560ms 260ms ease-in forwards' : undefined"
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
          Showing {{ rows.length }} of {{ present.length }} · sorted by reclaimable size
          <template v-if="view.problems.length"> · {{ view.problems.length }} registration{{ view.problems.length === 1 ? '' : 's' }} couldn't be trusted (protected)</template>
        </div>
      </section>

      <section v-if="current" aria-label="Worktree details" class="tile col-span-12 flex flex-col gap-5 rounded-tile bg-surface p-tile-padding lg:col-span-5">
        <div class="flex flex-col gap-1.5">
          <span class="text-label font-medium text-text-secondary">{{ repoOf(current) }}{{ current.kind === 'main' ? ' · main working tree' : '' }}</span>
          <h2 class="m-0 text-h2 font-semibold [overflow-wrap:anywhere]">{{ branchOf(current) }}</h2>
          <span class="font-mono text-caption text-text-secondary [overflow-wrap:anywhere]">{{ displayPath(current.path) }}</span>
        </div>
        <div class="flex flex-col gap-1 rounded-[14px] p-4" :class="VERDICT_BOX[verdictOf(current)]" data-testid="verdict">
          <span class="font-mono text-caption font-medium">{{ verdictLine(current) }}</span>
          <span class="text-label">{{ verdictReason(current) }}</span>
        </div>
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
        <div class="flex flex-col gap-2">
          <button
            type="button"
            class="min-h-12 rounded-button border-0 px-4 font-semibold transition-[background] duration-200"
            :class="canRemove(current) ? 'cursor-pointer bg-ink text-on-ink disabled:cursor-progress' : 'cursor-not-allowed bg-surface-raised text-text-secondary'"
            :disabled="!canRemove(current) || !!removing"
            :style="removing === current.path ? 'animation: scull-pulse 700ms ease-in-out infinite' : undefined"
            data-testid="remove"
            @click="remove(current)"
          >
            {{
              removing === current.path
                ? 'Removing worktree…'
                : canRemove(current)
                  ? `Remove worktree, keep branch · ${formatBytes(current.allocated)}`
                  : 'Whole-worktree removal blocked'
            }}
          </button>
          <RouterLink
            v-if="current.decision.artifact_cleanup === 'allowed' && current.decision.artifacts.length"
            to="/cleanup"
            class="inline-flex min-h-12 items-center justify-center rounded-button border border-border bg-surface px-4 font-medium text-ink no-underline hover:border-ink"
          >
            Clean artifacts only · {{ artifactNames(current) }}
          </RouterLink>
          <span class="text-caption text-text-secondary">Uses git worktree remove. Branches are kept. Artifact-only cleanup leaves the worktree in place.</span>
          <span v-if="removeError" role="alert" class="text-label text-accent-text">{{ removeError }}</span>
          <RemovalToast v-if="toast" :text="toast.text" :amount="toast.amount" />
        </div>
      </section>
    </div>
  </ScreenLayout>
</template>
