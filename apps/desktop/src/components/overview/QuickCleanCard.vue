<script setup lang="ts">
import { RouterLink } from 'vue-router';
import ActionFooter from '../shell/ActionFooter.vue';
import { computed, reactive, ref, watch } from 'vue';
import type { ScullBridge } from '../../bridge';
import { formatBytes } from '../../lib/format';
import { cacheTaskProgress } from '../caches/progress';
import { taskQueue } from '../../state/tasks';
import ActionBar from '../shell/ActionBar.vue';
import ActionConfirm from '../shell/ActionConfirm.vue';
import { quickCleanPreview, type Phase, type QuickCleanResult, type QuickCleanTool } from './quickClean';

const props = defineProps<{ tools: QuickCleanTool[]; bridge: ScullBridge; reduced: boolean }>();
const emit = defineEmits<{ state: [{ phase: Phase; cleaned: string[]; freed: number; result: QuickCleanResult | null }] }>();
const phase = ref<Phase>('idle');
const off = reactive(new Set<string>());
const result = ref<QuickCleanResult | null>(null);
const error = ref('');
const chosen = computed(() => props.tools.filter((t) => !off.has(t.id)));
const bytes = computed(() => chosen.value.reduce((n, t) => n + t.bytes, 0));
watch([phase, result], () => emit('state', { phase: phase.value, cleaned: result.value?.cleaned ?? [], freed: result.value?.measured ?? 0, result: result.value }));
async function run() {
  if (phase.value !== 'confirm' || !chosen.value.length) return;
  const tools = [...chosen.value];
  phase.value = 'cleaning'; error.value = '';
  try {
    result.value = await taskQueue.enqueue('Clean caches', (update) => quickCleanPreview(props.bridge, tools, undefined, (event) => update(cacheTaskProgress(event))), (r) => ({ summary: `${formatBytes(r.measured)} freed${r.issues?.length ? ` · ${r.issues.join('; ')}` : ''}`, partial: !!r.issues?.length }));
    phase.value = 'done';
  } catch (e) { error.value = e instanceof Error ? e.message : String(e); phase.value = 'unavailable'; }
}
function reset() { phase.value = 'idle'; result.value = null; error.value = ''; }
</script>
<template>
  <section aria-label="Quick Clean package caches" class="flex flex-col gap-3 rounded-tile-lg bg-surface p-tile-padding-sm" data-testid="quick-clean">
    <div class="flex items-baseline justify-between gap-3"><strong class="text-label">Package caches</strong><span class="font-mono text-label" data-testid="qc-total">{{ formatBytes(bytes) }}</span></div>
    <div v-if="tools.length" role="group" aria-label="Caches to clean" class="flex flex-wrap gap-2"><button v-for="t in tools" :key="t.id" type="button" :aria-pressed="!off.has(t.id)" :disabled="phase !== 'idle' && phase !== 'confirm'" class="min-h-touch rounded-chip border px-3 text-label" :class="off.has(t.id) ? 'border-border bg-surface text-ink' : 'border-ink bg-ink text-on-ink'" :data-chip="off.has(t.id) ? 'off' : 'on'" @click="off.has(t.id) ? off.delete(t.id) : off.add(t.id)">{{ t.label }} · {{ formatBytes(t.bytes) }}</button></div>
    <p v-else class="m-0 text-label text-text-secondary">No package caches found.</p>
    <ActionFooter aria-label="Overview actions"><ActionBar :summary="result ? `${formatBytes(result.measured)} freed` : `${chosen.length} selected`" :label="phase === 'done' || phase === 'unavailable' ? 'Reset selection' : phase === 'cleaning' ? 'Queued / cleaning…' : 'Clean caches'" :busy="phase === 'cleaning'" :disabled="!chosen.length" :error="error" data-testid="qc-button" @action="phase === 'done' || phase === 'unavailable' ? reset() : phase = 'confirm'"><RouterLink to="/cleanup" class="inline-flex min-h-touch items-center rounded-button border border-border px-4 text-label text-ink no-underline">Review cleanup</RouterLink></ActionBar></ActionFooter>
    <ActionConfirm v-if="phase === 'confirm'" title="Clean selected caches?" label="Clean caches" @cancel="phase = 'idle'" @confirm="run"><ul class="m-0 pl-5"><li v-for="t in chosen" :key="t.id">{{ t.label }} · {{ t.candidate.path }}</li></ul><p>Not undoable. Packages download again on the next install.</p></ActionConfirm>
  </section>
</template>
