use wokcore_protocols::{InboundCodecV1, InboundLimitsV1, canonical::RequestId};

use super::DataPlaneRequest;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientProtocol {
    OpenAiResponses,
    OpenAiChatCompletions,
    AnthropicMessages,
    AnthropicCountTokens,
    OpenAiModels,
    OpenAiImageGenerations,
    OpenAiImageEdits,
}

impl ClientProtocol {
    pub fn expects_json_body(self) -> bool {
        self.request_body_kind() == RequestBodyKind::Json
    }

    pub fn request_body_kind(self) -> RequestBodyKind {
        match self {
            Self::OpenAiModels => RequestBodyKind::None,
            Self::OpenAiImageEdits => RequestBodyKind::MultipartFormData,
            Self::OpenAiResponses
            | Self::OpenAiChatCompletions
            | Self::AnthropicMessages
            | Self::AnthropicCountTokens
            | Self::OpenAiImageGenerations => RequestBodyKind::Json,
        }
    }

    pub fn is_anthropic(self) -> bool {
        matches!(self, Self::AnthropicMessages | Self::AnthropicCountTokens)
    }

    pub fn inbound_codec(self) -> Option<InboundCodecV1> {
        match self {
            Self::OpenAiResponses => Some(InboundCodecV1::OpenAiResponses),
            Self::OpenAiChatCompletions => Some(InboundCodecV1::OpenAiChatCompletions),
            Self::AnthropicMessages => Some(InboundCodecV1::AnthropicMessages),
            Self::AnthropicCountTokens => Some(InboundCodecV1::AnthropicCountTokens),
            Self::OpenAiModels | Self::OpenAiImageGenerations | Self::OpenAiImageEdits => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestBodyKind {
    None,
    Json,
    MultipartFormData,
}

pub struct ProtocolRegistry;

impl ProtocolRegistry {
    pub fn resolve(path: &str) -> Option<ClientProtocol> {
        match path {
            "/v1/responses" => Some(ClientProtocol::OpenAiResponses),
            "/v1/chat/completions" => Some(ClientProtocol::OpenAiChatCompletions),
            "/v1/messages" => Some(ClientProtocol::AnthropicMessages),
            "/v1/messages/count_tokens" => Some(ClientProtocol::AnthropicCountTokens),
            "/v1/models" => Some(ClientProtocol::OpenAiModels),
            "/v1/images/generations" => Some(ClientProtocol::OpenAiImageGenerations),
            "/v1/images/edits" => Some(ClientProtocol::OpenAiImageEdits),
            _ => None,
        }
    }

    pub fn decode_json(
        path: &str,
        request_id: RequestId,
        content_type: Option<&str>,
        body: &[u8],
    ) -> Result<DataPlaneRequest, DataPlaneRequestError> {
        let protocol = Self::resolve(path).ok_or(DataPlaneRequestError::UnsupportedProtocol)?;
        let codec = protocol
            .inbound_codec()
            .ok_or(DataPlaneRequestError::UnsupportedProtocol)?;
        if !is_json_content_type(content_type) {
            return Err(DataPlaneRequestError::UnsupportedMediaType);
        }
        let canonical = codec
            .decode(request_id, body, InboundLimitsV1::default())
            .map_err(|error| {
                if error.code() == "unsupported_capability" {
                    DataPlaneRequestError::UnsupportedCapability
                } else {
                    DataPlaneRequestError::InvalidBody
                }
            })?;
        Ok(DataPlaneRequest::new(protocol, canonical))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataPlaneRequestError {
    UnsupportedProtocol,
    UnsupportedMediaType,
    InvalidBody,
    UnsupportedCapability,
}

impl DataPlaneRequestError {
    pub fn code(self) -> &'static str {
        match self {
            Self::UnsupportedProtocol => "unsupported_protocol",
            Self::UnsupportedMediaType => "unsupported_media_type",
            Self::InvalidBody => "invalid_body",
            Self::UnsupportedCapability => "unsupported_capability",
        }
    }
}

pub(crate) fn is_json_content_type(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|content_type| {
        matches!(
            content_type.trim().to_ascii_lowercase().as_str(),
            "application/json" | "application/json; charset=utf-8"
        )
    })
}
