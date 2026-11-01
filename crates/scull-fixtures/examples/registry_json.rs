//! Print the fixture registry ([`scull_fixtures::registry::REGISTRY`]) as JSON on stdout, for
//! the MVP release gate (`packaging/check_mvp_gates.py fixtures`), which decides from it which
//! fixtures must run and which still block the release.
//!
//! `cargo run -q -p scull-fixtures --example registry_json`

use scull_fixtures::registry::{Gate, Mode, REGISTRY, Status};
use serde_json::{Value, json};

fn main() {
    let fixtures: Vec<Value> = REGISTRY
        .iter()
        .map(|f| {
            let tests: Vec<Value> = f
                .status
                .tests()
                .iter()
                .map(|t| json!({ "file": t.file, "test_fn": t.test_fn }))
                .collect();
            let (status, issue, reason) = match f.status {
                Status::Implemented(_) => ("implemented", None, None),
                Status::Tracked { issue, reason, .. } => ("tracked", Some(issue), Some(reason)),
                Status::ManualChecklist => ("manual_checklist", None, None),
                Status::Deferred => ("deferred", None, None),
            };
            json!({
                "id": f.id,
                "gate": match f.gate { Gate::Mvp => "MVP", Gate::V1_0 => "v1.0", Gate::V1x => "v1.x" },
                "mode": match f.mode { Mode::Ci => "CI", Mode::Nightly => "Nightly", Mode::Manual => "Manual" },
                "os": f.os_letters().iter().collect::<String>(),
                "status": status,
                "issue": issue,
                "reason": reason,
                "tests": tests,
                "pending_ci": f.pending_ci,
                "missing_os": f.missing_os,
            })
        })
        .collect();
    let doc = json!({ "fixtures": fixtures });
    println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
}
