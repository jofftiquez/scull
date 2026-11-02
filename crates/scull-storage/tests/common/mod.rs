//! Shared helpers for scull-storage integration tests. Everything lives in temp dirs.
#![allow(dead_code, clippy::unwrap_used)]

use scull_core::{
    Action, Candidate, Category, FileIdentity, Flags, FsPath, ItemId, Plan, PlanId, PlanItem,
    PlanOrigin, PlanParams, ReasonCode, Reclaimable, Regeneration, RuleRef, ScanId, Sizes, Tier,
    TierReason,
};
use scull_storage::{OwnerOnly, ScanStart, Store};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const T0: i64 = 1_800_000_000;

/// Test implementation of the owner-only hooks. On Unix it really sets 0600/0700 (test code
/// may use `std::os::unix`; production code gets this from `scull-platform`). Elsewhere it
/// falls back to OS defaults. Records every call.
#[derive(Default)]
pub struct TestPerms {
    pub calls: Mutex<Vec<(String, PathBuf)>>,
}

impl TestPerms {
    pub fn calls(&self) -> Vec<(String, PathBuf)> {
        self.calls.lock().unwrap().clone()
    }

    fn log(&self, what: &str, path: &Path) {
        self.calls
            .lock()
            .unwrap()
            .push((what.to_owned(), path.to_owned()));
    }
}

impl OwnerOnly for TestPerms {
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.log("create_dir", path);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).create(path)
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir(path)
        }
    }

    fn create_file(&self, path: &Path) -> io::Result<()> {
        self.log("create_file", path);
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        o.open(path).map(drop)
    }

    fn restrict(&self, path: &Path) -> io::Result<()> {
        self.log("restrict", path);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::symlink_metadata(path)?;
            let mode = meta.permissions().mode() & 0o700;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Ok(())
        }
    }
}

pub fn db_in(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join(scull_storage::DB_FILE_NAME)
}

pub fn open(path: &Path) -> Store {
    Store::open(path, &TestPerms::default()).unwrap()
}

pub fn scan_id(n: u32) -> ScanId {
    ScanId::from_suffix(&format!("S{n}")).unwrap()
}

pub fn plan_id(n: u32) -> PlanId {
    PlanId::from_suffix(&format!("P{n}")).unwrap()
}

pub fn candidate(n: u128, tier: Tier, action: Action) -> Candidate {
    let identity = FileIdentity::new(7, n);
    let path = format!("/work/p{n}/target");
    Candidate {
        id: ItemId::derive(identity, path.as_bytes()),
        path: FsPath::from(path.as_str()),
        identity,
        category: Category::ProjectArtifact,
        artifact: "Cargo build output".into(),
        rule: RuleRef {
            id: "rust.target".into(),
            version: 1,
        },
        project: None,
        tier,
        reasons: vec![TierReason::new(tier, ReasonCode::Regenerable, "test")],
        evidence: vec![],
        sizes: Sizes {
            logical: 1000,
            allocated: 1024,
            reclaimable: Reclaimable {
                bytes: 900 + n as u64,
                confidence: scull_core::Confidence::High,
                shared_bytes: 0,
            },
        },
        flags: Flags::default(),
        regeneration: Regeneration::default(),
        action,
    }
}

pub fn candidates() -> Vec<Candidate> {
    vec![
        candidate(1, Tier::S0, Action::Delete),
        candidate(2, Tier::S1, Action::Quarantine),
        candidate(3, Tier::S2, Action::Trash),
    ]
}

/// An MCP plan (15-minute expiry) created at `T0`.
pub fn plan(n: u32) -> Plan {
    plan_of(n, &candidates())
}

/// An MCP plan with `count` S0 delete items.
pub fn plan_with_items(n: u32, count: u128) -> Plan {
    let cs: Vec<Candidate> = (1..=count)
        .map(|i| candidate(i, Tier::S0, Action::Delete))
        .collect();
    plan_of(n, &cs)
}

fn plan_of(n: u32, candidates: &[Candidate]) -> Plan {
    let items = candidates
        .iter()
        .map(|c| PlanItem::from_candidate(c).unwrap())
        .collect();
    Plan::new(
        PlanParams {
            id: plan_id(n),
            scan_id: scan_id(1),
            created_unix: T0,
            ttl_secs: None,
            machine_id: "machine-a".into(),
            user: "dev".into(),
            origin: PlanOrigin::Mcp,
            operations_watermark: None,
        },
        items,
    )
    .unwrap()
}

pub fn start_scan(store: &mut Store, n: u32) {
    store
        .record_scan_start(&ScanStart {
            scan_id: scan_id(n),
            started_at: T0,
            roots: vec![FsPath::from("/work")],
        })
        .unwrap();
}

/// Raw connection for assertions the typed API deliberately does not offer.
pub fn raw(path: &Path) -> rusqlite::Connection {
    let c = rusqlite::Connection::open(path).unwrap();
    c.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    c
}
