//! Read-only transport to the Docker Engine API (P5-DET-03, PRD §7.1, §10.5; PLT-11).
//!
//! The Docker detector (`scull_detectors::docker`) asks the daemon what it holds instead of
//! walking its data directory, which is root-owned on Linux and inside a VM on macOS and
//! Windows. Talking to the daemon is OS-level I/O (a Unix domain socket, a Windows named pipe,
//! or a loopback TCP port), so the transport lives here; everything else is in the detector.
//!
//! # Guarantees
//!
//! * **GET only, allow-listed paths only.** [`docker_get`] sends nothing but `GET` requests,
//!   and only for the read-only endpoints in [`is_read_only_path`] (`/_ping`, `/version`,
//!   `/system/df`, `/images/json`, `/containers/json`, `/volumes`). Anything else is refused
//!   before a connection is opened ([`DockerError::Forbidden`]), so no caller can reach a
//!   mutating endpoint (`/build/prune`, `/images/prune`, `DELETE ...`) through this module.
//! * **Fast, bounded failure.** Every request has one deadline covering connect, write and
//!   read. A missing socket, a refused connection, a permission error (the user is not in the
//!   `docker` group) or a silent daemon comes back as a [`DockerError`] instead of blocking.
//!   The request runs on a helper thread; if the deadline passes, the caller gets
//!   [`DockerError::TimedOut`] at once and the helper ends on its own socket timeout (Windows
//!   named pipes have none: the helper ends when the daemon answers or the process exits).
//! * **Bounded responses.** Bodies over [`MAX_RESPONSE_BYTES`] are refused.
//! * **Local daemons only.** `tcp://` endpoints must be loopback addresses, named pipes must be
//!   local (`\\.\pipe\...`), and `ssh://`/TLS endpoints are [`EndpointError::Unsupported`]: a
//!   remote daemon's disk is not this machine's disk.
//!
//! # Relation to SAFE-18
//!
//! The [`crate::Platform`] trait never opens sockets or pipes found while scanning. This module
//! is the one deliberate exception: it connects only to a Docker endpoint the user configured
//! (`DOCKER_HOST`, the current Docker context) or to Docker's documented default locations,
//! never to a socket discovered in a scan root.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Largest response body accepted (32 MiB). A daemon with thousands of build-cache records
/// answers `/system/df` in well under a megabyte.
pub const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// Largest response head (status line and headers) accepted.
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// Read-only endpoints [`docker_get`] may call, without the optional `/v1.NN` version prefix.
pub const READ_ONLY_ENDPOINTS: [&str; 6] = [
    "/_ping",
    "/version",
    "/system/df",
    "/images/json",
    "/containers/json",
    "/volumes",
];

/// Where a Docker daemon listens.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DockerEndpoint {
    /// A Unix domain socket (`unix:///var/run/docker.sock`).
    Unix(PathBuf),
    /// A local Windows named pipe, as a Win32 path (`\\.\pipe\docker_engine`).
    NamedPipe(String),
    /// Plain HTTP on a loopback address (`tcp://127.0.0.1:2375`).
    Tcp(SocketAddr),
}

impl DockerEndpoint {
    /// Parse a `DOCKER_HOST`-style URL: `unix://`, `npipe://`, `tcp://` or `http://`.
    ///
    /// `ssh://`, `https://`, non-loopback TCP hosts and remote named pipes are
    /// [`EndpointError::Unsupported`]; anything else unrecognised is
    /// [`EndpointError::Invalid`].
    pub fn parse(url: &str) -> Result<Self, EndpointError> {
        let url = url.trim();
        let invalid = |why: &str| EndpointError::Invalid {
            url: url.to_owned(),
            reason: why.to_owned(),
        };
        let unsupported = |why: &str| EndpointError::Unsupported {
            url: url.to_owned(),
            reason: why.to_owned(),
        };
        if url.chars().any(char::is_control) {
            return Err(invalid("contains control characters"));
        }
        let Some((scheme, rest)) = url.split_once("://") else {
            return Err(invalid(
                "expected a URL such as unix:///var/run/docker.sock",
            ));
        };
        match scheme.to_ascii_lowercase().as_str() {
            "unix" => {
                if !rest.starts_with('/') {
                    return Err(invalid("the socket path must be absolute"));
                }
                Ok(Self::Unix(PathBuf::from(rest)))
            }
            "npipe" => {
                let win = rest.replace('/', "\\");
                let lower = win.to_ascii_lowercase();
                let Some(name) = lower.strip_prefix("\\\\.\\pipe\\") else {
                    return if lower.starts_with("\\\\") && lower.contains("\\pipe\\") {
                        Err(unsupported(
                            "a named pipe on another machine is a remote daemon",
                        ))
                    } else {
                        Err(invalid("expected npipe:////./pipe/<name>"))
                    };
                };
                if name.is_empty() || name.contains('\\') {
                    return Err(invalid("expected npipe:////./pipe/<name>"));
                }
                Ok(Self::NamedPipe(win))
            }
            "tcp" | "http" => {
                let hostport = rest.trim_end_matches('/');
                if hostport.contains('/') || hostport.contains('@') || hostport.is_empty() {
                    return Err(invalid("expected tcp://<host>:<port>"));
                }
                let (host, port) = split_host_port(hostport)
                    .ok_or_else(|| invalid("expected tcp://<host>:<port>"))?;
                let ip = match host.to_ascii_lowercase().as_str() {
                    "localhost" => IpAddr::V4(Ipv4Addr::LOCALHOST),
                    h => h
                        .trim_start_matches('[')
                        .trim_end_matches(']')
                        .parse::<IpAddr>()
                        .map_err(|_| {
                            unsupported(
                                "only loopback TCP endpoints are supported; a remote daemon's \
                                 disk is not this machine's disk",
                            )
                        })?,
                };
                if !ip.is_loopback() {
                    return Err(unsupported(
                        "only loopback TCP endpoints are supported; a remote daemon's disk is \
                         not this machine's disk",
                    ));
                }
                Ok(Self::Tcp(SocketAddr::new(ip, port)))
            }
            "ssh" => Err(unsupported("ssh:// endpoints reach a remote daemon")),
            "https" => Err(unsupported("TLS endpoints are not supported")),
            _ => Err(invalid("unknown scheme")),
        }
    }

    /// The `unix://` endpoint for a socket path.
    pub fn unix(path: impl Into<PathBuf>) -> Self {
        Self::Unix(path.into())
    }
}

/// Splits `host:port`, `[v6]:port`; the port defaults to 2375 when absent.
fn split_host_port(s: &str) -> Option<(&str, u16)> {
    if let Some(rest) = s.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None if tail.is_empty() => 2375,
            None => return None,
        };
        return Some((host, port));
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => Some((h, p.parse().ok()?)),
        Some(_) => Some((s, 2375)), // bare IPv6 without brackets
        None => Some((s, 2375)),
    }
}

impl fmt::Display for DockerEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unix(p) => write!(f, "unix://{}", p.display()),
            Self::NamedPipe(p) => write!(f, "npipe://{}", p.replace('\\', "/")),
            Self::Tcp(a) => write!(f, "tcp://{a}"),
        }
    }
}

/// A `DOCKER_HOST`-style URL that can't be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointError {
    /// Not a URL scull understands.
    Invalid {
        /// The URL as given.
        url: String,
        /// Why.
        reason: String,
    },
    /// A valid endpoint scull deliberately doesn't talk to (remote, TLS, SSH).
    Unsupported {
        /// The URL as given.
        url: String,
        /// Why.
        reason: String,
    },
}

impl fmt::Display for EndpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { url, reason } => write!(f, "invalid Docker endpoint {url:?}: {reason}"),
            Self::Unsupported { url, reason } => {
                write!(f, "unsupported Docker endpoint {url:?}: {reason}")
            }
        }
    }
}

impl std::error::Error for EndpointError {}

/// A successful (`200 OK`) answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerResponse {
    /// The `Api-Version` header, when the daemon sent one.
    pub api_version: Option<String>,
    /// The body, decoded from chunked transfer encoding if needed.
    pub body: Vec<u8>,
}

/// Why a request failed. Every variant is an expected, reportable condition.
#[derive(Debug)]
pub enum DockerError {
    /// The path isn't a read-only endpoint ([`is_read_only_path`]); nothing was sent.
    Forbidden(String),
    /// The endpoint type isn't available on this OS (a named pipe on Unix, a Unix socket on
    /// Windows).
    Unsupported(String),
    /// Nothing listens there: the socket or pipe doesn't exist.
    NotFound,
    /// The OS refused access (Linux: the user isn't in the `docker` group).
    PermissionDenied,
    /// The socket exists but nothing accepts connections (daemon stopped, stale socket).
    Refused,
    /// No complete answer before the deadline.
    TimedOut,
    /// The daemon answered with a non-200 status.
    Status {
        /// HTTP status code.
        code: u16,
        /// The daemon's `message`, if any (truncated).
        message: String,
    },
    /// The answer isn't valid HTTP, or is larger than [`MAX_RESPONSE_BYTES`].
    Protocol(String),
    /// Any other I/O error.
    Io(io::Error),
}

impl fmt::Display for DockerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Forbidden(p) => write!(f, "refused to call {p:?}: not a read-only endpoint"),
            Self::Unsupported(why) => write!(f, "unsupported: {why}"),
            Self::NotFound => f.write_str("no Docker daemon socket"),
            Self::PermissionDenied => f.write_str("permission denied on the Docker socket"),
            Self::Refused => f.write_str("the Docker daemon is not accepting connections"),
            Self::TimedOut => f.write_str("the Docker daemon did not answer in time"),
            Self::Status { code, message } => write!(f, "Docker API error {code}: {message}"),
            Self::Protocol(why) => write!(f, "unexpected answer from the Docker daemon: {why}"),
            Self::Io(e) => write!(f, "I/O error talking to the Docker daemon: {e}"),
        }
    }
}

impl std::error::Error for DockerError {}

/// Whether `path_and_query` is one of the [`READ_ONLY_ENDPOINTS`], optionally behind a
/// `/v<major>.<minor>` API version prefix and followed by a query string.
///
/// Rejects anything with whitespace, control characters, `..`, `#` or a second path segment
/// after the endpoint, so a caller can't smuggle another request line or path through it.
pub fn is_read_only_path(path_and_query: &str) -> bool {
    if path_and_query
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '#' || c == '\\')
        || path_and_query.contains("..")
    {
        return false;
    }
    let (path, _query) = path_and_query
        .split_once('?')
        .unwrap_or((path_and_query, ""));
    let path = strip_version_prefix(path);
    READ_ONLY_ENDPOINTS.contains(&path)
}

/// `/v1.47/system/df` → `/system/df`; other paths unchanged.
fn strip_version_prefix(path: &str) -> &str {
    let Some(rest) = path.strip_prefix("/v") else {
        return path;
    };
    let Some(slash) = rest.find('/') else {
        return path;
    };
    let (ver, tail) = rest.split_at(slash);
    let mut parts = ver.split('.');
    let ok = matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(a), Some(b), None)
            if !a.is_empty() && !b.is_empty()
                && a.bytes().all(|c| c.is_ascii_digit())
                && b.bytes().all(|c| c.is_ascii_digit())
    );
    if ok { tail } else { path }
}

/// `GET path_and_query` from the daemon at `endpoint`, within `timeout` overall.
///
/// Only [read-only endpoints](is_read_only_path) are allowed; the request carries
/// `Connection: close` and no body. Returns the body of a `200 OK` answer.
pub fn docker_get(
    endpoint: &DockerEndpoint,
    path_and_query: &str,
    timeout: Duration,
) -> Result<DockerResponse, DockerError> {
    if !is_read_only_path(path_and_query) {
        return Err(DockerError::Forbidden(path_and_query.to_owned()));
    }
    let request = format!(
        "GET {path_and_query} HTTP/1.1\r\nHost: docker\r\nUser-Agent: scull\r\n\
         Accept: application/json\r\nConnection: close\r\n\r\n"
    );
    let endpoint = endpoint.clone();
    let deadline = Instant::now() + timeout;
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("scull-docker-api".into())
        .spawn(move || {
            let _ = tx.send(exchange(&endpoint, request.as_bytes(), deadline));
        });
    if let Err(e) = spawned {
        return Err(DockerError::Io(e));
    }
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => Err(DockerError::TimedOut),
    }
}

/// Time left before `deadline`, or `TimedOut`.
fn remaining(deadline: Instant) -> Result<Duration, DockerError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or(DockerError::TimedOut)
}

/// Map a connect error to a [`DockerError`].
fn connect_error(e: io::Error) -> DockerError {
    match e.kind() {
        io::ErrorKind::NotFound => DockerError::NotFound,
        io::ErrorKind::PermissionDenied => DockerError::PermissionDenied,
        io::ErrorKind::ConnectionRefused => DockerError::Refused,
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => DockerError::TimedOut,
        _ => DockerError::Io(e),
    }
}

/// Map a read/write error to a [`DockerError`].
fn io_error(e: io::Error) -> DockerError {
    match e.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => DockerError::TimedOut,
        _ => DockerError::Io(e),
    }
}

/// Connect, send `request`, read and decode the answer.
fn exchange(
    endpoint: &DockerEndpoint,
    request: &[u8],
    deadline: Instant,
) -> Result<DockerResponse, DockerError> {
    match endpoint {
        DockerEndpoint::Tcp(addr) => {
            let s =
                TcpStream::connect_timeout(addr, remaining(deadline)?).map_err(connect_error)?;
            run(s, request, deadline, |s, d| {
                s.set_read_timeout(Some(d))?;
                s.set_write_timeout(Some(d))
            })
        }
        DockerEndpoint::Unix(path) => unix_exchange(path, request, deadline),
        DockerEndpoint::NamedPipe(name) => pipe_exchange(name, request, deadline),
    }
}

#[cfg(unix)]
fn unix_exchange(
    path: &std::path::Path,
    request: &[u8],
    deadline: Instant,
) -> Result<DockerResponse, DockerError> {
    use std::os::unix::net::UnixStream;
    let s = UnixStream::connect(path).map_err(connect_error)?;
    run(s, request, deadline, |s, d| {
        s.set_read_timeout(Some(d))?;
        s.set_write_timeout(Some(d))
    })
}

#[cfg(not(unix))]
fn unix_exchange(
    _path: &std::path::Path,
    _request: &[u8],
    _deadline: Instant,
) -> Result<DockerResponse, DockerError> {
    Err(DockerError::Unsupported(
        "unix:// Docker endpoints are not supported on this OS".into(),
    ))
}

#[cfg(windows)]
fn pipe_exchange(
    name: &str,
    request: &[u8],
    deadline: Instant,
) -> Result<DockerResponse, DockerError> {
    /// `ERROR_PIPE_BUSY`: every pipe instance is in use; retry until the deadline.
    const ERROR_PIPE_BUSY: i32 = 231;
    let pipe = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name)
        {
            Ok(f) => break f,
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                remaining(deadline)?;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(connect_error(e)),
        }
    };
    // Synchronous pipe handles have no read timeout; `docker_get` enforces the deadline.
    run(pipe, request, deadline, |_, _| Ok(()))
}

#[cfg(not(windows))]
fn pipe_exchange(
    _name: &str,
    _request: &[u8],
    _deadline: Instant,
) -> Result<DockerResponse, DockerError> {
    Err(DockerError::Unsupported(
        "npipe:// Docker endpoints exist only on Windows".into(),
    ))
}

/// Write `request` to `conn` and read the answer, re-arming the timeout before every I/O.
fn run<C: Read + Write>(
    mut conn: C,
    request: &[u8],
    deadline: Instant,
    set_timeout: impl Fn(&C, Duration) -> io::Result<()>,
) -> Result<DockerResponse, DockerError> {
    set_timeout(&conn, remaining(deadline)?).map_err(io_error)?;
    conn.write_all(request).map_err(io_error)?;
    conn.flush().map_err(io_error)?;
    let mut buf = Vec::with_capacity(16 * 1024);
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        if let Some(done) = decode(&buf, false)? {
            return done;
        }
        // Narrow the read timeout to what's left. macOS refuses SO_RCVTIMEO with EINVAL once
        // the peer has closed its end (a daemon answering `Connection: close`, its reply still
        // buffered here): keep the earlier timeout then and read what's left. The caller's
        // deadline (`docker_get`) still bounds the exchange.
        match set_timeout(&conn, remaining(deadline)?) {
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => {}
            other => other.map_err(io_error)?,
        }
        let n = match conn.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io_error(e)),
        };
        if n == 0 {
            return match decode(&buf, true)? {
                Some(done) => done,
                None => Err(DockerError::Protocol(
                    "connection closed mid-response".into(),
                )),
            };
        }
        if buf.len() + n > MAX_RESPONSE_BYTES + MAX_HEAD_BYTES {
            return Err(DockerError::Protocol(format!(
                "response larger than {MAX_RESPONSE_BYTES} bytes"
            )));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// The finished outcome of a response, once complete.
type Decoded = Result<DockerResponse, DockerError>;

/// Decode an HTTP/1.x response from `buf`. `Ok(None)`: need more bytes. `eof`: the peer closed
/// the connection, so a body without framing ends here.
fn decode(buf: &[u8], eof: bool) -> Result<Option<Decoded>, DockerError> {
    let Some(head_end) = find(buf, b"\r\n\r\n") else {
        if buf.len() > MAX_HEAD_BYTES {
            return Err(DockerError::Protocol("response head too large".into()));
        }
        return if eof {
            Err(DockerError::Protocol(
                "connection closed before headers".into(),
            ))
        } else {
            Ok(None)
        };
    };
    let head = std::str::from_utf8(&buf[..head_end])
        .map_err(|_| DockerError::Protocol("response head is not UTF-8".into()))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    let (Some(version), Some(code)) = (parts.next(), parts.next()) else {
        return Err(DockerError::Protocol("bad status line".into()));
    };
    if !version.starts_with("HTTP/1.") {
        return Err(DockerError::Protocol("not an HTTP/1.x response".into()));
    }
    let code: u16 = code
        .parse()
        .map_err(|_| DockerError::Protocol("bad status code".into()))?;
    let mut content_length = None;
    let mut chunked = false;
    let mut api_version = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(DockerError::Protocol("bad header line".into()));
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => {
                let n: usize = value
                    .parse()
                    .map_err(|_| DockerError::Protocol("bad Content-Length".into()))?;
                if n > MAX_RESPONSE_BYTES {
                    return Err(DockerError::Protocol(format!(
                        "response larger than {MAX_RESPONSE_BYTES} bytes"
                    )));
                }
                content_length = Some(n);
            }
            "transfer-encoding" => {
                chunked = value
                    .to_ascii_lowercase()
                    .split(',')
                    .any(|t| t.trim() == "chunked");
            }
            "api-version" => api_version = Some(value.chars().take(32).collect()),
            _ => {}
        }
    }
    let raw = &buf[head_end + 4..];
    let body = if chunked {
        match dechunk(raw)? {
            Some(b) => b,
            None if eof => {
                return Err(DockerError::Protocol("truncated chunked body".into()));
            }
            None => return Ok(None),
        }
    } else if let Some(n) = content_length {
        if raw.len() < n {
            if eof {
                return Err(DockerError::Protocol("truncated body".into()));
            }
            return Ok(None);
        }
        raw[..n].to_vec()
    } else if code == 204 || code == 304 || (100..200).contains(&code) {
        Vec::new()
    } else if eof {
        raw.to_vec()
    } else {
        return Ok(None);
    };
    if code != 200 {
        return Ok(Some(Err(DockerError::Status {
            code,
            message: error_message(&body),
        })));
    }
    Ok(Some(Ok(DockerResponse { api_version, body })))
}

/// The daemon's `{"message": "..."}`, or the start of the body, at most 200 characters.
fn error_message(body: &[u8]) -> String {
    let text = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_owned))
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());
    text.chars().filter(|c| !c.is_control()).take(200).collect()
}

/// Decode a complete chunked body; `Ok(None)` if the terminating chunk hasn't arrived.
fn dechunk(mut raw: &[u8]) -> Result<Option<Vec<u8>>, DockerError> {
    let mut out = Vec::new();
    loop {
        let Some(line_end) = find(raw, b"\r\n") else {
            return Ok(None);
        };
        let size_line = std::str::from_utf8(&raw[..line_end])
            .map_err(|_| DockerError::Protocol("bad chunk size".into()))?;
        let size_hex = size_line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| DockerError::Protocol("bad chunk size".into()))?;
        if out.len().saturating_add(size) > MAX_RESPONSE_BYTES {
            return Err(DockerError::Protocol(format!(
                "response larger than {MAX_RESPONSE_BYTES} bytes"
            )));
        }
        raw = &raw[line_end + 2..];
        if size == 0 {
            // Trailers (if any) end with an empty line; we only need to know they're there.
            return Ok(
                if raw.starts_with(b"\r\n") || find(raw, b"\r\n\r\n").is_some() {
                    Some(out)
                } else {
                    None
                },
            );
        }
        if raw.len() < size + 2 {
            return Ok(None);
        }
        if &raw[size..size + 2] != b"\r\n" {
            return Err(DockerError::Protocol("chunk not followed by CRLF".into()));
        }
        out.extend_from_slice(&raw[..size]);
        raw = &raw[size + 2..];
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connection whose peer already sent the whole reply and closed: reads drain it.
    struct Closed(std::io::Cursor<Vec<u8>>, Vec<u8>);
    impl Read for Closed {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            // One byte at a time, so the timeout is re-armed many times.
            let n = b.len().min(1);
            self.0.read(&mut b[..n])
        }
    }
    impl Write for Closed {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.1.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// macOS: after the peer closed, setting SO_RCVTIMEO fails with EINVAL; the buffered
    /// reply is still read (CI flake on macos-latest, main @ ca78fee).
    #[test]
    fn a_refused_timeout_rearm_after_the_peer_closed_still_reads_the_reply() {
        let reply = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK".to_vec();
        let conn = Closed(std::io::Cursor::new(reply), Vec::new());
        let calls = std::cell::Cell::new(0);
        let r = run(
            conn,
            b"GET /_ping HTTP/1.1\r\n\r\n",
            Instant::now() + Duration::from_secs(5),
            |_, _| {
                calls.set(calls.get() + 1);
                if calls.get() == 1 {
                    Ok(())
                } else {
                    // EINVAL, as macOS reports it (ErrorKind::InvalidInput).
                    Err(io::Error::from_raw_os_error(22))
                }
            },
        )
        .unwrap();
        assert_eq!(r.body, b"OK");
        assert!(calls.get() > 2, "re-armed {} times", calls.get());
    }

    /// Any other failure to set the timeout still fails the exchange.
    #[test]
    fn other_timeout_errors_still_fail() {
        let conn = Closed(
            std::io::Cursor::new(b"HTTP/1.1 200 OK\r\n".to_vec()),
            Vec::new(),
        );
        let calls = std::cell::Cell::new(0);
        let e = run(
            conn,
            b"GET /_ping HTTP/1.1\r\n\r\n",
            Instant::now() + Duration::from_secs(5),
            |_, _| {
                calls.set(calls.get() + 1);
                if calls.get() == 1 {
                    Ok(())
                } else {
                    Err(io::Error::new(io::ErrorKind::PermissionDenied, "no"))
                }
            },
        )
        .unwrap_err();
        assert!(matches!(e, DockerError::Io(_)), "{e}");
    }

    #[test]
    fn parses_endpoints() {
        assert_eq!(
            DockerEndpoint::parse("unix:///var/run/docker.sock").unwrap(),
            DockerEndpoint::unix("/var/run/docker.sock")
        );
        assert_eq!(
            DockerEndpoint::parse("npipe:////./pipe/docker_engine").unwrap(),
            DockerEndpoint::NamedPipe(r"\\.\pipe\docker_engine".into())
        );
        assert_eq!(
            DockerEndpoint::parse("tcp://127.0.0.1:2375").unwrap(),
            DockerEndpoint::Tcp("127.0.0.1:2375".parse().unwrap())
        );
        assert_eq!(
            DockerEndpoint::parse("tcp://localhost").unwrap(),
            DockerEndpoint::Tcp("127.0.0.1:2375".parse().unwrap())
        );
        assert_eq!(
            DockerEndpoint::parse("tcp://[::1]:2376").unwrap(),
            DockerEndpoint::Tcp("[::1]:2376".parse().unwrap())
        );
        for remote in [
            "tcp://10.0.0.5:2375",
            "tcp://docker.example.com:2375",
            "ssh://me@host",
            "https://127.0.0.1:2376",
            "npipe:////server/pipe/docker_engine",
        ] {
            assert!(
                matches!(
                    DockerEndpoint::parse(remote),
                    Err(EndpointError::Unsupported { .. })
                ),
                "{remote}"
            );
        }
        for bad in [
            "",
            "/var/run/docker.sock",
            "unix://",
            "unix://relative.sock",
            "npipe://docker_engine",
            "tcp://127.0.0.1:2375/path",
            "ftp://x",
            "unix:///a\nb",
        ] {
            assert!(
                matches!(
                    DockerEndpoint::parse(bad),
                    Err(EndpointError::Invalid { .. })
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn endpoints_round_trip_through_display() {
        for url in [
            "unix:///var/run/docker.sock",
            "npipe:////./pipe/docker_engine",
            "tcp://127.0.0.1:2375",
        ] {
            assert_eq!(DockerEndpoint::parse(url).unwrap().to_string(), url);
        }
    }

    #[test]
    fn only_read_only_endpoints_are_allowed() {
        for ok in [
            "/_ping",
            "/version",
            "/system/df",
            "/system/df?type=build-cache",
            "/v1.44/system/df?type=container",
            "/images/json",
            "/images/json?filters=%7B%22dangling%22%3A%5B%22true%22%5D%7D",
            "/containers/json?all=1",
            "/volumes",
        ] {
            assert!(is_read_only_path(ok), "{ok}");
        }
        for bad in [
            "/build/prune",
            "/images/prune",
            "/containers/prune",
            "/volumes/prune",
            "/system/prune",
            "/images/abc/json",
            "/images/abc",
            "/containers/abc/kill",
            "/volumes/myvol",
            "/v1.44/build/prune",
            "/vX.1/system/df",
            "/system/df/../../build/prune",
            "/system/df HTTP/1.1\r\nX: y",
            "/system/df#x",
            "system/df",
            "",
        ] {
            assert!(!is_read_only_path(bad), "{bad:?}");
        }
    }

    #[test]
    fn forbidden_paths_never_connect() {
        // The endpoint doesn't exist; Forbidden must come back before any connect attempt.
        let ep = DockerEndpoint::unix("/nonexistent/scull-test/docker.sock");
        let e = docker_get(&ep, "/build/prune", Duration::from_secs(1)).unwrap_err();
        assert!(matches!(e, DockerError::Forbidden(_)), "{e}");
    }

    fn ok(buf: &[u8]) -> DockerResponse {
        decode(buf, false).unwrap().unwrap().unwrap()
    }

    #[test]
    fn decodes_content_length_bodies() {
        let r = ok(b"HTTP/1.1 200 OK\r\nApi-Version: 1.47\r\nContent-Length: 2\r\n\r\nOKextra");
        assert_eq!(r.body, b"OK");
        assert_eq!(r.api_version.as_deref(), Some("1.47"));
        assert!(
            decode(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nOK", false)
                .unwrap()
                .is_none()
        );
        assert!(decode(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nOK", true).is_err());
    }

    #[test]
    fn decodes_chunked_bodies() {
        let msg = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3;x=y\r\n:1}\r\n0\r\n\r\n";
        assert_eq!(ok(msg).body, b"{\"a\":1}");
        // Not complete until the terminal chunk arrives.
        for cut in 40..msg.len() - 1 {
            assert!(
                decode(&msg[..cut], false).unwrap().is_none(),
                "cut at {cut}"
            );
        }
        assert!(
            decode(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
                false
            )
            .is_err()
        );
    }

    #[test]
    fn unframed_bodies_end_at_eof() {
        let msg = b"HTTP/1.0 200 OK\r\n\r\n[1,2]";
        assert!(decode(msg, false).unwrap().is_none());
        assert_eq!(decode(msg, true).unwrap().unwrap().unwrap().body, b"[1,2]");
    }

    #[test]
    fn non_200_carries_the_daemon_message() {
        let msg = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 30\r\n\r\n{\"message\":\"client too new\"}  ";
        match decode(msg, false).unwrap().unwrap() {
            Err(DockerError::Status { code, message }) => {
                assert_eq!((code, message.as_str()), (400, "client too new"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn oversized_and_garbage_answers_are_refused() {
        let big = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_RESPONSE_BYTES + 1
        );
        assert!(decode(big.as_bytes(), false).is_err());
        assert!(decode(b"SSH-2.0-OpenSSH\r\n\r\n", false).is_err());
        assert!(decode(b"HTTP/1.1 abc OK\r\n\r\n", false).is_err());
        assert!(decode(&vec![b'x'; MAX_HEAD_BYTES + 1], false).is_err());
    }

    #[cfg(unix)]
    mod unix_socket {
        use super::super::*;
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixListener;

        /// A temp dir with a short path (Unix socket paths are limited to ~104 bytes).
        fn short_tempdir() -> tempfile::TempDir {
            let base = std::path::Path::new("/tmp");
            if base.is_dir() {
                tempfile::Builder::new()
                    .prefix("scull-dk")
                    .tempdir_in(base)
                    .unwrap()
            } else {
                tempfile::tempdir().unwrap()
            }
        }

        /// Serve one connection: record the request line, answer with `reply`.
        fn serve_once(
            listener: UnixListener,
            reply: &'static [u8],
        ) -> std::thread::JoinHandle<String> {
            std::thread::spawn(move || {
                let (s, _) = listener.accept().unwrap();
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut first = String::new();
                r.read_line(&mut first).unwrap();
                let mut line = String::new();
                while r.read_line(&mut line).unwrap() > 2 {
                    line.clear();
                }
                (&s).write_all(reply).unwrap();
                first
            })
        }

        #[test]
        fn gets_over_a_unix_socket() {
            let dir = short_tempdir();
            let sock = dir.path().join("docker.sock");
            let server = serve_once(
                UnixListener::bind(&sock).unwrap(),
                b"HTTP/1.1 200 OK\r\nApi-Version: 1.47\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nOK\r\n0\r\n\r\n",
            );
            let r = docker_get(
                &DockerEndpoint::unix(&sock),
                "/_ping",
                Duration::from_secs(5),
            )
            .unwrap();
            assert_eq!(r.body, b"OK");
            assert_eq!(server.join().unwrap(), "GET /_ping HTTP/1.1\r\n");
        }

        #[test]
        fn missing_socket_is_not_found() {
            let dir = short_tempdir();
            let ep = DockerEndpoint::unix(dir.path().join("none.sock"));
            let e = docker_get(&ep, "/_ping", Duration::from_secs(1)).unwrap_err();
            assert!(matches!(e, DockerError::NotFound), "{e}");
        }

        #[test]
        fn stale_socket_is_refused() {
            let dir = short_tempdir();
            let sock = dir.path().join("docker.sock");
            drop(UnixListener::bind(&sock).unwrap()); // file stays, nobody listens
            let e = docker_get(
                &DockerEndpoint::unix(&sock),
                "/_ping",
                Duration::from_secs(1),
            )
            .unwrap_err();
            assert!(matches!(e, DockerError::Refused), "{e}");
        }

        #[test]
        fn silent_daemon_times_out_fast() {
            let dir = short_tempdir();
            let sock = dir.path().join("docker.sock");
            let listener = UnixListener::bind(&sock).unwrap();
            // Accept but never answer.
            let hold = std::thread::spawn(move || {
                let conn = listener.accept();
                std::thread::sleep(Duration::from_millis(1500));
                drop(conn);
            });
            let t = Instant::now();
            let e = docker_get(
                &DockerEndpoint::unix(&sock),
                "/_ping",
                Duration::from_millis(300),
            )
            .unwrap_err();
            assert!(matches!(e, DockerError::TimedOut), "{e}");
            assert!(
                t.elapsed() < Duration::from_millis(1200),
                "{:?}",
                t.elapsed()
            );
            hold.join().unwrap();
        }

        #[test]
        fn named_pipes_are_unsupported_on_unix() {
            let ep = DockerEndpoint::NamedPipe(r"\\.\pipe\docker_engine".into());
            let e = docker_get(&ep, "/_ping", Duration::from_secs(1)).unwrap_err();
            assert!(matches!(e, DockerError::Unsupported(_)), "{e}");
        }
    }
}
