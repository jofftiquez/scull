<script setup lang="ts">
// Live drives widget (design 01): one bar per scanned drive, then the drives scull does not
// scan (backup, network) as a single line each. Unknown sizes read "size unknown", never 0.
import { computed, onMounted, ref } from 'vue';
import { useBridge, type Drive } from '../../bridge';
import { formatBytes, usedPercent } from '../../lib/format';

const bridge = useBridge();
const drives = ref<Drive[] | null>(null);
const failed = ref(false);

onMounted(async () => {
  try {
    drives.value = await bridge.drives();
  } catch {
    failed.value = true;
  }
});

const scanned = computed(() => (drives.value ?? []).filter((d) => d.scan_policy === 'scan'));
const notScanned = computed(() => (drives.value ?? []).filter((d) => d.scan_policy !== 'scan'));

function why(d: Drive): string {
  return d.scan_policy === 'never' ? 'not scanned' : 'not scanned unless you opt in';
}
</script>

<template>
  <section aria-label="Drives" class="flex flex-col gap-3 rounded-[18px] bg-surface-raised p-3.5" data-testid="drives">
    <div class="flex items-center justify-between">
      <span class="text-caption font-semibold text-text-secondary">Drives</span>
      <RouterLink to="/settings" class="text-caption text-text-secondary">Manage</RouterLink>
    </div>

    <span v-if="failed" class="text-caption text-text-secondary">Drive list unavailable.</span>
    <span v-else-if="drives === null" class="text-caption text-text-secondary" aria-busy="true">Reading drives…</span>
    <template v-else>
      <div v-for="d in scanned" :key="d.mount_point" class="flex flex-col gap-1.5" data-testid="drive">
        <div class="flex items-baseline justify-between gap-2">
          <span class="truncate text-label font-semibold text-ink" :title="d.mount_point">
            {{ d.label }}<span v-if="d.kind === 'removable'" class="font-normal text-text-secondary"> · external</span>
          </span>
          <span class="text-h3 font-bold text-ink tabular-nums">{{
            usedPercent(d.total_bytes, d.free_bytes) === null ? '—' : `${usedPercent(d.total_bytes, d.free_bytes)}%`
          }}</span>
        </div>
        <div
          class="flex h-1.5 overflow-hidden rounded-chip bg-ground"
          role="meter"
          :aria-label="`${d.label} used`"
          aria-valuemin="0"
          aria-valuemax="100"
          :aria-valuenow="usedPercent(d.total_bytes, d.free_bytes) ?? undefined"
        >
          <div
            class="h-full rounded-chip"
            :class="d.holds_home ? 'bar-system' : 'bar-other'"
            :style="{ width: `${usedPercent(d.total_bytes, d.free_bytes) ?? 0}%` }"
          ></div>
        </div>
        <span class="text-caption text-text-strong-muted">{{
          d.free_bytes == null ? 'size unknown' : `${formatBytes(d.free_bytes)} free`
        }}</span>
      </div>
      <span v-for="d in notScanned" :key="d.mount_point" class="text-caption text-text-secondary" data-testid="drive-not-scanned">
        {{ d.label }} · {{ why(d) }}
      </span>
      <span v-if="drives.length === 0" class="text-caption text-text-secondary">No drives found.</span>
    </template>
  </section>
</template>

<style scoped>
/* Drive bars from design/html/Main.html: the system drive in striped violet, others in a
   muted lilac. The pattern keeps the two apart without relying on color alone. */
.bar-system {
  background: repeating-linear-gradient(135deg, var(--color-tier-s0) 0 6px, #9c78ff 6px 12px);
  transition: width 200ms linear;
}
.bar-other {
  background: #8e7fb0;
  transition: width 200ms linear;
}
</style>
