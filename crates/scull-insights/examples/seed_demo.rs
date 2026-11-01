//! Seed a throwaway scull home with the demo history (for screenshots and manual checks):
//!
//! ```text
//! cargo run -p scull-insights --features fixtures --example seed_demo -- <SCULL_HOME>
//! ```
//!
//! Writes `<SCULL_HOME>/data/scull.db` only, and refuses a home that already has one.
#![allow(clippy::unwrap_used)]

use scull_storage::{DB_FILE_NAME, DefaultPermissions, Store};
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let home = std::env::args_os()
        .nth(1)
        .expect("usage: seed_demo <SCULL_HOME>");
    let db = std::path::Path::new(&home).join("data").join(DB_FILE_NAME);
    assert!(!db.exists(), "{} already exists", db.display());
    let now = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    let mut store = Store::open(&db, &DefaultPermissions).unwrap();
    scull_insights::fixtures::seed_demo(&mut store, now).unwrap();
    println!("seeded {}", db.display());
}
