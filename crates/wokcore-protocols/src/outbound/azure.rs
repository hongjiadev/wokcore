use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use url::Url;

use crate::{
    canonical::{CanonicalEvent, CanonicalRequest, GatewayError, InputItem, RequestId, Usage},
    stream::SseDecoder,
};

use super::{
    UpstreamLimits, UpstreamRequest, account_stream_events, checked_join, classify_http_error,
    encode_bounded, push_event, validate_base_url, validate_identifier, validate_value_size,
};

#[derive(Clone)]
pub struct AzureConfig {
    base_url: Url,
    deployment: String,
    api_version: String,
    api_key: String,
}

impl std::fmt::Debug for AzureConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AzureConfig")
            .field("base_url", &"[redacted]")
            .field("deployment", &"[redacted]")
            .field("api_version", &self.api_version)
            .field("api_key", &"[redacted]")
            .finish()
    }
}

impl AzureConfig {
    pub fn new(
        base_url: Url,
        deployment: impl Into<String>,
        api_version: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Result<Self, GatewayError> {
        validate_base_url(&base_url)?;
        let deployment = deployment.into();
        validate_identifier(&deployment, 128)?;
        let api_version = api_version.into();
        validate_api_version(&api_version)?;
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Err(GatewayError::invalid_request());
        }
        Ok(Self {
            base_url,
            deployment,
            api_version,
            api_key,
        })
    }
}

pub struct AzureAdapter {
    config: AzureConfig,
    limits: UpstreamLimits,
}

impl AzureAdapter {
    pub fn new(config: AzureConfig, limits: UpstreamLimits) -> Self {
        Self { config, limits }
    }

    pub fn build_request(
        &self,
        request: &CanonicalRequest,
    ) -> Result<UpstreamRequest, GatewayError> {
        if request.input.len() > self.limits.max_collection_items
            || request.tools.len() > self.limits.max_collection_items
        {
            return Err(GatewayError::invalid_request());
        }
        let deployment_directory = checked_join(&self.config.base_url, "openai/deployments/")?;
        let deployment_url = deployment_directory
            .join(&format!("{}/", self.config.deployment))
            .map_err(|_| GatewayError::invalid_request())?;
        let mut url = deployment_url
            .join("chat/completions")
            .map_err(|_| GatewayError::invalid_request())?;
        if url.scheme() != self.config.base_url.scheme()
            || url.host_str() != self.config.base_url.host_str()
            || !url.path().starts_with(deployment_directory.path())
        {
            return Err(GatewayError::invalid_request());
        }
        url.query_pairs_mut()
            .append_pair("api-version", &self.config.api_version);

        let mut messages = Vec::with_capacity(request.input.len());
        for item in &request.input {
            match item {
                InputItem::Text { text } => {
                    if text.len() > self.limits.max_text_delta_bytes {
                        return Err(GatewayError::invalid_request());
                    }
                    messages.push(json!({"role": "user", "content": text}));
                }
                InputItem::ImageUrl { url, detail } => {
                    if !matches!(url.scheme(), "http" | "https" | "data") {
                        return Err(GatewayError::unsupported_capability());
                    }
                    messages.push(json!({
                        "role": "user",
                        "content": [{
                            "type": "image_url",
                            "image_url": {"url": url, "detail": detail}
                        }]
                    }));
                }
                InputItem::ToolResult { call_id, output } => {
                    validate_identifier(call_id, self.limits.max_identifier_bytes)?;
                    validate_value_size(output, self.limits.max_tool_argument_bytes)?;
                    let content = serde_json::to_string(output)
                        .map_err(|_| GatewayError::invalid_request())?;
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": content
                    }));
                }
            }
        }
        let mut body = json!({
            "messages": messages,
            "stream": request.stream
        });
        if !request.tools.is_empty() {
            let tools = request
                .tools
                .iter()
                .map(|tool| {
                    validate_identifier(&tool.name, self.limits.max_identifier_bytes)?;
                    validate_value_size(&tool.input_schema, self.limits.max_request_body_bytes)?;
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
            body["tools"] = Value::Array(tools);
        }
        if request.reasoning.is_some() {
            let effort = request
                .reasoning
                .as_ref()
                .and_then(|reasoning| reasoning.effort.as_deref())
                .ok_or_else(GatewayError::unsupported_capability)?;
            validate_identifier(effort, 32)?;
            body["reasoning_effort"] = Value::String(effort.to_owned());
        }
        let body = encode_bounded(&body, self.limits.max_request_body_bytes)?;
        Ok(UpstreamRequest {
            url,
            headers: BTreeMap::from([
                ("api-key".to_owned(), self.config.api_key.clone()),
                ("content-type".to_owned(), "application/json".to_owned()),
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
        let mut decoder = AzureResponseDecoder::new(request_id, self.limits);
        let mut emitted_events = 0;
        let mut events = decoder.decode_non_stream(&value)?;
        account_stream_events(&mut emitted_events, events.len(), self.limits)?;
        let completed = decoder.finish()?;
        account_stream_events(&mut emitted_events, completed.len(), self.limits)?;
        events.extend(completed);
        Ok(events)
    }

    pub fn decode_http_error(&self, status: u16, retry_after: Option<&str>) -> GatewayError {
        classify_http_error(status, retry_after, "azure upstream")
    }

    pub fn stream_decoder(&self, request_id: RequestId) -> AzureStreamDecoder {
        AzureStreamDecoder {
            sse: SseDecoder::with_limits(
                self.limits.max_stream_frame_bytes,
                self.limits.max_events,
            ),
            response: AzureResponseDecoder::new(request_id, self.limits),
            received_bytes: 0,
            limits: self.limits,
            failed: false,
            finished: false,
            saw_done: false,
            emitted_events: 0,
        }
    }
}

pub struct AzureStreamDecoder {
    sse: SseDecoder,
    response: AzureResponseDecoder,
    received_bytes: usize,
    limits: UpstreamLimits,
    failed: bool,
    finished: bool,
    saw_done: bool,
    emitted_events: usize,
}

impl AzureStreamDecoder {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<CanonicalEvent>, GatewayError> {
        if self.failed || self.finished || self.saw_done {
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
            if self.saw_done {
                self.failed = true;
                return Err(GatewayError::invalid_request());
            }
            if frame.data.trim() == "[DONE]" {
                self.saw_done = true;
                continue;
            }
            let value: Value = serde_json::from_str(&frame.data).map_err(|_| {
                self.failed = true;
                GatewayError::invalid_request()
            })?;
            let decoded = self.response.decode_stream_chunk(&value).inspect_err(|_| {
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
        if !self.saw_done {
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

struct AzureResponseDecoder {
    request_id: RequestId,
    limits: UpstreamLimits,
    created: bool,
    completed: bool,
    next_item: usize,
    tool_calls: BTreeMap<usize, ToolState>,
    pending_usage: Option<Usage>,
}

struct ToolState {
    item_id: String,
    call_id: String,
    name: String,
}

impl AzureResponseDecoder {
    fn new(request_id: RequestId, limits: UpstreamLimits) -> Self {
        Self {
            request_id,
            limits,
            created: false,
            completed: false,
            next_item: 0,
            tool_calls: BTreeMap::new(),
            pending_usage: None,
        }
    }

    fn ensure_created(
        &mut self,
        root: &Value,
        events: &mut Vec<CanonicalEvent>,
    ) -> Result<(), GatewayError> {
        if self.created {
            return Ok(());
        }
        let response_id = root
            .get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("azure_{}", self.request_id.as_str()));
        validate_identifier(&response_id, self.limits.max_identifier_bytes)?;
        push_event(events, CanonicalEvent::Created { response_id }, self.limits)?;
        self.created = true;
        Ok(())
    }

    fn decode_non_stream(&mut self, root: &Value) -> Result<Vec<CanonicalEvent>, GatewayError> {
        reject_error(root)?;
        let mut events = Vec::new();
        self.ensure_created(root, &mut events)?;
        let choices = required_bounded_array(root, "choices", self.limits)?;
        for choice in choices {
            let message = choice
                .get("message")
                .and_then(Value::as_object)
                .ok_or_else(GatewayError::invalid_request)?;
            if let Some(content) = message.get("content").and_then(Value::as_str) {
                self.push_text(content, false, &mut events)?;
            }
            if let Some(reasoning) = message.get("reasoning_content").and_then(Value::as_str) {
                self.push_text(reasoning, true, &mut events)?;
            }
            if let Some(calls) = message.get("tool_calls") {
                for (index, call) in bounded_array(calls, self.limits)?.iter().enumerate() {
                    self.push_complete_tool(index, call, &mut events)?;
                }
            }
        }
        if let Some(usage) = root.get("usage") {
            self.pending_usage = Some(openai_usage(usage, self.limits)?);
        }
        Ok(events)
    }

    fn decode_stream_chunk(&mut self, root: &Value) -> Result<Vec<CanonicalEvent>, GatewayError> {
        reject_error(root)?;
        let mut events = Vec::new();
        self.ensure_created(root, &mut events)?;
        if let Some(choices) = root.get("choices") {
            for choice in bounded_array(choices, self.limits)? {
                let delta = choice
                    .get("delta")
                    .and_then(Value::as_object)
                    .ok_or_else(GatewayError::invalid_request)?;
                if let Some(content) = delta.get("content").and_then(Value::as_str) {
                    self.push_text(content, false, &mut events)?;
                }
                if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str) {
                    self.push_text(reasoning, true, &mut events)?;
                }
                if let Some(calls) = delta.get("tool_calls") {
                    for call in bounded_array(calls, self.limits)? {
                        self.push_tool_delta(call, &mut events)?;
                    }
                }
            }
        }
        if let Some(usage) = root.get("usage") {
            self.pending_usage = Some(openai_usage(usage, self.limits)?);
        }
        Ok(events)
    }

    fn push_text(
        &mut self,
        text: &str,
        reasoning: bool,
        events: &mut Vec<CanonicalEvent>,
    ) -> Result<(), GatewayError> {
        if text.len() > self.limits.max_text_delta_bytes {
            return Err(GatewayError::invalid_request());
        }
        let item_id = format!("azure_item_{}", self.next_item);
        self.next_item += 1;
        let event = if reasoning {
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
        push_event(events, event, self.limits)
    }

    fn push_complete_tool(
        &mut self,
        index: usize,
        call: &Value,
        events: &mut Vec<CanonicalEvent>,
    ) -> Result<(), GatewayError> {
        let function = call
            .get("function")
            .and_then(Value::as_object)
            .ok_or_else(GatewayError::invalid_request)?;
        let call_id = call
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(GatewayError::invalid_request)?;
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(GatewayError::invalid_request)?;
        let arguments = function
            .get("arguments")
            .and_then(Value::as_str)
            .ok_or_else(GatewayError::invalid_request)?;
        self.register_tool(index, call_id, name)?;
        self.emit_tool(index, arguments, events)
    }

    fn push_tool_delta(
        &mut self,
        call: &Value,
        events: &mut Vec<CanonicalEvent>,
    ) -> Result<(), GatewayError> {
        let index = call
            .get("index")
            .and_then(Value::as_u64)
            .ok_or_else(GatewayError::invalid_request)? as usize;
        if index >= self.limits.max_collection_items {
            return Err(GatewayError::invalid_request());
        }
        let function = call
            .get("function")
            .and_then(Value::as_object)
            .ok_or_else(GatewayError::invalid_request)?;
        if let (Some(call_id), Some(name)) = (
            call.get("id").and_then(Value::as_str),
            function.get("name").and_then(Value::as_str),
        ) {
            self.register_tool(index, call_id, name)?;
        }
        if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
            self.emit_tool(index, arguments, events)?;
        }
        Ok(())
    }

    fn register_tool(
        &mut self,
        index: usize,
        call_id: &str,
        name: &str,
    ) -> Result<(), GatewayError> {
        validate_identifier(call_id, self.limits.max_identifier_bytes)?;
        validate_identifier(name, self.limits.max_identifier_bytes)?;
        if let Some(existing) = self.tool_calls.get(&index) {
            if existing.call_id != call_id || existing.name != name {
                return Err(GatewayError::invalid_request());
            }
            return Ok(());
        }
        self.tool_calls.insert(
            index,
            ToolState {
                item_id: format!("azure_tool_{}", self.next_item),
                call_id: call_id.to_owned(),
                name: name.to_owned(),
            },
        );
        self.next_item += 1;
        Ok(())
    }

    fn emit_tool(
        &self,
        index: usize,
        arguments: &str,
        events: &mut Vec<CanonicalEvent>,
    ) -> Result<(), GatewayError> {
        if arguments.len() > self.limits.max_tool_argument_bytes {
            return Err(GatewayError::invalid_request());
        }
        let state = self
            .tool_calls
            .get(&index)
            .ok_or_else(GatewayError::invalid_request)?;
        push_event(
            events,
            CanonicalEvent::ToolCallDelta {
                item_id: state.item_id.clone(),
                call_id: state.call_id.clone(),
                name: state.name.clone(),
                delta: arguments.to_owned(),
            },
            self.limits,
        )
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
                    response_id: format!("azure_{}", self.request_id.as_str()),
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

fn validate_api_version(value: &str) -> Result<(), GatewayError> {
    let date = value.strip_suffix("-preview").unwrap_or(value);
    let mut segments = date.split('-');
    let (Some(year), Some(month), Some(day), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return Err(GatewayError::invalid_request());
    };
    let valid_digits = year.len() == 4
        && month.len() == 2
        && day.len() == 2
        && year.bytes().all(|byte| byte.is_ascii_digit())
        && month.bytes().all(|byte| byte.is_ascii_digit())
        && day.bytes().all(|byte| byte.is_ascii_digit());
    let valid_range = month
        .parse::<u8>()
        .is_ok_and(|month| (1..=12).contains(&month))
        && day.parse::<u8>().is_ok_and(|day| (1..=31).contains(&day));
    if valid_digits && valid_range {
        Ok(())
    } else {
        Err(GatewayError::invalid_request())
    }
}

fn reject_error(root: &Value) -> Result<(), GatewayError> {
    if root.get("error").is_some() {
        Err(GatewayError::upstream_response(502, "azure upstream error"))
    } else {
        Ok(())
    }
}

fn required_bounded_array<'a>(
    root: &'a Value,
    key: &str,
    limits: UpstreamLimits,
) -> Result<&'a [Value], GatewayError> {
    let value = root.get(key).ok_or_else(GatewayError::invalid_request)?;
    bounded_array(value, limits)
}

fn bounded_array(value: &Value, limits: UpstreamLimits) -> Result<&[Value], GatewayError> {
    let values = value.as_array().ok_or_else(GatewayError::invalid_request)?;
    if values.len() > limits.max_collection_items {
        return Err(GatewayError::invalid_request());
    }
    Ok(values)
}

#[derive(Deserialize)]
struct AzureUsageWire {
    #[serde(default, deserialize_with = "deserialize_present")]
    prompt_tokens: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_present")]
    completion_tokens: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_present")]
    prompt_tokens_details: Option<AzurePromptTokensDetailsWire>,
    #[serde(default, deserialize_with = "deserialize_present")]
    completion_tokens_details: Option<AzureCompletionTokensDetailsWire>,
    #[serde(flatten)]
    extensions: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct AzurePromptTokensDetailsWire {
    #[serde(default, deserialize_with = "deserialize_present")]
    cached_tokens: Option<u64>,
    #[serde(flatten)]
    extensions: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct AzureCompletionTokensDetailsWire {
    #[serde(default, deserialize_with = "deserialize_present")]
    reasoning_tokens: Option<u64>,
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

fn openai_usage(value: &Value, limits: UpstreamLimits) -> Result<Usage, GatewayError> {
    let wire: AzureUsageWire =
        serde_json::from_value(value.clone()).map_err(|_| GatewayError::invalid_request())?;
    if wire.extensions.len() > limits.max_collection_items
        || wire
            .prompt_tokens_details
            .as_ref()
            .is_some_and(|details| details.extensions.len() > limits.max_collection_items)
        || wire
            .completion_tokens_details
            .as_ref()
            .is_some_and(|details| details.extensions.len() > limits.max_collection_items)
    {
        return Err(GatewayError::invalid_request());
    }
    let mut extensions = wire.extensions;
    if let Some(details) = wire.prompt_tokens_details.as_ref()
        && !details.extensions.is_empty()
    {
        extensions.insert(
            "prompt_tokens_details".to_owned(),
            Value::Object(details.extensions.clone().into_iter().collect()),
        );
    }
    if let Some(details) = wire.completion_tokens_details.as_ref()
        && !details.extensions.is_empty()
    {
        extensions.insert(
            "completion_tokens_details".to_owned(),
            Value::Object(details.extensions.clone().into_iter().collect()),
        );
    }
    Ok(Usage {
        input_tokens: wire.prompt_tokens.unwrap_or(0),
        output_tokens: wire.completion_tokens.unwrap_or(0),
        cached_input_tokens: wire
            .prompt_tokens_details
            .and_then(|details| details.cached_tokens),
        reasoning_tokens: wire
            .completion_tokens_details
            .and_then(|details| details.reasoning_tokens),
        extensions,
    })
}
