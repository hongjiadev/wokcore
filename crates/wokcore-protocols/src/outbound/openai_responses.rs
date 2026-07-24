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

pub struct ResponsesCodec {
    context: ResponsesEncodeContext,
    terminal: bool,
    sequence_number: u64,
    response_id: Option<String>,
    output: Vec<ResponsesOutput>,
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
        Self {
            context,
            terminal: false,
            sequence_number: 1,
            response_id: None,
            output: Vec::new(),
            usage: None,
        }
    }

    pub fn encode_response(
        context: ResponsesEncodeContext,
        events: &[CanonicalEvent],
    ) -> Result<Value, GatewayError> {
        let mut codec = Self::new(context);
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
        if self.response_id.is_some() || response_id.is_empty() {
            return Err(GatewayError::invalid_request());
        }
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
        self.usage = Some(usage_value(usage));
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

        let response = self.response_value(
            "completed",
            output.iter().map(ResponsesOutput::value).collect(),
            usage,
        );
        events.push(self.wire("response.completed", [("response", response)].into_iter()));
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

    fn append_text(&mut self, item_id: &str, delta: &str) -> Result<(usize, bool), GatewayError> {
        if item_id.is_empty() {
            return Err(GatewayError::invalid_request());
        }
        if let Some(output_index) = self.find_output(item_id) {
            match &mut self.output[output_index] {
                ResponsesOutput::Text { text, .. } => {
                    text.push_str(delta);
                    return Ok((output_index, false));
                }
                _ => return Err(GatewayError::invalid_request()),
            }
        }
        let output_index = self.output.len();
        self.output.push(ResponsesOutput::Text {
            item_id: item_id.to_owned(),
            text: delta.to_owned(),
        });
        Ok((output_index, true))
    }

    fn append_reasoning(
        &mut self,
        item_id: &str,
        delta: &str,
    ) -> Result<(usize, bool), GatewayError> {
        if item_id.is_empty() {
            return Err(GatewayError::invalid_request());
        }
        if let Some(output_index) = self.find_output(item_id) {
            match &mut self.output[output_index] {
                ResponsesOutput::Reasoning { text, .. } => {
                    text.push_str(delta);
                    return Ok((output_index, false));
                }
                _ => return Err(GatewayError::invalid_request()),
            }
        }
        let output_index = self.output.len();
        self.output.push(ResponsesOutput::Reasoning {
            item_id: item_id.to_owned(),
            text: delta.to_owned(),
        });
        Ok((output_index, true))
    }

    fn append_tool(
        &mut self,
        item_id: &str,
        call_id: &str,
        name: &str,
        delta: &str,
    ) -> Result<(usize, bool), GatewayError> {
        if item_id.is_empty() || call_id.is_empty() || name.is_empty() {
            return Err(GatewayError::invalid_request());
        }
        if let Some(output_index) = self.find_output(item_id) {
            match &mut self.output[output_index] {
                ResponsesOutput::Tool {
                    call_id: existing_call_id,
                    name: existing_name,
                    arguments,
                    ..
                } if existing_call_id == call_id && existing_name == name => {
                    arguments.push_str(delta);
                    return Ok((output_index, false));
                }
                _ => return Err(GatewayError::invalid_request()),
            }
        }
        if self.output.iter().any(
            |output| matches!(output, ResponsesOutput::Tool { call_id: existing, .. } if existing == call_id),
        ) {
            return Err(GatewayError::invalid_request());
        }
        let output_index = self.output.len();
        self.output.push(ResponsesOutput::Tool {
            item_id: item_id.to_owned(),
            call_id: call_id.to_owned(),
            name: name.to_owned(),
            arguments: delta.to_owned(),
        });
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
