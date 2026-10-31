<script setup lang="ts">
// Segmented choice (design 10): a pill group, the chosen option ink. Buttons with aria-pressed;
// the visible pill is 32px tall and the hit area extends to 44px.
defineProps<{ label: string; options: readonly (readonly [string, string])[]; value: string | null }>();
defineEmits<{ pick: [value: string] }>();
</script>

<template>
  <span role="group" :aria-label="label" class="flex flex-none gap-0.5 rounded-chip bg-surface-raised p-[3px]">
    <button
      v-for="[v, text] in options"
      :key="v"
      type="button"
      :aria-pressed="value === v ? 'true' : 'false'"
      class="relative min-h-[32px] cursor-pointer rounded-chip border-0 px-3 text-label font-medium transition-[background] duration-200 after:absolute after:inset-x-0 after:-inset-y-[6px] after:content-['']"
      :class="value === v ? 'bg-ink text-on-ink' : 'bg-transparent text-text-strong-muted hover:text-ink'"
      @click="$emit('pick', v)"
    >
      {{ text }}
    </button>
  </span>
</template>
