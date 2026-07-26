use std::fmt;

use axum::{
    extract::Request,
    http::{
        HeaderValue,
        header::{CACHE_CONTROL, HeaderName},
    },
    middleware::Next,
    response::Response,
};
use uuid::Uuid;

const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");
const CONTENT_TYPE_OPTIONS_HEADER: HeaderName = HeaderName::from_static("x-content-type-options");

#[derive(Clone, Copy)]
pub(crate) struct RequestId(Uuid);

impl RequestId {
    pub(crate) fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

pub(crate) async fn apply_response_envelope(mut request: Request, next: Next) -> Response {
    let request_id = RequestId::new();
    request.headers_mut().remove(&REQUEST_ID_HEADER);
    request.extensions_mut().insert(request_id);
    let mut response = next.run(request).await;
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
