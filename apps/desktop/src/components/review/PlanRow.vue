<script setup lang="ts">
// One plan item (design 03/12 row): lead cell (checkbox or status), path + context, tier,
// evidence, action, predicted bytes with confidence, trail cell (undo/outcome), and a
// disclosure that opens every SAFE-01 field (ItemDetails).
import { computed, ref } from 'vue';
import type { PlanItem } from '../../bridge/plan';
import { formatBytes } from '../../lib/format';
import ItemDetails from './ItemDetails.vue';
import TierBadge from './TierBadge.vue';
import { actionLabel, evidenceSummary, flagChips, itemSubline, itemTitle, tierReason } from './plan';

const props = defineProps<{
  item: PlanItem;
  grid: 'plan-grid' | 'agent-grid';
  rowClass?: string | Record<string, boolean>;
  /** Replaces the sub-line (e.g. why the item changed). */
  sub?: string;
  dim?: boolean;
}>();

const open = ref(false);
const c = computed(() => props.item.candidate);
const title = computed(() => itemTitle(props.item));
const chips = computed(() => flagChips(c.value.flags));
const detailsId = computed(() => `details-${c.value.id}`);
</script>

<template>
  <div role="listitem" class="border-t border-line transition-[background] duration-[250ms]" :class="rowClass" :data-item="c.id">
    <div :class="grid" class="min-h-16" :style="dim ? 'opacity: 0.6' : undefined">
      <div class="flex h-16 items-center"><slot name="lead" /></div>
      <span class="flex min-w-0 flex-col gap-0.5 py-2.5 pr-3">
        <span class="font-mono text-label font-medium [overflow-wrap:anywhere]">{{ title }}</span>
        <span class="text-caption" :class="sub ? 'text-ink' : 'text-text-secondary'">{{ sub ?? itemSubline(item) }}</span>
        <span v-if="chips.length" class="flex flex-wrap gap-1">
          <span v-for="f in chips" :key="f" class="rounded-chip border border-border px-1.5 text-[11px] text-text-strong-muted">{{ f }}</span>
        </span>
      </span>
      <span><TierBadge :tier="c.tier" :reason="tierReason(c)" /></span>
      <span class="pr-3 text-label">{{ evidenceSummary(c) }}</span>
      <span class="text-label font-medium">{{ actionLabel(c.action) }}</span>
      <span class="flex flex-col items-end pr-4">
        <span class="font-mono tabular-nums">{{ formatBytes(c.sizes.reclaimable.bytes) }}</span>
        <span class="text-[11px] whitespace-nowrap text-text-secondary">{{ c.sizes.reclaimable.confidence }} confidence</span>
      </span>
      <span class="pr-2 text-label text-text-secondary"><slot name="trail" /></span>
      <span class="flex justify-center">
        <button
          type="button"
          class="inline-flex h-11 w-11 cursor-pointer items-center justify-center rounded-button border-0 bg-transparent text-text-secondary hover:bg-surface-raised hover:text-ink"
          :aria-expanded="open"
          :aria-controls="detailsId"
          :aria-label="`${open ? 'Hide' : 'Show'} details for ${title}`"
          data-testid="details-toggle"
          @click="open = !open"
        >
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true" :style="{ transform: open ? 'rotate(180deg)' : 'none', transition: 'transform 200ms ease' }">
            <path d="M6 9l6 6 6-6" />
          </svg>
        </button>
      </span>
    </div>
    <ItemDetails v-if="open" :id="detailsId" :item="item" />
  </div>
</template>
