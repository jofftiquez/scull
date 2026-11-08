<script setup lang="ts">
import { nextTick, onMounted, ref, watch } from 'vue';
import type { TaskLog } from '../../state/tasks';
const props = defineProps<{ logs: TaskLog[]; truncated?: boolean; busy?: boolean }>();
const output = ref<HTMLElement>();
let following = true;
function onScroll() {
  const el = output.value;
  if (el) following = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
}
async function follow() {
  if (!following) return;
  await nextTick();
  if (output.value) output.value.scrollTop = output.value.scrollHeight;
}
onMounted(follow);
watch(() => props.logs.at(-1), follow);
</script>
<template>
  <div ref="output" class="command-output" tabindex="0" role="region" aria-label="Command output" data-testid="task-output" @scroll="onScroll">
    <p v-if="truncated" class="log-note">Earlier output omitted to keep this view responsive.</p>
    <p v-if="!logs.length" class="log-note">{{ busy ? 'Waiting for command output…' : 'This command produced no output.' }}</p>
    <div v-for="(line, i) in logs" :key="i" class="output-line" :data-stream="line.stream"><span v-if="line.tool" class="tool-label">{{ line.tool }}</span><span class="output-text">{{ line.stream === 'command' ? '$ ' : '' }}{{ line.text }}</span></div>
  </div>
</template>
<style scoped>
.command-output { max-height:240px; overflow:auto; padding:12px; border:1px solid var(--color-line); border-radius:12px; background:var(--color-surface-subtle); font-family:var(--font-mono); font-size:11px; line-height:1.7; overscroll-behavior:contain; }
.output-line { display:flex; align-items:baseline; gap:8px; }
.tool-label { flex:0 0 52px; color:var(--color-text-secondary); font-size:10px; }
.output-text { min-width:0; white-space:pre-wrap; overflow-wrap:anywhere; }
[data-stream=stderr] .output-text { color:var(--color-accent-text); }
[data-stream=command] .output-text { color:var(--color-violet-text); font-weight:500; }
[data-stream=system] .output-text, .log-note { color:var(--color-text-secondary); }
.log-note { margin:0; }
</style>
