<script setup lang="ts">
import TaskToolbar from './TaskToolbar.vue';
import { computed } from 'vue';
import { useRoute } from 'vue-router';
import type { NavItem } from '../../navigation';
import { useBridge } from '../../bridge';
import { cancelScan, core, rescan } from '../../state/core';
const props = defineProps<{ busy?: boolean }>();
const scanning = computed(() => core.scanning || !!props.busy);
const route = useRoute();
const bridge = useBridge();
</script>

<template>
  <header class="main-toolbar" data-testid="header">
    <span class="font-semibold">{{ (route.meta.item as NavItem | undefined)?.label ?? 'scull' }}</span>
    <div class="toolbar-actions">
      <span v-if="core.error" role="alert" class="text-label text-accent-text">{{ core.error }}</span>
      <slot name="actions" />
      <TaskToolbar />
      <button v-if="core.scanning" type="button" class="min-h-touch rounded-button border border-border bg-surface px-4 text-ink" @click="cancelScan">Cancel scan</button>
      <button type="button" :disabled="scanning" :aria-busy="scanning" :class="{ 'is-scanning': scanning }" class="scan-button inline-flex min-h-12 min-w-32 cursor-pointer items-center justify-center gap-2 rounded-button border-0 bg-accent px-6 font-semibold text-on-fill disabled:cursor-progress" data-testid="rescan" @click="rescan(bridge)">
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"><path d="M3 12a9 9 0 0 1 15.36-6.36L21 8M21 3v5h-5M21 12a9 9 0 0 1-15.36 6.36L3 16M8 16H3v5" /></svg>
        <span aria-live="polite">{{ scanning ? 'Scanning…' : 'Scan' }}</span>
      </button>
    </div>
  </header>
</template>

<style scoped>
.main-toolbar { display: flex; flex-shrink: 0; align-items: center; justify-content: space-between; gap: 12px; min-height: 72px; padding: 12px 24px; border-bottom: 1px solid var(--color-line); background: var(--color-surface); }
.toolbar-actions { display: flex; flex-wrap: wrap; align-items: center; justify-content: flex-end; gap: 8px; }
.scan-button { position: relative; isolation: isolate; overflow: hidden; background: color-mix(in srgb, var(--color-accent) 85%, var(--color-on-drive-tile)); }
.scan-button > * { position: relative; z-index: 1; }
.scan-button.is-scanning { background: color-mix(in srgb, var(--color-accent) 75%, var(--color-on-drive-tile)); }
.scan-button.is-scanning svg { animation: scull-spin 800ms linear infinite; }
.scan-button.is-scanning::before { content: ''; position: absolute; inset: 0; background: linear-gradient(100deg, transparent 25%, color-mix(in srgb, var(--color-on-fill) 30%, transparent) 50%, transparent 75%); animation: scan-sweep 1.6s ease-in-out infinite; }
@keyframes scan-sweep { from { transform: translateX(-100%); } to { transform: translateX(100%); } }
@media (prefers-reduced-motion: reduce) { .scan-button.is-scanning svg, .scan-button.is-scanning::before { animation: none; } .scan-button.is-scanning::before { display: none; } }
</style>
