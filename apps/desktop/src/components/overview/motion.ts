// Reduced motion (D-20): CSS animations collapse globally (styles/main.css); script-driven
// motion (the radar sweep, count-downs) reads this and jumps to its end state instead.
import { onBeforeUnmount, onMounted, ref, type Ref } from 'vue';

export const REDUCED_MOTION_QUERY = '(prefers-reduced-motion: reduce)';

export function prefersReducedMotion(w: { matchMedia?: (q: string) => MediaQueryList } = globalThis): boolean {
  return typeof w.matchMedia === 'function' && w.matchMedia(REDUCED_MOTION_QUERY).matches;
}

/** Live `prefers-reduced-motion` flag. */
export function useReducedMotion(): Ref<boolean> {
  const reduced = ref(prefersReducedMotion());
  let mq: MediaQueryList | null = null;
  const update = (e: MediaQueryListEvent) => (reduced.value = e.matches);
  onMounted(() => {
    if (typeof globalThis.matchMedia !== 'function') return;
    mq = globalThis.matchMedia(REDUCED_MOTION_QUERY);
    reduced.value = mq.matches;
    mq.addEventListener?.('change', update);
  });
  onBeforeUnmount(() => mq?.removeEventListener?.('change', update));
  return reduced;
}

/** requestAnimationFrame, or a 16 ms timer where there is none (tests). */
export function nextFrame(cb: (t: number) => void): () => void {
  if (typeof globalThis.requestAnimationFrame === 'function') {
    const id = globalThis.requestAnimationFrame(cb);
    return () => globalThis.cancelAnimationFrame(id);
  }
  const id = setTimeout(() => cb(performance.now()), 16);
  return () => clearTimeout(id);
}
