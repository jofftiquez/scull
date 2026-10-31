<script setup lang="ts">
// Overview (design 01). The shell's layout with empty tiles: the storage wheel lands in
// P3-UI-01 and the right sidebar in P3-UI-02.
import ScreenLayout from '../../components/shell/ScreenLayout.vue';
import PlaceholderTile from '../../components/PlaceholderTile.vue';
import { useBridge } from '../../bridge';
import { cancelScan, core, rescan } from '../../state/core';

const bridge = useBridge();
</script>

<template>
  <ScreenLayout aside-label="Details">
    <template #actions>
      <button
        v-if="core.scanning"
        type="button"
        class="inline-flex min-h-10 cursor-pointer items-center rounded-button border-0 bg-transparent px-3.5 text-label text-text-strong-muted hover:text-ink"
        @click="cancelScan"
      >
        Cancel
      </button>
      <button
        type="button"
        :disabled="core.scanning"
        class="inline-flex min-h-10 cursor-pointer items-center gap-2 rounded-button border border-border bg-surface px-3.5 text-label font-medium text-ink disabled:cursor-progress"
        data-testid="rescan"
        @click="rescan(bridge)"
      >
        <svg
          width="15"
          height="15"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
          stroke-linecap="round"
          aria-hidden="true"
          :style="core.scanning ? 'animation: scull-spin 900ms linear infinite' : undefined"
        >
          <path d="M20 11a8 8 0 1 0-2.3 5.7" />
          <path d="M20 5v6h-6" />
        </svg>
        {{ core.scanning ? 'Scanning' : 'Rescan' }}
      </button>
    </template>

    <PlaceholderTile
      title="Storage wheel"
      issue="P3-UI-01"
      note="Categories on the inner ring, items colored and patterned by safety tier on the outer ring."
    />

    <template #aside>
      <section aria-label="Safe to reclaim" class="flex min-h-[276px] flex-col gap-3 rounded-tile-lg bg-surface p-tile-padding">
        <span class="text-label text-text-secondary">Safe to reclaim</span>
        <span class="text-stat-md font-bold text-text-hint">—</span>
        <span class="text-caption text-text-tertiary">Appears after the first scan.</span>
      </section>
      <section aria-label="Selection" class="flex min-h-[200px] flex-col gap-3 rounded-tile-lg bg-surface p-tile-padding-sm">
        <span class="text-label text-text-secondary">Selected</span>
        <span class="text-caption text-text-tertiary">Nothing selected.</span>
      </section>
      <section aria-label="By safety tier" class="flex flex-1 flex-col gap-3 rounded-tile-lg bg-surface p-tile-padding-sm">
        <span class="text-label text-text-secondary">By safety tier</span>
        <div v-for="t in tiers" :key="t.key" class="flex items-center gap-3">
          <span
            class="rounded-chip px-2.5 py-0.5 text-caption font-semibold"
            :style="{ background: t.fill, color: `var(--color-tier-${t.key}-text)` }"
          >{{ t.label }}</span>
          <span class="text-label text-text-secondary">{{ t.hint }}</span>
        </div>
      </section>
    </template>
  </ScreenLayout>
</template>

<script lang="ts">
// Tier chips carry a text label, so the tier never depends on color alone. As in the design,
// the uncertain chip keeps its stripes; the protected chip is solid plum so its white label
// stays legible (the hatch is for bars and areas).
const tiers = [
  { key: 's0', label: 'regenerable', hint: 'Rebuilds itself', fill: 'var(--tier-s0-fill)' },
  { key: 's1', label: 'costly', hint: 'Re-download or long build', fill: 'var(--tier-s1-fill)' },
  { key: 's2', label: 'uncertain', hint: 'Look before it goes', fill: 'var(--tier-s2-fill)' },
  { key: 's3', label: 'protected', hint: 'Never touched', fill: 'var(--color-tier-s3)' },
] as const;
</script>
