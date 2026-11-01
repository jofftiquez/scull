//! What `scull schedule` registers with the OS scheduler (PRO-07): the exact unit files and
//! tool invocations that run `scull check` periodically, at low CPU and I/O priority, as the
//! current user. Pure: nothing here writes a file or runs a command; `scull schedule` shows
//! everything this module produces and asks before applying it.
//!
//! | scheduler | files | enable | disable |
//! |---|---|---|---|
//! | launchd (macOS) | `~/Library/LaunchAgents/dev.scull.check.plist` | `launchctl load -w <plist>` | `launchctl unload -w <plist>`, remove |
//! | systemd user (Linux) | `~/.config/systemd/user/scull-check.{service,timer}` | `systemctl --user daemon-reload`, `systemctl --user enable --now scull-check.timer` | `systemctl --user disable --now scull-check.timer`, remove, `daemon-reload` |
//! | Task Scheduler (Windows) | `<scull data>\schedule\scull-check.xml` | `schtasks /Create /TN \scull\check /XML <xml> /F` | `schtasks /Delete /TN \scull\check /F`, remove |
//!
//! Low priority: launchd `ProcessType=Background`, `LowPriorityIO`, `Nice=10`; systemd
//! `Nice=10`, `CPUSchedulingPolicy=idle`, `IOSchedulingClass=idle`; Task Scheduler priority 7
//! (below normal) with `LeastPrivilege`. Every scheduler runs the job as the user who
//! registered it, never as root or an administrator (`scull schedule` refuses to register from
//! an elevated shell).
//!
//! The scheduled command is `scull check --quiet --no-progress`: read-only apart from scull's
//! own history and its ready-to-review plan. It never deletes anything (unattended cleanup is
//! a separate, later opt-in, D-11).

use scull_platform::scheduler::Scheduler;
use std::path::{Path, PathBuf};

/// The launchd job label (and plist file stem).
pub const LAUNCHD_LABEL: &str = "dev.scull.check";
/// The systemd unit name (without `.service` / `.timer`).
pub const SYSTEMD_UNIT: &str = "scull-check";
/// The Task Scheduler task path.
pub const TASK_NAME: &str = r"\scull\check";
/// The Task Scheduler definition file name (kept in scull's data directory).
pub const TASK_XML: &str = "scull-check.xml";
/// PRD default: every 6 hours.
pub const DEFAULT_INTERVAL_SECS: u64 = 6 * 60 * 60;
/// Shortest interval accepted: 15 minutes.
pub const MIN_INTERVAL_SECS: u64 = 15 * 60;
/// Longest interval accepted: 7 days.
pub const MAX_INTERVAL_SECS: u64 = 7 * 24 * 60 * 60;
/// The arguments the scheduler passes to `scull`.
pub const CHECK_ARGS: [&str; 3] = ["check", "--quiet", "--no-progress"];

/// What to register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleSpec {
    /// The OS scheduler.
    pub scheduler: Scheduler,
    /// The `scull` executable to run (absolute).
    pub exe: PathBuf,
    /// Seconds between checks.
    pub interval_secs: u64,
    /// Where the unit files go ([`unit_dir`]).
    pub unit_dir: PathBuf,
    /// `SCULL_HOME`, passed to the job when set (launchd and systemd; Task Scheduler has no
    /// per-task environment, see [`install_plan`]).
    pub scull_home: Option<PathBuf>,
}

/// A file `scull schedule enable` writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSpec {
    /// Where.
    pub path: PathBuf,
    /// The exact bytes written.
    pub bytes: Vec<u8>,
    /// The same contents as text, for review (identical to `bytes` except for the UTF-16
    /// encoding Task Scheduler expects).
    pub text: String,
}

/// One tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// The tool (absolute path, [`Scheduler::program`]).
    pub program: PathBuf,
    /// Its arguments.
    pub args: Vec<String>,
    /// Whether a failure stops the operation (`false` for best-effort steps such as unloading
    /// a job that may not be loaded).
    pub must_succeed: bool,
}

impl CommandSpec {
    fn new(scheduler: Scheduler, args: &[&str], must_succeed: bool) -> Self {
        Self {
            program: scheduler.program(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            must_succeed,
        }
    }

    /// The command line for display, arguments with spaces or quotes in double quotes.
    pub fn display(&self) -> String {
        std::iter::once(self.program.to_string_lossy().into_owned())
            .chain(self.args.iter().cloned())
            .map(|a| display_quote(&a))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn display_quote(s: &str) -> String {
    if !s.is_empty() && !s.contains(|c: char| c.is_whitespace() || c == '"' || c == '\'') {
        s.to_owned()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// Files to write, then commands to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    /// Written first (each replaces any previous version).
    pub files: Vec<FileSpec>,
    /// Run after the files are written, in order.
    pub commands: Vec<CommandSpec>,
    /// Caveats to show (e.g. `SCULL_HOME` not passed on Windows).
    pub notes: Vec<String>,
}

/// Commands to run, files to remove, commands to run after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovePlan {
    /// Run first (stop and unregister).
    pub before: Vec<CommandSpec>,
    /// Then removed (only the files scull wrote).
    pub remove: Vec<PathBuf>,
    /// Run last (e.g. `systemctl --user daemon-reload`).
    pub after: Vec<CommandSpec>,
}

/// Where the unit files live: `~/Library/LaunchAgents` (launchd);
/// `$XDG_CONFIG_HOME/systemd/user`, else `~/.config/systemd/user` (systemd); `<data>/schedule`
/// (Task Scheduler keeps its definition, scull keeps the XML it registered).
pub fn unit_dir(
    scheduler: Scheduler,
    home: &Path,
    xdg_config_home: Option<&Path>,
    data_dir: &Path,
) -> PathBuf {
    match scheduler {
        Scheduler::Launchd => home.join("Library").join("LaunchAgents"),
        Scheduler::SystemdUser => xdg_config_home
            .filter(|p| p.is_absolute())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".config"))
            .join("systemd")
            .join("user"),
        Scheduler::TaskScheduler => data_dir.join("schedule"),
    }
}

/// The files scull writes for `scheduler` in `unit_dir` (present after `enable`).
pub fn unit_files(scheduler: Scheduler, unit_dir: &Path) -> Vec<PathBuf> {
    match scheduler {
        Scheduler::Launchd => vec![unit_dir.join(format!("{LAUNCHD_LABEL}.plist"))],
        Scheduler::SystemdUser => vec![
            unit_dir.join(format!("{SYSTEMD_UNIT}.service")),
            unit_dir.join(format!("{SYSTEMD_UNIT}.timer")),
        ],
        Scheduler::TaskScheduler => vec![unit_dir.join(TASK_XML)],
    }
}

/// `6 hours`, `90 minutes`, `2 days`.
pub fn interval_text(secs: u64) -> String {
    let unit = |n: u64, u: &str| format!("{n} {u}{}", if n == 1 { "" } else { "s" });
    if secs.is_multiple_of(86_400) {
        unit(secs / 86_400, "day")
    } else if secs.is_multiple_of(3_600) {
        unit(secs / 3_600, "hour")
    } else {
        unit(secs.div_ceil(60), "minute")
    }
}

/// Everything `scull schedule enable` does for `spec`.
pub fn install_plan(spec: &ScheduleSpec) -> InstallPlan {
    let files = unit_files(spec.scheduler, &spec.unit_dir);
    let mut notes = Vec::new();
    let (files, commands) = match spec.scheduler {
        Scheduler::Launchd => {
            let plist = &files[0];
            let p = plist.to_string_lossy();
            (
                vec![text_file(plist, launchd_plist(spec))],
                vec![
                    // A previous version may be loaded; unloading one that isn't fails harmlessly.
                    CommandSpec::new(spec.scheduler, &["unload", &p], false),
                    CommandSpec::new(spec.scheduler, &["load", "-w", &p], true),
                ],
            )
        }
        Scheduler::SystemdUser => {
            let timer = format!("{SYSTEMD_UNIT}.timer");
            (
                vec![
                    text_file(&files[0], systemd_service(spec)),
                    text_file(&files[1], systemd_timer(spec)),
                ],
                vec![
                    CommandSpec::new(spec.scheduler, &["--user", "daemon-reload"], true),
                    CommandSpec::new(spec.scheduler, &["--user", "enable", "--now", &timer], true),
                ],
            )
        }
        Scheduler::TaskScheduler => {
            if spec.scull_home.is_some() {
                notes.push(
                    "SCULL_HOME is set, but Task Scheduler has no per-task environment: the \
                     scheduled check uses the default data folder unless SCULL_HOME is set for \
                     your account"
                        .to_owned(),
                );
            }
            let xml = &files[0];
            let text = task_xml(spec);
            (
                vec![FileSpec {
                    path: xml.clone(),
                    bytes: utf16le_with_bom(&text),
                    text,
                }],
                vec![CommandSpec::new(
                    spec.scheduler,
                    &[
                        "/Create",
                        "/TN",
                        TASK_NAME,
                        "/XML",
                        &xml.to_string_lossy(),
                        "/F",
                    ],
                    true,
                )],
            )
        }
    };
    InstallPlan {
        files,
        commands,
        notes,
    }
}

/// Everything `scull schedule disable` does (whether or not the files exist; the caller skips
/// it when none do).
pub fn remove_plan(scheduler: Scheduler, unit_dir: &Path) -> RemovePlan {
    let files = unit_files(scheduler, unit_dir);
    match scheduler {
        Scheduler::Launchd => RemovePlan {
            before: vec![CommandSpec::new(
                scheduler,
                &["unload", "-w", &files[0].to_string_lossy()],
                false,
            )],
            remove: files,
            after: Vec::new(),
        },
        Scheduler::SystemdUser => RemovePlan {
            before: vec![CommandSpec::new(
                scheduler,
                &[
                    "--user",
                    "disable",
                    "--now",
                    &format!("{SYSTEMD_UNIT}.timer"),
                ],
                false,
            )],
            remove: files,
            after: vec![CommandSpec::new(
                scheduler,
                &["--user", "daemon-reload"],
                false,
            )],
        },
        Scheduler::TaskScheduler => RemovePlan {
            before: vec![CommandSpec::new(
                scheduler,
                &["/Delete", "/TN", TASK_NAME, "/F"],
                false,
            )],
            remove: files,
            after: Vec::new(),
        },
    }
}

/// The read-only query `scull schedule status` runs when the files are present.
pub fn status_command(scheduler: Scheduler) -> CommandSpec {
    match scheduler {
        Scheduler::Launchd => CommandSpec::new(scheduler, &["list", LAUNCHD_LABEL], false),
        Scheduler::SystemdUser => CommandSpec::new(
            scheduler,
            &["--user", "is-enabled", &format!("{SYSTEMD_UNIT}.timer")],
            false,
        ),
        Scheduler::TaskScheduler => {
            CommandSpec::new(scheduler, &["/Query", "/TN", TASK_NAME], false)
        }
    }
}

fn text_file(path: &Path, text: String) -> FileSpec {
    FileSpec {
        path: path.to_path_buf(),
        bytes: text.clone().into_bytes(),
        text,
    }
}

fn utf16le_with_bom(s: &str) -> Vec<u8> {
    let mut out = vec![0xFF, 0xFE];
    for u in s.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out
}

/// `&`, `<`, `>`, `"`, `'` as XML entities.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// One systemd `ExecStart=` word: double-quoted, with `\`, `"` escaped and `%`, `$` doubled
/// (systemd specifiers and variable expansion).
pub fn systemd_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The LaunchAgent property list.
pub fn launchd_plist(spec: &ScheduleSpec) -> String {
    let mut args = format!(
        "    <string>{}</string>\n",
        xml_escape(&spec.exe.to_string_lossy())
    );
    for a in CHECK_ARGS {
        args.push_str(&format!("    <string>{a}</string>\n"));
    }
    let env = spec
        .scull_home
        .as_ref()
        .map(|h| {
            format!(
                "  <key>EnvironmentVariables</key>\n  <dict>\n    <key>SCULL_HOME</key>\n    \
                 <string>{}</string>\n  </dict>\n",
                xml_escape(&h.to_string_lossy())
            )
        })
        .unwrap_or_default();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- scull check every {every}: free-space forecast and alerts. Never deletes anything.\n     \
         Written by `scull schedule enable`; remove with `scull schedule disable`. -->\n\
         <plist version=\"1.0\">\n<dict>\n\
         \x20 <key>Label</key>\n  <string>{LAUNCHD_LABEL}</string>\n\
         \x20 <key>ProgramArguments</key>\n  <array>\n{args}  </array>\n\
         {env}\
         \x20 <key>StartInterval</key>\n  <integer>{secs}</integer>\n\
         \x20 <key>RunAtLoad</key>\n  <false/>\n\
         \x20 <key>ProcessType</key>\n  <string>Background</string>\n\
         \x20 <key>LowPriorityIO</key>\n  <true/>\n\
         \x20 <key>Nice</key>\n  <integer>10</integer>\n\
         \x20 <key>StandardOutPath</key>\n  <string>/dev/null</string>\n\
         \x20 <key>StandardErrorPath</key>\n  <string>/dev/null</string>\n\
         </dict>\n</plist>\n",
        every = interval_text(spec.interval_secs),
        secs = spec.interval_secs,
    )
}

/// The systemd user service (a one-shot `scull check` at idle priority).
pub fn systemd_service(spec: &ScheduleSpec) -> String {
    let mut exec = systemd_quote(&spec.exe.to_string_lossy());
    for a in CHECK_ARGS {
        exec.push(' ');
        exec.push_str(a);
    }
    let env = spec
        .scull_home
        .as_ref()
        .map(|h| {
            format!(
                "Environment={}\n",
                systemd_quote(&format!("SCULL_HOME={}", h.to_string_lossy()))
            )
        })
        .unwrap_or_default();
    format!(
        "# scull check: free-space forecast and alerts. Never deletes anything.\n\
         # Written by `scull schedule enable`; remove with `scull schedule disable`.\n\
         [Unit]\n\
         Description=scull check (free-space forecast and alerts, read-only)\n\n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={exec}\n\
         {env}\
         Nice=10\n\
         CPUSchedulingPolicy=idle\n\
         IOSchedulingClass=idle\n"
    )
}

/// The systemd user timer (every interval, starting 15 minutes after login).
pub fn systemd_timer(spec: &ScheduleSpec) -> String {
    format!(
        "# Runs {SYSTEMD_UNIT}.service every {every}. Written by `scull schedule enable`.\n\
         [Unit]\n\
         Description=Run scull check every {every}\n\n\
         [Timer]\n\
         OnStartupSec=15min\n\
         OnUnitActiveSec={secs}s\n\
         RandomizedDelaySec=5min\n\
         Unit={SYSTEMD_UNIT}.service\n\n\
         [Install]\n\
         WantedBy=timers.target\n",
        every = interval_text(spec.interval_secs),
        secs = spec.interval_secs,
    )
}

/// The Task Scheduler definition (`schtasks /XML`): a repeating time trigger, the current
/// user's interactive token at least privilege, priority 7, at most one instance.
pub fn task_xml(spec: &ScheduleSpec) -> String {
    let minutes = spec.interval_secs.div_ceil(60);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\r\n\
         <Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\r\n\
         \x20 <RegistrationInfo>\r\n\
         \x20   <Description>scull check every {every}: free-space forecast and alerts. Never deletes anything. Remove with scull schedule disable.</Description>\r\n\
         \x20   <URI>{task}</URI>\r\n\
         \x20 </RegistrationInfo>\r\n\
         \x20 <Triggers>\r\n\
         \x20   <TimeTrigger>\r\n\
         \x20     <Repetition>\r\n\
         \x20       <Interval>PT{minutes}M</Interval>\r\n\
         \x20       <StopAtDurationEnd>false</StopAtDurationEnd>\r\n\
         \x20     </Repetition>\r\n\
         \x20     <StartBoundary>2020-01-01T00:00:00</StartBoundary>\r\n\
         \x20     <Enabled>true</Enabled>\r\n\
         \x20   </TimeTrigger>\r\n\
         \x20 </Triggers>\r\n\
         \x20 <Principals>\r\n\
         \x20   <Principal id=\"Author\">\r\n\
         \x20     <LogonType>InteractiveToken</LogonType>\r\n\
         \x20     <RunLevel>LeastPrivilege</RunLevel>\r\n\
         \x20   </Principal>\r\n\
         \x20 </Principals>\r\n\
         \x20 <Settings>\r\n\
         \x20   <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\r\n\
         \x20   <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\r\n\
         \x20   <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\r\n\
         \x20   <StartWhenAvailable>true</StartWhenAvailable>\r\n\
         \x20   <ExecutionTimeLimit>PT1H</ExecutionTimeLimit>\r\n\
         \x20   <Priority>7</Priority>\r\n\
         \x20   <Enabled>true</Enabled>\r\n\
         \x20 </Settings>\r\n\
         \x20 <Actions Context=\"Author\">\r\n\
         \x20   <Exec>\r\n\
         \x20     <Command>{exe}</Command>\r\n\
         \x20     <Arguments>{args}</Arguments>\r\n\
         \x20   </Exec>\r\n\
         \x20 </Actions>\r\n\
         </Task>\r\n",
        every = interval_text(spec.interval_secs),
        task = xml_escape(TASK_NAME),
        exe = xml_escape(&spec.exe.to_string_lossy()),
        args = CHECK_ARGS.join(" "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(scheduler: Scheduler, exe: &str, home: Option<&str>) -> ScheduleSpec {
        ScheduleSpec {
            scheduler,
            exe: PathBuf::from(exe),
            interval_secs: DEFAULT_INTERVAL_SECS,
            unit_dir: PathBuf::from("/u"),
            scull_home: home.map(PathBuf::from),
        }
    }

    #[test]
    fn unit_dirs_follow_each_os() {
        let home = Path::new("/home/me");
        let data = Path::new("/data");
        assert_eq!(
            unit_dir(Scheduler::Launchd, home, None, data),
            Path::new("/home/me/Library/LaunchAgents")
        );
        assert_eq!(
            unit_dir(Scheduler::SystemdUser, home, None, data),
            Path::new("/home/me/.config/systemd/user")
        );
        // An absolute XDG_CONFIG_HOME by the host's rules (only Linux uses systemd; a Windows
        // host checking this needs a drive letter).
        let xdg = if cfg!(windows) {
            Path::new(r"C:\x\cfg")
        } else {
            Path::new("/x/cfg")
        };
        assert_eq!(
            unit_dir(Scheduler::SystemdUser, home, Some(xdg), data),
            xdg.join("systemd").join("user")
        );
        // A relative XDG_CONFIG_HOME is invalid per the spec and ignored.
        assert_eq!(
            unit_dir(Scheduler::SystemdUser, home, Some(Path::new("rel")), data),
            Path::new("/home/me/.config/systemd/user")
        );
        assert_eq!(
            unit_dir(Scheduler::TaskScheduler, home, None, data),
            Path::new("/data/schedule")
        );
    }

    #[test]
    fn launchd_agent_is_low_priority_and_escaped() {
        let s = spec(Scheduler::Launchd, "/Apps/A & B/scull", Some("/h/<s>"));
        let plan = install_plan(&s);
        assert_eq!(plan.files.len(), 1);
        let f = &plan.files[0];
        assert_eq!(f.path, Path::new("/u/dev.scull.check.plist"));
        assert_eq!(f.bytes, f.text.as_bytes());
        for needle in [
            "<string>dev.scull.check</string>",
            "<string>/Apps/A &amp; B/scull</string>",
            "<string>check</string>",
            "<string>--quiet</string>",
            "<integer>21600</integer>",
            "<string>Background</string>",
            "<key>LowPriorityIO</key>\n  <true/>",
            "<key>Nice</key>\n  <integer>10</integer>",
            "<string>/h/&lt;s&gt;</string>",
        ] {
            assert!(f.text.contains(needle), "{needle}\n{}", f.text);
        }
        let plist = Path::new("/u").join("dev.scull.check.plist");
        let plist = plist.to_string_lossy();
        let cmds: Vec<String> = plan.commands.iter().map(CommandSpec::display).collect();
        assert_eq!(
            cmds,
            vec![
                format!("/bin/launchctl unload {plist}"),
                format!("/bin/launchctl load -w {plist}")
            ]
        );
        assert!(!plan.commands[0].must_succeed && plan.commands[1].must_succeed);
        let rm = remove_plan(Scheduler::Launchd, Path::new("/u"));
        assert_eq!(rm.remove, vec![PathBuf::from("/u/dev.scull.check.plist")]);
        assert_eq!(
            rm.before[0].display(),
            format!("/bin/launchctl unload -w {plist}")
        );
        assert_eq!(
            status_command(Scheduler::Launchd).display(),
            "/bin/launchctl list dev.scull.check"
        );
    }

    #[test]
    fn systemd_units_are_idle_priority_and_quoted() {
        let s = spec(Scheduler::SystemdUser, "/opt/my tools/scull%1$", Some("/h"));
        let plan = install_plan(&s);
        let names: Vec<_> = plan.files.iter().map(|f| f.path.clone()).collect();
        assert_eq!(
            names,
            vec![
                PathBuf::from("/u/scull-check.service"),
                PathBuf::from("/u/scull-check.timer")
            ]
        );
        let service = &plan.files[0].text;
        assert!(
            service
                .contains("ExecStart=\"/opt/my tools/scull%%1$$\" check --quiet --no-progress\n"),
            "{service}"
        );
        assert!(
            service.contains("Environment=\"SCULL_HOME=/h\"\n"),
            "{service}"
        );
        for needle in [
            "Type=oneshot",
            "Nice=10",
            "CPUSchedulingPolicy=idle",
            "IOSchedulingClass=idle",
        ] {
            assert!(service.contains(needle), "{needle}");
        }
        let timer = &plan.files[1].text;
        for needle in [
            "OnUnitActiveSec=21600s",
            "Unit=scull-check.service",
            "WantedBy=timers.target",
            "every 6 hours",
        ] {
            assert!(timer.contains(needle), "{needle}\n{timer}");
        }
        let cmds: Vec<Vec<String>> = plan.commands.iter().map(|c| c.args.clone()).collect();
        assert_eq!(
            cmds,
            vec![
                vec!["--user".to_owned(), "daemon-reload".into()],
                vec![
                    "--user".into(),
                    "enable".into(),
                    "--now".into(),
                    "scull-check.timer".into()
                ],
            ]
        );
        let rm = remove_plan(Scheduler::SystemdUser, Path::new("/u"));
        assert_eq!(rm.remove.len(), 2);
        assert_eq!(
            rm.before[0].args,
            ["--user", "disable", "--now", "scull-check.timer"]
        );
        assert_eq!(rm.after[0].args, ["--user", "daemon-reload"]);
    }

    #[test]
    fn task_xml_is_utf16_least_privilege_and_notes_scull_home() {
        let s = spec(
            Scheduler::TaskScheduler,
            r"C:\Program Files\scull\scull.exe",
            Some("D:\\s"),
        );
        let plan = install_plan(&s);
        let f = &plan.files[0];
        assert_eq!(f.path, Path::new("/u").join(TASK_XML));
        assert_eq!(&f.bytes[..2], &[0xFF, 0xFE], "UTF-16LE BOM");
        let decoded: Vec<u16> = f.bytes[2..]
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(String::from_utf16(&decoded).expect("utf-16"), f.text);
        for needle in [
            "encoding=\"UTF-16\"",
            "<Interval>PT360M</Interval>",
            "<RunLevel>LeastPrivilege</RunLevel>",
            "<LogonType>InteractiveToken</LogonType>",
            "<Priority>7</Priority>",
            "<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
            r"<Command>C:\Program Files\scull\scull.exe</Command>",
            "<Arguments>check --quiet --no-progress</Arguments>",
            r"<URI>\scull\check</URI>",
        ] {
            assert!(f.text.contains(needle), "{needle}\n{}", f.text);
        }
        assert_eq!(plan.commands.len(), 1);
        assert_eq!(
            plan.commands[0].args,
            [
                "/Create",
                "/TN",
                TASK_NAME,
                "/XML",
                &f.path.to_string_lossy(),
                "/F"
            ]
        );
        assert_eq!(plan.notes.len(), 1, "SCULL_HOME can't be passed to a task");
        let rm = remove_plan(Scheduler::TaskScheduler, Path::new("/u"));
        assert_eq!(rm.before[0].args, ["/Delete", "/TN", TASK_NAME, "/F"]);
        assert_eq!(
            status_command(Scheduler::TaskScheduler).args,
            ["/Query", "/TN", TASK_NAME]
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(interval_text(21_600), "6 hours");
        assert_eq!(interval_text(86_400), "1 day");
        assert_eq!(interval_text(900), "15 minutes");
        assert_eq!(interval_text(5_400), "90 minutes");
        assert_eq!(xml_escape("a<&>\"'"), "a&lt;&amp;&gt;&quot;&apos;");
        assert_eq!(systemd_quote(r#"a\b"c"#), r#""a\\b\"c""#);
        assert_eq!(display_quote("plain"), "plain");
        assert_eq!(display_quote("two words"), "\"two words\"");
    }
}
