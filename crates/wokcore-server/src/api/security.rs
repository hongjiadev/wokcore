use axum::{
    body::{Body, to_bytes},
    extract::{Request, State},
    http::header::{AUTHORIZATION, HOST, ORIGIN},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::ServerState;
use crate::lifecycle::LifecyclePhase;

use super::{error::ApiError, request_id::RequestId};

pub(crate) async fn enforce_request_security(
    State(state): State<ServerState>,
    request: Request,
    next: Next,
) -> Response {
    let request_id = *request
        .extensions()
        .get::<RequestId>()
        .expect("request ID middleware runs first");
    let mut hosts = request.headers().get_all(HOST).iter();
    let valid_host = matches!(
        (hosts.next(), hosts.next()),
        (Some(host), None) if host.as_bytes() == state.authority.as_bytes()
    );
    if !valid_host {
        return ApiError::invalid_authority(request_id).into_response();
    }
    if request.headers().get_all(ORIGIN).iter().next().is_some() {
        return ApiError::origin_not_allowed(request_id).into_response();
    }
    let management = is_management_path(request.uri().path());
    if management {
        let mut authorization = request.headers().get_all(AUTHORIZATION).iter();
        let candidate = match (authorization.next(), authorization.next()) {
            (Some(value), None) => value
                .to_str()
                .ok()
                .and_then(|value| value.strip_prefix("Bearer ")),
            _ => None,
        };
        if !candidate.is_some_and(|candidate| state.auth.validate_management(candidate)) {
            return ApiError::unauthorized(request_id).into_response();
        }
        let phase = state.lifecycle.snapshot().phase;
        if phase != LifecyclePhase::Running && !allowed_during_maintenance(request.uri().path()) {
            return ApiError::service_maintenance(request_id).into_response();
        }
    }
    if management {
        let (parts, body) = request.into_parts();
        let body = match to_bytes(body, 16 * 1024).await {
            Ok(body) => body,
            Err(_) => return ApiError::payload_too_large(request_id).into_response(),
        };
        return next.run(Request::from_parts(parts, Body::from(body))).await;
    }
    next.run(request).await
}

fn allowed_during_maintenance(path: &str) -> bool {
    matches!(
        path,
        "/wokcore/v1/service/status"
            | "/wokcore/v1/service/drain/cancel"
            | "/wokcore/v1/service/stop"
    )
}

fn is_management_path(path: &str) -> bool {
    matches!(
        path,
        "/wokcore/v1/service/status"
            | "/wokcore/v1/service/drain"
            | "/wokcore/v1/service/drain/cancel"
            | "/wokcore/v1/service/stop"
            | "/wokcore/v1/clients/authorize"
    ) || (path.starts_with("/wokcore/v1/clients/") && path.contains("/tokens/"))
}
