mod admission_body;
mod anthropic;
mod body;
mod chat;
mod execute;
mod models;
mod models_endpoint;
mod registry;
mod response;
mod responses;
mod routes;

pub(crate) use admission_body::hold_admission_until_body_end;
pub(crate) use body::{IMAGE_MULTIPART_BODY_LIMIT, JSON_BODY_LIMIT};
pub(crate) use execute::{ExecutedResponse, execute_canonical};
pub use execute::{
    InvalidSafeUpstreamRequestId, InvalidUpstreamExecutionResponse, SafeUpstreamRequestId,
    UpstreamExecutionFailure, UpstreamExecutionOutput, UpstreamExecutionRequest,
    UpstreamExecutionResponse, UpstreamExecutionResult, UpstreamExecutor, UpstreamFailureKind,
    UpstreamFinishReason, UpstreamOperation,
};
pub use models::{DataPlaneRequest, DataPlaneRequestSummary};
pub(crate) use models_endpoint::models as models_endpoint;
pub(crate) use registry::is_json_content_type;
pub use registry::{ClientProtocol, DataPlaneRequestError, ProtocolRegistry, RequestBodyKind};
pub(crate) use response::public_error_response;
pub(crate) use routes::{
    anthropic, chat, count_tokens, responses, unsupported_json, unsupported_multipart,
};
