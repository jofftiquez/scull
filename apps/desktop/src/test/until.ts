// Test helper: wait for an event, not for a wall-clock budget.
//
// `vi.waitFor(check, { timeout })` polls and gives up after `timeout` ms. That is a race when
// the awaited work is CPU-bound and paced by timers: a mocked scan replays its progress with
// setTimeout, the Overview's sweep re-renders the whole app in jsdom every animation frame, and
// on a loaded machine (a parallel cargo build, CI) the same work takes several times longer.
// A 2-3 s budget then fails with the scan still at "Scanning 0%" (#290).
//
// `until(ready)` resolves as soon as `ready()` is true and re-checks it whenever something it
// could depend on changes: the reactive state it reads (Vue `watch`) and the DOM under `root`
// (a MutationObserver, for checks that read rendered text; default `document.body`, pass the
// wrapper's parent for a component mounted without `attachTo`). Nothing polls and
// there is no budget of its own; the test's own timeout (`testTimeout`) still ends a wait whose
// event never comes.
import { watch } from 'vue';

export function until(ready: () => boolean, root: Node = document.body): Promise<void> {
  const check = () => {
    try {
      return ready();
    } catch {
      return false; // e.g. the element isn't rendered yet
    }
  };
  return new Promise<void>((resolve) => {
    let done = false;
    const observer = new MutationObserver(() => finish());
    // Neither callback runs synchronously, so `stop` is set before `finish` can use it.
    const stop = watch(check, () => finish());
    function finish() {
      if (done || !check()) return;
      done = true;
      observer.disconnect();
      stop();
      resolve();
    }
    observer.observe(root, { subtree: true, childList: true, characterData: true, attributes: true });
    finish();
  });
}
