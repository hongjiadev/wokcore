use std::{collections::BTreeMap, fmt};

use serde_json::Value;
use url::Url;

use crate::canonical::GatewayError;

mod anthropic;
mod azure;
mod cursor;
mod gemini;
mod openai_chat;
mod openai_responses;

pub use anthropic::{
    AnthropicCodec, AnthropicEncodeContext, AnthropicResponseTemplate, AnthropicStopReason,
    AnthropicTokenCount, TokenCounter,
};
pub use azure::{AzureAdapter, AzureConfig, AzureStreamDecoder};
pub use cursor::{CursorAdapter, CursorConfig};
pub use gemini::{GeminiAdapter, GeminiConfig, GeminiStreamDecoder};
pub use openai_chat::{ChatCodec, ChatEncodeContext, ChatFinishReason, ChatResponseTemplate};
pub use openai_responses::{ResponsesCodec, ResponsesEncodeContext, ResponsesResponseTemplate};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UpstreamLimits {
    pub max_request_body_bytes: usize,
    pub max_response_body_bytes: usize,
    pub max_stream_frame_bytes: usize,
    pub max_events: usize,
    pub max_collection_items: usize,
    pub max_identifier_bytes: usize,
    pub max_text_delta_bytes: usize,
    pub max_tool_argument_bytes: usize,
}

impl Default for UpstreamLimits {
    fn default() -> Self {
        Self {
            max_request_body_bytes: 16 * 1024 * 1024,
            max_response_body_bytes: 16 * 1024 * 1024,
            max_stream_frame_bytes: 1024 * 1024,
            max_events: 4096,
            max_collection_items: 256,
            max_identifier_bytes: 512,
            max_text_delta_bytes: 1024 * 1024,
            max_tool_argument_bytes: 1024 * 1024,
        }
    }
}

#[derive(Clone)]
pub struct UpstreamRequest {
    pub url: Url,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub stream: bool,
}

impl fmt::Debug for UpstreamRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UpstreamRequest")
            .field("url", &"[redacted]")
            .field("headers", &"[redacted]")
            .field("body_bytes", &self.body.len())
            .field("stream", &self.stream)
            .finish()
    }
}

fn validate_base_url(url: &Url) -> Result<(), GatewayError> {
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.path().ends_with('/')
    {
        return Err(GatewayError::invalid_request());
    }
    Ok(())
}

fn checked_join(base: &Url, relative: &str) -> Result<Url, GatewayError> {
    validate_base_url(base)?;
    let joined = base
        .join(relative)
        .map_err(|_| GatewayError::invalid_request())?;
    if joined.scheme() != base.scheme()
        || joined.host_str() != base.host_str()
        || joined.port_or_known_default() != base.port_or_known_default()
        || !joined.path().starts_with(base.path())
    {
        return Err(GatewayError::invalid_request());
    }
    Ok(joined)
}

fn validate_identifier(value: &str, limit: usize) -> Result<(), GatewayError> {
    if value.is_empty()
        || matches!(value, "." | "..")
        || value.len() > limit
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(GatewayError::invalid_request());
    }
    Ok(())
}

fn validate_value_size(value: &Value, limit: usize) -> Result<(), GatewayError> {
    let bytes = serde_json::to_vec(value).map_err(|_| GatewayError::invalid_request())?;
    if bytes.len() > limit {
        return Err(GatewayError::invalid_request());
    }
    Ok(())
}

fn encode_bounded(value: &Value, limit: usize) -> Result<Vec<u8>, GatewayError> {
    let body = serde_json::to_vec(value).map_err(|_| GatewayError::invalid_request())?;
    if body.len() > limit {
        return Err(GatewayError::invalid_request());
    }
    Ok(body)
}

fn push_event(
    events: &mut Vec<crate::canonical::CanonicalEvent>,
    event: crate::canonical::CanonicalEvent,
    limits: UpstreamLimits,
) -> Result<(), GatewayError> {
    if events.len() >= limits.max_events {
        return Err(GatewayError::invalid_request());
    }
    events.push(event);
    Ok(())
}

fn account_stream_events(
    emitted_events: &mut usize,
    added_events: usize,
    limits: UpstreamLimits,
) -> Result<(), GatewayError> {
    let total = emitted_events
        .checked_add(added_events)
        .ok_or_else(GatewayError::invalid_request)?;
    if total > limits.max_events {
        return Err(GatewayError::invalid_request());
    }
    *emitted_events = total;
    Ok(())
}

fn classify_http_error(
    status: u16,
    retry_after: Option<&str>,
    provider: &'static str,
) -> GatewayError {
    match status {
        401 | 403 => GatewayError::upstream_auth(provider),
        429 => GatewayError::rate_limited(
            retry_after.and_then(|value| value.trim().parse::<u64>().ok()),
        ),
        500..=599 => GatewayError::upstream_5xx(status),
        _ => GatewayError::upstream_response(status, provider),
    }
}
