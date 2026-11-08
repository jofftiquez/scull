<script setup lang="ts">
import { nextTick, onBeforeUnmount, onMounted, ref } from 'vue';
const props = withDefaults(defineProps<{ title: string; label: string; count?: number; tone?: 'clean' | 'remove' }>(), { count: 0, tone: 'clean' });
const emit = defineEmits<{ confirm: []; cancel: [] }>();
const dialog = ref<HTMLDialogElement>();
const typed = ref('');
onMounted(() => { dialog.value?.showModal?.(); });
onBeforeUnmount(() => {
  dialog.value?.close?.();
  void nextTick(() => { if (document.activeElement === document.body) document.querySelector<HTMLElement>('[data-testid="toolbar-tasks"] > summary')?.focus(); });
});
</script>
<template>
  <dialog ref="dialog" class="confirmation" aria-labelledby="action-confirm-title" @cancel.prevent="emit('cancel')" @keydown.esc.stop>
    <h2 id="action-confirm-title" class="m-0 text-h3 font-semibold">{{ title }}</h2>
    <div class="confirmation-body text-label"><slot /></div>
    <label v-if="count" class="flex flex-col gap-2 text-label">Type {{ count }} to confirm {{ count }} item{{ count === 1 ? '' : 's' }} that scull cannot restore<input v-model="typed" aria-label="Confirm irreversible item count" inputmode="numeric" autocomplete="off" class="min-h-touch rounded-button border border-border bg-surface px-3 font-mono text-ink" /></label>
    <div class="flex justify-end gap-2"><button type="button" class="min-h-12 rounded-button border border-border bg-surface px-5 text-ink" autofocus @click="emit('cancel')">Cancel</button><button type="button" class="min-h-12 rounded-button border-0 bg-ink px-5 font-semibold text-on-ink disabled:opacity-45" :disabled="props.count > 0 && typed !== String(props.count)" data-testid="confirm-action" @click="emit('confirm')">{{ label }}</button></div>
  </dialog>
</template>
<style scoped>
.confirmation { position:fixed; inset:0; margin:auto; height:fit-content; width:min(520px, calc(100vw - 40px)); max-height:80dvh; overflow:auto; padding:24px; border:1px solid var(--color-line); border-radius:20px; background:var(--color-surface); color:var(--color-ink);  }
.confirmation[open] { display:flex; flex-direction:column; gap:20px; }
.confirmation::backdrop { background:color-mix(in srgb, var(--color-text-secondary) 40%, transparent); backdrop-filter:blur(3px); }
.confirmation-body { overflow-wrap:anywhere; }
</style>
