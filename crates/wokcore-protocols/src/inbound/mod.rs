use crate::canonical::{CanonicalRequest, GatewayError, InputItem, RequestId, ToolDefinition};

mod anthropic;
mod chat;
mod responses;

pub(crate) use anthropic::REQUEST_EXTENSION_KEY;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InboundCodecV1 {
    OpenAiResponses,
    OpenAiChatCompletions,
    AnthropicMessages,
    AnthropicCountTokens,
}

impl InboundCodecV1 {
    pub fn decode(
        self,
        request_id: RequestId,
        json: &[u8],
        limits: InboundLimitsV1,
    ) -> Result<CanonicalRequest, GatewayError> {
        if json.len() > limits.max_body_bytes {
            return Err(GatewayError::invalid_request());
        }

        let request = match self {
            Self::OpenAiResponses => crate::ResponsesCodec::decode_request(request_id, json),
            Self::OpenAiChatCompletions => crate::ChatCodec::decode_request(request_id, json),
            Self::AnthropicMessages => crate::AnthropicCodec::decode_message(request_id, json),
            Self::AnthropicCountTokens => {
                crate::AnthropicCodec::decode_count_tokens(request_id, json)
            }
        }?;
        validate_canonical_request(&request, limits)?;
        Ok(request)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InboundLimitsV1 {
    pub max_body_bytes: usize,
    pub max_identifier_bytes: usize,
    pub max_input_items: usize,
    pub max_tools: usize,
    pub max_images: usize,
    pub max_extension_fields: usize,
    pub max_retained_value_bytes: usize,
}

impl Default for InboundLimitsV1 {
    fn default() -> Self {
        Self {
            max_body_bytes: 16 * 1024 * 1024,
            max_identifier_bytes: 512,
            max_input_items: 16_384,
            max_tools: 4_096,
            max_images: 4_096,
            max_extension_fields: 65_536,
            max_retained_value_bytes: 16 * 1024 * 1024,
        }
    }
}

fn validate_canonical_request(
    request: &CanonicalRequest,
    limits: InboundLimitsV1,
) -> Result<(), GatewayError> {
    validate_identifier(request.request_id.as_str(), limits)?;
    validate_identifier(request.model.as_str(), limits)?;
    if let Some(thread_key) = &request.thread_key {
        validate_identifier(thread_key.as_str(), limits)?;
    }
    if request.input.len() > limits.max_input_items || request.tools.len() > limits.max_tools {
        return Err(GatewayError::invalid_request());
    }

    let mut image_count = 0_usize;
    for item in &request.input {
        match item {
            InputItem::Text { text } => validate_retained_text(text, limits)?,
            InputItem::ImageUrl { url, .. } => {
                image_count = image_count
                    .checked_add(1)
                    .ok_or_else(GatewayError::invalid_request)?;
                validate_retained_text(url.as_str(), limits)?;
            }
            InputItem::ToolResult { call_id, output } => {
                validate_identifier(call_id, limits)?;
                validate_retained_value(output, limits)?;
            }
        }
    }
    if image_count > limits.max_images {
        return Err(GatewayError::invalid_request());
    }

    let mut extension_fields = validate_extensions(&request.extensions, limits)?;
    for tool in &request.tools {
        validate_tool(tool, limits)?;
        extension_fields = extension_fields
            .checked_add(validate_extensions(&tool.extensions, limits)?)
            .ok_or_else(GatewayError::invalid_request)?;
    }
    if let Some(reasoning) = &request.reasoning {
        if let Some(effort) = &reasoning.effort {
            validate_identifier(effort, limits)?;
        }
        extension_fields = extension_fields
            .checked_add(validate_extensions(&reasoning.extensions, limits)?)
            .ok_or_else(GatewayError::invalid_request)?;
    }
    if extension_fields > limits.max_extension_fields {
        return Err(GatewayError::invalid_request());
    }
    Ok(())
}

fn validate_tool(tool: &ToolDefinition, limits: InboundLimitsV1) -> Result<(), GatewayError> {
    validate_identifier(&tool.name, limits)?;
    if let Some(description) = &tool.description {
        validate_retained_text(description, limits)?;
    }
    validate_retained_value(&tool.input_schema, limits)
}

fn validate_extensions(
    extensions: &std::collections::BTreeMap<String, serde_json::Value>,
    limits: InboundLimitsV1,
) -> Result<usize, GatewayError> {
    let mut fields = 0_usize;
    for (key, value) in extensions {
        validate_extension_key(key, limits)?;
        validate_retained_value(value, limits)?;
        fields = fields
            .checked_add(1)
            .and_then(|fields| {
                validate_extension_tree(value, limits).and_then(|nested| fields.checked_add(nested))
            })
            .ok_or_else(GatewayError::invalid_request)?;
    }
    Ok(fields)
}

fn validate_extension_tree(value: &serde_json::Value, limits: InboundLimitsV1) -> Option<usize> {
    match value {
        serde_json::Value::Array(values) => values.iter().try_fold(0_usize, |fields, value| {
            fields.checked_add(validate_extension_tree(value, limits)?)
        }),
        serde_json::Value::Object(object) => {
            object.iter().try_fold(0_usize, |fields, (key, value)| {
                validate_extension_key(key, limits).ok()?;
                if is_semantic_identifier_key(key)
                    && let Some(identifier) = value.as_str()
                {
                    validate_identifier(identifier, limits).ok()?;
                }
                fields
                    .checked_add(1)?
                    .checked_add(validate_extension_tree(value, limits)?)
            })
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => Some(0),
    }
}

fn validate_extension_key(key: &str, limits: InboundLimitsV1) -> Result<(), GatewayError> {
    if key.is_empty() || key.len() > limits.max_retained_value_bytes {
        Err(GatewayError::invalid_request())
    } else {
        Ok(())
    }
}

fn is_semantic_identifier_key(key: &str) -> bool {
    matches!(key, "id" | "model" | "name") || key.ends_with("_id")
}

fn validate_identifier(value: &str, limits: InboundLimitsV1) -> Result<(), GatewayError> {
    if value.is_empty() || value.len() > limits.max_identifier_bytes {
        Err(GatewayError::invalid_request())
    } else {
        Ok(())
    }
}

fn validate_retained_text(value: &str, limits: InboundLimitsV1) -> Result<(), GatewayError> {
    if value.len() > limits.max_retained_value_bytes {
        Err(GatewayError::invalid_request())
    } else {
        Ok(())
    }
}

fn validate_retained_value(
    value: &serde_json::Value,
    limits: InboundLimitsV1,
) -> Result<(), GatewayError> {
    if serde_json::to_vec(value)
        .is_ok_and(|encoded| encoded.len() <= limits.max_retained_value_bytes)
    {
        Ok(())
    } else {
        Err(GatewayError::invalid_request())
    }
}
