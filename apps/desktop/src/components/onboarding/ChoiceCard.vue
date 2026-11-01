<script setup lang="ts">
// A two-line single-choice button (design 02, questions 4 and 5): ink when picked.
defineProps<{ label: string; hint: string; pressed: boolean }>();
defineEmits<{ pick: [] }>();
</script>

<template>
  <button
    type="button"
    :aria-pressed="pressed"
    class="flex min-h-16 w-full cursor-pointer flex-col items-start gap-0.5 rounded-button border-0 px-4 py-3 text-left font-[inherit] text-[length:inherit]"
    :class="pressed ? 'bg-ink text-on-ink' : 'bg-surface-raised text-ink hover:bg-line'"
    @click="$emit('pick')"
  >
    <span class="font-semibold">{{ label }}</span>
    <span class="text-label opacity-80">{{ hint }}</span>
  </button>
</template>
