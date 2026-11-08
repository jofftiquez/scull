<script setup lang="ts">
import ActionBar from '../shell/ActionBar.vue';
import ActionFooter from '../shell/ActionFooter.vue';
// The Explorer's right column (design 07 / 07b): the folder shown, or the selected tile, with
// its size, tier split, largest items, a one-line note and the main action.
import { computed } from 'vue';
import type { Tier } from '../../bridge';
import { formatBytes } from '../../lib/format';
import { TIER_KEYS, TIERS } from '../overview/tiers';
import { ACTION_LABEL, tierShares, type DetailAction, type LargestRow, type TierBytes } from './model';

const props = defineProps<{
  kind: string;
  name: string;
  path: string;
  bytes: number;
  tiers: TierBytes;
  largest: LargestRow[];
  note: string;
  action: DetailAction;
  revealLabel: string;
  /** Hide "Show in …" (the machine and groups have no single folder). */
  canReveal: boolean;
  /** Shown under the actions when "Why protected?" is open. */
  reasons: string[] | null;
  message: string | null;
}>();
defineEmits<{ action: []; reveal: [] }>();

const shares = computed(() => tierShares(props.tiers));
const legendLabel = (t: Tier) => `${TIERS[t].label} ${formatBytes(props.tiers[t])}`;
</script>

<template>
  <section aria-live="polite" class="flex flex-col gap-3.5 rounded-tile bg-surface p-tile-padding" data-testid="detail">
    <span class="text-label font-medium text-text-tertiary" data-testid="detail-kind">{{ kind }}</span>
    <div class="flex flex-col gap-0.5">
      <h2 class="m-0 text-[22px] font-semibold" data-testid="detail-name">{{ name }}</h2>
      <span class="font-mono text-caption text-text-tertiary [overflow-wrap:anywhere]" data-testid="detail-path">{{ path }}</span>
    </div>
    <span class="text-stat-md leading-none font-bold tracking-[-0.03em] tabular-nums" data-testid="detail-size">{{ formatBytes(bytes) }}</span>

    <div class="flex flex-col gap-2">
      <div
        class="flex h-3 gap-0.5 overflow-hidden rounded-chip bg-surface-raised"
        role="img"
        :aria-label="TIER_KEYS.filter((t) => tiers[t] > 0).map((t) => `${TIERS[t].label} ${formatBytes(tiers[t])}`).join(', ') || 'Empty'"
      >
        <span v-for="t in TIER_KEYS" :key="t" class="seg h-full" :class="`seg-${t.toLowerCase()}`" :style="{ width: `${shares[t].toFixed(1)}%` }"></span>
      </div>
      <div class="grid grid-cols-2 gap-x-3 gap-y-1 text-caption" data-testid="legend">
        <span v-for="t in TIER_KEYS" :key="t" class="flex items-center gap-1.5">
          <span class="dot" :class="`dot-${t.toLowerCase()}`" aria-hidden="true"></span>{{ legendLabel(t) }}
        </span>
      </div>
    </div>

    <div class="flex flex-col">
      <span class="pb-1.5 text-label font-medium text-text-tertiary">Largest inside</span>
      <div v-for="l in largest" :key="l.path" class="flex items-center gap-2.5 border-t border-line py-[9px]" data-testid="largest">
        <span class="swatch" :class="`dot-${l.tier.toLowerCase()}`" :title="TIERS[l.tier].label"></span>
        <span class="sr-only">{{ TIERS[l.tier].label }}</span>
        <span class="min-w-0 flex-1 truncate font-mono text-label" :title="l.path">{{ l.path }}</span>
        <span class="font-mono text-label">{{ formatBytes(l.bytes) }}</span>
      </div>
    </div>

    <span class="text-label text-text-secondary" data-testid="detail-note">{{ note }}</span>
    <ul v-if="reasons" class="m-0 flex list-none flex-col gap-1.5 rounded-card bg-surface-raised p-3.5 text-label" data-testid="reasons">
      <li v-for="r in reasons" :key="r">{{ r }}</li>
      <li v-if="reasons.length === 0">Nothing in here is protected.</li>
    </ul>
    <p v-if="message" class="m-0 text-label text-text-secondary" role="status" data-testid="detail-message">{{ message }}</p>
    <ActionFooter panel="aside" class="flex flex-col gap-3" aria-label="Folder actions">
      <ActionBar :summary="name" :aria-expanded="action === 'why' ? reasons !== null : undefined" :label="ACTION_LABEL[action]" data-testid="detail-action" @action="$emit('action')">
        <button v-if="canReveal" type="button" class="min-h-touch cursor-pointer rounded-button border border-border bg-surface px-4 text-label text-ink" data-testid="detail-reveal" @click="$emit('reveal')">{{ revealLabel }}</button>
      </ActionBar>
    </ActionFooter>
  </section>
</template>

<style scoped>
.seg {
  transition: width 400ms ease;
}
.seg-s0 {
  background: var(--tier-s0-fill);
}
.seg-s1 {
  background: var(--tier-s1-fill);
}
.seg-s2 {
  background: repeating-linear-gradient(135deg, var(--color-tier-s2) 0 3px, var(--color-tier-s2-stripe) 3px 6px);
}
.seg-s3 {
  background: var(--color-tier-s3);
}
.dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
}
.swatch {
  width: 10px;
  height: 10px;
  border-radius: 3px;
  flex: none;
}
.dot-s0 {
  background: var(--color-tier-s0);
}
.dot-s1 {
  background: var(--color-tier-s1);
  border: 1px solid var(--color-lilac-border);
}
.dot-s2 {
  background: var(--color-tier-s2);
}
.dot-s3 {
  background: var(--color-tier-s3);
}
</style>
