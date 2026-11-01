<script setup lang="ts">
// One cache (design 08): tick, name and state, path · resolved command, Prune/Wipe, size, and a
// 3px progress line while it cleans. Orphaned caches (removed by scull's executor, D-36) ask
// for a per-tool confirmation before they can be ticked.
import { computed, nextTick, ref } from 'vue';
import { formatBytes } from '../../lib/format';
import { commandLine, shortPath, type Choice, type Mode, type PlannedCache, type RowState } from './model';

const props = defineProps<{ cache: PlannedCache; choice: Choice; state: RowState; pct: number; locked: boolean; freed: boolean }>();
const emit = defineEmits<{ toggle: [on: boolean]; mode: [mode: Mode] }>();

const confirming = ref(false);
const confirmBtn = ref<HTMLButtonElement | null>(null);
const cmd = computed(() => commandLine(props.cache, props.choice.mode));
const size = computed(() => {
  if (props.freed || props.state === 'done') return formatBytes(0);
  if (props.state === 'active') return formatBytes(Math.round(props.cache.bytes * (1 - props.pct / 100)));
  return formatBytes(props.cache.bytes);
});
const label = computed(() => ({ idle: '', queued: 'queued', active: 'cleaning…', done: 'cleaned ✓', failed: 'failed' })[props.state]);
const modes = computed(() => [
  { id: 'prune' as const, label: 'Prune', available: props.cache.runner === 'tool' && props.cache.prune !== null },
  { id: 'wipe' as const, label: 'Wipe', available: props.cache.wipe !== null },
]);

function tick(on: boolean, el: HTMLInputElement) {
  if (on && props.cache.runner === 'executor') {
    el.checked = false;
    confirming.value = true;
    void nextTick(() => confirmBtn.value?.focus());
    return;
  }
  emit('toggle', on);
}

function confirm(yes: boolean) {
  confirming.value = false;
  if (yes) emit('toggle', true);
}
</script>

<template>
  <div
    class="row relative grid min-h-16 items-center gap-3 overflow-hidden rounded-card px-3.5 py-2"
    :class="state === 'active' ? 'bg-violet-tint' : state === 'done' || freed ? 'bg-surface-subtle' : 'bg-transparent'"
    data-testid="cache-row"
  >
    <span class="progress absolute bottom-0 left-0 h-[3px] bg-tier-s0" :style="{ width: `${pct}%` }" aria-hidden="true"></span>
    <label class="flex h-11 cursor-pointer items-center justify-center">
      <input
        type="checkbox"
        class="m-0 size-[18px] accent-ink"
        :checked="choice.on"
        :disabled="locked || freed"
        :aria-label="`Include ${cache.name}`"
        @change="tick(($event.target as HTMLInputElement).checked, $event.target as HTMLInputElement)"
      />
    </label>
    <span class="flex min-w-0 flex-col gap-0.5">
      <span class="flex items-center gap-2">
        <span class="font-semibold">{{ cache.name }}</span>
        <span
          v-if="label"
          class="text-caption"
          :class="[state === 'done' ? 'pop text-violet-text' : 'text-accent-text', state === 'active' ? 'pulse' : '']"
          data-testid="row-state"
        >{{ label }}</span>
        <span v-if="cache.runner === 'executor' && !label" class="text-caption text-text-tertiary">tool not installed</span>
      </span>
      <span class="truncate font-mono text-caption text-text-tertiary" :title="`${cache.cache} · ${cmd}`" data-testid="row-cmd">
        {{ shortPath(cache.cache) }} · {{ cmd }}
      </span>
    </span>
    <span role="group" :aria-label="`${cache.name} mode`" class="flex gap-0.5 rounded-chip bg-surface-raised p-[3px]">
      <button
        v-for="m in modes"
        :key="m.id"
        type="button"
        :aria-pressed="choice.mode === m.id"
        :disabled="!m.available || locked || freed"
        class="mode min-h-[30px] rounded-chip border-0 px-2.5 font-[inherit] text-caption font-medium"
        :class="[choice.mode === m.id ? 'bg-ink text-on-ink' : 'bg-transparent text-text-strong-muted', m.available ? 'cursor-pointer' : 'opacity-35']"
        @click="emit('mode', m.id)"
      >
        {{ m.label }}
      </button>
    </span>
    <span class="text-right font-mono tabular-nums" data-testid="row-size">{{ size }}</span>
  </div>
  <div
    v-if="confirming"
    role="alertdialog"
    :aria-label="`Remove ${cache.name} cache`"
    class="mx-3.5 mb-2 flex flex-col gap-2 rounded-card border border-accent-tint-border bg-accent-tint p-4"
    data-testid="orphan-confirm"
  >
    <span class="font-semibold">Remove <span class="font-mono">{{ shortPath(cache.cache) }}</span>?</span>
    <span class="text-label text-text-strong-muted">
      {{ cache.name }} isn't installed, so there is no tool command to run. scull removes the folder itself through
      its safe executor, after checking it is a recognized cache, not a link, on the expected volume and not in use.
      Not undoable.
    </span>
    <div class="flex flex-wrap gap-2">
      <button ref="confirmBtn" type="button" class="btn-ink" @click="confirm(true)">Include it</button>
      <button type="button" class="btn-outline" @click="confirm(false)">Keep it</button>
    </div>
  </div>
</template>

<style scoped>
.row {
  grid-template-columns: 40px minmax(0, 1fr) auto 84px;
  transition: background 250ms ease;
}
.progress {
  transition: width 120ms linear;
}
.mode {
  min-width: 44px;
}
.pulse {
  animation: cache-pulse 700ms ease-in-out infinite;
}
.pop {
  display: inline-block;
  animation: cache-pop 340ms ease-out;
}
@keyframes cache-pulse {
  0%,
  100% {
    opacity: 1;
  }
  50% {
    opacity: 0.5;
  }
}
@keyframes cache-pop {
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
.btn-ink,
.btn-outline {
  min-height: 44px;
  padding: 0 16px;
  border-radius: var(--radius-button);
  font: inherit;
  font-weight: 500;
  cursor: pointer;
}
.btn-ink {
  background: var(--color-ink);
  color: var(--color-on-ink);
  border: 0;
}
.btn-outline {
  background: var(--color-surface);
  color: var(--color-ink);
  border: 1px solid var(--color-border);
}
</style>
