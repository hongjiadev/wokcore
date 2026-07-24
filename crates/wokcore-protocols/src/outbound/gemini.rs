use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};
use url::Url;

use crate::{
    canonical::{CanonicalEvent, CanonicalRequest, GatewayError, InputItem, RequestId, Usage},
    stream::SseDecoder,
};

use super::{
    UpstreamLimits, UpstreamRequest, account_stream_events, checked_join, classify_http_error,
    encode_bounded, push_event, validate_base_url, validate_identifier, validate_value_size,
};

const TOOL_NAMES_EXTENSION: &str = "gemini.tool_names";

#[derive(Clone)]
pub struct GeminiConfig {
    base_url: Url,
    api_key: String,
}

impl std::fmt::Debug for GeminiConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GeminiConfig")
            .field("base_url", &"[redacted]")
            .field("api_key", &"[redacted]")
            .finish()
    }
}

impl GeminiConfig {
    pub fn new(base_url: Url, api_key: impl Into<String>) -> Result<Self, GatewayError> {
        validate_base_url(&base_url)?;
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Err(GatewayError::invalid_request());
        }
        Ok(Self { base_url, api_key })
    }
}

pub struct GeminiAdapter {
    config: GeminiConfig,
    limits: UpstreamLimits,
}

impl GeminiAdapter {
    pub fn new(config: GeminiConfig, limits: UpstreamLimits) -> Self {
        Self { config, limits }
    }

    pub fn build_request(
        &self,
        request: &CanonicalRequest,
    ) -> Result<UpstreamRequest, GatewayError> {
        validate_identifier(request.model.as_str(), self.limits.max_identifier_bytes)?;
        if request.input.len() > self.limits.max_collection_items
            || request.tools.len() > self.limits.max_collection_items
        {
            return Err(GatewayError::invalid_request());
        }

        let method = if request.stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        let directory = checked_join(&self.config.base_url, "v1beta/models/")?;
        let mut url = directory.clone();
        url.path_segments_mut()
            .map_err(|_| GatewayError::invalid_request())?
            .pop_if_empty()
            .push(&format!("{}:{method}", request.model.as_str()));
        if url.scheme() != self.config.base_url.scheme()
            || url.host_str() != self.config.base_url.host_str()
            || !url.path().starts_with(directory.path())
        {
            return Err(GatewayError::invalid_request());
        }
        if request.stream {
            url.query_pairs_mut().append_pair("alt", "sse");
        }

        let tool_names = request
            .extensions
            .get(TOOL_NAMES_EXTENSION)
            .and_then(Value::as_object);
        let mut parts = Vec::with_capacity(request.input.len());
        for item in &request.input {
            let part = match item {
                InputItem::Text { text } => {
                    if text.len() > self.limits.max_text_delta_bytes {
                        return Err(GatewayError::invalid_request());
                    }
                    json!({"text": text})
                }
                InputItem::ImageUrl { url, .. } => {
                    if !matches!(url.scheme(), "http" | "https" | "data") {
                        return Err(GatewayError::unsupported_capability());
                    }
                    json!({"fileData": {"fileUri": url.as_str()}})
                }
                InputItem::ToolResult { call_id, output } => {
                    validate_identifier(call_id, self.limits.max_identifier_bytes)?;
                    validate_value_size(output, self.limits.max_tool_argument_bytes)?;
                    let name = tool_names
                        .and_then(|names| names.get(call_id))
                        .and_then(Value::as_str)
                        .ok_or_else(GatewayError::unsupported_capability)?;
                    validate_identifier(name, self.limits.max_identifier_bytes)?;
                    json!({
                        "functionResponse": {
                            "id": call_id,
                            "name": name,
                            "response": {"result": output}
                        }
                    })
                }
            };
            parts.push(part);
        }

        let mut body = Map::new();
        body.insert(
            "contents".to_owned(),
            json!([{"role": "user", "parts": parts}]),
        );
        if !request.tools.is_empty() {
            let mut declarations = Vec::with_capacity(request.tools.len());
            for tool in &request.tools {
                validate_identifier(&tool.name, self.limits.max_identifier_bytes)?;
                validate_value_size(&tool.input_schema, self.limits.max_request_body_bytes)?;
                declarations.push(json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema
                }));
            }
            body.insert(
                "tools".to_owned(),
                json!([{"functionDeclarations": declarations}]),
            );
        }
        if request.reasoning.is_some() {
            let thinking = request
                .extensions
                .get("gemini.thinking_config")
                .ok_or_else(GatewayError::unsupported_capability)?;
            validate_value_size(thinking, self.limits.max_request_body_bytes)?;
            body.insert(
                "generationConfig".to_owned(),
                json!({"thinkingConfig": thinking}),
            );
        }

        let body = encode_bounded(&Value::Object(body), self.limits.max_request_body_bytes)?;
        Ok(UpstreamRequest {
            url,
            headers: BTreeMap::from([
                ("content-type".to_owned(), "application/json".to_owned()),
                ("x-goog-api-key".to_owned(), self.config.api_key.clone()),
            ]),
            body,
            stream: request.stream,
        })
    }

    pub fn decode_response(
        &self,
        request_id: RequestId,
        body: &[u8],
    ) -> Result<Vec<CanonicalEvent>, GatewayError> {
        if body.len() > self.limits.max_response_body_bytes {
            return Err(GatewayError::invalid_request());
        }
        let value: Value =
            serde_json::from_slice(body).map_err(|_| GatewayError::invalid_request())?;
        let mut decoder = GeminiResponseDecoder::new(request_id, self.limits);
        let mut emitted_events = 0;
        let mut events = decoder.decode_value(&value)?;
        account_stream_events(&mut emitted_events, events.len(), self.limits)?;
        let completed = decoder.finish()?;
        account_stream_events(&mut emitted_events, completed.len(), self.limits)?;
        events.extend(completed);
        Ok(events)
    }

    pub fn decode_http_error(&self, status: u16, retry_after: Option<&str>) -> GatewayError {
        classify_http_error(status, retry_after, "gemini upstream")
    }

    pub fn stream_decoder(&self, request_id: RequestId) -> GeminiStreamDecoder {
        GeminiStreamDecoder {
            sse: SseDecoder::with_limits(
                self.limits.max_stream_frame_bytes,
                self.limits.max_events,
            ),
            response: GeminiResponseDecoder::new(request_id, self.limits),
            received_bytes: 0,
            limits: self.limits,
            failed: false,
            finished: false,
            emitted_events: 0,
        }
    }
}

pub struct GeminiStreamDecoder {
    sse: SseDecoder,
    response: GeminiResponseDecoder,
    received_bytes: usize,
    limits: UpstreamLimits,
    failed: bool,
    finished: bool,
    emitted_events: usize,
}

impl GeminiStreamDecoder {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<CanonicalEvent>, GatewayError> {
        if self.failed || self.finished || self.response.saw_terminal {
            return Err(GatewayError::invalid_request());
        }
        self.received_bytes = self.received_bytes.saturating_add(chunk.len());
        if self.received_bytes > self.limits.max_response_body_bytes {
            self.failed = true;
            return Err(GatewayError::invalid_request());
        }
        let frames = self.sse.push(chunk).map_err(|_| {
            self.failed = true;
            GatewayError::invalid_request()
        })?;
        let mut events = Vec::new();
        for frame in frames {
            let value: Value = serde_json::from_str(&frame.data).map_err(|_| {
                self.failed = true;
                GatewayError::invalid_request()
            })?;
            let decoded = self.response.decode_value(&value).inspect_err(|_| {
                self.failed = true;
            })?;
            account_stream_events(&mut self.emitted_events, decoded.len(), self.limits)
                .inspect_err(|_| {
                    self.failed = true;
                })?;
            events.extend(decoded);
        }
        Ok(events)
    }

    pub fn finish(&mut self) -> Result<Vec<CanonicalEvent>, GatewayError> {
        if self.failed || self.finished {
            return Err(GatewayError::invalid_request());
        }
        self.sse.finish().map_err(|_| {
            self.failed = true;
            GatewayError::invalid_request()
        })?;
        if !self.response.saw_terminal {
            self.failed = true;
            return Err(GatewayError::invalid_request());
        }
        let events = self.response.finish().inspect_err(|_| {
            self.failed = true;
        })?;
        account_stream_events(&mut self.emitted_events, events.len(), self.limits).inspect_err(
            |_| {
                self.failed = true;
            },
        )?;
        self.finished = true;
        Ok(events)
    }
}

struct GeminiResponseDecoder {
    request_id: RequestId,
    limits: UpstreamLimits,
    created: bool,
    completed: bool,
    next_item: usize,
    pending_usage: Option<Usage>,
    saw_terminal: bool,
}

impl GeminiResponseDecoder {
    fn new(request_id: RequestId, limits: UpstreamLimits) -> Self {
        Self {
            request_id,
            limits,
            created: false,
            completed: false,
            next_item: 0,
            pending_usage: None,
            saw_terminal: false,
        }
    }

    fn decode_value(&mut self, value: &Value) -> Result<Vec<CanonicalEvent>, GatewayError> {
        if self.saw_terminal {
            return Err(GatewayError::invalid_request());
        }
        let root = value
            .as_object()
            .ok_or_else(GatewayError::invalid_request)?;
        if root.contains_key("error") {
            return Err(GatewayError::upstream_response(
                502,
                "gemini upstream error",
            ));
        }
        let mut events = Vec::new();
        if !self.created {
            let response_id = root
                .get("responseId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("gemini_{}", self.request_id.as_str()));
            validate_identifier(&response_id, self.limits.max_identifier_bytes)?;
            push_event(
                &mut events,
                CanonicalEvent::Created { response_id },
                self.limits,
            )?;
            self.created = true;
        }

        if let Some(usage) = root.get("usageMetadata") {
            self.pending_usage = Some(gemini_usage(usage, self.limits)?);
        }
        if let Some(candidates) = root.get("candidates") {
            let candidates = candidates
                .as_array()
                .ok_or_else(GatewayError::invalid_request)?;
            if candidates.len() > self.limits.max_collection_items {
                return Err(GatewayError::invalid_request());
            }
            let mut saw_terminal = false;
            for candidate in candidates {
                if let Some(finish_reason) = candidate.get("finishReason") {
                    let finish_reason = finish_reason
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .ok_or_else(GatewayError::invalid_request)?;
                    let _ = finish_reason;
                    saw_terminal = true;
                }
                let Some(parts) = candidate
                    .get("content")
                    .and_then(|content| content.get("parts"))
                else {
                    continue;
                };
                let parts = parts.as_array().ok_or_else(GatewayError::invalid_request)?;
                if parts.len() > self.limits.max_collection_items {
                    return Err(GatewayError::invalid_request());
                }
                for part in parts {
                    self.decode_part(part, &mut events)?;
                }
            }
            self.saw_terminal = saw_terminal;
        }
        Ok(events)
    }

    fn decode_part(
        &mut self,
        part: &Value,
        events: &mut Vec<CanonicalEvent>,
    ) -> Result<(), GatewayError> {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if text.len() > self.limits.max_text_delta_bytes {
                return Err(GatewayError::invalid_request());
            }
            let item_id = format!("gemini_item_{}", self.next_item);
            self.next_item += 1;
            let event = if part.get("thought").and_then(Value::as_bool) == Some(true) {
                CanonicalEvent::ReasoningDelta {
                    item_id,
                    delta: text.to_owned(),
                }
            } else {
                CanonicalEvent::OutputTextDelta {
                    item_id,
                    delta: text.to_owned(),
                }
            };
            push_event(events, event, self.limits)?;
        }
        if let Some(call) = part.get("functionCall") {
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(GatewayError::invalid_request)?;
            validate_identifier(name, self.limits.max_identifier_bytes)?;
            let call_id = call
                .get("id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("call_gemini_{}", self.next_item));
            validate_identifier(&call_id, self.limits.max_identifier_bytes)?;
            let delta =
                serde_json::to_string(call.get("args").unwrap_or(&Value::Object(Map::new())))
                    .map_err(|_| GatewayError::invalid_request())?;
            if delta.len() > self.limits.max_tool_argument_bytes {
                return Err(GatewayError::invalid_request());
            }
            let item_id = format!("gemini_tool_{}", self.next_item);
            self.next_item += 1;
            push_event(
                events,
                CanonicalEvent::ToolCallDelta {
                    item_id,
                    call_id,
                    name: name.to_owned(),
                    delta,
                },
                self.limits,
            )?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<Vec<CanonicalEvent>, GatewayError> {
        if self.completed {
            return Ok(Vec::new());
        }
        let mut events = Vec::new();
        if !self.created {
            push_event(
                &mut events,
                CanonicalEvent::Created {
                    response_id: format!("gemini_{}", self.request_id.as_str()),
                },
                self.limits,
            )?;
            self.created = true;
        }
        if let Some(usage) = self.pending_usage.take() {
            push_event(&mut events, CanonicalEvent::Usage(usage), self.limits)?;
        }
        push_event(&mut events, CanonicalEvent::Completed, self.limits)?;
        self.completed = true;
        Ok(events)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiUsageWire {
    #[serde(default, deserialize_with = "deserialize_present")]
    prompt_token_count: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_present")]
    candidates_token_count: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_present")]
    cached_content_token_count: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_present")]
    thoughts_token_count: Option<u64>,
    #[serde(flatten)]
    extensions: BTreeMap<String, Value>,
}

fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn gemini_usage(value: &Value, limits: UpstreamLimits) -> Result<Usage, GatewayError> {
    let wire: GeminiUsageWire =
        serde_json::from_value(value.clone()).map_err(|_| GatewayError::invalid_request())?;
    if wire.extensions.len() > limits.max_collection_items {
        return Err(GatewayError::invalid_request());
    }
    Ok(Usage {
        input_tokens: wire.prompt_token_count.unwrap_or(0),
        output_tokens: wire.candidates_token_count.unwrap_or(0),
        cached_input_tokens: wire.cached_content_token_count,
        reasoning_tokens: wire.thoughts_token_count,
        extensions: wire.extensions,
    })
}
