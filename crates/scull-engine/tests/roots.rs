//! FX-MAC-03 (#249): a folder reached through two paths that `canonicalize` keeps apart is
//! scanned and counted once.
//!
//! On macOS `/System/Volumes/Data/Users/<me>/x` and `~/x` are the same folder (a firmlink),
//! but `realpath` returns both unchanged, so path-only root dedupe walked it twice and the
//! candidate IDs (which include the path) differed: totals counted it twice. The engine now
//! also compares roots by file identity (volume + file ID).
//!
//! Firmlinks exist only on macOS, and Linux/Windows can't make a second, non-symlink path to a
//! folder without privileges (bind mounts), so the alias is simulated behind the engine's
//! [`Platform`] seam on every OS: [`Aliased`] serves a path prefix that doesn't exist on disk
//! from a real fixture folder, with the real folder's identity, the way a firmlink does. On
//! macOS a second test uses the real `/System/Volumes/Data` alias of the temp directory.
//! Everything runs in fixture temp dirs; nothing is modified (I-2, I-3 checked).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_core::config::Config;
use scull_engine::paths::canon;
use scull_engine::{
    CancelToken, Engine, EngineConfig, FixedInUse, InUseSource, ScanReport, ScanRequest, now_unix,
};
use scull_fixtures::{Fixture, InvariantInputs};
use scull_platform::{Capabilities, DirIter, Metadata, Native, Platform};
use scull_safety::Tri;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const THREE_DAYS: i64 = 3 * 24 * 3600;

/// [`Native`], except that paths under `alias` are served from `target`: a second path to the
/// same folder, with the same identity (a firmlink, as far as the engine can tell).
struct Aliased {
    alias: PathBuf,
    target: PathBuf,
}

impl Aliased {
    fn real(&self, path: &Path) -> PathBuf {
        match path.strip_prefix(&self.alias) {
            Ok(rest) if rest.as_os_str().is_empty() => self.target.clone(),
            Ok(rest) => self.target.join(rest),
            Err(_) => path.to_path_buf(),
        }
    }
}

impl Platform for Aliased {
    fn capabilities(&self) -> Capabilities {
        Native.capabilities()
    }

    fn metadata(&self, path: &Path) -> scull_platform::Result<Metadata> {
        Native.metadata(&self.real(path))
    }

    fn read_dir<'a>(&'a self, path: &Path) -> scull_platform::Result<DirIter<'a>> {
        Native.read_dir(&self.real(path))
    }
}

fn engine(platform: Arc<dyn Platform>) -> Engine {
    let mut cfg = EngineConfig::new(Config::default(), platform);
    // Temp dirs are tmpfs on some machines; the D-22 volume policy is tested in scull-platform.
    cfg.allow_network_roots = true;
    cfg.in_use = InUseSource::Probe(Arc::new(FixedInUse(Tri::No)));
    Engine::new(cfg).unwrap()
}

fn scan(e: &Engine, roots: &[PathBuf]) -> ScanReport {
    e.scan(
        roots,
        &ScanRequest::full()
            .history(false)
            .at(now_unix() + THREE_DAYS),
        &CancelToken::new(),
        |_| {},
    )
    .unwrap()
}

/// Two Cargo projects with `target/` folders, aged past the activity window.
fn fixture() -> Fixture {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 64 * 1024)
        .unwrap()
        .cargo_project("lib", "lib")
        .unwrap()
        .cargo_target("lib", "lib", 32 * 1024)
        .unwrap();
    fx.checkpoint().unwrap();
    fx
}

/// What must not change when a root is also given through an alias: the roots actually
/// scanned, every candidate (ID, path, size, tier) and the totals.
fn assert_same_as_single(single: &ScanReport, both: &ScanReport) {
    assert_eq!(both.roots, single.roots, "the alias root is dropped");
    let view = |r: &ScanReport| {
        r.candidates
            .iter()
            .map(|c| {
                (
                    c.id.to_string(),
                    c.path.to_string(),
                    c.sizes.logical,
                    c.tier,
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(view(both), view(single), "candidates are not duplicated");
    assert_eq!(both.totals, single.totals, "totals count each folder once");
    assert!(!single.candidates.is_empty(), "the fixture has candidates");
}

/// FX-MAC-03 on every OS: the root given twice, once through an identity alias (in either
/// order), scans exactly like the root alone.
#[test]
fn fx_mac_03_firmlinked_root_is_not_counted_twice() {
    let fx = fixture();
    // Canonical spellings, as the engine compares roots (the temp dir can be a short 8.3
    // name such as `C:\Users\RUNNER~1` on Windows, which the engine spells out in full).
    let root = canon(fx.root());
    // Doesn't exist on disk: only the platform seam knows it (like a firmlink to `root`).
    let alias = canon(fx.base()).join("System-Volumes-Data-alias");
    let platform: Arc<dyn Platform> = Arc::new(Aliased {
        alias: alias.clone(),
        target: root.clone(),
    });
    let e = engine(platform);
    let single = scan(&e, std::slice::from_ref(&root));
    assert_eq!(single.roots.len(), 1);

    assert_eq!(
        e.prepare_roots(&[root.clone(), alias.clone()]).unwrap(),
        e.prepare_roots(std::slice::from_ref(&root)).unwrap(),
        "same identity: one root"
    );
    let both = scan(&e, &[root.clone(), alias.clone()]);
    assert_same_as_single(&single, &both);

    // The first of two aliases is kept. (Only the engine's platform seam knows the simulated
    // alias, while detectors read manifests from disk, so a scan *through* it finds nothing;
    // the macOS test below scans through a real one.)
    assert_eq!(
        e.prepare_roots(&[alias.clone(), root.clone()]).unwrap(),
        vec![alias.clone()]
    );

    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// FX-MAC-03, nested: a root inside another root *through the alias* (`alias/app` inside
/// `root`) is dropped like a path-nested one, whichever order they come in.
#[test]
fn fx_mac_03_root_nested_through_an_alias_is_not_counted_twice() {
    let fx = fixture();
    let root = canon(fx.root());
    let alias = canon(fx.base()).join("System-Volumes-Data-alias");
    let e = engine(Arc::new(Aliased {
        alias: alias.clone(),
        target: root.clone(),
    }));
    let single = scan(&e, std::slice::from_ref(&root));
    let inner = alias.join("app");
    for roots in [
        vec![root.clone(), inner.clone()],
        vec![inner.clone(), root.clone()],
    ] {
        assert_eq!(
            e.prepare_roots(&roots).unwrap(),
            vec![root.clone()],
            "{roots:?}"
        );
        assert_same_as_single(&single, &scan(&e, &roots));
    }
    // The other way round: a real subfolder inside the aliased root.
    for roots in [
        vec![alias.clone(), root.join("lib")],
        vec![root.join("lib"), alias.clone()],
    ] {
        assert_eq!(
            e.prepare_roots(&roots).unwrap(),
            vec![alias.clone()],
            "{roots:?}"
        );
    }

    // Distinct folders stay distinct roots.
    let siblings = [root.join("app"), root.join("lib")];
    assert_eq!(e.prepare_roots(&siblings).unwrap(), siblings.to_vec());

    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// FX-MAC-03 for real on macOS: the temp tree through its path and its `/System/Volumes/Data`
/// firmlink alias (`/private/var/...` is firmlinked) scans like the path alone.
#[cfg(target_os = "macos")]
#[test]
fn fx_mac_03_system_volumes_data_alias_is_not_counted_twice() {
    let fx = fixture();
    let root = canon(fx.root());
    let alias = Path::new("/System/Volumes/Data").join(root.strip_prefix("/").unwrap());
    assert!(
        alias.is_dir(),
        "{} should be the firmlinked form of {}",
        alias.display(),
        root.display()
    );
    let e = engine(Arc::new(Native));
    let single = scan(&e, std::slice::from_ref(&root));
    let both = scan(&e, &[root.clone(), alias.clone()]);
    assert_same_as_single(&single, &both);
    let nested = scan(&e, &[root.clone(), alias.join("app")]);
    assert_same_as_single(&single, &nested);
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}
