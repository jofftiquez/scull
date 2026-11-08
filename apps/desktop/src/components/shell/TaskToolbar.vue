<script setup lang="ts">
import TaskOutput from './TaskOutput.vue';
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';
import { RouterLink } from 'vue-router';
import { taskQueue, type ActionTask } from '../../state/tasks';
const state = taskQueue.state;
const now = ref(Date.now());
let clock: ReturnType<typeof setInterval> | undefined;
onMounted(() => { clock = setInterval(() => { now.value = Date.now(); }, 1000); });
onBeforeUnmount(() => clearInterval(clock));
const elapsed = (task: ActionTask) => `${Math.max(0, Math.floor((now.value - (task.startedAt ?? now.value)) / 1000))}s`;
const active = computed(() => state.tasks.filter((t) => t.status === 'running' || t.status === 'pending'));
const running = computed(() => active.value.some((t) => t.status === 'running') || state.externalBusy);
const attention = computed(() => state.tasks.filter((t) => t.status === 'failed' || t.status === 'partial'));
const groups = computed(() => [
  { title: 'In progress', items: [...active.value].sort((a) => a.status === 'running' ? -1 : 1) },
  { title: 'Recent activity', items: state.tasks.filter((t) => !active.value.includes(t)).slice().reverse() },
].filter((g) => g.items.length));
const labels: Record<ActionTask['status'], string> = { pending: 'Queued', running: 'Running', succeeded: 'Completed', partial: 'Partly completed', failed: 'Failed', cancelled: 'Cancelled' };
const current = computed(() => active.value.find((t) => t.status === 'running'));
const title = computed(() => current.value?.total ? `${current.value.completed ?? 0} / ${current.value.total} tools` : active.value.length ? `${active.value.length} task${active.value.length === 1 ? '' : 's'}` : state.externalBusy ? 'Cleanup running' : attention.value.length ? `${attention.value.length} need review` : 'Tasks');
</script>
<template>
  <details v-if="state.tasks.length || state.externalBusy || state.admissionError" class="task-menu" data-testid="toolbar-tasks">
    <summary class="task-toggle" :class="{ 'is-active': active.length || state.externalBusy, 'has-errors': !active.length && attention.length }">
      <span v-if="active.length || state.externalBusy" class="task-activity" :class="{ 'is-running': running }" aria-hidden="true"><i /><i /><i /></span>
      <span v-else-if="attention.length" class="attention-mark" aria-hidden="true">!</span>
      <span aria-live="polite">{{ title }}</span>
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path d="m6 9 6 6 6-6" /></svg>
    </summary>
    <div class="task-popover">
      <header class="flex items-center justify-between gap-3"><strong>Tasks</strong><RouterLink to="/history" class="inline-flex min-h-touch items-center text-label text-ink">Open History</RouterLink></header>
      <p v-if="state.externalBusy && !active.some((t) => t.status === 'running')" class="task-notice">Another cleanup is running. Queued tasks start when it finishes.</p>
      <p v-if="state.admissionError" role="alert" class="task-notice">Waiting to check the cleanup lock: {{ state.admissionError }}</p>
      <section v-for="group in groups" :key="group.title" :aria-label="group.title">
        <h2 class="group-title">{{ group.title }}</h2>
        <ol class="m-0 list-none p-0">
          <li v-for="task in group.items" :key="task.id" class="task-row" :data-status="task.status">
            <div class="flex items-start justify-between gap-3"><strong class="text-label">{{ task.label }}</strong><span class="status-chip" :data-status="task.status">{{ labels[task.status] }}</span></div>
            <p v-if="task.target" class="m-0 text-label text-text-secondary [overflow-wrap:anywhere]">{{ task.target }}</p>
            <div v-if="task.total != null" class="count-progress text-label"><strong>{{ task.completed ?? 0 }} / {{ task.total }} tools finished</strong><span v-if="task.status === 'running'" class="text-text-secondary">{{ elapsed(task) }}</span></div>
            <p v-if="task.current && task.status === 'running'" class="m-0 text-label text-text-secondary">{{ task.current }}</p>
            <div v-if="task.status === 'running'" class="task-progress" role="progressbar" :aria-label="`${task.label} progress`" :aria-valuenow="task.progress == null ? undefined : Math.round(task.progress * 100)" aria-valuemin="0" aria-valuemax="100"><span :class="{ indeterminate: task.progress == null }" :style="task.progress == null ? undefined : { width: `${task.progress * 100}%` }" /></div>
            <p v-if="task.status === 'pending'" class="m-0 text-label text-text-secondary">Waiting for the current action to finish.</p>
            <p v-else-if="task.summary && task.status !== 'failed'" class="m-0 text-label">{{ task.summary }}</p>
            <details v-if="task.total != null || task.logs?.length" class="result-details" :open="task.status === 'running'">
              <summary class="min-h-touch text-label font-medium">Command output</summary>
              <TaskOutput :logs="task.logs ?? []" :truncated="task.logsTruncated" :busy="task.status === 'running'" />
            </details>
            <details v-if="task.details?.length || task.status === 'failed'" class="result-details" :open="task.status === 'failed' || task.status === 'partial'">
              <summary class="min-h-touch text-label font-medium">{{ task.status === 'failed' ? 'Why it failed' : task.status === 'partial' ? 'Results and failure reasons' : 'View results' }}</summary>
              <ul class="m-0 list-none p-0"><li v-for="(item, i) in task.details ?? [{ label: 'Error', status: 'failed', reason: task.summary, path: undefined }]" :key="i" class="result-item"><div class="flex items-center justify-between gap-3"><strong class="text-label">{{ item.label }}</strong><span class="status-chip" :data-status="item.status">{{ item.status === 'completed' ? 'Completed' : item.status === 'skipped' ? 'Skipped' : 'Failed' }}</span></div><span v-if="item.path" class="font-mono text-caption text-text-secondary [overflow-wrap:anywhere]">{{ item.path }}</span><p v-if="item.reason" class="result-reason">{{ item.reason }}</p></li></ul>
            </details>
            <button v-if="task.status === 'pending'" type="button" class="cancel-task" @click="taskQueue.cancel(task.id)">Cancel queued task</button>
          </li>
        </ol>
      </section>
    </div>
  </details>
</template>
<style scoped>
.task-menu { position:relative; }
.task-toggle { display:flex; align-items:center; gap:8px; min-height:44px; padding:0 12px; border:1px solid var(--color-border); border-radius:var(--radius-button); cursor:pointer; list-style:none; font-size:13px; }
.task-toggle::-webkit-details-marker { display:none; }
.task-toggle.is-active { border-color:var(--color-accent-tint-border); background:var(--color-accent-tint); color:var(--color-accent-text); }
.task-toggle.has-errors { border-color:var(--color-accent-tint-border); }
.attention-mark { display:grid; place-items:center; width:18px; height:18px; border-radius:50%; background:var(--color-accent-tint); color:var(--color-accent-text); font-weight:700; }
.task-popover { position:absolute; top:calc(100% + 12px); right:0; z-index:60; width:min(480px, 85vw); max-height:70dvh; overflow:auto; padding:8px 20px 20px; border:1px solid var(--color-line); border-radius:16px; background:var(--color-surface); color:var(--color-ink); }
.group-title { margin:12px 0 0; padding:12px 0; border-bottom:1px solid var(--color-line); font-size:13px; color:var(--color-text-secondary); font-weight:500; }
.count-progress { display:flex; justify-content:space-between; gap:12px; }
.task-row { display:flex; flex-direction:column; gap:8px; padding:16px 0; border-bottom:1px solid var(--color-line); }
.status-chip { display:inline-flex; flex-shrink:0; align-items:center; padding:4px 8px; border-radius:var(--radius-chip); font-size:11px; font-weight:600; background:var(--color-surface-raised); color:var(--color-text-secondary); }
.status-chip[data-status=failed] { background:var(--color-accent-tint); color:var(--color-accent-text); }
.status-chip[data-status=partial], .status-chip[data-status=skipped], .status-chip[data-status=pending] { background:var(--color-amber-tint); color:var(--color-amber-text); }
.status-chip[data-status=running] { background:var(--color-accent-tint); color:var(--color-accent-text); }
.status-chip[data-status=succeeded], .status-chip[data-status=completed] { background:var(--color-violet-tint); color:var(--color-violet-text); }
.task-notice { margin:8px 0; padding:12px; border-radius:12px; background:var(--color-amber-tint); color:var(--color-amber-text); font-size:13px; }
.result-item { display:flex; flex-direction:column; gap:6px; padding:10px 12px; margin-top:6px; border-radius:10px; background:var(--color-surface-subtle); }
.result-reason { margin:0; white-space:pre-wrap; overflow-wrap:anywhere; font-size:13px; line-height:1.5; }
.result-details > summary { display:flex; align-items:center; cursor:pointer; }
.result-details > summary::before { content:'›'; margin-right:8px; }
.result-details[open] > summary::before { rotate:90deg; }
.cancel-task { min-height:44px; padding:0 12px; border:1px solid var(--color-border); border-radius:var(--radius-button); background:var(--color-surface); color:var(--color-ink); cursor:pointer; }
.task-activity { display:flex; align-items:center; gap:3px; height:20px; }
.task-activity i { display:block; width:4px; height:8px; border-radius:3px; background:currentColor; animation:queued-beat 1200ms ease-in-out infinite; }
.task-activity i:nth-child(2) { animation-delay:150ms; }
.task-activity i:nth-child(3) { animation-delay:300ms; }
.task-activity.is-running i { animation-name:running-beat; animation-duration:800ms; }
.task-progress { height:5px; overflow:hidden; border-radius:4px; background:var(--color-accent-tint); }
.task-progress span { display:block; height:100%; background:var(--color-accent); transition:width 180ms ease-out; }
.task-progress .indeterminate { width:35%; animation:task-travel 1400ms ease-in-out infinite; }
@keyframes queued-beat { 0%,100% { opacity:.35; transform:scaleY(.65); } 50% { opacity:1; transform:scaleY(1.2); } }
@keyframes running-beat { 0%,100% { transform:scaleY(.5); } 50% { transform:scaleY(1.8); } }
@keyframes task-travel { from { transform:translateX(-105%); } to { transform:translateX(390%); } }
@media(prefers-reduced-motion:reduce) { .task-activity i, .task-progress .indeterminate { animation:none; } .task-progress span { transition:none; } }
</style>
