<script setup lang="ts">
// The menu bar / tray widget (design 11 / 11b, D-23; P4-UI-01). Its own page (menubar.html) in a
// small window the tray icon toggles. States: idle, cleaning, done (and, inside Tauri,
// "unavailable": Quick Clean is refused because the desktop never deletes yet).
//
// READ-ONLY (SAFE-10, inspect-only alpha): it reads status, drives, caches and the free-space
// history; Review, the alerts, the gear and "Open scull" open the main window; "Check now"
// re-reads (live free space from the drives call). Quick Clean is the Overview's
// `quickCleanPreview` stub: simulated with the mock bridge, refused inside Tauri.
import { computed, inject, onBeforeUnmount, onMounted, ref } from 'vue';
import { toBridgeError, useBridge, type ScullBridge } from '../../bridge';
import ScullLogo from '../../components/ScullLogo.vue';
import { designMenuBarBridge, wantsDesign } from '../../components/menubar/designScenario';
import {
  BURST,
  CHECK_MIN_MS,
  CLEAN_MS,
  cleanFrame,
  daysLeft,
  menuBarModel,
  ring,
  toolNames,
  type CleanPhase,
  type MainRoute,
  type MenuBarData,
} from '../../components/menubar/model';
import { WIDGET_WINDOW_KEY, widgetWindowFor } from '../../components/menubar/window';
import { nextFrame, useReducedMotion } from '../../components/overview/motion';
import { quickCleanPreview, type QuickCleanResult } from '../../components/overview/quickClean';
import { ago } from '../../state/core';
import { formatBytes } from '../../lib/format';

const base = useBridge();
const bridge: ScullBridge = wantsDesign() ? designMenuBarBridge(base) : base;
const win = inject(WIDGET_WINDOW_KEY, null) ?? widgetWindowFor(bridge);
const reduced = useReducedMotion();

const data = ref<MenuBarData>({ drives: null, status: null, caches: null, history: null });
const loading = ref(true);
const error = ref('');
const checking = ref(false);
/** Set by "Check now": the figures were just re-read. */
const checkedNow = ref(false);

const model = computed(() => menuBarModel(data.value));

const phase = ref<CleanPhase>('idle');
const p = ref(0);
const result = ref<QuickCleanResult | null>(null);
const refusal = ref('');
let stopFrame: (() => void) | null = null;

const cacheTotal = computed(() => model.value.tools.reduce((a, t) => a + t.bytes, 0));
const frame = computed(() => cleanFrame(cacheTotal.value, phase.value, p.value, result.value?.freed ?? cacheTotal.value));
const free = computed(() => (model.value.volume ? model.value.volume.free + frame.value.freed : null));
const days = computed(() => (free.value === null ? null : daysLeft(free.value, model.value.perDay)));
const ringView = computed(() => {
  const v = model.value.volume;
  return v && free.value !== null ? ring(free.value, v.total, model.value.reclaim?.bytes ?? 0) : null;
});
const checkedLabel = computed(() => {
  if (checking.value) return 'Checking…';
  if (checkedNow.value) return 'Checked just now';
  const a = model.value.checkedAgo;
  return a === null ? 'Not checked yet' : `Checked ${ago(a)}`;
});

const DAY = 86_400;
const HISTORY_DAYS = 30;

async function load(): Promise<void> {
  const since = Math.floor(Date.now() / 1000) - HISTORY_DAYS * DAY;
  const [drives, status, caches, history] = await Promise.allSettled([
    bridge.drives(),
    bridge.status(),
    bridge.caches(),
    bridge.freeSpaceHistory(since),
  ]);
  const val = <T,>(r: PromiseSettledResult<T>): T | null => (r.status === 'fulfilled' ? r.value : null);
  data.value = { drives: val(drives), status: val(status), caches: val(caches), history: val(history) };
  const failed = [drives, status].find((r) => r.status === 'rejected') as PromiseRejectedResult | undefined;
  error.value = failed ? toBridgeError(failed.reason).message : '';
  loading.value = false;
}

/** "Check now": re-read everything (free space is measured live by the drives call). */
async function checkNow(): Promise<void> {
  if (checking.value) return;
  checking.value = true;
  const minimum = new Promise((r) => setTimeout(r, reduced.value ? 0 : CHECK_MIN_MS));
  await Promise.all([load(), minimum]);
  checking.value = false;
  checkedNow.value = true;
}

async function quickClean(): Promise<void> {
  if (phase.value === 'done' || phase.value === 'unavailable') {
    reset();
    return;
  }
  if (phase.value !== 'idle' || model.value.tools.length === 0) return;
  try {
    result.value = await quickCleanPreview(bridge, model.value.tools);
  } catch (e) {
    refusal.value = e instanceof Error ? e.message : String(e);
    phase.value = 'unavailable';
    return;
  }
  phase.value = 'cleaning';
  p.value = 0;
  if (reduced.value) {
    p.value = 1;
    phase.value = 'done';
    return;
  }
  const t0 = performance.now();
  const tick = (now: number) => {
    p.value = Math.min(1, (now - t0) / CLEAN_MS);
    if (p.value >= 1) {
      stopFrame = null;
      phase.value = 'done';
      return;
    }
    stopFrame = nextFrame(tick);
  };
  stopFrame = nextFrame(tick);
}

function reset() {
  stopFrame?.();
  stopFrame = null;
  phase.value = 'idle';
  p.value = 0;
  result.value = null;
  refusal.value = '';
}

function open(route: MainRoute) {
  void win.openMain(route).catch((e) => (error.value = toBridgeError(e).message));
}

function onKey(e: KeyboardEvent) {
  if (e.key === 'Escape') void win.hide().catch(() => undefined);
}
/** The tray shows the window again: re-read (cheap; nothing runs in the background). */
function onFocus() {
  if (!loading.value && !checking.value && phase.value !== 'cleaning') void load();
}

onMounted(() => {
  void load();
  globalThis.addEventListener?.('keydown', onKey);
  globalThis.addEventListener?.('focus', onFocus);
});
onBeforeUnmount(() => {
  stopFrame?.();
  globalThis.removeEventListener?.('keydown', onKey);
  globalThis.removeEventListener?.('focus', onFocus);
});

const DOT: Record<string, string> = {
  critical: 'rounded-full bg-accent',
  warning: 'rotate-45 rounded-[2px] bg-tier-s2',
  info: 'rounded-full border-2 border-tier-s0 bg-transparent',
};
const SEVERITY: Record<string, string> = { critical: 'Critical', warning: 'Warning', info: 'Info' };
</script>

<template>
  <div class="box-border h-screen bg-ground p-2.5 text-body text-ink" data-testid="menubar">
    <main
      class="flex h-full flex-col gap-3 overflow-hidden rounded-tile border border-line bg-surface p-4"
      :aria-busy="loading"
      aria-label="scull menu bar"
      :data-phase="phase"
    >
      <header class="flex items-center gap-2.5">
        <ScullLogo :size="26" :wink="phase === 'done'" />
        <h1 class="m-0 text-[16px] font-semibold">scull</h1>
        <span v-if="days !== null" class="rounded-chip bg-accent-track px-2.5 py-0.5 text-caption font-semibold text-accent-text" data-testid="days-badge">
          {{ days }} {{ days === 1 ? 'day' : 'days' }} left
        </span>
        <button
          type="button"
          aria-label="Settings"
          class="ml-auto inline-flex size-touch cursor-pointer items-center justify-center rounded-small border-0 bg-transparent text-text-strong-muted hover:bg-surface-raised"
          data-testid="settings"
          @click="open('/settings')"
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true">
            <circle cx="12" cy="12" r="3" />
            <path d="M12 3v3M12 18v3M3 12h3M18 12h3M5.6 5.6l2.1 2.1M16.3 16.3l2.1 2.1M5.6 18.4l2.1-2.1M16.3 7.7l2.1-2.1" />
          </svg>
        </button>
      </header>

      <p v-if="error" role="alert" class="m-0 rounded-small bg-accent-tint px-3 py-2 text-caption text-ink" data-testid="error">{{ error }}</p>

      <!-- Free space and forecast -->
      <section aria-label="Free space" class="flex flex-none items-center gap-4 rounded-[18px] bg-surface-raised p-3.5" data-testid="space">
        <svg width="96" height="96" viewBox="0 0 96 96" role="img" :aria-label="ringView?.label ?? 'Disk usage unknown'" class="flex-none" data-testid="ring">
          <circle cx="48" cy="48" r="38" fill="none" stroke="var(--color-ground)" stroke-width="12" />
          <template v-if="ringView">
            <circle
              cx="48"
              cy="48"
              r="38"
              fill="none"
              stroke="var(--color-tier-s3)"
              stroke-width="12"
              :stroke-dasharray="`${ringView.usedDash} 239`"
              transform="rotate(-90 48 48)"
              class="ring-used"
            />
            <circle
              cx="48"
              cy="48"
              r="38"
              fill="none"
              stroke="var(--color-tier-s0)"
              stroke-width="12"
              :stroke-dasharray="`${ringView.recDash} 239`"
              :stroke-dashoffset="ringView.recOff"
              transform="rotate(-90 48 48)"
              class="ring-rec"
            />
          </template>
          <text x="48" y="53" text-anchor="middle" font-size="17" font-weight="700" fill="var(--color-ink)" class="font-sans" data-testid="used-pct">
            {{ ringView ? `${ringView.usedPct}%` : '—' }}
          </text>
        </svg>
        <div class="flex min-w-0 flex-col gap-0.5">
          <span class="text-[30px] leading-[1.05] font-bold tracking-[-0.02em] whitespace-nowrap tabular-nums" data-testid="free">
            {{ free === null ? '—' : formatBytes(free) }}
          </span>
          <span class="text-label text-text-secondary">
            {{ model.volume ? `free of ${formatBytes(model.volume.total)}` : loading ? 'reading free space…' : 'free space unknown' }}
          </span>
          <span v-if="days !== null" class="text-caption text-accent-text" data-testid="forecast">full in about {{ days }} {{ days === 1 ? 'day' : 'days' }}</span>
          <span v-else-if="!loading" class="text-caption text-text-tertiary" data-testid="forecast">no forecast yet: needs a few days of checks</span>
        </div>
      </section>

      <!-- Safe to reclaim: the ready-to-review plan -->
      <section aria-label="Safe to reclaim" class="flex flex-none items-center gap-3 rounded-[18px] border border-accent-tint-border bg-accent-tint p-3.5" data-testid="reclaim">
        <div class="flex min-w-0 flex-1 flex-col">
          <template v-if="model.reclaim">
            <span class="text-h2 font-bold text-violet-text tabular-nums" data-testid="reclaim-bytes">{{ formatBytes(model.reclaim.bytes) }}</span>
            <span class="text-caption text-text-strong-muted" :title="model.reclaim.reviewCommand ?? undefined">
              {{ model.reclaim.source === 'plan' ? 'safe to reclaim now' : 'safe to reclaim (last scan)' }}
            </span>
          </template>
          <template v-else>
            <span class="text-label font-semibold">{{ loading ? '—' : 'Nothing safe to reclaim yet' }}</span>
            <span class="text-caption text-text-strong-muted">scull check prepares a plan to review</span>
          </template>
        </div>
        <button
          v-if="model.reclaim"
          type="button"
          class="inline-flex min-h-touch cursor-pointer items-center rounded-button border-0 bg-ink px-3.5 text-label font-bold text-on-ink"
          data-testid="review"
          @click="open('/cleanup')"
        >
          Review
        </button>
      </section>

      <!-- Quick Clean: package caches -->
      <section aria-label="Package caches" class="relative flex flex-none flex-col gap-2.5 overflow-hidden rounded-[18px] bg-surface-raised p-3.5" data-testid="caches">
        <div class="flex items-center gap-3">
          <div class="flex min-w-0 flex-1 flex-col">
            <span class="text-label font-semibold">Package caches</span>
            <span v-if="phase === 'done' && result?.simulated" class="font-mono text-[11px] leading-[18px] text-text-secondary" data-testid="simulated">simulated · mock bridge, nothing deleted</span>
            <span v-else class="text-caption text-text-secondary" data-testid="tools">{{ model.tools.length ? toolNames(model.tools) : loading ? '—' : 'none found' }}</span>
          </div>
          <span v-if="model.tools.length" class="font-mono text-body whitespace-nowrap tabular-nums" data-testid="cache-bytes">{{ formatBytes(frame.remaining) }}</span>
        </div>
        <p v-if="phase === 'unavailable'" role="alert" class="m-0 text-caption text-ink" data-testid="refused">{{ refusal }}</p>
        <button
          v-if="model.tools.length"
          type="button"
          :disabled="phase === 'cleaning'"
          class="clean relative min-h-touch cursor-pointer overflow-hidden rounded-button border-0 text-label font-bold disabled:cursor-default"
          :class="{
            'bg-ink text-on-ink': phase === 'idle',
            'bg-surface-raised text-ink': phase === 'cleaning',
            'done bg-tier-s0 text-on-ink': phase === 'done',
            'border border-border bg-transparent text-ink': phase === 'unavailable',
          }"
          :aria-label="phase === 'done' ? `${frame.label}. Activate to reset.` : undefined"
          data-testid="quick-clean"
          @click="quickClean"
        >
          <span class="fill absolute inset-y-0 left-0 rounded-button" :style="{ width: `${frame.fill}%` }" aria-hidden="true"></span>
          <span class="relative" aria-live="polite">{{ phase === 'unavailable' ? 'OK' : frame.label }}</span>
        </button>
        <div v-if="phase === 'done' && !reduced" class="pointer-events-none absolute top-[70%] left-1/2 size-0" aria-hidden="true">
          <span
            v-for="(pt, i) in BURST"
            :key="i"
            class="absolute -top-1 -left-1 opacity-0"
            :style="{ width: `${pt.size}px`, height: `${pt.size}px`, borderRadius: pt.radius, background: pt.color, animation: pt.animation }"
          ></span>
        </div>
      </section>

      <!-- Alerts -->
      <section aria-labelledby="alerts-h" class="flex min-h-0 flex-col gap-0.5 overflow-y-auto" data-testid="alerts">
        <h2 id="alerts-h" class="m-0 px-1 pb-1 text-caption font-medium text-text-tertiary">Alerts</h2>
        <p v-if="!model.alerts.length" class="m-0 px-2.5 text-label text-text-secondary" data-testid="no-alerts">{{ loading ? '—' : 'No alerts' }}</p>
        <ul v-else class="m-0 flex list-none flex-col gap-0.5 p-0">
          <li v-for="a in model.alerts" :key="a.key">
            <button
              type="button"
              class="flex min-h-touch w-full cursor-pointer items-center gap-2.5 rounded-button border-0 bg-transparent px-2.5 text-left text-ink hover:bg-surface-raised"
              data-testid="alert"
              :data-tone="a.tone"
              @click="open(a.route)"
            >
              <span class="size-2 flex-none" :class="DOT[a.tone]" aria-hidden="true"></span>
              <span class="sr-only">{{ SEVERITY[a.tone] }}:</span>
              <span class="flex-1 text-label">{{ a.message }}</span>
              <span class="text-text-tertiary" aria-hidden="true">›</span>
            </button>
          </li>
        </ul>
      </section>

      <footer class="mt-auto flex flex-none items-center gap-2 border-t border-line pt-3">
        <button
          type="button"
          class="inline-flex min-h-touch cursor-pointer items-center gap-2 rounded-small border-0 bg-transparent px-2.5 text-caption text-text-strong-muted hover:bg-surface-raised"
          :aria-busy="checking"
          data-testid="check"
          @click="checkNow"
        >
          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true" :class="{ spin: checking }">
            <path d="M20 11a8 8 0 1 0-2.3 5.7" />
            <path d="M20 5v6h-6" />
          </svg>
          <span aria-live="polite">{{ checkedLabel }}</span>
        </button>
        <button
          type="button"
          class="ml-auto inline-flex min-h-touch cursor-pointer items-center rounded-small border-0 bg-category-ring px-3.5 text-label font-semibold text-ink"
          data-testid="open"
          @click="open('/')"
        >
          Open scull
        </button>
      </footer>
    </main>
  </div>
</template>

<style scoped>
.ring-used {
  transition: stroke-dasharray 200ms linear;
}
.ring-rec {
  transition: stroke-dashoffset 200ms linear;
}
.fill {
  background: repeating-linear-gradient(135deg, #7c4dff 0 10px, #9c78ff 10px 20px);
  background-size: 40px 100%;
  animation: scull-shimmer 600ms linear infinite;
  transition: width 120ms linear;
}
.done {
  animation: mb-pop 360ms ease-out;
}
.spin {
  animation: scull-spin 700ms linear infinite;
}
@keyframes mb-pop {
  0% {
    transform: scale(1);
  }
  45% {
    transform: scale(1.14);
  }
  100% {
    transform: scale(1);
  }
}
</style>

<style>
/* Burst keyframes (design scull-b0..b9): unscoped, the particles use them from inline styles.
   Same as the Overview's; the widget is its own page, so it carries its own copy. */
@keyframes scull-b0 {
  from {
    transform: translate(0, 0) scale(1);
    opacity: 1;
  }
  to {
    transform: translate(160px, -90px) scale(0.3) rotate(160deg);
    opacity: 0;
  }
}
@keyframes scull-b1 {
  from {
    transform: translate(0, 0) scale(1);
    opacity: 1;
  }
  to {
    transform: translate(-160px, -90px) scale(0.3) rotate(-160deg);
    opacity: 0;
  }
}
@keyframes scull-b2 {
  from {
    transform: translate(0, 0) scale(1);
    opacity: 1;
  }
  to {
    transform: translate(90px, -140px) scale(0.3) rotate(120deg);
    opacity: 0;
  }
}
@keyframes scull-b3 {
  from {
    transform: translate(0, 0) scale(1);
    opacity: 1;
  }
  to {
    transform: translate(-90px, -140px) scale(0.3) rotate(-120deg);
    opacity: 0;
  }
}
@keyframes scull-b4 {
  from {
    transform: translate(0, 0) scale(1);
    opacity: 1;
  }
  to {
    transform: translate(200px, 10px) scale(0.3) rotate(200deg);
    opacity: 0;
  }
}
@keyframes scull-b5 {
  from {
    transform: translate(0, 0) scale(1);
    opacity: 1;
  }
  to {
    transform: translate(-200px, 10px) scale(0.3) rotate(-200deg);
    opacity: 0;
  }
}
@keyframes scull-b6 {
  from {
    transform: translate(0, 0) scale(1);
    opacity: 1;
  }
  to {
    transform: translate(30px, -160px) scale(0.3) rotate(90deg);
    opacity: 0;
  }
}
@keyframes scull-b7 {
  from {
    transform: translate(0, 0) scale(1);
    opacity: 1;
  }
  to {
    transform: translate(-30px, -160px) scale(0.3) rotate(-90deg);
    opacity: 0;
  }
}
@keyframes scull-b8 {
  from {
    transform: translate(0, 0) scale(1);
    opacity: 1;
  }
  to {
    transform: translate(140px, 80px) scale(0.3) rotate(140deg);
    opacity: 0;
  }
}
@keyframes scull-b9 {
  from {
    transform: translate(0, 0) scale(1);
    opacity: 1;
  }
  to {
    transform: translate(-140px, 80px) scale(0.3) rotate(-140deg);
    opacity: 0;
  }
}
</style>
