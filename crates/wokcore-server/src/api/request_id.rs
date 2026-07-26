use std::fmt;

use axum::{
    extract::{Request, State},
    http::{
        HeaderValue,
        header::{CACHE_CONTROL, HeaderName},
    },
    middleware::Next,
    response::{IntoResponse, Response},
};
use uuid::Uuid;

use crate::{ServerState, runtime::generate_uuid_v4};

use super::error::ApiError;

const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");
const CONTENT_TYPE_OPTIONS_HEADER: HeaderName = HeaderName::from_static("x-content-type-options");

#[derive(Clone, Copy)]
pub(crate) struct RequestId(Uuid);

impl RequestId {
    fn entropy_failure() -> Self {
        Self(Uuid::nil())
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

pub(crate) async fn apply_response_envelope(
    State(state): State<ServerState>,
    mut request: Request,
    next: Next,
) -> Response {
    let request_id = match generate_uuid_v4(state.request_id_entropy.as_ref()) {
        Ok(uuid) => RequestId(uuid),
        Err(_) => {
            return apply_headers(
                ApiError::internal_failure(RequestId::entropy_failure()).into_response(),
                RequestId::entropy_failure(),
            );
        }
    };
    request.headers_mut().remove(&REQUEST_ID_HEADER);
    request.extensions_mut().insert(request_id);
    apply_headers(next.run(request).await, request_id)
}

fn apply_headers(mut response: Response, request_id: RequestId) -> Response {
    response.headers_mut().insert(
        REQUEST_ID_HEADER,
        HeaderValue::from_str(&request_id.to_string()).expect("UUID is a valid header value"),
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        CONTENT_TYPE_OPTIONS_HEADER,
        HeaderValue::from_static("nosniff"),
    );
    response
}
