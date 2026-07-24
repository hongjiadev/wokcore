use std::collections::{BTreeMap, HashMap};

use prost::Message as ProstMessage;
use serde_json::Value;
use url::Url;

use crate::canonical::{
    CanonicalEvent, CanonicalRequest, GatewayError, InputItem, RequestId, Usage,
};

use super::{
    UpstreamLimits, UpstreamRequest, account_stream_events, checked_join, classify_http_error,
    push_event, validate_base_url, validate_identifier, validate_value_size,
};

const CONNECT_HEADER_BYTES: usize = 5;
const CONNECT_END_STREAM: u8 = 0x02;
const RESPONSES_TOOL_PROVIDER: &str = "opencodex-responses";

#[derive(Clone)]
pub struct CursorConfig {
    base_url: Url,
    enabled: bool,
}

impl std::fmt::Debug for CursorConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CursorConfig")
            .field("base_url", &"[redacted]")
            .field("enabled", &self.enabled)
            .field("native_local_exec", &false)
            .finish()
    }
}

impl CursorConfig {
    pub fn new(base_url: Url, enabled: bool) -> Result<Self, GatewayError> {
        validate_base_url(&base_url)?;
        Ok(Self { base_url, enabled })
    }
}

pub struct CursorAdapter {
    config: CursorConfig,
    limits: UpstreamLimits,
}

impl CursorAdapter {
    pub fn new(config: CursorConfig, limits: UpstreamLimits) -> Self {
        Self { config, limits }
    }

    pub fn build_request(
        &self,
        request: &CanonicalRequest,
    ) -> Result<UpstreamRequest, GatewayError> {
        if !self.config.enabled {
            return Err(GatewayError::unsupported_capability());
        }
        if request.reasoning.is_some() {
            return Err(GatewayError::unsupported_capability());
        }
        if request.input.len() > self.limits.max_collection_items
            || request.tools.len() > self.limits.max_collection_items
        {
            return Err(GatewayError::invalid_request());
        }

        let model = request
            .model
            .as_str()
            .strip_prefix("cursor/")
            .unwrap_or(request.model.as_str());
        validate_identifier(model, self.limits.max_identifier_bytes)?;
        validate_identifier(
            request.request_id.as_str(),
            self.limits.max_identifier_bytes,
        )?;
        let conversation_id = request
            .thread_key
            .as_ref()
            .map_or(request.request_id.as_str(), |key| key.as_str());
        validate_identifier(conversation_id, self.limits.max_identifier_bytes)?;

        let mut prompt = String::new();
        for item in &request.input {
            let text = match item {
                InputItem::Text { text } => {
                    if text.len() > self.limits.max_text_delta_bytes {
                        return Err(GatewayError::invalid_request());
                    }
                    text.clone()
                }
                InputItem::ImageUrl { .. } => {
                    return Err(GatewayError::unsupported_capability());
                }
                InputItem::ToolResult { call_id, output } => {
                    validate_identifier(call_id, self.limits.max_identifier_bytes)?;
                    validate_value_size(output, self.limits.max_tool_argument_bytes)?;
                    format!(
                        "[tool_result]\ncall_id: {call_id}\noutput:\n{}",
                        serde_json::to_string(output)
                            .map_err(|_| GatewayError::invalid_request())?
                    )
                }
            };
            if !prompt.is_empty() {
                prompt.push('\n');
            }
            prompt.push_str(&text);
            if prompt.len() > self.limits.max_request_body_bytes {
                return Err(GatewayError::invalid_request());
            }
        }

        let tool_definitions = request
            .tools
            .iter()
            .map(|tool| {
                validate_identifier(&tool.name, self.limits.max_identifier_bytes)?;
                if tool
                    .description
                    .as_ref()
                    .is_some_and(|value| value.len() > self.limits.max_text_delta_bytes)
                {
                    return Err(GatewayError::invalid_request());
                }
                validate_value_size(&tool.input_schema, self.limits.max_request_body_bytes)?;
                Ok(McpToolDefinition {
                    name: tool.name.clone(),
                    description: tool.description.clone().unwrap_or_default(),
                    input_schema: encode_google_value(&tool.input_schema),
                    provider_identifier: RESPONSES_TOOL_PROVIDER.to_owned(),
                    tool_name: tool.name.clone(),
                })
            })
            .collect::<Result<Vec<_>, GatewayError>>()?;

        let run_request = AgentRunRequest {
            conversation_state: Some(ConversationStateStructure {
                token_details: None,
            }),
            action: Some(ConversationAction {
                action: Some(conversation_action::Action::UserMessageAction(
                    UserMessageAction {
                        user_message: Some(UserMessage {
                            text: prompt,
                            message_id: request.request_id.as_str().to_owned(),
                        }),
                        request_context: Some(RequestContext {}),
                    },
                )),
            }),
            model_details: Some(ModelDetails {
                model_id: model.to_owned(),
                display_model_id: model.to_owned(),
                display_name: model.to_owned(),
                display_name_short: model.to_owned(),
            }),
            mcp_tools: (!tool_definitions.is_empty()).then_some(McpTools {
                mcp_tools: tool_definitions,
            }),
            conversation_id: Some(conversation_id.to_owned()),
        };
        let payload = AgentClientMessage {
            message: Some(agent_client_message::Message::RunRequest(run_request)),
        }
        .encode_to_vec();
        let body = encode_connect_frame(&payload, self.limits.max_request_body_bytes)?;

        Ok(UpstreamRequest {
            url: checked_join(&self.config.base_url, "agent.v1.AgentService/Run")?,
            headers: BTreeMap::from([
                (
                    "content-type".to_owned(),
                    "application/connect+proto".to_owned(),
                ),
                ("connect-protocol-version".to_owned(), "1".to_owned()),
            ]),
            body,
            stream: true,
        })
    }

    pub fn stream_decoder(&self, request_id: RequestId) -> CursorStreamDecoder {
        CursorStreamDecoder::new(request_id, self.limits)
    }

    pub fn decode_http_error(&self, status: u16, retry_after: Option<&str>) -> GatewayError {
        classify_http_error(status, retry_after, "cursor upstream")
    }
}

pub struct CursorStreamDecoder {
    request_id: RequestId,
    limits: UpstreamLimits,
    buffer: Vec<u8>,
    received_bytes: usize,
    created: bool,
    completed: bool,
    turn_ended: bool,
    end_stream: bool,
    failed: bool,
    emitted_events: usize,
    next_item: usize,
    output_tokens: u64,
    active_tools: BTreeMap<String, ToolState>,
}

impl CursorStreamDecoder {
    fn new(request_id: RequestId, limits: UpstreamLimits) -> Self {
        Self {
            request_id,
            limits,
            buffer: Vec::new(),
            received_bytes: 0,
            created: false,
            completed: false,
            turn_ended: false,
            end_stream: false,
            failed: false,
            emitted_events: 0,
            next_item: 0,
            output_tokens: 0,
            active_tools: BTreeMap::new(),
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<CanonicalEvent>, GatewayError> {
        if self.failed || self.end_stream {
            return Err(GatewayError::invalid_request());
        }
        match self.push_inner(chunk) {
            Ok(events) => {
                if let Err(error) =
                    account_stream_events(&mut self.emitted_events, events.len(), self.limits)
                {
                    self.failed = true;
                    return Err(error);
                }
                Ok(events)
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    fn push_inner(&mut self, chunk: &[u8]) -> Result<Vec<CanonicalEvent>, GatewayError> {
        self.received_bytes = self
            .received_bytes
            .checked_add(chunk.len())
            .ok_or_else(GatewayError::invalid_request)?;
        if self.received_bytes > self.limits.max_response_body_bytes {
            return Err(GatewayError::invalid_request());
        }
        self.buffer.extend_from_slice(chunk);

        let mut events = Vec::new();
        self.ensure_created(&mut events)?;
        loop {
            if self.buffer.len() < CONNECT_HEADER_BYTES {
                break;
            }
            let flags = self.buffer[0];
            let frame_len =
                u32::from_be_bytes(self.buffer[1..CONNECT_HEADER_BYTES].try_into().unwrap())
                    as usize;
            if frame_len > self.limits.max_stream_frame_bytes {
                return Err(GatewayError::invalid_request());
            }
            let envelope_len = CONNECT_HEADER_BYTES
                .checked_add(frame_len)
                .ok_or_else(GatewayError::invalid_request)?;
            if self.buffer.len() < envelope_len {
                break;
            }
            let payload = self.buffer[CONNECT_HEADER_BYTES..envelope_len].to_vec();
            self.buffer.drain(..envelope_len);

            if flags == CONNECT_END_STREAM {
                self.decode_end_stream(&payload, &mut events)?;
                if !self.buffer.is_empty() {
                    return Err(GatewayError::invalid_request());
                }
            } else if flags == 0 {
                self.decode_message(&payload, &mut events)?;
            } else {
                return Err(GatewayError::unsupported_capability());
            }
        }
        Ok(events)
    }

    pub fn finish(&mut self) -> Result<Vec<CanonicalEvent>, GatewayError> {
        if self.failed
            || !self.buffer.is_empty()
            || !self.active_tools.is_empty()
            || !self.turn_ended
            || !self.end_stream
            || !self.completed
        {
            self.failed = true;
            return Err(GatewayError::invalid_request());
        }
        Ok(Vec::new())
    }

    fn ensure_created(&mut self, events: &mut Vec<CanonicalEvent>) -> Result<(), GatewayError> {
        if !self.created {
            let response_id = format!("cursor_{}", self.request_id.as_str());
            validate_identifier(&response_id, self.limits.max_identifier_bytes)?;
            push_event(events, CanonicalEvent::Created { response_id }, self.limits)?;
            self.created = true;
        }
        Ok(())
    }

    fn decode_end_stream(
        &mut self,
        payload: &[u8],
        events: &mut Vec<CanonicalEvent>,
    ) -> Result<(), GatewayError> {
        let value: Value =
            serde_json::from_slice(payload).map_err(|_| GatewayError::invalid_request())?;
        let object = value
            .as_object()
            .ok_or_else(GatewayError::invalid_request)?;
        if object.contains_key("error") {
            return Err(GatewayError::upstream_response(
                502,
                "cursor connect end stream",
            ));
        }
        if !self.turn_ended || self.completed || !self.active_tools.is_empty() {
            return Err(GatewayError::invalid_request());
        }
        push_event(
            events,
            CanonicalEvent::Usage(Usage {
                input_tokens: 0,
                output_tokens: self.output_tokens,
                cached_input_tokens: None,
                reasoning_tokens: None,
                extensions: BTreeMap::new(),
            }),
            self.limits,
        )?;
        push_event(events, CanonicalEvent::Completed, self.limits)?;
        self.completed = true;
        self.end_stream = true;
        Ok(())
    }

    fn decode_message(
        &mut self,
        payload: &[u8],
        events: &mut Vec<CanonicalEvent>,
    ) -> Result<(), GatewayError> {
        if self.turn_ended {
            return Err(GatewayError::invalid_request());
        }
        let server =
            AgentServerMessage::decode(payload).map_err(|_| GatewayError::invalid_request())?;
        match server.message {
            Some(agent_server_message::Message::ExecServerMessage(_)) => {
                Err(GatewayError::no_executor())
            }
            Some(agent_server_message::Message::ConversationCheckpointUpdate(checkpoint)) => {
                if let Some(details) = checkpoint.token_details {
                    let used_tokens = u64::from(details.used_tokens);
                    let input_tokens = used_tokens.saturating_sub(self.output_tokens);
                    push_event(
                        events,
                        CanonicalEvent::Usage(Usage {
                            input_tokens,
                            output_tokens: self.output_tokens,
                            cached_input_tokens: None,
                            reasoning_tokens: None,
                            extensions: BTreeMap::new(),
                        }),
                        self.limits,
                    )?;
                }
                Ok(())
            }
            Some(agent_server_message::Message::InteractionUpdate(update)) => {
                self.decode_interaction(update, events)
            }
            None => Err(GatewayError::unsupported_capability()),
        }
    }

    fn decode_interaction(
        &mut self,
        update: InteractionUpdate,
        events: &mut Vec<CanonicalEvent>,
    ) -> Result<(), GatewayError> {
        match update.message {
            Some(interaction_update::Message::TextDelta(value)) => {
                self.push_text(events, value.text, false)
            }
            Some(interaction_update::Message::ThinkingDelta(value)) => {
                self.push_text(events, value.text, true)
            }
            Some(interaction_update::Message::ToolCallStarted(value)) => {
                self.start_tool(value.call_id, value.tool_call)
            }
            Some(interaction_update::Message::PartialToolCall(value)) => {
                if !self.active_tools.contains_key(&value.call_id) {
                    self.start_tool(value.call_id.clone(), value.tool_call)?;
                }
                let state = self
                    .active_tools
                    .get_mut(&value.call_id)
                    .ok_or_else(GatewayError::invalid_request)?;
                if value.args_text_delta.len() > self.limits.max_tool_argument_bytes {
                    return Err(GatewayError::invalid_request());
                }
                // The pinned schema defines this as aggregate argument text so far.
                if value.args_text_delta == state.arguments {
                    Ok(())
                } else if value.args_text_delta.starts_with(&state.arguments) {
                    state.arguments = value.args_text_delta;
                    Ok(())
                } else {
                    Err(GatewayError::invalid_request())
                }
            }
            Some(interaction_update::Message::ToolCallCompleted(value)) => {
                if !self.active_tools.contains_key(&value.call_id) {
                    self.start_tool(value.call_id.clone(), value.tool_call)?;
                }
                let state = self
                    .active_tools
                    .remove(&value.call_id)
                    .ok_or_else(GatewayError::invalid_request)?;
                push_event(
                    events,
                    CanonicalEvent::ToolCallDelta {
                        item_id: state.item_id,
                        call_id: value.call_id,
                        name: state.name,
                        delta: state.arguments,
                    },
                    self.limits,
                )
            }
            Some(interaction_update::Message::TokenDelta(value)) => {
                if value.tokens < 0 {
                    return Err(GatewayError::invalid_request());
                }
                self.output_tokens = self
                    .output_tokens
                    .checked_add(value.tokens as u64)
                    .ok_or_else(GatewayError::invalid_request)?;
                Ok(())
            }
            Some(interaction_update::Message::TurnEnded(_)) => {
                if !self.active_tools.is_empty() || self.turn_ended {
                    return Err(GatewayError::invalid_request());
                }
                self.turn_ended = true;
                Ok(())
            }
            None => Err(GatewayError::unsupported_capability()),
        }
    }

    fn push_text(
        &mut self,
        events: &mut Vec<CanonicalEvent>,
        delta: String,
        reasoning: bool,
    ) -> Result<(), GatewayError> {
        if delta.len() > self.limits.max_text_delta_bytes {
            return Err(GatewayError::invalid_request());
        }
        let item_id = format!("cursor_item_{}", self.next_item);
        validate_identifier(&item_id, self.limits.max_identifier_bytes)?;
        self.next_item = self
            .next_item
            .checked_add(1)
            .ok_or_else(GatewayError::invalid_request)?;
        let event = if reasoning {
            CanonicalEvent::ReasoningDelta { item_id, delta }
        } else {
            CanonicalEvent::OutputTextDelta { item_id, delta }
        };
        push_event(events, event, self.limits)
    }

    fn start_tool(
        &mut self,
        call_id: String,
        tool_call: Option<ToolCall>,
    ) -> Result<(), GatewayError> {
        if self.active_tools.len() >= self.limits.max_collection_items
            || self.active_tools.contains_key(&call_id)
        {
            return Err(GatewayError::invalid_request());
        }
        validate_identifier(&call_id, self.limits.max_identifier_bytes)?;
        let name = tool_call
            .and_then(|value| value.tool)
            .and_then(|value| match value {
                tool_call::Tool::McpToolCall(call) => call.args,
            })
            .map(|args| {
                if args.tool_name.is_empty() {
                    args.name
                } else {
                    args.tool_name
                }
            })
            .filter(|value| !value.is_empty())
            .ok_or_else(GatewayError::unsupported_capability)?;
        validate_identifier(&name, self.limits.max_identifier_bytes)?;
        let item_id = format!("cursor_tool_{}", self.next_item);
        validate_identifier(&item_id, self.limits.max_identifier_bytes)?;
        self.next_item = self
            .next_item
            .checked_add(1)
            .ok_or_else(GatewayError::invalid_request)?;
        self.active_tools.insert(
            call_id,
            ToolState {
                item_id,
                name,
                arguments: String::new(),
            },
        );
        Ok(())
    }
}

struct ToolState {
    item_id: String,
    name: String,
    arguments: String,
}

fn encode_connect_frame(payload: &[u8], limit: usize) -> Result<Vec<u8>, GatewayError> {
    let payload_len = u32::try_from(payload.len()).map_err(|_| GatewayError::invalid_request())?;
    let total_len = CONNECT_HEADER_BYTES
        .checked_add(payload.len())
        .ok_or_else(GatewayError::invalid_request)?;
    if total_len > limit {
        return Err(GatewayError::invalid_request());
    }
    let mut frame = Vec::with_capacity(total_len);
    frame.push(0);
    frame.extend_from_slice(&payload_len.to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

fn encode_google_value(value: &Value) -> Vec<u8> {
    GoogleValue {
        kind: Some(match value {
            Value::Null => google_value::Kind::Null(0),
            Value::Bool(value) => google_value::Kind::Bool(*value),
            Value::Number(value) => google_value::Kind::Number(value.as_f64().unwrap_or_default()),
            Value::String(value) => google_value::Kind::String(value.clone()),
            Value::Array(values) => google_value::Kind::List(ListValue {
                values: values
                    .iter()
                    .map(|value| GoogleValue {
                        kind: Some(match_google_value(value)),
                    })
                    .collect(),
            }),
            Value::Object(values) => google_value::Kind::Struct(StructValue {
                fields: values
                    .iter()
                    .map(|(key, value)| {
                        (
                            key.clone(),
                            GoogleValue {
                                kind: Some(match_google_value(value)),
                            },
                        )
                    })
                    .collect(),
            }),
        }),
    }
    .encode_to_vec()
}

fn match_google_value(value: &Value) -> google_value::Kind {
    match value {
        Value::Null => google_value::Kind::Null(0),
        Value::Bool(value) => google_value::Kind::Bool(*value),
        Value::Number(value) => google_value::Kind::Number(value.as_f64().unwrap_or_default()),
        Value::String(value) => google_value::Kind::String(value.clone()),
        Value::Array(values) => google_value::Kind::List(ListValue {
            values: values
                .iter()
                .map(|value| GoogleValue {
                    kind: Some(match_google_value(value)),
                })
                .collect(),
        }),
        Value::Object(values) => google_value::Kind::Struct(StructValue {
            fields: values
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        GoogleValue {
                            kind: Some(match_google_value(value)),
                        },
                    )
                })
                .collect(),
        }),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
struct AgentClientMessage {
    #[prost(oneof = "agent_client_message::Message", tags = "1")]
    message: Option<agent_client_message::Message>,
}

mod agent_client_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub(super) enum Message {
        #[prost(message, tag = "1")]
        RunRequest(super::AgentRunRequest),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
struct AgentRunRequest {
    #[prost(message, optional, tag = "1")]
    conversation_state: Option<ConversationStateStructure>,
    #[prost(message, optional, tag = "2")]
    action: Option<ConversationAction>,
    #[prost(message, optional, tag = "3")]
    model_details: Option<ModelDetails>,
    #[prost(message, optional, tag = "4")]
    mcp_tools: Option<McpTools>,
    #[prost(string, optional, tag = "5")]
    conversation_id: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ConversationAction {
    #[prost(oneof = "conversation_action::Action", tags = "1")]
    action: Option<conversation_action::Action>,
}

mod conversation_action {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub(super) enum Action {
        #[prost(message, tag = "1")]
        UserMessageAction(super::UserMessageAction),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
struct UserMessageAction {
    #[prost(message, optional, tag = "1")]
    user_message: Option<UserMessage>,
    #[prost(message, optional, tag = "2")]
    request_context: Option<RequestContext>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct UserMessage {
    #[prost(string, tag = "1")]
    text: String,
    #[prost(string, tag = "2")]
    message_id: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct RequestContext {}

#[derive(Clone, PartialEq, prost::Message)]
struct ModelDetails {
    #[prost(string, tag = "1")]
    model_id: String,
    #[prost(string, tag = "3")]
    display_model_id: String,
    #[prost(string, tag = "4")]
    display_name: String,
    #[prost(string, tag = "5")]
    display_name_short: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct McpTools {
    #[prost(message, repeated, tag = "1")]
    mcp_tools: Vec<McpToolDefinition>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct McpToolDefinition {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(string, tag = "2")]
    description: String,
    #[prost(bytes = "vec", tag = "3")]
    input_schema: Vec<u8>,
    #[prost(string, tag = "4")]
    provider_identifier: String,
    #[prost(string, tag = "5")]
    tool_name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct AgentServerMessage {
    #[prost(oneof = "agent_server_message::Message", tags = "1, 2, 3")]
    message: Option<agent_server_message::Message>,
}

mod agent_server_message {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub(super) enum Message {
        #[prost(message, tag = "1")]
        InteractionUpdate(super::InteractionUpdate),
        #[prost(message, tag = "2")]
        ExecServerMessage(super::ExecServerMessage),
        #[prost(message, tag = "3")]
        ConversationCheckpointUpdate(super::ConversationStateStructure),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
struct ExecServerMessage {}

#[derive(Clone, PartialEq, prost::Message)]
struct ConversationStateStructure {
    #[prost(message, optional, tag = "5")]
    token_details: Option<ConversationTokenDetails>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ConversationTokenDetails {
    #[prost(uint32, tag = "1")]
    used_tokens: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
struct InteractionUpdate {
    #[prost(oneof = "interaction_update::Message", tags = "1, 2, 3, 4, 7, 8, 14")]
    message: Option<interaction_update::Message>,
}

mod interaction_update {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub(super) enum Message {
        #[prost(message, tag = "1")]
        TextDelta(super::TextDeltaUpdate),
        #[prost(message, tag = "2")]
        ToolCallStarted(super::ToolCallStartedUpdate),
        #[prost(message, tag = "3")]
        ToolCallCompleted(super::ToolCallCompletedUpdate),
        #[prost(message, tag = "4")]
        ThinkingDelta(super::ThinkingDeltaUpdate),
        #[prost(message, tag = "7")]
        PartialToolCall(super::PartialToolCallUpdate),
        #[prost(message, tag = "8")]
        TokenDelta(super::TokenDeltaUpdate),
        #[prost(message, tag = "14")]
        TurnEnded(super::TurnEndedUpdate),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
struct TextDeltaUpdate {
    #[prost(string, tag = "1")]
    text: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ThinkingDeltaUpdate {
    #[prost(string, tag = "1")]
    text: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ToolCallStartedUpdate {
    #[prost(string, tag = "1")]
    call_id: String,
    #[prost(message, optional, tag = "2")]
    tool_call: Option<ToolCall>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct PartialToolCallUpdate {
    #[prost(string, tag = "1")]
    call_id: String,
    #[prost(message, optional, tag = "2")]
    tool_call: Option<ToolCall>,
    #[prost(string, tag = "3")]
    args_text_delta: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ToolCallCompletedUpdate {
    #[prost(string, tag = "1")]
    call_id: String,
    #[prost(message, optional, tag = "2")]
    tool_call: Option<ToolCall>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ToolCall {
    #[prost(oneof = "tool_call::Tool", tags = "15")]
    tool: Option<tool_call::Tool>,
}

mod tool_call {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub(super) enum Tool {
        #[prost(message, tag = "15")]
        McpToolCall(super::McpToolCall),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
struct McpToolCall {
    #[prost(message, optional, tag = "1")]
    args: Option<McpArgs>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct McpArgs {
    #[prost(string, tag = "1")]
    name: String,
    #[prost(map = "string, bytes", tag = "2")]
    args: HashMap<String, Vec<u8>>,
    #[prost(string, tag = "3")]
    tool_call_id: String,
    #[prost(string, tag = "4")]
    provider_identifier: String,
    #[prost(string, tag = "5")]
    tool_name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
struct TokenDeltaUpdate {
    #[prost(int32, tag = "1")]
    tokens: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
struct TurnEndedUpdate {}

#[derive(Clone, PartialEq, prost::Message)]
struct GoogleValue {
    #[prost(oneof = "google_value::Kind", tags = "1, 2, 3, 4, 5, 6")]
    kind: Option<google_value::Kind>,
}

mod google_value {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub(super) enum Kind {
        #[prost(enumeration = "super::NullValue", tag = "1")]
        Null(i32),
        #[prost(double, tag = "2")]
        Number(f64),
        #[prost(string, tag = "3")]
        String(String),
        #[prost(bool, tag = "4")]
        Bool(bool),
        #[prost(message, tag = "5")]
        Struct(super::StructValue),
        #[prost(message, tag = "6")]
        List(super::ListValue),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
enum NullValue {
    NullValue = 0,
}

#[derive(Clone, PartialEq, prost::Message)]
struct StructValue {
    #[prost(btree_map = "string, message", tag = "1")]
    fields: BTreeMap<String, GoogleValue>,
}

#[derive(Clone, PartialEq, prost::Message)]
struct ListValue {
    #[prost(message, repeated, tag = "1")]
    values: Vec<GoogleValue>,
}
