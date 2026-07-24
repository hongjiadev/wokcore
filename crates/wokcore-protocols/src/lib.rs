pub mod canonical;
pub mod stream;

mod inbound;
mod outbound;

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

pub(crate) fn valid_chat_function_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}
