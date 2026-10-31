<script setup lang="ts">
// Quick Clean · package caches (design 01 / 01c; D-20). Chips toggle caches; the button asks
// for confirmation (not undoable), then the chips pop in turn while the fill shimmers and the
// figure counts down; a violet card with a particle burst shows what was freed.
//
// The action is the `quickCleanPreview` STUB (quickClean.ts): simulated with the mock bridge,
// refused with the Tauri bridge. Nothing is ever deleted from here.
import { computed, onBeforeUnmount, reactive, ref, watch } from 'vue';
import type { ScullBridge } from '../../bridge';
import { formatBytes } from '../../lib/format';
import { nextFrame } from './motion';
import {
  CLEAN_MS,
  PARTICLES,
  quickCleanFrame,
  quickCleanPreview,
  type Phase,
  type QuickCleanResult,
  type QuickCleanTool,
} from './quickClean';
import { splitBytes } from './summary';

const props = defineProps<{ tools: QuickCleanTool[]; bridge: ScullBridge; reduced: boolean }>();
const emit = defineEmits<{ state: [{ phase: Phase; cleaned: string[]; freed: number; result: QuickCleanResult | null }] }>();

const phase = ref<Phase>('idle');
const p = ref(0);
const off = reactive(new Set<string>());
const result = ref<QuickCleanResult | null>(null);
const error = ref('');
let stop: (() => void) | null = null;

const frame = computed(() => quickCleanFrame(props.tools, off, phase.value, p.value));
const shown = computed(() => splitBytes(frame.value.remaining));

watch(
  [phase, () => frame.value.cleaned.length, () => Math.round(frame.value.freed)],
  () => emit('state', { phase: phase.value, cleaned: frame.value.cleaned, freed: frame.value.freed, result: result.value }),
);

function toggle(id: string) {
  if (phase.value !== 'idle' && phase.value !== 'confirm') return;
  if (off.has(id)) off.delete(id);
  else off.add(id);
}

function primary() {
  if (frame.value.chosen === 0) return;
  if (phase.value === 'idle') phase.value = 'confirm';
  else if (phase.value === 'confirm') void run();
}

async function run() {
  const chosen = props.tools.filter((t) => !off.has(t.id));
  let res: QuickCleanResult;
  try {
    res = await quickCleanPreview(props.bridge, chosen);
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e);
    phase.value = 'unavailable';
    return;
  }
  result.value = res;
  phase.value = 'cleaning';
  p.value = 0;
  if (props.reduced) {
    p.value = 1;
    phase.value = 'done';
    return;
  }
  const t0 = performance.now();
  const tick = (now: number) => {
    p.value = Math.min(1, (now - t0) / CLEAN_MS);
    if (p.value >= 1) {
      stop = null;
      phase.value = 'done';
      return;
    }
    stop = nextFrame(tick);
  };
  stop = nextFrame(tick);
}

function reset() {
  phase.value = 'idle';
  p.value = 0;
  result.value = null;
  error.value = '';
}

onBeforeUnmount(() => stop?.());

const CHIP = {
  off: 'border-border bg-transparent text-text-tertiary',
  on: 'border-ink bg-ink text-on-ink',
  active: 'chip-active border-accent bg-ink text-on-ink',
  done: 'chip-done border-[#7C4DFF] bg-[#7C4DFF] text-on-ink',
} as const;
</script>

<template>
  <section aria-label="Quick Clean package caches" class="relative flex flex-col gap-3 overflow-hidden rounded-tile-lg bg-surface p-tile-padding-sm" data-testid="quick-clean">
    <div class="flex items-baseline justify-between gap-2.5">
      <span class="text-label font-medium text-text-tertiary">Quick Clean · package caches</span>
      <span class="text-[28px] font-bold tracking-[-0.02em] whitespace-nowrap tabular-nums" data-testid="qc-total">{{ shown[0] }} {{ shown[1] }}</span>
    </div>

    <p v-if="tools.length === 0" class="m-0 text-label text-text-secondary">No package caches found.</p>
    <div v-else role="group" aria-label="Caches to clean" class="flex flex-wrap gap-1.5">
      <button
        v-for="c in frame.chips"
        :key="c.tool.id"
        type="button"
        :aria-pressed="c.state !== 'off'"
        class="chip min-h-[34px] cursor-pointer rounded-chip border px-3 text-label font-medium"
        :class="CHIP[c.state]"
        :data-chip="c.state"
        @click="toggle(c.tool.id)"
      >
        {{ c.label }}
      </button>
    </div>

    <p v-if="phase === 'confirm'" class="m-0 text-label text-ink" data-testid="qc-confirm">
      Clean {{ frame.chips.filter((c) => c.state !== 'off').map((c) => c.tool.label).join(', ') }} ({{ formatBytes(frame.total) }})?
      <span class="text-text-secondary">Not undoable: packages re-download on next install.</span>
    </p>
    <p v-if="phase === 'unavailable'" role="alert" class="m-0 rounded-card bg-accent-tint p-3 text-label text-ink" data-testid="qc-unavailable">
      {{ error }}
    </p>

    <div class="flex gap-2">
      <button
        v-if="tools.length > 0 && phase !== 'unavailable'"
        type="button"
        :disabled="phase === 'cleaning' || phase === 'done' || frame.chosen === 0"
        class="relative min-h-[46px] flex-1 cursor-pointer overflow-hidden rounded-button-lg border-0 font-semibold disabled:cursor-default"
        :class="phase === 'idle' || phase === 'confirm' ? 'bg-ink text-on-ink' : 'bg-surface-raised text-ink'"
        data-testid="qc-button"
        @click="primary"
      >
        <span class="fill absolute inset-y-0 left-0 rounded-button-lg" :style="{ width: `${frame.fill}%` }" aria-hidden="true"></span>
        <span class="relative">{{ frame.button }}</span>
      </button>
      <button
        v-if="phase === 'confirm' || phase === 'unavailable'"
        type="button"
        class="min-h-[46px] cursor-pointer rounded-button-lg border border-border bg-transparent px-4 font-medium text-ink"
        data-testid="qc-cancel"
        @click="reset"
      >
        {{ phase === 'unavailable' ? 'OK' : 'Cancel' }}
      </button>
    </div>
    <span class="text-caption text-text-hint">Re-downloads on next install · not undoable</span>

    <div
      v-if="phase === 'done' && result"
      class="done absolute inset-0 flex flex-col items-center justify-center gap-1 rounded-tile-lg bg-[#7C4DFF] p-5 text-center text-on-ink"
      role="status"
      data-testid="qc-done"
    >
      <div v-if="!reduced" class="absolute top-[40%] left-1/2 size-0" aria-hidden="true">
        <span
          v-for="(pt, i) in PARTICLES"
          :key="i"
          class="particle absolute -top-[5px] -left-[5px] opacity-0"
          :style="{ width: `${pt.size}px`, height: `${pt.size}px`, borderRadius: pt.radius, background: pt.color, animation: pt.animation }"
        ></span>
      </div>
      <span class="freed text-[44px] leading-none font-bold tracking-[-0.03em]">+{{ formatBytes(result.freed) }}</span>
      <span class="rise-1 text-body font-semibold">freed · measured {{ formatBytes(result.measured) }}</span>
      <button
        type="button"
        class="rise-2 mt-2.5 min-h-11 cursor-pointer rounded-chip border-0 bg-surface px-5 font-semibold text-ink"
        data-testid="qc-nice"
        @click="reset"
      >
        Nice
      </button>
      <span v-if="result.simulated" class="rise-2 mt-1 font-mono text-[11px] text-on-ink">simulated · mock bridge, nothing deleted</span>
    </div>
  </section>
</template>

<style scoped>
.chip {
  transition:
    background 200ms ease,
    color 200ms ease;
}
.chip-active {
  animation: scull-pulse 700ms ease-in-out infinite;
}
.chip-done {
  animation: scull-pop 340ms ease-out;
}
.fill {
  background: repeating-linear-gradient(135deg, #7c4dff 0 12px, #9c78ff 12px 24px);
  background-size: 48px 100%;
  animation: scull-shimmer 600ms linear infinite;
  transition: width 120ms linear;
}
.done {
  animation: scull-rise 260ms ease-out;
}
.freed {
  animation: qc-bounce 640ms cubic-bezier(0.2, 0.9, 0.3, 1.3) both;
}
.rise-1 {
  animation: scull-rise 400ms 350ms ease-out both;
}
.rise-2 {
  animation: scull-rise 400ms 600ms ease-out both;
}
@keyframes scull-pulse {
  0%,
  100% {
    opacity: 1;
  }
  50% {
    opacity: 0.5;
  }
}
@keyframes scull-pop {
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
@keyframes qc-bounce {
  0% {
    transform: scale(0.4);
    opacity: 0;
  }
  55% {
    transform: scale(1.12);
    opacity: 1;
  }
  75% {
    transform: scale(0.96);
  }
  100% {
    transform: scale(1);
  }
}
</style>

<style>
/* Burst keyframes (design scull-b0..b9): unscoped, because the particles reference them from
   inline styles, which scoped CSS does not rewrite. */
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
