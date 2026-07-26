use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;

use super::request_id::RequestId;

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: &'a str,
    request_id: &'a str,
}

pub(crate) struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    request_id: RequestId,
}

impl ApiError {
    pub(crate) fn invalid_authority(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_authority",
            "request authority is invalid",
            request_id,
        )
    }

    pub(crate) fn origin_not_allowed(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "origin_not_allowed",
            "browser origins are not allowed",
            request_id,
        )
    }

    pub(crate) fn not_found(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "route was not found",
            request_id,
        )
    }

    pub(crate) fn method_not_allowed(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "method is not allowed",
            request_id,
        )
    }

    pub(crate) fn unauthorized(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "management authentication is required",
            request_id,
        )
    }

    pub(crate) fn invalid_request_body(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_request_body",
            "request body is invalid",
            request_id,
        )
    }

    pub(crate) fn payload_too_large(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "request body exceeds the allowed size",
            request_id,
        )
    }

    pub(crate) fn service_maintenance(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "service_maintenance",
            "service is in maintenance",
            request_id,
        )
    }

    pub(crate) fn lifecycle_conflict(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "lifecycle_conflict",
            "service lifecycle transition is unavailable",
            request_id,
        )
    }

    pub(crate) fn invalid_client_id(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_client_id",
            "client identifier is invalid",
            request_id,
        )
    }

    pub(crate) fn storage_failure(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "authentication metadata operation failed",
            request_id,
        )
    }

    fn new(
        status: StatusCode,
        code: &'static str,
        message: &'static str,
        request_id: RequestId,
    ) -> Self {
        Self {
            status,
            code,
            message,
            request_id,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let unauthorized = self.status == StatusCode::UNAUTHORIZED;
        let request_id = self.request_id.to_string();
        let mut response = (
            self.status,
            Json(ErrorEnvelope {
                error: ErrorBody {
                    code: self.code,
                    message: self.message,
                    request_id: &request_id,
                },
            }),
        )
            .into_response();
        if unauthorized {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}
