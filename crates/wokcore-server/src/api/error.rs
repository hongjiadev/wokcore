use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use wokcore_diagnostics::event::DiagnosticComponent;

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
    component: DiagnosticComponent,
}

#[derive(Clone, Copy)]
pub(crate) struct ApiErrorComponent(pub(crate) DiagnosticComponent);

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
            "bearer authentication is required",
            request_id,
        )
    }

    pub(crate) fn insufficient_scope(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "insufficient_scope",
            "client token does not grant the required scope",
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

    pub(crate) fn unsupported_media_type(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "request content type is not supported",
            request_id,
        )
    }

    pub(crate) fn provider_runtime_unavailable(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "provider_runtime_unavailable",
            "Provider runtime is unavailable",
            request_id,
        )
        .with_component(DiagnosticComponent::Provider)
    }

    pub(crate) fn provider_config_invalid(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "provider_config_invalid",
            "Provider configuration is invalid",
            request_id,
        )
        .with_component(DiagnosticComponent::Provider)
    }

    pub(crate) fn provider_config_revision_conflict(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "provider_config_revision_conflict",
            "Provider configuration revision conflicts",
            request_id,
        )
        .with_component(DiagnosticComponent::Provider)
    }

    pub(crate) fn provider_secret_not_found(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "provider_secret_not_found",
            "Provider secret was not found",
            request_id,
        )
        .with_component(DiagnosticComponent::Provider)
    }

    pub(crate) fn provider_secret_already_exists(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "provider_secret_already_exists",
            "a different Provider secret already exists for this credential scope",
            request_id,
        )
        .with_component(DiagnosticComponent::Provider)
    }

    pub(crate) fn provider_secret_in_use(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "provider_secret_in_use",
            "Provider secret is referenced by the active configuration",
            request_id,
        )
        .with_component(DiagnosticComponent::Provider)
    }

    pub(crate) fn provider_secret_protected(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "provider_secret_protected",
            "secret reference is reserved for WokCore runtime authentication",
            request_id,
        )
        .with_component(DiagnosticComponent::Provider)
    }

    pub(crate) fn provider_secret_store_read_only(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "provider_secret_store_read_only",
            "Provider secret store is read-only",
            request_id,
        )
        .with_component(DiagnosticComponent::Provider)
    }

    pub(crate) fn provider_storage_failure(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Provider management storage operation failed",
            request_id,
        )
        .with_component(DiagnosticComponent::Storage)
    }

    pub(crate) fn provider_internal_failure(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Provider management operation failed",
            request_id,
        )
        .with_component(DiagnosticComponent::Provider)
    }

    pub(crate) fn invalid_query(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "query parameters are invalid",
            request_id,
        )
    }

    pub(crate) fn limit_out_of_range(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "limit_out_of_range",
            "query count limit is out of range",
            request_id,
        )
    }

    pub(crate) fn response_limit_out_of_range(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "response_limit_out_of_range",
            "response byte limit is out of range",
            request_id,
        )
    }

    pub(crate) fn export_limit_out_of_range(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "export_limit_out_of_range",
            "diagnostic export byte limit is out of range",
            request_id,
        )
    }

    pub(crate) fn invalid_time_range(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_time_range",
            "query time range is invalid",
            request_id,
        )
    }

    pub(crate) fn invalid_cursor(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_cursor",
            "pagination cursor is invalid",
            request_id,
        )
    }

    pub(crate) fn query_busy(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "query_busy",
            "query service is busy",
            request_id,
        )
    }

    pub(crate) fn query_timeout(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "query_timeout",
            "query deadline elapsed",
            request_id,
        )
    }

    pub(crate) fn session_not_found(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "session_not_found",
            "Session was not found",
            request_id,
        )
        .with_component(DiagnosticComponent::Sessions)
    }

    pub(crate) fn session_unavailable(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::GONE,
            "session_unavailable",
            "Session source is unavailable",
            request_id,
        )
        .with_component(DiagnosticComponent::Sessions)
    }

    pub(crate) fn session_cursor_stale(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "session_cursor_stale",
            "Session cursor no longer matches the source generation",
            request_id,
        )
        .with_component(DiagnosticComponent::Sessions)
    }

    pub(crate) fn diagnostics_export_busy(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "diagnostics_export_busy",
            "another diagnostic export is already running",
            request_id,
        )
        .with_component(DiagnosticComponent::Diagnostics)
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

    pub(crate) fn invalid_path_parameters(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_path_parameters",
            "path parameters are invalid",
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
        .with_component(DiagnosticComponent::Storage)
    }

    pub(crate) fn internal_failure(request_id: RequestId) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "control-plane request failed",
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
            component: DiagnosticComponent::Core,
        }
    }

    fn with_component(mut self, component: DiagnosticComponent) -> Self {
        self.component = component;
        self
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
            .extensions_mut()
            .insert(ApiErrorComponent(self.component));
        response
    }
}
