//! OS-specific nightly fixtures (docs/FIXTURES.md §3.11, §3.12, mode **Nightly**; P2-FX-04):
//! the ones that need real mounts, so they never run in the per-PR `cargo test --workspace`
//! (they are `#[ignore]`d). Run them with:
//!
//! ```text
//! cargo test -p scull-fixtures --test fx_os_nightly -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The nightly workflow (`.github/workflows/nightly-fixtures.yml`, job `fx-os`) runs exactly
//! that on Linux and macOS with `SCULL_FIXTURES_REQUIRE_PRIVILEGED=1` and
//! `SCULL_FIXTURES_ALLOW_SUDO=1`; privileges come from [`scull_fixtures::require_privileges!`].
//!
//! * **FX-LNX-02** (Linux): an item on a real second volume (a tmpfs, or a loop-mounted ext4
//!   image as real root) goes to that volume's `$topdir/.Trash-$uid`, with `$topdir` found by
//!   the platform layer from the mount table (not injected), never to the home Trash on the
//!   other volume.
//! * **FX-LNX-03** (Linux, root): an NFS export (`SCULL_FX_NFS_EXPORT`, set up by the
//!   workflow) or a FUSE mount (`bindfs`) inside a scan root is not descended, its content is
//!   never classified, a candidate containing it is S3, and a scan root on it is refused by
//!   default; a FUSE mount whose daemon is frozen (`SIGSTOP`) doesn't hang the scan.
//! * **FX-LNX-06** (Linux, root): a Btrfs subvolume inside a candidate (loop-mounted Btrfs
//!   image) is a mount boundary: the candidate is S3 and can't enter a plan; executing the plan
//!   of its sibling through the real Executor deletes the sibling only, the subvolume is
//!   untouched.
//! * **FX-MAC-05** (macOS): an item on an attached disk image goes to that volume's
//!   `.Trashes/<uid>`, with the volume found by the platform layer (not injected).
//!
//! Everything happens inside fixture temporary directories, with canaries (I-2) and the
//! universal invariants; mounts are confined and undone by [`scull_fixtures::Mounts`].

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(any(target_os = "linux", target_os = "macos"))]
// Most helpers and imports serve the Linux fixtures only.
#![cfg_attr(target_os = "macos", allow(unused_imports, dead_code))]

mod exec;

use exec::engine::{harness, request};
use scull_cleaner::trash::{TrashKind, TrashLocations, TrashReport};
use scull_cleaner::{DeleteTarget, Executor};
use scull_core::{Action, Candidate, PlanItem, ReasonCode, Tier};
use scull_engine::paths::canon;
use scull_engine::{CancelToken, ScanReport};
use scull_fixtures::privileged::Need;
use scull_fixtures::{Fixture, InvariantInputs, Mounts, NodeLock, TreeSnapshot};
use scull_platform::{Native, Platform};
use std::fs;
use std::path::{Path, PathBuf};

/// The engine scan of `root` (a mount point inside the fixture).
fn scan_root(h: &exec::engine::Harness, root: &Path) -> ScanReport {
    h.engine
        .scan(&[canon(root)], &request(), &CancelToken::new(), |_| {})
        .unwrap()
}

fn rel_to(root: &Path, p: &Path) -> String {
    p.strip_prefix(canon(root))
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The candidate at `rel` below `root`.
fn cand<'a>(r: &'a ScanReport, root: &Path, rel: &str) -> &'a Candidate {
    r.candidates
        .iter()
        .find(|c| rel_to(root, &c.path.to_path_buf()) == rel)
        .unwrap_or_else(|| {
            let all: Vec<String> = r
                .candidates
                .iter()
                .map(|c| format!("{} {}", rel_to(root, &c.path.to_path_buf()), c.tier))
                .collect();
            panic!("no candidate at {rel}: {all:#?}")
        })
}

fn current_uid() -> String {
    let o = std::process::Command::new("id").arg("-u").output().unwrap();
    String::from_utf8_lossy(&o.stdout).trim().to_owned()
}

/// An npm project without a lockfile at `rel` (S2, quarantine: Trash is its fallback).
fn npm_without_lock(fx: &mut Fixture, rel: &str) {
    fx.npm_project(rel, "web", NodeLock::None)
        .unwrap()
        .node_modules(rel, &["left-pad", "react"])
        .unwrap()
        .file_of_size(format!("{rel}/node_modules/react/big.js"), 32 * 1024)
        .unwrap();
}

/// Move the candidate `node_modules` of the project at `<mount>/web` to the Trash through the
/// real Executor, with the volume top **not** injected: the platform finds the mount point.
/// The home Trash is offered on the fixture's own (other) volume and must not be used.
fn trash_on_mount(fx: &Fixture, mount: &Path) -> (PlanItem, TrashReport, PathBuf) {
    let h = harness();
    let r = scan_root(&h, mount);
    let c = cand(&r, mount, "web/node_modules");
    println!(
        "candidate web/node_modules on the mount: {} {:?}",
        c.tier, c.action
    );
    assert!(
        matches!(c.action, Action::Quarantine | Action::Trash),
        "{c:?}"
    );
    let item = PlanItem::from_candidate(c).unwrap();
    let anchor = canon(mount);
    let path = c.path.to_path_buf();
    let home = fx.root().join("xdg/Trash");
    let locations = TrashLocations {
        home: Some(home.clone()),
        topdir: None,
    };
    let report = Executor::new()
        .trash(
            &item,
            &DeleteTarget {
                anchor: &anchor,
                anchor_identity: Native.metadata(&anchor).unwrap().identity,
                path: &path,
            },
            &locations,
        )
        .unwrap();
    println!(
        "trashed: {:?} -> {:?} (moved {} B, freed {} B, {})",
        report.outcome.kind,
        report.outcome.location,
        report.moved_bytes(),
        report.freed_bytes(),
        TrashReport::DISPOSITION
    );
    assert!(
        !home.exists(),
        "the home Trash on the other volume is unused"
    );
    assert!(fs::symlink_metadata(&path).is_err(), "the original is gone");
    assert_eq!(
        Native
            .metadata(report.outcome.location.as_ref().unwrap())
            .unwrap()
            .identity,
        Some(item.identity),
        "the moved item is the planned object"
    );
    assert_eq!(
        report.freed_bytes(),
        0,
        "Trash moves bytes, never frees them"
    );
    (item, report, path)
}

// ---------------------------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------------------------

/// FX-LNX-02: an item on a real second volume goes to `$topdir/.Trash-$uid` of that volume
/// (created `0700`), found from the mount table; the `.trashinfo` path is relative to
/// `$topdir`.
#[test]
#[ignore = "nightly: mounts a second volume (user+mount namespace or root); run with --ignored"]
#[cfg(target_os = "linux")]
fn fx_lnx_02_trash_on_a_real_second_volume_uses_its_topdir() {
    use scull_fixtures::privileged::{is_real_root, tool_path};
    use std::os::unix::fs::PermissionsExt;

    let how = scull_fixtures::require_privileges!(Need::Mounts);
    println!("FX-LNX-02: privileges: {how}");
    let mut fx = Fixture::new().unwrap();
    fx.dir("vol").unwrap().dir("xdg").unwrap();
    let mut mounts = Mounts::new(&fx);
    let vol = fx.root().join("vol");
    let fs_kind = if is_real_root() && tool_path("mkfs.ext4").is_some() {
        mounts
            .loop_image(&fx.base().join("vol.img"), "ext4", 32, &vol)
            .unwrap();
        "loop-mounted ext4"
    } else {
        mounts.tmpfs(&vol, 16).unwrap();
        "tmpfs"
    };
    println!("FX-LNX-02: second volume: {fs_kind} at vol/");
    npm_without_lock(&mut fx, "vol/web");
    fx.checkpoint().unwrap();

    let (item, report, _) = trash_on_mount(&fx, &vol);
    let uid = current_uid();
    let dir = canon(&vol).join(format!(".Trash-{uid}"));
    assert_eq!(report.outcome.kind, TrashKind::VolumeUser);
    assert_eq!(report.outcome.trash_dir, dir);
    assert_eq!(
        report.outcome.location,
        Some(dir.join("files/node_modules"))
    );
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o7777,
        0o700
    );
    let info = fs::read_to_string(report.outcome.info_file.as_ref().unwrap()).unwrap();
    assert!(info.contains("\nPath=web/node_modules\n"), "{info}");
    println!("FX-LNX-02: {}", info.replace('\n', " | "));
    println!("FX-LNX-02: per-volume Trash {} used", dir.display());

    let trash_rel = format!("vol/.Trash-{uid}");
    fx.check_invariants(
        &InvariantInputs::new()
            .plan([(item.item_id.clone(), item.tier, Action::Trash)])
            .allow("vol/web/node_modules")
            .allow(trash_rel.as_str()),
    )
    .assert_ok();
    mounts.unmount_all().unwrap();
}

/// A `bindfs` FUSE mount of `source` at `target`, run in the foreground as a child (so it can
/// be frozen); returns the child once the mount shows in the mount table.
#[cfg(target_os = "linux")]
fn bindfs(source: &Path, target: &Path) -> std::process::Child {
    let bin = scull_fixtures::privileged::tool_path("bindfs").expect("bindfs installed");
    let mut child = std::process::Command::new(bin)
        .arg("-f")
        .arg(source)
        .arg(target)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let want = target.to_string_lossy().into_owned();
    for _ in 0..100 {
        let table = fs::read_to_string("/proc/self/mountinfo").unwrap();
        if table
            .lines()
            .any(|l| l.split_whitespace().nth(4) == Some(want.as_str()))
        {
            return child;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("bindfs did not mount {want}");
}

#[cfg(target_os = "linux")]
fn signal(pid: u32, sig: &str) {
    let ok = std::process::Command::new("kill")
        .arg(format!("-{sig}"))
        .arg(pid.to_string())
        .status()
        .unwrap()
        .success();
    assert!(ok, "kill -{sig} {pid}");
}

/// FX-LNX-03: an NFS (or FUSE, which scull treats as network) mount inside a scan root is
/// skipped by default, never classified, makes its candidate S3, can't be a scan root by
/// default; and a frozen FUSE mount doesn't hang the scan.
#[test]
#[ignore = "nightly: mounts NFS/FUSE as root; run with --ignored"]
#[cfg(target_os = "linux")]
fn fx_lnx_03_network_mount_inside_scan_root_is_skipped_and_never_hangs() {
    use scull_core::config::Config;
    use scull_engine::{Engine, EngineConfig, IssueKind};
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    let how = scull_fixtures::require_privileges!(Need::Root);
    println!("FX-LNX-03: privileges: {how}");
    let nfs = std::env::var("SCULL_FX_NFS_EXPORT")
        .ok()
        .filter(|v| !v.is_empty());
    let has_bindfs = scull_fixtures::privileged::has_tool("bindfs");
    if nfs.is_none() && !has_bindfs {
        scull_fixtures::privileged::skip(
            "fx_lnx_03_network_mount_inside_scan_root_is_skipped_and_never_hangs",
            "neither SCULL_FX_NFS_EXPORT (an NFS export to mount) nor bindfs (FUSE) is available",
        );
        return;
    }

    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("proj", "proj")
        .unwrap()
        .cargo_target("proj", "proj", 4096)
        .unwrap()
        .dir("proj/target/net")
        .unwrap()
        .dir("share")
        .unwrap()
        .dir("hang")
        .unwrap();
    // Remote content: a project whose `target/` would be a candidate if it were ever walked.
    let remote = fx.base().join("remote");
    for d in ["app/src", "app/target/debug"] {
        fs::create_dir_all(remote.join(d)).unwrap();
    }
    fs::write(
        remote.join("app/Cargo.toml"),
        "[package]\nname = \"remote\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(remote.join("app/Cargo.lock"), "version = 4\n").unwrap();
    fs::write(remote.join("app/target/debug/remote"), [0u8; 2048]).unwrap();

    let mut mounts = Mounts::new(&fx);
    let share = fx.root().join("share");
    let net = fx.root().join("proj/target/net");
    let mut children = Vec::new();
    let kind = if let Some(export) = &nfs {
        for t in [&share, &net] {
            mounts
                .mount("nfs4", export, Some("soft,timeo=30,retrans=1"), t)
                .unwrap();
        }
        format!("NFS ({export})")
    } else {
        for t in [&share, &net] {
            children.push(bindfs(&remote, t));
            mounts.adopt(t).unwrap();
        }
        "FUSE (fuse.bindfs)".to_owned()
    };
    println!("FX-LNX-03: network mounts at share/ and proj/target/net: {kind}");
    if nfs.is_some() {
        // The export may be empty: give it the remote project (written through the mount).
        let p = share.join("scull-fx-remote");
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(p.join("app/target/debug")).unwrap();
        fs::write(p.join("app/Cargo.toml"), "[package]\nname = \"r\"\n").unwrap();
        fs::write(p.join("app/Cargo.lock"), "version = 4\n").unwrap();
        fs::write(p.join("app/target/debug/r"), [0u8; 2048]).unwrap();
    }
    fx.checkpoint().unwrap();

    let h = harness();
    let r = scan_root(&h, fx.root());
    let root = fx.root();
    for c in &r.candidates {
        let rel = rel_to(root, &c.path.to_path_buf());
        assert!(
            !rel.starts_with("share/") && !rel.starts_with("proj/target/net/"),
            "content of a network mount was classified: {rel}"
        );
    }
    let proj = cand(&r, root, "proj/target");
    assert_eq!(proj.tier, Tier::S3, "{:?}", proj.reasons);
    assert!(
        proj.reasons
            .iter()
            .any(|x| x.code == ReasonCode::CrossesMount)
    );
    println!(
        "FX-LNX-03: proj/target -> {} ({:?})",
        proj.tier, proj.reasons[0].code
    );
    let skipped: Vec<String> = r
        .issues
        .iter()
        .filter(|i| i.kind == IssueKind::OtherVolume)
        .map(|i| rel_to(root, &i.path.to_path_buf()))
        .collect();
    println!("FX-LNX-03: not descended (other volume): {skipped:?}");
    assert!(skipped.iter().any(|s| s == "share"), "{:?}", r.issues);

    // Default configuration: a scan root on a network volume is refused.
    let default_engine = Engine::new(EngineConfig::new(Config::default(), Arc::new(Native)))
        .expect("default engine");
    let refused = default_engine
        .prepare_roots(std::slice::from_ref(&share))
        .unwrap_err();
    println!("FX-LNX-03: scan root on the network mount: refused: {refused}");
    assert!(refused.to_string().contains("network"), "{refused}");

    // A FUSE mount whose daemon is frozen: any access to it blocks. The scan must finish.
    if has_bindfs {
        let hang = fx.root().join("hang");
        let frozen = bindfs(&remote, &hang);
        mounts.adopt(&hang).unwrap();
        signal(frozen.id(), "STOP");
        // Prove it is frozen: once the kernel's cached attributes expire (1 s by default), a
        // listing of the mount blocks (this probe thread is released by SIGCONT below).
        std::thread::sleep(Duration::from_secs(2));
        let (ptx, prx) = mpsc::channel();
        let probe_dir = hang.clone();
        std::thread::spawn(move || {
            let _ = ptx.send(fs::read_dir(&probe_dir).map(|d| d.count()));
        });
        assert!(
            prx.recv_timeout(Duration::from_secs(3)).is_err(),
            "the frozen FUSE mount still answers: the hang sub-case would prove nothing"
        );
        println!("FX-LNX-03: hang/ is frozen: a plain listing of it blocks");
        let (tx, rx) = mpsc::channel();
        let scan_dir = fx.root().to_path_buf();
        let t = Instant::now();
        std::thread::spawn(move || {
            let h = harness();
            let _ = tx.send(scan_root(&h, &scan_dir));
        });
        let outcome = rx.recv_timeout(Duration::from_secs(180));
        let secs = t.elapsed().as_secs_f64();
        signal(frozen.id(), "CONT");
        let frozen_report = outcome.expect("the scan hung on a frozen FUSE mount (T-53)");
        let issues: Vec<String> = frozen_report
            .issues
            .iter()
            .map(|i| {
                format!(
                    "{:?} at {:?}: {}",
                    i.kind,
                    rel_to(root, &i.path.to_path_buf()),
                    i.message
                )
            })
            .collect();
        println!(
            "FX-LNX-03: frozen FUSE mount at hang/: scan finished in {secs:.1} s; issues: {issues:#?}"
        );
        // Nothing behind the frozen mount was classified, and whatever the scan could not
        // read is disclosed as an issue, never reported as a complete, empty scan.
        assert!(
            frozen_report
                .issues
                .iter()
                .any(|i| matches!(i.kind, IssueKind::TimedOut | IssueKind::OtherVolume)),
            "{issues:?}"
        );
        for c in &frozen_report.candidates {
            let rel = rel_to(root, &c.path.to_path_buf());
            assert!(
                !rel.starts_with("hang"),
                "classified behind a frozen mount: {rel}"
            );
            if rel == "proj/target" {
                assert_eq!(c.tier, Tier::S3);
            }
        }
        children.push(frozen);
    } else {
        println!("FX-LNX-03: frozen-mount sub-case skipped (needs bindfs)");
    }
    // Every mount is still in place (and unfrozen): nothing under the root changed, except
    // the `hang/` mount added after the baseline.
    fx.check_invariants(&InvariantInputs::new().allow("hang"))
        .assert_ok();
    mounts.unmount_all().unwrap();
    for mut c in children {
        let _ = c.wait();
    }
    assert!(remote.join("app/target/debug/remote").is_file());
    println!("FX-LNX-03: network mounts skipped; no hang; invariants hold");
}

/// FX-LNX-06: a Btrfs subvolume inside a candidate is a mount boundary: the candidate is S3
/// and can't be planned; executing its sibling's plan deletes the sibling only.
#[test]
#[ignore = "nightly: loop-mounts a Btrfs image as root; run with --ignored"]
#[cfg(target_os = "linux")]
fn fx_lnx_06_btrfs_subvolume_inside_candidate_is_a_mount_boundary() {
    use exec::engine::{ctx, inputs, locked, print_report, start, store};
    use scull_cleaner::{QuarantineSettings, Revalidation};
    use scull_core::{OperationId, Plan, PlanId, PlanOrigin, PlanParams};
    use scull_engine::now_unix;
    use scull_fixtures::privileged::tool_path;
    use scull_storage::OperationEnd;

    let how = scull_fixtures::require_privileges!(Need::Root);
    println!("FX-LNX-06: privileges: {how}");
    let (Some(_), Some(btrfs)) = (tool_path("mkfs.btrfs"), tool_path("btrfs")) else {
        scull_fixtures::privileged::skip(
            "fx_lnx_06_btrfs_subvolume_inside_candidate_is_a_mount_boundary",
            "btrfs-progs (mkfs.btrfs, btrfs) is not installed",
        );
        return;
    };

    let mut fx = Fixture::new().unwrap();
    fx.dir("btr").unwrap();
    let mut mounts = Mounts::new(&fx);
    let btr = fx.root().join("btr");
    mounts
        .loop_image(&fx.base().join("btrfs.img"), "btrfs", 160, &btr)
        .unwrap();
    for p in ["sub", "plain"] {
        fx.cargo_project(format!("btr/{p}"), p)
            .unwrap()
            .cargo_target(format!("btr/{p}"), p, 8192)
            .unwrap();
    }
    let subvol = btr.join("sub/target/vol");
    let out = std::process::Command::new(&btrfs)
        .args(["subvolume", "create"])
        .arg(&subvol)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::write(subvol.join("precious.bin"), [9u8; 4096]).unwrap();
    fs::create_dir(subvol.join("nested")).unwrap();
    fs::write(subvol.join("nested/data.txt"), "subvolume data\n").unwrap();
    fx.checkpoint().unwrap();
    let sub_before = TreeSnapshot::capture(btr.join("sub")).unwrap();
    println!("FX-LNX-06: Btrfs image at btr/, subvolume at btr/sub/target/vol");

    let h = harness();
    let r = scan_root(&h, &btr);
    let sub = cand(&r, &btr, "sub/target");
    let plain = cand(&r, &btr, "plain/target");
    println!(
        "FX-LNX-06: sub/target -> {} ({:?}); plain/target -> {} {:?}",
        sub.tier, sub.reasons[0].code, plain.tier, plain.action
    );
    assert_eq!(sub.tier, Tier::S3, "{:?}", sub.reasons);
    assert!(
        sub.reasons
            .iter()
            .any(|x| x.code == ReasonCode::CrossesMount)
    );
    assert_eq!(sub.flags.cross_mount, Some(true));
    let refused = PlanItem::from_candidate(sub).unwrap_err();
    println!(
        "FX-LNX-06: planning sub/target: refused: {}",
        refused.message
    );
    assert!(plain.tier < Tier::S3, "{:?}", plain.reasons);

    // Execute the sibling's plan through the real Executor (revalidated against btr/).
    let plan = Plan::new(
        PlanParams {
            id: PlanId::from_suffix("PXL06A").unwrap(),
            scan_id: r.scan_id.clone(),
            created_unix: now_unix(),
            ttl_secs: None,
            machine_id: exec::engine::MACHINE.into(),
            user: exec::engine::USER.into(),
            origin: PlanOrigin::Cli,
        },
        vec![PlanItem::from_candidate(plain).unwrap()],
    )
    .unwrap();
    let mut s = store(&fx);
    s.insert_plan(&plan).unwrap();
    let op = OperationId::from_suffix("OXL06A").unwrap();
    let exec = locked(&fx);
    let mut operation = exec
        .begin_operation(&mut s, &plan, &start(&plan, &op))
        .unwrap()
        .with_quarantine(QuarantineSettings {
            base: Some(fx.base().to_path_buf()),
            ..QuarantineSettings::default()
        });
    let reval = Revalidation::new(&h.engine, &[canon(&btr)]).with_request(request());
    let report = exec
        .execute_plan(
            &plan,
            &ctx(),
            &reval,
            Some(&mut operation),
            &CancelToken::new(),
        )
        .unwrap();
    operation.finish(OperationEnd::Normal, now_unix()).unwrap();
    print_report(&fx, &report);
    assert!(
        !btr.join("plain/target").exists(),
        "the sibling was executed"
    );
    let d = sub_before.diff(&TreeSnapshot::capture(btr.join("sub")).unwrap());
    assert!(d.is_empty(), "the subvolume's candidate changed: {d}");
    let list = std::process::Command::new(&btrfs)
        .args(["subvolume", "list"])
        .arg(&btr)
        .output()
        .unwrap();
    let list = String::from_utf8_lossy(&list.stdout);
    assert!(list.contains("sub/target/vol"), "{list}");
    println!("FX-LNX-06: subvolume still there: {}", list.trim());

    fx.check_invariants(&inputs(&plan, &report, &s, &op, &["btr/plain/target"]))
        .assert_ok();
    drop(exec);
    mounts.unmount_all().unwrap();
}

// ---------------------------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------------------------

/// FX-MAC-05: an item on an attached disk image goes to that volume's `.Trashes/<uid>`, found
/// by the platform layer; reported as moved.
#[test]
#[ignore = "nightly: attaches a disk image with hdiutil; run with --ignored"]
#[cfg(target_os = "macos")]
fn fx_mac_05_trash_on_an_attached_disk_image_uses_its_trashes() {
    use std::os::unix::fs::PermissionsExt;

    let how = scull_fixtures::require_privileges!(Need::Mounts);
    println!("FX-MAC-05: privileges: {how}");
    let mut fx = Fixture::new().unwrap();
    fx.dir("vol").unwrap().dir("xdg").unwrap();
    let mut mounts = Mounts::new(&fx);
    let vol = fx.root().join("vol");
    mounts
        .disk_image(&fx.base().join("vol.dmg"), "scullfx05", 32, &vol)
        .unwrap();
    npm_without_lock(&mut fx, "vol/web");
    // Make an existing `.Trashes` (d-wx-wx-wt) readable so the fixture can snapshot it.
    let trashes = vol.join(".Trashes");
    if trashes.exists() {
        let _ = fs::set_permissions(&trashes, fs::Permissions::from_mode(0o1777));
    }
    fx.checkpoint().unwrap();

    let (item, report, _) = trash_on_mount(&fx, &vol);
    let uid = current_uid();
    let dir = canon(&vol).join(format!(".Trashes/{uid}"));
    assert_eq!(report.outcome.kind, TrashKind::VolumeShared);
    assert_eq!(report.outcome.location, Some(dir.join("node_modules")));
    assert_eq!(report.outcome.info_file, None);
    let _ = fs::set_permissions(&trashes, fs::Permissions::from_mode(0o1777));
    println!("FX-MAC-05: per-volume .Trashes {} used", dir.display());

    let mut inputs = InvariantInputs::new()
        .plan([(item.item_id.clone(), item.tier, Action::Trash)])
        .allow("vol/web/node_modules")
        .allow("vol/.Trashes");
    // macOS bookkeeping on the attached volume (FSEvents log, Spotlight index).
    for b in ["vol/.fseventsd", "vol/.Spotlight-V100"] {
        inputs = inputs.allow(b);
    }
    fx.check_invariants(&inputs).assert_ok();
    mounts.unmount_all().unwrap();
}
