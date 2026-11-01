import { beforeEach, describe, expect, it } from 'vitest';
import { addToCleanup, basketPlanRequest, cleanupBasket, removeFromCleanup } from './basket';
import { DESIGN_CANDIDATES } from './designScenario';
import { buildWheelModel, type WheelItem } from './wheel';

const items = (): WheelItem[] => buildWheelModel(DESIGN_CANDIDATES).categories.flatMap((c) => c.items);
const item = (id: string): WheelItem => items().find((i) => i.id === id)!;

beforeEach(() => cleanupBasket.clear());

describe('cleanup basket → plan request', () => {
  it('an empty basket asks for the whole last scan', () => {
    expect(basketPlanRequest()).toEqual({});
  });

  it('a marked basket asks for exactly its items, in the order they were marked', () => {
    addToCleanup(item('design-nm'));
    addToCleanup(item('design-target'));
    addToCleanup(item('design-nm'));
    expect(basketPlanRequest()).toEqual({ items: ['design-nm', 'design-target'] });
    expect(basketPlanRequest(new Set(['x']))).toEqual({ items: ['x'] });
  });

  it('a grouped segment adds every candidate behind it', () => {
    const grouped = buildWheelModel(DESIGN_CANDIDATES, 90)
      .categories.flatMap((c) => c.items)
      .find((i) => i.count > 1 && i.tier !== 'S3')!;
    addToCleanup(grouped);
    expect(basketPlanRequest().items).toEqual(grouped.candidateIds);
  });

  it('S3 items are never accepted, so they never reach a plan request', () => {
    const s3 = items().filter((i) => i.tier === 'S3');
    expect(s3.length).toBeGreaterThan(0);
    for (const it of s3) expect(addToCleanup(it)).toContain('is protected; scull never cleans it.');
    expect(cleanupBasket.size).toBe(0);
    expect(basketPlanRequest()).toEqual({});
  });

  it('cleaned items leave the basket', () => {
    addToCleanup(item('design-nm'));
    addToCleanup(item('design-target'));
    removeFromCleanup(['design-nm', 'not-marked']);
    expect([...cleanupBasket]).toEqual(['design-target']);
  });
});
