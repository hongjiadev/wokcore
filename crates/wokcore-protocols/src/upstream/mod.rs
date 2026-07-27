use std::collections::{BTreeMap, HashMap};

use serde_json::{Map, Value, json};
use url::Url;

use crate::{
    AzureAdapter, AzureConfig, AzureStreamDecoder, GeminiAdapter, GeminiConfig,
    GeminiStreamDecoder, UpstreamLimits, UpstreamRequest,
    canonical::{
        CanonicalEvent, CanonicalRequest, GatewayError, ImageDetail, InputItem, RequestId, Usage,
    },
    stream::SseDecoder,
};

const DEFAULT_AZURE_API_VERSION: &str = "2024-10-21";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";
const DEFAULT_ANTHROPIC_MAX_TOKENS: u64 = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpstreamProtocol {
    OpenAiResponses,
    OpenAiChat,
    Anthropic,
    Gemini,
    AzureOpenAi,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpstreamOperation {
    Text,
    CountTokens,
}

#[derive(Clone)]
pub struct UpstreamAdapter {
    protocol: UpstreamProtocol,
    base_url: Url,
    limits: UpstreamLimits,
}

impl UpstreamAdapter {
    pub fn new(
        protocol: UpstreamProtocol,
        base_url: Url,
        limits: UpstreamLimits,
    ) -> Result<Self, GatewayError> {
        let base_url = normalize_base_url(base_url)?;
        validate_limits(limits)?;
        Ok(Self {
            protocol,
            base_url,
            limits,
        })
    }

    pub const fn protocol(&self) -> UpstreamProtocol {
        self.protocol
    }

    pub fn build_request(
        &self,
        request: &CanonicalRequest,
        operation: UpstreamOperation,
    ) -> Result<UpstreamRequest, GatewayError> {
        validate_request(request, self.limits)?;
        match (self.protocol, operation) {
            (UpstreamProtocol::OpenAiResponses, UpstreamOperation::Text) => {
                self.build_openai_responses(request)
            }
            (UpstreamProtocol::OpenAiChat, UpstreamOperation::Text) => {
                self.build_openai_chat(request)
            }
            (UpstreamProtocol::Anthropic, operation) => self.build_anthropic(request, operation),
            (UpstreamProtocol::Gemini, UpstreamOperation::Text) => self.build_gemini(request),
            (UpstreamProtocol::AzureOpenAi, UpstreamOperation::Text) => self.build_azure(request),
            _ => Err(GatewayError::unsupported_capability()),
        }
    }

    pub fn decode_response(
        &self,
        request_id: RequestId,
        body: &[u8],
    ) -> Result<Vec<CanonicalEvent>, GatewayError> {
        if body.len() > self.limits.max_response_body_bytes {
            return Err(GatewayError::invalid_request());
        }
        match self.protocol {
            UpstreamProtocol::OpenAiResponses => {
                decode_responses_complete(request_id, body, self.limits)
            }
            UpstreamProtocol::OpenAiChat | UpstreamProtocol::AzureOpenAi => self
                .azure_decoder_adapter()?
                .decode_response(request_id, body),
            UpstreamProtocol::Anthropic => decode_anthropic_complete(request_id, body, self.limits),
            UpstreamProtocol::Gemini => self
                .gemini_decoder_adapter()?
                .decode_response(request_id, body),
        }
    }

    pub fn decode_token_count(&self, body: &[u8]) -> Result<u64, GatewayError> {
        if self.protocol != UpstreamProtocol::Anthropic
            || body.len() > self.limits.max_response_body_bytes
        {
            return Err(GatewayError::unsupported_capability());
        }
        let value: Value =
            serde_json::from_slice(body).map_err(|_| GatewayError::invalid_request())?;
        value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .ok_or_else(GatewayError::invalid_request)
    }

    pub fn stream_decoder(&self, request_id: RequestId) -> UpstreamStreamDecoder {
        let inner = match self.protocol {
            UpstreamProtocol::OpenAiResponses => {
                StreamDecoderInner::Responses(ResponsesStreamDecoder::new(request_id, self.limits))
            }
            UpstreamProtocol::OpenAiChat => StreamDecoderInner::OpenAiChat(
                self.azure_decoder_adapter()
                    .expect("validated adapter URL")
                    .stream_decoder(request_id),
            ),
            UpstreamProtocol::Anthropic => {
                StreamDecoderInner::Anthropic(AnthropicStreamDecoder::new(request_id, self.limits))
            }
            UpstreamProtocol::Gemini => StreamDecoderInner::Gemini(
                self.gemini_decoder_adapter()
                    .expect("validated adapter URL")
                    .stream_decoder(request_id),
            ),
            UpstreamProtocol::AzureOpenAi => StreamDecoderInner::Azure(
                self.azure_decoder_adapter()
                    .expect("validated adapter URL")
                    .stream_decoder(request_id),
            ),
        };
        UpstreamStreamDecoder { inner }
    }

    pub fn decode_http_error(&self, status: u16, retry_after: Option<&str>) -> GatewayError {
        match status {
            401 | 403 => GatewayError::upstream_auth("upstream authentication rejected"),
            429 => GatewayError::rate_limited(
                retry_after.and_then(|value| value.trim().parse::<u64>().ok()),
            ),
            500..=599 => GatewayError::upstream_5xx(status),
            _ => GatewayError::upstream_response(status, "upstream response rejected"),
        }
    }

    fn build_openai_responses(
        &self,
        request: &CanonicalRequest,
    ) -> Result<UpstreamRequest, GatewayError> {
        let input = encode_responses_input(request, self.limits)?;
        let tools = encode_responses_tools(request, self.limits)?;
        let mut body = json!({
            "model": request.model.as_str(),
            "input": input,
            "stream": request.stream,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        if let Some(reasoning) = encode_reasoning(request)? {
            body["reasoning"] = reasoning;
        }
        self.json_request(
            join_within(&self.base_url, "responses")?,
            body,
            request.stream,
        )
    }

    fn build_openai_chat(
        &self,
        request: &CanonicalRequest,
    ) -> Result<UpstreamRequest, GatewayError> {
        let body = encode_chat_body(request, self.limits)?;
        self.json_request(
            join_within(&self.base_url, "chat/completions")?,
            body,
            request.stream,
        )
    }

    fn build_anthropic(
        &self,
        request: &CanonicalRequest,
        operation: UpstreamOperation,
    ) -> Result<UpstreamRequest, GatewayError> {
        let mut content = Vec::with_capacity(request.input.len());
        for item in &request.input {
            content.push(match item {
                InputItem::Text { text } => json!({"type": "text", "text": text}),
                InputItem::ImageUrl { url, .. } => json!({
                    "type": "image",
                    "source": {"type": "url", "url": url.as_str()}
                }),
                InputItem::ToolResult { call_id, output } => {
                    validate_identifier(call_id, self.limits.max_identifier_bytes)?;
                    validate_value_size(output, self.limits.max_tool_argument_bytes)?;
                    json!({
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "content": output
                    })
                }
            });
        }
        let tools = request
            .tools
            .iter()
            .map(|tool| {
                validate_identifier(&tool.name, self.limits.max_identifier_bytes)?;
                validate_value_size(&tool.input_schema, self.limits.max_request_body_bytes)?;
                Ok(json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.input_schema
                }))
            })
            .collect::<Result<Vec<_>, GatewayError>>()?;
        let mut body = json!({
            "model": request.model.as_str(),
            "messages": [{"role": "user", "content": content}],
        });
        if operation == UpstreamOperation::Text {
            body["max_tokens"] = json!(anthropic_max_tokens(request));
            body["stream"] = json!(request.stream);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        if request.reasoning.is_some() {
            body["thinking"] = json!({
                "type": "enabled",
                "budget_tokens": anthropic_thinking_budget(request)
            });
        }
        let relative = match operation {
            UpstreamOperation::Text => "v1/messages",
            UpstreamOperation::CountTokens => "v1/messages/count_tokens",
        };
        let mut outbound = self.json_request(
            join_within(&self.base_url, relative)?,
            body,
            operation == UpstreamOperation::Text && request.stream,
        )?;
        outbound.headers.insert(
            "anthropic-version".to_owned(),
            ANTHROPIC_API_VERSION.to_owned(),
        );
        Ok(outbound)
    }

    fn build_gemini(&self, request: &CanonicalRequest) -> Result<UpstreamRequest, GatewayError> {
        let mut request = request.clone();
        if request.reasoning.is_some() && !request.extensions.contains_key("gemini.thinking_config")
        {
            request.extensions.insert(
                "gemini.thinking_config".to_owned(),
                json!({"thinkingBudget": anthropic_thinking_budget(&request)}),
            );
        }
        let mut outbound = GeminiAdapter::new(
            GeminiConfig::new(self.base_url.clone(), "credential-injected-by-transport")?,
            self.limits,
        )
        .build_request(&request)?;
        outbound.headers.remove("x-goog-api-key");
        outbound.headers.insert(
            "accept".to_owned(),
            expected_accept(request.stream).to_owned(),
        );
        Ok(outbound)
    }

    fn build_azure(&self, request: &CanonicalRequest) -> Result<UpstreamRequest, GatewayError> {
        validate_identifier(request.model.as_str(), self.limits.max_identifier_bytes)?;
        let base_path = self.base_url.path().trim_end_matches('/');
        let relative = if base_path.ends_with("/openai") {
            format!("deployments/{}/chat/completions", request.model.as_str())
        } else {
            format!(
                "openai/deployments/{}/chat/completions",
                request.model.as_str()
            )
        };
        let mut url = join_within(&self.base_url, &relative)?;
        url.query_pairs_mut()
            .append_pair("api-version", DEFAULT_AZURE_API_VERSION);
        self.json_request(url, encode_chat_body(request, self.limits)?, request.stream)
    }

    fn json_request(
        &self,
        url: Url,
        body: Value,
        stream: bool,
    ) -> Result<UpstreamRequest, GatewayError> {
        let body = serde_json::to_vec(&body).map_err(|_| GatewayError::invalid_request())?;
        if body.len() > self.limits.max_request_body_bytes {
            return Err(GatewayError::invalid_request());
        }
        Ok(UpstreamRequest {
            url,
            headers: BTreeMap::from([
                ("accept".to_owned(), expected_accept(stream).to_owned()),
                ("content-type".to_owned(), "application/json".to_owned()),
            ]),
            body,
            stream,
        })
    }

    fn azure_decoder_adapter(&self) -> Result<AzureAdapter, GatewayError> {
        Ok(AzureAdapter::new(
            AzureConfig::new(
                self.base_url.clone(),
                "decoder",
                DEFAULT_AZURE_API_VERSION,
                "credential-injected-by-transport",
            )?,
            self.limits,
        ))
    }

    fn gemini_decoder_adapter(&self) -> Result<GeminiAdapter, GatewayError> {
        Ok(GeminiAdapter::new(
            GeminiConfig::new(self.base_url.clone(), "credential-injected-by-transport")?,
            self.limits,
        ))
    }
}

impl std::fmt::Debug for UpstreamAdapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UpstreamAdapter")
            .field("protocol", &self.protocol)
            .field("base_url", &"[redacted]")
            .field("limits", &self.limits)
            .finish()
    }
}

pub struct UpstreamStreamDecoder {
    inner: StreamDecoderInner,
}

enum StreamDecoderInner {
    Responses(ResponsesStreamDecoder),
    OpenAiChat(AzureStreamDecoder),
    Anthropic(AnthropicStreamDecoder),
    Gemini(GeminiStreamDecoder),
    Azure(AzureStreamDecoder),
}

impl UpstreamStreamDecoder {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<CanonicalEvent>, GatewayError> {
        match &mut self.inner {
            StreamDecoderInner::Responses(decoder) => decoder.push(chunk),
            StreamDecoderInner::OpenAiChat(decoder) | StreamDecoderInner::Azure(decoder) => {
                decoder.push(chunk)
            }
            StreamDecoderInner::Anthropic(decoder) => decoder.push(chunk),
            StreamDecoderInner::Gemini(decoder) => decoder.push(chunk),
        }
    }

    pub fn finish(&mut self) -> Result<Vec<CanonicalEvent>, GatewayError> {
        match &mut self.inner {
            StreamDecoderInner::Responses(decoder) => decoder.finish(),
            StreamDecoderInner::OpenAiChat(decoder) | StreamDecoderInner::Azure(decoder) => {
                decoder.finish()
            }
            StreamDecoderInner::Anthropic(decoder) => decoder.finish(),
            StreamDecoderInner::Gemini(decoder) => decoder.finish(),
        }
    }
}

fn normalize_base_url(mut url: Url) -> Result<Url, GatewayError> {
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(GatewayError::invalid_request());
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

fn join_within(base: &Url, relative: &str) -> Result<Url, GatewayError> {
    if relative.starts_with('/') || relative.contains(['?', '#', '\\']) {
        return Err(GatewayError::invalid_request());
    }
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

fn validate_limits(limits: UpstreamLimits) -> Result<(), GatewayError> {
    if limits.max_request_body_bytes == 0
        || limits.max_response_body_bytes == 0
        || limits.max_stream_frame_bytes == 0
        || limits.max_events == 0
        || limits.max_collection_items == 0
        || limits.max_identifier_bytes == 0
        || limits.max_text_delta_bytes == 0
        || limits.max_tool_argument_bytes == 0
    {
        return Err(GatewayError::invalid_request());
    }
    Ok(())
}

fn validate_request(
    request: &CanonicalRequest,
    limits: UpstreamLimits,
) -> Result<(), GatewayError> {
    validate_identifier(request.model.as_str(), limits.max_identifier_bytes)?;
    if request.input.len() > limits.max_collection_items
        || request.tools.len() > limits.max_collection_items
    {
        return Err(GatewayError::invalid_request());
    }
    for item in &request.input {
        match item {
            InputItem::Text { text } if text.len() > limits.max_text_delta_bytes => {
                return Err(GatewayError::invalid_request());
            }
            InputItem::ToolResult { call_id, output } => {
                validate_identifier(call_id, limits.max_identifier_bytes)?;
                validate_value_size(output, limits.max_tool_argument_bytes)?;
            }
            InputItem::ImageUrl { url, .. }
                if !matches!(url.scheme(), "http" | "https" | "data") =>
            {
                return Err(GatewayError::unsupported_capability());
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_identifier(value: &str, maximum_bytes: usize) -> Result<(), GatewayError> {
    if value.is_empty()
        || value.len() > maximum_bytes
        || value.chars().any(char::is_control)
        || matches!(value, "." | "..")
    {
        return Err(GatewayError::invalid_request());
    }
    Ok(())
}

fn validate_value_size(value: &Value, maximum_bytes: usize) -> Result<(), GatewayError> {
    if serde_json::to_vec(value)
        .map_err(|_| GatewayError::invalid_request())?
        .len()
        > maximum_bytes
    {
        return Err(GatewayError::invalid_request());
    }
    Ok(())
}

fn expected_accept(stream: bool) -> &'static str {
    if stream {
        "text/event-stream"
    } else {
        "application/json"
    }
}

fn encode_reasoning(request: &CanonicalRequest) -> Result<Option<Value>, GatewayError> {
    request
        .reasoning
        .as_ref()
        .map(|reasoning| {
            let effort = reasoning
                .effort
                .as_deref()
                .ok_or_else(GatewayError::unsupported_capability)?;
            validate_identifier(effort, 32)?;
            Ok(json!({"effort": effort}))
        })
        .transpose()
}

fn encode_responses_input(
    request: &CanonicalRequest,
    limits: UpstreamLimits,
) -> Result<Vec<Value>, GatewayError> {
    request
        .input
        .iter()
        .map(|item| {
            Ok(match item {
                InputItem::Text { text } => json!({
                    "role": "user",
                    "content": [{"type": "input_text", "text": text}]
                }),
                InputItem::ImageUrl { url, detail } => json!({
                    "role": "user",
                    "content": [{
                        "type": "input_image",
                        "image_url": url.as_str(),
                        "detail": image_detail(*detail)
                    }]
                }),
                InputItem::ToolResult { call_id, output } => {
                    validate_identifier(call_id, limits.max_identifier_bytes)?;
                    validate_value_size(output, limits.max_tool_argument_bytes)?;
                    json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": output
                    })
                }
            })
        })
        .collect()
}

fn encode_responses_tools(
    request: &CanonicalRequest,
    limits: UpstreamLimits,
) -> Result<Vec<Value>, GatewayError> {
    request
        .tools
        .iter()
        .map(|tool| {
            validate_identifier(&tool.name, limits.max_identifier_bytes)?;
            validate_value_size(&tool.input_schema, limits.max_request_body_bytes)?;
            Ok(json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
                "strict": false
            }))
        })
        .collect()
}

fn encode_chat_body(
    request: &CanonicalRequest,
    limits: UpstreamLimits,
) -> Result<Value, GatewayError> {
    let mut messages = Vec::with_capacity(request.input.len());
    for item in &request.input {
        messages.push(match item {
            InputItem::Text { text } => json!({"role": "user", "content": text}),
            InputItem::ImageUrl { url, detail } => json!({
                "role": "user",
                "content": [{
                    "type": "image_url",
                    "image_url": {
                        "url": url.as_str(),
                        "detail": image_detail(*detail)
                    }
                }]
            }),
            InputItem::ToolResult { call_id, output } => {
                validate_identifier(call_id, limits.max_identifier_bytes)?;
                validate_value_size(output, limits.max_tool_argument_bytes)?;
                json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": serde_json::to_string(output)
                        .map_err(|_| GatewayError::invalid_request())?
                })
            }
        });
    }
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            validate_identifier(&tool.name, limits.max_identifier_bytes)?;
            validate_value_size(&tool.input_schema, limits.max_request_body_bytes)?;
            Ok(json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema
                }
            }))
        })
        .collect::<Result<Vec<_>, GatewayError>>()?;
    let mut body = json!({
        "model": request.model.as_str(),
        "messages": messages,
        "stream": request.stream
    });
    if request.stream {
        body["stream_options"] = json!({"include_usage": true});
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(reasoning) = request.reasoning.as_ref() {
        let effort = reasoning
            .effort
            .as_deref()
            .ok_or_else(GatewayError::unsupported_capability)?;
        validate_identifier(effort, 32)?;
        body["reasoning_effort"] = json!(effort);
    }
    Ok(body)
}

fn image_detail(detail: Option<ImageDetail>) -> &'static str {
    match detail {
        None | Some(ImageDetail::Auto) => "auto",
        Some(ImageDetail::Low) => "low",
        Some(ImageDetail::High) => "high",
    }
}

fn anthropic_max_tokens(request: &CanonicalRequest) -> u64 {
    request
        .extensions
        .get("max_output_tokens")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_ANTHROPIC_MAX_TOKENS)
}

fn anthropic_thinking_budget(request: &CanonicalRequest) -> u64 {
    request
        .reasoning
        .as_ref()
        .and_then(|reasoning| reasoning.extensions.get("budget_tokens"))
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .unwrap_or(1_024)
}

fn decode_responses_complete(
    request_id: RequestId,
    body: &[u8],
    limits: UpstreamLimits,
) -> Result<Vec<CanonicalEvent>, GatewayError> {
    let root: Value = serde_json::from_slice(body).map_err(|_| GatewayError::invalid_request())?;
    if root.get("error").is_some() {
        return Err(GatewayError::upstream_response(
            502,
            "responses upstream error",
        ));
    }
    let response_id = root
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_else(|| request_id.as_str());
    validate_identifier(response_id, limits.max_identifier_bytes)?;
    let mut events = vec![CanonicalEvent::Created {
        response_id: response_id.to_owned(),
    }];
    let output = root
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(GatewayError::invalid_request)?;
    if output.len() > limits.max_collection_items {
        return Err(GatewayError::invalid_request());
    }
    for (index, item) in output.iter().enumerate() {
        decode_responses_output(item, index, limits, &mut events)?;
    }
    push_event(
        &mut events,
        CanonicalEvent::Usage(openai_responses_usage(root.get("usage"), limits)?),
        limits,
    )?;
    push_event(&mut events, CanonicalEvent::Completed, limits)?;
    Ok(events)
}

fn decode_responses_output(
    item: &Value,
    index: usize,
    limits: UpstreamLimits,
    events: &mut Vec<CanonicalEvent>,
) -> Result<(), GatewayError> {
    let kind = item
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(GatewayError::invalid_request)?;
    let item_id = item
        .get("id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("responses_item_{index}"));
    validate_identifier(&item_id, limits.max_identifier_bytes)?;
    match kind {
        "message" => {
            let content = item
                .get("content")
                .and_then(Value::as_array)
                .ok_or_else(GatewayError::invalid_request)?;
            for part in content {
                if let Some(text) = part.get("text").and_then(Value::as_str).filter(|_| {
                    matches!(
                        part.get("type").and_then(Value::as_str),
                        Some("output_text" | "refusal")
                    )
                }) {
                    push_text(events, &item_id, text, false, limits)?;
                }
            }
        }
        "reasoning" => {
            if let Some(summary) = item.get("summary").and_then(Value::as_array) {
                for part in summary {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        push_text(events, &item_id, text, true, limits)?;
                    }
                }
            }
        }
        "function_call" => {
            let call_id = required_string(item, "call_id")?;
            let name = required_string(item, "name")?;
            let arguments = required_string(item, "arguments")?;
            push_tool(events, &item_id, call_id, name, arguments, limits)?;
        }
        _ => {}
    }
    Ok(())
}

fn decode_anthropic_complete(
    request_id: RequestId,
    body: &[u8],
    limits: UpstreamLimits,
) -> Result<Vec<CanonicalEvent>, GatewayError> {
    let root: Value = serde_json::from_slice(body).map_err(|_| GatewayError::invalid_request())?;
    if root.get("type").and_then(Value::as_str) == Some("error") || root.get("error").is_some() {
        return Err(GatewayError::upstream_response(
            502,
            "anthropic upstream error",
        ));
    }
    let response_id = root
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_else(|| request_id.as_str());
    validate_identifier(response_id, limits.max_identifier_bytes)?;
    let mut events = vec![CanonicalEvent::Created {
        response_id: response_id.to_owned(),
    }];
    let content = root
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(GatewayError::invalid_request)?;
    if content.len() > limits.max_collection_items {
        return Err(GatewayError::invalid_request());
    }
    for (index, block) in content.iter().enumerate() {
        let item_id = format!("anthropic_item_{index}");
        match block.get("type").and_then(Value::as_str) {
            Some("text") => push_text(
                &mut events,
                &item_id,
                required_string(block, "text")?,
                false,
                limits,
            )?,
            Some("thinking") => push_text(
                &mut events,
                &item_id,
                required_string(block, "thinking")?,
                true,
                limits,
            )?,
            Some("tool_use") => {
                let call_id = required_string(block, "id")?;
                let name = required_string(block, "name")?;
                let arguments =
                    serde_json::to_string(block.get("input").unwrap_or(&Value::Object(Map::new())))
                        .map_err(|_| GatewayError::invalid_request())?;
                push_tool(&mut events, &item_id, call_id, name, &arguments, limits)?;
            }
            _ => {}
        }
    }
    push_event(
        &mut events,
        CanonicalEvent::Usage(anthropic_usage(root.get("usage"), limits)?),
        limits,
    )?;
    push_event(&mut events, CanonicalEvent::Completed, limits)?;
    Ok(events)
}

fn openai_responses_usage(
    value: Option<&Value>,
    limits: UpstreamLimits,
) -> Result<Usage, GatewayError> {
    let Some(value) = value else {
        return Ok(empty_usage());
    };
    validate_value_size(value, limits.max_response_body_bytes)?;
    let input_tokens = optional_u64(value, "input_tokens")?.unwrap_or(0);
    let output_tokens = optional_u64(value, "output_tokens")?.unwrap_or(0);
    let cached_input_tokens = value
        .get("input_tokens_details")
        .map(|details| optional_u64(details, "cached_tokens"))
        .transpose()?
        .flatten();
    let reasoning_tokens = value
        .get("output_tokens_details")
        .map(|details| optional_u64(details, "reasoning_tokens"))
        .transpose()?
        .flatten();
    Ok(Usage {
        input_tokens,
        output_tokens,
        cached_input_tokens,
        reasoning_tokens,
        extensions: BTreeMap::new(),
    })
}

fn anthropic_usage(value: Option<&Value>, limits: UpstreamLimits) -> Result<Usage, GatewayError> {
    let Some(value) = value else {
        return Ok(empty_usage());
    };
    validate_value_size(value, limits.max_response_body_bytes)?;
    Ok(Usage {
        input_tokens: optional_u64(value, "input_tokens")?.unwrap_or(0),
        output_tokens: optional_u64(value, "output_tokens")?.unwrap_or(0),
        cached_input_tokens: optional_u64(value, "cache_read_input_tokens")?,
        reasoning_tokens: None,
        extensions: BTreeMap::new(),
    })
}

fn empty_usage() -> Usage {
    Usage {
        input_tokens: 0,
        output_tokens: 0,
        cached_input_tokens: None,
        reasoning_tokens: None,
        extensions: BTreeMap::new(),
    }
}

fn optional_u64(value: &Value, key: &str) -> Result<Option<u64>, GatewayError> {
    match value.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(GatewayError::invalid_request),
    }
}

fn required_string<'a>(value: &'a Value, key: &str) -> Result<&'a str, GatewayError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(GatewayError::invalid_request)
}

fn push_text(
    events: &mut Vec<CanonicalEvent>,
    item_id: &str,
    text: &str,
    reasoning: bool,
    limits: UpstreamLimits,
) -> Result<(), GatewayError> {
    validate_identifier(item_id, limits.max_identifier_bytes)?;
    if text.len() > limits.max_text_delta_bytes {
        return Err(GatewayError::invalid_request());
    }
    let event = if reasoning {
        CanonicalEvent::ReasoningDelta {
            item_id: item_id.to_owned(),
            delta: text.to_owned(),
        }
    } else {
        CanonicalEvent::OutputTextDelta {
            item_id: item_id.to_owned(),
            delta: text.to_owned(),
        }
    };
    push_event(events, event, limits)
}

fn push_tool(
    events: &mut Vec<CanonicalEvent>,
    item_id: &str,
    call_id: &str,
    name: &str,
    delta: &str,
    limits: UpstreamLimits,
) -> Result<(), GatewayError> {
    validate_identifier(item_id, limits.max_identifier_bytes)?;
    validate_identifier(call_id, limits.max_identifier_bytes)?;
    validate_identifier(name, limits.max_identifier_bytes)?;
    if delta.len() > limits.max_tool_argument_bytes {
        return Err(GatewayError::invalid_request());
    }
    push_event(
        events,
        CanonicalEvent::ToolCallDelta {
            item_id: item_id.to_owned(),
            call_id: call_id.to_owned(),
            name: name.to_owned(),
            delta: delta.to_owned(),
        },
        limits,
    )
}

fn push_event(
    events: &mut Vec<CanonicalEvent>,
    event: CanonicalEvent,
    limits: UpstreamLimits,
) -> Result<(), GatewayError> {
    if events.len() >= limits.max_events {
        return Err(GatewayError::invalid_request());
    }
    events.push(event);
    Ok(())
}

struct ResponsesStreamDecoder {
    sse: SseDecoder,
    request_id: RequestId,
    limits: UpstreamLimits,
    received_bytes: usize,
    emitted_events: usize,
    created: bool,
    completed: bool,
    tools: HashMap<String, (String, String)>,
}

impl ResponsesStreamDecoder {
    fn new(request_id: RequestId, limits: UpstreamLimits) -> Self {
        Self {
            sse: SseDecoder::with_limits(limits.max_stream_frame_bytes, limits.max_events),
            request_id,
            limits,
            received_bytes: 0,
            emitted_events: 0,
            created: false,
            completed: false,
            tools: HashMap::new(),
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Result<Vec<CanonicalEvent>, GatewayError> {
        if self.completed {
            return Err(GatewayError::invalid_request());
        }
        self.received_bytes = self.received_bytes.saturating_add(chunk.len());
        if self.received_bytes > self.limits.max_response_body_bytes {
            return Err(GatewayError::invalid_request());
        }
        let frames = self
            .sse
            .push(chunk)
            .map_err(|_| GatewayError::invalid_request())?;
        let mut events = Vec::new();
        for frame in frames {
            let value: Value =
                serde_json::from_str(&frame.data).map_err(|_| GatewayError::invalid_request())?;
            self.decode_event(&value, &mut events)?;
        }
        self.account(events.len())?;
        Ok(events)
    }

    fn decode_event(
        &mut self,
        value: &Value,
        events: &mut Vec<CanonicalEvent>,
    ) -> Result<(), GatewayError> {
        let kind = required_string(value, "type")?;
        match kind {
            "response.created" => {
                if self.created {
                    return Err(GatewayError::invalid_request());
                }
                let response_id = value
                    .get("response")
                    .and_then(|response| response.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| self.request_id.as_str());
                validate_identifier(response_id, self.limits.max_identifier_bytes)?;
                events.push(CanonicalEvent::Created {
                    response_id: response_id.to_owned(),
                });
                self.created = true;
            }
            "response.output_item.added" => {
                if let Some(item) = value.get("item")
                    && item.get("type").and_then(Value::as_str) == Some("function_call")
                {
                    let item_id = required_string(item, "id")?;
                    let call_id = required_string(item, "call_id")?;
                    let name = required_string(item, "name")?;
                    self.tools
                        .insert(item_id.to_owned(), (call_id.to_owned(), name.to_owned()));
                }
            }
            "response.output_text.delta" => {
                self.require_created()?;
                push_text(
                    events,
                    required_string(value, "item_id")?,
                    required_string(value, "delta")?,
                    false,
                    self.limits,
                )?;
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.require_created()?;
                push_text(
                    events,
                    required_string(value, "item_id")?,
                    required_string(value, "delta")?,
                    true,
                    self.limits,
                )?;
            }
            "response.function_call_arguments.delta" => {
                self.require_created()?;
                let item_id = required_string(value, "item_id")?;
                let (call_id, name) = self
                    .tools
                    .get(item_id)
                    .ok_or_else(GatewayError::invalid_request)?;
                push_tool(
                    events,
                    item_id,
                    call_id,
                    name,
                    required_string(value, "delta")?,
                    self.limits,
                )?;
            }
            "response.completed" => {
                self.require_created()?;
                let usage = openai_responses_usage(
                    value
                        .get("response")
                        .and_then(|response| response.get("usage")),
                    self.limits,
                )?;
                events.push(CanonicalEvent::Usage(usage));
                events.push(CanonicalEvent::Completed);
                self.completed = true;
            }
            "response.failed" | "error" => {
                return Err(GatewayError::upstream_response(
                    502,
                    "responses stream failed",
                ));
            }
            _ => {}
        }
        Ok(())
    }

    fn require_created(&self) -> Result<(), GatewayError> {
        if self.created {
            Ok(())
        } else {
            Err(GatewayError::invalid_request())
        }
    }

    fn account(&mut self, added: usize) -> Result<(), GatewayError> {
        self.emitted_events = self.emitted_events.saturating_add(added);
        if self.emitted_events > self.limits.max_events {
            return Err(GatewayError::invalid_request());
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<Vec<CanonicalEvent>, GatewayError> {
        self.sse
            .finish()
            .map_err(|_| GatewayError::invalid_request())?;
        if !self.completed {
            return Err(GatewayError::invalid_request());
        }
        Ok(Vec::new())
    }
}

enum AnthropicBlock {
    Text {
        item_id: String,
    },
    Thinking {
        item_id: String,
    },
    Tool {
        item_id: String,
        call_id: String,
        name: String,
    },
}

struct AnthropicStreamDecoder {
    sse: SseDecoder,
    request_id: RequestId,
    limits: UpstreamLimits,
    received_bytes: usize,
    emitted_events: usize,
    created: bool,
    completed: bool,
    input_tokens: u64,
    output_tokens: u64,
    blocks: BTreeMap<usize, AnthropicBlock>,
}

impl AnthropicStreamDecoder {
    fn new(request_id: RequestId, limits: UpstreamLimits) -> Self {
        Self {
            sse: SseDecoder::with_limits(limits.max_stream_frame_bytes, limits.max_events),
            request_id,
            limits,
            received_bytes: 0,
            emitted_events: 0,
            created: false,
            completed: false,
            input_tokens: 0,
            output_tokens: 0,
            blocks: BTreeMap::new(),
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Result<Vec<CanonicalEvent>, GatewayError> {
        if self.completed {
            return Err(GatewayError::invalid_request());
        }
        self.received_bytes = self.received_bytes.saturating_add(chunk.len());
        if self.received_bytes > self.limits.max_response_body_bytes {
            return Err(GatewayError::invalid_request());
        }
        let frames = self
            .sse
            .push(chunk)
            .map_err(|_| GatewayError::invalid_request())?;
        let mut events = Vec::new();
        for frame in frames {
            let value: Value =
                serde_json::from_str(&frame.data).map_err(|_| GatewayError::invalid_request())?;
            self.decode_event(&value, &mut events)?;
        }
        self.emitted_events = self.emitted_events.saturating_add(events.len());
        if self.emitted_events > self.limits.max_events {
            return Err(GatewayError::invalid_request());
        }
        Ok(events)
    }

    fn decode_event(
        &mut self,
        value: &Value,
        events: &mut Vec<CanonicalEvent>,
    ) -> Result<(), GatewayError> {
        match required_string(value, "type")? {
            "message_start" => {
                if self.created {
                    return Err(GatewayError::invalid_request());
                }
                let message = value
                    .get("message")
                    .ok_or_else(GatewayError::invalid_request)?;
                let response_id = message
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| self.request_id.as_str());
                validate_identifier(response_id, self.limits.max_identifier_bytes)?;
                if let Some(usage) = message.get("usage") {
                    self.input_tokens = optional_u64(usage, "input_tokens")?.unwrap_or(0);
                    self.output_tokens = optional_u64(usage, "output_tokens")?.unwrap_or(0);
                }
                events.push(CanonicalEvent::Created {
                    response_id: response_id.to_owned(),
                });
                self.created = true;
            }
            "content_block_start" => {
                self.require_created()?;
                let index = required_index(value, self.limits.max_collection_items)?;
                let block = value
                    .get("content_block")
                    .ok_or_else(GatewayError::invalid_request)?;
                let item_id = format!("anthropic_item_{index}");
                let block = match required_string(block, "type")? {
                    "text" => AnthropicBlock::Text { item_id },
                    "thinking" => AnthropicBlock::Thinking { item_id },
                    "tool_use" => AnthropicBlock::Tool {
                        item_id,
                        call_id: required_string(block, "id")?.to_owned(),
                        name: required_string(block, "name")?.to_owned(),
                    },
                    _ => return Ok(()),
                };
                if self.blocks.insert(index, block).is_some() {
                    return Err(GatewayError::invalid_request());
                }
            }
            "content_block_delta" => {
                self.require_created()?;
                let index = required_index(value, self.limits.max_collection_items)?;
                let delta = value
                    .get("delta")
                    .ok_or_else(GatewayError::invalid_request)?;
                let block = self
                    .blocks
                    .get(&index)
                    .ok_or_else(GatewayError::invalid_request)?;
                match (block, required_string(delta, "type")?) {
                    (AnthropicBlock::Text { item_id }, "text_delta") => push_text(
                        events,
                        item_id,
                        required_string(delta, "text")?,
                        false,
                        self.limits,
                    )?,
                    (AnthropicBlock::Thinking { item_id }, "thinking_delta") => push_text(
                        events,
                        item_id,
                        required_string(delta, "thinking")?,
                        true,
                        self.limits,
                    )?,
                    (
                        AnthropicBlock::Tool {
                            item_id,
                            call_id,
                            name,
                        },
                        "input_json_delta",
                    ) => push_tool(
                        events,
                        item_id,
                        call_id,
                        name,
                        required_string(delta, "partial_json")?,
                        self.limits,
                    )?,
                    _ => return Err(GatewayError::invalid_request()),
                }
            }
            "message_delta" => {
                self.require_created()?;
                if let Some(usage) = value.get("usage") {
                    self.output_tokens =
                        optional_u64(usage, "output_tokens")?.unwrap_or(self.output_tokens);
                }
            }
            "message_stop" => {
                self.require_created()?;
                events.push(CanonicalEvent::Usage(Usage {
                    input_tokens: self.input_tokens,
                    output_tokens: self.output_tokens,
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                    extensions: BTreeMap::new(),
                }));
                events.push(CanonicalEvent::Completed);
                self.completed = true;
            }
            "error" => {
                return Err(GatewayError::upstream_response(
                    502,
                    "anthropic stream failed",
                ));
            }
            _ => {}
        }
        Ok(())
    }

    fn require_created(&self) -> Result<(), GatewayError> {
        if self.created {
            Ok(())
        } else {
            Err(GatewayError::invalid_request())
        }
    }

    fn finish(&mut self) -> Result<Vec<CanonicalEvent>, GatewayError> {
        self.sse
            .finish()
            .map_err(|_| GatewayError::invalid_request())?;
        if !self.completed {
            return Err(GatewayError::invalid_request());
        }
        Ok(Vec::new())
    }
}

fn required_index(value: &Value, maximum: usize) -> Result<usize, GatewayError> {
    let index = value
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(GatewayError::invalid_request)?;
    if index >= maximum {
        return Err(GatewayError::invalid_request());
    }
    Ok(index)
}
