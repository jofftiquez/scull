// Items marked for cleanup from the Overview (shift-click a segment, or "Add to cleanup").
// The desktop never deletes: this is only the list Cleanup Review (P3-UI-04) can start from.
// Protected (S3) items are never added.
import { reactive } from 'vue';
import type { WheelItem } from './wheel';

export const cleanupBasket = reactive(new Set<string>());

/** Add an item's candidates; returns the sentence to announce. */
export function addToCleanup(item: WheelItem): string {
  if (item.tier === 'S3') return `${item.name} is protected; scull never cleans it.`;
  for (const id of item.candidateIds) cleanupBasket.add(id);
  const n = cleanupBasket.size;
  return `Added ${item.name} to cleanup · ${n} item${n === 1 ? '' : 's'} marked`;
}
