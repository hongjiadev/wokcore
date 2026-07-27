use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;

use crate::api::RequestId;

use super::ClientProtocol;

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

pub(crate) fn unsupported_capability(request_id: RequestId, protocol: ClientProtocol) -> Response {
    public_error_response(
        StatusCode::UNPROCESSABLE_ENTITY,
        "unsupported_capability",
        "The requested capability is not supported.",
        request_id,
        protocol,
    )
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
