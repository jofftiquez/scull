<script setup lang="ts">
// "Right before each item is touched" (design 03/12): the SAFE-05 re-checks, in plain words.
defineProps<{ footnote?: string }>();

const CHECKS = [
  'Same folder as scanned (identity check)',
  'Not tracked by Git',
  'Not in use by any process',
  'Never follows links outside the item',
  "Tier hasn't gone up",
  'Stays on the same disk',
];
</script>

<template>
  <section aria-label="Safety checks" class="tile col-span-12 flex flex-col gap-2 rounded-tile bg-surface p-tile-padding lg:col-span-6">
    <h2 class="m-0 text-label font-medium text-text-secondary">Right before each item is touched</h2>
    <ul class="m-0 grid list-none grid-cols-1 gap-x-4 gap-y-1.5 p-0 text-label sm:grid-cols-2">
      <li v-for="c in CHECKS" :key="c">{{ c }}</li>
    </ul>
    <p v-if="footnote" class="m-0 text-label text-text-secondary">{{ footnote }}</p>
  </section>
</template>
