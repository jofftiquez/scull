<script setup lang="ts">
// Overview (design 01, 01b, 01c): the storage wheel with its tier toolbar in the middle, the
// radar-sweep rescan in the header; on the right the forecast alert, the safe-to-reclaim hero,
// the selected segment, Quick Clean and the tier breakdown.
//
// Data: the last scan of this session (core.report) plus the package caches. The bridge has
// no "last report" call, so the first visit in a session starts a (read-only) scan.
import { computed, onBeforeUnmount, onMounted, ref, shallowRef, watch } from 'vue';
import type { CachesReport, Drive } from '../../bridge';
import ScreenLayout from '../../components/shell/ScreenLayout.vue';
import StorageWheel from '../../components/overview/StorageWheel.vue';
import TierToolbar from '../../components/overview/TierToolbar.vue';
import { useOverviewBridge } from '../../components/overview/overviewBridge';
import { useReducedMotion } from '../../components/overview/motion';
import { useRadarSweep } from '../../components/overview/useRadarSweep';
import { wheelCenter } from '../../components/overview/center';
import { addToCleanup, cleanupBasket } from '../../components/overview/basket';
import { type TierFilter } from '../../components/overview/tiers';
import { buildWheelModel, collectCandidates, layoutWheel, type WheelItem } from '../../components/overview/wheel';
import ForecastAlert from '../../components/overview/ForecastAlert.vue';
import ReclaimHero from '../../components/overview/ReclaimHero.vue';
import SelectedDetail from '../../components/overview/SelectedDetail.vue';
import QuickCleanCard from '../../components/overview/QuickCleanCard.vue';
import TierBreakdown from '../../components/overview/TierBreakdown.vue';
import DriveSwitcher from '../../components/overview/DriveSwitcher.vue';
import { candidatesOn, chosenDrive, driveChoice, driveContext, driveForecast, driveOptions } from '../../components/overview/drives';
import { ALERT_DAYS } from '../../components/overview/forecast';
import { quickCleanTools, type Phase, type QuickCleanResult } from '../../components/overview/quickClean';
import { reclaimSummary } from '../../components/overview/summary';
import { core, rescan } from '../../state/core';
import { needsOnboarding } from '../../components/onboarding/model';

const bridge = useOverviewBridge();
const reduced = useReducedMotion();
const sweep = useRadarSweep(reduced);

const caches = shallowRef<CachesReport | null>(null);
const filter = ref<TierFilter>('all');
const selected = ref<string | null>(null);
const announcement = ref('');

// Drives (P3-UI-12, D-22): the wheel, Quick Clean, forecast and quarantine follow the chosen drive.
const drives = shallowRef<Drive[]>([]);
const allCandidates = computed(() => collectCandidates(core.report, caches.value));
const driveOpts = computed(() => driveOptions(drives.value, allCandidates.value));
const drive = computed(() => chosenDrive(driveChoice.value, driveOpts.value));
const candidates = computed(() => candidatesOn(allCandidates.value, drive.value, drives.value));
const model = computed(() => (core.report || caches.value ? buildWheelModel(candidates.value) : null));
const arcs = computed(() => (model.value ? layoutWheel(model.value) : []));
// Before the first scan of the session starts, mount waits for `status` to decide whether it may
// scan (onboarding owns the first one). Count that wait as scanning so the header never offers an
// idle "Rescan" over a wheel that is about to be replaced.
const preparing = ref(!core.report && !core.scanning);
const scanningView = computed(() => preparing.value || core.scanning || sweep.sweeping.value);

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

// Quick Clean state, reported by the card while it animates.
const qc = ref<{ phase: Phase; cleaned: string[]; freed: number; result: QuickCleanResult | null }>({
  phase: 'idle',
  cleaned: [],
  freed: 0,
  result: null,
});
const cleaned = computed(() => new Set(qc.value.cleaned));
const tools = computed(() => quickCleanTools(candidatesOn(caches.value?.candidates ?? [], drive.value, drives.value)));
const forecast = computed(() => driveForecast(drive.value, drives.value, core.forecasts, qc.value.freed));
watch(drive, () => (selected.value = null));
const center = computed(() => {
  if (scanningView.value) return wheelCenter({ kind: 'scanning', entries: seenEntries.value, where: where.value });
  if (qc.value.phase === 'done' && qc.value.result) {
    return wheelCenter({ kind: 'freed', bytes: qc.value.result.freed, what: 'package caches' });
  }
  return wheelCenter({ kind: 'idle', model: model.value, selected: selected.value });
});

const regenerable = computed(() => model.value?.byTier.S0.reclaimable ?? 0);

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

function addSelected(item: WheelItem) {
  announcement.value = addToCleanup(item);
}

function focusTiers() {
  document.querySelector<HTMLElement>('[data-testid="tiers"]')?.focus();
}

function clear() {
  selected.value = null;
  filter.value = 'all';
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

watch(() => core.scanning, (running) => {
  if (running && !sweep.sweeping.value) followRunningScan();
});

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
    // Hand over to the sweep in the same tick: startScan sets it before its first await.
    preparing.value = false;
    if (!needsOnboarding(status) && !core.report && !core.scanning) void startScan();
    else if (core.scanning) followRunningScan();
  }
  preparing.value = false;
  bridge
    .drives()
    .then((d) => (drives.value = d))
    .catch(() => {
      // Without the drive list the Overview shows every drive together (no switcher).
    });
  try {
    caches.value = await bridge.caches();
  } catch {
    // Caches are optional on the wheel; the Caches screen reports the error.
  }
});
onBeforeUnmount(() => document.removeEventListener('keydown', onKeydown));
</script>

<template>
  <ScreenLayout :loading="preparing && !core.scanning" loading-label="Preparing storage overview…" :scan-busy="scanningView" aside-label="Details" :detail-open="!!selected" @close-details="selected = null">
    <h1 class="sr-only">Overview</h1>

    <section
      aria-label="Storage wheel"
      class="flex flex-1 flex-col items-center gap-4 rounded-[28px] bg-surface px-6 pt-6 pb-5"
      data-testid="wheel-tile"
    >
      <DriveSwitcher v-if="driveOpts.length" v-model="driveChoice" :options="driveOpts" :context="driveContext(drive, driveOpts)" />
      <StorageWheel
        :arcs="arcs"
        :filter="filter"
        :selected="selected"
        :center="center"
        :sweep="sweep.progress.value"
        :scanning="scanningView"
        :bounce-key="sweep.completed.value + (qc.phase === 'done' ? 1 : 0)"
        :cleaned="cleaned"
        @select="onSelect"
      />
      <TierToolbar v-model="filter" @clear="clear" />
      <span class="sr-only" role="status" data-testid="announce">{{ announcement }}</span>
      <span v-if="cleanupBasket.size > 0" class="font-mono text-caption text-text-tertiary" data-testid="basket">
        {{ cleanupBasket.size }} item{{ cleanupBasket.size === 1 ? '' : 's' }} marked for cleanup
      </span>
    </section>

    <template #aside>
      <ForecastAlert
        v-if="forecast && forecast.days <= ALERT_DAYS"
        :days="forecast.days"
        :safe-bytes="regenerable"
        :drive="driveOpts.length && forecast.drive ? forecast.drive.label : null"
      />
      <ReclaimHero :bytes="regenerable" :summary="reclaimSummary(candidates)" :scanned="model !== null" />
      <SelectedDetail :item="selectedItem" :category="selectedCategory" @add="addSelected" @tiers="focusTiers" />
      <QuickCleanCard :tools="tools" :bridge="bridge" :reduced="reduced" @state="qc = $event" />
      <TierBreakdown :model="model" tabindex="-1" />
    </template>
  </ScreenLayout>
</template>
