use axum::{
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{
        Method,
        header::{AUTHORIZATION, HOST, ORIGIN},
    },
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::ServerState;
use crate::lifecycle::LifecyclePhase;
use wokcore_storage::ClientTokenScope;

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
    let valid_target_authority = request
        .uri()
        .authority()
        .is_none_or(|authority| authority.as_str().as_bytes() == state.authority.as_bytes());
    if !valid_host || !valid_target_authority {
        return ApiError::invalid_authority(request_id).into_response();
    }
    if request.headers().get_all(ORIGIN).iter().next().is_some() {
        return ApiError::origin_not_allowed(request_id).into_response();
    }
    let auth_class = classify_route(request.uri().path());
    if auth_class != RouteAuthClass::Public {
        let candidate = bearer_candidate(&request);
        match auth_class {
            RouteAuthClass::Public => unreachable!(),
            RouteAuthClass::Management => {
                if !candidate.is_some_and(|candidate| state.auth.validate_management(candidate)) {
                    return ApiError::unauthorized(request_id).into_response();
                }
            }
            RouteAuthClass::ManagementOrClientScope(scope) => {
                let Some(candidate) = candidate else {
                    return ApiError::unauthorized(request_id).into_response();
                };
                if !state.auth.validate_management(candidate)
                    && state.auth.validate_client_scope(candidate, scope).is_none()
                {
                    if state.auth.validate_any_client(candidate).is_some() {
                        return ApiError::insufficient_scope(request_id).into_response();
                    }
                    return ApiError::unauthorized(request_id).into_response();
                }
            }
        }
        if is_metadata_mutation_path(request.uri().path()) {
            let guard = match state.lifecycle.admission_controller().try_enter() {
                Ok(guard) => guard,
                Err(_) => return ApiError::service_maintenance(request_id).into_response(),
            };
            let owned = tokio::spawn(async move {
                let _guard = guard;
                run_bounded_request(request, next, request_id).await
            });
            return match owned.await {
                Ok(response) => response,
                Err(_) => ApiError::internal_failure(request_id).into_response(),
            };
        } else if request.method() == Method::GET {
            let phase = state.lifecycle.snapshot().phase;
            if phase != LifecyclePhase::Running && !allowed_during_maintenance(request.uri().path())
            {
                return ApiError::service_maintenance(request_id).into_response();
            }
        }
    }
    if auth_class != RouteAuthClass::Public && requires_bounded_body(request.method()) {
        return run_bounded_request(request, next, request_id).await;
    }
    next.run(request).await
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RouteAuthClass {
    Public,
    Management,
    ManagementOrClientScope(ClientTokenScope),
}

fn bearer_candidate(request: &Request) -> Option<&str> {
    let mut authorization = request.headers().get_all(AUTHORIZATION).iter();
    match (authorization.next(), authorization.next()) {
        (Some(value), None) => value
            .to_str()
            .ok()
            .and_then(|value| value.strip_prefix("Bearer ")),
        _ => None,
    }
}

fn requires_bounded_body(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

async fn run_bounded_request(request: Request, next: Next, request_id: RequestId) -> Response {
    let (parts, body) = request.into_parts();
    let body = match to_bytes(body, 16 * 1024).await {
        Ok(body) => body,
        Err(_) => return ApiError::payload_too_large(request_id).into_response(),
    };
    next.run(Request::from_parts(parts, Body::from(body))).await
}

fn allowed_during_maintenance(path: &str) -> bool {
    matches!(
        path,
        "/wokcore/v1/service/status"
            | "/wokcore/v1/service/drain/cancel"
            | "/wokcore/v1/service/stop"
    )
}

fn classify_route(path: &str) -> RouteAuthClass {
    if matches!(path, "/wokcore/v1/health" | "/wokcore/v1/capabilities") {
        return RouteAuthClass::Public;
    }
    if matches!(
        path,
        "/wokcore/v1/service/status"
            | "/wokcore/v1/service/drain"
            | "/wokcore/v1/service/drain/cancel"
            | "/wokcore/v1/service/stop"
            | "/wokcore/v1/clients/authorize"
    ) || is_revoke_path(path)
    {
        return RouteAuthClass::Management;
    }
    if path == "/wokcore/v1/sessions" || is_session_messages_path(path) {
        return RouteAuthClass::ManagementOrClientScope(ClientTokenScope::SessionsRead);
    }
    if path == "/wokcore/v1/usage" {
        return RouteAuthClass::ManagementOrClientScope(ClientTokenScope::UsageRead);
    }
    if path == "/wokcore/v1/logs" {
        return RouteAuthClass::ManagementOrClientScope(ClientTokenScope::DiagnosticsRead);
    }
    if path == "/wokcore/v1/diagnostics/export" {
        return RouteAuthClass::ManagementOrClientScope(ClientTokenScope::DiagnosticsExport);
    }
    if path.starts_with("/wokcore/v1/") {
        return RouteAuthClass::Management;
    }
    RouteAuthClass::Public
}

fn is_metadata_mutation_path(path: &str) -> bool {
    path == "/wokcore/v1/clients/authorize" || is_revoke_path(path)
}

fn is_revoke_path(path: &str) -> bool {
    let Some(segments) = path.strip_prefix("/wokcore/v1/clients/") else {
        return false;
    };
    let mut segments = segments.split('/');
    matches!(
        (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next()
        ),
        (Some(client_id), Some("tokens"), Some(token_id), None)
            if !client_id.is_empty() && !token_id.is_empty()
    )
}

fn is_session_messages_path(path: &str) -> bool {
    let Some(segments) = path.strip_prefix("/wokcore/v1/sessions/") else {
        return false;
    };
    let mut segments = segments.split('/');
    matches!(
        (segments.next(), segments.next(), segments.next()),
        (Some(session_key), Some("messages"), None) if !session_key.is_empty()
    )
}
