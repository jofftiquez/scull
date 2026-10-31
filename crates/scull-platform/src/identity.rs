//! Who and where: the account scull acts as and this installation's machine ID, the two values
//! a plan is bound to (SAFE-11, PRD §10.4: "an edited, expired, or foreign plan is rejected").
//!
//! * [`current_user`]: the OS account of this process, in a form that survives renaming the
//!   account: `uid:<effective uid>` on Unix, the token user's SID string on Windows. Under
//!   `sudo` this is `uid:0`, so a plan made by the user is foreign to root and vice versa.
//! * [`machine_id`]: a random 128-bit ID (32 lowercase hex characters) stored owner-only in
//!   scull's data directory ([`MACHINE_ID_FILE`]) on first use and read back afterwards. It is
//!   not derived from hardware or OS identifiers (nothing to fingerprint, nothing to leak) and
//!   is not a secret: it only tells this installation's plans from another's. Deleting the
//!   file makes every existing plan foreign, which is the safe direction.

use crate::EntryKind;
use crate::{ErrorKind, Operation, PlatformError, Result, create_dir_owner_only, native};
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

/// File name of the machine ID inside scull's data directory.
pub const MACHINE_ID_FILE: &str = "machine-id";

/// Length of a machine ID in hex characters.
const ID_HEX_LEN: usize = 32;

/// How often [`machine_id`] re-reads a file another process is still writing.
const ATTEMPTS: u32 = 50;

/// The OS account this process acts as: `uid:<n>` on Unix, the user SID on Windows.
pub fn current_user() -> io::Result<String> {
    native::current_user_id()
}

/// This installation's machine ID, created in `data_dir` (owner-only, with the directory if
/// missing) on first use. Concurrent first uses agree on one ID: the file is created with
/// `create_new`, and a loser reads the winner's.
///
/// Refuses ([`ErrorKind::Io`] with [`io::ErrorKind::InvalidData`]) a file that is not a
/// regular file (a link is never followed) or does not hold a valid ID; it is never
/// overwritten, since replacing it would silently re-bind plans.
pub fn machine_id(data_dir: &Path) -> Result<String> {
    let path = data_dir.join(MACHINE_ID_FILE);
    for _ in 0..ATTEMPTS {
        match read_id(&path)? {
            Read::Valid(id) => return Ok(id),
            Read::Empty => {
                // Another process created it and is about to write the ID.
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Read::Missing => {}
        }
        create_dir_owner_only(data_dir)?;
        let id = new_id();
        match native::create_file_owner_only(&path) {
            Ok(mut f) => {
                f.write_all(format!("{id}\n").as_bytes())
                    .and_then(|()| f.sync_all())
                    .map_err(|e| PlatformError::from_io(Operation::CreateFile, &path, e))?;
                return Ok(id);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(PlatformError::from_io(Operation::CreateFile, &path, e)),
        }
    }
    Err(invalid(
        &path,
        "the machine ID file stays empty; delete it so scull can create a new one (plans made \
         before then will be rejected as foreign)",
    ))
}

enum Read {
    Valid(String),
    Empty,
    Missing,
}

fn read_id(path: &Path) -> Result<Read> {
    let md = match native::os_metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Read::Missing),
        Err(e) => return Err(PlatformError::from_io(Operation::Metadata, path, e)),
    };
    if md.kind != EntryKind::File {
        return Err(invalid(
            path,
            &format!(
                "the machine ID file is a {}, not a regular file; refusing to use it",
                md.kind.as_str()
            ),
        ));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| PlatformError::from_io(Operation::Metadata, path, e))?;
    let id = text.trim();
    if id.is_empty() {
        return Ok(Read::Empty);
    }
    if is_valid_id(id) {
        Ok(Read::Valid(id.to_owned()))
    } else {
        Err(invalid(
            path,
            "the machine ID file is damaged; delete it so scull can create a new one (plans \
             made before then will be rejected as foreign)",
        ))
    }
}

fn is_valid_id(s: &str) -> bool {
    s.len() == ID_HEX_LEN && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn invalid(path: &Path, msg: &str) -> PlatformError {
    PlatformError::with_kind(
        Operation::Metadata,
        path,
        ErrorKind::Io(io::Error::new(io::ErrorKind::InvalidData, msg.to_owned())),
    )
}

/// 128 random bits from the standard library's per-process random hasher keys (seeded from
/// the OS), mixed with the time and process ID.
fn new_id() -> String {
    use std::hash::{BuildHasher, RandomState};
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let a = RandomState::new().hash_one((nanos, pid, 0u8));
    let b = RandomState::new().hash_one((nanos, pid, 1u8));
    format!("{a:016x}{b:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_id_is_created_once_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let a = machine_id(&data).unwrap();
        assert!(is_valid_id(&a), "{a}");
        assert_eq!(machine_id(&data).unwrap(), a, "stable across calls");
        assert!(crate::ensure_owner_only(&data.join(MACHINE_ID_FILE)).is_ok());

        let other = tempfile::tempdir().unwrap();
        assert_ne!(
            machine_id(other.path()).unwrap(),
            a,
            "random per installation"
        );
    }

    #[test]
    fn damaged_machine_id_is_refused_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MACHINE_ID_FILE);
        std::fs::write(&path, "not-an-id\n").unwrap();
        let e = machine_id(dir.path()).unwrap_err();
        assert!(e.to_string().contains("damaged"), "{e}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not-an-id\n");
    }

    #[cfg(unix)]
    #[test]
    fn linked_machine_id_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, format!("{}\n", "a".repeat(ID_HEX_LEN))).unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join(MACHINE_ID_FILE)).unwrap();
        let e = machine_id(dir.path()).unwrap_err();
        assert!(e.to_string().contains("not a regular file"), "{e}");
    }

    #[test]
    fn concurrent_first_use_agrees() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let ids: Vec<String> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8).map(|_| s.spawn(|| machine_id(&data))).collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap().unwrap())
                .collect()
        });
        assert!(ids.windows(2).all(|w| w[0] == w[1]), "{ids:?}");
    }

    #[test]
    fn current_user_is_stable_and_nonempty() {
        let u = current_user().unwrap();
        assert!(!u.is_empty());
        assert_eq!(current_user().unwrap(), u);
        #[cfg(unix)]
        assert!(u.starts_with("uid:"), "{u}");
        #[cfg(windows)]
        assert!(u.starts_with("S-1-"), "{u}");
    }
}
