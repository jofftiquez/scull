// Items marked for cleanup from the Overview (shift-click a segment, or "Add to cleanup").
// The desktop never deletes: this is only the list Cleanup Review (P3-UI-04) plans from.
// Protected (S3) items are never added.
import { reactive } from 'vue';
import type { ItemId } from '../../bridge';
import type { WheelItem } from './wheel';

export const cleanupBasket = reactive(new Set<string>());

/** Add an item's candidates; returns the sentence to announce. */
export function addToCleanup(item: WheelItem): string {
  if (item.tier === 'S3') return `${item.name} is protected; scull never cleans it.`;
  for (const id of item.candidateIds) cleanupBasket.add(id);
  const n = cleanupBasket.size;
  return `Added ${item.name} to cleanup · ${n} item${n === 1 ? '' : 's'} marked`;
}

/**
 * The plan request Cleanup Review sends: the marked items when there are any, else `{}` so
 * the core plans from the whole last scan. The core still re-checks every item and never
 * plans an S3 one (SAFE-11).
 */
export function basketPlanRequest(basket: ReadonlySet<ItemId> = cleanupBasket): { items?: ItemId[] } {
  return basket.size > 0 ? { items: [...basket] } : {};
}

/** Forget marked items (cleaned, or no longer plannable). */
export function removeFromCleanup(ids: Iterable<ItemId>): void {
  for (const id of ids) cleanupBasket.delete(id);
}
