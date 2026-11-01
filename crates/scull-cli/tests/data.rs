//! `scull data show|export|purge` end to end (P2-CLI-04, FX-DATA-04).
//!
//! Every run points `SCULL_HOME` at a fresh temp dir. Purge tests build that home inside a
//! `scull-fixtures` fixture (the canary directory sits beside it) and check the safety
//! invariants afterwards: only scull's own files under `data/` may change, the canary never.

use scull_core::{FsPath, ScanId};
use scull_fixtures::{Fixture, InvariantInputs};
use scull_storage::{
    DefaultPermissions, ScanFinish, ScanRecord, ScanStart, ScanStatus, ScanTotals, Store,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

const BACKUP: &str = "scull.db.backup-20260101T000000Z-v1";
const DAMAGED: &str = "scull.db.damaged-20260102T000000Z";

fn scull_in(home: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scull"));
    cmd.args(args).env("SCULL_HOME", home).env("TERM", "dumb");
    for (k, _) in std::env::vars() {
        if k.starts_with("SCULL_") && k != "SCULL_HOME" {
            cmd.env_remove(k);
        }
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.output().expect("run scull")
}

fn db_path(home: &Path) -> PathBuf {
    home.join("data").join(scull_storage::DB_FILE_NAME)
}

/// A database with one recorded scan.
fn seed(home: &Path) {
    let mut store = Store::open(&db_path(home), &DefaultPermissions).expect("open");
    let scan_id = ScanId::from_suffix("s1").expect("id");
    store
        .record_scan_start(&ScanStart {
            scan_id: scan_id.clone(),
            started_at: 1_700_000_000,
            roots: vec![FsPath::from("/home/u/Projects, \"quoted\"")],
        })
        .expect("start");
    store
        .record_scan(&ScanRecord {
            finish: ScanFinish {
                scan_id,
                finished_at: 1_700_000_060,
                status: ScanStatus::Completed,
                totals: ScanTotals {
                    entries_scanned: 10,
                    candidates: 0,
                    logical_bytes: 0,
                    allocated_bytes: 0,
                    reclaimable_bytes: 0,
                    errors: 0,
                },
                rollups: vec![],
            },
            candidates: &[],
            volumes: &[],
        })
        .expect("record");
}

fn schema_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("schemas/{name}.schema.json"))
}

fn validate(name: &str, v: &Value) {
    let schema: Value =
        serde_json::from_str(&std::fs::read_to_string(schema_path(name)).expect("schema file"))
            .expect("schema JSON");
    let validator = jsonschema::validator_for(&schema).expect("compiles");
    let errors: Vec<String> = validator.iter_errors(v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{name}: {errors:?}\n{v:#}");
}

fn json_ok(o: &Output) -> Value {
    assert_eq!(
        o.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    serde_json::from_slice(&o.stdout).expect("JSON")
}

fn error_kind(o: &Output) -> String {
    let v: Value = serde_json::from_slice(&o.stderr).expect("error JSON on stderr");
    v["error"]["kind"].as_str().unwrap_or_default().to_owned()
}

/// Regenerate with `SCULL_BLESS=1 cargo test -p scull-cli --test data`.
#[test]
fn committed_schemas_match_generated() {
    use scull_cli::cmd::data::*;
    use scull_cli::output::json::document_schema;
    let generated = [
        (
            "data-show",
            document_schema::<DataShowReport>(DATA_SHOW_SCHEMA),
        ),
        (
            "data-export",
            document_schema::<DataExportReport>(DATA_EXPORT_SCHEMA),
        ),
        ("data-dump", document_schema::<DataDump>(DATA_DUMP_SCHEMA)),
        (
            "data-purge",
            document_schema::<DataPurgeReport>(DATA_PURGE_SCHEMA),
        ),
    ];
    for (name, schema) in generated {
        let path = schema_path(name);
        let text = serde_json::to_string_pretty(&schema).expect("serialize") + "\n";
        if std::env::var_os("SCULL_BLESS").is_some() {
            std::fs::write(&path, &text).expect("write");
        }
        let committed = std::fs::read_to_string(&path).expect("schema file");
        assert_eq!(
            committed.replace("\r\n", "\n"),
            text,
            "schemas/{name}.schema.json is stale; rerun with SCULL_BLESS=1"
        );
    }
}

#[test]
fn show_lists_tables_files_and_quarantine() {
    let home = tempfile::tempdir().expect("tempdir");
    seed(home.path());
    std::fs::write(home.path().join("data").join(BACKUP), b"backup").expect("backup");

    let v = json_ok(&scull_in(home.path(), &["data", "show", "--json"]));
    validate("data-show", &v);
    assert_eq!(v["schema"], "scull/data-show@1");
    let d = &v["data"];
    assert_eq!(d["database"]["state"], "ok");
    assert_eq!(
        d["database"]["schema_version"],
        scull_storage::SCHEMA_VERSION
    );
    let tables: Vec<&str> = d["tables"]
        .as_array()
        .expect("tables")
        .iter()
        .map(|t| t["table"].as_str().expect("name"))
        .collect();
    assert_eq!(tables, scull_storage::EXPORT_TABLES);
    let scans = d["tables"]
        .as_array()
        .expect("tables")
        .iter()
        .find(|t| t["table"] == "scans")
        .expect("scans");
    assert_eq!(scans["rows"], 1);
    let kinds: Vec<&str> = d["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| f["kind"].as_str().expect("kind"))
        .collect();
    assert!(kinds.contains(&"database"), "{kinds:?}");
    assert!(kinds.contains(&"backup"), "{kinds:?}");
    assert!(d["footprint_bytes"].as_u64().expect("bytes") > 0);
    assert_eq!(d["quarantine"]["known"], true);
    assert_eq!(d["quarantine"]["outstanding_items"], 0);

    let human = scull_in(home.path(), &["data", "show"]);
    assert_eq!(human.status.code(), Some(0));
    let text = String::from_utf8(human.stdout).expect("utf-8");
    assert!(text.contains("scull.db"), "{text}");
    assert!(text.contains("nothing in quarantine"), "{text}");
}

#[test]
fn show_without_a_database_creates_nothing() {
    let home = tempfile::tempdir().expect("tempdir");
    let v = json_ok(&scull_in(home.path(), &["data", "show", "--json"]));
    validate("data-show", &v);
    assert_eq!(v["data"]["database"]["state"], "missing");
    assert_eq!(v["data"]["files"], Value::Array(vec![]));
    assert!(
        !home.path().join("data").exists(),
        "show must not create anything"
    );
}

#[test]
fn export_json_writes_an_owner_only_document_that_validates() {
    let home = tempfile::tempdir().expect("tempdir");
    seed(home.path());
    let out = home.path().join("export");
    let v = json_ok(&scull_in(
        home.path(),
        &[
            "data",
            "export",
            "--out",
            out.to_str().expect("utf-8"),
            "--json",
        ],
    ));
    validate("data-export", &v);
    assert_eq!(v["data"]["format"], "json");
    let file = out.join("scull-data.json");
    assert_eq!(v["data"]["files"][0]["path"], file.to_str().expect("utf-8"));
    scull_platform::ensure_owner_only(&out).expect("owner-only folder");
    scull_platform::ensure_owner_only(&file).expect("owner-only file");

    let dump: Value =
        serde_json::from_str(&std::fs::read_to_string(&file).expect("read")).expect("JSON");
    validate("data-dump", &dump);
    assert_eq!(dump["schema"], "scull/data-dump@1");
    let tables = dump["data"]["tables"].as_array().expect("tables");
    assert_eq!(tables.len(), scull_storage::EXPORT_TABLES.len());
    let scans = tables
        .iter()
        .find(|t| t["table"] == "scans")
        .expect("scans");
    assert_eq!(scans["rows"][0]["scan_id"], "scan_s1");
    // Paths exactly as recorded.
    assert!(
        scans["rows"][0]["roots_json"]
            .as_str()
            .expect("roots")
            .contains("/home/u/Projects, \\\"quoted\\\"")
    );

    // A second export into the same folder never overwrites.
    let before = std::fs::read(&file).expect("read");
    let again = scull_in(
        home.path(),
        &[
            "data",
            "export",
            "--out",
            out.to_str().expect("utf-8"),
            "--json",
        ],
    );
    assert_ne!(again.status.code(), Some(0));
    assert_eq!(std::fs::read(&file).expect("read"), before);
}

#[test]
fn export_csv_writes_one_file_per_table() {
    let home = tempfile::tempdir().expect("tempdir");
    seed(home.path());
    let out = home.path().join("csv");
    let v = json_ok(&scull_in(
        home.path(),
        &[
            "data",
            "export",
            "--format",
            "csv",
            "--out",
            out.to_str().expect("utf-8"),
            "--json",
        ],
    ));
    validate("data-export", &v);
    let files = v["data"]["files"].as_array().expect("files");
    assert_eq!(files.len(), scull_storage::EXPORT_TABLES.len());
    for t in scull_storage::EXPORT_TABLES {
        let p = out.join(format!("{t}.csv"));
        scull_platform::ensure_owner_only(&p).expect("owner-only");
        let text = std::fs::read_to_string(&p).expect("csv");
        assert!(text.ends_with("\r\n"), "{t}: header row at least");
    }
    let scans = std::fs::read_to_string(out.join("scans.csv")).expect("scans");
    let mut lines = scans.split("\r\n");
    assert!(
        lines
            .next()
            .expect("header")
            .starts_with("scan_id,started_at,")
    );
    let row = lines.next().expect("row");
    assert!(row.starts_with("scan_s1,1700000000,1700000060,"), "{row}");
    // A value with commas and quotes is quoted, quotes doubled.
    assert!(
        row.contains(r#""[""/home/u/Projects, \""quoted\""""]""#),
        "{row}"
    );
}

#[test]
fn export_without_a_database_fails_and_creates_nothing() {
    let home = tempfile::tempdir().expect("tempdir");
    let out = home.path().join("out");
    let o = scull_in(
        home.path(),
        &[
            "data",
            "export",
            "--out",
            out.to_str().expect("utf-8"),
            "--json",
        ],
    );
    assert_eq!(o.status.code(), Some(2));
    assert_eq!(error_kind(&o), "config");
    assert!(!out.exists());
    assert!(!db_path(home.path()).exists());
}

/// A fixture whose root is a scull home with a database, a backup, a damaged copy with a side
/// file, the MCP salt, two symlinks named like backups (to the canary file and the canary
/// folder), a user file in the data folder, and a config file.
struct Home {
    fx: Fixture,
    links: bool,
}

impl Home {
    fn new() -> Self {
        let mut fx = Fixture::new().expect("fixture");
        seed(fx.root());
        let canary_file = fx.canary().symlink_target_file.clone();
        let canary_dir = fx.canary().symlink_target_dir.clone();
        fx.file(Path::new("data").join(BACKUP), b"backup")
            .expect("backup")
            .file(Path::new("data").join(DAMAGED), b"damaged")
            .expect("damaged")
            .file(Path::new("data").join(format!("{DAMAGED}-wal")), b"wal")
            .expect("damaged wal")
            .file(Path::new("data").join(scull_mcp::SALT_FILE), [b'a'; 64])
            .expect("salt")
            .file("data/audit/op-1.ndjson", b"{}\n")
            .expect("audit mirror")
            .file("data/notes.txt", b"not scull's")
            .expect("user file")
            .file("config/config.toml", b"[scan]\n")
            .expect("config");
        let a = fx
            .symlink(
                Path::new("data").join("scull.db.backup-20260103T000000Z-v1"),
                &canary_file,
            )
            .expect("file link");
        let b = fx
            .symlink(
                Path::new("data").join("scull.db.backup-20260104T000000Z-v1"),
                &canary_dir,
            )
            .expect("dir link");
        let links = a.is_created() && b.is_created();
        fx.checkpoint().expect("checkpoint");
        Self { fx, links }
    }

    fn home(&self) -> &Path {
        self.fx.root()
    }

    fn data_entries(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.home().join("data"))
            .expect("read data")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn check(&self) {
        self.fx
            .check_invariants(&InvariantInputs::new().allow("data"))
            .assert_ok();
    }

    fn check_unchanged(&self) {
        self.fx
            .check_invariants(&InvariantInputs::new())
            .assert_ok();
    }
}

/// Removal goes through the cleaner's walker, which Windows does not have yet (#68): there the
/// purge must refuse as unsupported with nothing deleted. Returns true when that was checked.
fn no_walker_here(h: &Home, args: &[&str]) -> bool {
    if !cfg!(windows) {
        return false;
    }
    let before = h.data_entries();
    let o = scull_in(h.home(), args);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(error_kind(&o), "unsupported_platform");
    assert_eq!(h.data_entries(), before);
    h.check_unchanged();
    true
}

#[test]
fn fx_data_04_purge_all_removes_exactly_sculls_files() {
    let h = Home::new();
    if no_walker_here(&h, &["data", "purge", "--all", "--confirm", "--json"]) {
        return;
    }
    let v = json_ok(&scull_in(
        h.home(),
        &["data", "purge", "--all", "--confirm", "--json"],
    ));
    validate("data-purge", &v);
    let d = &v["data"];
    assert_eq!(d["scope"], "all");
    let outcomes: Vec<(&str, &str)> = d["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| {
            (
                f["file"]["kind"].as_str().expect("kind"),
                f["outcome"].as_str().expect("outcome"),
            )
        })
        .collect();
    assert!(
        outcomes.iter().all(|(_, o)| *o == "removed"),
        "{outcomes:?}"
    );
    for kind in [
        "database",
        "backup",
        "damaged",
        "side_file",
        "salt",
        "audit_mirror",
    ] {
        assert!(
            outcomes.iter().any(|(k, _)| *k == kind),
            "{kind}: {outcomes:?}"
        );
    }

    // Only the user's file (and the emptied audit folder) is left; the symlinks went as
    // links, their targets stayed.
    assert_eq!(h.data_entries(), ["audit", "notes.txt"]);
    assert_eq!(
        std::fs::read_dir(h.home().join("data/audit"))
            .expect("audit dir")
            .count(),
        0
    );
    assert!(h.fx.canary().symlink_target_file.is_file());
    assert!(h.fx.canary().symlink_target_dir.join("keep.txt").is_file());
    assert_eq!(
        std::fs::read(h.home().join("config/config.toml")).expect("config"),
        b"[scan]\n"
    );
    assert!(scull_storage::footprint_files(&db_path(h.home())).is_empty());
    if !h.links {
        eprintln!(
            "symlinks not permitted here; link cases skipped: {:?}",
            h.fx.skipped()
        );
    }
    h.check();

    // Nothing left: a second purge is a no-op.
    let again = json_ok(&scull_in(
        h.home(),
        &["data", "purge", "--all", "--confirm", "--json"],
    ));
    assert_eq!(again["data"]["files"], Value::Array(vec![]));
}

#[test]
fn purge_needs_confirmation_without_a_terminal() {
    let h = Home::new();
    let before = h.data_entries();
    for args in [
        &["data", "purge", "--all"][..],
        &["data", "purge", "--all", "--json"][..],
        &["data", "purge", "--older-than", "1d"][..],
    ] {
        let o = scull_in(h.home(), args);
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(
            String::from_utf8_lossy(&o.stderr).contains("--confirm"),
            "{args:?}"
        );
    }
    // A scope is required, and only one.
    for args in [
        &["data", "purge", "--confirm"][..],
        &["data", "purge", "--all", "--older-than", "1d", "--confirm"][..],
    ] {
        assert_eq!(scull_in(h.home(), args).status.code(), Some(2), "{args:?}");
    }
    assert_eq!(h.data_entries(), before);
    h.check_unchanged();
}

#[test]
fn purge_refuses_quarantine_contents() {
    let h = Home::new();
    let before = h.data_entries();
    let o = scull_in(
        h.home(),
        &[
            "data",
            "purge",
            "--all",
            "--include-quarantine",
            "--confirm",
            "--json",
        ],
    );
    assert_eq!(o.status.code(), Some(5));
    assert_eq!(error_kind(&o), "safety_blocked");
    assert!(String::from_utf8_lossy(&o.stderr).contains("#71"));
    assert_eq!(h.data_entries(), before);
    h.check_unchanged();
}

#[test]
fn purge_all_refuses_while_items_are_in_quarantine() {
    let h = Home::new();
    {
        // A manifest row as the Executor records it (schema v4 facts included); the plan and
        // operation rows its foreign keys need are skipped here.
        let conn = rusqlite::Connection::open(db_path(h.home())).expect("open");
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF; \
             INSERT INTO quarantine_manifest (operation_id, item_id, original_path, \
               quarantine_path, bytes, quarantined_at, entry_identity, entry_kind, \
               parent_identity, logical_bytes) \
             VALUES ('op1', 'i1', '/p/a', '/vol/.scull-quarantine/op1/a', 7, 1, '7:1', 'dir', \
               '7:2', 7);",
        )
        .expect("insert");
    }
    let before = h.data_entries();

    let show = json_ok(&scull_in(h.home(), &["data", "show", "--json"]));
    validate("data-show", &show);
    assert_eq!(show["data"]["quarantine"]["outstanding_items"], 1);
    assert_eq!(
        show["data"]["quarantine"]["folders"][0],
        "/vol/.scull-quarantine/op1"
    );

    let o = scull_in(h.home(), &["data", "purge", "--all", "--confirm", "--json"]);
    assert_eq!(o.status.code(), Some(5));
    assert_eq!(error_kind(&o), "safety_blocked");
    assert!(String::from_utf8_lossy(&o.stderr).contains("still in quarantine"));
    assert_eq!(h.data_entries(), before);
    h.fx.check_invariants(&InvariantInputs::new().allow("data/scull.db"))
        .assert_ok();
}

#[test]
fn purge_all_refuses_a_database_it_cannot_read() {
    let h = Home::new();
    {
        let conn = rusqlite::Connection::open(db_path(h.home())).expect("open");
        conn.execute_batch("PRAGMA user_version = 999;")
            .expect("newer");
    }
    let before = h.data_entries();
    let o = scull_in(h.home(), &["data", "purge", "--all", "--confirm", "--json"]);
    assert_eq!(o.status.code(), Some(5));
    assert!(String::from_utf8_lossy(&o.stderr).contains("newer scull"));
    assert_eq!(h.data_entries(), before);
    h.fx.check_invariants(&InvariantInputs::new().allow("data/scull.db"))
        .assert_ok();
}

#[test]
fn purge_never_opens_the_database_through_a_linked_side_file() {
    let mut fx = Fixture::new().expect("fixture");
    seed(fx.root());
    let canary = fx.canary().symlink_target_file.clone();
    let made = fx
        .symlink(Path::new("data").join("scull.db-wal"), &canary)
        .expect("link");
    fx.checkpoint().expect("checkpoint");
    if made.is_created() {
        let o = scull_in(
            fx.root(),
            &["data", "purge", "--all", "--confirm", "--json"],
        );
        assert_eq!(o.status.code(), Some(5));
        assert!(String::from_utf8_lossy(&o.stderr).contains("not a regular file"));
        let show = json_ok(&scull_in(fx.root(), &["data", "show", "--json"]));
        assert_eq!(show["data"]["database"]["state"], "unreadable");
    }
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn purge_never_follows_a_linked_audit_folder() {
    let mut fx = Fixture::new().expect("fixture");
    seed(fx.root());
    let canary_dir = fx.canary().symlink_target_dir.clone();
    let made = fx
        .symlink(
            Path::new("data").join(scull_storage::AUDIT_DIR_NAME),
            &canary_dir,
        )
        .expect("link");
    fx.checkpoint().expect("checkpoint");
    if made.is_created() && !cfg!(windows) {
        let o = scull_in(
            fx.root(),
            &["data", "purge", "--all", "--confirm", "--json"],
        );
        // The database goes; the canary files listed through the link are refused.
        assert_eq!(
            o.status.code(),
            Some(3),
            "{}",
            String::from_utf8_lossy(&o.stderr)
        );
        let v: Value = serde_json::from_slice(&o.stdout).expect("report");
        validate("data-purge", &v);
        let refused: Vec<&Value> = v["data"]["files"]
            .as_array()
            .expect("files")
            .iter()
            .filter(|f| f["outcome"] == "refused")
            .collect();
        assert!(!refused.is_empty(), "{v:#}");
        assert!(refused.iter().all(|f| f["file"]["kind"] == "audit_mirror"));
        assert!(canary_dir.join("keep.txt").is_file());
    }
    fx.check_invariants(&InvariantInputs::new().allow("data"))
        .assert_ok();
}

#[test]
fn purge_older_than_removes_only_old_copies() {
    let h = Home::new();
    let old = SystemTime::now() - Duration::from_secs(40 * 86_400);
    for name in [BACKUP, DAMAGED] {
        std::fs::File::options()
            .write(true)
            .open(h.home().join("data").join(name))
            .and_then(|f| f.set_modified(old))
            .expect("age");
    }
    let args = [
        "data",
        "purge",
        "--older-than",
        "30d",
        "--confirm",
        "--json",
    ];
    if no_walker_here(&h, &args) {
        return;
    }
    let v = json_ok(&scull_in(
        h.home(),
        &[
            "data",
            "purge",
            "--older-than",
            "30d",
            "--confirm",
            "--json",
        ],
    ));
    validate("data-purge", &v);
    assert_eq!(v["data"]["older_than_secs"], 30 * 86_400);
    let entries = h.data_entries();
    assert!(!entries.contains(&BACKUP.to_owned()), "{entries:?}");
    assert!(!entries.contains(&DAMAGED.to_owned()), "{entries:?}");
    // The live database, the salt, the new side file and the user's file stay.
    for keep in ["scull.db", scull_mcp::SALT_FILE, "notes.txt"] {
        assert!(entries.contains(&keep.to_owned()), "{keep}: {entries:?}");
    }
    assert!(h.fx.canary().symlink_target_file.is_file());
    h.check();
}

/// `scull data show` reports where new quarantines go (P2-EXEC-13): automatic by default, the
/// configured base (`SCULL_QUARANTINE_BASE` over the config file) and per-volume bases.
#[test]
fn show_reports_the_quarantine_location() {
    let home = tempfile::tempdir().expect("tempdir");
    let d = json_ok(&scull_in(home.path(), &["data", "show", "--json"]));
    assert_eq!(
        d["data"]["quarantine"]["location"]["configured"], false,
        "{d:#}"
    );

    let base = home.path().join("q");
    let vol = home.path().join("vol");
    let vol_base = vol.join("q");
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(
        home.path().join("config").join("config.toml"),
        format!(
            "[quarantine]\nbase = {:?}\n[quarantine.bases]\n{:?} = {:?}\n",
            home.path().join("from-file").to_str().unwrap(),
            vol.to_str().unwrap(),
            vol_base.to_str().unwrap()
        ),
    )
    .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scull"));
    cmd.args(["data", "show", "--json"])
        .env("SCULL_HOME", home.path())
        .env("TERM", "dumb");
    for (k, _) in std::env::vars() {
        if k.starts_with("SCULL_") && k != "SCULL_HOME" {
            cmd.env_remove(k);
        }
    }
    cmd.env("SCULL_QUARANTINE_BASE", &base)
        .stdin(std::process::Stdio::null());
    let d = json_ok(&cmd.output().expect("run scull"));
    let l = &d["data"]["quarantine"]["location"];
    assert_eq!(l["configured"], true, "{d:#}");
    assert_eq!(
        l["base"],
        base.to_str().unwrap(),
        "the environment wins: {d:#}"
    );
    assert_eq!(l["bases"][0]["prefix"], vol.to_str().unwrap(), "{d:#}");
    assert_eq!(l["bases"][0]["base"], vol_base.to_str().unwrap(), "{d:#}");
    assert!(l["error"].is_null(), "{d:#}");

    let text =
        String::from_utf8_lossy(&scull_in(home.path(), &["data", "show"]).stdout).into_owned();
    assert!(text.contains("Quarantine to"), "{text}");
}
