//! Proactive protection (PRD §16): the user hears about a problem days before the disk
//! becomes unusable.
//!
//! Pure, read-only logic over the local history that `scull-storage` records (INS-01,
//! INS-06). Nothing here touches the filesystem or deletes anything; the CLI (`scull status`,
//! `scull check`), the MCP server and the desktop call it with samples they read.
//!
//! * [`mod@forecast`]: per-volume disk-full forecast (PRO-01, D-22): a robust trend over the last
//!   30 days of free-space samples that ignores cleanups and one-off spikes, the days until
//!   free space drops below a reserve, and a confidence level. See the [module
//!   docs](mod@forecast) for the method and the differences from the desktop's `forecast.ts`.
//! * [`alert`]: alert records with severity and a stable key, and the forecast alerts.
//! * [`policy`]: staleness policies, budgets and spike alerts (PRO-02/03/04/08), configured by
//!   `[proactive]` in the user config.
//! * [`history`]: thin read helpers that load the inputs from a [`scull_storage::Store`].
//! * [`review`]: the ready-to-review reclaim plan (PRO-05): which items a proposal holds, and
//!   the announcement `scull status` / `scull check` read back. A proposal only: reviewing and
//!   running it is `scull clean --plan`.
//! * [`monitor`]: tray monitoring (PRD §14.6): which alerts become desktop notifications,
//!   cooldowns, pause, exclusions, and the weekly digest's schedule (INS-04). Reads and
//!   notifies only.
//! * [`schedule`]: the unit files and tool invocations `scull schedule` registers with launchd,
//!   systemd or Task Scheduler (PRO-07).

pub mod alert;
pub mod forecast;
pub mod history;
pub mod monitor;
pub mod policy;
pub mod review;
pub mod schedule;
pub mod stats;

pub use alert::{Alert, AlertKind, Severity, forecast_alerts};
pub use forecast::{Confidence, Forecast, ForecastPolicy, ForecastState, Release, forecast};
pub use history::{
    ScanInput, category_series, category_spike_alerts, category_usage, check_alerts, forecast_all,
    forecast_volume, free_space_drop_alerts, latest_scan_roots,
};
pub use policy::{
    Activity, SeriesPoint, StaleKind, Usage, activity_from_candidate, budget_alerts,
    forecast_policy, stale_alerts, usage_from_candidates,
};
