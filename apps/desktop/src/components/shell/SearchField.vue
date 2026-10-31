<script setup lang="ts">
// Sidebar search (design 01). ⌘K / Ctrl+K focuses it from anywhere. Results arrive with the
// screens that own the data; the field itself is the shell's.
import { onBeforeUnmount, onMounted, ref } from 'vue';

const model = defineModel<string>({ default: '' });
const input = ref<HTMLInputElement | null>(null);
const isMac = typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);
const shortcut = isMac ? '⌘K' : 'Ctrl K';

function onKey(e: KeyboardEvent) {
  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
    e.preventDefault();
    input.value?.focus();
  }
}
onMounted(() => window.addEventListener('keydown', onKey));
onBeforeUnmount(() => window.removeEventListener('keydown', onKey));
</script>

<template>
  <label
    class="flex min-h-touch cursor-text items-center gap-2.5 rounded-button-lg border border-line bg-surface-raised px-3 text-text-tertiary"
  >
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true">
      <circle cx="11" cy="11" r="6" />
      <path d="M20 20l-4.5-4.5" />
    </svg>
    <span class="sr-only">Search</span>
    <input
      ref="input"
      v-model="model"
      type="search"
      placeholder="Search paths, projects…"
      class="min-w-0 flex-1 border-0 bg-transparent text-label text-ink outline-none"
    />
    <kbd class="rounded-[6px] border border-border px-1.5 py-0.5 font-mono text-[11px] text-text-tertiary">{{ shortcut }}</kbd>
  </label>
</template>
