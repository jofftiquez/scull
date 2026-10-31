//! App directories and owner-only creation (P1-PLAT-06), through the public API on every OS.
//! Everything is created inside a fresh temp directory; the real app directories are only
//! *resolved* (never created or written).

use scull_platform::{
    AppDirs, DirLayout, DirSource, ErrorKind, Exposure, HOME_OVERRIDE_ENV, Operation, app_dirs,
    create_dir_owner_only, create_file_owner_only, ensure_owner_only, owner_only,
};
use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

type TestResult = Result<(), Box<dyn Error>>;

/// The native resolver pointed at `root` through `SCULL_HOME`, without touching the real
/// environment.
fn dirs_under(root: &Path) -> Result<AppDirs, Box<dyn Error>> {
    let root = OsString::from(root);
    Ok(AppDirs::resolve(DirLayout::native(), move |source| {
        (source == DirSource::Env(HOME_OVERRIDE_ENV)).then(|| root.clone())
    })?)
}

#[test]
fn native_app_dirs_follow_os_conventions() -> TestResult {
    // Read-only: resolves paths from the real environment, creates nothing.
    let dirs = app_dirs()?;
    for dir in dirs.all() {
        assert!(dir.is_absolute(), "{}", dir.display());
    }
    if std::env::var_os(HOME_OVERRIDE_ENV).is_some_and(|v| !v.is_empty()) {
        return Ok(()); // overridden: covered by the resolver tests
    }
    if cfg!(windows) {
        let local = dirs.data.parent().ok_or("data has a parent")?;
        assert_eq!(local.file_name(), Some("scull".as_ref()));
        assert!(dirs.cache.starts_with(local) && dirs.state.starts_with(local));
        assert_ne!(
            dirs.config.parent(),
            local.parent(),
            "config is roaming, data is local"
        );
        // Known folders and the environment normally agree; compare case-insensitively, as
        // Windows paths are.
        let under = |dir: &Path, base: &OsString| {
            let dir = dir.to_string_lossy().to_lowercase();
            let base = base.to_string_lossy().to_lowercase();
            dir.starts_with(base.trim_end_matches('\\'))
        };
        if let Some(env_local) = std::env::var_os("LOCALAPPDATA") {
            assert!(
                under(&dirs.data, &env_local),
                "{} not under %LOCALAPPDATA%",
                dirs.data.display()
            );
        }
        if let Some(env_roaming) = std::env::var_os("APPDATA") {
            assert!(!under(&dirs.data, &env_roaming), "data must never roam");
            assert!(under(&dirs.config, &env_roaming));
        }
    } else if cfg!(target_os = "macos") {
        assert!(dirs.data.ends_with("Library/Application Support/scull"));
        assert!(dirs.cache.ends_with("Library/Caches/scull"));
        assert!(dirs.state.ends_with("Library/Logs/scull"));
    } else {
        for dir in dirs.all() {
            assert_eq!(dir.file_name(), Some("scull".as_ref()), "{}", dir.display());
        }
    }
    Ok(())
}

#[test]
fn scull_home_dirs_are_created_owner_only() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let dirs = dirs_under(&tmp.path().join("home"))?;
    for dir in dirs.all() {
        assert!(dir.starts_with(tmp.path()));
        create_dir_owner_only(dir)?;
        assert!(fs::symlink_metadata(dir)?.is_dir());
        ensure_owner_only(dir)?;
    }
    // The intermediate `home` folder was created by scull too, so it is owner-only as well.
    ensure_owner_only(&tmp.path().join("home"))?;
    // Idempotent on an existing directory.
    create_dir_owner_only(&dirs.data)?;
    Ok(())
}

#[test]
fn owner_only_file_is_created_new_and_usable() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().join("data");
    create_dir_owner_only(&dir)?;
    let path = dir.join("scull.db");

    let mut file = create_file_owner_only(&path)?;
    file.write_all(b"hello")?;
    file.seek(SeekFrom::Start(0))?;
    let mut back = String::new();
    file.read_to_string(&mut back)?;
    assert_eq!(back, "hello");
    drop(file);
    ensure_owner_only(&path)?;

    // create_new: an existing file is an error and is left untouched.
    let err = create_file_owner_only(&path).expect_err("must not reopen an existing file");
    assert_eq!(err.operation(), Operation::CreateFile);
    assert!(
        matches!(err.kind(), ErrorKind::Io(e) if e.kind() == std::io::ErrorKind::AlreadyExists),
        "{err}"
    );
    assert_eq!(fs::read(&path)?, b"hello");
    Ok(())
}

#[test]
fn file_in_the_way_of_a_directory_is_refused() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let file = tmp.path().join("taken");
    fs::write(&file, b"x")?;

    let err = create_dir_owner_only(&file).expect_err("a file is not a directory");
    assert_eq!(err.operation(), Operation::CreateDir);
    assert!(
        matches!(err.kind(), ErrorKind::NotADirectory { .. }),
        "{err}"
    );

    let err = create_dir_owner_only(&file.join("below")).expect_err("parent is a file");
    assert_eq!(err.operation(), Operation::CreateDir);
    assert_eq!(fs::read(&file)?, b"x");
    Ok(())
}

#[test]
fn ensure_owner_only_reports_missing_path() {
    let tmp = tempfile::tempdir().unwrap();
    let err = ensure_owner_only(&tmp.path().join("nope")).expect_err("missing");
    assert_eq!(err.operation(), Operation::CheckPermissions);
    assert!(err.is_not_found());
}

#[test]
fn exposure_serializes_with_a_kind_tag() -> TestResult {
    let json = serde_json::to_value(Exposure::Mode { mode: 0o644 })?;
    assert_eq!(json, serde_json::json!({ "kind": "mode", "mode": 0o644 }));
    Ok(())
}

#[test]
fn io_primitives_keep_the_error_kind() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().join("d");
    owner_only::create_dir(&dir)?;
    ensure_owner_only(&dir)?;
    let err = owner_only::create_dir(&dir).expect_err("exists");
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    let err = owner_only::create_dir(&tmp.path().join("missing").join("d")).expect_err("no parent");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);

    let file = dir.join("f");
    drop(owner_only::create_file(&file)?);
    ensure_owner_only(&file)?;
    let err = owner_only::create_file(&file).expect_err("exists");
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    Ok(())
}

#[test]
fn restrict_narrows_plain_entries_to_owner_only() -> TestResult {
    // Like a database created by an older build with default permissions (scull-storage
    // restricts the DB and its -wal/-shm files on every open).
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().join("plain-dir");
    fs::create_dir(&dir)?;
    let file = dir.join("scull.db");
    fs::write(&file, b"data")?;

    owner_only::restrict(&file)?;
    owner_only::restrict(&dir)?;
    ensure_owner_only(&file)?;
    ensure_owner_only(&dir)?;
    assert_eq!(fs::read(&file)?, b"data", "contents untouched");
    // Idempotent.
    owner_only::restrict(&file)?;
    ensure_owner_only(&file)?;

    let err = owner_only::restrict(&tmp.path().join("missing")).expect_err("missing");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    Ok(())
}
