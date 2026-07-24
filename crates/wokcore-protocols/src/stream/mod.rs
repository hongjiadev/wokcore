mod channel;
mod sse;

pub use channel::{EventReceiver, ReceiveError, bounded_event_channel};
pub use sse::{DEFAULT_MAX_SSE_FRAME_BYTES, SseDecoder, SseFrame, encode_sse};

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ProtocolError {
    #[error("SSE frame exceeds the {limit}-byte limit")]
    FrameTooLarge { limit: usize },
    #[error("SSE field is not valid UTF-8")]
    InvalidUtf8,
    #[error("SSE decoder cannot be reused after an error")]
    DecoderFailed,
    #[error("SSE stream ended with an unterminated frame")]
    UnexpectedEof,
    #[error("event channel capacity must be greater than zero")]
    InvalidChannelCapacity,
}
