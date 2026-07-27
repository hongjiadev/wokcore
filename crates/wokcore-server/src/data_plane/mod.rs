mod admission_body;
mod anthropic;
mod body;
mod chat;
mod execute;
mod images;
mod models;
mod models_endpoint;
mod registry;
mod response;
mod responses;
mod routes;
mod stream;

#[derive(Clone, Debug)]
pub(crate) struct RequestObservationContext {
    pub(crate) attempt_id: Option<String>,
    pub(crate) provider_id: String,
    pub(crate) model: String,
    pub(crate) input_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
}

pub(crate) use admission_body::hold_admission_until_body_end;
pub(crate) use body::{IMAGE_MULTIPART_WIRE_LIMIT, JSON_BODY_LIMIT};
pub(crate) use execute::{Executed, ExecutedResponse, ExecutedStream, execute_canonical};
pub use execute::{
    InvalidSafeUpstreamRequestId, InvalidUpstreamExecutionResponse, InvalidUpstreamExecutionStream,
    SafeUpstreamRequestId, UPSTREAM_STREAM_CHANNEL_CAPACITY, UpstreamExecutionFailure,
    UpstreamExecutionOutput, UpstreamExecutionRequest, UpstreamExecutionResponse,
    UpstreamExecutionResult, UpstreamExecutionStream, UpstreamExecutor, UpstreamFailureKind,
    UpstreamFinishReason, UpstreamOperation, UpstreamStreamSendError, UpstreamStreamSender,
};
pub use images::{
    ImageEditRequest, ImageExecutionInput, ImageExecutionRequest, ImageExecutionResponse,
    ImageExecutionResult, ImageInputFile, ImageInputReader,
};
pub use models::{DataPlaneRequest, DataPlaneRequestSummary};
pub(crate) use models_endpoint::models as models_endpoint;
pub(crate) use registry::is_json_content_type;
pub use registry::{ClientProtocol, DataPlaneRequestError, ProtocolRegistry, RequestBodyKind};
pub(crate) use response::public_error_response;
pub(crate) use routes::{anthropic, chat, count_tokens, images_edit, images_generation, responses};
