<script setup lang="ts">
// Insights (design 05): trends from the user's own history. The forecast (tile and dashed line)
// is the core's (`forecasts`), the same as the digest, the menu bar and `scull check`. Every series comes from the
// read-only history commands (free_space_history, rollup_history, regrowth, footprint_history)
// and the last scan's worktrees; src/components/insights/model.ts turns them into figures.
// Tiles without a data source yet (duplication, cleanup accuracy) say so; with the mock bridge
// `?demo=design` shows the design's illustrative values. Nothing here changes anything: the
// two actions go through clearly named stubs (src/components/insights/actions.ts).
import { computed, onMounted, ref, watch } from 'vue';
import { RouterLink, useRoute } from 'vue-router';
import {
  toBridgeError,
  useBridge,
  type DesktopStatus,
  type DigestView,
  type FootprintHistory,
  type FreeSpaceHistory,
  type Forecast,
  type MonitorStatus,
  type RegrowthHistory,
  type RollupHistory,
  type WorktreesView,
} from '../../bridge';
import { formatBytes } from '../../lib/format';
import { formatWhen } from '../../monitor/format';
import { forecastOf } from '../../lib/forecast';
import ScreenLayout from '../../components/shell/ScreenLayout.vue';
import ActionFooter from '../../components/shell/ActionFooter.vue';
import ActionBar from '../../components/shell/ActionBar.vue';
import ForecastChart from '../../components/insights/ForecastChart.vue';
import { insightsActionsFor } from '../../components/insights/actions';
import { DESIGN_ACCURACY, DESIGN_DUPLICATION } from '../../components/insights/designScenario';
import {
  DAY,
  HISTORY_BUDGET,
  RANGES,
  agentModel,
  drivenBy,
  footprintModel,
  freeSpaceModel,
  mixModel,
  rangeDays,
  regrowthModel,
  type RangeId,
} from '../../components/insights/model';

const bridge = useBridge();
const actions = insightsActionsFor(bridge);
const route = useRoute();
const demo = computed(() => bridge.kind === 'mock' && route.query.demo === 'design');

const range = ref<RangeId>('30d');
const free = ref<FreeSpaceHistory | null>(null);
const rollups = ref<RollupHistory | null>(null);
const mixRollups = ref<RollupHistory | null>(null);
const regrowth = ref<RegrowthHistory | null>(null);
const footprint = ref<FootprintHistory | null>(null);
const worktrees = ref<WorktreesView | null>(null);
const status = ref<DesktopStatus | null>(null);
const digest = ref<DigestView | null>(null);
/** The core's forecasts (one forecast source with check/status/digest/menu bar). */
const forecasts = ref<Forecast[]>([]);
const monitor = ref<MonitorStatus | null>(null);
const loadError = ref('');
const loading = ref(true);
const updating = ref(false);
let loadSequence = 0;
const actionNote = ref('');
const now = ref(Date.now() / 1000);

async function load() {
  const sequence = ++loadSequence;
  updating.value = true;
  now.value = Date.now() / 1000;
  const since = Math.floor(now.value - rangeDays(range.value) * DAY);
  try {
    const [f, r, g, p] = await Promise.all([
      bridge.freeSpaceHistory(since),
      bridge.rollupHistory(since),
      bridge.regrowth(since),
      bridge.footprintHistory(since),
    ]);
    if (sequence !== loadSequence) return;
    free.value = f;
    rollups.value = r;
    regrowth.value = g;
    footprint.value = p;
    loadError.value = '';
  } catch (e) {
    if (sequence === loadSequence) loadError.value = toBridgeError(e).message;
  } finally {
    if (sequence === loadSequence) updating.value = false;
  }
}

onMounted(async () => {
  const initialRange = load();
  // Six months for the mix, the last scan's worktrees and reclaimable bytes: once.
  const [m, w, s, d, ms, fc] = await Promise.allSettled([
    bridge.rollupHistory(Math.floor(now.value - 183 * DAY)),
    bridge.worktrees(),
    bridge.status(),
    bridge.weeklyDigest(),
    bridge.monitorStatus(),
    bridge.forecasts(),
  ]);
  if (fc.status === 'fulfilled') forecasts.value = fc.value.forecasts;
  if (m.status === 'fulfilled') mixRollups.value = m.value;
  if (w.status === 'fulfilled') worktrees.value = w.value;
  if (s.status === 'fulfilled') status.value = s.value;
  if (d.status === 'fulfilled') digest.value = d.value;
  if (ms.status === 'fulfilled') monitor.value = ms.value;
  await initialRange;
  loading.value = false;
});
watch(range, load);

const rangeLabel = computed(() => RANGES.find((r) => r.id === range.value)?.label ?? '');
const noHistory = computed(() => free.value?.state === 'no_database');
const unavailable = computed(() => [free.value, rollups.value, regrowth.value, footprint.value].find((d) => d?.state === 'unavailable')?.detail ?? null);

const fs = computed(() => freeSpaceModel(free.value?.samples ?? [], forecastOf(forecasts.value, free.value?.volume_id)));
/** Why there is no forecast, in the core's words. */
const noForecastText = computed(() => {
  switch (fs.value.state) {
    case 'below_reserve':
      return `Already below ${formatBytes(fs.value.reserve)} free: free up space now.`;
    case 'not_filling':
      return "Free space isn't shrinking.";
    default:
      return 'Not enough history yet: the forecast needs a few days of scans or checks.';
  }
});
const driver = computed(() => drivenBy(rollups.value?.series ?? []));
const regrow = computed(() => regrowthModel(regrowth.value?.events ?? []));
const mix = computed(() => mixModel(mixRollups.value?.series ?? []));
const agents = computed(() => agentModel(worktrees.value?.worktrees ?? [], now.value));
const fp = computed(() => footprintModel(footprint.value?.samples ?? []));
const reclaimable = computed(() => status.value?.last_scan?.reclaimable_bytes ?? 0);

// The weekly digest (INS-04): the same lines the Monday notification carries.
const digestLines = computed(() => digest.value?.digest?.lines ?? []);
const digestOn = computed(() => monitor.value?.settings.digest?.enabled === true);
const digestNote = computed(() => {
  if (!digestOn.value) return 'The weekly digest is off. Turn it on to get these lines as a notification every Monday. It never leaves this computer.';
  const next = monitor.value?.next_digest;
  return next ? `Sent as a notification every Monday morning. Next: ${formatWhen(next)}.` : 'Sent as a notification every Monday morning.';
});

const rateText = computed(() => (fs.value.trend ? `−${formatBytes(fs.value.trend.perDay)}/day` : 'Not shrinking'));

/** Mix colors: ML caches carry the amber stripes (design 05), the rest ink, violet, plum. */
const STRIPES = 'var(--tier-s2-fill)';
const STRIPES_SM = 'repeating-linear-gradient(135deg, var(--color-tier-s2) 0 3px, var(--color-tier-s2-stripe) 3px 6px)';
const mixLook = computed(() => {
  const solid = ['var(--color-ink)', 'var(--color-tier-s0)', 'var(--color-tier-s3-swatch)'];
  let i = 0;
  return Object.fromEntries(
    (mix.value?.groups ?? []).map((g) => {
      if (g.key === 'ml_cache') return [g.key, { bar: STRIPES, swatch: STRIPES_SM, pattern: 'striped' }];
      if (g.key === 'rest') return [g.key, { bar: 'var(--color-tier-s3-swatch)', swatch: 'var(--color-tier-s3-swatch)', pattern: 'solid' }];
      const c = solid[Math.min(i++, solid.length - 1)]!;
      return [g.key, { bar: c, swatch: c, pattern: 'solid' }];
    }),
  ) as Record<string, { bar: string; swatch: string; pattern: string }>;
});
/** Stacking order top to bottom, like the design (ML on top). */
const mixStack = computed(() => {
  const gs = mix.value?.groups ?? [];
  return [...gs.filter((g) => g.key === 'ml_cache'), ...gs.filter((g) => g.key !== 'ml_cache')];
});
const pct = (x: number) => `${Math.round(x * 100)}%`;
const mixLabel = computed(() => {
  const m = mix.value;
  if (!m) return '';
  return `Development storage by category per month, ${m.months[0]} to ${m.months.at(-1)}. ${m.groups.map((g) => `${g.label} ${pct(g.share)}`).join(', ')} in ${m.months.at(-1)}.`;
});

const agentLabel = computed(() =>
  agents.value ? `Agent worktrees by week of last activity over 8 weeks: ${agents.value.weeks.join(', ')}` : '',
);
const agentMax = computed(() => Math.max(1, ...(agents.value?.weeks ?? [])));

const accuracyMax = computed(() => Math.max(...DESIGN_ACCURACY.rows.map((r) => r.predicted)));

async function stopSuggesting() {
  if (!regrow.value) return;
  try {
    await actions.neverSuggest(regrow.value.path);
    actionNote.value = `scull won't suggest ${regrow.value.name} again.`;
  } catch (e) {
    actionNote.value = toBridgeError(e).message;
  }
}
async function exportHistory() {
  try {
    await actions.exportHistory();
    actionNote.value = 'History export prepared.';
  } catch (e) {
    actionNote.value = toBridgeError(e).message;
  }
}
</script>

<template>
  <ScreenLayout :loading="loading" loading-label="Reading storage trends…">
    <header class="flex flex-wrap items-center justify-between gap-3 px-2 py-2">
      <div class="flex flex-col gap-1">
        <h1 class="m-0 text-h2 font-semibold">Insights</h1>
        <span role="status" aria-live="polite" class="text-label text-text-secondary">{{ updating ? 'Updating storage trends…' : `Trends from your own history · last ${rangeLabel}` }}</span>
      </div>
      <div role="group" aria-label="Range" class="flex gap-0.5">
        <button
          v-for="r in RANGES"
          :key="r.id"
          type="button"
          :aria-pressed="range === r.id"
          class="min-h-11 cursor-pointer rounded-chip border-0 px-4 font-medium"
          :class="range === r.id ? 'bg-ink text-on-ink' : 'bg-surface-raised text-ink hover:bg-line'"
          @click="range = r.id"
        >
          {{ r.label }}
        </button>
      </div>
    </header>

    <section v-if="loadError || unavailable" role="alert" class="rounded-tile border border-accent-tint-border bg-accent-tint p-tile-padding">
      <h2 class="m-0 text-h3 font-semibold">History unavailable</h2>
      <p class="m-0 text-label text-text-strong-muted">{{ loadError || unavailable }}</p>
    </section>
    <section v-else-if="noHistory" class="rounded-tile bg-surface p-tile-padding" data-testid="no-history">
      <h2 class="m-0 text-h3 font-semibold">No history yet</h2>
      <p class="m-0 text-label text-text-secondary">Insights appear after a few scans. scull records sizes and free space, never file contents.</p>
    </section>

    <p v-if="actionNote" role="status" class="m-0 rounded-card bg-violet-tint px-4 py-3 text-label">{{ actionNote }}</p>

    <div class="grid grid-cols-12 gap-page-gap">
      <section
        v-if="digestLines.length"
        aria-label="This week"
        class="tile col-span-12 flex flex-wrap items-start gap-x-10 gap-y-4 rounded-tile bg-surface p-tile-padding"
        data-testid="digest"
      >
        <div class="flex flex-[2_1_480px] flex-col gap-2">
          <h2 class="m-0 text-label font-medium text-text-secondary">This week · weekly digest</h2>
          <p class="m-0 text-h3 leading-[1.3] font-semibold">{{ digestLines[0] }}</p>
          <ul v-if="digestLines.length > 1" class="m-0 flex list-none flex-col gap-1 p-0 text-label text-text-strong-muted">
            <li v-for="l in digestLines.slice(1)" :key="l">{{ l }}</li>
          </ul>
        </div>
        <div class="flex flex-[1_1_260px] flex-col items-start gap-3">
          <span class="text-label text-text-secondary" data-testid="digest-note">{{ digestNote }}</span>
          <RouterLink
            :to="{ path: '/settings', query: { tab: 'alerts' } }"
            class="flex min-h-11 items-center rounded-button border border-border bg-surface px-4 font-medium text-ink no-underline hover:bg-surface-raised"
          >
            {{ digestOn ? 'Digest settings' : 'Turn on the weekly digest' }}
          </RouterLink>
        </div>
      </section>

      <section aria-label="Free space forecast" class="tile col-span-12 flex flex-col gap-4 rounded-tile bg-surface p-tile-padding xl:col-span-8">
        <div class="flex flex-wrap justify-between gap-3">
          <h2 class="m-0 text-label font-medium text-text-secondary">Free space · history and forecast</h2>
          <div class="flex gap-4 text-caption" aria-hidden="true">
            <span class="flex items-center gap-1.5"><span class="h-0.5 w-4 bg-ink"></span>Measured</span>
            <span class="flex items-center gap-1.5"><span class="h-0 w-4 border-t-2 border-dashed border-violet-figure"></span>Forecast</span>
            <span class="flex items-center gap-1.5"><span class="size-2 rounded-full bg-violet-figure"></span>Cleanup</span>
          </div>
        </div>
        <ForecastChart v-if="fs.points.length >= 2" :model="fs" />
        <p v-else class="m-0 py-10 text-label text-text-secondary">Free space is recorded with each scan; the chart appears after two scans.</p>
      </section>

      <section aria-label="Forecast summary" class="tile col-span-12 flex flex-col gap-4 rounded-tile border border-accent-tint-border bg-accent-tint p-tile-padding md:col-span-6 xl:col-span-4" data-testid="forecast-summary">
        <h2 class="m-0 text-label font-medium text-text-strong-muted">Forecast</h2>
        <template v-if="fs.trend">
          <span class="text-stat-lg leading-none font-semibold tracking-[-0.04em] tabular-nums">≈ {{ fs.trend.days }} day{{ fs.trend.days === 1 ? '' : 's' }}</span>
          <span class="text-body text-text-strong-muted">until less than {{ formatBytes(fs.reserve) }} free</span>
        </template>
        <template v-else>
          <span class="text-stat-md leading-none font-semibold">No forecast</span>
          <span class="text-body text-text-strong-muted" data-testid="no-forecast">{{ noForecastText }}</span>
        </template>
        <dl class="m-0 mt-2 grid grid-cols-2 gap-1.5">
          <div class="flex flex-col gap-0.5 rounded-[14px] bg-surface p-3.5">
            <dt class="font-mono text-[11px] text-text-strong-muted">Rate</dt>
            <dd class="m-0 font-mono">{{ rateText }}</dd>
          </div>
          <div class="flex flex-col gap-0.5 rounded-[14px] bg-surface p-3.5">
            <dt class="font-mono text-[11px] text-text-strong-muted">Confidence</dt>
            <dd class="m-0 font-mono">{{ fs.trend?.confidence ?? '–' }}</dd>
          </div>
          <div class="col-span-2 flex flex-col gap-0.5 rounded-[14px] bg-surface p-3.5">
            <dt class="font-mono text-[11px] text-text-strong-muted">Driven by</dt>
            <dd class="m-0">{{ driver ? `${driver.label} · ${pct(driver.share)} of growth` : 'No category grew in this range' }}</dd>
          </div>
        </dl>
        <RouterLink
          to="/cleanup"
          class="mt-auto flex min-h-12 items-center justify-center rounded-button bg-tier-s0 font-semibold text-tier-s0-text no-underline hover:bg-violet-hover"
        >
          {{ reclaimable > 0 ? `Review ${formatBytes(reclaimable)} now` : 'Open Cleanup Review' }}
        </RouterLink>
      </section>

      <section aria-label="Regrowth" class="tile col-span-12 flex flex-col gap-3 rounded-tile bg-surface p-tile-padding md:col-span-6 xl:col-span-4" data-testid="regrowth">
        <h2 class="m-0 text-label font-medium text-text-secondary">Regrowth</h2>
        <template v-if="regrow">
          <p class="m-0 text-h3 leading-[1.3] font-semibold">{{ regrow.name }} came back {{ regrow.count }} time{{ regrow.count === 1 ? '' : 's' }} in {{ rangeLabel }}</p>
          <div
            class="flex h-10 items-end gap-0.5"
            role="img"
            :aria-label="`${regrow.name}: present, then gone and rebuilt ${regrow.count} times`"
          >
            <div
              v-for="(b, i) in regrow.bars"
              :key="i"
              class="flex-1 rounded-[6px]"
              :class="b === 'present' ? 'h-full bg-ink' : 'h-[8%] bg-tier-s3-swatch'"
            ></div>
          </div>
          <p class="m-0 text-label text-text-secondary">
            Each time it was rebuilt within {{ Math.max(1, Math.round(regrow.medianGapDays)) }} day{{ Math.round(regrow.medianGapDays) > 1 ? 's' : '' }}, so cleaning it only costs you install time. You use this project actively.
          </p>
          <button
            type="button"
            class="mt-auto min-h-11 cursor-pointer rounded-button border border-border bg-surface font-medium text-ink hover:bg-surface-raised"
            @click="stopSuggesting"
          >
            Stop suggesting it
          </button>
        </template>
        <p v-else class="m-0 text-label text-text-secondary">Nothing came back after a cleanup in {{ rangeLabel }}.</p>
      </section>

      <section aria-label="Duplication" class="tile col-span-12 flex flex-col gap-3 rounded-tile bg-surface p-tile-padding md:col-span-6 xl:col-span-4" data-testid="duplication">
        <h2 class="m-0 text-label font-medium text-text-secondary">Duplication</h2>
        <template v-if="demo">
          <div class="flex items-baseline gap-2">
            <span class="text-stat-md leading-none font-semibold tracking-[-0.03em] tabular-nums">{{ (DESIGN_DUPLICATION.bytes / 1024 ** 3).toFixed(1) }}</span>
            <span class="text-[15px]">GiB of identical packages</span>
          </div>
          <ul class="m-0 flex list-none flex-col p-0">
            <li v-for="(r, i) in DESIGN_DUPLICATION.rows" :key="r.name" class="flex justify-between border-t border-line py-2 text-label" :class="{ 'border-b': i === DESIGN_DUPLICATION.rows.length - 1 }">
              <span class="font-mono">{{ r.name }}</span><span>{{ r.where }}</span>
            </li>
          </ul>
          <p class="m-0 text-label text-text-secondary">{{ DESIGN_DUPLICATION.note }}</p>
        </template>
        <p v-else class="m-0 text-label text-text-secondary">Identical packages across projects aren't measured yet. scull will show them here once it compares package stores.</p>
      </section>

      <section aria-label="Agent footprint" class="tile col-span-12 flex flex-col gap-3 rounded-tile bg-surface p-tile-padding md:col-span-6 xl:col-span-4" data-testid="agents">
        <h2 class="m-0 text-label font-medium text-text-secondary">Agent footprint</h2>
        <template v-if="agents">
          <div class="flex items-baseline gap-2">
            <span class="text-stat-md leading-none font-semibold tracking-[-0.03em] tabular-nums">{{ agents.count }}</span>
            <span class="text-[15px]">agent worktree{{ agents.count === 1 ? '' : 's' }} · {{ formatBytes(agents.idleBytes) }} idle</span>
          </div>
          <div role="img" :aria-label="agentLabel" class="flex h-[72px] items-end gap-1">
            <div
              v-for="(n, i) in agents.weeks"
              :key="i"
              class="flex-1 rounded-[6px]"
              :class="i === agents.weeks.length - 1 ? 'bg-violet-figure' : 'bg-tier-s1'"
              :style="{ height: `${Math.max(4, (n / agentMax) * 100)}%` }"
            ></div>
          </div>
          <div class="flex justify-between font-mono text-[11px] text-text-secondary" aria-hidden="true">
            <span>8 weeks ago</span><span>this week · {{ agents.weeks.at(-1) }} active</span>
          </div>
          <p class="m-0 text-label text-text-secondary">
            {{ agents.stale }} idle more than a week. {{ agents.byTool.map((t) => `${t.tool} ${t.count}`).join(' · ') }}.
          </p>
        </template>
        <p v-else class="m-0 text-label text-text-secondary">No agent worktrees in the last scan.</p>
      </section>

      <section aria-label="Cleanup accuracy" class="tile col-span-12 flex flex-col gap-3.5 rounded-tile bg-surface p-tile-padding lg:col-span-6" data-testid="accuracy">
        <div class="flex flex-wrap justify-between gap-3">
          <h2 class="m-0 text-label font-medium text-text-secondary">Predicted vs measured · last 4 cleanups</h2>
          <span v-if="demo" class="font-mono text-caption">median error {{ pct(DESIGN_ACCURACY.medianError) }}</span>
        </div>
        <template v-if="demo">
          <table class="w-full border-collapse">
            <caption class="sr-only">Predicted and measured bytes freed per cleanup</caption>
            <thead class="sr-only">
              <tr><th scope="col">Date</th><th scope="col">Bars</th><th scope="col">Predicted → measured</th></tr>
            </thead>
            <tbody>
              <tr v-for="r in DESIGN_ACCURACY.rows" :key="r.date">
                <th scope="row" class="w-16 py-1.5 text-left font-mono text-caption font-normal">{{ r.date }}</th>
                <td class="px-3 py-1.5" aria-hidden="true">
                  <div class="flex flex-col gap-[3px]">
                    <div class="h-2 rounded-chip bg-tier-s3-swatch" :style="{ width: `${(r.predicted / accuracyMax) * 100}%` }"></div>
                    <div class="h-2 rounded-chip bg-ink" :style="{ width: `${(r.measured / accuracyMax) * 100}%` }"></div>
                  </div>
                </td>
                <td class="w-[160px] py-1.5 text-right font-mono text-caption whitespace-nowrap">{{ formatBytes(r.predicted) }} → {{ formatBytes(r.measured) }}</td>
              </tr>
            </tbody>
          </table>
          <div class="flex gap-4 text-caption" aria-hidden="true">
            <span class="flex items-center gap-1.5"><span class="h-2 w-3 rounded-[4px] bg-tier-s3-swatch"></span>Predicted</span>
            <span class="flex items-center gap-1.5"><span class="h-2 w-3 rounded-[4px] bg-ink"></span>Measured</span>
          </div>
          <p class="m-0 text-label text-text-secondary">{{ DESIGN_ACCURACY.note }}</p>
        </template>
        <p v-else class="m-0 text-label text-text-secondary">After your first cleanups, scull compares what it predicted with what the disk measured.</p>
      </section>

      <section aria-label="Storage mix" class="tile col-span-12 flex flex-col gap-3.5 rounded-tile bg-surface p-tile-padding lg:col-span-6" data-testid="mix">
        <h2 class="m-0 text-label font-medium text-text-secondary">Storage mix · {{ mix?.months.length ?? 6 }} months</h2>
        <template v-if="mix && mix.max > 0">
          <div role="img" :aria-label="mixLabel" class="flex h-[180px] items-end gap-2">
            <div
              v-for="(m, i) in mix.months"
              :key="m"
              class="flex flex-1 flex-col gap-0.5 overflow-hidden rounded-small"
              :style="{ height: `${(mixStack.reduce((a, g) => a + g.values[i]!, 0) / mix.max) * 100}%` }"
            >
              <div v-for="g in mixStack" :key="g.key" :style="{ flex: g.values[i], background: mixLook[g.key]?.bar }"></div>
            </div>
          </div>
          <div class="flex justify-between font-mono text-[11px] text-text-secondary" aria-hidden="true">
            <span v-for="m in mix.months" :key="m">{{ m }}</span>
          </div>
          <div class="flex flex-wrap gap-4 text-caption">
            <span v-for="g in mixStack" :key="g.key" class="flex items-center gap-1.5">
              <span class="size-3 rounded-[4px]" :style="{ background: mixLook[g.key]?.swatch }" aria-hidden="true"></span>{{ g.label }} {{ pct(g.share) }}
            </span>
          </div>
          <table class="sr-only">
            <caption>Storage by category per month</caption>
            <thead>
              <tr><th scope="col">Month</th><th v-for="g in mixStack" :key="g.key" scope="col">{{ g.label }}</th></tr>
            </thead>
            <tbody>
              <tr v-for="(m, i) in mix.months" :key="m">
                <th scope="row">{{ m }}</th>
                <td v-for="g in mixStack" :key="g.key">{{ formatBytes(g.values[i]) }}</td>
              </tr>
            </tbody>
          </table>
        </template>
        <p v-else class="m-0 text-label text-text-secondary">The mix appears after scans in more than one month.</p>
      </section>

      <section aria-label="What scull knows" class="tile col-span-12 flex flex-wrap items-center gap-x-10 gap-y-6 rounded-tile bg-surface p-tile-padding" data-testid="knows">
        <div class="flex flex-[1_1_280px] flex-col gap-1.5">
          <h2 class="m-0 text-label font-medium text-text-secondary">What scull knows</h2>
          <span class="text-h3 font-semibold">{{ fp ? `${formatBytes(fp.bytes)} of history, all on this computer` : 'No history recorded yet' }}</span>
          <span class="text-label text-text-secondary">No file contents, secrets or environment values, ever.</span>
        </div>
        <div class="flex flex-[2_1_420px] flex-col gap-2">
          <div
            class="flex h-3 overflow-hidden rounded-chip bg-surface-raised"
            role="meter"
            aria-label="History size"
            :aria-valuenow="fp?.bytes ?? 0"
            aria-valuemin="0"
            :aria-valuemax="HISTORY_BUDGET"
            :aria-valuetext="`${formatBytes(fp?.bytes ?? 0)} of ${formatBytes(HISTORY_BUDGET)}`"
          >
            <div class="bg-ink" :style="{ width: `${Math.max(fp ? 1 : 0, (fp?.share ?? 0) * 100)}%` }"></div>
          </div>
          <div class="flex justify-between font-mono text-[11px] text-text-secondary" aria-hidden="true">
            <span>{{ formatBytes(fp?.bytes ?? 0) }} used</span><span>{{ formatBytes(HISTORY_BUDGET) }} budget</span>
          </div>
        </div>
      </section>
    </div>
    <ActionFooter aria-label="Insights actions">
      <ActionBar summary="Storage history stays on this computer" label="Export history" @action="exportHistory">
        <RouterLink to="/settings?tab=data" class="flex min-h-12 items-center rounded-button border border-border bg-surface px-4 font-medium text-ink no-underline">Open data page</RouterLink>
      </ActionBar>
    </ActionFooter>
  </ScreenLayout>
</template>
