<script setup lang="ts">
// Supporter card (design 01). Pay-what-you-want; nothing is locked behind it. The purchase
// and key flows are not built yet, so the buttons only announce that.
import { ref } from 'vue';

const note = ref('');
</script>

<template>
  <div class="flex flex-col gap-2.5 rounded-[18px] bg-surface-raised p-3.5">
    <span class="text-label text-ink"><strong>Supporter</strong> · pay what you want</span>
    <span class="text-caption text-text-secondary">Signed builds, early access and extra skull themes.</span>
    <div class="flex gap-2">
      <button
        type="button"
        class="min-h-11 cursor-pointer rounded-small border-0 bg-ink px-3.5 text-label font-semibold text-on-ink"
        @click="note = 'Supporter checkout is not available yet.'"
      >
        Support
      </button>
      <button
        type="button"
        class="min-h-11 cursor-pointer rounded-small border-0 bg-transparent px-3 text-label text-text-strong-muted hover:text-ink"
        @click="note = 'Supporter keys are not available yet.'"
      >
        I have a key
      </button>
    </div>
    <span v-if="note" class="text-caption text-text-secondary" role="status">{{ note }}</span>
  </div>
</template>
