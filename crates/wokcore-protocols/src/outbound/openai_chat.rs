use std::collections::BTreeMap;

use bytes::Bytes;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::{
    canonical::{CanonicalEvent, GatewayError, PublicModelId, Usage},
    stream::encode_sse,
    valid_chat_function_name,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatFinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    FunctionCall,
}

impl ChatFinishReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolCalls => "tool_calls",
            Self::ContentFilter => "content_filter",
            Self::FunctionCall => "function_call",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatEncodeContext {
    pub model: PublicModelId,
    pub created: u64,
    pub response: ChatResponseTemplate,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatResponseTemplate {
    pub choice_index: u64,
    pub finish_reason: ChatFinishReason,
    pub logprobs: Option<Value>,
    pub include_usage: bool,
    pub extra: BTreeMap<String, Value>,
}

// Keep non-stream aggregation within the protocol body's existing safety envelope.
const MAX_CHAT_AGGREGATE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CHAT_OUTPUT_ITEMS: usize = 4_096;
const MAX_CHAT_IDENTIFIER_BYTES: usize = 512;
const MAX_CHAT_RETAINED_VALUE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy)]
struct ChatLimits {
    max_output_items: usize,
    max_identifier_bytes: usize,
    max_aggregate_bytes: usize,
    max_value_bytes: usize,
}

impl Default for ChatLimits {
    fn default() -> Self {
        Self {
            max_output_items: MAX_CHAT_OUTPUT_ITEMS,
            max_identifier_bytes: MAX_CHAT_IDENTIFIER_BYTES,
            max_aggregate_bytes: MAX_CHAT_AGGREGATE_BYTES,
            max_value_bytes: MAX_CHAT_RETAINED_VALUE_BYTES,
        }
    }
}

pub struct ChatCodec {
    context: ChatEncodeContext,
    limits: ChatLimits,
    context_validated: bool,
    terminal: bool,
    response_id: Option<String>,
    outputs: OutputRegistry,
    usage: Option<Value>,
}

enum OutputIdentity {
    Text,
    Tool(usize),
}

struct ChatToolIdentity {
    call_id: String,
    name: String,
}

#[derive(Default)]
struct OutputRegistry {
    identities: BTreeMap<String, OutputIdentity>,
    tools: Vec<ChatToolIdentity>,
}

struct ChatResponseAggregator {
    context: ChatEncodeContext,
    limits: ChatLimits,
    context_validated: bool,
    terminal: bool,
    failed: Option<Value>,
    response_id: Option<String>,
    outputs: OutputRegistry,
    text: String,
    arguments: Vec<String>,
    aggregate_bytes: usize,
    usage: Option<Value>,
}

#[derive(Serialize)]
struct ChatCompletionWire<'a> {
    id: &'a str,
    object: &'static str,
    created: u64,
    model: &'a str,
    choices: Vec<Value>,
    usage: Value,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Serialize)]
struct ChatChunkWire<'a> {
    id: &'a str,
    object: &'static str,
    created: u64,
    model: &'a str,
    choices: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<Value>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Serialize)]
struct ChatErrorEnvelope<'a> {
    error: ChatErrorWire<'a>,
}

#[derive(Serialize)]
struct ChatErrorWire<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    code: &'static str,
    message: &'a str,
}

enum Transition {
    None,
    Chunks(Vec<Value>),
    Failed(Value),
}

impl ChatCodec {
    pub fn new(context: ChatEncodeContext) -> Self {
        Self::with_limits(context, ChatLimits::default())
    }

    fn with_limits(context: ChatEncodeContext, limits: ChatLimits) -> Self {
        Self {
            context,
            limits,
            context_validated: false,
            terminal: false,
            response_id: None,
            outputs: OutputRegistry::default(),
            usage: None,
        }
    }

    pub fn encode_response(
        context: ChatEncodeContext,
        events: &[CanonicalEvent],
    ) -> Result<Value, GatewayError> {
        Self::encode_response_with_limits(context, events, ChatLimits::default())
    }

    fn encode_response_with_limits(
        context: ChatEncodeContext,
        events: &[CanonicalEvent],
        limits: ChatLimits,
    ) -> Result<Value, GatewayError> {
        ChatResponseAggregator::with_limits(context, limits).encode(events)
    }

    pub fn encode_chunk(&mut self, event: &CanonicalEvent) -> Result<Bytes, GatewayError> {
        let transition = self.transition(event)?;
        let mut encoded = Vec::new();
        match transition {
            Transition::None => {}
            Transition::Chunks(chunks) => {
                for chunk in chunks {
                    encoded.extend_from_slice(&encode_sse(None, &chunk));
                }
            }
            Transition::Failed(error) => {
                encoded.extend_from_slice(&encode_sse(None, &error));
            }
        }
        if self.terminal {
            encoded.extend_from_slice(b"data: [DONE]\n\n");
        }
        Ok(Bytes::from(encoded))
    }

    fn transition(&mut self, event: &CanonicalEvent) -> Result<Transition, GatewayError> {
        if self.terminal {
            return Err(GatewayError::invalid_request());
        }
        self.validate_context_once()?;

        match event {
            CanonicalEvent::Created { response_id } => self.created(response_id),
            CanonicalEvent::OutputTextDelta { item_id, delta } => self.text_delta(item_id, delta),
            CanonicalEvent::ReasoningDelta { .. } => Err(GatewayError::unsupported_capability()),
            CanonicalEvent::ToolCallDelta {
                item_id,
                call_id,
                name,
                delta,
            } => self.tool_delta(item_id, call_id, name, delta),
            CanonicalEvent::Usage(usage) => self.usage(usage),
            CanonicalEvent::Completed => self.completed(),
            CanonicalEvent::Failed(error) => Ok(self.failed(error)),
        }
    }

    fn created(&mut self, response_id: &str) -> Result<Transition, GatewayError> {
        if self.response_id.is_some() {
            return Err(GatewayError::invalid_request());
        }
        self.limits.validate_identifier(response_id)?;
        self.response_id = Some(response_id.to_owned());
        let choice = self.choice(json!({"role": "assistant"}), Value::Null);
        Ok(Transition::Chunks(vec![
            self.chunk(vec![choice], Value::Null),
        ]))
    }

    fn text_delta(&mut self, item_id: &str, delta: &str) -> Result<Transition, GatewayError> {
        self.require_delta_allowed()?;
        self.outputs.register_text(item_id, self.limits)?;
        let choice = self.choice(json!({"content": delta}), Value::Null);
        Ok(Transition::Chunks(vec![
            self.chunk(vec![choice], Value::Null),
        ]))
    }

    fn tool_delta(
        &mut self,
        item_id: &str,
        call_id: &str,
        name: &str,
        delta: &str,
    ) -> Result<Transition, GatewayError> {
        self.require_delta_allowed()?;
        let (tool_index, is_new) =
            self.outputs
                .register_tool(item_id, call_id, name, self.limits)?;

        let tool_delta = if is_new {
            json!({
                "index": tool_index,
                "id": call_id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": delta,
                },
            })
        } else {
            json!({
                "index": tool_index,
                "function": {
                    "arguments": delta,
                },
            })
        };
        let choice = self.choice(json!({"tool_calls": [tool_delta]}), Value::Null);
        Ok(Transition::Chunks(vec![
            self.chunk(vec![choice], Value::Null),
        ]))
    }

    fn usage(&mut self, usage: &Usage) -> Result<Transition, GatewayError> {
        self.require_created()?;
        if self.usage.is_some() {
            return Err(GatewayError::invalid_request());
        }
        self.usage = Some(self.limits.validate_usage(usage)?);
        Ok(Transition::None)
    }

    fn completed(&mut self) -> Result<Transition, GatewayError> {
        self.require_created()?;
        let usage = self
            .usage
            .clone()
            .ok_or_else(GatewayError::invalid_request)?;

        let finish_reason = json!(self.context.response.finish_reason.as_str());
        let finish_choice = self.choice(json!({}), finish_reason);
        let mut chunks = vec![self.chunk(vec![finish_choice], Value::Null)];
        if self.context.response.include_usage {
            chunks.push(self.chunk(Vec::new(), usage));
        }
        self.terminal = true;
        Ok(Transition::Chunks(chunks))
    }

    fn failed(&mut self, error: &GatewayError) -> Transition {
        let envelope = error_envelope(error);
        self.terminal = true;
        Transition::Failed(envelope)
    }

    fn require_created(&self) -> Result<(), GatewayError> {
        if self.response_id.is_some() {
            Ok(())
        } else {
            Err(GatewayError::invalid_request())
        }
    }

    fn require_delta_allowed(&self) -> Result<(), GatewayError> {
        self.require_created()?;
        if self.usage.is_none() {
            Ok(())
        } else {
            Err(GatewayError::invalid_request())
        }
    }

    fn validate_context_once(&mut self) -> Result<(), GatewayError> {
        if !self.context_validated {
            self.limits.validate_context(&self.context)?;
            self.context_validated = true;
        }
        Ok(())
    }

    fn choice(&self, delta: Value, finish_reason: Value) -> Value {
        json!({
            "index": self.context.response.choice_index,
            "delta": delta,
            "logprobs": self.context.response.logprobs,
            "finish_reason": finish_reason,
        })
    }

    fn chunk(&self, choices: Vec<Value>, usage: Value) -> Value {
        serde_json::to_value(ChatChunkWire {
            id: self.response_id.as_deref().unwrap_or_default(),
            object: "chat.completion.chunk",
            created: self.context.created,
            model: self.context.model.as_str(),
            choices,
            usage: self.context.response.include_usage.then_some(usage),
            extra: filtered_extra(
                &self.context.response.extra,
                &["id", "object", "created", "model", "choices", "usage"],
            ),
        })
        .expect("serializing a Chat Completions chunk cannot fail")
    }
}

impl ChatResponseAggregator {
    fn with_limits(context: ChatEncodeContext, limits: ChatLimits) -> Self {
        Self {
            context,
            limits,
            context_validated: false,
            terminal: false,
            failed: None,
            response_id: None,
            outputs: OutputRegistry::default(),
            text: String::new(),
            arguments: Vec::new(),
            aggregate_bytes: 0,
            usage: None,
        }
    }

    fn encode(mut self, events: &[CanonicalEvent]) -> Result<Value, GatewayError> {
        for event in events {
            self.transition(event)?;
        }
        if !self.terminal {
            return Err(GatewayError::invalid_request());
        }
        if let Some(error) = self.failed {
            return Ok(error);
        }
        let usage = self
            .usage
            .clone()
            .ok_or_else(GatewayError::invalid_request)?;
        Ok(self.response_value(usage))
    }

    fn transition(&mut self, event: &CanonicalEvent) -> Result<(), GatewayError> {
        if self.terminal {
            return Err(GatewayError::invalid_request());
        }
        self.validate_context_once()?;

        match event {
            CanonicalEvent::Created { response_id } => self.created(response_id),
            CanonicalEvent::OutputTextDelta { item_id, delta } => self.text_delta(item_id, delta),
            CanonicalEvent::ReasoningDelta { .. } => Err(GatewayError::unsupported_capability()),
            CanonicalEvent::ToolCallDelta {
                item_id,
                call_id,
                name,
                delta,
            } => self.tool_delta(item_id, call_id, name, delta),
            CanonicalEvent::Usage(usage) => self.usage(usage),
            CanonicalEvent::Completed => self.completed(),
            CanonicalEvent::Failed(error) => {
                self.failed = Some(error_envelope(error));
                self.terminal = true;
                Ok(())
            }
        }
    }

    fn created(&mut self, response_id: &str) -> Result<(), GatewayError> {
        if self.response_id.is_some() {
            return Err(GatewayError::invalid_request());
        }
        self.limits.validate_identifier(response_id)?;
        self.response_id = Some(response_id.to_owned());
        Ok(())
    }

    fn text_delta(&mut self, item_id: &str, delta: &str) -> Result<(), GatewayError> {
        self.require_delta_allowed()?;
        self.ensure_payload_capacity(delta.len())?;
        self.outputs.register_text(item_id, self.limits)?;
        self.text.push_str(delta);
        self.aggregate_bytes += delta.len();
        Ok(())
    }

    fn tool_delta(
        &mut self,
        item_id: &str,
        call_id: &str,
        name: &str,
        delta: &str,
    ) -> Result<(), GatewayError> {
        self.require_delta_allowed()?;
        self.ensure_payload_capacity(delta.len())?;
        let (tool_index, is_new) =
            self.outputs
                .register_tool(item_id, call_id, name, self.limits)?;
        if is_new {
            if self.arguments.len() >= self.limits.max_output_items {
                return Err(GatewayError::invalid_request());
            }
            self.arguments.push(String::new());
        }
        self.arguments[tool_index].push_str(delta);
        self.aggregate_bytes += delta.len();
        Ok(())
    }

    fn usage(&mut self, usage: &Usage) -> Result<(), GatewayError> {
        self.require_created()?;
        if self.usage.is_some() {
            return Err(GatewayError::invalid_request());
        }
        self.usage = Some(self.limits.validate_usage(usage)?);
        Ok(())
    }

    fn completed(&mut self) -> Result<(), GatewayError> {
        self.require_created()?;
        if self.usage.is_none() {
            return Err(GatewayError::invalid_request());
        }
        self.terminal = true;
        Ok(())
    }

    fn require_created(&self) -> Result<(), GatewayError> {
        if self.response_id.is_some() {
            Ok(())
        } else {
            Err(GatewayError::invalid_request())
        }
    }

    fn require_delta_allowed(&self) -> Result<(), GatewayError> {
        self.require_created()?;
        if self.usage.is_none() {
            Ok(())
        } else {
            Err(GatewayError::invalid_request())
        }
    }

    fn validate_context_once(&mut self) -> Result<(), GatewayError> {
        if !self.context_validated {
            self.limits.validate_context(&self.context)?;
            self.context_validated = true;
        }
        Ok(())
    }

    fn ensure_payload_capacity(&self, additional: usize) -> Result<(), GatewayError> {
        if self
            .aggregate_bytes
            .checked_add(additional)
            .is_some_and(|total| total <= self.limits.max_aggregate_bytes)
        {
            Ok(())
        } else {
            Err(GatewayError::invalid_request())
        }
    }

    fn response_value(&self, usage: Value) -> Value {
        let mut message = Map::from_iter([
            ("role".to_owned(), json!("assistant")),
            (
                "content".to_owned(),
                if self.text.is_empty() {
                    Value::Null
                } else {
                    json!(self.text)
                },
            ),
        ]);
        if !self.outputs.tools.is_empty() {
            message.insert(
                "tool_calls".to_owned(),
                Value::Array(
                    self.outputs
                        .tools
                        .iter()
                        .zip(&self.arguments)
                        .map(|(tool, arguments)| {
                            json!({
                                "id": tool.call_id,
                                "type": "function",
                                "function": {
                                    "name": tool.name,
                                    "arguments": arguments,
                                },
                            })
                        })
                        .collect(),
                ),
            );
        }

        let choice = json!({
            "index": self.context.response.choice_index,
            "message": Value::Object(message),
            "logprobs": self.context.response.logprobs,
            "finish_reason": self.context.response.finish_reason.as_str(),
        });
        serde_json::to_value(ChatCompletionWire {
            id: self.response_id.as_deref().unwrap_or_default(),
            object: "chat.completion",
            created: self.context.created,
            model: self.context.model.as_str(),
            choices: vec![choice],
            usage,
            extra: filtered_extra(
                &self.context.response.extra,
                &["id", "object", "created", "model", "choices", "usage"],
            ),
        })
        .expect("serializing a Chat Completion cannot fail")
    }
}

impl OutputRegistry {
    fn register_text(&mut self, item_id: &str, limits: ChatLimits) -> Result<(), GatewayError> {
        limits.validate_identifier(item_id)?;
        match self.identities.get(item_id) {
            Some(OutputIdentity::Text) => Ok(()),
            Some(OutputIdentity::Tool(_)) => Err(GatewayError::invalid_request()),
            None => {
                if self.identities.len() >= limits.max_output_items {
                    return Err(GatewayError::invalid_request());
                }
                self.identities
                    .insert(item_id.to_owned(), OutputIdentity::Text);
                Ok(())
            }
        }
    }

    fn register_tool(
        &mut self,
        item_id: &str,
        call_id: &str,
        name: &str,
        limits: ChatLimits,
    ) -> Result<(usize, bool), GatewayError> {
        limits.validate_identifier(item_id)?;
        limits.validate_identifier(call_id)?;
        limits.validate_identifier(name)?;
        if !valid_chat_function_name(name) {
            return Err(GatewayError::invalid_request());
        }

        match self.identities.get(item_id) {
            Some(OutputIdentity::Text) => Err(GatewayError::invalid_request()),
            Some(OutputIdentity::Tool(tool_index)) => {
                let tool = &self.tools[*tool_index];
                if tool.call_id == call_id && tool.name == name {
                    Ok((*tool_index, false))
                } else {
                    Err(GatewayError::invalid_request())
                }
            }
            None => {
                if self.identities.len() >= limits.max_output_items
                    || self.tools.len() >= limits.max_output_items
                    || self.tools.iter().any(|tool| tool.call_id == call_id)
                {
                    return Err(GatewayError::invalid_request());
                }
                let tool_index = self.tools.len();
                self.tools.push(ChatToolIdentity {
                    call_id: call_id.to_owned(),
                    name: name.to_owned(),
                });
                self.identities
                    .insert(item_id.to_owned(), OutputIdentity::Tool(tool_index));
                Ok((tool_index, true))
            }
        }
    }
}

impl ChatLimits {
    fn validate_identifier(self, value: &str) -> Result<(), GatewayError> {
        if value.is_empty() || value.len() > self.max_identifier_bytes {
            Err(GatewayError::invalid_request())
        } else {
            Ok(())
        }
    }

    fn validate_context(self, context: &ChatEncodeContext) -> Result<(), GatewayError> {
        self.validate_identifier(context.model.as_str())?;
        self.validate_string_map(&context.response.extra)?;
        self.validate_serialized(&context.response.logprobs)
    }

    fn validate_usage(self, usage: &Usage) -> Result<Value, GatewayError> {
        self.validate_string_map(&usage.extensions)?;
        let value = usage_value(usage);
        self.validate_serialized(&value)?;
        Ok(value)
    }

    fn validate_string_map(self, values: &BTreeMap<String, Value>) -> Result<(), GatewayError> {
        if values.len() > self.max_output_items {
            return Err(GatewayError::invalid_request());
        }
        for key in values.keys() {
            self.validate_identifier(key)?;
        }
        self.validate_serialized(values)
    }

    fn validate_serialized<T: Serialize>(self, value: &T) -> Result<(), GatewayError> {
        if serde_json::to_vec(value).is_ok_and(|encoded| encoded.len() <= self.max_value_bytes) {
            Ok(())
        } else {
            Err(GatewayError::invalid_request())
        }
    }
}

fn usage_value(usage: &Usage) -> Value {
    let mut value = Map::from_iter([
        ("prompt_tokens".to_owned(), json!(usage.input_tokens)),
        ("completion_tokens".to_owned(), json!(usage.output_tokens)),
        (
            "total_tokens".to_owned(),
            json!(usage.input_tokens.saturating_add(usage.output_tokens)),
        ),
        (
            "prompt_tokens_details".to_owned(),
            usage_details(
                usage,
                "prompt_tokens_details",
                "cached_tokens",
                usage.cached_input_tokens.unwrap_or(0),
            ),
        ),
        (
            "completion_tokens_details".to_owned(),
            usage_details(
                usage,
                "completion_tokens_details",
                "reasoning_tokens",
                usage.reasoning_tokens.unwrap_or(0),
            ),
        ),
    ]);
    for (key, extension) in &usage.extensions {
        value
            .entry(key.clone())
            .or_insert_with(|| extension.clone());
    }
    Value::Object(value)
}

fn usage_details(
    usage: &Usage,
    extension_key: &str,
    canonical_key: &str,
    canonical_value: u64,
) -> Value {
    let mut details = usage
        .extensions
        .get(extension_key)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    details.insert(canonical_key.to_owned(), json!(canonical_value));
    Value::Object(details)
}

fn error_envelope(error: &GatewayError) -> Value {
    serde_json::to_value(ChatErrorEnvelope {
        error: ChatErrorWire {
            kind: "gateway_error",
            code: error.code(),
            message: error.public_message(),
        },
    })
    .expect("serializing a Chat Completions error cannot fail")
}

fn filtered_extra(extra: &BTreeMap<String, Value>, reserved: &[&str]) -> BTreeMap<String, Value> {
    extra
        .iter()
        .filter(|(key, _)| !reserved.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> ChatEncodeContext {
        ChatEncodeContext {
            model: PublicModelId::new("model"),
            created: 1,
            response: ChatResponseTemplate {
                choice_index: 0,
                finish_reason: ChatFinishReason::Stop,
                logprobs: None,
                include_usage: true,
                extra: BTreeMap::new(),
            },
        }
    }

    fn tiny_limits() -> ChatLimits {
        ChatLimits {
            max_output_items: 2,
            max_identifier_bytes: 8,
            max_aggregate_bytes: 8,
            max_value_bytes: 256,
        }
    }

    fn created() -> CanonicalEvent {
        CanonicalEvent::Created {
            response_id: "resp".to_owned(),
        }
    }

    #[test]
    fn bounded_stream_retains_no_accumulated_text_or_arguments() {
        let mut codec = ChatCodec::with_limits(context(), tiny_limits());
        codec.encode_chunk(&created()).unwrap();

        for _ in 0..3 {
            codec
                .encode_chunk(&CanonicalEvent::OutputTextDelta {
                    item_id: "text".to_owned(),
                    delta: "1234".to_owned(),
                })
                .unwrap();
            codec
                .encode_chunk(&CanonicalEvent::ToolCallDelta {
                    item_id: "tool".to_owned(),
                    call_id: "call".to_owned(),
                    name: "tool".to_owned(),
                    delta: "1234".to_owned(),
                })
                .unwrap();
        }

        // Exact destructuring makes retained stream state a compile-checked allowlist.
        let ChatCodec {
            context: _,
            limits: _,
            context_validated: _,
            terminal: _,
            response_id: _,
            outputs,
            usage: _,
        } = codec;
        assert_eq!(outputs.identities.len(), 2);
        assert_eq!(outputs.tools.len(), 1);
    }

    #[test]
    fn bounded_stream_rejects_identifier_collection_and_value_overflow() {
        let mut response_id = ChatCodec::with_limits(context(), tiny_limits());
        assert_eq!(
            response_id
                .encode_chunk(&CanonicalEvent::Created {
                    response_id: "123456789".to_owned(),
                })
                .unwrap_err(),
            GatewayError::invalid_request()
        );

        let mut identifiers = ChatCodec::with_limits(context(), tiny_limits());
        identifiers.encode_chunk(&created()).unwrap();
        assert_eq!(
            identifiers
                .encode_chunk(&CanonicalEvent::OutputTextDelta {
                    item_id: "123456789".to_owned(),
                    delta: String::new(),
                })
                .unwrap_err(),
            GatewayError::invalid_request()
        );
        assert_eq!(
            identifiers
                .encode_chunk(&CanonicalEvent::ToolCallDelta {
                    item_id: "tool".to_owned(),
                    call_id: "123456789".to_owned(),
                    name: "tool".to_owned(),
                    delta: String::new(),
                })
                .unwrap_err(),
            GatewayError::invalid_request()
        );

        for item_id in ["one", "two"] {
            identifiers
                .encode_chunk(&CanonicalEvent::OutputTextDelta {
                    item_id: item_id.to_owned(),
                    delta: String::new(),
                })
                .unwrap();
        }
        assert_eq!(
            identifiers
                .encode_chunk(&CanonicalEvent::OutputTextDelta {
                    item_id: "three".to_owned(),
                    delta: String::new(),
                })
                .unwrap_err(),
            GatewayError::invalid_request()
        );

        let mut usage = ChatCodec::with_limits(context(), tiny_limits());
        usage.encode_chunk(&created()).unwrap();
        assert_eq!(
            usage
                .encode_chunk(&CanonicalEvent::Usage(Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                    extensions: BTreeMap::from([("large".to_owned(), json!("x".repeat(300)))]),
                }))
                .unwrap_err(),
            GatewayError::invalid_request()
        );

        let mut oversized_context = context();
        oversized_context.response.extra = BTreeMap::from([
            ("one".to_owned(), Value::Null),
            ("two".to_owned(), Value::Null),
            ("three".to_owned(), Value::Null),
        ]);
        assert_eq!(
            ChatCodec::with_limits(oversized_context, tiny_limits())
                .encode_chunk(&created())
                .unwrap_err(),
            GatewayError::invalid_request()
        );
    }

    #[test]
    fn bounded_non_stream_rejects_payload_and_item_overflow() {
        let payload_events = [
            created(),
            CanonicalEvent::OutputTextDelta {
                item_id: "text".to_owned(),
                delta: "12345".to_owned(),
            },
            CanonicalEvent::OutputTextDelta {
                item_id: "text".to_owned(),
                delta: "6789".to_owned(),
            },
        ];
        assert_eq!(
            ChatCodec::encode_response_with_limits(context(), &payload_events, tiny_limits())
                .unwrap_err(),
            GatewayError::invalid_request()
        );

        let item_events = [
            created(),
            CanonicalEvent::OutputTextDelta {
                item_id: "one".to_owned(),
                delta: String::new(),
            },
            CanonicalEvent::OutputTextDelta {
                item_id: "two".to_owned(),
                delta: String::new(),
            },
            CanonicalEvent::OutputTextDelta {
                item_id: "three".to_owned(),
                delta: String::new(),
            },
        ];
        assert_eq!(
            ChatCodec::encode_response_with_limits(context(), &item_events, tiny_limits())
                .unwrap_err(),
            GatewayError::invalid_request()
        );
    }
}
