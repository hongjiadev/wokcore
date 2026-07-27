use std::collections::BTreeMap;

use bytes::Bytes;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::{
    canonical::{CanonicalEvent, GatewayError, PublicModelId, Usage},
    stream::encode_sse,
};

#[derive(Clone, Debug, PartialEq)]
pub struct ResponsesEncodeContext {
    pub model: PublicModelId,
    pub created_at: u64,
    pub response: ResponsesResponseTemplate,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResponsesResponseTemplate {
    pub completed_at: Option<u64>,
    pub error: Option<Value>,
    pub incomplete_details: Option<Value>,
    pub instructions: Option<Value>,
    pub max_output_tokens: Option<u64>,
    pub metadata: BTreeMap<String, Value>,
    pub parallel_tool_calls: bool,
    pub previous_response_id: Option<String>,
    pub reasoning: Value,
    pub store: bool,
    pub temperature: Option<f64>,
    pub text: Value,
    pub tool_choice: Value,
    pub tools: Vec<Value>,
    pub top_p: Option<f64>,
    pub truncation: Value,
    pub user: Option<String>,
}

const MAX_RESPONSES_AGGREGATE_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESPONSES_OUTPUT_ITEMS: usize = 4_096;
const MAX_RESPONSES_IDENTIFIER_BYTES: usize = 512;
const MAX_RESPONSES_RETAINED_VALUE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy)]
struct ResponsesLimits {
    max_output_items: usize,
    max_identifier_bytes: usize,
    max_aggregate_bytes: usize,
    max_value_bytes: usize,
}

impl Default for ResponsesLimits {
    fn default() -> Self {
        Self {
            max_output_items: MAX_RESPONSES_OUTPUT_ITEMS,
            max_identifier_bytes: MAX_RESPONSES_IDENTIFIER_BYTES,
            max_aggregate_bytes: MAX_RESPONSES_AGGREGATE_BYTES,
            max_value_bytes: MAX_RESPONSES_RETAINED_VALUE_BYTES,
        }
    }
}

pub struct ResponsesCodec {
    context: ResponsesEncodeContext,
    limits: ResponsesLimits,
    context_validated: bool,
    terminal: bool,
    sequence_number: u64,
    response_id: Option<String>,
    output: Vec<ResponsesOutput>,
    aggregate_bytes: usize,
    usage: Option<Value>,
}

#[derive(Clone)]
enum ResponsesOutput {
    Text {
        item_id: String,
        text: String,
    },
    Reasoning {
        item_id: String,
        text: String,
    },
    Tool {
        item_id: String,
        call_id: String,
        name: String,
        arguments: String,
    },
}

#[derive(Serialize)]
struct ResponsesEvent {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(flatten)]
    fields: Map<String, Value>,
}

type WireEvent = (&'static str, Value);

impl ResponsesCodec {
    pub fn new(context: ResponsesEncodeContext) -> Self {
        Self::with_limits(context, ResponsesLimits::default())
    }

    fn with_limits(context: ResponsesEncodeContext, limits: ResponsesLimits) -> Self {
        Self {
            context,
            limits,
            context_validated: false,
            terminal: false,
            sequence_number: 1,
            response_id: None,
            output: Vec::new(),
            aggregate_bytes: 0,
            usage: None,
        }
    }

    pub fn encode_response(
        context: ResponsesEncodeContext,
        events: &[CanonicalEvent],
    ) -> Result<Value, GatewayError> {
        Self::encode_response_with_limits(context, events, ResponsesLimits::default())
    }

    fn encode_response_with_limits(
        context: ResponsesEncodeContext,
        events: &[CanonicalEvent],
        limits: ResponsesLimits,
    ) -> Result<Value, GatewayError> {
        let mut codec = Self::with_limits(context, limits);
        let mut response = None;

        for event in events {
            let wire_events = codec.encode_event_values(event)?;
            match event {
                CanonicalEvent::Completed => {
                    response = wire_events
                        .last()
                        .and_then(|(_, value)| value.get("response"))
                        .cloned();
                }
                CanonicalEvent::Failed(error) => {
                    response = Some(json!({
                        "error": {
                            "type": "gateway_error",
                            "code": error.code(),
                            "message": error.public_message(),
                        }
                    }));
                }
                _ => {}
            }
        }

        if !codec.terminal {
            return Err(GatewayError::invalid_request());
        }
        response.ok_or_else(GatewayError::invalid_request)
    }

    pub fn encode_event(&mut self, event: &CanonicalEvent) -> Result<Bytes, GatewayError> {
        let wire_events = self.encode_event_values(event)?;
        let mut encoded = Vec::new();
        for (event_name, value) in wire_events {
            encoded.extend_from_slice(&encode_sse(Some(event_name), &value));
        }
        Ok(Bytes::from(encoded))
    }

    fn encode_event_values(
        &mut self,
        event: &CanonicalEvent,
    ) -> Result<Vec<WireEvent>, GatewayError> {
        if self.terminal {
            return Err(GatewayError::invalid_request());
        }
        self.validate_context_once()?;

        match event {
            CanonicalEvent::Created { response_id } => self.encode_created(response_id),
            CanonicalEvent::OutputTextDelta { item_id, delta } => {
                self.encode_text_delta(item_id, delta)
            }
            CanonicalEvent::ReasoningDelta { item_id, delta } => {
                self.encode_reasoning_delta(item_id, delta)
            }
            CanonicalEvent::ToolCallDelta {
                item_id,
                call_id,
                name,
                delta,
            } => self.encode_tool_delta(item_id, call_id, name, delta),
            CanonicalEvent::Usage(usage) => self.encode_usage(usage),
            CanonicalEvent::Completed => self.encode_completed(),
            CanonicalEvent::Failed(error) => Ok(self.encode_failed(error)),
        }
    }

    fn encode_created(&mut self, response_id: &str) -> Result<Vec<WireEvent>, GatewayError> {
        if self.response_id.is_some() {
            return Err(GatewayError::invalid_request());
        }
        self.limits.validate_identifier(response_id)?;
        self.response_id = Some(response_id.to_owned());
        let response = self.response_value("in_progress", Vec::new(), Value::Null);
        Ok(vec![self.wire(
            "response.created",
            [("response", response)].into_iter(),
        )])
    }

    fn encode_text_delta(
        &mut self,
        item_id: &str,
        delta: &str,
    ) -> Result<Vec<WireEvent>, GatewayError> {
        self.require_delta_allowed()?;
        let (output_index, is_new) = self.append_text(item_id, delta)?;
        let mut events = Vec::new();

        if is_new {
            events.push(
                self.wire(
                    "response.output_item.added",
                    [
                        ("output_index", json!(output_index)),
                        (
                            "item",
                            json!({
                                "id": item_id,
                                "type": "message",
                                "role": "assistant",
                                "status": "in_progress",
                                "content": [],
                            }),
                        ),
                    ]
                    .into_iter(),
                ),
            );
            events.push(
                self.wire(
                    "response.content_part.added",
                    [
                        ("item_id", json!(item_id)),
                        ("output_index", json!(output_index)),
                        ("content_index", json!(0)),
                        (
                            "part",
                            json!({
                                "type": "output_text",
                                "text": "",
                                "annotations": [],
                            }),
                        ),
                    ]
                    .into_iter(),
                ),
            );
        }

        events.push(
            self.wire(
                "response.output_text.delta",
                [
                    ("item_id", json!(item_id)),
                    ("output_index", json!(output_index)),
                    ("content_index", json!(0)),
                    ("delta", json!(delta)),
                ]
                .into_iter(),
            ),
        );
        Ok(events)
    }

    fn encode_reasoning_delta(
        &mut self,
        item_id: &str,
        delta: &str,
    ) -> Result<Vec<WireEvent>, GatewayError> {
        self.require_delta_allowed()?;
        let (output_index, is_new) = self.append_reasoning(item_id, delta)?;
        let mut events = Vec::new();

        if is_new {
            events.push(
                self.wire(
                    "response.output_item.added",
                    [
                        ("output_index", json!(output_index)),
                        (
                            "item",
                            json!({
                                "id": item_id,
                                "type": "reasoning",
                                "status": "in_progress",
                                "summary": [],
                            }),
                        ),
                    ]
                    .into_iter(),
                ),
            );
            events.push(
                self.wire(
                    "response.reasoning_summary_part.added",
                    [
                        ("item_id", json!(item_id)),
                        ("output_index", json!(output_index)),
                        ("summary_index", json!(0)),
                        (
                            "part",
                            json!({
                                "type": "summary_text",
                                "text": "",
                            }),
                        ),
                    ]
                    .into_iter(),
                ),
            );
        }

        events.push(
            self.wire(
                "response.reasoning_summary_text.delta",
                [
                    ("item_id", json!(item_id)),
                    ("output_index", json!(output_index)),
                    ("summary_index", json!(0)),
                    ("delta", json!(delta)),
                ]
                .into_iter(),
            ),
        );
        Ok(events)
    }

    fn encode_tool_delta(
        &mut self,
        item_id: &str,
        call_id: &str,
        name: &str,
        delta: &str,
    ) -> Result<Vec<WireEvent>, GatewayError> {
        self.require_delta_allowed()?;
        let (output_index, is_new) = self.append_tool(item_id, call_id, name, delta)?;
        let mut events = Vec::new();

        if is_new {
            events.push(
                self.wire(
                    "response.output_item.added",
                    [
                        ("output_index", json!(output_index)),
                        (
                            "item",
                            json!({
                                "id": item_id,
                                "type": "function_call",
                                "call_id": call_id,
                                "name": name,
                                "arguments": "",
                                "status": "in_progress",
                            }),
                        ),
                    ]
                    .into_iter(),
                ),
            );
        }

        events.push(
            self.wire(
                "response.function_call_arguments.delta",
                [
                    ("item_id", json!(item_id)),
                    ("output_index", json!(output_index)),
                    ("delta", json!(delta)),
                ]
                .into_iter(),
            ),
        );
        Ok(events)
    }

    fn encode_usage(&mut self, usage: &Usage) -> Result<Vec<WireEvent>, GatewayError> {
        self.require_created()?;
        if self.usage.is_some() {
            return Err(GatewayError::invalid_request());
        }
        self.usage = Some(self.limits.validate_usage(usage)?);
        Ok(Vec::new())
    }

    fn encode_completed(&mut self) -> Result<Vec<WireEvent>, GatewayError> {
        self.require_created()?;
        let usage = self
            .usage
            .clone()
            .ok_or_else(GatewayError::invalid_request)?;
        let output = self.output.clone();
        let mut events = Vec::new();

        for (output_index, item) in output.iter().enumerate() {
            match item {
                ResponsesOutput::Text { item_id, text } => {
                    events.push(
                        self.wire(
                            "response.output_text.done",
                            [
                                ("item_id", json!(item_id)),
                                ("output_index", json!(output_index)),
                                ("content_index", json!(0)),
                                ("text", json!(text)),
                            ]
                            .into_iter(),
                        ),
                    );
                    events.push(
                        self.wire(
                            "response.content_part.done",
                            [
                                ("item_id", json!(item_id)),
                                ("output_index", json!(output_index)),
                                ("content_index", json!(0)),
                                (
                                    "part",
                                    json!({
                                        "type": "output_text",
                                        "text": text,
                                        "annotations": [],
                                    }),
                                ),
                            ]
                            .into_iter(),
                        ),
                    );
                    events.push(
                        self.wire(
                            "response.output_item.done",
                            [
                                ("output_index", json!(output_index)),
                                ("item", item.value()),
                            ]
                            .into_iter(),
                        ),
                    );
                }
                ResponsesOutput::Reasoning { item_id, text } => {
                    events.push(
                        self.wire(
                            "response.reasoning_summary_text.done",
                            [
                                ("item_id", json!(item_id)),
                                ("output_index", json!(output_index)),
                                ("summary_index", json!(0)),
                                ("text", json!(text)),
                            ]
                            .into_iter(),
                        ),
                    );
                    events.push(
                        self.wire(
                            "response.reasoning_summary_part.done",
                            [
                                ("item_id", json!(item_id)),
                                ("output_index", json!(output_index)),
                                ("summary_index", json!(0)),
                                (
                                    "part",
                                    json!({
                                        "type": "summary_text",
                                        "text": text,
                                    }),
                                ),
                            ]
                            .into_iter(),
                        ),
                    );
                    events.push(
                        self.wire(
                            "response.output_item.done",
                            [
                                ("output_index", json!(output_index)),
                                ("item", item.value()),
                            ]
                            .into_iter(),
                        ),
                    );
                }
                ResponsesOutput::Tool {
                    item_id,
                    name,
                    arguments,
                    ..
                } => {
                    events.push(
                        self.wire(
                            "response.function_call_arguments.done",
                            [
                                ("item_id", json!(item_id)),
                                ("name", json!(name)),
                                ("output_index", json!(output_index)),
                                ("arguments", json!(arguments)),
                            ]
                            .into_iter(),
                        ),
                    );
                    events.push(
                        self.wire(
                            "response.output_item.done",
                            [
                                ("output_index", json!(output_index)),
                                ("item", item.value()),
                            ]
                            .into_iter(),
                        ),
                    );
                }
            }
        }

        let (status, event_name) = if self.context.response.incomplete_details.is_some() {
            ("incomplete", "response.incomplete")
        } else {
            ("completed", "response.completed")
        };
        let response = self.response_value(
            status,
            output.iter().map(ResponsesOutput::value).collect(),
            usage,
        );
        events.push(self.wire(event_name, [("response", response)].into_iter()));
        self.terminal = true;
        Ok(events)
    }

    fn encode_failed(&mut self, error: &GatewayError) -> Vec<WireEvent> {
        self.terminal = true;
        vec![
            self.wire(
                "error",
                [
                    ("code", json!(error.code())),
                    ("message", json!(error.public_message())),
                    ("param", Value::Null),
                ]
                .into_iter(),
            ),
        ]
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

    fn checked_aggregate_bytes(&self, additional: usize) -> Result<usize, GatewayError> {
        self.aggregate_bytes
            .checked_add(additional)
            .filter(|total| *total <= self.limits.max_aggregate_bytes)
            .ok_or_else(GatewayError::invalid_request)
    }

    fn append_text(&mut self, item_id: &str, delta: &str) -> Result<(usize, bool), GatewayError> {
        self.limits.validate_identifier(item_id)?;
        if let Some(output_index) = self.find_output(item_id) {
            if !matches!(self.output[output_index], ResponsesOutput::Text { .. }) {
                return Err(GatewayError::invalid_request());
            }
            let aggregate_bytes = self.checked_aggregate_bytes(delta.len())?;
            let ResponsesOutput::Text { text, .. } = &mut self.output[output_index] else {
                unreachable!("the output kind was checked before mutation");
            };
            text.push_str(delta);
            self.aggregate_bytes = aggregate_bytes;
            return Ok((output_index, false));
        }
        if self.output.len() >= self.limits.max_output_items {
            return Err(GatewayError::invalid_request());
        }
        let aggregate_bytes = self.checked_aggregate_bytes(delta.len())?;
        let output_index = self.output.len();
        self.output.push(ResponsesOutput::Text {
            item_id: item_id.to_owned(),
            text: delta.to_owned(),
        });
        self.aggregate_bytes = aggregate_bytes;
        Ok((output_index, true))
    }

    fn append_reasoning(
        &mut self,
        item_id: &str,
        delta: &str,
    ) -> Result<(usize, bool), GatewayError> {
        self.limits.validate_identifier(item_id)?;
        if let Some(output_index) = self.find_output(item_id) {
            if !matches!(self.output[output_index], ResponsesOutput::Reasoning { .. }) {
                return Err(GatewayError::invalid_request());
            }
            let aggregate_bytes = self.checked_aggregate_bytes(delta.len())?;
            let ResponsesOutput::Reasoning { text, .. } = &mut self.output[output_index] else {
                unreachable!("the output kind was checked before mutation");
            };
            text.push_str(delta);
            self.aggregate_bytes = aggregate_bytes;
            return Ok((output_index, false));
        }
        if self.output.len() >= self.limits.max_output_items {
            return Err(GatewayError::invalid_request());
        }
        let aggregate_bytes = self.checked_aggregate_bytes(delta.len())?;
        let output_index = self.output.len();
        self.output.push(ResponsesOutput::Reasoning {
            item_id: item_id.to_owned(),
            text: delta.to_owned(),
        });
        self.aggregate_bytes = aggregate_bytes;
        Ok((output_index, true))
    }

    fn append_tool(
        &mut self,
        item_id: &str,
        call_id: &str,
        name: &str,
        delta: &str,
    ) -> Result<(usize, bool), GatewayError> {
        self.limits.validate_identifier(item_id)?;
        self.limits.validate_identifier(call_id)?;
        self.limits.validate_identifier(name)?;
        if let Some(output_index) = self.find_output(item_id) {
            match &self.output[output_index] {
                ResponsesOutput::Tool {
                    call_id: existing_call_id,
                    name: existing_name,
                    ..
                } if existing_call_id == call_id && existing_name == name => {}
                _ => return Err(GatewayError::invalid_request()),
            }
            let aggregate_bytes = self.checked_aggregate_bytes(delta.len())?;
            let ResponsesOutput::Tool { arguments, .. } = &mut self.output[output_index] else {
                unreachable!("the output kind was checked before mutation");
            };
            arguments.push_str(delta);
            self.aggregate_bytes = aggregate_bytes;
            return Ok((output_index, false));
        }
        if self.output.len() >= self.limits.max_output_items
            || self.output.iter().any(
                |output| matches!(output, ResponsesOutput::Tool { call_id: existing, .. } if existing == call_id),
            )
        {
            return Err(GatewayError::invalid_request());
        }
        let aggregate_bytes = self.checked_aggregate_bytes(delta.len())?;
        let output_index = self.output.len();
        self.output.push(ResponsesOutput::Tool {
            item_id: item_id.to_owned(),
            call_id: call_id.to_owned(),
            name: name.to_owned(),
            arguments: delta.to_owned(),
        });
        self.aggregate_bytes = aggregate_bytes;
        Ok((output_index, true))
    }

    fn find_output(&self, item_id: &str) -> Option<usize> {
        self.output
            .iter()
            .position(|output| output.item_id() == item_id)
    }

    fn response_value(&self, status: &str, output: Vec<Value>, usage: Value) -> Value {
        let completed_at = if status == "completed" {
            json!(self.context.response.completed_at)
        } else {
            Value::Null
        };
        json!({
            "id": self.response_id.as_deref().unwrap_or_default(),
            "object": "response",
            "created_at": self.context.created_at,
            "status": status,
            "completed_at": completed_at,
            "error": self.context.response.error,
            "incomplete_details": self.context.response.incomplete_details,
            "instructions": self.context.response.instructions,
            "max_output_tokens": self.context.response.max_output_tokens,
            "model": self.context.model.as_str(),
            "output": output,
            "parallel_tool_calls": self.context.response.parallel_tool_calls,
            "previous_response_id": self.context.response.previous_response_id,
            "reasoning": self.context.response.reasoning,
            "store": self.context.response.store,
            "temperature": self.context.response.temperature,
            "text": self.context.response.text,
            "tool_choice": self.context.response.tool_choice,
            "tools": self.context.response.tools,
            "top_p": self.context.response.top_p,
            "truncation": self.context.response.truncation,
            "usage": usage,
            "user": self.context.response.user,
            "metadata": self.context.response.metadata,
        })
    }

    fn wire(
        &mut self,
        kind: &'static str,
        fields: impl Iterator<Item = (&'static str, Value)>,
    ) -> WireEvent {
        let sequence_number = self.sequence_number;
        self.sequence_number += 1;
        (
            kind,
            wire_event(
                kind,
                fields
                    .into_iter()
                    .chain([("sequence_number", json!(sequence_number))]),
            ),
        )
    }
}

impl ResponsesLimits {
    fn validate_identifier(self, value: &str) -> Result<(), GatewayError> {
        if value.is_empty() || value.len() > self.max_identifier_bytes {
            Err(GatewayError::invalid_request())
        } else {
            Ok(())
        }
    }

    fn validate_context(self, context: &ResponsesEncodeContext) -> Result<(), GatewayError> {
        self.validate_identifier(context.model.as_str())?;
        if let Some(previous_response_id) = &context.response.previous_response_id {
            self.validate_identifier(previous_response_id)?;
        }
        self.validate_string_map(&context.response.metadata)?;
        if context.response.tools.len() > self.max_output_items {
            return Err(GatewayError::invalid_request());
        }
        self.validate_serialized(&context.response.error)?;
        self.validate_serialized(&context.response.incomplete_details)?;
        self.validate_serialized(&context.response.instructions)?;
        self.validate_serialized(&context.response.reasoning)?;
        self.validate_serialized(&context.response.text)?;
        self.validate_serialized(&context.response.tool_choice)?;
        self.validate_serialized(&context.response.tools)?;
        self.validate_serialized(&context.response.truncation)?;
        self.validate_serialized(&context.response.user)
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

impl ResponsesOutput {
    fn item_id(&self) -> &str {
        match self {
            Self::Text { item_id, .. }
            | Self::Reasoning { item_id, .. }
            | Self::Tool { item_id, .. } => item_id,
        }
    }

    fn value(&self) -> Value {
        match self {
            Self::Text { item_id, text } => json!({
                "id": item_id,
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{
                    "type": "output_text",
                    "text": text,
                    "annotations": [],
                }],
            }),
            Self::Reasoning { item_id, text } => json!({
                "id": item_id,
                "type": "reasoning",
                "status": "completed",
                "summary": [{
                    "type": "summary_text",
                    "text": text,
                }],
            }),
            Self::Tool {
                item_id,
                call_id,
                name,
                arguments,
            } => json!({
                "id": item_id,
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": arguments,
                "status": "completed",
            }),
        }
    }
}

fn wire_event(
    kind: &'static str,
    fields: impl IntoIterator<Item = (&'static str, Value)>,
) -> Value {
    serde_json::to_value(ResponsesEvent {
        kind,
        fields: fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    })
    .expect("serializing a Responses event cannot fail")
}

fn usage_value(usage: &Usage) -> Value {
    let mut value = Map::from_iter([
        ("input_tokens".to_owned(), json!(usage.input_tokens)),
        (
            "input_tokens_details".to_owned(),
            json!({"cached_tokens": usage.cached_input_tokens.unwrap_or(0)}),
        ),
        ("output_tokens".to_owned(), json!(usage.output_tokens)),
        (
            "output_tokens_details".to_owned(),
            json!({"reasoning_tokens": usage.reasoning_tokens.unwrap_or(0)}),
        ),
        (
            "total_tokens".to_owned(),
            json!(usage.input_tokens.saturating_add(usage.output_tokens)),
        ),
    ]);
    for (key, extension) in &usage.extensions {
        value
            .entry(key.clone())
            .or_insert_with(|| extension.clone());
    }
    Value::Object(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_limits() -> ResponsesLimits {
        ResponsesLimits {
            max_output_items: 2,
            max_identifier_bytes: 8,
            max_aggregate_bytes: 8,
            max_value_bytes: 1_024,
        }
    }

    fn context() -> ResponsesEncodeContext {
        ResponsesEncodeContext {
            model: PublicModelId::new("model"),
            created_at: 1,
            response: ResponsesResponseTemplate {
                completed_at: Some(2),
                error: None,
                incomplete_details: None,
                instructions: None,
                max_output_tokens: None,
                metadata: BTreeMap::new(),
                parallel_tool_calls: false,
                previous_response_id: None,
                reasoning: Value::Null,
                store: false,
                temperature: None,
                text: Value::Null,
                tool_choice: Value::Null,
                tools: Vec::new(),
                top_p: None,
                truncation: Value::Null,
                user: None,
            },
        }
    }

    fn created() -> CanonicalEvent {
        CanonicalEvent::Created {
            response_id: "resp".to_owned(),
        }
    }

    fn usage() -> CanonicalEvent {
        CanonicalEvent::Usage(Usage {
            input_tokens: 1,
            output_tokens: 1,
            cached_input_tokens: None,
            reasoning_tokens: None,
            extensions: BTreeMap::new(),
        })
    }

    fn assert_aggregate_rejected_atomically(first: CanonicalEvent, second: CanonicalEvent) {
        let mut codec = ResponsesCodec::with_limits(context(), tiny_limits());
        codec.encode_event(&created()).unwrap();
        codec.encode_event(&first).unwrap();

        assert_eq!(
            codec.encode_event(&second).unwrap_err(),
            GatewayError::invalid_request()
        );
        assert_eq!(codec.aggregate_bytes, 5);
        assert_eq!(codec.output.len(), 1);
        let retained = match &codec.output[0] {
            ResponsesOutput::Text { text, .. } | ResponsesOutput::Reasoning { text, .. } => text,
            ResponsesOutput::Tool { arguments, .. } => arguments,
        };
        assert_eq!(retained, "12345");
    }

    #[test]
    fn private_limits_bound_stream_deltas_atomically() {
        assert_aggregate_rejected_atomically(
            CanonicalEvent::OutputTextDelta {
                item_id: "text".to_owned(),
                delta: "12345".to_owned(),
            },
            CanonicalEvent::OutputTextDelta {
                item_id: "text".to_owned(),
                delta: "6789".to_owned(),
            },
        );
        assert_aggregate_rejected_atomically(
            CanonicalEvent::ReasoningDelta {
                item_id: "reason".to_owned(),
                delta: "12345".to_owned(),
            },
            CanonicalEvent::ReasoningDelta {
                item_id: "reason".to_owned(),
                delta: "6789".to_owned(),
            },
        );
        assert_aggregate_rejected_atomically(
            CanonicalEvent::ToolCallDelta {
                item_id: "tool".to_owned(),
                call_id: "call".to_owned(),
                name: "run".to_owned(),
                delta: "12345".to_owned(),
            },
            CanonicalEvent::ToolCallDelta {
                item_id: "tool".to_owned(),
                call_id: "call".to_owned(),
                name: "run".to_owned(),
                delta: "6789".to_owned(),
            },
        );
    }

    #[test]
    fn private_limits_bound_stream_items_and_identifiers_atomically() {
        let mut response_id = ResponsesCodec::with_limits(context(), tiny_limits());
        assert_eq!(
            response_id
                .encode_event(&CanonicalEvent::Created {
                    response_id: "123456789".to_owned(),
                })
                .unwrap_err(),
            GatewayError::invalid_request()
        );
        assert!(response_id.response_id.is_none());

        for event in [
            CanonicalEvent::OutputTextDelta {
                item_id: "123456789".to_owned(),
                delta: String::new(),
            },
            CanonicalEvent::ToolCallDelta {
                item_id: "tool".to_owned(),
                call_id: "123456789".to_owned(),
                name: "run".to_owned(),
                delta: String::new(),
            },
            CanonicalEvent::ToolCallDelta {
                item_id: "tool".to_owned(),
                call_id: "call".to_owned(),
                name: "123456789".to_owned(),
                delta: String::new(),
            },
        ] {
            let mut codec = ResponsesCodec::with_limits(context(), tiny_limits());
            codec.encode_event(&created()).unwrap();
            assert_eq!(
                codec.encode_event(&event).unwrap_err(),
                GatewayError::invalid_request()
            );
            assert!(codec.output.is_empty());
            assert_eq!(codec.aggregate_bytes, 0);
        }

        let mut oversized_new_item = ResponsesCodec::with_limits(context(), tiny_limits());
        oversized_new_item.encode_event(&created()).unwrap();
        assert_eq!(
            oversized_new_item
                .encode_event(&CanonicalEvent::OutputTextDelta {
                    item_id: "text".to_owned(),
                    delta: "123456789".to_owned(),
                })
                .unwrap_err(),
            GatewayError::invalid_request()
        );
        assert!(oversized_new_item.output.is_empty());
        assert_eq!(oversized_new_item.aggregate_bytes, 0);

        let mut items = ResponsesCodec::with_limits(context(), tiny_limits());
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
        assert_eq!(items.output.len(), 2);
        assert_eq!(items.aggregate_bytes, 0);
    }

    #[test]
    fn private_limits_bound_context_and_usage_values() {
        for oversized in [
            {
                let mut value = context();
                value.model = PublicModelId::new("123456789");
                value
            },
            {
                let mut value = context();
                value.response.previous_response_id = Some("123456789".to_owned());
                value
            },
            {
                let mut value = context();
                value
                    .response
                    .metadata
                    .insert("123456789".to_owned(), Value::Null);
                value
            },
            {
                let mut value = context();
                value.response.instructions = Some(json!("x".repeat(1_100)));
                value
            },
        ] {
            let mut codec = ResponsesCodec::with_limits(oversized, tiny_limits());
            assert_eq!(
                codec.encode_event(&created()).unwrap_err(),
                GatewayError::invalid_request()
            );
            assert!(codec.response_id.is_none());
        }

        let mut usage_codec = ResponsesCodec::with_limits(context(), tiny_limits());
        usage_codec.encode_event(&created()).unwrap();
        assert_eq!(
            usage_codec
                .encode_event(&CanonicalEvent::Usage(Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                    extensions: BTreeMap::from([("large".to_owned(), json!("x".repeat(1_100)),)]),
                }))
                .unwrap_err(),
            GatewayError::invalid_request()
        );
        assert!(usage_codec.usage.is_none());
    }

    #[test]
    fn private_limits_bound_non_stream_aggregation_and_items() {
        let aggregate_events = vec![
            created(),
            CanonicalEvent::OutputTextDelta {
                item_id: "text".to_owned(),
                delta: "12345".to_owned(),
            },
            CanonicalEvent::OutputTextDelta {
                item_id: "text".to_owned(),
                delta: "6789".to_owned(),
            },
            usage(),
            CanonicalEvent::Completed,
        ];
        assert_eq!(
            ResponsesCodec::encode_response_with_limits(
                context(),
                &aggregate_events,
                tiny_limits(),
            )
            .unwrap_err(),
            GatewayError::invalid_request()
        );

        let item_events = vec![
            created(),
            CanonicalEvent::OutputTextDelta {
                item_id: "one".to_owned(),
                delta: String::new(),
            },
            CanonicalEvent::ReasoningDelta {
                item_id: "two".to_owned(),
                delta: String::new(),
            },
            CanonicalEvent::ToolCallDelta {
                item_id: "three".to_owned(),
                call_id: "call".to_owned(),
                name: "run".to_owned(),
                delta: String::new(),
            },
            usage(),
            CanonicalEvent::Completed,
        ];
        assert_eq!(
            ResponsesCodec::encode_response_with_limits(context(), &item_events, tiny_limits())
                .unwrap_err(),
            GatewayError::invalid_request()
        );
    }

    #[test]
    fn private_limits_accept_exact_boundaries_and_preserve_completed_output() {
        let events = [
            CanonicalEvent::Created {
                response_id: "12345678".to_owned(),
            },
            CanonicalEvent::OutputTextDelta {
                item_id: "12345678".to_owned(),
                delta: "1234".to_owned(),
            },
            CanonicalEvent::ToolCallDelta {
                item_id: "tool".to_owned(),
                call_id: "12345678".to_owned(),
                name: "12345678".to_owned(),
                delta: "5678".to_owned(),
            },
            usage(),
            CanonicalEvent::Completed,
        ];
        let response =
            ResponsesCodec::encode_response_with_limits(context(), &events, tiny_limits()).unwrap();

        assert_eq!(response["output"][0]["content"][0]["text"], "1234");
        assert_eq!(response["output"][1]["arguments"], "5678");
    }
}
