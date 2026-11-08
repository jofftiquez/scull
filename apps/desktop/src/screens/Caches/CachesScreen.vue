<script setup lang="ts">
// Caches (P3-UI-08; design 08 / 08b; PRD §10.8 QC-01…06). Caches from the core's `caches()`,
// the Quick Clean preview (resolved binaries, prune/wipe per tool, skipped tools with reasons),
// and a sequential run with per-row progress and the burst. Planning and running go through
// the Caches action stub (components/caches/bridge.ts): the desktop itself never deletes and
// never starts a process (SAFE-10).
import { computed, onBeforeUnmount, onMounted, reactive, ref } from 'vue';
import { useRoute } from 'vue-router';
import ScreenLayout from '../../components/shell/ScreenLayout.vue';
import { usePagination } from '../../lib/pagination';
import ListPagination from '../../components/shell/ListPagination.vue';
import CacheRow from '../../components/caches/CacheRow.vue';
import ActionFooter from '../../components/shell/ActionFooter.vue';
import ActionBar from '../../components/shell/ActionBar.vue';
import ActionConfirm from '../../components/shell/ActionConfirm.vue';
import { cacheTaskProgress } from '../../components/caches/progress';
import { taskQueue } from '../../state/tasks';
import { cacheActionsFor, designPreview } from '../../components/caches/bridge';
import {
  GROUPS,
  NOT_UNDOABLE,
  groupTotals,
  initialChoices,
  overall,
  rowProgress,
  runSummary,
  selected,
  selectedBytes,
  shortPath,
  skipText,
  toolName,
  type Choice,
  type QuickCleanPreview,
  type RunPhase,
  type RunResult,
} from '../../components/caches/model';
import { toBridgeError, useBridge } from '../../bridge';
import { formatBytes } from '../../lib/format';
import { refresh } from '../../state/core';

const bridge = useBridge();
const actions = cacheActionsFor(bridge);
const route = useRoute();
const demo = bridge.kind === 'mock' && route.query.demo === 'design';

const preview = ref<QuickCleanPreview | null>(null);
const error = ref<string | null>(null);
const choices = reactive<Record<string, Choice>>({});
const phase = ref<RunPhase>('idle');
const progress = ref(0);
const order = ref<string[]>([]);
const results = ref<RunResult[]>([]);
const freed = reactive(new Set<string>());
const runError = ref<string | null>(null);
const reloading = ref(false);
const loading = ref(true);
const confirming = ref(false);
const live = reactive<Record<string, 'active' | 'done' | 'failed'>>({});
let alive = true;

onMounted(async () => {
  try {
    if (demo) {
      const d = designPreview();
      preview.value = d.preview;
      Object.assign(choices, initialChoices(d.preview.tools));
      for (const t of d.preview.tools) choices[t.item_id]!.on = d.preselected.includes(t.item_id);
    } else {
      const report = bridge.kind === 'mock' ? await bridge.caches() : undefined;
      preview.value = await actions.plan(report);
      Object.assign(choices, initialChoices(preview.value.tools));
    }
  } catch (e) {
    error.value = toBridgeError(e).message;
  } finally { loading.value = false; }
});
onBeforeUnmount(() => (alive = false));

const tools = computed(() => (preview.value?.tools ?? []).filter((t) => !freed.has(t.item_id)));
const chosen = computed(() => selected(tools.value, choices));
const chosenBytes = computed(() => selectedBytes(tools.value, choices));
const allGroups = computed(() => {
  const p = preview.value;
  if (!p) return [];
  const totals = groupTotals(p.tools, p.skipped);
  return GROUPS.map((group) => ({
    group,
    total: totals.find((t) => t.group === group)?.bytes ?? 0,
    tools: p.tools.filter((t) => t.group === group),
    skipped: p.skipped.filter((s) => s.group === group),
  })).filter((g) => g.tools.length + g.skipped.length > 0);
});
const entries = computed(() => allGroups.value.flatMap((g) => [...g.tools.map((t) => t.item_id), ...g.skipped.map((t) => `skip:${t.tool}:${t.path}`)]));
const { page, current: currentPage, rows: pageEntries } = usePagination(entries);
const groups = computed(() => allGroups.value.map((g) => ({ ...g,
  tools: g.tools.filter((t) => pageEntries.value.includes(t.item_id)),
  skipped: g.skipped.filter((t) => pageEntries.value.includes(`skip:${t.tool}:${t.path}`)),
})).filter((g) => g.tools.length + g.skipped.length));
const allBytes = computed(() => {
  const p = preview.value;
  if (!p) return null;
  return p.tools.reduce((a, t) => a + t.bytes, 0) + p.skipped.reduce((a, s) => a + (s.bytes ?? 0), 0);
});
const summary = computed(() => (phase.value === 'done' ? runSummary(results.value, plannedBytes.value) : null));
const failures = computed(() => phase.value === 'done' ? results.value.filter((r) => !r.ok) : []);
const plannedBytes = ref(0);

function rowState(id: string) {
  if (phase.value === 'idle') return { state: 'idle' as const, pct: 0 };
  if (actions.kind === 'tauri' && phase.value === 'running') return { state: live[id] ?? 'queued' as const, pct: live[id] === 'done' ? 100 : 0 };
  const k = order.value.indexOf(id);
  if (k < 0) return { state: 'idle' as const, pct: 0 };
  const r = results.value.find((x) => x.item_id === id);
  if (phase.value === 'done' && r && !r.ok) return { state: 'failed' as const, pct: 0 };
  return rowProgress(k, order.value.length, phase.value === 'done' ? 1 : progress.value);
}

function remainingBytes(id: string): number | undefined {
  if (actions.kind !== 'tauri') return undefined;
  // Live events finish commands; measured remaining bytes arrive with the final report.
  if (phase.value === 'running') return tools.value.find((t) => t.item_id === id)?.bytes;
  if (phase.value !== 'done') return undefined;
  const result = results.value.find((r) => r.item_id === id);
  const tool = tools.value.find((t) => t.item_id === id);
  return result?.ok && tool ? Math.max(0, tool.bytes - result.freed_bytes) : undefined;
}

async function start() {
  const p = preview.value;
  if (!p || phase.value !== 'idle' || chosen.value.length === 0 || p.unavailable) return;
  const run = chosen.value.map((t) => ({ item_id: t.item_id, mode: choices[t.item_id]!.mode }));
  order.value = run.map((r) => r.item_id);
  plannedBytes.value = chosenBytes.value;
  confirming.value = false;
  phase.value = 'running';
  progress.value = 0;
  runError.value = null;
  for (const key of Object.keys(live)) delete live[key];
  try {
    results.value = await taskQueue.enqueue('Clean caches', async (update) => actions.run(p, run, (i, f) => {
      progress.value = overall(i, f, run.length);
    }, (event) => {
      progress.value = event.total ? event.completed / event.total : 0;
      if (event.item_id && event.kind === 'started') live[event.item_id] = 'active';
      if (event.item_id && (event.kind === 'finished' || event.kind === 'skipped')) live[event.item_id] = event.succeeded ? 'done' : 'failed';
      update(cacheTaskProgress(event));
    }), (items) => ({
      summary: `${items.filter((r) => r.ok).length} of ${run.length} caches cleaned · ${formatBytes(items.reduce((n, r) => n + r.freed_bytes, 0))} freed`,
      partial: items.length !== run.length || items.some((r) => !r.ok),
      failed: !items.some((r) => r.ok),
      details: items.map((r) => ({ label: toolName(r.tool), path: p.tools.find((t) => t.item_id === r.item_id)?.cache, status: r.ok ? 'completed' as const : 'failed' as const, reason: r.error ?? r.explanation ?? `${formatBytes(r.freed_bytes)} freed` })),
    }), `${run.length} selected caches`);
    if (!demo) void refresh(bridge);
    if (!alive) return;
    progress.value = 1;
    phase.value = 'done';
  } catch (e) {
    runError.value = toBridgeError(e).message;
    phase.value = 'idle';
    order.value = [];
  }
}

async function reset() {
  if (reloading.value) return;
  if (actions.kind === 'tauri') {
    reloading.value = true;
    try {
      preview.value = await actions.plan();
      for (const key of Object.keys(choices)) delete choices[key];
      Object.assign(choices, initialChoices(preview.value.tools));
    } catch (e) {
      error.value = toBridgeError(e).message;
      preview.value = null;
    } finally {
      reloading.value = false;
    }
    results.value = [];
    phase.value = 'idle';
    order.value = [];
    progress.value = 0;
    return;
  }
  for (const r of results.value) if (r.ok) freed.add(r.item_id);
  for (const id of order.value) if (choices[id]) choices[id].on = false;
  phase.value = 'idle';
  order.value = [];
  progress.value = 0;
}
</script>

<template>
  <ScreenLayout :loading="loading" loading-label="Checking caches and available cleanup commands…" aside-label="Cache notes">
    <header class="flex flex-col gap-1 px-2 py-2">
      <h1 class="m-0 text-h2 font-semibold">Caches</h1>
      <span class="text-label text-text-secondary">Package managers, models, containers and test browsers · one tap, each tool’s own command</span>
    </header>


    <p v-if="runError" class="m-0 px-2 text-label text-accent-text" role="alert">{{ runError }}</p>
    <p v-if="error" class="m-0 rounded-tile bg-surface p-6 text-label text-accent-text" role="alert">{{ error }}</p>
    <p v-else-if="preview && allGroups.length === 0" class="m-0 rounded-tile bg-surface p-6 text-center text-label text-text-secondary">
      No package caches found.
    </p>

    <section v-for="g in groups" :key="g.group" :aria-label="g.group" class="rounded-tile bg-surface p-2" data-testid="group">
      <div class="flex items-baseline justify-between px-3.5 pt-3 pb-2">
        <h2 class="m-0 text-[15px] font-semibold">{{ g.group }}</h2>
        <span class="font-mono text-label text-text-secondary">{{ formatBytes(g.total) }}</span>
      </div>
      <CacheRow
        v-for="t in g.tools"
        :key="t.item_id"
        :cache="t"
        :choice="choices[t.item_id]!"
        :state="rowState(t.item_id).state"
        :pct="rowState(t.item_id).pct"
        :remaining="remainingBytes(t.item_id)"
        :locked="phase !== 'idle' || reloading"
        :freed="freed.has(t.item_id)"
        @toggle="(on) => (choices[t.item_id]!.on = on)"
        @mode="(m) => (choices[t.item_id]!.mode = m)"
      />
      <div
        v-for="s in g.skipped"
        :key="`${s.tool}-${s.path}`"
        class="skip grid min-h-16 items-center gap-3 rounded-card px-3.5 py-2"
        data-testid="skipped-row"
      >
        <span class="flex h-11 items-center justify-center">
          <input type="checkbox" class="m-0 size-[18px]" disabled :aria-label="`${s.name} is skipped`" />
        </span>
        <span class="flex min-w-0 flex-col gap-0.5">
          <span class="flex items-center gap-2">
            <span class="font-semibold text-text-strong-muted">{{ s.name }}</span>
            <span class="skip-chip rounded-chip px-2 py-0.5 text-[11px] font-semibold text-tier-s2-text">skipped</span>
          </span>
          <span class="text-caption text-text-secondary">{{ s.path ? `${shortPath(s.path)} · ` : '' }}{{ skipText(s.reason) }}</span>
        </span>
        <span class="text-right font-mono text-text-tertiary tabular-nums">{{ formatBytes(s.bytes) }}</span>
      </div>
    </section>

    <section v-if="failures.length" class="rounded-tile bg-surface p-5" aria-label="Cache cleanup failures"><h2 class="m-0 text-h3 font-semibold">Some caches could not be cleaned</h2><p v-for="r in failures" :key="r.item_id" role="alert" class="text-label text-accent-text">{{ toolName(r.tool) }}: {{ r.error ?? 'The command did not finish successfully. Refresh the preview to check what remains.' }}</p></section>

    <ListPagination :page="currentPage" :total="entries.length" @update:page="page = $event" />

    <template v-if="!loading" #footer>
      <ActionFooter>
        <ActionBar :summary="phase === 'done' ? `${formatBytes(summary?.measured ?? 0)} freed` : `${chosen.length} caches selected · ${formatBytes(chosenBytes)} estimated`" :label="phase === 'done' ? 'Refresh preview' : phase === 'running' ? 'Queued / cleaning…' : 'Clean caches'" :busy="phase === 'running' || reloading" :disabled="phase !== 'done' && (!chosen.length || !!preview?.unavailable)" :error="preview?.unavailable" data-testid="start" @action="phase === 'done' ? reset() : confirming = true" />
      </ActionFooter>
      <ActionConfirm v-if="confirming" title="Clean selected caches?" label="Clean caches" @cancel="confirming = false" @confirm="start">
        <ul class="m-0 pl-5"><li v-for="t in chosen" :key="t.item_id">{{ t.name }} · {{ choices[t.item_id]?.mode }} · {{ shortPath(t.cache) }}</li></ul>
        <p>{{ NOT_UNDOABLE }}</p>
      </ActionConfirm>
    </template>

    <details class="rounded-card bg-surface p-4">
      <summary class="min-h-touch cursor-pointer font-medium">Cache details and skipped tools</summary>
      <section class="flex flex-col gap-2.5 rounded-tile bg-surface p-[22px]">
        <span class="text-label font-medium text-text-tertiary">All caches</span>
        <span class="text-stat-md leading-none font-bold tracking-[-0.03em] tabular-nums" data-testid="all-bytes">{{ formatBytes(allBytes) }}</span>
        <span class="text-label text-text-secondary">Everything here can be downloaded again. Nothing unique lives in a cache.</span>
      </section>
      <section class="flex flex-col gap-2 rounded-tile border border-accent-tint-border bg-accent-tint p-[22px]">
        <h2 class="m-0 text-body font-semibold">Prune or wipe?</h2>
        <p class="m-0 text-label text-text-strong-muted">
          <strong>Prune</strong> removes only what no project uses. <strong>Wipe</strong> empties the cache; the next install
          downloads everything again.
        </p>
        <p class="m-0 text-label text-text-strong-muted">
          pnpm can only be pruned. Your projects link into its store, so deleting it would free almost nothing.
        </p>
        <p class="m-0 text-label text-text-strong-muted" data-testid="not-undoable">{{ NOT_UNDOABLE }}</p>
      </section>
      <section v-if="preview && preview.skipped.length" class="flex flex-col gap-1 rounded-tile bg-surface p-[22px]" data-testid="skipped-card">
        <h2 class="m-0 pb-1.5 text-label font-medium text-text-tertiary">Skipped ({{ preview.skipped.length }})</h2>
        <div v-for="s in preview.skipped" :key="`${s.tool}-${s.path}`" class="flex flex-col gap-0.5 border-t border-line py-2 text-label">
          <span class="font-medium">{{ s.name }}</span>
          <span class="text-caption text-text-secondary">{{ skipText(s.reason) }}</span>
        </div>
        <span class="pt-2 text-caption text-text-tertiary">Skipped tools are never run. Nothing of theirs is touched.</span>
      </section>
      <section v-if="summary && results.some((r) => r.explanation)" class="flex flex-col gap-1 rounded-tile bg-surface p-[22px]">
        <h2 class="m-0 pb-1.5 text-label font-medium text-text-tertiary">Measured</h2>
        <div v-for="r in results" :key="r.item_id" class="flex justify-between gap-3 border-t border-line py-2 text-label">
          <span>{{ r.explanation ? `${toolName(r.tool)}: ${r.explanation}` : toolName(r.tool) }}</span>
          <span class="font-mono">{{ formatBytes(r.freed_bytes) }}</span>
        </div>
      </section>
    </details>
  </ScreenLayout>
</template>

<style scoped>
.skip {
  grid-template-columns: 40px minmax(0, 1fr) 84px;
}
.skip-chip {
  background: var(--tier-s2-fill);
}
</style>
