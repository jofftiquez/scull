<script setup lang="ts">
// "Selected for Quick Clean" (design 08): the selected total, one button, and the done state
// (08b) with the particle burst and the measured result.
import { computed } from 'vue';
import { formatBytes } from '../../lib/format';
import { particles, type RunPhase } from './model';

const props = defineProps<{
  phase: RunPhase;
  bytes: number;
  count: number;
  /** Run progress 0–1. */
  progress: number;
  /** Bytes still to go while running. */
  remaining: number;
  done: { count: number; planned: number; measured: number } | null;
  disabledReason: string | null;
}>();
defineEmits<{ start: []; reset: [] }>();

const running = computed(() => props.phase === 'running');
const burst = particles();
const label = computed(() => {
  if (running.value) return `Cleaning ${Math.round(props.progress * 100)}%`;
  return props.count ? `Clean ${props.count} cache${props.count === 1 ? '' : 's'} · ${formatBytes(props.bytes)}` : 'Select caches';
});
</script>

<template>
  <section aria-label="Quick Clean" class="relative flex flex-wrap items-center gap-x-8 gap-y-5 overflow-hidden rounded-tile bg-ink p-tile-padding text-on-ink" data-testid="hero">
    <div class="flex flex-[1_1_260px] flex-col gap-1">
      <span class="text-label font-medium text-on-ink-muted">Selected for Quick Clean</span>
      <span class="text-[52px] leading-none font-bold tracking-[-0.03em] tabular-nums" data-testid="hero-bytes">{{ formatBytes(running ? remaining : bytes) }}</span>
      <span class="text-label text-on-ink-muted">{{ count }} cache{{ count === 1 ? '' : 's' }} · each tool's own cleanup command · not undoable</span>
    </div>
    <button
      type="button"
      class="relative min-h-[52px] min-w-[240px] overflow-hidden rounded-button-lg border-0 px-6 font-[inherit] text-[15px] font-bold"
      :class="running ? 'bg-ink-raised text-on-ink' : count && !disabledReason ? 'cursor-pointer bg-surface text-ink' : 'bg-surface/70 text-ink'"
      :disabled="running || count === 0 || !!disabledReason || phase === 'done'"
      :title="disabledReason ?? undefined"
      data-testid="start"
      @click="$emit('start')"
    >
      <span class="fill absolute inset-y-0 left-0 rounded-button-lg" :style="{ width: `${running ? Math.round(progress * 100) : 0}%` }"></span>
      <span class="relative">{{ label }}</span>
    </button>
    <p v-if="disabledReason" class="m-0 basis-full text-caption text-on-ink-muted" data-testid="unavailable">{{ disabledReason }}</p>

    <div
      v-if="phase === 'done' && done"
      class="done absolute inset-0 flex flex-wrap items-center justify-center gap-6 rounded-tile bg-ink p-6"
      role="status"
      data-testid="done"
    >
      <div class="pointer-events-none absolute top-1/2 left-1/2 size-0" aria-hidden="true">
        <span
          v-for="(p, i) in burst"
          :key="i"
          class="absolute top-[-5px] left-[-5px] opacity-0"
          :style="{ width: `${p.size}px`, height: `${p.size}px`, borderRadius: p.round ? '50%' : '3px', background: p.color, animation: p.anim }"
        ></span>
      </div>
      <span class="bounce text-[52px] font-bold tracking-[-0.03em]">+{{ formatBytes(done.planned) }}</span>
      <span class="rise1 flex flex-col">
        <span class="font-semibold">freed from {{ done.count }} cache{{ done.count === 1 ? '' : 's' }}</span>
        <span class="text-label text-on-ink-muted">measured {{ formatBytes(done.measured) }}</span>
      </span>
      <button type="button" class="rise2 min-h-touch cursor-pointer rounded-chip border-0 bg-surface px-5 font-[inherit] font-semibold text-ink" @click="$emit('reset')">
        Nice
      </button>
    </div>
  </section>
</template>

<style scoped>
.fill {
  background: repeating-linear-gradient(135deg, var(--color-tier-s0) 0 12px, var(--color-violet-light) 12px 24px);
  background-size: 48px 100%;
  animation: scull-shimmer 600ms linear infinite;
  transition: width 120ms linear;
}
.done {
  animation: scull-rise 280ms ease-out;
}
.bounce {
  animation: cache-bounce 640ms cubic-bezier(0.2, 0.9, 0.3, 1.3) both;
}
.rise1 {
  animation: scull-rise 400ms 300ms ease-out both;
}
.rise2 {
  animation: scull-rise 400ms 500ms ease-out both;
}
@keyframes cache-bounce {
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
/* Burst directions (design 08b); global because the animation names are set inline. */
@keyframes cache-b0 { from { transform: translate(0, 0) scale(1); opacity: 1 } to { transform: translate(160px, -90px) scale(0.3) rotate(160deg); opacity: 0 } }
@keyframes cache-b1 { from { transform: translate(0, 0) scale(1); opacity: 1 } to { transform: translate(-160px, -90px) scale(0.3) rotate(-160deg); opacity: 0 } }
@keyframes cache-b2 { from { transform: translate(0, 0) scale(1); opacity: 1 } to { transform: translate(90px, -140px) scale(0.3) rotate(120deg); opacity: 0 } }
@keyframes cache-b3 { from { transform: translate(0, 0) scale(1); opacity: 1 } to { transform: translate(-90px, -140px) scale(0.3) rotate(-120deg); opacity: 0 } }
@keyframes cache-b4 { from { transform: translate(0, 0) scale(1); opacity: 1 } to { transform: translate(200px, 10px) scale(0.3) rotate(200deg); opacity: 0 } }
@keyframes cache-b5 { from { transform: translate(0, 0) scale(1); opacity: 1 } to { transform: translate(-200px, 10px) scale(0.3) rotate(-200deg); opacity: 0 } }
@keyframes cache-b6 { from { transform: translate(0, 0) scale(1); opacity: 1 } to { transform: translate(30px, -160px) scale(0.3) rotate(90deg); opacity: 0 } }
@keyframes cache-b7 { from { transform: translate(0, 0) scale(1); opacity: 1 } to { transform: translate(-30px, -160px) scale(0.3) rotate(-90deg); opacity: 0 } }
@keyframes cache-b8 { from { transform: translate(0, 0) scale(1); opacity: 1 } to { transform: translate(140px, 80px) scale(0.3) rotate(140deg); opacity: 0 } }
@keyframes cache-b9 { from { transform: translate(0, 0) scale(1); opacity: 1 } to { transform: translate(-140px, 80px) scale(0.3) rotate(-140deg); opacity: 0 } }
</style>
