<script setup lang="ts">
// "Found on this Mac" (design 02, ONB-01): what the metadata-only probe saw, plus the disk.
import { computed } from 'vue';
import type { Drive } from '../../bridge';
import { formatBytes } from '../../lib/format';
import { aiClientLabel, toolchainDetail, type Detection } from './model';

const props = defineProps<{ detection: Detection | null; drive: Drive | null; noun: string }>();

const rows = computed(() => {
  const d = props.detection;
  if (!d) return [];
  const out = d.toolchains.map((t) => ({ k: t.label, v: toolchainDetail(t) }));
  if (d.ai_clients.length > 0) {
    out.push({ k: 'AI clients', v: d.ai_clients.map((c) => aiClientLabel(c).toLowerCase()).join(' · ') });
  }
  const dr = props.drive;
  if (dr) {
    out.push({
      k: 'Disk',
      v: `${formatBytes(dr.total_bytes)} · ${dr.free_bytes == null ? 'free space unknown' : `${formatBytes(dr.free_bytes)} free`}`,
    });
  }
  return out;
});
</script>

<template>
  <section :aria-label="`Found on this ${noun}`" class="flex flex-col gap-3 rounded-tile bg-surface p-tile-padding" data-testid="found">
    <span class="text-label font-medium text-text-secondary">Found on this {{ noun }}</span>
    <div v-if="detection === null" class="py-2.5 text-label text-text-secondary">Looking around…</div>
    <dl v-else class="m-0 flex flex-col">
      <div
        v-for="(r, i) in rows"
        :key="r.k"
        class="flex justify-between gap-3 border-t border-line py-2.5"
        :class="i === rows.length - 1 ? 'border-b' : ''"
      >
        <dt class="font-medium">{{ r.k }}</dt>
        <dd class="m-0 text-right font-mono text-caption text-text-secondary">{{ r.v }}</dd>
      </div>
      <div v-if="rows.length === 0" class="border-y border-line py-2.5 text-label text-text-secondary">
        No toolchains found. You can still pick what you build.
      </div>
    </dl>
    <span class="text-caption text-text-secondary">Metadata only. scull didn't open any of your files.</span>
  </section>
</template>
