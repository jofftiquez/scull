<script setup lang="ts">
// Cleanup Review (PRD §14: the transaction screen). Two modes:
//   /cleanup                    the user's own plan from the last scan (design 03)
//   /cleanup?request=<plan-id>  an agent's MCP request waiting for approval (design 12, §13.3);
//                               the OS notification deep-links here.
import { computed } from 'vue';
import { useRoute } from 'vue-router';
import ScreenLayout from '../../components/shell/ScreenLayout.vue';
import AgentApproval from './AgentApproval.vue';
import DesktopReview from './DesktopReview.vue';

const route = useRoute();
const request = computed(() => {
  const r = route.query.request;
  return typeof r === 'string' && /^[A-Za-z0-9_-]{1,64}$/.test(r) ? r : null;
});
</script>

<template>
  <ScreenLayout>
    <AgentApproval v-if="request" :key="request" :plan-id="request" />
    <DesktopReview v-else />
  </ScreenLayout>
</template>
