pub mod canonical;
pub mod images;
pub mod inbound;
pub mod stream;
pub mod upstream;

mod outbound;

pub use inbound::{InboundCodecV1, InboundLimitsV1};
pub use outbound::{
    AnthropicCodec, AnthropicEncodeContext, AnthropicResponseTemplate, AnthropicStopReason,
    AnthropicTokenCount, AzureAdapter, AzureConfig, AzureStreamDecoder, ChatCodec,
    ChatEncodeContext, ChatFinishReason, ChatResponseTemplate, CursorAdapter, CursorConfig,
    GeminiAdapter, GeminiConfig, GeminiStreamDecoder, ResponsesCodec, ResponsesEncodeContext,
    ResponsesResponseTemplate, TokenCounter, UpstreamLimits, UpstreamRequest,
};

/// Canonical extension containing validated Anthropic blocks that have no
/// lossless `InputItem` representation. The value is an ordered array of
/// `{message_index, block_index, role, block}` records.
pub const ANTHROPIC_KNOWN_BLOCKS_EXTENSION_KEY: &str = "anthropic.known_blocks";

pub const IMPLEMENTED_PROVIDER_PROTOCOLS: &[&str] = &[
    "anthropic.messages.v1",
    "azure.openai.v1",
    "cursor.connect.v1",
    "google.gemini.v1",
    "openai.chat_completions.v1",
    "openai.responses.v1",
];

pub(crate) fn valid_chat_function_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}
