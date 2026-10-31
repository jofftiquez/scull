//! The best-effort I-6 socket detector: lists the internet sockets the **current process**
//! holds right now and keeps the ones that are not loopback-only.
//!
//! | OS | Source |
//! |---|---|
//! | Linux | socket inodes from `/proc/self/fd` matched against `/proc/self/net/{tcp,tcp6,udp,udp6,raw,raw6,packet}` |
//! | macOS | `/usr/sbin/lsof -nP -a -p <pid> -i -F Pn` |
//! | Windows | `%SystemRoot%\System32\netstat.exe -ano`, filtered by PID |
//!
//! Limits (why it is only *best-effort*): it sees sockets that are still open when it runs, so
//! a connection opened and closed during the fixture goes unseen; it does not see sockets of
//! child processes (e.g. `git`); and it sees the whole process, so a socket opened by another
//! test thread of the same binary is attributed to the fixture being checked.
//!
//! The parsers are plain functions over text so that all three are unit-tested on every OS.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// One internet socket held by the process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SocketEntry {
    /// Protocol as the source names it (`tcp`, `udp6`, `TCP`, `UDP`, `packet`, ...).
    pub proto: String,
    /// Local endpoint (`host:port`, `[v6]:port`, `*:port`).
    pub local: String,
    /// Remote endpoint, if connected.
    pub remote: Option<String>,
}

impl SocketEntry {
    /// Whether this socket can talk to (or be reached from) something other than this host's
    /// loopback interface. Bound to all interfaces counts as external; so does any socket
    /// whose address cannot be parsed (protective).
    pub(crate) fn is_external(&self) -> bool {
        if self.proto.starts_with("packet") {
            return true;
        }
        let local_lo = host_of(&self.local).is_some_and(is_loopback);
        let remote_ok = match &self.remote {
            None => true,
            Some(r) => host_of(r).is_some_and(|h| is_loopback(h) || is_unspecified(h)),
        };
        !(local_lo && remote_ok)
    }
}

impl fmt::Display for SocketEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.remote {
            Some(r) => write!(f, "{} {} -> {}", self.proto, self.local, r),
            None => write!(f, "{} {}", self.proto, self.local),
        }
    }
}

/// The host part of an endpoint: `1.2.3.4:5` → `1.2.3.4`, `[::1]:5` → `::1`, `*:5` → `*`,
/// `[fe80::1%5]:5` → `fe80::1`.
fn host_of(endpoint: &str) -> Option<&str> {
    let e = endpoint.trim();
    let host = if let Some(rest) = e.strip_prefix('[') {
        rest.split_once(']')?.0
    } else {
        match e.rsplit_once(':') {
            Some((h, _)) => h,
            None => e,
        }
    };
    Some(host.split('%').next().unwrap_or(host))
}

fn is_unspecified(host: &str) -> bool {
    host == "*" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_unspecified())
}

fn is_loopback(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => v4.is_loopback(),
        Ok(IpAddr::V6(v6)) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Err(_) => false,
    }
}

/// Every internet socket of the current process, and how they were found.
pub(crate) fn current_process() -> Result<(Vec<SocketEntry>, &'static str), String> {
    imp::current_process()
}

// --- Linux: /proc ----------------------------------------------------------------------------

/// Socket inode from a `/proc/<pid>/fd/<n>` link target (`socket:[12345]`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // parsed on every OS in unit tests
pub(crate) fn parse_fd_link(target: &str) -> Option<u64> {
    target
        .strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// A `/proc/net/{tcp,udp,raw}[6]` address: `0100007F:0035` (each 32-bit word printed in host
/// byte order) → `127.0.0.1:53`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // parsed on every OS in unit tests
pub(crate) fn parse_proc_addr(field: &str) -> Option<String> {
    let (addr, port) = field.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let words: Option<Vec<[u8; 4]>> = (0..addr.len() / 8)
        .map(|i| {
            u32::from_str_radix(addr.get(i * 8..i * 8 + 8)?, 16)
                .ok()
                .map(u32::to_ne_bytes)
        })
        .collect();
    let bytes: Vec<u8> = words?.concat();
    match (addr.len(), bytes.len()) {
        (8, 4) => {
            let ip = Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]);
            Some(format!("{ip}:{port}"))
        }
        (32, 16) => {
            let mut b = [0u8; 16];
            b.copy_from_slice(&bytes);
            Some(format!("[{}]:{port}", Ipv6Addr::from(b)))
        }
        _ => None,
    }
}

/// Entries of one `/proc/net/{tcp,udp,raw}[6]` table whose inode is in `inodes`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // parsed on every OS in unit tests
pub(crate) fn parse_proc_table(text: &str, proto: &str, inodes: &[u64]) -> Vec<SocketEntry> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let inode: u64 = f.get(9)?.parse().ok()?;
            if !inodes.contains(&inode) {
                return None;
            }
            let local = parse_proc_addr(f.get(1)?)?;
            let remote = parse_proc_addr(f.get(2)?)?;
            let remote = host_of(&remote)
                .is_some_and(|h| !is_unspecified(h))
                .then_some(remote);
            Some(SocketEntry {
                proto: proto.to_owned(),
                local,
                remote,
            })
        })
        .collect()
}

/// Entries of `/proc/net/packet` (link-level sockets; always external) whose inode is in
/// `inodes`. Columns: `sk RefCnt Type Proto Iface R Rmem User Inode`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // parsed on every OS in unit tests
pub(crate) fn parse_proc_packet(text: &str, inodes: &[u64]) -> Vec<SocketEntry> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let inode: u64 = f.get(8)?.parse().ok()?;
            inodes.contains(&inode).then(|| SocketEntry {
                proto: "packet".into(),
                local: format!("iface {}", f.get(4).copied().unwrap_or("?")),
                remote: None,
            })
        })
        .collect()
}

// --- macOS: lsof -F Pn -----------------------------------------------------------------------

/// `lsof -F Pn` output: `p<pid>`, then per file `f<fd>`, `P<proto>`, `n<name>` where the name
/// is `local` or `local->remote`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // parsed on every OS in unit tests
pub(crate) fn parse_lsof(text: &str) -> Vec<SocketEntry> {
    let mut out = Vec::new();
    let mut proto = String::new();
    for line in text.lines() {
        let Some(tag) = line.chars().next() else {
            continue;
        };
        let value = &line[tag.len_utf8()..];
        match tag {
            'f' => proto.clear(),
            'P' => proto = value.to_ascii_lowercase(),
            'n' => {
                let (local, remote) = match value.split_once("->") {
                    Some((l, r)) => (l.to_owned(), Some(r.to_owned())),
                    None => (value.to_owned(), None),
                };
                let proto = if proto.is_empty() { "inet" } else { &proto };
                out.push(SocketEntry {
                    proto: proto.to_owned(),
                    local,
                    remote,
                });
            }
            _ => {}
        }
    }
    out
}

// --- Windows: netstat -ano -------------------------------------------------------------------

/// `netstat -ano` rows owned by `pid`: `TCP local foreign state pid` and `UDP local *:* pid`.
/// Only the protocol and address columns are read (the state column is localized).
#[cfg_attr(not(windows), allow(dead_code))] // parsed on every OS in unit tests
pub(crate) fn parse_netstat(text: &str, pid: u32) -> Vec<SocketEntry> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let proto = *f.first()?;
            if !(proto.eq_ignore_ascii_case("TCP") || proto.eq_ignore_ascii_case("UDP")) {
                return None;
            }
            if f.last()?.parse::<u32>().ok()? != pid || f.len() < 4 {
                return None;
            }
            let local = f.get(1)?.to_string();
            let foreign = f.get(2)?;
            let remote = host_of(foreign)
                .is_some_and(|h| !is_unspecified(h))
                .then(|| foreign.to_string());
            Some(SocketEntry {
                proto: proto.to_owned(),
                local,
                remote,
            })
        })
        .collect()
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{SocketEntry, parse_fd_link, parse_proc_packet, parse_proc_table};
    use std::fs;
    use std::io::ErrorKind;

    pub(super) fn current_process() -> Result<(Vec<SocketEntry>, &'static str), String> {
        let dir = fs::read_dir("/proc/self/fd").map_err(|e| format!("/proc/self/fd: {e}"))?;
        let inodes: Vec<u64> = dir
            .filter_map(|e| fs::read_link(e.ok()?.path()).ok())
            .filter_map(|t| parse_fd_link(&t.to_string_lossy()))
            .collect();
        let mut out = Vec::new();
        if inodes.is_empty() {
            return Ok((out, "/proc/self/fd + /proc/self/net"));
        }
        for proto in ["tcp", "tcp6", "udp", "udp6", "raw", "raw6", "packet"] {
            let path = format!("/proc/self/net/{proto}");
            let text = match fs::read_to_string(&path) {
                Ok(t) => t,
                // No IPv6 (or no packet sockets) in this kernel: nothing of that kind exists.
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("{path}: {e}")),
            };
            if proto == "packet" {
                out.extend(parse_proc_packet(&text, &inodes));
            } else {
                out.extend(parse_proc_table(&text, proto, &inodes));
            }
        }
        Ok((out, "/proc/self/fd + /proc/self/net"))
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{SocketEntry, parse_lsof};
    use std::process::Command;

    const LSOF: &str = "/usr/sbin/lsof";

    pub(super) fn current_process() -> Result<(Vec<SocketEntry>, &'static str), String> {
        let pid = std::process::id().to_string();
        let out = Command::new(LSOF)
            .args(["-nP", "-a", "-p", &pid, "-i", "-F", "Pn"])
            .output()
            .map_err(|e| format!("{LSOF}: {e}"))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        // lsof exits 1 with no output when the process has no internet sockets.
        if out.status.success() || (out.status.code() == Some(1) && stderr.trim().is_empty()) {
            Ok((parse_lsof(&stdout), "lsof -i"))
        } else {
            Err(format!("{LSOF} failed ({}): {}", out.status, stderr.trim()))
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::{SocketEntry, parse_netstat};
    use std::path::PathBuf;
    use std::process::Command;

    pub(super) fn current_process() -> Result<(Vec<SocketEntry>, &'static str), String> {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        let netstat = PathBuf::from(root).join("System32").join("netstat.exe");
        let out = Command::new(&netstat)
            .arg("-ano")
            .output()
            .map_err(|e| format!("{}: {e}", netstat.display()))?;
        if !out.status.success() {
            return Err(format!(
                "{} failed ({}): {}",
                netstat.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        Ok((parse_netstat(&text, std::process::id()), "netstat -ano"))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod imp {
    use super::SocketEntry;

    pub(super) fn current_process() -> Result<(Vec<SocketEntry>, &'static str), String> {
        Err("no socket detector for this OS".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(proto: &str, local: &str, remote: Option<&str>) -> SocketEntry {
        SocketEntry {
            proto: proto.into(),
            local: local.into(),
            remote: remote.map(Into::into),
        }
    }

    #[test]
    fn loopback_only_sockets_are_not_external() {
        assert!(!e("tcp", "127.0.0.1:8080", None).is_external());
        assert!(!e("tcp", "127.0.0.1:5000", Some("127.0.0.1:8080")).is_external());
        assert!(!e("tcp6", "[::1]:5000", None).is_external());
        assert!(!e("tcp6", "[::ffff:127.0.0.1]:5000", Some("[::1]:1")).is_external());
        assert!(!e("TCP", "127.0.0.1:5000", Some("0.0.0.0:0")).is_external());
        assert!(!e("tcp", "localhost:5000", None).is_external());
    }

    #[test]
    fn bound_any_or_remote_sockets_are_external() {
        assert!(e("udp", "0.0.0.0:5353", None).is_external());
        assert!(e("udp", "*:5353", None).is_external());
        assert!(e("tcp6", "[::]:80", None).is_external());
        assert!(e("udp", "192.168.1.5:40000", Some("192.0.2.1:9")).is_external());
        assert!(e("tcp", "127.0.0.1:1", Some("192.0.2.1:9")).is_external());
        assert!(e("packet", "iface 2", None).is_external());
        assert!(
            e("tcp", "garbage", None).is_external(),
            "unparsable is external"
        );
    }

    #[test]
    fn host_of_handles_v4_v6_wildcard_and_scope() {
        assert_eq!(host_of("1.2.3.4:5"), Some("1.2.3.4"));
        assert_eq!(host_of("[::1]:5"), Some("::1"));
        assert_eq!(host_of("[fe80::1%5]:5"), Some("fe80::1"));
        assert_eq!(host_of("*:5"), Some("*"));
        assert_eq!(host_of("*:*"), Some("*"));
    }

    #[test]
    fn proc_fd_link_and_addresses_parse() {
        assert_eq!(parse_fd_link("socket:[12345]"), Some(12345));
        assert_eq!(parse_fd_link("pipe:[1]"), None);
        assert_eq!(parse_fd_link("/dev/null"), None);
        let lo = u32::from_ne_bytes([127, 0, 0, 1]);
        assert_eq!(
            parse_proc_addr(&format!("{lo:08X}:0035")).as_deref(),
            Some("127.0.0.1:53")
        );
        let words: String = [[0, 0, 0, 0], [0, 0, 0, 0], [0, 0, 0, 0], [0, 0, 0, 1]]
            .iter()
            .map(|w| format!("{:08X}", u32::from_ne_bytes(*w)))
            .collect();
        assert_eq!(
            parse_proc_addr(&format!("{words}:1F90")).as_deref(),
            Some("[::1]:8080")
        );
        assert_eq!(parse_proc_addr("zz:1"), None);
    }

    #[test]
    fn proc_table_keeps_only_this_process_inodes() {
        let a = u32::from_ne_bytes([192, 168, 1, 5]);
        let b = u32::from_ne_bytes([192, 0, 2, 1]);
        let lo = u32::from_ne_bytes([127, 0, 0, 1]);
        let text = format!(
            "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
             \x20  0: {a:08X}:9C40 {b:08X}:0009 01 00000000:00000000 00:00000000 00000000  1000        0 111 2 0\n\
             \x20  1: {lo:08X}:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 222 1 0\n\
             \x20  2: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 333 1 0\n"
        );
        let got = parse_proc_table(&text, "udp", &[111, 222]);
        assert_eq!(
            got,
            vec![
                e("udp", "192.168.1.5:40000", Some("192.0.2.1:9")),
                e("udp", "127.0.0.1:8080", None),
            ]
        );
        let packet = "sk       RefCnt Type Proto  Iface R Rmem   User   Inode\n\
                      0000000000000000 3      3    0003   2     1 0      1000   444\n";
        assert_eq!(
            parse_proc_packet(packet, &[444]),
            vec![e("packet", "iface 2", None)]
        );
        assert!(parse_proc_packet(packet, &[1]).is_empty());
    }

    #[test]
    fn lsof_field_output_parses() {
        let text = "p4242\nf5\nPUDP\nn*:56000\nf6\nPTCP\nn10.0.0.2:50000->192.0.2.1:9\nf7\nPTCP\nn[::1]:8080\n";
        assert_eq!(
            parse_lsof(text),
            vec![
                e("udp", "*:56000", None),
                e("tcp", "10.0.0.2:50000", Some("192.0.2.1:9")),
                e("tcp", "[::1]:8080", None),
            ]
        );
        assert!(parse_lsof("").is_empty());
    }

    #[test]
    fn netstat_rows_are_filtered_by_pid() {
        let text = "\r\nActive Connections\r\n\r\n  Proto  Local Address          Foreign Address        State           PID\r\n\
                    \x20 TCP    0.0.0.0:135            0.0.0.0:0              LISTENING       1000\r\n\
                    \x20 TCP    10.0.0.2:50000         192.0.2.1:9            SYN_SENT        4242\r\n\
                    \x20 TCP    [::1]:8080             [::]:0                 ABH\u{00d6}REN          4242\r\n\
                    \x20 UDP    0.0.0.0:56000          *:*                                    4242\r\n\
                    \x20 UDP    [::]:5353              *:*                                    1000\r\n";
        assert_eq!(
            parse_netstat(text, 4242),
            vec![
                e("TCP", "10.0.0.2:50000", Some("192.0.2.1:9")),
                e("TCP", "[::1]:8080", None),
                e("UDP", "0.0.0.0:56000", None),
            ]
        );
        assert!(parse_netstat(text, 7).is_empty());
    }

    /// The detector works on this host and sees a deliberately held socket, with no traffic:
    /// a UDP `connect` only records the peer.
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn detector_sees_a_held_udp_socket() {
        let _g = crate::net::lock();
        let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") else {
            assert!(
                crate::net::active_sandbox().is_some(),
                "bind refused outside a sandbox"
            );
            return;
        };
        let port = sock.local_addr().unwrap().port();
        let (found, how) = current_process().unwrap();
        assert!(
            found
                .iter()
                .any(|s| s.is_external() && s.local.ends_with(&format!(":{port}"))),
            "{how} did not see port {port}: {found:?}"
        );
        drop(sock);
        let (found, _) = current_process().unwrap();
        assert!(
            !found.iter().any(|s| s.local.ends_with(&format!(":{port}"))),
            "{found:?}"
        );
    }
}
