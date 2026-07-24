use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::{
    ANTHROPIC_KNOWN_BLOCKS_EXTENSION_KEY,
    canonical::{CanonicalEvent, CanonicalRequest, GatewayError, PublicModelId, RequestId, Usage},
    inbound::anthropic::REQUEST_EXTENSION_KEY,
    stream::encode_sse,
};

const MAX_OUTPUT_ITEMS: usize = 4_096;
const MAX_IDENTIFIER_BYTES: usize = 512;
const MAX_OPAQUE_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_RETAINED_VALUE_BYTES: usize = 1024 * 1024;
const MAX_AGGREGATE_BYTES: usize = 16 * 1024 * 1024;
const MAX_MESSAGE_REQUEST_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnthropicStopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    PauseTurn,
    Refusal,
    ModelContextWindowExceeded,
}

impl AnthropicStopReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::EndTurn => "end_turn",
            Self::MaxTokens => "max_tokens",
            Self::StopSequence => "stop_sequence",
            Self::ToolUse => "tool_use",
            Self::PauseTurn => "pause_turn",
            Self::Refusal => "refusal",
            Self::ModelContextWindowExceeded => "model_context_window_exceeded",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnthropicEncodeContext {
    pub request_id: RequestId,
    pub model: PublicModelId,
    pub initial_usage: Usage,
    pub response: AnthropicResponseTemplate,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnthropicResponseTemplate {
    pub stop_reason: AnthropicStopReason,
    pub stop_sequence: Option<String>,
    pub thinking_signatures: BTreeMap<String, String>,
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AnthropicTokenCount {
    pub input_tokens: u64,
}

pub trait TokenCounter {
    fn count_tokens(&self, request: &CanonicalRequest) -> Result<u64, GatewayError>;
}

#[derive(Clone, Copy)]
struct AnthropicLimits {
    max_output_items: usize,
    max_identifier_bytes: usize,
    max_opaque_payload_bytes: usize,
    max_value_bytes: usize,
    max_aggregate_bytes: usize,
}

impl Default for AnthropicLimits {
    fn default() -> Self {
        Self {
            max_output_items: MAX_OUTPUT_ITEMS,
            max_identifier_bytes: MAX_IDENTIFIER_BYTES,
            max_opaque_payload_bytes: MAX_OPAQUE_PAYLOAD_BYTES,
            max_value_bytes: MAX_RETAINED_VALUE_BYTES,
            max_aggregate_bytes: MAX_AGGREGATE_BYTES,
        }
    }
}

pub struct AnthropicCodec {
    context: AnthropicEncodeContext,
    limits: AnthropicLimits,
    context_validated: bool,
    terminal: bool,
    response_id: Option<String>,
    active: Option<ActiveBlock>,
    seen_items: BTreeSet<String>,
    call_ids: BTreeSet<String>,
    next_index: usize,
    usage: Option<Usage>,
}

enum ActiveBlock {
    Text {
        item_id: String,
        index: usize,
    },
    Reasoning {
        item_id: String,
        index: usize,
    },
    Tool {
        item_id: String,
        call_id: String,
        name: String,
        index: usize,
    },
}

struct AnthropicResponseAggregator {
    context: AnthropicEncodeContext,
    limits: AnthropicLimits,
    terminal: bool,
    failed: Option<Value>,
    response_id: Option<String>,
    outputs: Vec<AggregateOutput>,
    seen_items: BTreeSet<String>,
    call_ids: BTreeSet<String>,
    aggregate_bytes: usize,
    usage: Option<Usage>,
}

enum AggregateOutput {
    Text {
        item_id: String,
        text: String,
    },
    Reasoning {
        item_id: String,
        thinking: String,
    },
    Tool {
        item_id: String,
        call_id: String,
        name: String,
        partial_json: String,
    },
}

impl ActiveBlock {
    fn index(&self) -> usize {
        match self {
            Self::Text { index, .. } | Self::Reasoning { index, .. } | Self::Tool { index, .. } => {
                *index
            }
        }
    }
}

impl AnthropicCodec {
    pub fn new(context: AnthropicEncodeContext) -> Self {
        Self::with_limits(context, AnthropicLimits::default())
    }

    fn with_limits(context: AnthropicEncodeContext, limits: AnthropicLimits) -> Self {
        Self {
            context,
            limits,
            context_validated: false,
            terminal: false,
            response_id: None,
            active: None,
            seen_items: BTreeSet::new(),
            call_ids: BTreeSet::new(),
            next_index: 0,
            usage: None,
        }
    }

    pub fn encode_message(request: &CanonicalRequest) -> Result<Value, GatewayError> {
        Self::encode_message_with_limit(request, MAX_MESSAGE_REQUEST_BYTES)
    }

    fn encode_message_with_limit(
        request: &CanonicalRequest,
        max_bytes: usize,
    ) -> Result<Value, GatewayError> {
        let retained = request
            .extensions
            .get(REQUEST_EXTENSION_KEY)
            .cloned()
            .ok_or_else(GatewayError::unsupported_capability)?;
        let encoded = serde_json::to_vec(&retained).map_err(|_| GatewayError::invalid_request())?;
        if encoded.len() > max_bytes {
            return Err(GatewayError::invalid_request());
        }
        let validated = Self::decode_message(request.request_id.clone(), &encoded)?;
        if validated.model != request.model
            || validated.input != request.input
            || validated.tools != request.tools
            || validated.stream != request.stream
            || validated.reasoning != request.reasoning
            || validated
                .extensions
                .get(ANTHROPIC_KNOWN_BLOCKS_EXTENSION_KEY)
                != request.extensions.get(ANTHROPIC_KNOWN_BLOCKS_EXTENSION_KEY)
        {
            return Err(GatewayError::unsupported_capability());
        }
        Ok(retained)
    }

    pub fn encode_response(
        context: AnthropicEncodeContext,
        events: &[CanonicalEvent],
    ) -> Result<Value, GatewayError> {
        Self::encode_response_with_limits(context, events, AnthropicLimits::default())
    }

    fn encode_response_with_limits(
        context: AnthropicEncodeContext,
        events: &[CanonicalEvent],
        limits: AnthropicLimits,
    ) -> Result<Value, GatewayError> {
        AnthropicResponseAggregator::new(context, limits).encode(events)
    }

    pub fn count_tokens_input(
        request_id: RequestId,
        json: &[u8],
        counter: &dyn TokenCounter,
    ) -> Result<AnthropicTokenCount, GatewayError> {
        let request = Self::decode_count_tokens(request_id, json)?;
        counter
            .count_tokens(&request)
            .map(|input_tokens| AnthropicTokenCount { input_tokens })
    }

    pub fn encode_event(&mut self, event: &CanonicalEvent) -> Result<Bytes, GatewayError> {
        if self.terminal {
            return Err(GatewayError::invalid_request());
        }
        self.validate_context_once()?;

        let wires = match event {
            CanonicalEvent::Created { response_id } => self.created(response_id)?,
            CanonicalEvent::OutputTextDelta { item_id, delta } => {
                self.text_delta(item_id, delta)?
            }
            CanonicalEvent::ReasoningDelta { item_id, delta } => {
                self.reasoning_delta(item_id, delta)?
            }
            CanonicalEvent::ToolCallDelta {
                item_id,
                call_id,
                name,
                delta,
            } => self.tool_delta(item_id, call_id, name, delta)?,
            CanonicalEvent::Usage(usage) => self.usage(usage)?,
            CanonicalEvent::Completed => self.completed()?,
            CanonicalEvent::Failed(error) => self.failed(error),
        };
        let mut encoded = Vec::new();
        for (kind, wire) in wires {
            encoded.extend_from_slice(&encode_sse(Some(kind), &wire));
        }
        Ok(Bytes::from(encoded))
    }

    fn created(&mut self, response_id: &str) -> Result<Vec<WireEvent>, GatewayError> {
        if self.response_id.is_some() {
            return Err(GatewayError::invalid_request());
        }
        self.limits.validate_identifier(response_id)?;
        self.response_id = Some(response_id.to_owned());
        let mut message = Map::from_iter([
            ("id".to_owned(), json!(response_id)),
            ("type".to_owned(), json!("message")),
            ("role".to_owned(), json!("assistant")),
            ("content".to_owned(), json!([])),
            ("model".to_owned(), json!(self.context.model.as_str())),
            ("stop_reason".to_owned(), Value::Null),
            ("stop_sequence".to_owned(), Value::Null),
            (
                "usage".to_owned(),
                initial_usage_value(&self.context.initial_usage),
            ),
        ]);
        for (key, value) in &self.context.response.extra {
            message.entry(key.clone()).or_insert_with(|| value.clone());
        }
        Ok(vec![wire(
            "message_start",
            [("message", Value::Object(message))],
        )])
    }

    fn text_delta(&mut self, item_id: &str, delta: &str) -> Result<Vec<WireEvent>, GatewayError> {
        self.require_delta_allowed()?;
        self.limits.validate_value(&json!(delta))?;
        let mut wires = self.activate_text(item_id)?;
        let index = self.active.as_ref().expect("active block").index();
        wires.push(wire(
            "content_block_delta",
            [
                ("index", json!(index)),
                ("delta", json!({"type": "text_delta", "text": delta})),
            ],
        ));
        Ok(wires)
    }

    fn reasoning_delta(
        &mut self,
        item_id: &str,
        delta: &str,
    ) -> Result<Vec<WireEvent>, GatewayError> {
        self.require_delta_allowed()?;
        self.limits.validate_value(&json!(delta))?;
        let mut wires = self.activate_reasoning(item_id)?;
        let index = self.active.as_ref().expect("active block").index();
        wires.push(wire(
            "content_block_delta",
            [
                ("index", json!(index)),
                (
                    "delta",
                    json!({"type": "thinking_delta", "thinking": delta}),
                ),
            ],
        ));
        Ok(wires)
    }

    fn tool_delta(
        &mut self,
        item_id: &str,
        call_id: &str,
        name: &str,
        delta: &str,
    ) -> Result<Vec<WireEvent>, GatewayError> {
        self.require_delta_allowed()?;
        self.limits.validate_value(&json!(delta))?;
        let mut wires = self.activate_tool(item_id, call_id, name)?;
        let index = self.active.as_ref().expect("active block").index();
        wires.push(wire(
            "content_block_delta",
            [
                ("index", json!(index)),
                (
                    "delta",
                    json!({"type": "input_json_delta", "partial_json": delta}),
                ),
            ],
        ));
        Ok(wires)
    }

    fn usage(&mut self, usage: &Usage) -> Result<Vec<WireEvent>, GatewayError> {
        self.require_created()?;
        if self.usage.is_some() {
            return Err(GatewayError::invalid_request());
        }
        self.limits.validate_value(&usage_value(usage))?;
        let mut wires = self.close_active()?;
        wires.push(wire(
            "message_delta",
            [
                (
                    "delta",
                    json!({
                        "stop_reason": self.context.response.stop_reason.as_str(),
                        "stop_sequence": self.context.response.stop_sequence,
                    }),
                ),
                ("usage", final_usage_value(usage)),
            ],
        ));
        self.usage = Some(usage.clone());
        Ok(wires)
    }

    fn completed(&mut self) -> Result<Vec<WireEvent>, GatewayError> {
        self.require_created()?;
        if self.usage.is_none() || self.active.is_some() {
            return Err(GatewayError::invalid_request());
        }
        self.terminal = true;
        Ok(vec![wire("message_stop", [])])
    }

    fn failed(&mut self, error: &GatewayError) -> Vec<WireEvent> {
        self.terminal = true;
        vec![wire(
            "error",
            [(
                "error",
                json!({
                    "type": anthropic_error_type(error),
                    "message": error.public_message(),
                }),
            )],
        )]
    }

    fn activate_text(&mut self, item_id: &str) -> Result<Vec<WireEvent>, GatewayError> {
        if matches!(&self.active, Some(ActiveBlock::Text { item_id: active, .. }) if active == item_id)
        {
            return Ok(Vec::new());
        }
        let mut wires = self.close_active()?;
        let index = self.register_item(item_id)?;
        self.active = Some(ActiveBlock::Text {
            item_id: item_id.to_owned(),
            index,
        });
        wires.push(wire(
            "content_block_start",
            [
                ("index", json!(index)),
                ("content_block", json!({"type": "text", "text": ""})),
            ],
        ));
        Ok(wires)
    }

    fn activate_reasoning(&mut self, item_id: &str) -> Result<Vec<WireEvent>, GatewayError> {
        if matches!(&self.active, Some(ActiveBlock::Reasoning { item_id: active, .. }) if active == item_id)
        {
            return Ok(Vec::new());
        }
        let signature = self
            .context
            .response
            .thinking_signatures
            .get(item_id)
            .ok_or_else(GatewayError::unsupported_capability)?;
        self.limits.validate_opaque_payload(signature)?;
        let mut wires = self.close_active()?;
        let index = self.register_item(item_id)?;
        self.active = Some(ActiveBlock::Reasoning {
            item_id: item_id.to_owned(),
            index,
        });
        wires.push(wire(
            "content_block_start",
            [
                ("index", json!(index)),
                (
                    "content_block",
                    json!({"type": "thinking", "thinking": "", "signature": ""}),
                ),
            ],
        ));
        Ok(wires)
    }

    fn activate_tool(
        &mut self,
        item_id: &str,
        call_id: &str,
        name: &str,
    ) -> Result<Vec<WireEvent>, GatewayError> {
        if let Some(ActiveBlock::Tool {
            item_id: active_item,
            call_id: active_call,
            name: active_name,
            ..
        }) = &self.active
            && active_item == item_id
            && active_call == call_id
            && active_name == name
        {
            return Ok(Vec::new());
        }
        self.limits.validate_identifier(call_id)?;
        self.limits.validate_identifier(name)?;
        let mut wires = self.close_active()?;
        if self.call_ids.contains(call_id) {
            return Err(GatewayError::invalid_request());
        }
        let index = self.register_item(item_id)?;
        self.call_ids.insert(call_id.to_owned());
        self.active = Some(ActiveBlock::Tool {
            item_id: item_id.to_owned(),
            call_id: call_id.to_owned(),
            name: name.to_owned(),
            index,
        });
        wires.push(wire(
            "content_block_start",
            [
                ("index", json!(index)),
                (
                    "content_block",
                    json!({"type": "tool_use", "id": call_id, "name": name, "input": {}}),
                ),
            ],
        ));
        Ok(wires)
    }

    fn close_active(&mut self) -> Result<Vec<WireEvent>, GatewayError> {
        let Some(active) = self.active.take() else {
            return Ok(Vec::new());
        };
        let index = active.index();
        let mut wires = Vec::new();
        if let ActiveBlock::Reasoning { item_id, .. } = &active {
            let signature = self
                .context
                .response
                .thinking_signatures
                .get(item_id)
                .ok_or_else(GatewayError::unsupported_capability)?;
            wires.push(wire(
                "content_block_delta",
                [
                    ("index", json!(index)),
                    (
                        "delta",
                        json!({"type": "signature_delta", "signature": signature}),
                    ),
                ],
            ));
        }
        wires.push(wire("content_block_stop", [("index", json!(index))]));
        Ok(wires)
    }

    fn register_item(&mut self, item_id: &str) -> Result<usize, GatewayError> {
        self.limits.validate_identifier(item_id)?;
        if self.seen_items.len() >= self.limits.max_output_items
            || !self.seen_items.insert(item_id.to_owned())
        {
            return Err(GatewayError::invalid_request());
        }
        let index = self.next_index;
        self.next_index += 1;
        Ok(index)
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
        if self.context_validated {
            return Ok(());
        }
        self.limits
            .validate_identifier(self.context.model.as_str())?;
        self.limits
            .validate_identifier(self.context.request_id.as_str())?;
        if let Some(stop_sequence) = &self.context.response.stop_sequence {
            self.limits.validate_identifier(stop_sequence)?;
        }
        self.limits
            .validate_value(&initial_usage_value(&self.context.initial_usage))?;
        self.limits.validate_value(&Value::Object(
            self.context.response.extra.clone().into_iter().collect(),
        ))?;
        if self.context.response.thinking_signatures.len() > self.limits.max_output_items {
            return Err(GatewayError::invalid_request());
        }
        for (item_id, signature) in &self.context.response.thinking_signatures {
            self.limits.validate_identifier(item_id)?;
            self.limits.validate_opaque_payload(signature)?;
        }
        self.context_validated = true;
        Ok(())
    }
}

impl AnthropicResponseAggregator {
    fn new(context: AnthropicEncodeContext, limits: AnthropicLimits) -> Self {
        Self {
            context,
            limits,
            terminal: false,
            failed: None,
            response_id: None,
            outputs: Vec::new(),
            seen_items: BTreeSet::new(),
            call_ids: BTreeSet::new(),
            aggregate_bytes: 0,
            usage: None,
        }
    }

    fn encode(mut self, events: &[CanonicalEvent]) -> Result<Value, GatewayError> {
        self.validate_context()?;
        for event in events {
            self.transition(event)?;
        }
        if !self.terminal {
            return Err(GatewayError::invalid_request());
        }
        if let Some(error) = self.failed {
            return Ok(error);
        }
        self.response_value()
    }

    fn transition(&mut self, event: &CanonicalEvent) -> Result<(), GatewayError> {
        if self.terminal {
            return Err(GatewayError::invalid_request());
        }
        match event {
            CanonicalEvent::Created { response_id } => self.created(response_id),
            CanonicalEvent::OutputTextDelta { item_id, delta } => self.append_text(item_id, delta),
            CanonicalEvent::ReasoningDelta { item_id, delta } => {
                self.append_reasoning(item_id, delta)
            }
            CanonicalEvent::ToolCallDelta {
                item_id,
                call_id,
                name,
                delta,
            } => self.append_tool(item_id, call_id, name, delta),
            CanonicalEvent::Usage(usage) => self.set_usage(usage),
            CanonicalEvent::Completed => self.completed(),
            CanonicalEvent::Failed(error) => {
                self.failed = Some(error_value_with_request_id(error, &self.context.request_id));
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

    fn append_text(&mut self, item_id: &str, delta: &str) -> Result<(), GatewayError> {
        self.require_delta_allowed()?;
        self.ensure_payload_capacity(delta.len())?;
        match self.outputs.last_mut() {
            Some(AggregateOutput::Text {
                item_id: active,
                text,
            }) if active == item_id => text.push_str(delta),
            _ => {
                self.register_item(item_id)?;
                self.outputs.push(AggregateOutput::Text {
                    item_id: item_id.to_owned(),
                    text: delta.to_owned(),
                });
            }
        }
        self.aggregate_bytes += delta.len();
        Ok(())
    }

    fn append_reasoning(&mut self, item_id: &str, delta: &str) -> Result<(), GatewayError> {
        self.require_delta_allowed()?;
        self.ensure_payload_capacity(delta.len())?;
        if !self
            .context
            .response
            .thinking_signatures
            .contains_key(item_id)
        {
            return Err(GatewayError::unsupported_capability());
        }
        match self.outputs.last_mut() {
            Some(AggregateOutput::Reasoning {
                item_id: active,
                thinking,
            }) if active == item_id => thinking.push_str(delta),
            _ => {
                self.register_item(item_id)?;
                self.outputs.push(AggregateOutput::Reasoning {
                    item_id: item_id.to_owned(),
                    thinking: delta.to_owned(),
                });
            }
        }
        self.aggregate_bytes += delta.len();
        Ok(())
    }

    fn append_tool(
        &mut self,
        item_id: &str,
        call_id: &str,
        name: &str,
        delta: &str,
    ) -> Result<(), GatewayError> {
        self.require_delta_allowed()?;
        self.ensure_payload_capacity(delta.len())?;
        self.limits.validate_identifier(call_id)?;
        self.limits.validate_identifier(name)?;
        match self.outputs.last_mut() {
            Some(AggregateOutput::Tool {
                item_id: active_item,
                call_id: active_call,
                name: active_name,
                partial_json,
            }) if active_item == item_id && active_call == call_id && active_name == name => {
                partial_json.push_str(delta);
            }
            _ => {
                self.register_item(item_id)?;
                if !self.call_ids.insert(call_id.to_owned()) {
                    return Err(GatewayError::invalid_request());
                }
                self.outputs.push(AggregateOutput::Tool {
                    item_id: item_id.to_owned(),
                    call_id: call_id.to_owned(),
                    name: name.to_owned(),
                    partial_json: delta.to_owned(),
                });
            }
        }
        self.aggregate_bytes += delta.len();
        Ok(())
    }

    fn set_usage(&mut self, usage: &Usage) -> Result<(), GatewayError> {
        self.require_created()?;
        if self.usage.is_some() {
            return Err(GatewayError::invalid_request());
        }
        self.limits.validate_value(&usage_value(usage))?;
        self.usage = Some(usage.clone());
        Ok(())
    }

    fn completed(&mut self) -> Result<(), GatewayError> {
        self.require_created()?;
        if self.usage.is_none() || self.outputs.is_empty() {
            return Err(GatewayError::invalid_request());
        }
        self.terminal = true;
        Ok(())
    }

    fn response_value(&self) -> Result<Value, GatewayError> {
        let response_id = self
            .response_id
            .as_deref()
            .ok_or_else(GatewayError::invalid_request)?;
        let usage = self
            .usage
            .as_ref()
            .ok_or_else(GatewayError::invalid_request)?;
        let content = self
            .outputs
            .iter()
            .map(|output| match output {
                AggregateOutput::Text { text, .. } => Ok(json!({
                    "type": "text",
                    "text": text,
                })),
                AggregateOutput::Reasoning { item_id, thinking } => {
                    let signature = self
                        .context
                        .response
                        .thinking_signatures
                        .get(item_id)
                        .ok_or_else(GatewayError::unsupported_capability)?;
                    Ok(json!({
                        "type": "thinking",
                        "thinking": thinking,
                        "signature": signature,
                    }))
                }
                AggregateOutput::Tool {
                    call_id,
                    name,
                    partial_json,
                    ..
                } => {
                    let input: Value = serde_json::from_str(partial_json)
                        .map_err(|_| GatewayError::invalid_request())?;
                    if !input.is_object() {
                        return Err(GatewayError::invalid_request());
                    }
                    Ok(json!({
                        "type": "tool_use",
                        "id": call_id,
                        "name": name,
                        "input": input,
                    }))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut response = Map::from_iter([
            ("id".to_owned(), json!(response_id)),
            ("type".to_owned(), json!("message")),
            ("role".to_owned(), json!("assistant")),
            ("content".to_owned(), Value::Array(content)),
            ("model".to_owned(), json!(self.context.model.as_str())),
            (
                "stop_reason".to_owned(),
                json!(self.context.response.stop_reason.as_str()),
            ),
            (
                "stop_sequence".to_owned(),
                json!(self.context.response.stop_sequence),
            ),
            ("usage".to_owned(), usage_value(usage)),
        ]);
        for (key, value) in &self.context.response.extra {
            response.entry(key.clone()).or_insert_with(|| value.clone());
        }
        Ok(Value::Object(response))
    }

    fn register_item(&mut self, item_id: &str) -> Result<(), GatewayError> {
        self.limits.validate_identifier(item_id)?;
        if self.seen_items.len() >= self.limits.max_output_items
            || !self.seen_items.insert(item_id.to_owned())
        {
            return Err(GatewayError::invalid_request());
        }
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

    fn validate_context(&self) -> Result<(), GatewayError> {
        self.limits
            .validate_identifier(self.context.model.as_str())?;
        self.limits
            .validate_identifier(self.context.request_id.as_str())?;
        if let Some(stop_sequence) = &self.context.response.stop_sequence {
            self.limits.validate_identifier(stop_sequence)?;
        }
        self.limits
            .validate_value(&initial_usage_value(&self.context.initial_usage))?;
        self.limits.validate_value(&Value::Object(
            self.context.response.extra.clone().into_iter().collect(),
        ))?;
        if self.context.response.thinking_signatures.len() > self.limits.max_output_items {
            return Err(GatewayError::invalid_request());
        }
        for (item_id, signature) in &self.context.response.thinking_signatures {
            self.limits.validate_identifier(item_id)?;
            self.limits.validate_opaque_payload(signature)?;
        }
        Ok(())
    }
}

impl AnthropicLimits {
    fn validate_identifier(&self, value: &str) -> Result<(), GatewayError> {
        if value.is_empty() || value.len() > self.max_identifier_bytes {
            Err(GatewayError::invalid_request())
        } else {
            Ok(())
        }
    }

    fn validate_value(&self, value: &Value) -> Result<(), GatewayError> {
        if serde_json::to_vec(value).is_ok_and(|encoded| encoded.len() <= self.max_value_bytes) {
            Ok(())
        } else {
            Err(GatewayError::invalid_request())
        }
    }

    fn validate_opaque_payload(&self, value: &str) -> Result<(), GatewayError> {
        if value.is_empty() || value.len() > self.max_opaque_payload_bytes {
            Err(GatewayError::invalid_request())
        } else {
            Ok(())
        }
    }
}

type WireEvent = (&'static str, Value);

fn wire<const N: usize>(kind: &'static str, fields: [(&'static str, Value); N]) -> WireEvent {
    let mut value = Map::from_iter([("type".to_owned(), json!(kind))]);
    value.extend(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value)),
    );
    (kind, Value::Object(value))
}

fn initial_usage_value(usage: &Usage) -> Value {
    let mut value = usage_value(usage);
    if let Value::Object(fields) = &mut value {
        fields.insert("output_tokens".to_owned(), json!(usage.output_tokens));
    }
    value
}

fn final_usage_value(usage: &Usage) -> Value {
    let mut value = Map::from_iter([("output_tokens".to_owned(), json!(usage.output_tokens))]);
    if let Some(reasoning_tokens) = usage.reasoning_tokens {
        value.insert(
            "output_tokens_details".to_owned(),
            json!({"thinking_tokens": reasoning_tokens}),
        );
    }
    for (key, extension) in &usage.extensions {
        if key.starts_with("output_") {
            value
                .entry(key.clone())
                .or_insert_with(|| extension.clone());
        }
    }
    Value::Object(value)
}

fn usage_value(usage: &Usage) -> Value {
    let cache_creation = usage
        .extensions
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_read = usage
        .extensions
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .or(usage.cached_input_tokens)
        .unwrap_or(0);
    let mut value = Map::from_iter([
        ("input_tokens".to_owned(), json!(usage.input_tokens)),
        (
            "cache_creation_input_tokens".to_owned(),
            json!(cache_creation),
        ),
        ("cache_read_input_tokens".to_owned(), json!(cache_read)),
        ("output_tokens".to_owned(), json!(usage.output_tokens)),
    ]);
    if let Some(reasoning_tokens) = usage.reasoning_tokens {
        let mut details = usage
            .extensions
            .get("output_tokens_details")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        details.insert("thinking_tokens".to_owned(), json!(reasoning_tokens));
        value.insert("output_tokens_details".to_owned(), Value::Object(details));
    }
    for (key, extension) in &usage.extensions {
        value
            .entry(key.clone())
            .or_insert_with(|| extension.clone());
    }
    Value::Object(value)
}

fn anthropic_error_type(error: &GatewayError) -> &'static str {
    match error.code() {
        "invalid_request" | "unsupported_capability" => "invalid_request_error",
        "upstream_auth" => "authentication_error",
        "rate_limited" => "rate_limit_error",
        "upstream_unavailable" => "overloaded_error",
        "model_not_found" => "not_found_error",
        "upstream_error" | "internal_error" => "api_error",
        _ => "api_error",
    }
}

fn error_value(error: &GatewayError) -> Value {
    wire(
        "error",
        [(
            "error",
            json!({
                "type": anthropic_error_type(error),
                "message": error.public_message(),
            }),
        )],
    )
    .1
}

fn error_value_with_request_id(error: &GatewayError, request_id: &RequestId) -> Value {
    let mut value = error_value(error);
    value
        .as_object_mut()
        .expect("an Anthropic error envelope is always an object")
        .insert("request_id".to_owned(), json!(request_id.as_str()));
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_limits() -> AnthropicLimits {
        AnthropicLimits {
            max_output_items: 2,
            max_identifier_bytes: 8,
            max_opaque_payload_bytes: 16,
            max_value_bytes: 256,
            max_aggregate_bytes: 8,
        }
    }

    fn context() -> AnthropicEncodeContext {
        AnthropicEncodeContext {
            request_id: RequestId::new("req"),
            model: PublicModelId::new("model"),
            initial_usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
                cached_input_tokens: None,
                reasoning_tokens: None,
                extensions: BTreeMap::new(),
            },
            response: AnthropicResponseTemplate {
                stop_reason: AnthropicStopReason::EndTurn,
                stop_sequence: None,
                thinking_signatures: BTreeMap::new(),
                extra: BTreeMap::new(),
            },
        }
    }

    fn created() -> CanonicalEvent {
        CanonicalEvent::Created {
            response_id: "msg".to_owned(),
        }
    }

    #[test]
    fn bounded_stream_retains_no_accumulated_delta_payload() {
        let mut codec = AnthropicCodec::with_limits(context(), tiny_limits());
        codec.encode_event(&created()).unwrap();
        for _ in 0..4 {
            codec
                .encode_event(&CanonicalEvent::OutputTextDelta {
                    item_id: "text".to_owned(),
                    delta: "12345678".to_owned(),
                })
                .unwrap();
        }

        let AnthropicCodec {
            context: _,
            limits: _,
            context_validated: _,
            terminal: _,
            response_id: _,
            active,
            seen_items,
            call_ids,
            next_index: _,
            usage: _,
        } = codec;
        assert!(matches!(active, Some(ActiveBlock::Text { .. })));
        assert_eq!(seen_items.len(), 1);
        assert!(call_ids.is_empty());
    }

    #[test]
    fn private_limits_bound_stream_identifiers_items_and_values() {
        let mut response_id = AnthropicCodec::with_limits(context(), tiny_limits());
        assert_eq!(
            response_id
                .encode_event(&CanonicalEvent::Created {
                    response_id: "123456789".to_owned(),
                })
                .unwrap_err(),
            GatewayError::invalid_request()
        );

        let mut items = AnthropicCodec::with_limits(context(), tiny_limits());
        items.encode_event(&created()).unwrap();
        for item_id in ["one", "two"] {
            items
                .encode_event(&CanonicalEvent::OutputTextDelta {
                    item_id: item_id.to_owned(),
                    delta: String::new(),
                })
                .unwrap();
        }
        assert_eq!(
            items
                .encode_event(&CanonicalEvent::OutputTextDelta {
                    item_id: "three".to_owned(),
                    delta: String::new(),
                })
                .unwrap_err(),
            GatewayError::invalid_request()
        );

        let mut value = AnthropicCodec::with_limits(context(), tiny_limits());
        value.encode_event(&created()).unwrap();
        assert_eq!(
            value
                .encode_event(&CanonicalEvent::OutputTextDelta {
                    item_id: "text".to_owned(),
                    delta: "x".repeat(300),
                })
                .unwrap_err(),
            GatewayError::invalid_request()
        );
    }

    #[test]
    fn private_limits_bound_non_stream_aggregation() {
        let events = [
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
            AnthropicCodec::encode_response_with_limits(context(), &events, tiny_limits())
                .unwrap_err(),
            GatewayError::invalid_request()
        );
    }

    #[test]
    fn private_limit_bounds_opaque_thinking_signatures() {
        let mut context = context();
        context
            .response
            .thinking_signatures
            .insert("think".to_owned(), "x".repeat(17));
        assert_eq!(
            AnthropicCodec::encode_response_with_limits(
                context,
                &[
                    created(),
                    CanonicalEvent::ReasoningDelta {
                        item_id: "think".to_owned(),
                        delta: "safe".to_owned(),
                    },
                ],
                tiny_limits(),
            )
            .unwrap_err(),
            GatewayError::invalid_request()
        );
    }

    #[test]
    fn private_limit_bounds_retained_request_reencoding() {
        let request = CanonicalRequest {
            request_id: RequestId::new("req"),
            model: PublicModelId::new("model"),
            thread_key: None,
            input: Vec::new(),
            tools: Vec::new(),
            stream: false,
            reasoning: None,
            extensions: BTreeMap::from([(
                REQUEST_EXTENSION_KEY.to_owned(),
                json!({"model": "model", "messages": [{"role": "user", "content": "safe"}]}),
            )]),
        };
        assert_eq!(
            AnthropicCodec::encode_message_with_limit(&request, 16).unwrap_err(),
            GatewayError::invalid_request()
        );
    }
}
