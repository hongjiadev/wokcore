use std::collections::BTreeMap;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use url::Url;

use crate::{
    ResponsesCodec,
    canonical::{
        CanonicalRequest, GatewayError, ImageDetail, InputItem, PublicModelId, ReasoningOptions,
        RequestId, ThreadKey, ToolDefinition,
    },
};

const INPUT_EXTENSIONS_KEY: &str = "responses.input_extensions";

#[derive(Deserialize)]
struct ResponsesRequest {
    model: String,
    input: ResponsesInput,
    #[serde(default)]
    tools: Vec<ResponsesTool>,
    #[serde(default)]
    stream: bool,
    reasoning: Option<ResponsesReasoning>,
    previous_response_id: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ResponsesInput {
    Text(String),
    Items(Vec<ResponsesInputItem>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ResponsesInputItem {
    Message(ResponsesMessage),
    Text(ResponsesTextItem),
    Image(ResponsesImageItem),
    ToolResult(ResponsesToolResult),
}

#[derive(Deserialize)]
struct ResponsesMessage {
    #[serde(rename = "type")]
    kind: Option<String>,
    role: ResponsesRole,
    content: ResponsesMessageContent,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ResponsesRole {
    User,
    Assistant,
    System,
    Developer,
}

impl ResponsesRole {
    fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::System => "system",
            Self::Developer => "developer",
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ResponsesMessageContent {
    Text(String),
    Parts(Vec<ResponsesContentPart>),
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ResponsesContentPart {
    #[serde(rename = "input_text")]
    Text {
        text: String,
        #[serde(flatten)]
        extra: BTreeMap<String, Value>,
    },
    #[serde(rename = "input_image")]
    Image {
        image_url: String,
        detail: Option<ImageDetail>,
        #[serde(flatten)]
        extra: BTreeMap<String, Value>,
    },
}

#[derive(Deserialize)]
struct ResponsesTextItem {
    #[serde(rename = "type")]
    kind: String,
    text: String,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct ResponsesImageItem {
    #[serde(rename = "type")]
    kind: String,
    image_url: String,
    detail: Option<ImageDetail>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct ResponsesToolResult {
    #[serde(rename = "type")]
    kind: String,
    call_id: String,
    output: Value,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct ResponsesTool {
    #[serde(rename = "type")]
    kind: String,
    name: String,
    description: Option<String>,
    parameters: Value,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct ResponsesReasoning {
    effort: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl ResponsesCodec {
    pub fn decode_request(
        request_id: RequestId,
        json: &[u8],
    ) -> Result<CanonicalRequest, GatewayError> {
        let wire: ResponsesRequest =
            serde_json::from_slice(json).map_err(|_| GatewayError::invalid_request())?;
        validate_non_empty(&wire.model)?;

        let (input, input_extensions) = decode_input(wire.input)?;
        let tools = wire
            .tools
            .into_iter()
            .map(decode_tool)
            .collect::<Result<Vec<_>, _>>()?;
        let reasoning = wire.reasoning.map(|reasoning| ReasoningOptions {
            effort: reasoning.effort,
            extensions: reasoning.extra,
        });
        let thread_key = wire
            .previous_response_id
            .map(|value| {
                validate_non_empty(&value)?;
                Ok(ThreadKey::new(value))
            })
            .transpose()?;
        let mut extensions = wire.extra;
        if !input_extensions.is_empty() {
            insert_extension_without_collision(
                &mut extensions,
                INPUT_EXTENSIONS_KEY,
                Value::Array(input_extensions),
            );
        }

        Ok(CanonicalRequest {
            request_id,
            model: PublicModelId::new(wire.model),
            thread_key,
            input,
            tools,
            stream: wire.stream,
            reasoning,
            extensions,
        })
    }
}

fn decode_input(wire: ResponsesInput) -> Result<(Vec<InputItem>, Vec<Value>), GatewayError> {
    match wire {
        ResponsesInput::Text(text) => Ok((vec![InputItem::Text { text }], Vec::new())),
        ResponsesInput::Items(items) => {
            let mut input = Vec::new();
            let mut extensions = Vec::new();
            for (index, item) in items.into_iter().enumerate() {
                decode_input_item(item, index, &mut input, &mut extensions)?;
            }
            Ok((input, extensions))
        }
    }
}

fn decode_input_item(
    wire: ResponsesInputItem,
    index: usize,
    input: &mut Vec<InputItem>,
    extensions: &mut Vec<Value>,
) -> Result<(), GatewayError> {
    match wire {
        ResponsesInputItem::Message(message) => {
            if message
                .kind
                .as_deref()
                .is_some_and(|kind| kind != "message")
            {
                return Err(GatewayError::invalid_request());
            }
            let mut message_extension = Map::new();
            message_extension.insert("role".to_owned(), json!(message.role.as_str()));
            if !message.extra.is_empty() {
                message_extension.insert("message".to_owned(), map_value(message.extra));
            }
            let content_extensions = decode_message_content(message.content, input)?;
            if !content_extensions.is_empty() {
                message_extension.insert("content".to_owned(), Value::Array(content_extensions));
            }
            if !message_extension.is_empty() {
                message_extension.insert("index".to_owned(), json!(index));
                extensions.push(Value::Object(message_extension));
            }
        }
        ResponsesInputItem::Text(text) => {
            if text.kind != "input_text" {
                return Err(GatewayError::invalid_request());
            }
            input.push(InputItem::Text { text: text.text });
            push_item_extension(extensions, index, text.extra);
        }
        ResponsesInputItem::Image(image) => {
            if image.kind != "input_image" {
                return Err(GatewayError::invalid_request());
            }
            input.push(InputItem::ImageUrl {
                url: decode_image_url(&image.image_url)?,
                detail: image.detail,
            });
            push_item_extension(extensions, index, image.extra);
        }
        ResponsesInputItem::ToolResult(result) => {
            if result.kind != "function_call_output" || result.call_id.is_empty() {
                return Err(GatewayError::invalid_request());
            }
            input.push(InputItem::ToolResult {
                call_id: result.call_id,
                output: result.output,
            });
            push_item_extension(extensions, index, result.extra);
        }
    }
    Ok(())
}

fn decode_message_content(
    wire: ResponsesMessageContent,
    input: &mut Vec<InputItem>,
) -> Result<Vec<Value>, GatewayError> {
    match wire {
        ResponsesMessageContent::Text(text) => {
            input.push(InputItem::Text { text });
            Ok(Vec::new())
        }
        ResponsesMessageContent::Parts(parts) => {
            let mut extensions = Vec::new();
            for (index, part) in parts.into_iter().enumerate() {
                let extra = match part {
                    ResponsesContentPart::Text { text, extra } => {
                        input.push(InputItem::Text { text });
                        extra
                    }
                    ResponsesContentPart::Image {
                        image_url,
                        detail,
                        extra,
                    } => {
                        input.push(InputItem::ImageUrl {
                            url: decode_image_url(&image_url)?,
                            detail,
                        });
                        extra
                    }
                };
                if !extra.is_empty() {
                    extensions.push(json!({
                        "index": index,
                        "extra": extra,
                    }));
                }
            }
            Ok(extensions)
        }
    }
}

fn decode_tool(wire: ResponsesTool) -> Result<ToolDefinition, GatewayError> {
    if wire.kind != "function" || wire.name.is_empty() || !wire.parameters.is_object() {
        return Err(GatewayError::invalid_request());
    }
    Ok(ToolDefinition {
        name: wire.name,
        description: wire.description,
        input_schema: wire.parameters,
        extensions: wire.extra,
    })
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

fn push_item_extension(extensions: &mut Vec<Value>, index: usize, extra: BTreeMap<String, Value>) {
    if !extra.is_empty() {
        extensions.push(json!({
            "index": index,
            "extra": extra,
        }));
    }
}

fn map_value(map: BTreeMap<String, Value>) -> Value {
    Value::Object(map.into_iter().collect())
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
