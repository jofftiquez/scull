<script setup lang="ts">
import { onBeforeUnmount, ref, watch } from 'vue';
import { shell } from '../../state/shell';
import { taskQueue, type ActionTask } from '../../state/tasks';
import SuccessMark from '../review/SuccessMark.vue';
import { useReducedMotion } from '../overview/motion';
import '../review/review.css';
const reduced = useReducedMotion();
const current = ref<ActionTask | null>(null);
let timer: ReturnType<typeof setTimeout> | undefined;
function next() {
  clearTimeout(timer);
  current.value = taskQueue.state.tasks.find((t) => t.status === 'succeeded' && !t.celebrated) ?? null;
  if (current.value) { current.value.celebrated = true; shell.wink += 1; timer = setTimeout(next, 4500); }
}
watch(() => taskQueue.state.tasks.filter((t) => t.status === 'succeeded').length, () => { if (!current.value) next(); }, { immediate:true });
onBeforeUnmount(() => clearTimeout(timer));
</script>
<template>
  <section v-if="current" :key="current.id" role="status" aria-live="polite" class="celebration" data-testid="task-success"><SuccessMark :size="88" burst :animate="!reduced" /><div><strong class="text-h2">{{ current.label }} complete!</strong><p v-if="current.target" class="m-0 mt-1 text-label">{{ current.target }}</p><p class="m-0 mt-1 text-label">{{ current.summary }}</p></div><button type="button" aria-label="Dismiss success" class="min-h-touch min-w-touch rounded-button border-0 bg-transparent text-ink" @click="next">✕</button></section>
</template>
<style scoped>
.celebration { position:fixed; z-index:90; bottom:28px; left:50%; transform:translateX(-50%); display:flex; align-items:center; gap:20px; width:max-content; max-width:calc(100vw - 40px); padding:24px; border:1px solid var(--color-violet-light); border-radius:24px; background:var(--color-violet-tint); color:var(--color-ink);  animation:arrival 650ms cubic-bezier(.16,1.5,.4,1); }
@keyframes arrival { from { opacity:0; translate:0 80px; scale:.65; } to { opacity:1; translate:0; scale:1; } }
@media(prefers-reduced-motion:reduce) { .celebration { animation:none; } }
</style>
