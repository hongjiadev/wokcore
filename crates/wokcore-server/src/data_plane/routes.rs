use axum::{Extension, response::Response};

use crate::api::RequestId;

use super::{
    ClientProtocol,
    body::{ValidatedEmptyBody, ValidatedImageEditBody, ValidatedJsonBody},
    response::unsupported_capability,
};

pub(crate) async fn unsupported_json(
    Extension(request_id): Extension<RequestId>,
    Extension(protocol): Extension<ClientProtocol>,
    _body: ValidatedJsonBody,
) -> Response {
    unsupported_capability(request_id, protocol)
}

pub(crate) async fn unsupported_models(
    Extension(request_id): Extension<RequestId>,
    Extension(protocol): Extension<ClientProtocol>,
    _body: ValidatedEmptyBody,
) -> Response {
    unsupported_capability(request_id, protocol)
}

pub(crate) async fn unsupported_multipart(
    Extension(request_id): Extension<RequestId>,
    Extension(protocol): Extension<ClientProtocol>,
    _body: ValidatedImageEditBody,
) -> Response {
    unsupported_capability(request_id, protocol)
}
