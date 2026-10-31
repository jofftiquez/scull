//! Parser for Linux `/proc/self/mountinfo` (proc(5)). OS-independent so it is unit-tested on
//! every OS; only the Linux backend uses it.
//!
//! Each line is:
//!
//! ```text
//! 36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue
//! (1)(2)(3)   (4)   (5)      (6)      (7)   (8) (9)   (10)         (11)
//! ```
//!
//! mount ID, parent ID, `major:minor`, root of the mount within its filesystem, mount point,
//! per-mount options, zero or more optional fields, a `-` separator, filesystem type, mount
//! source, superblock options. Spaces, tabs, newlines and backslashes inside paths are
//! escaped as octal (`\040`, `\011`, `\012`, `\134`).

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

/// One parsed mountinfo line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MountEntry {
    /// Device major number (`st_dev` major of entries on this mount).
    pub(crate) major: u32,
    /// Device minor number.
    pub(crate) minor: u32,
    /// Root of the mount within its filesystem (`/` unless a bind mount of a subdirectory,
    /// or a Btrfs subvolume such as `/@home`). Raw bytes, unescaped.
    pub(crate) root: Vec<u8>,
    /// Mount point, raw bytes, unescaped.
    pub(crate) mount_point: Vec<u8>,
    /// Filesystem type, with any subtype (`fuse.sshfs`).
    pub(crate) fs_type: String,
    /// Mount source (`/dev/sda1`, `server:/export`, `overlay`), unescaped.
    pub(crate) source: Vec<u8>,
    /// Mounted read-only: `ro` in the per-mount or the superblock options.
    pub(crate) read_only: bool,
}

impl MountEntry {
    /// The Linux `dev_t` for `major:minor`, as `stat` reports it in `st_dev` (glibc and musl
    /// `makedev` encoding).
    pub(crate) fn dev(&self) -> u64 {
        let (major, minor) = (u64::from(self.major), u64::from(self.minor));
        ((major & 0xffff_f000) << 32)
            | ((major & 0x0000_0fff) << 8)
            | ((minor & 0xffff_ff00) << 12)
            | (minor & 0x0000_00ff)
    }
}

/// Parses a whole mountinfo file. Malformed lines are skipped rather than failing the whole
/// table.
pub(crate) fn parse(contents: &[u8]) -> Vec<MountEntry> {
    contents
        .split(|&b| b == b'\n')
        .filter_map(parse_line)
        .collect()
}

/// Parses one line; `None` if it is malformed.
pub(crate) fn parse_line(line: &[u8]) -> Option<MountEntry> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let fields: Vec<&[u8]> = line
        .split(|&b| b == b' ')
        .filter(|f| !f.is_empty())
        .collect();
    // Fixed fields before the optional ones.
    let [_id, _parent, dev, root, mount_point, mount_opts, rest @ ..] = fields.as_slice() else {
        return None;
    };
    let sep = rest.iter().position(|f| *f == b"-")?;
    let [fs_type, source, super_opts, ..] = &rest[sep + 1..] else {
        return None;
    };
    let dev = std::str::from_utf8(dev).ok()?;
    let (major, minor) = dev.split_once(':')?;
    let has_ro = |opts: &[u8]| opts.split(|&b| b == b',').any(|o| o == b"ro");
    Some(MountEntry {
        major: major.parse().ok()?,
        minor: minor.parse().ok()?,
        root: unescape(root),
        mount_point: unescape(mount_point),
        fs_type: String::from_utf8_lossy(&unescape(fs_type)).into_owned(),
        source: unescape(source),
        read_only: has_ro(mount_opts) || has_ro(super_opts),
    })
}

/// Decodes the kernel's `\ooo` octal escapes. Anything that isn't a valid three-digit
/// escape is kept as is.
pub(crate) fn unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        if field[i] == b'\\'
            && let Some(digits) = field.get(i + 1..i + 4)
            && digits.iter().all(|d| (b'0'..=b'7').contains(d))
        {
            let value = digits
                .iter()
                .fold(0u32, |acc, d| acc * 8 + u32::from(d - b'0'));
            if let Ok(byte) = u8::try_from(value) {
                out.push(byte);
                i += 4;
                continue;
            }
        }
        out.push(field[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed real-world table: ext4 root, pseudo filesystems, a Btrfs subvolume, an NFS
    /// export, an overlay (Docker), sshfs over FUSE, a bind mount, a path with a space and a
    /// read-only squashfs.
    const FIXTURE: &str = "\
22 29 0:21 / /proc rw,nosuid,nodev,noexec,relatime shared:12 - proc proc rw
24 29 0:22 / /sys rw,nosuid,nodev,noexec,relatime shared:7 - sysfs sysfs rw
26 29 0:5 / /dev rw,nosuid,relatime shared:2 - devtmpfs udev rw,size=8k,mode=755
29 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw,errors=remount-ro
31 29 0:35 /@home /home rw,relatime shared:30 - btrfs /dev/nvme0n1p2 rw,ssd,space_cache=v2,subvolid=257,subvol=/@home
40 29 0:52 / /mnt/nfs rw,relatime shared:40 - nfs4 server:/export rw,vers=4.2,addr=10.0.0.2
41 29 0:53 / /var/lib/docker/overlay2/abc/merged rw,relatime - overlay overlay rw,lowerdir=/l,upperdir=/u,workdir=/w
42 29 0:54 / /home/u/remote rw,nosuid,nodev,relatime shared:50 - fuse.sshfs u@host:/srv rw,user_id=1000,group_id=1000
43 31 0:35 /@home/u/src /home/u/proj/target rw,relatime shared:30 - btrfs /dev/nvme0n1p2 rw,subvol=/@home
44 29 8:17 / /media/u/My\\040Disk rw,nosuid,nodev,relatime shared:60 - exfat /dev/sdb1 rw,uid=1000
45 29 7:3 / /snap/core/1 ro,nodev,relatime shared:70 - squashfs /dev/loop3 ro
";

    #[test]
    fn parses_every_line_of_the_fixture() {
        let all = parse(FIXTURE.as_bytes());
        assert_eq!(all.len(), 11);
        assert_eq!(all[3].mount_point, b"/");
        assert_eq!(all[3].fs_type, "ext4");
        assert_eq!(all[3].source, b"/dev/sda1");
        assert_eq!((all[3].major, all[3].minor), (8, 1));
        assert!(!all[3].read_only);
    }

    #[test]
    fn nfs() {
        let e = &parse(FIXTURE.as_bytes())[5];
        assert_eq!(e.fs_type, "nfs4");
        assert_eq!(e.source, b"server:/export");
        assert_eq!(e.mount_point, b"/mnt/nfs");
    }

    #[test]
    fn overlay_without_optional_fields() {
        let e = &parse(FIXTURE.as_bytes())[6];
        assert_eq!(e.fs_type, "overlay");
        assert_eq!(e.mount_point, b"/var/lib/docker/overlay2/abc/merged");
    }

    #[test]
    fn btrfs_subvolume_and_bind_mount_share_the_device() {
        let all = parse(FIXTURE.as_bytes());
        let (home, bind) = (&all[4], &all[8]);
        assert_eq!(home.fs_type, "btrfs");
        assert_eq!(home.root, b"/@home");
        assert_eq!(bind.root, b"/@home/u/src");
        // A bind mount keeps the device number: only the mount table reveals it (T-12).
        assert_eq!(home.dev(), bind.dev());
    }

    #[test]
    fn fuse_subtype_kept() {
        assert_eq!(parse(FIXTURE.as_bytes())[7].fs_type, "fuse.sshfs");
    }

    #[test]
    fn escaped_space_in_mount_point() {
        let e = &parse(FIXTURE.as_bytes())[9];
        assert_eq!(e.mount_point, b"/media/u/My Disk");
    }

    #[test]
    fn read_only_from_either_option_list() {
        assert!(parse(FIXTURE.as_bytes())[10].read_only);
        let e = parse_line(b"1 0 8:2 / /ro rw,relatime - ext4 /dev/sda2 ro,errors=continue");
        assert!(e.is_some_and(|e| e.read_only));
        let e = parse_line(b"1 0 8:2 / /rw rw,relatime - ext4 /dev/sda2 rw,errors=remount-ro");
        assert!(e.is_some_and(|e| !e.read_only));
    }

    #[test]
    fn multiple_optional_fields() {
        let e = parse_line(
            b"36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 shared:2 propagate_from:3 - ext3 /dev/root rw",
        );
        let e = e.expect("valid line");
        assert_eq!(e.mount_point, b"/mnt2");
        assert_eq!(e.root, b"/mnt1");
        assert_eq!(e.fs_type, "ext3");
        assert_eq!((e.major, e.minor), (98, 0));
    }

    #[test]
    fn malformed_lines_are_skipped() {
        assert_eq!(parse_line(b""), None);
        assert_eq!(
            parse_line(b"1 2 8:1 / /x rw shared:1 ext4 /dev/sda1 rw"),
            None
        ); // no '-'
        assert_eq!(parse_line(b"1 2 8:1 / /x rw - ext4"), None); // truncated after '-'
        assert_eq!(parse_line(b"1 2 junk / /x rw - ext4 /dev/sda1 rw"), None);
        assert_eq!(
            parse(b"garbage\n29 1 8:1 / / rw - ext4 /dev/sda1 rw\n").len(),
            1
        );
    }

    #[test]
    fn unescapes_octal_and_keeps_invalid_escapes() {
        assert_eq!(unescape(br"a\040b\011c\012d\134e"), b"a b\tc\nd\\e");
        assert_eq!(unescape(br"bad\09x\4"), br"bad\09x\4");
        assert_eq!(unescape(br"\777"), br"\777"); // > 255: not a byte, kept
    }

    #[test]
    fn dev_encoding_matches_makedev() {
        let e = |major, minor| MountEntry {
            major,
            minor,
            root: Vec::new(),
            mount_point: Vec::new(),
            fs_type: String::new(),
            source: Vec::new(),
            read_only: false,
        };
        assert_eq!(e(8, 1).dev(), 0x801);
        assert_eq!(e(0, 35).dev(), 35);
        assert_eq!(e(259, 0).dev(), 0x10300);
        assert_eq!(e(8, 300).dev(), (0x100 << 12) | (8 << 8) | 0x2c);
        #[cfg(target_os = "linux")]
        for (major, minor) in [(8u32, 1u32), (0, 35), (259, 7), (4095, 1_000_000)] {
            assert_eq!(e(major, minor).dev(), libc::makedev(major, minor));
        }
    }
}
