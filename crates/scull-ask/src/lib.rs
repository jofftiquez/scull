//! `scull ask`: an optional, bring-your-own-model assistant over scull's read-only tools (PRD
//! AI-05, decision D-13).
//!
//! * **Off by default.** Nothing is sent until the user turns it on in [`CONFIG_FILE`] (or
//!   `SCULL_ASK_ENABLED=1`) and names a provider: the Anthropic Messages API (default model
//!   `claude-opus-5-5`) or any OpenAI-compatible endpoint, including local servers such as
//!   Ollama or LM Studio.
//! * **Bring your own key.** The key comes from the environment (or an `api_key` the user wrote
//!   into [`CONFIG_FILE`]); scull never writes, stores or logs it ([`ApiKey`]). It is only sent
//!   as the authentication header to the configured endpoint, which must be `https://` unless it
//!   is on this machine ([`Endpoint`]). Redirects are never followed.
//! * **Consent per provider.** Before the first request to a provider and endpoint, the user
//!   sees [`disclosure`] (what is sent, and to whom) and must agree; [`ConsentLog`] records the
//!   agreement (provider, endpoint origin, notice version; no key, no question). `scull ask
//!   --show-request` prints the exact first request without sending anything.
//! * **Read-only tools.** The model can call exactly the tools `scull mcp serve` offers, minus
//!   the metadata-only `register_worktree`, through [`scull_mcp::ScullMcp::call_in_process`]:
//!   the same allowlist (scan roots) and the same redaction (`mcp.redact_home`,
//!   `mcp.hash_names`, escaping, size caps). It cannot delete, move or change files, or approve
//!   or run a cleanup. This crate never depends on `scull-cleaner`.
//! * **Preparing plans, only if the user opted in** (AI-05). When `[mcp] destructive = true` is
//!   in the user config file and scull is not elevated (the same gates as MCP's destructive
//!   mode; checked by the CLI), the model also gets `prepare_cleanup` and the read-only plan
//!   views ([`scull_mcp::IN_PROCESS_PLAN_TOOL_NAMES`]), never `execute_cleanup`. A prepared plan
//!   is an agent plan like any other: it changes nothing, awaits the user's own
//!   `scull approve <plan_id>` (or the desktop dialog), and counts against
//!   `mcp.max_pending_plans` and `mcp.prepare_per_minute`. [`Answer::prepared_plans`] lists
//!   them so the CLI can tell the user exactly what to run. The notice says so
//!   ([`disclosure`]), and consent given for read-only use doesn't cover it.

mod config;
mod endpoint;
mod http;
mod provider;
mod session;

pub use config::{
    ApiKey, AskConfig, CONFIG_FILE, DEFAULT_ANTHROPIC_MODEL, DEFAULT_MAX_TOKENS, DEFAULT_MAX_TURNS,
    DEFAULT_TIMEOUT_SECS, KeySource, ProviderKind,
};
pub use endpoint::Endpoint;
pub use http::{HttpResponse, MAX_RESPONSE_BYTES, Transport, UreqTransport};
pub use provider::{
    ANTHROPIC_VERSION, FALLBACK_BETA, FALLBACK_MODELS, MAX_TOOL_RESULT_CHARS, ModelTurn, Stop,
    ToolCall, Usage, headers, parse_response, request_body, tool_result_text, uses_fallback,
};
pub use session::{
    Answer, Event, MAX_CALLS_PER_TURN, PreparedPlan, SYSTEM_PROMPT, SYSTEM_PROMPT_PREPARE, Session,
    ToolHost, system_prompt,
};

use serde::{Deserialize, Serialize};

/// Crate name, used in diagnostics.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

/// Bumped whenever [`disclosure`] changes what it promises: earlier consent no longer counts.
pub const DISCLOSURE_VERSION: u32 = 1;

/// File in scull's state directory recording consent.
pub const CONSENT_FILE: &str = "ask-consent.json";

/// The privacy notice shown before the first request to a provider and endpoint (PRD §13.5
/// applied to AI-05). `hash_names` is `mcp.hash_names`, `redact_home` is `mcp.redact_home`,
/// `can_prepare` whether the model may prepare cleanup plans (the user opted in).
pub fn disclosure(
    cfg: &AskConfig,
    redact_home: bool,
    hash_names: bool,
    can_prepare: bool,
) -> String {
    let model = if cfg.model.is_empty() {
        "model not set"
    } else {
        cfg.model.as_str()
    };
    let paths = if redact_home {
        "your home folder is shown as `~`"
    } else {
        "full paths (mcp.redact_home is off)"
    };
    let names = if hash_names {
        "hashed (mcp.hash_names is on)"
    } else {
        "shown (set mcp.hash_names = true to hash them)"
    };
    let tools = if can_prepare {
        "The model can call read-only tools and prepare cleanup plans ([mcp] destructive = true).
  Preparing a plan changes nothing: it only runs after you review and approve it yourself with
  `scull approve <plan-id>` in your own terminal (or in the scull desktop app). The model
  cannot approve or run a cleanup, or delete, move or change your files."
    } else {
        "The model can only call read-only tools: it cannot delete, move or change your files, write
  anything, or prepare or approve a cleanup."
    };
    let where_ = if cfg.endpoint.is_loopback() {
        "This endpoint is on this machine: nothing leaves your computer unless that server\n  \
         sends it on."
    } else {
        "That provider's own terms and data policy apply to what it receives."
    };
    format!(
        "scull ask: privacy notice
  Questions go to {provider} ({model}) at {origin}.
  Each question sends that endpoint:
  * your question and scull's instructions to the model;
  * the list of scull's read-only tools;
  * the results of the tools the model calls: paths under your scan roots, project and
    folder names, Git branch names, sizes, safety tiers and scull's storage history.
    Paths: {paths}. Names: {names}.
  File contents, commit messages and environment values are never sent.
  Your API key ({key}) goes only to this endpoint, as its authentication header;
  scull never stores or logs it.
  {tools}
  {where_}
  Preview the exact first request with `scull ask --show-request \"<question>\"`.",
        provider = cfg.provider.label(),
        origin = cfg.endpoint.origin(),
        key = cfg.key_source,
    )
}

/// One recorded agreement to [`disclosure`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Consent {
    /// `anthropic` or `openai`.
    pub provider: String,
    /// `scheme://host[:port]` of the endpoint.
    pub endpoint: String,
    /// [`DISCLOSURE_VERSION`] agreed to.
    pub disclosure_version: u32,
    /// When (Unix seconds).
    pub at_unix: u64,
    /// Whether the notice agreed to included plan preparation (AI-05). Older records lack it:
    /// read-only consent.
    #[serde(default)]
    pub prepare: bool,
}

/// The consent records in [`CONSENT_FILE`]. A missing, unreadable or invalid file means no
/// consent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsentLog {
    /// Records, one per provider and endpoint.
    pub consents: Vec<Consent>,
}

impl ConsentLog {
    /// Parse the file's text (anything invalid is an empty log, so consent is asked again).
    pub fn parse(text: &str) -> Self {
        serde_json::from_str(text).unwrap_or_default()
    }

    /// Whether the user agreed to the current notice for `cfg`'s provider and endpoint; with
    /// `can_prepare`, to the notice that includes plan preparation (read-only consent doesn't
    /// cover it).
    pub fn allows(&self, cfg: &AskConfig, can_prepare: bool) -> bool {
        let origin = cfg.endpoint.origin();
        self.consents.iter().any(|c| {
            c.provider == cfg.provider.as_str()
                && c.endpoint == origin
                && c.disclosure_version == DISCLOSURE_VERSION
                && (c.prepare || !can_prepare)
        })
    }

    /// Record agreement for `cfg`'s provider and endpoint (replacing an older record), to the
    /// notice with or without plan preparation.
    pub fn record(&mut self, cfg: &AskConfig, can_prepare: bool, at_unix: u64) {
        let origin = cfg.endpoint.origin();
        self.consents
            .retain(|c| !(c.provider == cfg.provider.as_str() && c.endpoint == origin));
        self.consents.push(Consent {
            provider: cfg.provider.as_str().to_owned(),
            endpoint: origin,
            disclosure_version: DISCLOSURE_VERSION,
            at_unix,
            prepare: can_prepare,
        });
    }

    /// The file's text.
    pub fn to_text(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_owned());
        s.push('\n');
        s
    }
}

/// The exact first request as text for `--show-request`: method, URL, headers (key masked) and
/// the JSON body.
pub fn render_request(cfg: &AskConfig, body: &serde_json::Value) -> String {
    let mut out = format!("POST {}\n", provider::url(cfg));
    for (k, v) in headers(cfg, true) {
        out.push_str(&format!("{k}: {v}\n"));
    }
    out.push('\n');
    out.push_str(&serde_json::to_string_pretty(body).unwrap_or_default());
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn cfg(file: &str) -> AskConfig {
        AskConfig::resolve(Some(file), |k| {
            (k == "ANTHROPIC_API_KEY").then(|| "sk-ant-secret".to_owned())
        })
        .unwrap()
    }

    #[test]
    fn consent_is_per_provider_endpoint_and_notice_version() {
        let a = cfg("enabled = true");
        let local = cfg("provider = \"openai\"\nbase_url = \"http://localhost:11434/v1\"");
        let mut log = ConsentLog::default();
        assert!(!log.allows(&a, false));
        log.record(&a, false, 1);
        assert!(log.allows(&a, false));
        assert!(!log.allows(&local, false));
        let text = log.to_text();
        assert!(!text.contains("sk-ant-secret"));
        let mut back = ConsentLog::parse(&text);
        assert!(back.allows(&a, false));
        back.consents[0].disclosure_version = DISCLOSURE_VERSION + 1;
        assert!(!back.allows(&a, false));
        assert_eq!(ConsentLog::parse("not json"), ConsentLog::default());
        log.record(&a, false, 2);
        assert_eq!(log.consents.len(), 1);
    }

    /// AI-05: read-only consent doesn't cover plan preparation; consent to the notice with
    /// preparation covers both. Records from before this field are read-only consent.
    #[test]
    fn read_only_consent_does_not_cover_plan_preparation() {
        let a = cfg("enabled = true");
        let mut log = ConsentLog::default();
        log.record(&a, false, 1);
        assert!(log.allows(&a, false));
        assert!(!log.allows(&a, true));
        log.record(&a, true, 2);
        assert!(log.allows(&a, true) && log.allows(&a, false));
        assert_eq!(log.consents.len(), 1);
        let old = r#"{"consents":[{"provider":"anthropic","endpoint":"https://api.anthropic.com","disclosure_version":1,"at_unix":5}]}"#;
        let old = ConsentLog::parse(old);
        assert!(old.allows(&a, false));
        assert!(!old.allows(&a, true));
    }

    #[test]
    fn disclosure_names_the_endpoint_and_the_key_source_never_the_key() {
        let a = cfg("enabled = true");
        let d = disclosure(&a, true, false, false);
        assert!(d.contains("Anthropic (claude-opus-5-5) at https://api.anthropic.com"));
        assert!(d.contains("from $ANTHROPIC_API_KEY"));
        assert!(d.contains("`~`"));
        assert!(d.contains("cannot delete"));
        assert!(d.contains("prepare or approve a cleanup"));
        assert!(!d.contains("sk-ant-secret"));
        let local = cfg("provider = \"openai\"\nbase_url = \"http://127.0.0.1:1234/v1\"");
        assert!(disclosure(&local, true, true, false).contains("on this machine"));
        assert!(disclosure(&local, true, true, false).contains("hashed"));
        // AI-05: with plan preparation, the notice says what it means.
        let p = disclosure(&a, true, false, true);
        assert!(p.contains("prepare cleanup plans"), "{p}");
        assert!(p.contains("scull approve <plan-id>"), "{p}");
        assert!(p.contains("cannot approve or run a cleanup"), "{p}");
    }

    #[test]
    fn the_preview_masks_the_key() {
        let a = cfg("enabled = true");
        let r = render_request(&a, &serde_json::json!({"model": "m"}));
        assert!(r.starts_with("POST https://api.anthropic.com/v1/messages\n"));
        assert!(r.contains("x-api-key: <redacted>"));
        assert!(r.contains("anthropic-version: 2026-10-21"));
        assert!(r.contains("anthropic-beta: server-side-fallback-2026-07-01"));
        assert!(!r.contains("sk-ant-secret"));
    }
}
