// Design 05's duplication and cleanup-accuracy tiles. The history has no source for either yet
// (identical-package detection and measured cleanups arrive with later phases), so the screen
// shows an honest empty state; `#/insights?demo=design` with the mock bridge shows these
// illustrative values instead, like the Projects screen's design demo.
const GiB = 1024 ** 3;

export const DESIGN_DUPLICATION = {
  bytes: 7.4 * GiB,
  rows: [
    { name: 'typescript@5.9', where: 'in 17 projects' },
    { name: 'next@16.1', where: 'in 6 projects' },
    { name: 'torch@2.9', where: 'in 3 venvs' },
  ],
  note: 'Switching the 11 npm projects to pnpm would store each package once.',
};

export const DESIGN_ACCURACY = {
  medianError: 0.06,
  rows: [
    { date: 'Oct 20', predicted: 9.4 * GiB, measured: 9.0 * GiB },
    { date: 'Oct 9', predicted: 22.8 * GiB, measured: 21.9 * GiB },
    { date: 'Sep 28', predicted: 7.1 * GiB, measured: 2.8 * GiB },
    { date: 'Sep 14', predicted: 12.6 * GiB, measured: 11.9 * GiB },
  ],
  note: 'Sep 28 freed less: a Time Machine snapshot was holding 4.3 GB until it expired the next day.',
};
