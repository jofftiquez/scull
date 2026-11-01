//! Trash fallback fixtures (P2-EXEC-07): `Executor::trash` moves one verified item into a
//! trash directory on its own volume. Every test injects the trash locations
//! ([`TrashLocations`]) into its own `scull-fixtures` temp tree, so the user's real Trash is
//! never touched, keeps a canary beside the root and checks the invariants (I-1..I-8 run; I-2
//! canary untouched, I-3 only the item and the trash directory changed).
//!
//! Fixture IDs (docs/FIXTURES.md): FX-PHY-07 (reported as moved, not freed), FX-LNX-01
//! (no Trash ⇒ blocked), FX-LNX-02 (per-volume `$topdir/.Trash-$uid`, `$topdir/.Trash/$uid`),
//! FX-MAC-05 (a volume's `.Trashes`), FX-WIN-06 (Recycle Bin), and the Trash half of
//! FX-EXEC-03 (read-only parent: Trash impossible too ⇒ blocked, never deleted).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use scull_cleaner::Executor;
use scull_cleaner::trash::{TrashKind, TrashLocations, TrashReport};
use scull_core::{Action, ErrorKind, PlanItem, Restorability, ScullError, Tier};
use scull_fixtures::Fixture;
use std::fs;
use std::path::{Path, PathBuf};

/// A candidate directory `rel` with a few files (12 KiB of content).
fn candidate(fx: &mut Fixture, rel: &str) {
    fx.file(format!("{rel}/a.txt"), b"a")
        .unwrap()
        .file_of_size(format!("{rel}/b.bin"), 8192)
        .unwrap()
        .file_of_size(format!("{rel}/sub/c.bin"), 4096)
        .unwrap()
        .file("keep/outside.txt", b"sibling, never touched")
        .unwrap();
}

fn trash_item(fx: &Fixture, rel: &str, tier: Tier) -> PlanItem {
    plan_item(&fx.root().join(rel), tier, Action::Trash)
}

fn locations(home: Option<PathBuf>, topdir: Option<&Path>) -> TrashLocations {
    TrashLocations {
        home,
        topdir: topdir.map(Path::to_path_buf),
    }
}

fn trash(
    fx: &Fixture,
    item: &PlanItem,
    rel: &str,
    loc: &TrashLocations,
) -> Result<TrashReport, ScullError> {
    let path = fx.root().join(rel);
    Executor::new().trash(item, &target(fx, &path), loc)
}

/// The moved entry is the planned object, in the reported location; the original is gone.
fn assert_moved(fx: &Fixture, item: &PlanItem, rel: &str, r: &TrashReport) {
    let location = r.outcome.location.clone().expect("location known");
    assert_eq!(
        identity(&location),
        item.identity,
        "moved item keeps its identity"
    );
    assert!(
        fs::symlink_metadata(fx.root().join(rel)).is_err(),
        "original path is gone"
    );
    assert_eq!(r.item_id, item.item_id);
    assert_eq!(r.restorability(), Restorability::ViaTrash);
    assert_eq!(r.freed_bytes(), 0, "Trash moves bytes, it never frees them");
    assert_eq!(TrashReport::DISPOSITION, "moved");
}

// ------------------------------------------------------------------------------------------
// Every OS: refusals before anything is touched
// ------------------------------------------------------------------------------------------

/// S3 items, and actions other than Trash/Quarantine, are refused; nothing moves.
#[test]
fn refuses_s3_and_non_trash_actions() {
    let mut fx = Fixture::new().unwrap();
    candidate(&mut fx, "cand");
    fx.checkpoint().unwrap();
    let loc = locations(None, Some(fx.root()));
    for (tier, action) in [
        (Tier::S3, Action::Trash),
        (Tier::S0, Action::Delete),
        (Tier::S1, Action::GitWorktreeRemove),
    ] {
        let item = plan_item(&fx.root().join("cand"), tier, action.clone());
        let e = trash(&fx, &item, "cand", &loc).unwrap_err();
        assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{tier} {action:?}: {e}");
    }
    assert!(fx.root().join("cand/b.bin").exists());
    check(&fx, &[], &[]);
}

/// A path that does not hash to the item's ID is refused (a path cannot be swapped in).
#[test]
fn refuses_a_path_that_is_not_the_items() {
    let mut fx = Fixture::new().unwrap();
    candidate(&mut fx, "cand");
    candidate(&mut fx, "other");
    fx.checkpoint().unwrap();
    let item = trash_item(&fx, "cand", Tier::S1);
    let loc = locations(None, Some(fx.root()));
    let e = trash(&fx, &item, "other", &loc).unwrap_err();
    assert_eq!(e.kind, ErrorKind::PlanInvalidated, "{e}");
    check(&fx, &[&item], &[]);
}

// ------------------------------------------------------------------------------------------
// Linux: freedesktop.org Trash spec
// ------------------------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    fn uid(fx: &Fixture) -> u32 {
        fs::metadata(fx.root()).unwrap().uid()
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().mode() & 0o7777
    }

    /// Whether permission bits bind this process (not root): a write into a `0555` directory
    /// must fail.
    fn permissions_enforced(dir: &Path) -> bool {
        let probe = dir.join(".scull-probe");
        match fs::write(&probe, b"x") {
            Ok(()) => {
                let _ = fs::remove_file(&probe);
                false
            }
            Err(_) => true,
        }
    }

    /// A temporary directory on another filesystem than the fixture (`/dev/shm`), removed on
    /// drop. Created by this test only.
    struct OtherVolumeDir(PathBuf);

    impl OtherVolumeDir {
        fn new(fx: &Fixture) -> Option<Self> {
            let shm = Path::new("/dev/shm");
            let dev = |p: &Path| fs::metadata(p).ok().map(|m| m.dev());
            if dev(shm).is_none() || dev(shm) == dev(fx.root()) {
                return None;
            }
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = shm.join(format!("scull-trash-test-{}-{nanos}", std::process::id()));
            fs::create_dir(&dir).ok()?;
            Some(Self(dir))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for OtherVolumeDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn read_info(r: &TrashReport) -> String {
        fs::read_to_string(r.outcome.info_file.as_ref().expect("info file")).unwrap()
    }

    /// FX-PHY-07: an S1 item goes to the (home) Trash and is reported as "moved", not
    /// "freed"; the `.trashinfo` records its absolute path and deletion date.
    #[test]
    fn fx_phy_07_trashed_item_reported_moved_not_freed() {
        let mut fx = Fixture::new().unwrap();
        candidate(&mut fx, "proj/target");
        fx.dir("xdg").unwrap().checkpoint().unwrap();
        let home = fx.root().join("xdg/Trash");
        let item = trash_item(&fx, "proj/target", Tier::S1);

        print_tree(&fx, "FX-PHY-07 before");
        let r = trash(
            &fx,
            &item,
            "proj/target",
            &locations(Some(home.clone()), Some(fx.root())),
        )
        .unwrap();
        print_tree(&fx, "FX-PHY-07 after");
        println!(
            "moved {} B ({} logical, {} entries) to {:?}; freed {} B; disposition {:?}",
            r.moved_bytes(),
            r.outcome.moved.logical_bytes,
            r.outcome.moved.entries,
            r.outcome.location,
            r.freed_bytes(),
            TrashReport::DISPOSITION
        );

        assert_eq!(r.outcome.kind, TrashKind::Home);
        assert_eq!(r.outcome.trash_dir, home);
        assert_eq!(r.outcome.location, Some(home.join("files/target")));
        assert_moved(&fx, &item, "proj/target", &r);
        assert_eq!(r.outcome.moved.logical_bytes, 1 + 8192 + 4096);
        assert!(r.moved_bytes() >= 8192 + 4096, "{:?}", r.outcome.moved);
        assert_eq!(r.outcome.moved.entries, 5);
        assert_eq!(
            fs::read(home.join("files/target/a.txt")).unwrap(),
            b"a",
            "content moved intact"
        );
        let info = read_info(&r);
        let abs = fx.root().join("proj/target");
        assert!(info.starts_with("[Trash Info]\n"), "{info}");
        assert!(
            info.contains(&format!("\nPath={}\n", abs.display())),
            "{info}"
        );
        assert!(info.contains("\nDeletionDate=2"), "{info}");
        for dir in ["", "/files", "/info"] {
            assert_eq!(
                mode(&PathBuf::from(format!("{}{dir}", home.display()))),
                0o700
            );
        }
        assert_canary_files(&fx);
        check(&fx, &[&item], &["proj/target", "xdg"]);
    }

    /// FX-LNX-02: an item on a volume without a home trash goes to that volume's
    /// `$topdir/.Trash-$uid` (created owner-only); the `.trashinfo` path is relative to
    /// `$topdir`.
    #[test]
    fn fx_lnx_02_per_volume_trash_dir_is_created_owner_only() {
        let mut fx = Fixture::new().unwrap();
        candidate(&mut fx, "mnt/data/cache");
        fx.checkpoint().unwrap();
        let item = trash_item(&fx, "mnt/data/cache", Tier::S2);
        let r = trash(
            &fx,
            &item,
            "mnt/data/cache",
            &locations(None, Some(fx.root())),
        )
        .unwrap();
        let dir = fx.root().join(format!(".Trash-{}", uid(&fx)));
        println!(
            "FX-LNX-02: {:?} -> {:?}",
            r.outcome.kind, r.outcome.location
        );
        assert_eq!(r.outcome.kind, TrashKind::VolumeUser);
        assert_eq!(r.outcome.trash_dir, dir);
        assert_eq!(r.outcome.location, Some(dir.join("files/cache")));
        assert_eq!(mode(&dir), 0o700);
        assert_moved(&fx, &item, "mnt/data/cache", &r);
        let info = read_info(&r);
        assert!(info.contains("\nPath=mnt/data/cache\n"), "{info}");
        assert_eq!(r.outcome.info_file, Some(dir.join("info/cache.trashinfo")));
        assert_canary_files(&fx);
        let trash_rel = format!(".Trash-{}", uid(&fx));
        check(&fx, &[&item], &["mnt/data/cache", &trash_rel]);
    }

    /// FX-LNX-02: an administrator-created `$topdir/.Trash` with the sticky bit is used through
    /// the user's own `$topdir/.Trash/$uid`.
    #[test]
    fn fx_lnx_02_shared_sticky_trash_uses_uid_subdirectory() {
        let mut fx = Fixture::new().unwrap();
        candidate(&mut fx, "cand");
        fx.dir(".Trash").unwrap();
        fs::set_permissions(fx.root().join(".Trash"), fs::Permissions::from_mode(0o1777)).unwrap();
        fx.checkpoint().unwrap();
        let item = trash_item(&fx, "cand", Tier::S1);
        let r = trash(&fx, &item, "cand", &locations(None, Some(fx.root()))).unwrap();
        let dir = fx.root().join(format!(".Trash/{}", uid(&fx)));
        assert_eq!(r.outcome.kind, TrashKind::VolumeShared);
        assert_eq!(r.outcome.location, Some(dir.join("files/cand")));
        assert_eq!(mode(&dir), 0o700);
        assert_moved(&fx, &item, "cand", &r);
        check(&fx, &[&item], &["cand", ".Trash"]);
    }

    /// FX-LNX-02: a `$topdir/.Trash` without the sticky bit, or one that is a symlink, is not
    /// used (spec: the check fails); `$topdir/.Trash-$uid` is used instead and the link's
    /// target is untouched.
    #[test]
    fn fx_lnx_02_unsafe_shared_trash_is_skipped() {
        for variant in ["no-sticky", "symlink"] {
            let mut fx = Fixture::new().unwrap();
            candidate(&mut fx, "cand");
            match variant {
                "no-sticky" => {
                    fx.dir(".Trash").unwrap();
                    fs::set_permissions(
                        fx.root().join(".Trash"),
                        fs::Permissions::from_mode(0o777),
                    )
                    .unwrap();
                }
                _ => {
                    // A sticky directory elsewhere, reached through a `.Trash` symlink.
                    fx.dir("elsewhere").unwrap();
                    fs::set_permissions(
                        fx.root().join("elsewhere"),
                        fs::Permissions::from_mode(0o1777),
                    )
                    .unwrap();
                    assert!(fx.symlink(".Trash", "elsewhere").unwrap().is_created());
                }
            }
            fx.checkpoint().unwrap();
            let item = trash_item(&fx, "cand", Tier::S1);
            let r = trash(&fx, &item, "cand", &locations(None, Some(fx.root()))).unwrap();
            assert_eq!(r.outcome.kind, TrashKind::VolumeUser, "{variant}");
            assert_moved(&fx, &item, "cand", &r);
            assert!(
                fs::read_dir(fx.root().join(".Trash"))
                    .unwrap()
                    .next()
                    .is_none(),
                "{variant}: the unsafe .Trash is left untouched"
            );
            let trash_rel = format!(".Trash-{}", uid(&fx));
            check(&fx, &[&item], &["cand", &trash_rel]);
        }
    }

    /// FX-LNX-02: the home trash is on another volume than the item, so the item's own volume
    /// trash is used; nothing is ever copied across volumes. Needs a second filesystem
    /// (`/dev/shm`); skipped where there is none.
    #[test]
    fn fx_lnx_02_home_trash_on_another_volume_uses_the_items_volume() {
        let mut fx = Fixture::new().unwrap();
        let other = match OtherVolumeDir::new(&fx) {
            Some(d) => d,
            None => {
                println!("skipped: no second filesystem at /dev/shm");
                check(&fx, &[], &[]);
                return;
            }
        };
        candidate(&mut fx, "cand");
        fx.checkpoint().unwrap();
        let item = trash_item(&fx, "cand", Tier::S1);
        let home = other.path().join("Trash");
        let r = trash(
            &fx,
            &item,
            "cand",
            &locations(Some(home.clone()), Some(fx.root())),
        )
        .unwrap();
        assert_eq!(r.outcome.kind, TrashKind::VolumeUser);
        assert!(
            !home.exists(),
            "the other volume's home trash is not even created"
        );
        assert_moved(&fx, &item, "cand", &r);

        // The only trash offered is on the other volume: blocked, never copied.
        candidate(&mut fx, "cand2");
        let item2 = trash_item(&fx, "cand2", Tier::S1);
        let e = trash(&fx, &item2, "cand2", &locations(None, Some(other.path()))).unwrap_err();
        println!("cross-volume: {e}");
        assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{e}");
        assert!(e.message.contains("never copied"), "{e}");
        assert!(fx.root().join("cand2/b.bin").exists());
        let trash_rel = format!(".Trash-{}", uid(&fx));
        check(
            &fx,
            &[&item, &item2],
            &["cand", "cand2", "keep", &trash_rel],
        );
    }

    /// FX-LNX-01: a headless account with no Trash (no `$XDG_DATA_HOME`, and the volume's top
    /// directory can't hold a `.Trash-$uid`): the item is blocked, never deleted.
    #[test]
    fn fx_lnx_01_headless_no_trash_item_blocked() {
        let mut fx = Fixture::new().unwrap();
        candidate(&mut fx, "srv/cache");
        fx.dir("vol").unwrap();
        fx.checkpoint().unwrap();
        // The "volume top" is read-only (as `/` is for a user on a server).
        let top = fx.root().to_path_buf();
        let original = fs::metadata(&top).unwrap().permissions();
        fs::set_permissions(&top, fs::Permissions::from_mode(0o555)).unwrap();
        if !permissions_enforced(&top) {
            fs::set_permissions(&top, original).unwrap();
            println!("skipped: running with privileges that bypass permissions");
            check(&fx, &[], &[]);
            return;
        }
        let item = trash_item(&fx, "srv/cache", Tier::S1);
        let no_home = Some(fx.root().join("no-such-xdg-data-home/Trash"));
        let r = trash(&fx, &item, "srv/cache", &locations(no_home, Some(&top)));
        fs::set_permissions(&top, original).unwrap();
        let e = r.unwrap_err();
        println!("FX-LNX-01: {} ({})", e.kind, e.message);
        assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{e}");
        assert!(e.message.contains("item blocked"), "{e}");
        assert!(fx.root().join("srv/cache/b.bin").exists(), "never deleted");
        assert!(!fx.root().join(format!(".Trash-{}", uid(&fx))).exists());
        assert!(!fx.root().join("no-such-xdg-data-home").exists());
        assert_canary_files(&fx);
        check(&fx, &[&item], &[]);
    }

    /// FX-EXEC-03 (Trash half): the item's parent is read-only, so quarantine is impossible
    /// and so is the Trash (a rename needs write access to the parent): refused, the record
    /// written for it is removed, nothing is deleted.
    #[test]
    fn fx_exec_03_read_only_parent_trash_refused_never_deleted() {
        let mut fx = Fixture::new().unwrap();
        candidate(&mut fx, "ro/cand");
        fx.dir("xdg").unwrap().checkpoint().unwrap();
        let parent = fx.root().join("ro");
        let original = fs::metadata(&parent).unwrap().permissions();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();
        if !permissions_enforced(&parent) {
            fs::set_permissions(&parent, original).unwrap();
            println!("skipped: running with privileges that bypass permissions");
            check(&fx, &[], &[]);
            return;
        }
        let item = trash_item(&fx, "ro/cand", Tier::S1);
        let home = fx.root().join("xdg/Trash");
        let r = trash(
            &fx,
            &item,
            "ro/cand",
            &locations(Some(home.clone()), Some(fx.root())),
        );
        fs::set_permissions(&parent, original).unwrap();
        let e = r.unwrap_err();
        println!("FX-EXEC-03: {} ({})", e.kind, e.message);
        assert_eq!(e.kind, ErrorKind::PermissionDenied, "{e}");
        assert!(fx.root().join("ro/cand/b.bin").exists(), "never deleted");
        assert_eq!(
            fs::read_dir(home.join("info")).unwrap().count(),
            0,
            "record removed"
        );
        assert_eq!(fs::read_dir(home.join("files")).unwrap().count(), 0);
        check(&fx, &[&item], &["xdg"]);
    }

    /// SAFE-05: the item was replaced after planning: refused before anything moves, and no
    /// record is left behind.
    #[test]
    fn replaced_item_is_refused() {
        let mut fx = Fixture::new().unwrap();
        candidate(&mut fx, "cand");
        fx.dir("xdg").unwrap();
        let item = trash_item(&fx, "cand", Tier::S1);
        fs::rename(fx.root().join("cand"), fx.root().join("old")).unwrap();
        fs::create_dir(fx.root().join("cand")).unwrap();
        fx.checkpoint().unwrap();
        let home = fx.root().join("xdg/Trash");
        let e = trash(
            &fx,
            &item,
            "cand",
            &locations(Some(home.clone()), Some(fx.root())),
        )
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::PlanInvalidated, "{e}");
        assert!(fx.root().join("cand").is_dir());
        assert!(!home.exists(), "nothing created before the identity check");
        check(&fx, &[&item], &[]);
    }

    /// The item is a symlink to the canary: the link itself goes to the Trash, the canary is
    /// untouched (never followed).
    #[test]
    fn symlink_item_moves_only_the_link() {
        let mut fx = Fixture::new().unwrap();
        let canary_dir = fx.canary().symlink_target_dir.clone();
        assert!(fx.symlink("cand", &canary_dir).unwrap().is_created());
        fx.dir("xdg").unwrap().checkpoint().unwrap();
        let item = trash_item(&fx, "cand", Tier::S1);
        let home = fx.root().join("xdg/Trash");
        let r = trash(
            &fx,
            &item,
            "cand",
            &locations(Some(home.clone()), Some(fx.root())),
        )
        .unwrap();
        let moved = home.join("files/cand");
        assert!(
            fs::symlink_metadata(&moved)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_link(&moved).unwrap(), canary_dir);
        assert_moved(&fx, &item, "cand", &r);
        assert_eq!(r.outcome.moved.entries, 1);
        assert_canary_files(&fx);
        check(&fx, &[&item], &["cand", "xdg"]);
    }

    /// A symlink between the anchor and the item is never followed: refused.
    #[test]
    fn link_in_path_is_refused() {
        let mut fx = Fixture::new().unwrap();
        candidate(&mut fx, "real/cand");
        symlink("real", fx.root().join("via")).unwrap();
        fx.checkpoint().unwrap();
        let path = fx.root().join("via/cand");
        let item = plan_item(&path, Tier::S1, Action::Trash);
        let e = Executor::new()
            .trash(
                &item,
                &target(&fx, &path),
                &locations(None, Some(fx.root())),
            )
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{e}");
        assert!(fx.root().join("real/cand/b.bin").exists());
        check(&fx, &[&item], &[]);
    }

    /// Names already taken in the trash (a record, or an orphan in `files/`) are never
    /// replaced: the item gets the next free name.
    #[test]
    fn taken_names_are_never_replaced() {
        let mut fx = Fixture::new().unwrap();
        candidate(&mut fx, "cand");
        fx.file("xdg/Trash/info/cand.trashinfo", b"older record")
            .unwrap()
            .file("xdg/Trash/files/cand.2", b"orphan without a record")
            .unwrap();
        for d in ["xdg/Trash", "xdg/Trash/info", "xdg/Trash/files"] {
            fs::set_permissions(fx.root().join(d), fs::Permissions::from_mode(0o700)).unwrap();
        }
        fx.checkpoint().unwrap();
        let item = trash_item(&fx, "cand", Tier::S1);
        let home = fx.root().join("xdg/Trash");
        let r = trash(
            &fx,
            &item,
            "cand",
            &locations(Some(home.clone()), Some(fx.root())),
        )
        .unwrap();
        assert_eq!(r.outcome.location, Some(home.join("files/cand.3")));
        assert_eq!(
            r.outcome.info_file,
            Some(home.join("info/cand.3.trashinfo"))
        );
        assert_eq!(
            fs::read(home.join("info/cand.trashinfo")).unwrap(),
            b"older record"
        );
        assert_eq!(
            fs::read(home.join("files/cand.2")).unwrap(),
            b"orphan without a record"
        );
        assert!(
            !home.join("info/cand.2.trashinfo").exists(),
            "record removed"
        );
        assert_moved(&fx, &item, "cand", &r);
        check(
            &fx,
            &[&item],
            &["cand", "xdg/Trash/files", "xdg/Trash/info"],
        );
    }

    /// An existing trash directory other users can write to is not used (they could plant or
    /// swap entries): blocked.
    #[test]
    fn trash_writable_by_others_is_not_used() {
        let mut fx = Fixture::new().unwrap();
        candidate(&mut fx, "cand");
        fx.dir("xdg/Trash").unwrap();
        fs::set_permissions(
            fx.root().join("xdg/Trash"),
            fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        fx.checkpoint().unwrap();
        let item = trash_item(&fx, "cand", Tier::S1);
        let home = fx.root().join("xdg/Trash");
        let e = trash(&fx, &item, "cand", &locations(Some(home), Some(fx.root()))).unwrap_err();
        assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{e}");
        assert!(e.message.contains("writable by other users"), "{e}");
        assert!(fx.root().join("cand/b.bin").exists());
        check(&fx, &[&item], &[]);
    }

    /// An injected volume top that is not an ancestor of the item is refused.
    #[test]
    fn topdir_must_be_an_ancestor() {
        let mut fx = Fixture::new().unwrap();
        candidate(&mut fx, "a/cand");
        fx.dir("b").unwrap().checkpoint().unwrap();
        let item = trash_item(&fx, "a/cand", Tier::S1);
        let b = fx.root().join("b");
        let e = trash(&fx, &item, "a/cand", &locations(None, Some(&b))).unwrap_err();
        assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{e}");
        assert!(e.message.contains("not an ancestor"), "{e}");
        assert!(fx.root().join("a/cand/b.bin").exists());
        check(&fx, &[&item], &[]);
    }

    /// A quarantine item (whose quarantine is impossible) may fall back to the Trash; a single
    /// file keeps its identity.
    #[test]
    fn quarantine_item_falls_back_to_trash() {
        let mut fx = Fixture::new().unwrap();
        fx.file_of_size("big.log", 10_000)
            .unwrap()
            .dir("xdg")
            .unwrap()
            .checkpoint()
            .unwrap();
        let item = plan_item(&fx.root().join("big.log"), Tier::S2, Action::Quarantine);
        let home = fx.root().join("xdg/Trash");
        let r = trash(
            &fx,
            &item,
            "big.log",
            &locations(Some(home.clone()), Some(fx.root())),
        )
        .unwrap();
        assert_eq!(r.outcome.location, Some(home.join("files/big.log")));
        assert_eq!(r.outcome.moved.logical_bytes, 10_000);
        assert_moved(&fx, &item, "big.log", &r);
        check(&fx, &[&item], &["big.log", "xdg"]);
    }
}

// ------------------------------------------------------------------------------------------
// macOS: ~/.Trash and a volume's .Trashes/<uid>
// ------------------------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn uid(fx: &Fixture) -> u32 {
        fs::metadata(fx.root()).unwrap().uid()
    }

    /// FX-MAC-05: an item on an "external volume" (the injected volume top) goes to that
    /// volume's `.Trashes/<uid>`, reported as moved.
    #[test]
    fn fx_mac_05_external_volume_uses_its_trashes() {
        let mut fx = Fixture::new().unwrap();
        candidate(&mut fx, "proj/build");
        fx.dir(".Trashes").unwrap();
        // Real `.Trashes` is d-wx-wx-wt (1333); readable here so the fixture can snapshot it.
        fs::set_permissions(
            fx.root().join(".Trashes"),
            fs::Permissions::from_mode(0o1777),
        )
        .unwrap();
        fx.checkpoint().unwrap();
        let item = trash_item(&fx, "proj/build", Tier::S1);
        let r = trash(&fx, &item, "proj/build", &locations(None, Some(fx.root()))).unwrap();
        let dir = fx.root().join(format!(".Trashes/{}", uid(&fx)));
        println!(
            "FX-MAC-05: {:?} -> {:?}",
            r.outcome.kind, r.outcome.location
        );
        assert_eq!(r.outcome.kind, TrashKind::VolumeShared);
        assert_eq!(r.outcome.location, Some(dir.join("build")));
        assert_eq!(r.outcome.info_file, None);
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        assert_moved(&fx, &item, "proj/build", &r);
        assert_canary_files(&fx);
        check(&fx, &[&item], &["proj/build", ".Trashes"]);
    }

    /// FX-MAC-05: a volume without `.Trashes` gets one (`d-wx-wx-wt`, as Finder creates it).
    #[test]
    fn fx_mac_05_missing_trashes_is_created() {
        let mut fx = Fixture::new().unwrap();
        candidate(&mut fx, "cand");
        fx.checkpoint().unwrap();
        let item = trash_item(&fx, "cand", Tier::S1);
        let r = trash(&fx, &item, "cand", &locations(None, Some(fx.root()))).unwrap();
        let trashes = fx.root().join(".Trashes");
        let mode = fs::symlink_metadata(&trashes).unwrap().mode() & 0o7777;
        // Make it readable again so the fixture can snapshot and remove it.
        fs::set_permissions(&trashes, fs::Permissions::from_mode(0o1777)).unwrap();
        assert_eq!(mode, 0o1333);
        assert_moved(&fx, &item, "cand", &r);
        check(&fx, &[&item], &["cand", ".Trashes"]);
    }

    /// The home volume's `~/.Trash`, with Finder-style names on collision.
    #[test]
    fn home_trash_used_for_the_home_volume() {
        let mut fx = Fixture::new().unwrap();
        candidate(&mut fx, "cand");
        fx.file("home/.Trash/cand", b"older").unwrap();
        fs::set_permissions(
            fx.root().join("home/.Trash"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        fx.checkpoint().unwrap();
        let item = trash_item(&fx, "cand", Tier::S1);
        let home = fx.root().join("home/.Trash");
        let r = trash(
            &fx,
            &item,
            "cand",
            &locations(Some(home.clone()), Some(fx.root())),
        )
        .unwrap();
        assert_eq!(r.outcome.kind, TrashKind::Home);
        assert_eq!(r.outcome.location, Some(home.join("cand 2")));
        assert_eq!(fs::read(home.join("cand")).unwrap(), b"older");
        assert_moved(&fx, &item, "cand", &r);
        check(&fx, &[&item], &["cand", "home/.Trash"]);
    }
}

// ------------------------------------------------------------------------------------------
// Windows: the volume's Recycle Bin
// ------------------------------------------------------------------------------------------

#[cfg(windows)]
mod windows {
    use super::*;

    fn bin_entries(fx: &Fixture) -> Vec<PathBuf> {
        let bin = fx.root().join("$Recycle.Bin");
        let mut out = Vec::new();
        for user in fs::read_dir(bin).unwrap() {
            for e in fs::read_dir(user.unwrap().path()).unwrap() {
                out.push(e.unwrap().path());
            }
        }
        out.sort();
        out
    }

    /// FX-WIN-06: Recycle Bin fallback: the item becomes `$R…` in the volume's
    /// `$Recycle.Bin\<SID>` with a `$I…` record (format 2) naming its original path; reported
    /// as moved.
    #[test]
    fn fx_win_06_recycle_bin_fallback_reported_moved() {
        let mut fx = Fixture::new().unwrap();
        candidate(&mut fx, "proj/target");
        fx.dir("$Recycle.Bin").unwrap().checkpoint().unwrap();
        let item = trash_item(&fx, "proj/target", Tier::S1);
        let r = trash(&fx, &item, "proj/target", &locations(None, Some(fx.root()))).unwrap();
        println!(
            "FX-WIN-06: {:?} -> {:?}",
            r.outcome.kind, r.outcome.location
        );
        assert_eq!(r.outcome.kind, TrashKind::RecycleBin);
        let location = r.outcome.location.clone().unwrap();
        let info = r.outcome.info_file.clone().unwrap();
        let rname = location.file_name().unwrap().to_string_lossy().into_owned();
        let iname = info.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            rname.starts_with("$R") && iname.starts_with("$I"),
            "{rname} {iname}"
        );
        assert_eq!(rname[2..], iname[2..]);
        assert_eq!(bin_entries(&fx).len(), 2);
        let record = fs::read(&info).unwrap();
        assert_eq!(&record[..8], &2u64.to_le_bytes());
        assert_eq!(
            u64::from_le_bytes(record[8..16].try_into().unwrap()),
            r.outcome.moved.logical_bytes
        );
        let original: Vec<u16> = fx
            .root()
            .join("proj/target")
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .collect();
        let text: Vec<u16> = record[28..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(&text[..text.len() - 1], &original[..]);
        assert_moved(&fx, &item, "proj/target", &r);
        assert_canary_files(&fx);
        check(&fx, &[&item], &["proj/target", "$Recycle.Bin"]);
    }

    /// A volume without `$Recycle.Bin` has no Recycle Bin: blocked, never deleted.
    #[test]
    fn fx_win_06_no_recycle_bin_blocks() {
        let mut fx = Fixture::new().unwrap();
        candidate(&mut fx, "cand");
        fx.checkpoint().unwrap();
        let item = trash_item(&fx, "cand", Tier::S1);
        let e = trash(&fx, &item, "cand", &locations(None, Some(fx.root()))).unwrap_err();
        assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{e}");
        assert!(fx.root().join("cand/b.bin").exists());
        check(&fx, &[&item], &[]);
    }

    /// The real Recycle Bin of the temp directory's volume. Ignored by default; CI opts in with
    /// `SCULL_TEST_REAL_RECYCLE_BIN=1`. Moves one marker file from a fixture and removes its
    /// `$R`/`$I` entries afterwards.
    #[test]
    #[ignore = "uses the real Recycle Bin; set SCULL_TEST_REAL_RECYCLE_BIN=1 on CI"]
    fn real_recycle_bin_round_trip() {
        let mut fx = Fixture::new().unwrap();
        if std::env::var_os("SCULL_TEST_REAL_RECYCLE_BIN").is_none() {
            check(&fx, &[], &[]);
            return;
        }
        fx.file("scull-trash-marker.txt", b"scull test marker")
            .unwrap()
            .checkpoint()
            .unwrap();
        let item = trash_item(&fx, "scull-trash-marker.txt", Tier::S1);
        let r = trash(
            &fx,
            &item,
            "scull-trash-marker.txt",
            &TrashLocations::from_env(),
        )
        .unwrap();
        let location = r.outcome.location.clone().unwrap();
        let info = r.outcome.info_file.clone().unwrap();
        let content = fs::read(&location);
        let _ = fs::remove_file(&location);
        let _ = fs::remove_file(&info);
        assert_eq!(content.unwrap(), b"scull test marker");
        check(&fx, &[&item], &["scull-trash-marker.txt"]);
    }
}
