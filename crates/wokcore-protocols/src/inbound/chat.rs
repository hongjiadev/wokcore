use std::collections::{BTreeMap, BTreeSet};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use url::Url;

use crate::{
    ChatCodec,
    canonical::{
        CanonicalRequest, GatewayError, ImageDetail, InputItem, PublicModelId, ReasoningOptions,
        RequestId, ToolDefinition,
    },
    valid_chat_function_name,
};

const MESSAGES_EXTENSION_KEY: &str = "chat.messages";
const PARALLEL_TOOLS_EXTENSION_KEY: &str = "chat.parallel_tool_calls";
const STREAM_OPTIONS_EXTENSION_KEY: &str = "chat.stream_options";
const TOOL_WRAPPER_EXTENSION_KEY: &str = "chat.wrapper";

#[derive(Deserialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(default)]
    tools: Vec<ChatTool>,
    parallel_tool_calls: Option<bool>,
    #[serde(default)]
    stream: bool,
    stream_options: Option<ChatStreamOptions>,
    reasoning_effort: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct ChatStreamOptions {
    include_usage: Option<bool>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(tag = "role")]
enum ChatMessage {
    #[serde(rename = "system")]
    System {
        content: ChatTextContent,
        name: Option<String>,
        #[serde(flatten)]
        extra: BTreeMap<String, Value>,
    },
    #[serde(rename = "developer")]
    Developer {
        content: ChatTextContent,
        name: Option<String>,
        #[serde(flatten)]
        extra: BTreeMap<String, Value>,
    },
    #[serde(rename = "user")]
    User {
        content: ChatUserContent,
        name: Option<String>,
        #[serde(flatten)]
        extra: BTreeMap<String, Value>,
    },
    #[serde(rename = "assistant")]
    Assistant {
        content: Option<ChatAssistantContent>,
        #[serde(default)]
        tool_calls: Vec<ChatToolCall>,
        name: Option<String>,
        refusal: Option<String>,
        #[serde(flatten)]
        extra: BTreeMap<String, Value>,
    },
    #[serde(rename = "tool")]
    Tool {
        content: ChatTextContent,
        tool_call_id: String,
        #[serde(flatten)]
        extra: BTreeMap<String, Value>,
    },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ChatTextContent {
    Text(String),
    Parts(Vec<ChatTextPart>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ChatUserContent {
    Text(String),
    Parts(Vec<ChatUserPart>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ChatAssistantContent {
    Text(String),
    Parts(Vec<ChatAssistantPart>),
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ChatTextPart {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(flatten)]
        extra: BTreeMap<String, Value>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ChatUserPart {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(flatten)]
        extra: BTreeMap<String, Value>,
    },
    #[serde(rename = "image_url")]
    Image {
        image_url: ChatImageUrl,
        #[serde(flatten)]
        extra: BTreeMap<String, Value>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ChatAssistantPart {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(flatten)]
        extra: BTreeMap<String, Value>,
    },
    #[serde(rename = "refusal")]
    Refusal {
        refusal: String,
        #[serde(flatten)]
        extra: BTreeMap<String, Value>,
    },
}

#[derive(Deserialize)]
struct ChatImageUrl {
    url: String,
    detail: Option<ImageDetail>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct ChatToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: ChatCalledFunction,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct ChatCalledFunction {
    name: String,
    arguments: String,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct ChatTool {
    #[serde(rename = "type")]
    kind: String,
    function: ChatFunction,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct ChatFunction {
    name: String,
    description: Option<String>,
    parameters: Option<Value>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl ChatCodec {
    pub fn decode_request(
        request_id: RequestId,
        json: &[u8],
    ) -> Result<CanonicalRequest, GatewayError> {
        let wire: ChatRequest =
            serde_json::from_slice(json).map_err(|_| GatewayError::invalid_request())?;
        validate_non_empty(&wire.model)?;
        if wire.messages.is_empty() || (wire.stream_options.is_some() && !wire.stream) {
            return Err(GatewayError::invalid_request());
        }

        let (input, message_extensions) = decode_messages(wire.messages)?;
        let tools = decode_tools(wire.tools)?;
        let reasoning = wire
            .reasoning_effort
            .map(|effort| {
                validate_non_empty(&effort)?;
                Ok(ReasoningOptions {
                    effort: Some(effort),
                    extensions: BTreeMap::new(),
                })
            })
            .transpose()?;
        let mut extensions = wire.extra;
        insert_extension_without_collision(
            &mut extensions,
            MESSAGES_EXTENSION_KEY,
            Value::Array(message_extensions),
        );
        if let Some(parallel_tool_calls) = wire.parallel_tool_calls {
            insert_extension_without_collision(
                &mut extensions,
                PARALLEL_TOOLS_EXTENSION_KEY,
                json!(parallel_tool_calls),
            );
        }
        if let Some(stream_options) = wire.stream_options {
            insert_extension_without_collision(
                &mut extensions,
                STREAM_OPTIONS_EXTENSION_KEY,
                stream_options_value(stream_options),
            );
        }

        Ok(CanonicalRequest {
            request_id,
            model: PublicModelId::new(wire.model),
            thread_key: None,
            input,
            tools,
            stream: wire.stream,
            reasoning,
            extensions,
        })
    }
}

fn decode_messages(
    messages: Vec<ChatMessage>,
) -> Result<(Vec<InputItem>, Vec<Value>), GatewayError> {
    let mut input = Vec::new();
    let mut extensions = Vec::with_capacity(messages.len());
    for (index, message) in messages.into_iter().enumerate() {
        extensions.push(decode_message(message, index, &mut input)?);
    }
    Ok((input, extensions))
}

fn decode_message(
    message: ChatMessage,
    index: usize,
    input: &mut Vec<InputItem>,
) -> Result<Value, GatewayError> {
    let input_start = input.len();
    let mut extension = Map::new();
    extension.insert("index".to_owned(), json!(index));

    match message {
        ChatMessage::System {
            content,
            name,
            extra,
        } => {
            extension.insert("role".to_owned(), json!("system"));
            decode_text_content(content, input, &mut extension)?;
            insert_name(name, &mut extension)?;
            insert_map("extra", extra, &mut extension);
        }
        ChatMessage::Developer {
            content,
            name,
            extra,
        } => {
            extension.insert("role".to_owned(), json!("developer"));
            decode_text_content(content, input, &mut extension)?;
            insert_name(name, &mut extension)?;
            insert_map("extra", extra, &mut extension);
        }
        ChatMessage::User {
            content,
            name,
            extra,
        } => {
            extension.insert("role".to_owned(), json!("user"));
            decode_user_content(content, input, &mut extension)?;
            insert_name(name, &mut extension)?;
            insert_map("extra", extra, &mut extension);
        }
        ChatMessage::Assistant {
            content,
            tool_calls,
            name,
            refusal,
            extra,
        } => {
            extension.insert("role".to_owned(), json!("assistant"));
            match content {
                Some(content) => decode_assistant_content(content, input, &mut extension)?,
                None => {
                    extension.insert("content_form".to_owned(), json!("null"));
                }
            }
            if let Some(refusal) = refusal {
                validate_non_empty(&refusal)?;
                input.push(InputItem::Text {
                    text: refusal.clone(),
                });
                extension.insert("refusal".to_owned(), json!(refusal));
            }
            if tool_calls.is_empty()
                && input.len() == input_start
                && !extension.contains_key("refusal")
            {
                return Err(GatewayError::invalid_request());
            }
            if !tool_calls.is_empty() {
                extension.insert("tool_calls".to_owned(), decode_tool_calls(tool_calls)?);
            }
            insert_name(name, &mut extension)?;
            insert_map("extra", extra, &mut extension);
        }
        ChatMessage::Tool {
            content,
            tool_call_id,
            extra,
        } => {
            validate_non_empty(&tool_call_id)?;
            extension.insert("role".to_owned(), json!("tool"));
            extension.insert("tool_call_id".to_owned(), json!(tool_call_id));
            let (content_form, output) = tool_content_value(content)?;
            extension.insert("content_form".to_owned(), json!(content_form));
            input.push(InputItem::ToolResult {
                call_id: extension["tool_call_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                output,
            });
            insert_map("extra", extra, &mut extension);
        }
    }

    extension.insert("input_start".to_owned(), json!(input_start));
    extension.insert("input_end".to_owned(), json!(input.len()));
    Ok(Value::Object(extension))
}

fn decode_text_content(
    content: ChatTextContent,
    input: &mut Vec<InputItem>,
    extension: &mut Map<String, Value>,
) -> Result<(), GatewayError> {
    match content {
        ChatTextContent::Text(text) => {
            extension.insert("content_form".to_owned(), json!("text"));
            input.push(InputItem::Text { text });
        }
        ChatTextContent::Parts(parts) => {
            if parts.is_empty() {
                return Err(GatewayError::invalid_request());
            }
            extension.insert("content_form".to_owned(), json!("parts"));
            let mut content_extra = Vec::new();
            for (index, part) in parts.into_iter().enumerate() {
                let ChatTextPart::Text { text, extra } = part;
                input.push(InputItem::Text { text });
                push_part_extra(&mut content_extra, index, extra);
            }
            insert_values("content_extra", content_extra, extension);
        }
    }
    Ok(())
}

fn decode_user_content(
    content: ChatUserContent,
    input: &mut Vec<InputItem>,
    extension: &mut Map<String, Value>,
) -> Result<(), GatewayError> {
    match content {
        ChatUserContent::Text(text) => {
            extension.insert("content_form".to_owned(), json!("text"));
            input.push(InputItem::Text { text });
        }
        ChatUserContent::Parts(parts) => {
            if parts.is_empty() {
                return Err(GatewayError::invalid_request());
            }
            extension.insert("content_form".to_owned(), json!("parts"));
            let mut content_extra = Vec::new();
            for (index, part) in parts.into_iter().enumerate() {
                match part {
                    ChatUserPart::Text { text, extra } => {
                        input.push(InputItem::Text { text });
                        push_part_extra(&mut content_extra, index, extra);
                    }
                    ChatUserPart::Image { image_url, extra } => {
                        input.push(InputItem::ImageUrl {
                            url: decode_image_url(&image_url.url)?,
                            detail: image_url.detail,
                        });
                        let mut combined = extra;
                        if !image_url.extra.is_empty() {
                            combined.insert(
                                "image_url".to_owned(),
                                Value::Object(image_url.extra.into_iter().collect()),
                            );
                        }
                        push_part_extra(&mut content_extra, index, combined);
                    }
                }
            }
            insert_values("content_extra", content_extra, extension);
        }
    }
    Ok(())
}

fn decode_assistant_content(
    content: ChatAssistantContent,
    input: &mut Vec<InputItem>,
    extension: &mut Map<String, Value>,
) -> Result<(), GatewayError> {
    match content {
        ChatAssistantContent::Text(text) => {
            extension.insert("content_form".to_owned(), json!("text"));
            input.push(InputItem::Text { text });
        }
        ChatAssistantContent::Parts(parts) => {
            if parts.is_empty() {
                return Err(GatewayError::invalid_request());
            }
            extension.insert("content_form".to_owned(), json!("parts"));
            let mut content_extra = Vec::new();
            for (index, part) in parts.into_iter().enumerate() {
                match part {
                    ChatAssistantPart::Text { text, extra } => {
                        input.push(InputItem::Text { text });
                        push_part_extra(&mut content_extra, index, extra);
                    }
                    ChatAssistantPart::Refusal { refusal, extra } => {
                        input.push(InputItem::Text { text: refusal });
                        let mut value = Map::from_iter([
                            ("index".to_owned(), json!(index)),
                            ("type".to_owned(), json!("refusal")),
                        ]);
                        if !extra.is_empty() {
                            value.insert(
                                "extra".to_owned(),
                                Value::Object(extra.into_iter().collect()),
                            );
                        }
                        content_extra.push(Value::Object(value));
                    }
                }
            }
            insert_values("content_extra", content_extra, extension);
        }
    }
    Ok(())
}

fn tool_content_value(content: ChatTextContent) -> Result<(&'static str, Value), GatewayError> {
    match content {
        ChatTextContent::Text(text) => Ok(("text", json!(text))),
        ChatTextContent::Parts(parts) => {
            if parts.is_empty() {
                return Err(GatewayError::invalid_request());
            }
            let values = parts
                .into_iter()
                .map(|part| {
                    let ChatTextPart::Text { text, extra } = part;
                    let mut value = Map::from_iter([
                        ("type".to_owned(), json!("text")),
                        ("text".to_owned(), json!(text)),
                    ]);
                    value.extend(extra);
                    Value::Object(value)
                })
                .collect();
            Ok(("parts", Value::Array(values)))
        }
    }
}

fn decode_tool_calls(tool_calls: Vec<ChatToolCall>) -> Result<Value, GatewayError> {
    let mut ids = BTreeSet::new();
    let mut values = Vec::with_capacity(tool_calls.len());
    for tool_call in tool_calls {
        if tool_call.kind != "function"
            || tool_call.id.is_empty()
            || !valid_chat_function_name(&tool_call.function.name)
            || !ids.insert(tool_call.id.clone())
        {
            return Err(GatewayError::invalid_request());
        }
        let mut function = Map::from_iter([
            ("name".to_owned(), json!(tool_call.function.name)),
            ("arguments".to_owned(), json!(tool_call.function.arguments)),
        ]);
        function.extend(tool_call.function.extra);
        let mut value = Map::from_iter([
            ("id".to_owned(), json!(tool_call.id)),
            ("type".to_owned(), json!("function")),
            ("function".to_owned(), Value::Object(function)),
        ]);
        value.extend(tool_call.extra);
        values.push(Value::Object(value));
    }
    Ok(Value::Array(values))
}

fn decode_tools(tools: Vec<ChatTool>) -> Result<Vec<ToolDefinition>, GatewayError> {
    let mut names = BTreeSet::new();
    tools
        .into_iter()
        .map(|tool| {
            let decoded = decode_tool(tool)?;
            if !names.insert(decoded.name.clone()) {
                return Err(GatewayError::invalid_request());
            }
            Ok(decoded)
        })
        .collect()
}

fn decode_tool(wire: ChatTool) -> Result<ToolDefinition, GatewayError> {
    let parameters = wire.function.parameters.unwrap_or_else(|| json!({}));
    if wire.kind != "function"
        || !valid_chat_function_name(&wire.function.name)
        || !parameters.is_object()
    {
        return Err(GatewayError::invalid_request());
    }
    let mut extensions = wire.function.extra;
    if !wire.extra.is_empty() {
        insert_extension_without_collision(
            &mut extensions,
            TOOL_WRAPPER_EXTENSION_KEY,
            Value::Object(wire.extra.into_iter().collect()),
        );
    }
    Ok(ToolDefinition {
        name: wire.function.name,
        description: wire.function.description,
        input_schema: parameters,
        extensions,
    })
}

fn stream_options_value(options: ChatStreamOptions) -> Value {
    let mut value: Map<String, Value> = options.extra.into_iter().collect();
    if let Some(include_usage) = options.include_usage {
        value.insert("include_usage".to_owned(), json!(include_usage));
    }
    Value::Object(value)
}

fn decode_image_url(value: &str) -> Result<Url, GatewayError> {
    let url = Url::parse(value).map_err(|_| GatewayError::invalid_request())?;
    let allowed = match url.scheme() {
        "http" | "https" => true,
        "data" => validate_data_image_url(value),
        _ => false,
    };
    if allowed {
        Ok(url)
    } else {
        Err(GatewayError::invalid_request())
    }
}

fn validate_data_image_url(value: &str) -> bool {
    let Some((metadata, payload)) = value.split_once(',') else {
        return false;
    };
    let Some(metadata) = metadata.get("data:".len()..) else {
        return false;
    };
    let mut fields = metadata.split(';');
    let mime = fields.next().unwrap_or_default();
    let encoding = fields.next();
    if fields.next().is_some()
        || !matches!(
            mime.to_ascii_lowercase().as_str(),
            "image/png" | "image/jpeg" | "image/webp" | "image/gif"
        )
        || !encoding.is_some_and(|value| value.eq_ignore_ascii_case("base64"))
        || payload.is_empty()
    {
        return false;
    }
    STANDARD
        .decode(payload.as_bytes())
        .is_ok_and(|decoded| !decoded.is_empty())
}

fn validate_non_empty(value: &str) -> Result<(), GatewayError> {
    if value.is_empty() {
        Err(GatewayError::invalid_request())
    } else {
        Ok(())
    }
}

fn insert_name(
    name: Option<String>,
    extension: &mut Map<String, Value>,
) -> Result<(), GatewayError> {
    if let Some(name) = name {
        validate_non_empty(&name)?;
        extension.insert("name".to_owned(), json!(name));
    }
    Ok(())
}

fn push_part_extra(values: &mut Vec<Value>, index: usize, extra: BTreeMap<String, Value>) {
    if !extra.is_empty() {
        values.push(json!({"index": index, "extra": extra}));
    }
}

fn insert_values(key: &str, values: Vec<Value>, extension: &mut Map<String, Value>) {
    if !values.is_empty() {
        extension.insert(key.to_owned(), Value::Array(values));
    }
}

fn insert_map(key: &str, map: BTreeMap<String, Value>, extension: &mut Map<String, Value>) {
    if !map.is_empty() {
        extension.insert(key.to_owned(), Value::Object(map.into_iter().collect()));
    }
}

fn insert_extension_without_collision(
    extensions: &mut BTreeMap<String, Value>,
    key: &str,
    value: Value,
) {
    if !extensions.contains_key(key) {
        extensions.insert(key.to_owned(), value);
        return;
    }

    let base = format!("{key}.nested");
    let mut nested_key = base.clone();
    let mut suffix = 2_u64;
    while extensions.contains_key(&nested_key) {
        nested_key = format!("{base}.{suffix}");
        suffix += 1;
    }
    extensions.insert(nested_key, value);
}
