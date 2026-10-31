<script setup lang="ts">
// Overview (design 01, 01b): the storage wheel with its tier toolbar in the middle, the
// radar-sweep rescan in the header. The right sidebar lands in P3-UI-02.
//
// Data: the last scan of this session (core.report) plus the package caches. The bridge has
// no "last report" call, so the first visit in a session starts a (read-only) scan.
import { computed, onBeforeUnmount, onMounted, ref, shallowRef, watch } from 'vue';
import type { CachesReport } from '../../bridge';
import ScreenLayout from '../../components/shell/ScreenLayout.vue';
import StorageWheel from '../../components/overview/StorageWheel.vue';
import TierToolbar from '../../components/overview/TierToolbar.vue';
import { useOverviewBridge } from '../../components/overview/overviewBridge';
import { useReducedMotion } from '../../components/overview/motion';
import { useRadarSweep } from '../../components/overview/useRadarSweep';
import { wheelCenter } from '../../components/overview/center';
import { addToCleanup, cleanupBasket } from '../../components/overview/basket';
import { TIERS, TIER_KEYS, type TierFilter } from '../../components/overview/tiers';
import { buildWheelModel, collectCandidates, layoutWheel, type WheelItem } from '../../components/overview/wheel';
import { formatBytes } from '../../lib/format';
import { cancelScan, core, rescan } from '../../state/core';
import { needsOnboarding } from '../../components/onboarding/model';

const bridge = useOverviewBridge();
const reduced = useReducedMotion();
const sweep = useRadarSweep(reduced);

const caches = shallowRef<CachesReport | null>(null);
const filter = ref<TierFilter>('all');
const selected = ref<string | null>(null);
const announcement = ref('');

const model = computed(() =>
  core.report || caches.value ? buildWheelModel(collectCandidates(core.report, caches.value)) : null,
);
const arcs = computed(() => (model.value ? layoutWheel(model.value) : []));
const scanningView = computed(() => core.scanning || sweep.sweeping.value);

// Live scan progress, kept between events (the analysing phase reports no entry count).
const seenEntries = ref(0);
const where = ref('');
watch(
  () => core.progress,
  (p) => {
    if (!p) return;
    if (p.entries != null) seenEntries.value = Math.max(seenEntries.value, p.entries);
    where.value = p.current ?? p.root ?? p.what ?? where.value;
  },
);

const center = computed(() => {
  if (scanningView.value) return wheelCenter({ kind: 'scanning', entries: seenEntries.value, where: where.value });
  return wheelCenter({ kind: 'idle', model: model.value, selected: selected.value });
});

const regenerable = computed(() => model.value?.byTier.S0.reclaimable ?? 0);
const rescanLabel = computed(() =>
  scanningView.value ? `Scanning ${Math.round(sweep.progress.value * 100)}%` : 'Rescan',
);

function findItem(id: string): WheelItem | null {
  for (const c of model.value?.categories ?? []) {
    const it = c.items.find((i) => i.id === id);
    if (it) return it;
  }
  return null;
}

const selectedItem = computed(() => (selected.value ? findItem(selected.value) : null));
const selectedCategory = computed(() => model.value?.categories.find((c) => c.key === selected.value) ?? null);

function onSelect(id: string | null, add: boolean) {
  if (scanningView.value) return;
  if (add && id) {
    const it = findItem(id);
    if (it) announcement.value = addToCleanup(it);
    return;
  }
  selected.value = selected.value === id ? null : id;
}

function clear() {
  selected.value = null;
  filter.value = 'all';
}

function showRegenerable() {
  filter.value = 'S0';
  selected.value = null;
}

async function startScan() {
  if (scanningView.value) return;
  selected.value = null;
  seenEntries.value = 0;
  where.value = '';
  const expected = core.report?.entries_scanned ?? null;
  await sweep.run(() => rescan(bridge), () => seenEntries.value, expected);
}

/** A scan started elsewhere (or before this screen mounted): follow it with the arm. */
function followRunningScan() {
  const expected = core.report?.entries_scanned ?? null;
  void sweep.run(
    () =>
      new Promise<void>((resolve) => {
        const stop = watch(
          () => core.scanning,
          (s) => {
            if (!s) {
              stop();
              resolve();
            }
          },
          { immediate: true },
        );
      }),
    () => seenEntries.value,
    expected,
  );
}

function onKeydown(e: KeyboardEvent) {
  if (e.key === 'Escape') clear();
}

onMounted(async () => {
  document.addEventListener('keydown', onKeydown);
  if (core.scanning) followRunningScan();
  else if (!core.report) {
    // First run: scan roots must be chosen explicitly (CFG-04), so onboarding owns the first
    // scan. Never auto-scan default roots before that.
    const status = core.status ?? (await bridge.status().catch(() => null));
    if (!needsOnboarding(status)) void startScan();
  }
  try {
    caches.value = await bridge.caches();
  } catch {
    // Caches are optional on the wheel; the Caches screen reports the error.
  }
});
onBeforeUnmount(() => document.removeEventListener('keydown', onKeydown));
</script>

<template>
  <ScreenLayout aside-label="Details">
    <template #actions>
      <button
        type="button"
        class="inline-flex min-h-10 cursor-pointer items-center rounded-button border-0 bg-transparent px-3.5 text-label text-text-strong-muted hover:text-ink"
        data-testid="show-regenerable"
        @click="showRegenerable"
      >
        Show regenerable · {{ formatBytes(regenerable) }}
      </button>
      <button
        v-if="core.scanning"
        type="button"
        class="inline-flex min-h-10 cursor-pointer items-center rounded-button border-0 bg-transparent px-3.5 text-label text-text-strong-muted hover:text-ink"
        @click="cancelScan"
      >
        Cancel
      </button>
      <button
        type="button"
        :disabled="scanningView"
        class="inline-flex min-h-10 cursor-pointer items-center gap-2 rounded-button border border-border bg-surface px-3.5 text-label font-medium text-ink disabled:cursor-progress"
        data-testid="rescan"
        @click="startScan"
      >
        <svg
          width="15"
          height="15"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
          stroke-linecap="round"
          aria-hidden="true"
          :style="scanningView ? 'animation: scull-spin 800ms linear infinite' : undefined"
        >
          <path d="M20 11a8 8 0 1 0-2.3 5.7" />
          <path d="M20 5v6h-6" />
        </svg>
        <span class="tabular-nums">{{ rescanLabel }}</span>
      </button>
    </template>

    <section
      aria-label="Storage wheel"
      class="flex flex-1 flex-col items-center gap-4 rounded-[28px] bg-surface px-6 pt-6 pb-5"
      data-testid="wheel-tile"
    >
      <StorageWheel
        :arcs="arcs"
        :filter="filter"
        :selected="selected"
        :center="center"
        :sweep="sweep.progress.value"
        :scanning="scanningView"
        :bounce-key="sweep.completed.value"
        @select="onSelect"
      />
      <TierToolbar v-model="filter" @clear="clear" />
      <span class="text-center font-mono text-caption text-text-hint">
        click a segment to inspect · shift-click adds it to cleanup · esc clears
      </span>
      <span class="sr-only" role="status" data-testid="announce">{{ announcement }}</span>
      <span v-if="cleanupBasket.size > 0" class="font-mono text-caption text-text-tertiary" data-testid="basket">
        {{ cleanupBasket.size }} item{{ cleanupBasket.size === 1 ? '' : 's' }} marked for cleanup
      </span>
    </section>

    <template #aside>
      <section aria-label="Safe to reclaim" class="flex min-h-[276px] flex-col gap-3 rounded-tile-lg bg-surface p-tile-padding">
        <span class="text-label text-text-secondary">Safe to reclaim</span>
        <span class="text-stat-md font-bold text-text-hint">—</span>
        <span class="text-caption text-text-tertiary">P3-UI-02</span>
      </section>
      <section aria-label="Selected item" aria-live="polite" class="flex min-h-[200px] flex-col gap-3 rounded-tile-lg bg-surface p-tile-padding-sm" data-testid="selected">
        <span class="text-label text-text-secondary">{{ selectedItem || selectedCategory ? 'Selected' : 'Nothing selected' }}</span>
        <template v-if="selectedItem">
          <span class="text-h3 font-semibold text-ink">{{ selectedItem.name }}</span>
          <span class="font-mono text-label">{{ formatBytes(selectedItem.bytes) }} · {{ TIERS[selectedItem.tier].label }}</span>
        </template>
        <template v-else-if="selectedCategory">
          <span class="text-h3 font-semibold text-ink">{{ selectedCategory.label }}</span>
          <span class="font-mono text-label">{{ formatBytes(selectedCategory.bytes) }} · {{ TIERS[selectedCategory.tier].label }}</span>
        </template>
        <span v-else class="text-caption text-text-tertiary">Click a segment of the wheel.</span>
      </section>
      <section aria-label="By safety tier" class="flex flex-1 flex-col gap-3 rounded-tile-lg bg-surface p-tile-padding-sm">
        <span class="text-label text-text-secondary">By safety tier</span>
        <div v-for="t in TIER_KEYS" :key="t" class="flex items-center gap-3">
          <span
            class="rounded-chip px-2.5 py-0.5 text-caption font-semibold"
            :style="{ background: t === 'S3' ? 'var(--color-tier-s3)' : `var(--tier-${t.toLowerCase()}-fill)`, color: TIERS[t].badgeFg }"
          >{{ TIERS[t].chip }}</span>
          <span class="text-label text-text-secondary">{{ TIERS[t].hint }}</span>
        </div>
      </section>
    </template>
  </ScreenLayout>
</template>
