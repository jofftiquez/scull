//! OS-independent encodings for the Trash: freedesktop `.trashinfo` files, Windows Recycle Bin
//! `$I` records, and the names items get inside a trash directory. Pure functions, compiled on
//! every OS so their tests run everywhere (the Windows record format is checked on Linux CI
//! too).

// Each OS module uses its own subset of these helpers.
#![cfg_attr(not(test), allow(dead_code))]

use std::time::{SystemTime, UNIX_EPOCH};

/// Longest name (bytes) given to an item inside a trash directory. Leaves room for a
/// collision suffix and for `.trashinfo` within the usual 255-byte name limit.
pub(crate) const MAX_TRASH_NAME: usize = 200;

/// How many names are tried before giving up on a crowded trash directory.
pub(crate) const MAX_NAME_ATTEMPTS: u32 = 1000;

/// The `n`-th candidate name for an item called `name` (`n` starts at 1).
///
/// `n == 1` is the name itself (shortened to [`MAX_TRASH_NAME`] bytes); later candidates
/// append `sep` and the number, e.g. `target.2` (freedesktop, GIO's convention) or
/// `target 2` (macOS). Valid UTF-8 is only ever cut at a character boundary, so the result
/// stays valid on filesystems that require UTF-8 names (APFS).
pub(crate) fn numbered_name(name: &[u8], n: u32, sep: u8) -> Vec<u8> {
    let mut out = truncate_name(name, MAX_TRASH_NAME).to_vec();
    if n > 1 {
        out.push(sep);
        out.extend_from_slice(n.to_string().as_bytes());
    }
    out
}

fn truncate_name(name: &[u8], max: usize) -> &[u8] {
    if name.len() <= max {
        return name;
    }
    match std::str::from_utf8(name) {
        Ok(s) => {
            let mut end = max;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            &name[..end]
        }
        Err(_) => &name[..max],
    }
}

/// Bytes kept as-is in a `.trashinfo` `Path` value: RFC 2396 unreserved characters plus the
/// path separator. Everything else is `%XX`-escaped.
fn is_unescaped(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"-_.!~*'()/".contains(&b)
}

/// URL-escapes a path's raw bytes for the `Path` key (freedesktop Trash spec, RFC 2396 §2).
pub(crate) fn escape_path(raw: &[u8]) -> String {
    let mut out = String::with_capacity(raw.len());
    for &b in raw {
        if is_unescaped(b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A local wall-clock time, as `localtime` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocalTime {
    pub(crate) year: i32,
    pub(crate) month: u32,
    pub(crate) day: u32,
    pub(crate) hour: u32,
    pub(crate) minute: u32,
    pub(crate) second: u32,
}

/// The contents of a `.trashinfo` file: the original path (absolute, or relative to the
/// volume's top directory for a per-volume trash) and the deletion time in local time,
/// `YYYY-MM-DDThh:mm:ss` (freedesktop Trash spec 1.0).
pub(crate) fn trash_info(original: &[u8], deleted: LocalTime) -> String {
    let LocalTime {
        year,
        month,
        day,
        hour,
        minute,
        second,
    } = deleted;
    format!(
        "[Trash Info]\nPath={}\nDeletionDate={year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}\n",
        escape_path(original)
    )
}

/// `secs` since the Unix epoch as a UTC wall-clock time (the fallback when the local time zone
/// can't be determined). Howard Hinnant's `civil_from_days`.
pub(crate) fn utc_time(secs: u64) -> LocalTime {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    LocalTime {
        year: i32::try_from(year).unwrap_or(i32::MAX),
        month: month as u32,
        day: day as u32,
        hour: (rem / 3600) as u32,
        minute: (rem / 60 % 60) as u32,
        second: (rem % 60) as u32,
    }
}

/// 100-ns intervals between 1601-01-01 (FILETIME epoch) and 1970-01-01.
const FILETIME_UNIX_OFFSET_SECS: u64 = 11_644_473_600;

/// `time` as a Windows FILETIME (100-ns ticks since 1601-01-01 UTC).
pub(crate) fn filetime(time: SystemTime) -> u64 {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    (since.as_secs() + FILETIME_UNIX_OFFSET_SECS)
        .saturating_mul(10_000_000)
        .saturating_add(u64::from(since.subsec_nanos() / 100))
}

/// A Windows 10+ Recycle Bin `$I` record (format version 2): version, size of the item in
/// bytes, deletion time (FILETIME, UTC), then the original path as a length-prefixed,
/// NUL-terminated UTF-16LE string. Explorer reads it to list the item and restore it to
/// `original`.
pub(crate) fn recycle_bin_record(size: u64, deleted_filetime: u64, original: &[u16]) -> Vec<u8> {
    let chars = original.len() + 1;
    let mut out = Vec::with_capacity(28 + 2 * chars);
    out.extend_from_slice(&2u64.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&deleted_filetime.to_le_bytes());
    out.extend_from_slice(&u32::try_from(chars).unwrap_or(u32::MAX).to_le_bytes());
    for unit in original.iter().copied().chain([0]) {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

/// A parsed `$I` record: (size, deletion FILETIME, original path without the NUL).
#[cfg(test)]
pub(crate) fn parse_recycle_bin_record(bytes: &[u8]) -> Option<(u64, u64, Vec<u16>)> {
    let u64_at = |at: usize| -> Option<u64> {
        Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
    };
    if u64_at(0)? != 2 {
        return None;
    }
    let size = u64_at(8)?;
    let time = u64_at(16)?;
    let chars = u32::from_le_bytes(bytes.get(24..28)?.try_into().ok()?) as usize;
    let text = bytes.get(28..28 + 2 * chars)?;
    if text.len() != 2 * chars || chars == 0 {
        return None;
    }
    let mut units: Vec<u16> = text
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    (units.pop() == Some(0)).then_some((size, time, units))
}

/// The six-character identifier Explorer uses in `$R…`/`$I…` names, from `seed`
/// (`[A-Z0-9]`). Collisions are handled by the caller (`CREATE_NEW`, retry).
pub(crate) fn recycle_id(seed: u64) -> String {
    const ALPHABET: &[u8; 36] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    // SplitMix64 finalizer: neighbouring seeds give unrelated identifiers.
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (0..6)
        .map(|i| char::from(ALPHABET[((z >> (i * 6)) % 36) as usize]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn numbered_names_keep_the_name_then_append_a_counter() {
        assert_eq!(numbered_name(b"target", 1, b'.'), b"target");
        assert_eq!(numbered_name(b"target", 2, b'.'), b"target.2");
        assert_eq!(numbered_name(b"a.txt", 3, b' '), b"a.txt 3");
    }

    #[test]
    fn long_names_are_cut_at_a_character_boundary() {
        let long = "é".repeat(150); // 300 bytes
        let cut = numbered_name(long.as_bytes(), 1, b'.');
        assert!(cut.len() <= MAX_TRASH_NAME);
        assert!(std::str::from_utf8(&cut).is_ok());
        let raw = vec![0xffu8; 300];
        assert_eq!(numbered_name(&raw, 1, b'.').len(), MAX_TRASH_NAME);
        assert!(numbered_name(&raw, 999, b'.').len() + ".trashinfo".len() <= 255);
    }

    #[test]
    fn paths_are_url_escaped_except_unreserved_and_slash() {
        assert_eq!(escape_path(b"/home/u/a b%c"), "/home/u/a%20b%25c");
        assert_eq!(escape_path(b"/x/\xff\n"), "/x/%FF%0A");
        assert_eq!(escape_path(b"proj/target-1_(x)~"), "proj/target-1_(x)~");
        assert_eq!(escape_path("é".as_bytes()), "%C3%A9");
    }

    #[test]
    fn trash_info_has_the_spec_layout() {
        let t = LocalTime {
            year: 2026,
            month: 3,
            day: 7,
            hour: 9,
            minute: 5,
            second: 1,
        };
        assert_eq!(
            trash_info(b"/home/u/my file", t),
            "[Trash Info]\nPath=/home/u/my%20file\nDeletionDate=2026-03-07T09:05:01\n"
        );
    }

    #[test]
    fn utc_time_is_the_civil_date() {
        let t = |y, mo, d, h, mi, s| LocalTime {
            year: y,
            month: mo,
            day: d,
            hour: h,
            minute: mi,
            second: s,
        };
        assert_eq!(utc_time(0), t(1970, 1, 1, 0, 0, 0));
        assert_eq!(utc_time(951_782_400), t(2000, 2, 29, 0, 0, 0));
        assert_eq!(utc_time(1_793_446_245), t(2026, 10, 31, 11, 30, 45));
    }

    #[test]
    fn filetime_epoch() {
        assert_eq!(filetime(UNIX_EPOCH), 116_444_736_000_000_000);
        assert_eq!(
            filetime(UNIX_EPOCH + Duration::from_nanos(1_000_000_150)),
            116_444_736_010_000_001
        );
    }

    /// FX-WIN-06 (format half, runs on every OS): the `$I` record Explorer reads.
    #[test]
    fn fx_win_06_recycle_bin_record_v2_round_trips() {
        let path: Vec<u16> = r"C:\Users\u\proj\target".encode_utf16().collect();
        let rec = recycle_bin_record(4096, 133_000_000_000_000_000, &path);
        assert_eq!(&rec[..8], &2u64.to_le_bytes());
        assert_eq!(&rec[24..28], &(path.len() as u32 + 1).to_le_bytes());
        assert_eq!(rec.len(), 28 + 2 * (path.len() + 1));
        assert_eq!(&rec[rec.len() - 2..], &[0, 0]);
        assert_eq!(
            parse_recycle_bin_record(&rec),
            Some((4096, 133_000_000_000_000_000, path))
        );
    }

    #[test]
    fn recycle_ids_are_six_alphanumerics_and_vary() {
        let a = recycle_id(1);
        assert_eq!(a.len(), 6);
        assert!(
            a.bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        );
        assert_ne!(a, recycle_id(2));
    }
}
