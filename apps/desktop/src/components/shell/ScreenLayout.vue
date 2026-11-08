<script setup lang="ts">
import { nextTick, provide, ref, watch } from 'vue';
import AppHeader from './AppHeader.vue';
import LoadingState from './LoadingState.vue';
import { FOOTER_TARGETS } from './footer';

const props = withDefaults(defineProps<{ asideLabel?: string; detailOpen?: boolean; scanBusy?: boolean; loading?: boolean; loadingLabel?: string }>(), { asideLabel: 'Details', detailOpen: true, scanBusy: false, loading: false, loadingLabel: 'Loading…' });
const emit = defineEmits<{ 'close-details': [] }>();
let returnFocus: HTMLElement | null = null;
watch(() => props.detailOpen, (open) => { if (open) returnFocus = document.activeElement as HTMLElement | null; });
async function closeDetails() {
  emit('close-details');
  await nextTick();
  if (returnFocus?.isConnected) returnFocus.focus();
}
const footer = ref<HTMLElement | null>(null);
provide(FOOTER_TARGETS, footer);
</script>

<template>
  <section class="screen-layout" aria-label="Dashboard">
    <AppHeader :busy="scanBusy"><template #actions><slot name="actions" /></template></AppHeader>
    <div class="screen-panels" :class="{ 'has-aside': $slots.aside && props.detailOpen !== false }">
      <div class="main-panel">
        <main id="content" tabindex="-1" class="main-scroll" :aria-busy="loading" data-testid="main-scroll"><LoadingState v-if="loading" :label="loadingLabel" /><slot v-else /></main>
      </div>
      <aside v-if="$slots.aside && props.detailOpen !== false" :aria-label="asideLabel ?? 'Details'" class="aside-panel">
        <div v-if="props.detailOpen" class="flex shrink-0 justify-end border-b border-line px-3 py-1">
          <button type="button" aria-label="Close details" class="min-h-touch min-w-touch rounded-button border-0 bg-transparent text-ink" @click="closeDetails">✕</button>
        </div>
        <div class="aside-scroll" tabindex="0" data-testid="aside-scroll"><slot name="aside" /></div>
      </aside>
    </div>
    <div ref="footer" class="panel-footer" data-testid="main-footer"><slot name="footer" /><slot v-if="props.detailOpen !== false" name="aside-footer" /></div>
  </section>
</template>

<style scoped>
.screen-layout { display: flex; flex: 1; flex-direction: column; min-width: 0; min-height: 0; overflow: hidden; }
.screen-panels { display: grid; grid-template-columns: minmax(0, 1fr); flex: 1; min-height: 0; }
.screen-panels.has-aside { grid-template-columns: minmax(0, 1fr) 340px; }
.main-panel, .aside-panel { display: flex; flex-direction: column; min-width: 0; min-height: 0; overflow: hidden; }
.aside-panel { border-left: 1px solid var(--color-line); }
.main-scroll, .aside-scroll { display: flex; flex: 1; flex-direction: column; gap: var(--spacing-page-gap); min-width: 0; min-height: 0; padding: var(--spacing-page-padding); overflow: auto; overscroll-behavior: contain; scrollbar-gutter: stable; }
.main-scroll { outline: none; }
.aside-scroll:focus-visible { outline: 2px solid var(--color-accent); outline-offset: -2px; }
.main-scroll > *, .aside-scroll > * { flex-shrink: 0; }
.panel-footer { flex-shrink: 0; min-width: 0; max-height: 50dvh; overflow: auto; overscroll-behavior: contain; background: var(--color-surface); }
.panel-footer:empty { display: none; }
@media (max-width: 1150px) { .screen-panels.has-aside { grid-template-columns: minmax(0, 1fr) 280px; } }
@media (max-width: 760px) { .screen-panels.has-aside { grid-template-columns: minmax(0, 1fr) 260px; } }
</style>
