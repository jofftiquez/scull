//! One JSON POST to the model endpoint. [`Transport`] is the seam tests use; [`UreqTransport`]
//! is the real client (rustls + ring, OS trust store, no redirects, bounded response size).

use scull_core::{ErrorKind, ScullError};
use std::sync::Arc;
use std::time::Duration;

/// Largest response body read (bytes).
pub const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;

/// An HTTP response: status and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// Status code.
    pub status: u16,
    /// Body bytes (at most [`MAX_RESPONSE_BYTES`]).
    pub body: Vec<u8>,
}

/// Sends one request. Implementations must never log or echo header values.
pub trait Transport {
    /// POST `body` (JSON) to `url` with `headers`.
    fn post_json(
        &self,
        url: &str,
        headers: &[(&'static str, String)],
        body: &[u8],
    ) -> Result<HttpResponse, ScullError>;
}

/// The real HTTP client.
#[derive(Debug)]
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    /// A client with a per-request `timeout`. `use_env_proxy`: honor `HTTPS_PROXY` and friends
    /// (off for a server on this machine).
    pub fn new(timeout: Duration, use_env_proxy: bool) -> Self {
        let crypto = Arc::new(rustls::crypto::ring::default_provider());
        let tls = ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::Rustls)
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .unversioned_rustls_crypto_provider(crypto)
            .build();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            // A redirect could carry the API key or tool results somewhere else: never follow.
            .max_redirects(0)
            .http_status_as_error(false)
            .proxy(if use_env_proxy {
                ureq::Proxy::try_from_env()
            } else {
                None
            })
            .tls_config(tls)
            .user_agent(format!("scull/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Self { agent }
    }
}

impl Transport for UreqTransport {
    fn post_json(
        &self,
        url: &str,
        headers: &[(&'static str, String)],
        body: &[u8],
    ) -> Result<HttpResponse, ScullError> {
        let mut req = self.agent.post(url);
        for (k, v) in headers {
            req = req.header(*k, v.as_str());
        }
        let mut resp = req.send(body).map_err(|e| transport_error(&e))?;
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BYTES)
            .read_to_vec()
            .map_err(|e| transport_error(&e))?;
        Ok(HttpResponse { status, body })
    }
}

/// A connection-level failure. ureq's messages name the host and the failure (never headers).
fn transport_error(e: &ureq::Error) -> ScullError {
    let what = match e {
        ureq::Error::Timeout(_) => "the request timed out".to_owned(),
        ureq::Error::HostNotFound => "the endpoint's host name was not found".to_owned(),
        ureq::Error::ConnectionFailed => "could not connect to the endpoint".to_owned(),
        ureq::Error::BodyExceedsLimit(_) => "the response was too large".to_owned(),
        other => other.to_string(),
    };
    ScullError::new(ErrorKind::Io, format!("model request failed: {what}"))
}
