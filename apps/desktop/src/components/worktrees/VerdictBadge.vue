<script setup lang="ts">
// Worktree verdict chip (design 04): the word names the verdict; Review is striped and
// Protected is solid plum, so it never depends on color alone.
import type { Verdict } from './worktrees';
import { VERDICT_LABEL } from './worktrees';

defineProps<{ verdict: Verdict }>();

const STYLE: Record<Verdict, string> = {
  removable: 'background: var(--tier-s1-fill); color: var(--color-tier-s1-text)',
  review: 'background: var(--tier-s2-fill); color: var(--color-tier-s2-text)',
  protected: 'background: var(--color-tier-s3); color: var(--color-tier-s3-text)',
};
</script>

<template>
  <span class="rounded-chip px-2 py-[3px] font-mono text-[11px] font-medium" :style="STYLE[verdict]" :data-verdict="verdict">
    {{ VERDICT_LABEL[verdict] }}
  </span>
</template>
