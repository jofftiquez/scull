//! [`Mounts`]: real mounts inside a fixture's temporary directory, for the privileged nightly
//! fixtures (FX-FS-04, FX-LNX-02/03/06, FX-MAC-05). Get the privileges first with
//! [`require_privileges!`](crate::require_privileges).
//!
//! Every mount point and every source must lie inside the fixture's temporary directory
//! ([`Fixture::base`]); anything else is refused. Mounts are undone in reverse order by
//! [`Mounts::unmount_all`] or on drop. Declare the `Mounts` **after** the `Fixture`, so it is
//! dropped first: the fixture's temporary directory is removed recursively, and that must
//! never walk into a still-mounted filesystem (a bind mount of the canary, for instance).
//! If an unmount fails, a lazy unmount (`umount -l`) detaches it from the tree anyway; if even
//! that fails, the process aborts rather than let the cleanup walk into it.
//!
//! | Method | Linux | macOS |
//! |---|---|---|
//! | [`Mounts::tmpfs`] | `mount -t tmpfs` (own device number) | n/a |
//! | [`Mounts::bind`] | `mount --bind` (same device number: only the mount table shows it) | n/a |
//! | [`Mounts::loop_image`] | `mkfs.<fs>` on an image file + `mount -o loop` (real root) | n/a |
//! | [`Mounts::disk_image`] | n/a | `hdiutil create` + `hdiutil attach -mountpoint` (no root) |
//! | [`Mounts::mount`] | any `mount -t <type> <source> <target>` (e.g. NFS) | n/a |
//! | [`Mounts::adopt`] | a mount made by another tool (e.g. `bindfs`, `btrfs subvolume`), unmounted like the others | same |

use crate::error::{FixtureError, Result};
use crate::fixture::Fixture;
use crate::privileged::{is_dir, tool_path};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
enum Active {
    /// Unmounted with `umount` (Linux) or `umount` / `diskutil unmount` (macOS).
    Mount(PathBuf),
    /// Detached with `hdiutil detach`.
    DiskImage(PathBuf),
}

/// Mounts made by a fixture; see the [module docs](self).
#[derive(Debug)]
pub struct Mounts {
    base: PathBuf,
    active: Vec<Active>,
}

impl Mounts {
    /// No mounts yet, confined to `fx`'s temporary directory.
    pub fn new(fx: &Fixture) -> Self {
        Self {
            base: fx.base().to_path_buf(),
            active: Vec::new(),
        }
    }

    /// Mount points still mounted, outermost first.
    pub fn mount_points(&self) -> Vec<&Path> {
        self.active
            .iter()
            .map(|a| match a {
                Active::Mount(p) | Active::DiskImage(p) => p.as_path(),
            })
            .collect()
    }

    fn inside(&self, p: &Path, what: &'static str) -> Result<()> {
        let plain = p
            .components()
            .all(|c| !matches!(c, Component::ParentDir | Component::CurDir));
        if !(p.is_absolute() && plain && p.starts_with(&self.base) && p != self.base) {
            return Err(FixtureError::unsafe_path(p, what));
        }
        Ok(())
    }

    fn target(&self, target: &Path) -> Result<()> {
        self.inside(
            target,
            "mount points must be inside the fixture's temporary directory",
        )?;
        if !is_dir(target) {
            return Err(FixtureError::unsafe_path(
                target,
                "the mount point must be an existing directory (not a link)",
            ));
        }
        Ok(())
    }

    /// Mount a fresh tmpfs of `size_mb` MiB at `target` (Linux).
    pub fn tmpfs(&mut self, target: &Path, size_mb: u32) -> Result<&mut Self> {
        self.target(target)?;
        run(Command::new(tool("mount")?)
            .args(["-t", "tmpfs", "-o"])
            .arg(format!("size={size_mb}m,mode=0755"))
            .arg("scull-fx-tmpfs")
            .arg(target))?;
        self.active.push(Active::Mount(target.to_path_buf()));
        Ok(self)
    }

    /// Bind-mount the directory `source` at `target` (Linux). Both must be inside the
    /// fixture's temporary directory.
    pub fn bind(&mut self, source: &Path, target: &Path) -> Result<&mut Self> {
        self.inside(
            source,
            "bind sources must be inside the fixture's temporary directory",
        )?;
        self.target(target)?;
        run(Command::new(tool("mount")?)
            .arg("--bind")
            .arg(source)
            .arg(target))?;
        self.active.push(Active::Mount(target.to_path_buf()));
        Ok(self)
    }

    /// `mount -t fs_type [-o options] source target` (Linux), e.g. an NFS export. `source`
    /// is not a path here and is not confined; the mount point is.
    pub fn mount(
        &mut self,
        fs_type: &str,
        source: &str,
        options: Option<&str>,
        target: &Path,
    ) -> Result<&mut Self> {
        self.target(target)?;
        let mut c = Command::new(tool("mount")?);
        c.args(["-t", fs_type]);
        if let Some(o) = options {
            c.args(["-o", o]);
        }
        run(c.arg(source).arg(target))?;
        self.active.push(Active::Mount(target.to_path_buf()));
        Ok(self)
    }

    /// Create a `size_mb` MiB image file at `image`, format it with `mkfs.<fs_type>` and
    /// loop-mount it at `target` (Linux, real root). The loop device is released on unmount
    /// (`mount -o loop` sets autoclear).
    pub fn loop_image(
        &mut self,
        image: &Path,
        fs_type: &str,
        size_mb: u64,
        target: &Path,
    ) -> Result<&mut Self> {
        self.inside(
            image,
            "images must be inside the fixture's temporary directory",
        )?;
        self.target(target)?;
        let f = std::fs::File::create(image)
            .map_err(|e| FixtureError::io("create image file", image, e))?;
        f.set_len(size_mb * 1024 * 1024)
            .map_err(|e| FixtureError::io("size image file", image, e))?;
        drop(f);
        let mkfs = tool(&format!("mkfs.{fs_type}"))?;
        let mut c = Command::new(mkfs);
        // Non-interactive on a regular file: ext* asks for confirmation without -F.
        if fs_type.starts_with("ext") {
            c.arg("-F");
        }
        if fs_type == "btrfs" {
            c.arg("-f");
        }
        run(c.arg("-q").arg(image))?;
        run(Command::new(tool("mount")?)
            .args(["-o", "loop", "-t", fs_type])
            .arg(image)
            .arg(target))?;
        self.active.push(Active::Mount(target.to_path_buf()));
        Ok(self)
    }

    /// Create a `size_mb` MiB disk image (`HFS+`, volume `name`) at `image` and attach it at
    /// `target` with `hdiutil` (macOS, no root). `image` must end in `.dmg`.
    pub fn disk_image(
        &mut self,
        image: &Path,
        name: &str,
        size_mb: u64,
        target: &Path,
    ) -> Result<&mut Self> {
        self.inside(
            image,
            "images must be inside the fixture's temporary directory",
        )?;
        self.target(target)?;
        let hdiutil = tool("hdiutil")?;
        run(Command::new(&hdiutil)
            .args(["create", "-quiet", "-size"])
            .arg(format!("{size_mb}m"))
            .args(["-fs", "HFS+", "-layout", "NONE", "-volname", name])
            .arg(image))?;
        run(Command::new(&hdiutil)
            .args([
                "attach",
                "-quiet",
                "-nobrowse",
                "-noautoopen",
                "-mountpoint",
            ])
            .arg(target)
            .arg(image))?;
        self.active.push(Active::DiskImage(target.to_path_buf()));
        Ok(self)
    }

    /// Track a mount made by another tool at `target` (inside the fixture), so it is
    /// unmounted with the others.
    pub fn adopt(&mut self, target: &Path) -> Result<&mut Self> {
        self.target(target)?;
        self.active.push(Active::Mount(target.to_path_buf()));
        Ok(self)
    }

    /// Unmount everything, innermost first. On failure the remaining mounts stay tracked
    /// (and are retried on drop).
    pub fn unmount_all(&mut self) -> Result<()> {
        while let Some(a) = self.active.last().cloned() {
            unmount(&a, false)?;
            self.active.pop();
        }
        Ok(())
    }
}

impl Drop for Mounts {
    fn drop(&mut self) {
        while let Some(a) = self.active.pop() {
            if let Err(e) = unmount(&a, false) {
                eprintln!("fixture mounts: {e}; trying a forced/lazy unmount");
                if let Err(e) = unmount(&a, true) {
                    eprintln!(
                        "fixture mounts: {e}; aborting so the temporary directory's cleanup can \
                         never walk into a mounted filesystem"
                    );
                    std::process::abort();
                }
            }
        }
    }
}

fn unmount(a: &Active, force: bool) -> Result<()> {
    match a {
        Active::Mount(p) => {
            let mut c = Command::new(tool("umount")?);
            if force {
                c.arg(if cfg!(target_os = "linux") {
                    "-l"
                } else {
                    "-f"
                });
            }
            run(c.arg(p))
        }
        Active::DiskImage(p) => {
            let mut c = Command::new(tool("hdiutil")?);
            c.args(["detach", "-quiet"]);
            if force {
                c.arg("-force");
            }
            run(c.arg(p))
        }
    }
}

fn tool(name: &str) -> Result<PathBuf> {
    tool_path(name).ok_or_else(|| FixtureError::Tool {
        command: name.to_owned(),
        status: "not found".into(),
        stderr: format!("`{name}` is not installed (looked in PATH and the sbin/bin dirs)"),
    })
}

fn run(c: &mut Command) -> Result<()> {
    let command = format!("{c:?}");
    let out = c.output().map_err(|e| FixtureError::Tool {
        command: command.clone(),
        status: e.to_string(),
        stderr: String::new(),
    })?;
    if out.status.success() {
        Ok(())
    } else {
        Err(FixtureError::Tool {
            command,
            status: out.status.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_mount_points_and_sources_outside_the_fixture() {
        let mut fx = Fixture::new().unwrap();
        fx.checkpoint().unwrap();
        let mut m = Mounts::new(&fx);
        let outside = std::env::temp_dir();
        assert!(matches!(
            m.tmpfs(&outside, 1),
            Err(FixtureError::UnsafePath { .. })
        ));
        assert!(matches!(
            m.bind(&outside, fx.root()),
            Err(FixtureError::UnsafePath { .. })
        ));
        let dotdot = fx.root().join("..").join("..");
        assert!(matches!(
            m.tmpfs(&dotdot, 1),
            Err(FixtureError::UnsafePath { .. })
        ));
        assert!(matches!(
            m.tmpfs(&fx.root().join("missing"), 1),
            Err(FixtureError::UnsafePath { .. })
        ));
        assert!(m.mount_points().is_empty());
        fx.check_invariants(&crate::InvariantInputs::new())
            .assert_ok();
    }
}
