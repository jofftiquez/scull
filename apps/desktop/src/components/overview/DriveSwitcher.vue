<script setup lang="ts">
// Overview drive switcher (P3-UI-12; not in the design files, designed to match them). A
// segmented control at the top of the wheel tile, like the Projects ecosystem filter: All
// drives, then each drive scull scans with its used share (the sidebar's figure). The line
// under it says where this drive's cleanups are quarantined (D-22: one quarantine per drive).
import type { DriveOption } from './drives';
import { formatBytes } from '../../lib/format';

defineProps<{ options: DriveOption[]; context: string }>();
const model = defineModel<string>({ required: true });

function figure(o: DriveOption): string {
  if (o.drive === null) return formatBytes(o.bytes);
  return o.used === null ? '—' : `${o.used}%`;
}

/** Read after the visible text (WCAG 2.5.3: the accessible name starts with what is shown). */
function details(o: DriveOption): string {
  const items = `, ${o.count} item${o.count === 1 ? '' : 's'}`;
  return o.drive === null ? items : `${items}, ${formatBytes(o.bytes)}`;
}
</script>

<template>
  <div class="flex w-full flex-col items-center gap-1.5" data-testid="drive-switcher">
    <div role="group" aria-label="Drive" class="flex max-w-full flex-wrap justify-center gap-1 rounded-chip bg-surface-raised p-1">
      <button
        v-for="o in options"
        :key="o.id"
        type="button"
        :aria-pressed="model === o.id"
        class="inline-flex min-h-touch cursor-pointer items-center gap-2 rounded-chip border-0 px-3.5 font-[inherit] text-label transition-colors duration-200"
        :class="model === o.id ? 'bg-ink text-on-ink' : 'bg-transparent text-text-strong-muted hover:text-ink'"
        data-testid="drive-option"
        @click="model = o.id"
      >
        <svg v-if="o.drive" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <rect x="3" y="6" width="18" height="12" rx="3" />
          <path d="M7 14h.01M11 14h6" />
        </svg>
        <span class="font-medium">{{ o.label }}<span v-if="o.kind" class="font-normal"> · {{ o.kind }}</span></span>
        <span class="font-mono text-caption tabular-nums" :class="model === o.id ? 'text-on-ink' : 'text-text-tertiary'">{{ figure(o) }}<span v-if="o.drive && o.used !== null" class="sr-only"> used</span></span>
        <span class="sr-only">{{ details(o) }}</span>
      </button>
    </div>
    <span class="text-center text-caption text-text-secondary" data-testid="drive-context">{{ context }}</span>
  </div>
</template>
