//! Activity signals (PRD §8.4, D-8, D-26): whether a candidate is **in use** right now, and
//! whether it was **modified recently**.
//!
//! This module holds the OS-independent policy. The facts come from an [`ActivityProbe`]:
//! [`NativeActivityProbe`] reads them from the OS through `scull_platform::activity` (Linux
//! `/proc`, macOS `libproc`, Windows PEB + Restart Manager); tests use fakes.
//!
//! # In use ([`Signals::in_use`](crate::Signals::in_use))
//!
//! [`check_in_use`] asks three questions and combines the answers:
//!
//! | Question | Probe method | Evidence on "yes" |
//! |---|---|---|
//! | Does a running process have its working directory inside? | [`ActivityProbe::process_cwds_under`] | `in_use` / "process cwd" |
//! | Does a running process hold a file inside open? | [`ActivityProbe::open_files_under`] | `in_use` / "open file" |
//! | Is a tool's lock file held (e.g. Cargo's `.cargo-lock`)? | [`ActivityProbe::files_held_open`] | `in_use` / "tool lock" |
//!
//! A lock file that merely *exists* proves nothing (Cargo leaves `.cargo-lock` behind after
//! every build); it counts only while a process holds it open, which is how Cargo holds its
//! lock.
//!
//! Combination: any "yes" ⇒ [`Tri::Yes`]; every answer "no" ⇒ [`Tri::No`]; otherwise
//! [`Tri::Unknown`]. An answer is "no" only when the probe saw **everything** it needed to:
//! a signal the OS doesn't provide, a process it couldn't inspect, or a bound that cut the
//! search short makes the answer unknown, never "not in use" (PRD §8.4). The rubric then
//! treats the candidate as S2 with a `signal_unavailable` reason (D-26), and every gap is
//! recorded as [`Evidence::SignalUnavailable`] so the preview says so.
//!
//! ## Processes that cannot be inspected ([`GapPolicy`])
//!
//! Without elevated privileges no OS lets a user inspect other users' processes, so reading
//! every uninspectable process as a gap would make every answer unknown on every normal
//! machine. [`NativeActivityProbe`] applies a [`GapPolicy`]:
//!
//! | Policy | Own-user processes not inspectable | Other users' processes not inspectable |
//! |---|---|---|
//! | [`GapPolicy::Strict`] | unknown | unknown |
//! | [`GapPolicy::OwnUser`] (default) | unknown | disclosed ([`Evidence::Note`]) |
//! | [`GapPolicy::Disclose`] | disclosed | disclosed |
//!
//! Truncated searches, unsupported signals and failed queries are unknown under every policy.
//!
//! Rationale for the default: scull deletes as the current user and quarantines anything not
//! S0; another user's process working in the user's build output is rare, and running scull
//! elevated removes the limitation. Note that on Linux some of the user's *own* processes are
//! not inspectable either (non-dumpable ones such as `systemd --user`, the `sshd` session
//! process, processes in rootless containers); under the default policy their presence makes
//! the answer unknown (S2).
//!
//! # Recent activity ([`Signals::recent_activity`](crate::Signals::recent_activity))
//!
//! [`sample_newest_mtime`] walks the candidate (no-follow, bounded by an entry count) for the
//! newest modification time of any entry, directories included (adding or removing a file is
//! activity). Callers that already have the scanner's `DirSummary::newest_mtime` and
//! `complete` flag build an [`MtimeSample`] from those instead. [`recent_activity`] turns the
//! sample into a [`Tri`] through [`ActivityWindow::observe`]: a sample cut short by its bound
//! only ever says "recent" or "unknown", never "idle".

use crate::rubric::{ActivityWindow, Tri};
use scull_core::Evidence;
use scull_platform::activity::{Limits, Observation, Snapshot};
use scull_platform::{EntryKind, Platform};
use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};

/// A process found holding something inside a candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// Process ID.
    pub pid: u32,
    /// Process name, if known.
    pub name: Option<String>,
    /// The working directory or file it holds (or the candidate, when the OS doesn't say).
    pub path: PathBuf,
}

/// Why a probe could not answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unavailable {
    /// The OS (or this build) does not provide the signal at all.
    Unsupported(String),
    /// Some processes could not be inspected (permissions, protected processes).
    PermissionLimited {
        /// How many processes.
        processes: usize,
        /// Whether they belong to other users (a gap only under [`GapPolicy::Strict`]).
        other_users: bool,
    },
    /// A time or entry bound cut the search short.
    Truncated,
    /// The query failed.
    Failed(String),
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unavailable::Unsupported(r) => write!(f, "not available on this platform: {r}"),
            Unavailable::PermissionLimited {
                processes,
                other_users: false,
            } => write!(
                f,
                "{processes} of your own processes could not be inspected (permissions)"
            ),
            Unavailable::PermissionLimited {
                processes,
                other_users: true,
            } => write!(
                f,
                "{processes} processes of other users could not be inspected (run elevated to include them)"
            ),
            Unavailable::Truncated => {
                write!(f, "the search hit its time or size bound before finishing")
            }
            Unavailable::Failed(e) => write!(f, "the check failed: {e}"),
        }
    }
}

/// The answer to one activity question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    /// At least one process holds it.
    InUse(Vec<Holder>),
    /// Everything was inspected and nothing holds it. `notes` disclose limitations that the
    /// policy does not count as gaps (see the module docs on other users' processes).
    NotInUse {
        /// Disclosed limitations.
        notes: Vec<String>,
    },
    /// The question could not be answered; never read as "not in use".
    Unknown(Unavailable),
}

impl ProbeResult {
    /// "Not in use" with no notes.
    pub fn not_in_use() -> Self {
        ProbeResult::NotInUse { notes: Vec::new() }
    }

    /// The answer as a three-valued signal.
    pub fn tri(&self) -> Tri {
        match self {
            ProbeResult::InUse(_) => Tri::Yes,
            ProbeResult::NotInUse { .. } => Tri::No,
            ProbeResult::Unknown(_) => Tri::Unknown,
        }
    }
}

/// Source of process activity facts. [`NativeActivityProbe`] is the real OS; tests implement
/// it with fakes.
///
/// Implementations must be read-only and must never report "not in use" for something they
/// did not fully check.
pub trait ActivityProbe {
    /// Processes whose working directory is `dir` or inside it.
    fn process_cwds_under(&self, dir: &Path) -> ProbeResult;
    /// Processes holding a file or directory at `dir` or inside it open.
    fn open_files_under(&self, dir: &Path) -> ProbeResult;
    /// Processes holding any of `files` open. Files that don't exist are not held.
    fn files_held_open(&self, files: &[PathBuf]) -> ProbeResult;
}

/// Which activity question an answer belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActivitySignal {
    /// A process's working directory.
    ProcessCwd,
    /// An open file or directory handle.
    OpenFile,
    /// A held tool lock file.
    ToolLock,
}

impl ActivitySignal {
    /// Label used in evidence (`Evidence::InUse::signal`, `Evidence::SignalUnavailable::signal`).
    pub fn label(self) -> &'static str {
        match self {
            ActivitySignal::ProcessCwd => "process cwd",
            ActivitySignal::OpenFile => "open file",
            ActivitySignal::ToolLock => "tool lock",
        }
    }
}

/// The combined in-use verdict for one candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InUse {
    /// The value for [`Signals::in_use`](crate::Signals::in_use).
    pub in_use: Tri,
    /// Each question and its answer, in the order asked.
    pub answers: Vec<(ActivitySignal, ProbeResult)>,
    /// Evidence to attach to the candidate: who holds it, which signals were unavailable and
    /// why, and disclosed limitations.
    pub evidence: Vec<Evidence>,
}

/// Most holders recorded as evidence per signal (the rest are counted in a note).
const MAX_HOLDER_EVIDENCE: usize = 5;

/// Whether `dir` is in use: process working directories, open files, and the held state of
/// `lock_files` (the detector's `activity_lock_files`), combined as described in the module
/// docs.
pub fn check_in_use(dir: &Path, lock_files: &[PathBuf], probe: &dyn ActivityProbe) -> InUse {
    let mut answers = vec![
        (ActivitySignal::ProcessCwd, probe.process_cwds_under(dir)),
        (ActivitySignal::OpenFile, probe.open_files_under(dir)),
    ];
    if !lock_files.is_empty() {
        answers.push((ActivitySignal::ToolLock, probe.files_held_open(lock_files)));
    }

    let tris: Vec<Tri> = answers.iter().map(|(_, r)| r.tri()).collect();
    let in_use = if tris.contains(&Tri::Yes) {
        Tri::Yes
    } else if tris.iter().all(|t| *t == Tri::No) {
        Tri::No
    } else {
        Tri::Unknown
    };

    let mut evidence = Vec::new();
    for (signal, result) in &answers {
        match result {
            ProbeResult::InUse(holders) => {
                for h in holders.iter().take(MAX_HOLDER_EVIDENCE) {
                    let by = match &h.name {
                        Some(name) => format!("{name} (pid {})", h.pid),
                        None => format!("pid {}", h.pid),
                    };
                    evidence.push(Evidence::InUse {
                        signal: signal.label().into(),
                        by: Some(by),
                    });
                }
                if holders.len() > MAX_HOLDER_EVIDENCE {
                    evidence.push(Evidence::Note {
                        text: format!(
                            "{} more processes hold it ({})",
                            holders.len() - MAX_HOLDER_EVIDENCE,
                            signal.label()
                        ),
                    });
                }
            }
            ProbeResult::NotInUse { notes } => {
                for n in notes {
                    evidence.push(Evidence::Note {
                        text: format!("{}: {n}", signal.label()),
                    });
                }
            }
            ProbeResult::Unknown(why) => evidence.push(Evidence::SignalUnavailable {
                signal: signal.label().into(),
                reason: why.to_string(),
            }),
        }
    }
    InUse {
        in_use,
        answers,
        evidence,
    }
}

/// How uninspectable processes affect an answer (see the module docs).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum GapPolicy {
    /// Any process that could not be inspected makes the answer unknown.
    Strict,
    /// The current user's uninspectable processes make the answer unknown; other users' are
    /// disclosed as a note.
    #[default]
    OwnUser,
    /// Every uninspectable process is disclosed as a note; only truncation, unsupported
    /// signals and failures make the answer unknown.
    Disclose,
}

/// [`ActivityProbe`] backed by the OS, through one `scull_platform::activity::Snapshot`.
///
/// Take one per scan (and a fresh one when revalidating before execution): listing processes
/// is the expensive part, and each candidate query only matches paths.
#[derive(Debug)]
pub struct NativeActivityProbe {
    snapshot: Snapshot,
    policy: GapPolicy,
}

impl NativeActivityProbe {
    /// Inspect the running processes now, within `limits`, with the default [`GapPolicy`].
    pub fn capture(limits: Limits) -> Self {
        Self::from_snapshot(Snapshot::capture(limits))
    }

    /// Use an existing snapshot, with the default [`GapPolicy`].
    pub fn from_snapshot(snapshot: Snapshot) -> Self {
        Self {
            snapshot,
            policy: GapPolicy::default(),
        }
    }

    /// Use `policy` for uninspectable processes.
    pub fn with_policy(mut self, policy: GapPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// The underlying snapshot.
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    fn map(&self, obs: Observation) -> ProbeResult {
        map_observation(obs, self.policy)
    }
}

/// Apply `policy` to one platform observation: hits ⇒ in use; any gap the policy counts ⇒
/// unknown; otherwise not in use, with the gaps it doesn't count disclosed as notes.
pub fn map_observation(obs: Observation, policy: GapPolicy) -> ProbeResult {
    if !obs.hits.is_empty() {
        return ProbeResult::InUse(
            obs.hits
                .into_iter()
                .map(|h| Holder {
                    pid: h.pid,
                    name: h.name,
                    path: h.path,
                })
                .collect(),
        );
    }
    if let Some(reason) = obs.unsupported {
        return ProbeResult::Unknown(Unavailable::Unsupported(reason));
    }
    if let Some(error) = obs.error {
        return ProbeResult::Unknown(Unavailable::Failed(error));
    }
    if obs.truncated {
        return ProbeResult::Unknown(Unavailable::Truncated);
    }
    let own = Unavailable::PermissionLimited {
        processes: obs.denied_own,
        other_users: false,
    };
    let other = Unavailable::PermissionLimited {
        processes: obs.denied_other,
        other_users: true,
    };
    let (own_is_gap, other_is_gap) = match policy {
        GapPolicy::Strict => (true, true),
        GapPolicy::OwnUser => (true, false),
        GapPolicy::Disclose => (false, false),
    };
    let mut notes = Vec::new();
    for (count, why, is_gap) in [
        (obs.denied_own, own, own_is_gap),
        (obs.denied_other, other, other_is_gap),
    ] {
        if count == 0 {
            continue;
        }
        if is_gap {
            return ProbeResult::Unknown(why);
        }
        notes.push(why.to_string());
    }
    ProbeResult::NotInUse { notes }
}

impl ActivityProbe for NativeActivityProbe {
    fn process_cwds_under(&self, dir: &Path) -> ProbeResult {
        self.map(self.snapshot.cwds_under(dir))
    }

    fn open_files_under(&self, dir: &Path) -> ProbeResult {
        self.map(self.snapshot.open_files_under(dir))
    }

    fn files_held_open(&self, files: &[PathBuf]) -> ProbeResult {
        self.map(self.snapshot.files_open(files))
    }
}

/// The newest modification time observed under a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MtimeSample {
    /// Newest mtime (Unix seconds) of any entry seen, the candidate itself included. `None` if
    /// no mtime could be read.
    pub newest: Option<i64>,
    /// Whether every entry was seen. `false` if the bound was hit or anything was unreadable,
    /// so the true newest may be later.
    pub complete: bool,
}

impl MtimeSample {
    fn see(&mut self, mtime: Option<i64>) {
        match mtime {
            Some(m) => self.newest = Some(self.newest.map_or(m, |n| n.max(m))),
            None => self.complete = false,
        }
    }

    /// From the scanner's `DirSummary` (`newest_mtime`, `complete`).
    pub fn from_summary(newest_mtime: Option<i64>, complete: bool) -> Self {
        Self {
            newest: newest_mtime,
            complete,
        }
    }
}

/// Default bound for [`sample_newest_mtime`]: entries whose metadata is read.
pub const DEFAULT_MTIME_SAMPLE_ENTRIES: usize = 20_000;

/// Walk `dir` breadth first (never through links, never opening files) and return the newest
/// modification time of anything in it, reading metadata of at most `max_entries` entries.
pub fn sample_newest_mtime(platform: &dyn Platform, dir: &Path, max_entries: usize) -> MtimeSample {
    let mut sample = MtimeSample {
        newest: None,
        complete: true,
    };
    match platform.metadata(dir) {
        Ok(md) => {
            sample.see(md.mtime_unix);
            if md.kind != EntryKind::Dir {
                return sample;
            }
        }
        Err(_) => {
            sample.complete = false;
            return sample;
        }
    }

    let mut seen = 0usize;
    let mut queue = VecDeque::from([dir.to_path_buf()]);
    'walk: while let Some(d) = queue.pop_front() {
        let Ok(entries) = platform.read_dir(&d) else {
            sample.complete = false;
            continue;
        };
        for entry in entries {
            if seen >= max_entries {
                sample.complete = false;
                break 'walk;
            }
            seen += 1;
            let Ok(entry) = entry else {
                sample.complete = false;
                continue;
            };
            match platform.metadata(&entry.path) {
                Ok(md) => sample.see(md.mtime_unix),
                Err(_) => sample.complete = false,
            }
            if entry.kind == EntryKind::Dir {
                queue.push_back(entry.path);
            }
        }
    }
    sample
}

/// The recent-activity verdict for one candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentActivity {
    /// The value for [`Signals::recent_activity`](crate::Signals::recent_activity).
    pub recent_activity: Tri,
    /// [`Evidence::LastModified`] when an mtime was read, else [`Evidence::SignalUnavailable`].
    pub evidence: Evidence,
}

/// Apply `window` to `sample` at `now_unix` (supplied by the caller).
pub fn recent_activity(
    window: ActivityWindow,
    sample: MtimeSample,
    now_unix: i64,
) -> RecentActivity {
    let recent_activity = window.observe(sample.newest, !sample.complete, now_unix);
    let evidence = match sample.newest {
        Some(newest_unix) => Evidence::LastModified {
            newest_unix,
            sampled: !sample.complete,
        },
        None => Evidence::SignalUnavailable {
            signal: "recent activity".into(),
            reason: "no modification time could be read".into(),
        },
    };
    RecentActivity {
        recent_activity,
        evidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scull_platform::activity::Hit;

    fn obs() -> Observation {
        Observation::default()
    }

    #[test]
    fn map_hits_win_over_gaps() {
        let mut o = obs();
        o.hits.push(Hit {
            pid: 7,
            name: Some("cargo".into()),
            path: PathBuf::from("/p/target"),
        });
        o.truncated = true;
        o.denied_own = 3;
        assert_eq!(map_observation(o, GapPolicy::Strict).tri(), Tri::Yes);
    }

    #[test]
    fn map_gaps_are_unknown_never_no() {
        let mut o = obs();
        o.unsupported = Some("x".into());
        assert!(matches!(
            map_observation(o, GapPolicy::OwnUser),
            ProbeResult::Unknown(Unavailable::Unsupported(_))
        ));
        let mut o = obs();
        o.error = Some("x".into());
        assert!(matches!(
            map_observation(o, GapPolicy::OwnUser),
            ProbeResult::Unknown(Unavailable::Failed(_))
        ));
        let mut o = obs();
        o.truncated = true;
        assert_eq!(
            map_observation(o, GapPolicy::OwnUser),
            ProbeResult::Unknown(Unavailable::Truncated)
        );
        let mut o = obs();
        o.denied_own = 2;
        assert_eq!(
            map_observation(o, GapPolicy::OwnUser),
            ProbeResult::Unknown(Unavailable::PermissionLimited {
                processes: 2,
                other_users: false
            })
        );
    }

    #[test]
    fn map_other_users_policy() {
        let mut o = obs();
        o.denied_other = 4;
        let lenient = map_observation(o.clone(), GapPolicy::OwnUser);
        let ProbeResult::NotInUse { notes } = &lenient else {
            panic!("expected NotInUse, got {lenient:?}");
        };
        assert_eq!(notes.len(), 1, "the limitation is disclosed");
        assert_eq!(map_observation(o, GapPolicy::Strict).tri(), Tri::Unknown);
        assert_eq!(
            map_observation(obs(), GapPolicy::Strict),
            ProbeResult::not_in_use()
        );
        let mut o = obs();
        o.denied_own = 1;
        o.denied_other = 1;
        assert_eq!(
            map_observation(o.clone(), GapPolicy::OwnUser).tri(),
            Tri::Unknown
        );
        let ProbeResult::NotInUse { notes } = map_observation(o, GapPolicy::Disclose) else {
            panic!("Disclose never turns permission gaps into unknown");
        };
        assert_eq!(notes.len(), 2);
    }
}
