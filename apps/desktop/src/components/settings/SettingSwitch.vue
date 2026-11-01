<script setup lang="ts">
// Sliding switch (design 10): 48×28 track, magenta when on, the knob springs over in 220ms
// (cubic-bezier(.3,1.4,.5,1)). A real button with role="switch" and a 44px tall hit area;
// locked switches are disabled at 45% opacity.
defineProps<{ on: boolean; label: string; locked?: boolean; busy?: boolean }>();
defineEmits<{ toggle: [] }>();
</script>

<template>
  <button
    type="button"
    role="switch"
    :aria-checked="on ? 'true' : 'false'"
    :aria-label="label"
    :aria-busy="busy ? 'true' : undefined"
    :disabled="locked"
    class="flex min-h-[44px] flex-none cursor-pointer items-center border-0 bg-transparent p-0 disabled:cursor-not-allowed"
    data-testid="switch"
    @click="$emit('toggle')"
  >
    <span
      class="flex h-[28px] w-[48px] rounded-chip p-[3px] transition-[background] duration-[220ms] ease-[ease]"
      :style="{ background: on ? 'var(--color-accent)' : 'var(--color-border)', opacity: locked ? 0.45 : 1 }"
      aria-hidden="true"
    >
      <span
        class="h-[22px] w-[22px] rounded-full bg-surface transition-[margin-left] duration-[220ms] ease-[cubic-bezier(.3,1.4,.5,1)]"
        :style="{ marginLeft: on ? '20px' : '0px' }"
      />
    </span>
  </button>
</template>
