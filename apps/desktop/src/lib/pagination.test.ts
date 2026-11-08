import { ref } from 'vue';
import { describe, expect, it } from 'vitest';
import { usePagination } from './pagination';

describe('list pagination', () => {
  it('pages all rows and clamps the page when a filter or removal shrinks the list', () => {
    const rows = ref(Array.from({ length: 53 }, (_, i) => i));
    const p = usePagination(rows);
    expect(p.rows.value).toHaveLength(25);
    p.page.value = 3;
    expect(p.rows.value).toEqual([50, 51, 52]);
    rows.value = [7, 8];
    expect(p.rows.value).toEqual([7, 8]);
    expect(p.current.value).toBe(1);
    rows.value = [];
    expect(p.rows.value).toEqual([]);
    expect(p.current.value).toBe(1);
  });
});
