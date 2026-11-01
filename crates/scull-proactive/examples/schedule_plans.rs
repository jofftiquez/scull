//! Print what `scull schedule enable` would register on each OS (unit files and scheduler
//! commands), for review on any machine: `cargo run -p scull-proactive --example
//! schedule_plans`. Prints only; writes and runs nothing.

use scull_platform::scheduler::Scheduler;
use scull_proactive::schedule::{DEFAULT_INTERVAL_SECS, ScheduleSpec, install_plan, unit_dir};
use std::path::{Path, PathBuf};

fn main() {
    let cases = [
        (
            Scheduler::Launchd,
            "/Users/me",
            "/Applications/scull/bin/scull",
        ),
        (
            Scheduler::SystemdUser,
            "/home/me",
            "/home/me/.cargo/bin/scull",
        ),
        (
            Scheduler::TaskScheduler,
            r"C:\Users\me",
            r"C:\Users\me\AppData\Local\scull\bin\scull.exe",
        ),
    ];
    for (scheduler, home, exe) in cases {
        let home = Path::new(home);
        let data = home.join("scull-data");
        let spec = ScheduleSpec {
            scheduler,
            exe: PathBuf::from(exe),
            interval_secs: DEFAULT_INTERVAL_SECS,
            unit_dir: unit_dir(scheduler, home, None, &data),
            scull_home: None,
        };
        let plan = install_plan(&spec);
        println!("=== {} ===", scheduler.as_str());
        for f in &plan.files {
            println!("--- {} ({} bytes)", f.path.display(), f.bytes.len());
            print!("{}", f.text.replace("\r\n", "\n"));
        }
        for c in &plan.commands {
            println!("$ {}", c.display());
        }
        println!();
    }
}
