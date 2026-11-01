//! macOS: list a directory with `getattrlistbulk(2)` (#91, FX-FS-15).
//!
//! One call returns the names, kinds and attributes of many entries, so a plain file needs no
//! `fstatat` of its own. Only what provably equals `lstat` is taken from the listing: a regular
//! file with one link, not compressed and not dataless. Everything else (directories, which
//! may be mount points; links; hard-linked, compressed or dataless files; special files;
//! entries the kernel reported an error for) is still stat'ed with `fstatat` relative to the
//! listed directory, exactly as before. The first listed metadata of each listing is checked
//! against `fstatat`; if they differ, the listing stops offering any.

use super::{EntryAt, convert_stat, fstatat_nofollow, kind_of_mode};
use crate::{DirEntry, EntryKind, Metadata, Operation, PlatformError, Result};
use scull_core::FileIdentity;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `ATTR_CMN_ERROR` (`<sys/attr.h>`): a per-entry error, packed right after
/// `ATTR_CMN_RETURNED_ATTRS`. Not in the `libc` crate.
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
/// `SF_DATALESS` (`<sys/stat.h>`): a dataless (cloud placeholder) file. Not in `libc`.
const SF_DATALESS: u32 = 0x4000_0000;
/// `enum vtype` (`<sys/vnode.h>`), as `ATTR_CMN_OBJTYPE` reports it.
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VBLK: u32 = 3;
const VCHR: u32 = 4;
const VLNK: u32 = 5;
const VSOCK: u32 = 6;
const VFIFO: u32 = 7;

const COMMON: u32 = libc::ATTR_CMN_RETURNED_ATTRS
    | ATTR_CMN_ERROR
    | libc::ATTR_CMN_NAME
    | libc::ATTR_CMN_OBJTYPE
    | libc::ATTR_CMN_MODTIME
    | libc::ATTR_CMN_ACCESSMASK
    | libc::ATTR_CMN_FLAGS
    | libc::ATTR_CMN_FILEID;
/// Every common attribute a listed file's metadata needs.
const COMMON_FOR_MD: u32 = libc::ATTR_CMN_OBJTYPE
    | libc::ATTR_CMN_MODTIME
    | libc::ATTR_CMN_ACCESSMASK
    | libc::ATTR_CMN_FLAGS
    | libc::ATTR_CMN_FILEID;
const FILE: u32 =
    libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE | libc::ATTR_FILE_DATALENGTH;

/// Bytes per `getattrlistbulk` call: a few hundred typical entries.
const BUF_LEN: usize = 32 * 1024;

/// `st_blocks` is `roundup(total allocation, 512) / 512`; [`Metadata::allocated_size`] is
/// `st_blocks * 512`.
const ST_BLOCK_UNIT: u64 = 512;

/// A `getattrlistbulk` listing of the directory open as `dir` (already checked to be the
/// directory at `path` with identity `dir_id`). `None` when the first call fails (e.g. the
/// filesystem doesn't support it): the caller then lists the directory the portable way.
pub(super) fn list(
    path: &Path,
    dir: Arc<fs::File>,
    dir_id: FileIdentity,
) -> Option<Box<dyn Iterator<Item = Result<DirEntry>> + Send>> {
    let mut it = Bulk {
        at: EntryAt {
            dir: Arc::downgrade(&dir),
            dir_id,
        },
        dir,
        dir_path: path.to_path_buf(),
        buf: vec![0; BUF_LEN],
        pos: 0,
        left: 0,
        done: false,
        trust: None,
    };
    match it.fill() {
        Ok(()) => Some(Box::new(it)),
        Err(_) => None,
    }
}

struct Bulk {
    /// Keeps the descriptor open while the listing is iterated (entries hold it weakly).
    dir: Arc<fs::File>,
    at: EntryAt,
    dir_path: PathBuf,
    buf: Vec<u8>,
    /// Offset of the next record in `buf`.
    pos: usize,
    /// Records left in `buf`.
    left: u32,
    /// The kernel reported the end of the directory, or the listing failed.
    done: bool,
    /// Whether listed metadata matched `fstatat` (`None`: not checked yet).
    trust: Option<bool>,
}

/// One parsed record.
struct Record {
    name: Vec<u8>,
    error: u32,
    objtype: Option<u32>,
    /// Listed metadata of a plain file; its identity's volume is still 0.
    md: Option<Metadata>,
}

impl Bulk {
    /// The next batch of records (`left == 0` and `done` at the end).
    #[allow(unsafe_code)]
    fn fill(&mut self) -> io::Result<()> {
        let mut attrs = libc::attrlist {
            bitmapcount: libc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: COMMON,
            volattr: 0,
            dirattr: 0,
            fileattr: FILE,
            forkattr: 0,
        };
        // SAFETY: `self.dir` is an open directory descriptor for the duration of the call;
        // `attrs` is a valid `attrlist`; `buf` is writable for `buf.len()` bytes. The kernel
        // writes at most that many bytes and returns how many records it packed.
        let n = loop {
            let n = unsafe {
                libc::getattrlistbulk(
                    self.dir.as_raw_fd(),
                    std::ptr::from_mut(&mut attrs).cast(),
                    self.buf.as_mut_ptr().cast(),
                    self.buf.len(),
                    0,
                )
            };
            if n >= 0 {
                break n;
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                self.done = true;
                return Err(e);
            }
        };
        self.pos = 0;
        self.left = n.unsigned_abs();
        self.done = n == 0;
        Ok(())
    }

    fn malformed(&mut self) -> PlatformError {
        self.done = true;
        self.left = 0;
        PlatformError::from_io(
            Operation::ReadDirEntry,
            &self.dir_path,
            io::Error::new(
                io::ErrorKind::InvalidData,
                "malformed getattrlistbulk record",
            ),
        )
    }

    /// Whether listed metadata can be used in this listing: the first time, compare it with
    /// `fstatat` of the same entry.
    fn trusted(&mut self, name: &OsStr, md: &Metadata) -> bool {
        if let Some(t) = self.trust {
            return t;
        }
        match fstatat_nofollow(&self.dir, name) {
            Ok(st) => {
                let same = convert_stat(&st) == *md;
                self.trust = Some(same);
                same
            }
            // Gone or unreadable since it was listed: decide on the next one.
            Err(_) => false,
        }
    }

    fn entry(&mut self, rec: Record) -> Result<DirEntry> {
        let file_name = OsStr::from_bytes(&rec.name).to_os_string();
        let path = self.dir_path.join(&file_name);
        let kind = match rec
            .objtype
            .filter(|_| rec.error == 0)
            .and_then(kind_of_objtype)
        {
            Some(k) => k,
            // No usable type in the listing: `lstat` it, as `DirEntry::file_type` does when
            // `readdir` gives no `d_type`.
            None => fstatat_nofollow(&self.dir, &file_name)
                .map(|st| kind_of_mode(st.st_mode))
                .map_err(|e| PlatformError::from_io(Operation::ReadDirEntry, &path, e))?,
        };
        let listed = match rec.md {
            Some(mut md) => {
                // A file is on its directory's volume (no mount point is a regular file).
                md.identity = md
                    .identity
                    .map(|id| FileIdentity::new(self.at.dir_id.volume, id.file));
                self.trusted(&file_name, &md).then_some(md)
            }
            None => None,
        };
        Ok(DirEntry {
            file_name,
            path,
            kind,
            reparse_tag: None,
            at: Some(self.at.clone()),
            listed,
        })
    }
}

impl Iterator for Bulk {
    type Item = Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.left == 0 {
                if self.done {
                    return None;
                }
                if let Err(e) = self.fill() {
                    return Some(Err(PlatformError::from_io(
                        Operation::ReadDirEntry,
                        &self.dir_path,
                        e,
                    )));
                }
                continue;
            }
            let Some((rec, len)) = parse(&self.buf, self.pos) else {
                return Some(Err(self.malformed()));
            };
            self.pos += len;
            self.left -= 1;
            return Some(self.entry(rec));
        }
    }
}

fn kind_of_objtype(t: u32) -> Option<EntryKind> {
    Some(match t {
        VREG => EntryKind::File,
        VDIR => EntryKind::Dir,
        VLNK => EntryKind::Symlink,
        VFIFO => EntryKind::Fifo,
        VSOCK => EntryKind::Socket,
        VBLK => EntryKind::BlockDevice,
        VCHR => EntryKind::CharDevice,
        _ => return None,
    })
}

/// Reads fixed-size fields at a cursor inside one record.
struct Cursor<'a> {
    rec: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let b = self.rec.get(self.at..self.at + N)?.try_into().ok()?;
        self.at += N;
        Some(b)
    }
    fn u32(&mut self) -> Option<u32> {
        self.take::<4>().map(u32::from_ne_bytes)
    }
    fn i32(&mut self) -> Option<i32> {
        self.take::<4>().map(i32::from_ne_bytes)
    }
    fn u64(&mut self) -> Option<u64> {
        self.take::<8>().map(u64::from_ne_bytes)
    }
    fn i64(&mut self) -> Option<i64> {
        self.take::<8>().map(i64::from_ne_bytes)
    }
    /// An `attrreference_t` at the cursor: the referenced bytes, without the trailing NUL.
    fn reference(&mut self) -> Option<&'a [u8]> {
        let start = self.at;
        let off = usize::try_from(self.i32()?).ok()?;
        let len = usize::try_from(self.u32()?).ok()?;
        let data = self
            .rec
            .get(start.checked_add(off)?..start.checked_add(off)?.checked_add(len)?)?;
        data.strip_suffix(&[0])
    }
}

/// The record at `pos` and its length. `None` if it doesn't fit or doesn't parse.
///
/// Layout (`getattrlist(2)`): `u32` length, the returned `attribute_set_t`, then each
/// returned attribute in bit order, except `ATTR_CMN_ERROR`, which comes first. Only returned
/// attributes are packed (no `FSOPT_PACK_INVAL_ATTRS`).
fn parse(buf: &[u8], pos: usize) -> Option<(Record, usize)> {
    let len = usize::try_from(u32::from_ne_bytes(buf.get(pos..pos + 4)?.try_into().ok()?)).ok()?;
    // At least the length and the returned attribute set: never loops in place.
    if len < 24 {
        return None;
    }
    let rec = buf.get(pos..pos.checked_add(len)?)?;
    let mut c = Cursor { rec, at: 4 };
    let common = c.u32()?;
    let _vol = c.u32()?;
    let _dir = c.u32()?;
    let file = c.u32()?;
    let _fork = c.u32()?;
    let has = |bit: u32| common & bit != 0;
    let error = if has(ATTR_CMN_ERROR) { c.u32()? } else { 0 };
    let name = if has(libc::ATTR_CMN_NAME) {
        c.reference()?
    } else {
        return None;
    };
    if name.is_empty() || name.contains(&b'/') || name.contains(&0) {
        return None;
    }
    let objtype = if has(libc::ATTR_CMN_OBJTYPE) {
        Some(c.u32()?)
    } else {
        None
    };
    let mtime = if has(libc::ATTR_CMN_MODTIME) {
        let sec = c.i64()?;
        let _nsec = c.i64()?;
        Some(sec)
    } else {
        None
    };
    let mode = if has(libc::ATTR_CMN_ACCESSMASK) {
        Some(c.u32()?)
    } else {
        None
    };
    let flags = if has(libc::ATTR_CMN_FLAGS) {
        Some(c.u32()?)
    } else {
        None
    };
    let fileid = if has(libc::ATTR_CMN_FILEID) {
        Some(c.u64()?)
    } else {
        None
    };
    let nlink = if file & libc::ATTR_FILE_LINKCOUNT != 0 {
        Some(c.u32()?)
    } else {
        None
    };
    let alloc = if file & libc::ATTR_FILE_ALLOCSIZE != 0 {
        Some(c.i64()?)
    } else {
        None
    };
    let size = if file & libc::ATTR_FILE_DATALENGTH != 0 {
        Some(c.i64()?)
    } else {
        None
    };

    let complete = common & COMMON_FOR_MD == COMMON_FOR_MD && file & FILE == FILE;
    let md = match (mtime, mode, flags, fileid, nlink, alloc, size) {
        (Some(mtime), Some(mode), Some(flags), Some(fileid), Some(1), Some(alloc), Some(size))
            if complete
                && error == 0
                && objtype == Some(VREG)
                && flags & (libc::UF_COMPRESSED | SF_DATALESS) == 0 =>
        {
            let alloc = u64::try_from(alloc).ok()?;
            Some(Metadata {
                kind: EntryKind::File,
                // Filled in by the caller's directory: a file is on its directory's volume.
                identity: Some(FileIdentity::new(0, u128::from(fileid))),
                logical_size: u64::try_from(size).ok()?,
                allocated_size: Some(alloc.div_ceil(ST_BLOCK_UNIT) * ST_BLOCK_UNIT),
                nlink: Some(1),
                mtime_unix: Some(mtime),
                readonly: mode & 0o222 == 0,
                file_attributes: None,
                reparse_tag: None,
                bsd_flags: Some(flags),
            })
        }
        _ => None,
    };
    Some((
        Record {
            name: name.to_vec(),
            error,
            objtype,
            md,
        },
        len,
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::{Native, Platform};

    /// The listing really is `getattrlistbulk`'s and offers plain files' metadata (otherwise
    /// the speed-up would be silently off), and never for directories or links.
    #[test]
    fn plain_files_come_with_listed_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        for i in 0..50 {
            fs::write(d.join(format!("f{i}")), b"x").unwrap();
        }
        fs::create_dir(d.join("sub")).unwrap();
        std::os::unix::fs::symlink("f0", d.join("link")).unwrap();
        fs::hard_link(d.join("f1"), d.join("f1-hard")).unwrap();
        let p = Native::new();
        let mut listed = 0;
        for e in p.read_dir(d).unwrap().map(Result::unwrap) {
            let name = e.file_name.to_str().unwrap().to_owned();
            if name == "sub" || name == "link" || name == "f1" || name == "f1-hard" {
                assert!(e.listed.is_none(), "{name}");
            } else if e.listed.is_some() {
                listed += 1;
                assert_eq!(e.listed.clone().unwrap(), p.metadata(&e.path).unwrap());
            }
        }
        assert_eq!(listed, 49, "plain files with listed metadata");
    }

    fn record(common: u32, file: u32, body: &[u8]) -> Vec<u8> {
        let len = u32::try_from(24 + body.len()).unwrap();
        let mut r = Vec::new();
        r.extend_from_slice(&len.to_ne_bytes());
        for g in [common, 0, 0, file, 0] {
            r.extend_from_slice(&g.to_ne_bytes());
        }
        r.extend_from_slice(body);
        r
    }

    /// A name reference: offset from the reference itself, length with the NUL.
    fn name_ref(name: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&8i32.to_ne_bytes());
        b.extend_from_slice(&u32::try_from(name.len() + 1).unwrap().to_ne_bytes());
        b.extend_from_slice(name);
        b.push(0);
        b
    }

    #[test]
    fn records_that_do_not_parse_are_rejected() {
        let ok = record(
            libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_NAME,
            0,
            &name_ref(b"a"),
        );
        let (r, len) = parse(&ok, 0).unwrap();
        assert_eq!(
            (r.name.as_slice(), len, r.md.is_none()),
            (&b"a"[..], ok.len(), true)
        );
        // Too short to hold the attribute set (would loop in place), cut off, no name, a name
        // with a slash, a reference outside the record.
        let mut short = ok.clone();
        short[..4].copy_from_slice(&4u32.to_ne_bytes());
        assert!(parse(&short, 0).is_none());
        assert!(parse(&ok[..ok.len() - 1], 0).is_none());
        assert!(parse(&record(libc::ATTR_CMN_RETURNED_ATTRS, 0, &[]), 0).is_none());
        let slash = record(libc::ATTR_CMN_NAME, 0, &name_ref(b"a/b"));
        assert!(parse(&slash, 0).is_none());
        let mut far = name_ref(b"a");
        far[..4].copy_from_slice(&4096i32.to_ne_bytes());
        assert!(parse(&record(libc::ATTR_CMN_NAME, 0, &far), 0).is_none());
    }

    /// Listed metadata only for a complete, error-free record of a regular file with one link
    /// that is neither compressed nor dataless.
    #[test]
    fn listed_metadata_only_for_plain_files() {
        let build = |error: Option<u32>, objtype: u32, flags: u32, nlink: u32| {
            let mut common = libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_NAME | COMMON_FOR_MD;
            let mut body = Vec::new();
            if let Some(e) = error {
                common |= ATTR_CMN_ERROR;
                body.extend_from_slice(&e.to_ne_bytes());
            }
            // The name reference points past the fixed attributes.
            let fixed = 4 + 16 + 4 + 4 + 8 + 4 + 8 + 8;
            body.extend_from_slice(&(8i32 + fixed).to_ne_bytes());
            body.extend_from_slice(&2u32.to_ne_bytes());
            body.extend_from_slice(&objtype.to_ne_bytes());
            body.extend_from_slice(&1_700_000_000i64.to_ne_bytes());
            body.extend_from_slice(&0i64.to_ne_bytes());
            body.extend_from_slice(&0o644u32.to_ne_bytes());
            body.extend_from_slice(&flags.to_ne_bytes());
            body.extend_from_slice(&42u64.to_ne_bytes());
            body.extend_from_slice(&nlink.to_ne_bytes());
            body.extend_from_slice(&4096i64.to_ne_bytes());
            body.extend_from_slice(&5i64.to_ne_bytes());
            body.extend_from_slice(b"f\0");
            record(common, FILE, &body)
        };
        let md = |r: Vec<u8>| parse(&r, 0).unwrap().0.md;
        let plain = md(build(None, VREG, 0, 1)).unwrap();
        assert_eq!(
            (
                plain.logical_size,
                plain.allocated_size,
                plain.mtime_unix,
                plain.readonly
            ),
            (5, Some(4096), Some(1_700_000_000), false)
        );
        assert_eq!(plain.identity.unwrap().file, 42);
        assert!(md(build(Some(0), VREG, 0, 1)).is_some());
        assert!(md(build(Some(libc::EACCES as u32), VREG, 0, 1)).is_none());
        assert!(md(build(None, VDIR, 0, 1)).is_none());
        assert!(md(build(None, VLNK, 0, 1)).is_none());
        assert!(md(build(None, VREG, 0, 2)).is_none());
        assert!(md(build(None, VREG, libc::UF_COMPRESSED, 1)).is_none());
        assert!(md(build(None, VREG, SF_DATALESS, 1)).is_none());
    }
}
