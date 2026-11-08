<script setup lang="ts">
defineOptions({ inheritAttrs: false });
withDefaults(defineProps<{ summary: string; label: string; disabled?: boolean; busy?: boolean; error?: string | null; tone?: 'clean' | 'remove' }>(), { tone: 'clean', error: null });
defineEmits<{ action: [] }>();
</script>
<template>
  <div class="action-bar" :data-tone="tone">
    <div class="min-w-0 flex-1"><strong class="text-label">{{ summary }}</strong><p v-if="error" role="alert" class="m-0 mt-1 text-label text-accent-text">{{ error }}</p></div>
    <slot />
    <button v-bind="$attrs" type="button" class="primary" :disabled="disabled || busy" @click="$emit('action')"><span v-if="busy" class="activity" aria-hidden="true" />{{ label }}</button>
  </div>
</template>
<style scoped>
.action-bar { display:flex; flex-wrap:wrap; align-items:center; gap:12px; width:100%; }
.primary { display:flex; justify-content:center; align-items:center; gap:8px; min-height:48px; padding:0 20px; border:0; border-radius:var(--radius-button); background:var(--color-ink); color:var(--color-on-ink); font-weight:600; cursor:pointer; }
[data-tone=remove] .primary { background:var(--color-accent); color:var(--color-on-fill); }
.primary:disabled { opacity:.5; cursor:default; }
.activity { width:14px; height:14px; border:2px solid currentColor; border-right-color:transparent; border-radius:50%; animation:turn 900ms linear infinite; }
@keyframes turn { to { transform:rotate(360deg); } }
@media(prefers-reduced-motion:reduce) { .activity { animation:none; } }
</style>
