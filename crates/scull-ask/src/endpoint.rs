//! The model endpoint: `https://` anywhere, plain `http://` only on this machine (loopback),
//! so an API key and scull's tool results never cross a network unencrypted.

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};

/// A parsed base URL (`scheme://host[:port][/path]`, no user info, query or fragment).
#[derive(Clone, PartialEq, Eq)]
pub struct Endpoint {
    https: bool,
    host: String,
    port: u16,
    path: String,
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Endpoint({})", self.base())
    }
}

impl Endpoint {
    /// Parse a base URL. `http://` is accepted only for loopback hosts (`localhost`,
    /// `127.0.0.0/8`, `::1`).
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (https, rest) = if let Some(r) = strip_prefix_ci(s, "https://") {
            (true, r)
        } else if let Some(r) = strip_prefix_ci(s, "http://") {
            (false, r)
        } else {
            return Err(
                "must start with https:// (or http:// for a server on this machine)".into(),
            );
        };
        if rest.contains(['?', '#']) || rest.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("must be a plain URL without spaces, a query or a fragment".into());
        }
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if authority.contains('@') {
            return Err("must not contain a user name or password".into());
        }
        let (host, port) = split_host_port(authority)?;
        let port = port.unwrap_or(if https { 443 } else { 80 });
        let host = host.to_ascii_lowercase();
        let ep = Self {
            https,
            host,
            port,
            path: path.trim_end_matches('/').to_owned(),
        };
        if !https && !ep.is_loopback() {
            return Err(format!(
                "plain http:// is only allowed for a server on this machine (localhost); use \
                 https:// for {}",
                ep.host
            ));
        }
        Ok(ep)
    }

    /// Whether the host is this machine.
    pub fn is_loopback(&self) -> bool {
        if self.host == "localhost" {
            return true;
        }
        let bare = self.host.trim_start_matches('[').trim_end_matches(']');
        match bare.parse::<IpAddr>() {
            Ok(IpAddr::V4(a)) => a.is_loopback(),
            Ok(IpAddr::V6(a)) => a == Ipv6Addr::LOCALHOST,
            Err(_) => false,
        }
    }

    /// Whether this is Anthropic's own API (where server-side fallbacks exist).
    pub fn is_anthropic_api(&self) -> bool {
        self.https && self.host == "api.anthropic.com" && self.port == 443
    }

    /// `scheme://host[:port]`: what consent is recorded for.
    pub fn origin(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        let default = if self.https { 443 } else { 80 };
        if self.port == default {
            format!("{scheme}://{}", self.host)
        } else {
            format!("{scheme}://{}:{}", self.host, self.port)
        }
    }

    /// The base URL as configured (normalized).
    pub fn base(&self) -> String {
        format!("{}{}", self.origin(), self.path)
    }

    /// `base` + `suffix` (e.g. `/v1/messages`).
    pub fn join(&self, suffix: &str) -> String {
        format!("{}{suffix}", self.base())
    }
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    (s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix))
        .then(|| &s[prefix.len()..])
}

fn split_host_port(authority: &str) -> Result<(&str, Option<u16>), String> {
    let bad_port = || "has an invalid port".to_owned();
    let (host, port) = if authority.starts_with('[') {
        let end = authority
            .find(']')
            .ok_or_else(|| "has an unclosed IPv6 address".to_owned())?;
        let host = &authority[..=end];
        match &authority[end + 1..] {
            "" => (host, None),
            p => (
                host,
                Some(
                    p.strip_prefix(':')
                        .and_then(|p| p.parse().ok())
                        .ok_or_else(bad_port)?,
                ),
            ),
        }
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p.parse().map_err(|_| bad_port())?)),
            None => (authority, None),
        }
    };
    let ok_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_');
    let valid = if host.starts_with('[') {
        host.trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<Ipv6Addr>()
            .is_ok()
    } else {
        !host.is_empty() && host.chars().all(ok_char)
    };
    if !valid {
        return Err("has an invalid host name".into());
    }
    if port == Some(0) {
        return Err(bad_port());
    }
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn https_anywhere_http_only_on_loopback() {
        let a = Endpoint::parse("https://api.anthropic.com/").unwrap();
        assert!(a.is_anthropic_api() && !a.is_loopback());
        assert_eq!(
            a.join("/v1/messages"),
            "https://api.anthropic.com/v1/messages"
        );
        let o = Endpoint::parse("http://localhost:11434/v1").unwrap();
        assert!(o.is_loopback());
        assert_eq!(
            o.join("/chat/completions"),
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(o.origin(), "http://localhost:11434");
        for ok in [
            "http://127.0.0.1:9",
            "http://[::1]:8080/x",
            "HTTPS://Example.COM:8443",
        ] {
            Endpoint::parse(ok).unwrap();
        }
        for bad in [
            "http://example.com",
            "http://10.0.0.1:1234",
            "example.com",
            "https://user:pw@example.com",
            "https://example.com/?q=1",
            "https://exa mple.com",
            "https://example.com:0",
            "https://example.com:99999",
            "https://",
            "https://[::1",
        ] {
            assert!(Endpoint::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn origin_keeps_non_default_ports_only() {
        assert_eq!(
            Endpoint::parse("https://example.com:443/a/")
                .unwrap()
                .base(),
            "https://example.com/a"
        );
        assert_eq!(
            Endpoint::parse("https://example.com:8443")
                .unwrap()
                .origin(),
            "https://example.com:8443"
        );
    }
}
