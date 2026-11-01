//! The two wire formats: the Anthropic Messages API and OpenAI-compatible Chat Completions.
//!
//! Requests are built from a stable prefix (tools, then the system prompt) so the Anthropic
//! prompt cache can reuse it across the turns of one question; assistant turns are appended
//! back exactly as received (thinking blocks included), and tool results follow in one message.

use crate::config::{AskConfig, ProviderKind};
use scull_core::{ErrorKind, ScullError};
use scull_mcp::{LocalResult, ToolSpec, escape};
use serde_json::{Map, Value, json};

/// The Anthropic API version sent with every request.
pub const ANTHROPIC_VERSION: &str = "2026-10-21";

/// Beta header for `fallbacks: "default"` (server-side retry of a declined request).
pub const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

/// Models that accept `fallbacks: "default"` on the Claude API.
pub const FALLBACK_MODELS: [&str; 4] = [
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-fable-5-1",
    "claude-sonnet-5-5",
];

/// Most characters of one tool result sent to the model.
pub const MAX_TOOL_RESULT_CHARS: usize = 60_000;

/// Most characters of a provider error message shown to the user.
const MAX_ERROR_CHARS: usize = 300;

/// A tool the model asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    /// The provider's call id (echoed in the result).
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Arguments; `Err` with the reason if the model sent something that is not a JSON object.
    pub input: Result<Map<String, Value>, String>,
}

/// Why the model stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// A final answer.
    Done,
    /// It wants tool results.
    ToolUse,
    /// The output limit was reached.
    MaxTokens,
    /// The provider declined the request (with its category, if given).
    Refusal(Option<String>),
    /// Anything else (the provider's own reason).
    Other(String),
}

/// Token counts reported by the provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Input tokens (uncached).
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Input tokens read from the prompt cache.
    pub cache_read_tokens: u64,
}

impl Usage {
    /// Add `other` to these counts.
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
    }
}

/// One parsed model response.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelTurn {
    /// Text blocks, in order.
    pub texts: Vec<String>,
    /// Tool calls, in order.
    pub calls: Vec<ToolCall>,
    /// Why it stopped.
    pub stop: Stop,
    /// The assistant message to append to the conversation, exactly as received.
    pub assistant: Value,
    /// Token counts.
    pub usage: Usage,
}

/// Whether requests carry `fallbacks: "default"` (Anthropic's own API, a supported model, and
/// `fallback = true`).
pub fn uses_fallback(cfg: &AskConfig) -> bool {
    cfg.provider == ProviderKind::Anthropic
        && cfg.fallback
        && cfg.endpoint.is_anthropic_api()
        && FALLBACK_MODELS.contains(&cfg.model.as_str())
}

/// The request URL.
pub fn url(cfg: &AskConfig) -> String {
    match cfg.provider {
        ProviderKind::Anthropic => cfg.endpoint.join("/v1/messages"),
        ProviderKind::OpenAi => cfg.endpoint.join("/chat/completions"),
    }
}

/// The request headers. With `mask`, the key is replaced by `<redacted>` (for showing the
/// request to the user).
pub fn headers(cfg: &AskConfig, mask: bool) -> Vec<(&'static str, String)> {
    let key = cfg.key.as_ref().map(|k| {
        if mask {
            "<redacted>".to_owned()
        } else {
            k.expose().to_owned()
        }
    });
    let mut h = vec![("content-type", "application/json".to_owned())];
    match cfg.provider {
        ProviderKind::Anthropic => {
            if let Some(k) = key {
                h.push(("x-api-key", k));
            }
            h.push(("anthropic-version", ANTHROPIC_VERSION.to_owned()));
            if uses_fallback(cfg) {
                h.push(("anthropic-beta", FALLBACK_BETA.to_owned()));
            }
        }
        ProviderKind::OpenAi => {
            if let Some(k) = key {
                h.push(("authorization", format!("Bearer {k}")));
            }
        }
    }
    h
}

/// The first user message.
pub fn user_message(question: &str) -> Value {
    json!({ "role": "user", "content": question })
}

/// The request body for `messages` so far.
pub fn request_body(
    cfg: &AskConfig,
    system: &str,
    tools: &[ToolSpec],
    messages: &[Value],
) -> Value {
    match cfg.provider {
        ProviderKind::Anthropic => {
            let tools: Vec<Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
                    })
                })
                .collect();
            let mut body = json!({
                "model": cfg.model,
                "max_tokens": cfg.max_tokens,
                "tools": tools,
                "system": system,
                "messages": messages,
                // Automatic prompt caching: the tools and system prompt are identical on every
                // turn of a question.
                "cache_control": { "type": "ephemeral" },
            });
            if let Some(e) = &cfg.effort {
                body["output_config"] = json!({ "effort": e });
            }
            if uses_fallback(cfg) {
                body["fallbacks"] = json!("default");
            }
            body
        }
        ProviderKind::OpenAi => {
            let tools: Vec<Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect();
            let mut all = Vec::with_capacity(messages.len() + 1);
            all.push(json!({ "role": "system", "content": system }));
            all.extend(messages.iter().cloned());
            json!({ "model": cfg.model, "messages": all, "tools": tools })
        }
    }
}

fn u64_at(v: &Value, ptr: &str) -> u64 {
    v.pointer(ptr).and_then(Value::as_u64).unwrap_or(0)
}

fn bad_response(what: &str) -> ScullError {
    ScullError::new(
        ErrorKind::Io,
        format!("the model endpoint sent an unexpected response ({what})"),
    )
}

/// Parse a successful (HTTP 200) response body.
pub fn parse_response(kind: ProviderKind, body: &Value) -> Result<ModelTurn, ScullError> {
    match kind {
        ProviderKind::Anthropic => {
            let content = body
                .get("content")
                .and_then(Value::as_array)
                .ok_or_else(|| bad_response("no content"))?;
            let mut texts = Vec::new();
            let mut calls = Vec::new();
            for block in content {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(Value::as_str) {
                            texts.push(t.to_owned());
                        }
                    }
                    Some("tool_use") => calls.push(ToolCall {
                        id: str_at(block, "id"),
                        name: str_at(block, "name"),
                        input: match block.get("input") {
                            Some(Value::Object(m)) => Ok(m.clone()),
                            _ => Err("arguments are not a JSON object".to_owned()),
                        },
                    }),
                    // Thinking, fallback and other blocks are kept in the history, not shown.
                    _ => {}
                }
            }
            let stop = match body.get("stop_reason").and_then(Value::as_str) {
                Some("end_turn" | "stop_sequence") => Stop::Done,
                Some("tool_use") => Stop::ToolUse,
                Some("max_tokens") => Stop::MaxTokens,
                Some("refusal") => Stop::Refusal(
                    body.pointer("/stop_details/category")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                ),
                Some(other) => Stop::Other(other.to_owned()),
                None => Stop::Other("unknown".to_owned()),
            };
            Ok(ModelTurn {
                texts,
                calls,
                stop,
                assistant: json!({ "role": "assistant", "content": content }),
                usage: Usage {
                    input_tokens: u64_at(body, "/usage/input_tokens"),
                    output_tokens: u64_at(body, "/usage/output_tokens"),
                    cache_read_tokens: u64_at(body, "/usage/cache_read_input_tokens"),
                },
            })
        }
        ProviderKind::OpenAi => {
            let choice = body
                .pointer("/choices/0")
                .ok_or_else(|| bad_response("no choices"))?;
            let message = choice
                .get("message")
                .filter(|m| m.is_object())
                .ok_or_else(|| bad_response("no message"))?;
            let texts = match message.get("content") {
                Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
                _ => Vec::new(),
            };
            let calls: Vec<ToolCall> = message
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .map(|c| ToolCall {
                            id: str_at(c, "id"),
                            name: c
                                .pointer("/function/name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            input: parse_arguments(c.pointer("/function/arguments")),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let stop = match choice.get("finish_reason").and_then(Value::as_str) {
                _ if !calls.is_empty() => Stop::ToolUse,
                Some("stop") | None => Stop::Done,
                Some("length") => Stop::MaxTokens,
                Some("content_filter") => Stop::Refusal(None),
                Some(other) => Stop::Other(other.to_owned()),
            };
            Ok(ModelTurn {
                texts,
                calls,
                stop,
                assistant: message.clone(),
                usage: Usage {
                    input_tokens: u64_at(body, "/usage/prompt_tokens"),
                    output_tokens: u64_at(body, "/usage/completion_tokens"),
                    cache_read_tokens: u64_at(body, "/usage/prompt_tokens_details/cached_tokens"),
                },
            })
        }
    }
}

fn str_at(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// OpenAI sends arguments as a JSON string (some local servers send an object).
fn parse_arguments(v: Option<&Value>) -> Result<Map<String, Value>, String> {
    match v {
        Some(Value::Object(m)) => Ok(m.clone()),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(Map::new()),
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Object(m)) => Ok(m),
            _ => Err("arguments are not a JSON object".to_owned()),
        },
        None | Some(Value::Null) => Ok(Map::new()),
        Some(_) => Err("arguments are not a JSON object".to_owned()),
    }
}

/// The text the model receives for one tool result: the summary, then the structured result
/// (both already escaped and redacted by scull-mcp), capped at [`MAX_TOOL_RESULT_CHARS`].
pub fn tool_result_text(r: &LocalResult) -> String {
    let structured = serde_json::to_string(&r.structured).unwrap_or_default();
    let full = format!("{}\n{structured}", r.text);
    if full.chars().count() <= MAX_TOOL_RESULT_CHARS {
        return full;
    }
    let cut: String = full.chars().take(MAX_TOOL_RESULT_CHARS).collect();
    format!("{cut}\n[truncated: result too long; ask for fewer items]")
}

/// The message(s) carrying the results of `calls`, in order (Anthropic: one user message with
/// every `tool_result`; OpenAI: one `tool` message per call).
pub fn tool_results(kind: ProviderKind, results: &[(ToolCall, LocalResult)]) -> Vec<Value> {
    match kind {
        ProviderKind::Anthropic => {
            let blocks: Vec<Value> = results
                .iter()
                .map(|(call, r)| {
                    let mut b = json!({
                        "type": "tool_result",
                        "tool_use_id": call.id,
                        "content": tool_result_text(r),
                    });
                    if r.is_error {
                        b["is_error"] = json!(true);
                    }
                    b
                })
                .collect();
            vec![json!({ "role": "user", "content": blocks })]
        }
        ProviderKind::OpenAi => results
            .iter()
            .map(|(call, r)| {
                json!({
                    "role": "tool",
                    "tool_call_id": call.id,
                    "content": tool_result_text(r),
                })
            })
            .collect(),
    }
}

/// The error for a non-200 response: the provider's own message (escaped and capped), never
/// the request.
pub fn http_error(status: u16, body: &[u8]) -> ScullError {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let msg = v
        .pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| v.get("message").and_then(Value::as_str))
        .or_else(|| v.get("error").and_then(Value::as_str))
        .map(|m| {
            let m: String = m.chars().take(MAX_ERROR_CHARS).collect();
            escape(&m).into_owned()
        });
    let hint = match status {
        401 | 403 => " (check the API key)",
        404 => " (check base_url and model)",
        429 => " (rate limited; try again later)",
        500..=599 => " (the provider had a problem; try again later)",
        _ => "",
    };
    let kind = if matches!(status, 400..=404) {
        ErrorKind::Config
    } else {
        ErrorKind::Io
    };
    let text = match msg {
        Some(m) => format!("the model endpoint answered HTTP {status}{hint}: {m}"),
        None => format!("the model endpoint answered HTTP {status}{hint}"),
    };
    ScullError::new(kind, text)
}
