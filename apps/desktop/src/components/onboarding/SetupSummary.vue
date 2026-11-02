<script setup lang="ts">
import { formatBytes } from '../../lib/format';
import type { FirstScan, OnboardingStep, SummaryRow } from './model';
defineProps<{ rows: SummaryRow[]; scan: FirstScan }>();
defineEmits<{ edit: [step: OnboardingStep] }>();
</script>

<template>
  <section v-if="scan.phase === 'idle'" aria-label="Review your setup" data-testid="summary">
    <dl class="review-list">
      <div v-for="row in rows" :key="row.k" class="review-row" data-testid="review-row">
        <div class="review-copy"><dt>{{ row.k }}</dt><dd>{{ row.v }}</dd></div>
        <button v-if="row.step" type="button" class="edit" :aria-label="`Edit ${row.k.toLowerCase()}`" :data-testid="`edit-step-${row.step}`" @click="$emit('edit', row.step)">Edit</button>
      </div>
    </dl>
    <p class="review-note">Your preferences are remembered on this device. This scan uses your selected folders. Agent connections, alerts, and cleanup settings are managed separately.</p>
  </section>
  <section v-else class="scan-status" data-testid="scan" aria-label="First scan">
    <div aria-live="polite" role="status">
      <p class="scan-label">{{ scan.phase === 'running' ? scan.activity : scan.phase === 'done' ? 'Scan complete' : scan.phase === 'cancelled' ? 'Scan stopped · partial results' : 'Scan failed' }}</p>
      <p class="entry-count">{{ (scan.result?.entries ?? scan.entries).toLocaleString('en-US') }} <span>entries</span></p>
    </div>
    <div v-if="scan.phase === 'running'" class="scan-track" role="progressbar" aria-label="Scanning selected folders"><span></span></div>
    <dl v-if="scan.result" class="result-list" data-testid="found-chips">
      <div><dt>Projects</dt><dd>{{ scan.result.projects }}</dd></div>
      <div><dt>Git worktrees</dt><dd>{{ scan.result.worktrees }}</dd></div>
      <div><dt>Regenerable storage found</dt><dd>{{ formatBytes(scan.result.safeBytes) }}</dd></div>
    </dl>
    <p v-if="scan.error" class="scan-error" role="alert">{{ scan.error }}</p>
  </section>
</template>

<style scoped>
.review-list { margin: 0; }
.review-row { display: flex; align-items: center; gap: 24px; padding: 18px 0; border-bottom: 1px solid var(--color-line); }
.review-copy { flex: 1; min-width: 0; }
dt { color: var(--color-text-secondary); font-size: 13px; }
dd { margin: 6px 0 0; font-weight: 500; line-height: 1.6; overflow-wrap: anywhere; }
.edit { padding: 8px; min-height: 44px; border: 0; background: transparent; color: var(--color-accent-text); font: inherit; cursor: pointer; }
.edit:focus-visible { outline: 2px solid var(--color-ink); outline-offset: 3px; }
.review-note { margin: 24px 0 0; font-size: 13px; line-height: 1.6; color: var(--color-text-secondary); }
.scan-status { margin: 36px 0; }
.scan-label { font-family: var(--font-mono); font-size: 13px; overflow-wrap: anywhere; color: var(--color-text-secondary); }
.entry-count { margin: 16px 0 24px; font-size: 40px; font-weight: 600; font-variant-numeric: tabular-nums; }
.entry-count span { font-size: 16px; font-weight: 400; color: var(--color-text-secondary); }
.scan-track { height: 4px; background: var(--color-border); overflow: hidden; border-radius: 2px; }
.scan-track span { display: block; width: 35%; height: 100%; background: var(--color-accent); animation: scanning 1.6s ease-in-out infinite alternate; }
@keyframes scanning { from { transform: translateX(-90%); } to { transform: translateX(275%); } }
.result-list { display: flex; flex-direction: column; gap: 0; margin: 24px 0; }
.result-list > div { display: flex; align-items: baseline; justify-content: space-between; gap: 24px; padding: 12px 0; border-bottom: 1px solid var(--color-line); }
.result-list dd { margin: 0; font-variant-numeric: tabular-nums; }
.scan-error { color: var(--color-accent-text); line-height: 1.6; }
@media (prefers-reduced-motion: reduce) { .scan-track span { animation: none; width: 100%; opacity: .6; } }
</style>
