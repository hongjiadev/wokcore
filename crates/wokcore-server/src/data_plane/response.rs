use std::io::{self, Write};

use axum::{
    Json,
    body::Body,
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use wokcore_protocols::canonical::GatewayError;

use crate::api::RequestId;

use super::{ClientProtocol, JSON_BODY_LIMIT, SafeUpstreamRequestId};

const UPSTREAM_REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-upstream-request-id");

#[derive(Serialize)]
struct OpenAiErrorEnvelope<'a> {
    error: OpenAiPublicError<'a>,
}

#[derive(Serialize)]
struct OpenAiPublicError<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    code: &'a str,
    message: &'a str,
    request_id: &'a str,
}

#[derive(Serialize)]
struct AnthropicErrorEnvelope<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    error: AnthropicPublicError<'a>,
    request_id: &'a str,
}

#[derive(Serialize)]
struct AnthropicPublicError<'a> {
    #[serde(rename = "type")]
    code: &'a str,
    message: &'a str,
}

pub(crate) fn public_error_response(
    status: StatusCode,
    code: &str,
    message: &str,
    request_id: RequestId,
    protocol: ClientProtocol,
) -> Response {
    let request_id = request_id.to_string();
    let mut response = if protocol.is_anthropic() {
        (
            status,
            Json(AnthropicErrorEnvelope {
                kind: "error",
                error: AnthropicPublicError { code, message },
                request_id: &request_id,
            }),
        )
            .into_response()
    } else {
        (
            status,
            Json(OpenAiErrorEnvelope {
                error: OpenAiPublicError {
                    kind: "gateway_error",
                    code,
                    message,
                    request_id: &request_id,
                },
            }),
        )
            .into_response()
    };
    if status == StatusCode::UNAUTHORIZED {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    response
}

pub(crate) fn gateway_error_response(
    error: &GatewayError,
    request_id: RequestId,
    protocol: ClientProtocol,
    upstream_request_id: Option<&SafeUpstreamRequestId>,
) -> Response {
    let status =
        StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let response = public_error_response(
        status,
        error.code(),
        error.public_message(),
        request_id,
        protocol,
    );
    attach_upstream_request_id(response, upstream_request_id)
}

pub(crate) fn attach_upstream_request_id(
    mut response: Response,
    upstream_request_id: Option<&SafeUpstreamRequestId>,
) -> Response {
    if let Some(upstream_request_id) = upstream_request_id {
        response.headers_mut().insert(
            UPSTREAM_REQUEST_ID_HEADER,
            HeaderValue::from_str(upstream_request_id.as_str())
                .expect("safe upstream request IDs are valid header values"),
        );
    }
    response
}

pub(crate) fn bounded_json_response<T>(
    value: &T,
    upstream_request_id: Option<&SafeUpstreamRequestId>,
) -> Result<Response, GatewayError>
where
    T: serde::Serialize + ?Sized,
{
    let mut encoded = BoundedJsonWriter::new(JSON_BODY_LIMIT);
    serde_json::to_writer(&mut encoded, value)
        .map_err(|_| GatewayError::internal("response encoding"))?;
    let mut response = Response::new(Body::from(encoded.into_inner()));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(attach_upstream_request_id(response, upstream_request_id))
}

struct BoundedJsonWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl BoundedJsonWriter {
    const fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for BoundedJsonWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let length = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("response length overflow"))?;
        if length > self.limit {
            return Err(io::Error::other("response length limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn payload_too_large(request_id: RequestId, protocol: ClientProtocol) -> Response {
    public_error_response(
        StatusCode::PAYLOAD_TOO_LARGE,
        "payload_too_large",
        "The request body exceeds the configured limit.",
        request_id,
        protocol,
    )
}

pub(crate) fn invalid_request(request_id: RequestId, protocol: ClientProtocol) -> Response {
    public_error_response(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "The request is invalid.",
        request_id,
        protocol,
    )
}

pub(crate) fn invalid_body(request_id: RequestId, protocol: ClientProtocol) -> Response {
    public_error_response(
        StatusCode::BAD_REQUEST,
        "invalid_body",
        "The request body could not be read.",
        request_id,
        protocol,
    )
}
