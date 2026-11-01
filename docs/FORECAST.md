# Disk-full forecast

scull tells you how many days you have before a drive's free space drops below a reserve
(PRD PRO-01). There is **one forecast**: `scull_proactive::forecast`, run with one policy built
from your config (`scull_proactive::forecast_policy`). Every surface that shows days left reads
it, so they all give the same number for the same history:

| Surface | Where it comes from |
|---|---|
| `scull check` (text, `--json`), alerts | `forecast_all(store, now, policy)` |
| `scull status`, `scull status --short` | the same call (an unreadable config falls back to the defaults, so the prompt line never fails) |
| `scull insights` (`free_space[].forecast`) | the same method and policy, inside the insights report |
| MCP `check_capacity` | `forecast_volume` with the same policy |
| Weekly digest (notification, Insights "This week") | the insights report of the last 7 days |
| Desktop: Insights tile and chart, Overview alert, menu bar widget | the `forecasts` bridge command (`forecast_all`), read by `apps/desktop/src/lib/forecast.ts` |
| Tray monitor alerts | the same method and policy, with the latest live free-space measurement added as one more sample |

The desktop does not fit a trend of its own. It draws the core's result: days, rate, confidence,
and the dashed line from the latest sample down to the reserve.

`crates/scull-cli/tests/forecast_surfaces.rs` feeds one history (with a non-default reserve) to
`scull status`, `scull check`, `scull insights`, the digest, the tray monitor and the desktop
bridge, and checks that they report the same days left, confidence and rate.
`apps/desktop/src/lib/forecast.test.ts` does the same for the desktop screens, using the recorded
core payloads.

## Settings

`[proactive]` in `config.toml`:

| Key | Default | Meaning |
|---|---|---|
| `reserve_gib` | `10` | "Full" means less free space than this. |
| `alert_days` | `14` | A forecast alerts when the drive will reach the reserve within this many days. |

The method's other settings are fixed: 30-day window, at least 4 samples over 48 hours,
3 days for a critical alert, 365-day horizon. The desktop's "Warn me when the disk is N days
out" setting applies to the tray monitor's alerts only.

## Method

1. **Window.** Only samples from the last 30 days count. Each scan and each `scull check`
   records one sample per drive. Samples from the future, or with more free space than the
   drive's size, are ignored.
2. **Below the reserve already?** Then the answer is *below reserve*: a measurement, so it
   needs only one sample.
3. **Enough history?** It needs at least 4 samples spanning at least 48 hours. Otherwise the
   answer is *not enough history*: scull never extrapolates from a handful of points.
4. **One-off spikes** (a big download deleted again soon after) are dropped.
5. **Cleanups and one-off steps** are levelled out, so the trend only sees how fast space is
   being used:
   - known cleanups;
   - unexplained rises in free space that are big compared with the noise;
   - rare large drops, such as an OS update.
6. **Robust trend.** The rate is the median of the slopes between pairs of samples at least
   7 days apart (Theil–Sen). One odd day can't move it much.
7. **Days** are counted from the latest measured free space: `(free − reserve) / rate`. A drive
   whose free space is steady or growing, or that would take more than a year to fill, is *not
   filling*.

People see whole days: at least 1 while filling, 0 when already below the reserve.

## Confidence

Confidence is the lowest of three checks:

| | high | medium | low |
|---|---|---|---|
| History | ≥ 10 samples over ≥ 14 days | ≥ 6 samples over ≥ 7 days | less |
| Noise (spread ÷ the change the trend explains) | ≤ 0.10 | ≤ 0.25 | more |
| Reach (days ahead ÷ days of history) | ≤ 2 | ≤ 6 (one level lower) | more |

**Where each confidence shows.** Alerts and status lines only use a forecast with at least
medium confidence, or one that is already below the reserve (`Forecast::is_actionable`). That
covers `scull check` alerts, the `scull status` line, the tray monitor, the digest's days, the
Overview alert and the menu bar widget. The Insights tile shows any filling forecast, with its
confidence next to it, so a low-confidence estimate is visible but never raises an alert.

## Bytes freed since the latest sample

When the desktop has freed space since the latest sample (Quick Clean), the Overview alert and
the menu bar move the date back by `freed ÷ rate` days. The core does the same with a known
release. The next scan or check records the new free space, and the forecast then counts from
that sample.
