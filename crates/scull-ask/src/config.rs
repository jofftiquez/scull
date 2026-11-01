//! `ask.toml` and the `SCULL_ASK_*` environment variables: which provider, model and endpoint
//! the user chose, and where their API key comes from.
//!
//! The assistant is **off by default** (`enabled = false`). scull never writes this file and
//! never stores the API key: it is read from the environment variable the user names
//! (`api_key_env`), the provider's usual variable (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`), or,
//! last, an `api_key` the user wrote into `ask.toml` themselves. [`ApiKey`] never prints its
//! value.

use crate::endpoint::Endpoint;
use scull_core::{ErrorKind, ScullError};
use serde::Deserialize;
use std::fmt;
use std::time::Duration;

/// The file, in scull's config directory next to `config.toml`.
pub const CONFIG_FILE: &str = "ask.toml";

/// Default model for the Anthropic provider.
pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-opus-5-5";

/// Default most model calls per question (each tool round trip is one call).
pub const DEFAULT_MAX_TURNS: u32 = 10;

/// Default `max_tokens` of each Anthropic request.
pub const DEFAULT_MAX_TOKENS: u32 = 16_000;

/// Default timeout of one HTTP request.
pub const DEFAULT_TIMEOUT_SECS: u64 = 180;

/// Which API shape the endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// The Anthropic Messages API (`POST {base_url}/v1/messages`).
    Anthropic,
    /// An OpenAI-compatible Chat Completions API (`POST {base_url}/chat/completions`): OpenAI,
    /// or a local server such as Ollama (`http://localhost:11434/v1`) or LM Studio
    /// (`http://localhost:1234/v1`).
    #[serde(rename = "openai")]
    OpenAi,
}

impl ProviderKind {
    /// Parse `anthropic` / `openai`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "anthropic" => Some(Self::Anthropic),
            "openai" | "openai-compatible" => Some(Self::OpenAi),
            _ => None,
        }
    }

    /// Stable lowercase name (`anthropic`, `openai`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
        }
    }

    /// Human name.
    pub fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::OpenAi => "OpenAI-compatible",
        }
    }

    /// The endpoint used when `base_url` is not set.
    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com",
            Self::OpenAi => "https://api.openai.com/v1",
        }
    }

    /// The environment variable the provider's own tools read the key from.
    pub fn default_key_env(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::OpenAi => "OPENAI_API_KEY",
        }
    }
}

/// An API key. Its value is only ever written into the authentication header of a request to
/// the configured endpoint: `Debug` prints `ApiKey(<redacted>)`, and there is no `Display` or
/// `Serialize`.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct ApiKey(String);

impl ApiKey {
    /// Wrap a key.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The secret, for the authentication header (and for scrubbing it from error text).
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

/// Where the key came from (shown to the user; never the value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// No key found.
    None,
    /// An environment variable.
    Env(String),
    /// `api_key` in `ask.toml`.
    File,
}

impl fmt::Display for KeySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("not set"),
            Self::Env(v) => write!(f, "from ${v}"),
            Self::File => f.write_str("from api_key in ask.toml"),
        }
    }
}

/// `ask.toml` as written (every key optional; unknown keys are an error).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AskFile {
    enabled: Option<bool>,
    provider: Option<ProviderKind>,
    model: Option<String>,
    base_url: Option<String>,
    api_key_env: Option<String>,
    api_key: Option<ApiKey>,
    effort: Option<String>,
    max_turns: Option<u32>,
    max_tokens: Option<u32>,
    timeout_secs: Option<u64>,
    fallback: Option<bool>,
}

/// The resolved assistant settings.
#[derive(Debug, Clone)]
pub struct AskConfig {
    /// `enabled` (default false). Nothing is ever sent while this is false.
    pub enabled: bool,
    /// The API shape.
    pub provider: ProviderKind,
    /// Model name (`claude-opus-5-5` by default for Anthropic; required for OpenAI-compatible).
    pub model: String,
    /// Where requests go.
    pub endpoint: Endpoint,
    /// The key, if one was found.
    pub key: Option<ApiKey>,
    /// Where the key came from.
    pub key_source: KeySource,
    /// Anthropic `output_config.effort` (`low` … `max`); the model's default when unset.
    pub effort: Option<String>,
    /// Most model calls per question.
    pub max_turns: u32,
    /// `max_tokens` of each Anthropic request (OpenAI-compatible servers use their default).
    pub max_tokens: u32,
    /// Timeout of one HTTP request.
    pub timeout: Duration,
    /// Ask Anthropic to retry a declined request on its recommended fallback model (Claude API
    /// only; default true).
    pub fallback: bool,
}

const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

fn config_err(msg: impl Into<String>) -> ScullError {
    ScullError::new(ErrorKind::Config, msg)
}

impl AskConfig {
    /// Resolve `ask.toml` (`file`, if it exists) and the environment (`env`, normally
    /// `|k| std::env::var(k).ok()`). Environment variables win over the file:
    /// `SCULL_ASK_ENABLED` (`1`/`true`/`0`/`false`), `SCULL_ASK_PROVIDER`, `SCULL_ASK_MODEL`,
    /// `SCULL_ASK_BASE_URL`. An invalid setting is a configuration error (exit 2).
    pub fn resolve<F: Fn(&str) -> Option<String>>(
        file: Option<&str>,
        env: F,
    ) -> Result<Self, ScullError> {
        let env = |k: &str| env(k).filter(|v| !v.trim().is_empty());
        let f: AskFile = match file {
            Some(text) => toml::from_str(text)
                .map_err(|e| config_err(format!("invalid {CONFIG_FILE}: {}", e.message())))?,
            None => AskFile::default(),
        };
        let enabled = match env("SCULL_ASK_ENABLED") {
            Some(v) => match v.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => true,
                "0" | "false" | "no" | "off" => false,
                _ => return Err(config_err("SCULL_ASK_ENABLED must be true or false")),
            },
            None => f.enabled.unwrap_or(false),
        };
        let provider = match env("SCULL_ASK_PROVIDER") {
            Some(v) => ProviderKind::parse(&v)
                .ok_or_else(|| config_err("SCULL_ASK_PROVIDER must be `anthropic` or `openai`"))?,
            None => f.provider.unwrap_or(ProviderKind::Anthropic),
        };
        let model = match env("SCULL_ASK_MODEL").or(f.model) {
            Some(m) if !m.trim().is_empty() => m.trim().to_owned(),
            _ => match provider {
                ProviderKind::Anthropic => DEFAULT_ANTHROPIC_MODEL.to_owned(),
                // There is no sensible default across OpenAI, Ollama and LM Studio.
                ProviderKind::OpenAi => String::new(),
            },
        };
        let base = env("SCULL_ASK_BASE_URL")
            .or(f.base_url)
            .unwrap_or_else(|| provider.default_base_url().to_owned());
        let endpoint = Endpoint::parse(&base).map_err(|e| config_err(format!("base_url: {e}")))?;
        let (key, key_source) = match f.api_key_env.as_deref().map(str::trim) {
            Some(var) if !var.is_empty() => match env(var) {
                Some(k) => (Some(ApiKey::new(k.trim())), KeySource::Env(var.to_owned())),
                None => (None, KeySource::None),
            },
            _ => match env(provider.default_key_env()) {
                Some(k) => (
                    Some(ApiKey::new(k.trim())),
                    KeySource::Env(provider.default_key_env().to_owned()),
                ),
                None => match f.api_key {
                    Some(k) if !k.expose().trim().is_empty() => {
                        (Some(ApiKey::new(k.expose().trim())), KeySource::File)
                    }
                    _ => (None, KeySource::None),
                },
            },
        };
        if let Some(k) = &key
            && k.expose().chars().any(|c| c.is_control() || c == ' ')
        {
            return Err(config_err(
                "the API key contains spaces or control characters",
            ));
        }
        let effort = match f.effort {
            Some(e) if EFFORTS.contains(&e.as_str()) => Some(e),
            Some(_) => {
                return Err(config_err(
                    "effort must be one of low, medium, high, xhigh, max",
                ));
            }
            None => None,
        };
        let max_turns = f.max_turns.unwrap_or(DEFAULT_MAX_TURNS);
        if !(1..=50).contains(&max_turns) {
            return Err(config_err("max_turns must be between 1 and 50"));
        }
        let max_tokens = f.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
        if !(256..=64_000).contains(&max_tokens) {
            return Err(config_err("max_tokens must be between 256 and 64000"));
        }
        let timeout_secs = f.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
        if !(5..=1800).contains(&timeout_secs) {
            return Err(config_err("timeout_secs must be between 5 and 1800"));
        }
        Ok(Self {
            enabled,
            provider,
            model,
            endpoint,
            key,
            key_source,
            effort,
            max_turns,
            max_tokens,
            timeout: Duration::from_secs(timeout_secs),
            fallback: f.fallback.unwrap_or(true),
        })
    }

    /// Everything needed before a request may be sent: enabled, a model, and a key (a key is
    /// optional only for an OpenAI-compatible server on this machine, such as Ollama).
    pub fn ready(&self) -> Result<(), ScullError> {
        if !self.enabled {
            return Err(config_err(format!(
                "scull ask is off (the default). To use it, create {CONFIG_FILE} in scull's \
                 config folder with `enabled = true` and your provider (see `scull ask --help`), \
                 or set SCULL_ASK_ENABLED=1"
            )));
        }
        if self.model.is_empty() {
            return Err(config_err(format!(
                "set `model` in {CONFIG_FILE} (or SCULL_ASK_MODEL): an OpenAI-compatible \
                 endpoint has no default model"
            )));
        }
        if self.key.is_none()
            && !(self.provider == ProviderKind::OpenAi && self.endpoint.is_loopback())
        {
            return Err(config_err(format!(
                "no API key: set {} (or name another variable with `api_key_env` in {CONFIG_FILE})",
                self.provider.default_key_env()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn none(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn off_by_default_with_anthropic_defaults() {
        let c = AskConfig::resolve(None, none).unwrap();
        assert!(!c.enabled);
        assert_eq!(c.provider, ProviderKind::Anthropic);
        assert_eq!(c.model, "claude-opus-5-5");
        assert_eq!(c.endpoint.origin(), "https://api.anthropic.com");
        assert_eq!(c.key_source, KeySource::None);
        let e = c.ready().unwrap_err();
        assert_eq!(e.kind, ErrorKind::Config);
        assert!(e.message.contains("off (the default)"), "{}", e.message);
    }

    #[test]
    fn key_never_appears_in_debug() {
        let c = AskConfig::resolve(Some("enabled = true\napi_key = \"sk-secret-123\"\n"), none)
            .unwrap();
        assert_eq!(c.key_source, KeySource::File);
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("sk-secret-123"), "{dbg}");
        assert!(dbg.contains("<redacted>"));
        c.ready().unwrap();
    }

    #[test]
    fn env_key_wins_over_file_and_named_variable_wins_over_default() {
        let env = |k: &str| match k {
            "ANTHROPIC_API_KEY" => Some("from-env".to_owned()),
            "MY_KEY" => Some("from-named".to_owned()),
            _ => None,
        };
        let c = AskConfig::resolve(Some("api_key = \"from-file\""), env).unwrap();
        assert_eq!(c.key.unwrap().expose(), "from-env");
        let c = AskConfig::resolve(Some("api_key_env = \"MY_KEY\""), env).unwrap();
        assert_eq!(c.key.unwrap().expose(), "from-named");
        assert_eq!(c.key_source, KeySource::Env("MY_KEY".into()));
        // A named variable that is unset means no key (the file's default variable is not
        // silently used instead).
        let c = AskConfig::resolve(Some("api_key_env = \"UNSET\""), env).unwrap();
        assert!(c.key.is_none());
    }

    #[test]
    fn environment_overrides_the_file() {
        let env = |k: &str| match k {
            "SCULL_ASK_ENABLED" => Some("1".to_owned()),
            "SCULL_ASK_PROVIDER" => Some("openai".to_owned()),
            "SCULL_ASK_MODEL" => Some("llama3.2".to_owned()),
            "SCULL_ASK_BASE_URL" => Some("http://localhost:11434/v1".to_owned()),
            _ => None,
        };
        let c = AskConfig::resolve(Some("enabled = false\nprovider = \"anthropic\""), env).unwrap();
        assert!(c.enabled);
        assert_eq!(c.provider, ProviderKind::OpenAi);
        assert_eq!(c.model, "llama3.2");
        assert!(c.endpoint.is_loopback());
        // A local OpenAI-compatible server needs no key.
        c.ready().unwrap();
    }

    #[test]
    fn invalid_settings_are_config_errors() {
        for bad in [
            "unknown = 1",
            "provider = \"gemini\"",
            "effort = \"extreme\"",
            "max_turns = 0",
            "base_url = \"http://example.com\"",
            "base_url = \"ftp://example.com\"",
            "timeout_secs = 1",
        ] {
            let e = AskConfig::resolve(Some(bad), none).unwrap_err();
            assert_eq!(e.kind, ErrorKind::Config, "{bad}");
        }
    }

    #[test]
    fn openai_needs_a_model_and_a_key_off_this_machine() {
        let c = AskConfig::resolve(Some("enabled = true\nprovider = \"openai\""), none).unwrap();
        assert!(c.ready().unwrap_err().message.contains("model"));
        let c = AskConfig::resolve(
            Some("enabled = true\nprovider = \"openai\"\nmodel = \"gpt-x\""),
            none,
        )
        .unwrap();
        assert!(c.ready().unwrap_err().message.contains("OPENAI_API_KEY"));
    }
}
