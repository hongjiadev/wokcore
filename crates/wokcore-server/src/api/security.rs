use axum::{
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{
        Method, StatusCode,
        header::{AUTHORIZATION, CONTENT_TYPE, HOST, ORIGIN},
    },
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::lifecycle::LifecyclePhase;
use crate::{
    ServerState,
    data_plane::{
        ClientProtocol, ProtocolRegistry, RequestBodyKind, hold_admission_until_body_end,
        is_json_content_type, public_error_response,
    },
};
use wokcore_storage::ClientTokenScope;

use super::{error::ApiError, request_id::RequestId};

pub(crate) async fn enforce_request_security(
    State(state): State<ServerState>,
    mut request: Request,
    next: Next,
) -> Response {
    let request_id = *request
        .extensions()
        .get::<RequestId>()
        .expect("request ID middleware runs first");
    let protocol = ProtocolRegistry::resolve(request.uri().path());
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
        if let Some(protocol) = protocol {
            return public_error_response(
                StatusCode::BAD_REQUEST,
                "invalid_authority",
                "The request authority is invalid.",
                request_id,
                protocol,
            );
        }
        return ApiError::invalid_authority(request_id).into_response();
    }
    if request.headers().get_all(ORIGIN).iter().next().is_some() {
        if let Some(protocol) = protocol {
            return public_error_response(
                StatusCode::FORBIDDEN,
                "origin_not_allowed",
                "Browser origins are not allowed.",
                request_id,
                protocol,
            );
        }
        return ApiError::origin_not_allowed(request_id).into_response();
    }
    let auth_class = classify_route(request.method(), request.uri().path());
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
            RouteAuthClass::ClientScope(scope) => {
                let Some(candidate) = candidate else {
                    return protocol_auth_error(
                        protocol,
                        request_id,
                        StatusCode::UNAUTHORIZED,
                        "unauthorized",
                        "The request is not authorized.",
                    );
                };
                let Some(authorized) = state.auth.validate_client_scope(candidate, scope) else {
                    if state.auth.validate_any_client(candidate).is_some() {
                        return protocol_auth_error(
                            protocol,
                            request_id,
                            StatusCode::FORBIDDEN,
                            "insufficient_scope",
                            "The client token does not grant proxy.use.",
                        );
                    }
                    return protocol_auth_error(
                        protocol,
                        request_id,
                        StatusCode::UNAUTHORIZED,
                        "unauthorized",
                        "The request is not authorized.",
                    );
                };
                request.extensions_mut().insert(authorized);
            }
        }
        if let Some(protocol) = protocol {
            if let Some(failure) = validate_data_plane_request(&request, protocol) {
                return public_error_response(
                    failure.status,
                    failure.code,
                    failure.message,
                    request_id,
                    protocol,
                );
            }
            request.headers_mut().remove(AUTHORIZATION);
            request.extensions_mut().insert(protocol);
            let guard = match state.lifecycle.admission_controller().try_enter() {
                Ok(guard) => guard,
                Err(_) => {
                    return public_error_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "service_maintenance",
                        "The service is temporarily unavailable.",
                        request_id,
                        protocol,
                    );
                }
            };
            let response = next.run(request).await;
            return hold_admission_until_body_end(response, guard);
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
    ClientScope(ClientTokenScope),
}

#[derive(Clone, Copy)]
struct DataPlaneSecurityFailure {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
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

fn classify_route(method: &Method, path: &str) -> RouteAuthClass {
    if matches!(path, "/wokcore/v1/health" | "/wokcore/v1/capabilities") {
        return RouteAuthClass::Public;
    }
    if method == Method::GET && path == "/wokcore/v1/service/status" {
        return RouteAuthClass::ManagementOrClientScope(ClientTokenScope::ServiceRead);
    }
    if method == Method::POST
        && matches!(
            path,
            "/wokcore/v1/service/drain"
                | "/wokcore/v1/service/drain/cancel"
                | "/wokcore/v1/service/stop"
        )
    {
        return RouteAuthClass::ManagementOrClientScope(ClientTokenScope::ServiceControl);
    }
    if method == Method::GET
        && matches!(
            path,
            "/wokcore/v1/providers/catalog"
                | "/wokcore/v1/providers/runtime"
                | "/wokcore/v1/providers/models"
        )
    {
        return RouteAuthClass::ManagementOrClientScope(ClientTokenScope::ProvidersRead);
    }
    if (method == Method::POST && path == "/wokcore/v1/providers/config/validate")
        || (method == Method::PUT && path == "/wokcore/v1/providers/config")
        || (method == Method::POST && path == "/wokcore/v1/providers/reload")
        || (method == Method::POST && path == "/wokcore/v1/provider-secrets")
        || ((method == Method::PUT || method == Method::DELETE) && is_provider_secret_path(path))
    {
        return RouteAuthClass::ManagementOrClientScope(ClientTokenScope::ProvidersWrite);
    }
    if (method == Method::POST && path == "/wokcore/v1/clients/authorize")
        || (method == Method::DELETE && is_revoke_path(path))
    {
        return RouteAuthClass::ManagementOrClientScope(ClientTokenScope::ClientsManage);
    }
    if method == Method::GET && (path == "/wokcore/v1/sessions" || is_session_messages_path(path)) {
        return RouteAuthClass::ManagementOrClientScope(ClientTokenScope::SessionsRead);
    }
    if method == Method::GET && path == "/wokcore/v1/usage" {
        return RouteAuthClass::ManagementOrClientScope(ClientTokenScope::UsageRead);
    }
    if method == Method::GET && path == "/wokcore/v1/logs" {
        return RouteAuthClass::ManagementOrClientScope(ClientTokenScope::DiagnosticsRead);
    }
    if method == Method::GET && path == "/wokcore/v1/diagnostics/export" {
        return RouteAuthClass::ManagementOrClientScope(ClientTokenScope::DiagnosticsExport);
    }
    if path.starts_with("/wokcore/v1/") {
        return RouteAuthClass::Management;
    }
    if path.starts_with("/v1/") {
        return RouteAuthClass::ClientScope(ClientTokenScope::ProxyUse);
    }
    RouteAuthClass::Public
}

fn protocol_auth_error(
    protocol: Option<ClientProtocol>,
    request_id: RequestId,
    status: StatusCode,
    code: &'static str,
    message: &'static str,
) -> Response {
    if let Some(protocol) = protocol {
        return public_error_response(status, code, message, request_id, protocol);
    }
    if status == StatusCode::FORBIDDEN {
        ApiError::insufficient_scope(request_id).into_response()
    } else {
        ApiError::unauthorized(request_id).into_response()
    }
}

fn validate_data_plane_request(
    request: &Request,
    protocol: ClientProtocol,
) -> Option<DataPlaneSecurityFailure> {
    let expected_method = if protocol == ClientProtocol::OpenAiModels {
        Method::GET
    } else {
        Method::POST
    };
    if request.method() != expected_method {
        return Some(DataPlaneSecurityFailure {
            status: StatusCode::METHOD_NOT_ALLOWED,
            code: "method_not_allowed",
            message: "The request method is not allowed.",
        });
    }

    match protocol.request_body_kind() {
        RequestBodyKind::None => {
            if request.headers().contains_key(CONTENT_TYPE) {
                return Some(DataPlaneSecurityFailure {
                    status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    code: "unsupported_media_type",
                    message: "This endpoint does not accept a request body.",
                });
            }
        }
        RequestBodyKind::Json => {
            if !single_content_type(request).is_some_and(|value| {
                value
                    .to_str()
                    .ok()
                    .is_some_and(|value| is_json_content_type(Some(value)))
            }) {
                return Some(DataPlaneSecurityFailure {
                    status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    code: "unsupported_media_type",
                    message: "Content-Type must be application/json.",
                });
            }
        }
        RequestBodyKind::MultipartFormData => {
            if !single_content_type(request)
                .and_then(|value| value.to_str().ok())
                .is_some_and(is_multipart_form_data)
            {
                return Some(DataPlaneSecurityFailure {
                    status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    code: "unsupported_media_type",
                    message: "Content-Type must be multipart/form-data with a boundary.",
                });
            }
        }
    }
    None
}

fn single_content_type(request: &Request) -> Option<&axum::http::HeaderValue> {
    let mut values = request.headers().get_all(CONTENT_TYPE).iter();
    match (values.next(), values.next()) {
        (Some(value), None) => Some(value),
        _ => None,
    }
}

fn is_multipart_form_data(value: &str) -> bool {
    let mut segments = value.split(';');
    if !segments.next().is_some_and(|media_type| {
        media_type
            .trim()
            .eq_ignore_ascii_case("multipart/form-data")
    }) {
        return false;
    }
    let Some(parameter) = segments.next() else {
        return false;
    };
    if segments.next().is_some() {
        return false;
    }
    let Some((name, boundary)) = parameter.trim().split_once('=') else {
        return false;
    };
    if !name.trim().eq_ignore_ascii_case("boundary") {
        return false;
    }
    let boundary = boundary.trim();
    let boundary = boundary
        .strip_prefix('"')
        .and_then(|boundary| boundary.strip_suffix('"'))
        .unwrap_or(boundary);
    !boundary.is_empty()
}

fn is_metadata_mutation_path(path: &str) -> bool {
    path == "/wokcore/v1/clients/authorize"
        || path == "/wokcore/v1/providers/config"
        || path == "/wokcore/v1/providers/reload"
        || path == "/wokcore/v1/provider-secrets"
        || path.starts_with("/wokcore/v1/provider-secrets/")
        || is_revoke_path(path)
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

fn is_provider_secret_path(path: &str) -> bool {
    let Some(secret_ref) = path.strip_prefix("/wokcore/v1/provider-secrets/") else {
        return false;
    };
    !secret_ref.is_empty() && !secret_ref.contains('/')
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

#[cfg(test)]
mod route_auth_tests {
    use axum::http::Method;
    use wokcore_storage::ClientTokenScope;

    use super::{RouteAuthClass, classify_route};

    #[test]
    fn management_route_scope_matrix_is_exact_and_method_aware() {
        for (method, path, expected) in [
            (
                Method::GET,
                "/wokcore/v1/service/status",
                ClientTokenScope::ServiceRead,
            ),
            (
                Method::POST,
                "/wokcore/v1/service/drain",
                ClientTokenScope::ServiceControl,
            ),
            (
                Method::POST,
                "/wokcore/v1/service/drain/cancel",
                ClientTokenScope::ServiceControl,
            ),
            (
                Method::POST,
                "/wokcore/v1/service/stop",
                ClientTokenScope::ServiceControl,
            ),
            (
                Method::GET,
                "/wokcore/v1/providers/catalog",
                ClientTokenScope::ProvidersRead,
            ),
            (
                Method::GET,
                "/wokcore/v1/providers/runtime",
                ClientTokenScope::ProvidersRead,
            ),
            (
                Method::GET,
                "/wokcore/v1/providers/models",
                ClientTokenScope::ProvidersRead,
            ),
            (
                Method::POST,
                "/wokcore/v1/providers/config/validate",
                ClientTokenScope::ProvidersWrite,
            ),
            (
                Method::PUT,
                "/wokcore/v1/providers/config",
                ClientTokenScope::ProvidersWrite,
            ),
            (
                Method::POST,
                "/wokcore/v1/providers/reload",
                ClientTokenScope::ProvidersWrite,
            ),
            (
                Method::POST,
                "/wokcore/v1/provider-secrets",
                ClientTokenScope::ProvidersWrite,
            ),
            (
                Method::PUT,
                "/wokcore/v1/provider-secrets/secret",
                ClientTokenScope::ProvidersWrite,
            ),
            (
                Method::DELETE,
                "/wokcore/v1/provider-secrets/secret",
                ClientTokenScope::ProvidersWrite,
            ),
            (
                Method::POST,
                "/wokcore/v1/clients/authorize",
                ClientTokenScope::ClientsManage,
            ),
            (
                Method::DELETE,
                "/wokcore/v1/clients/client/tokens/token",
                ClientTokenScope::ClientsManage,
            ),
        ] {
            assert_eq!(
                classify_route(&method, path),
                RouteAuthClass::ManagementOrClientScope(expected),
                "{method} {path}"
            );
        }

        for (method, path) in [
            (Method::GET, "/wokcore/v1/providers/config"),
            (Method::POST, "/wokcore/v1/providers/catalog"),
            (Method::GET, "/wokcore/v1/clients/authorize"),
            (Method::GET, "/wokcore/v1/future-private-route"),
        ] {
            assert_eq!(
                classify_route(&method, path),
                RouteAuthClass::Management,
                "{method} {path}"
            );
        }
    }
}

#[cfg(test)]
mod response_body_tests {
    use std::{
        io,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicU8, Ordering},
        },
        task::{Context, Poll},
    };

    use axum::{
        Router,
        body::{Body, Bytes, to_bytes},
        http::{Method, Request, Uri, header},
        middleware,
        response::Response,
        routing::post,
    };
    use futures_core::Stream;
    use secrecy::ExposeSecret;
    use tower::ServiceExt;
    use wokcore_core::{
        id::{ClientId, ProviderId},
        secret::{SecretPurpose, SecretScope},
    };
    use wokcore_storage::{ClientTokenScope, MemorySecretStore, StateStore};

    use crate::{
        ServerState,
        api::request_id::apply_response_envelope,
        auth::{AuthRegistry, EntropySource, StateAuthMetadataStore, TokenError},
        lifecycle::ServiceLifecycle,
    };

    use super::enforce_request_security;

    const AUTHORITY: &str = "127.0.0.1:43131";
    const CREATED_AT: &str = "2026-07-27T00:00:00Z";

    struct TestRuntime {
        app: Router,
        proxy: String,
        lifecycle: ServiceLifecycle,
        _directory: tempfile::TempDir,
    }

    #[derive(Default)]
    struct IncrementingEntropy(AtomicU8);

    impl EntropySource for IncrementingEntropy {
        fn fill(&self, output: &mut [u8; 32]) -> Result<(), TokenError> {
            output.fill(self.0.fetch_add(1, Ordering::SeqCst).wrapping_add(1));
            Ok(())
        }
    }

    async fn runtime() -> TestRuntime {
        let directory = tempfile::tempdir().unwrap();
        let secrets = Arc::new(MemorySecretStore::default());
        let metadata = Arc::new(StateAuthMetadataStore::new(
            StateStore::open(directory.path().join("state.sqlite3")).unwrap(),
        ));
        let auth = Arc::new(
            AuthRegistry::bootstrap(
                secrets,
                metadata,
                Arc::new(IncrementingEntropy::default()),
                SecretScope {
                    provider_id: ProviderId::new("wokcore-runtime").unwrap(),
                    account_id: None,
                    purpose: SecretPurpose::Auxiliary,
                },
                CREATED_AT.to_owned(),
            )
            .await
            .unwrap(),
        );
        let proxy = auth
            .issue_client_token_with_scopes(
                "019844f0-4de0-7000-8000-000000000131".to_owned(),
                ClientId::new("response-body-probe").unwrap(),
                CREATED_AT.to_owned(),
                vec![ClientTokenScope::ProxyUse],
            )
            .await
            .unwrap()
            .into_response_value()
            .expose_secret()
            .to_owned();
        let lifecycle = ServiceLifecycle::new();
        lifecycle.mark_running().unwrap();
        let state = ServerState::new(
            AUTHORITY,
            uuid::Uuid::parse_str("019844f0-4de0-7000-8000-000000000132").unwrap(),
            auth,
            lifecycle.clone(),
        );
        let app = Router::new()
            .route("/v1/responses", post(response_probe))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                enforce_request_security,
            ))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                apply_response_envelope,
            ))
            .with_state(state);
        TestRuntime {
            app,
            proxy,
            lifecycle,
            _directory: directory,
        }
    }

    fn request(mode: &str, proxy: &str) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri(format!("/v1/responses?mode={mode}"))
            .header(header::HOST, AUTHORITY)
            .header(header::AUTHORIZATION, format!("Bearer {proxy}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap()
    }

    async fn response_probe(uri: Uri) -> Response {
        let mode = uri.query().unwrap_or_default();
        let body = match mode {
            "mode=pending" => Body::from_stream(PendingStream),
            "mode=error" => Body::from_stream(ErrorStream(false)),
            "mode=panic" => Body::from_stream(PanicStream),
            _ => Body::from("complete"),
        };
        Response::new(body)
    }

    #[tokio::test]
    async fn data_plane_admission_guard_follows_response_body_eos_and_drop() {
        let runtime = runtime().await;
        let pending = runtime
            .app
            .clone()
            .oneshot(request("pending", &runtime.proxy))
            .await
            .unwrap();
        assert_eq!(runtime.lifecycle.snapshot().active_requests, 1);
        drop(pending);
        assert_eq!(runtime.lifecycle.snapshot().active_requests, 0);

        let complete = runtime
            .app
            .clone()
            .oneshot(request("complete", &runtime.proxy))
            .await
            .unwrap();
        assert_eq!(runtime.lifecycle.snapshot().active_requests, 1);
        assert_eq!(
            to_bytes(complete.into_body(), 32).await.unwrap(),
            "complete"
        );
        assert_eq!(runtime.lifecycle.snapshot().active_requests, 0);
    }

    #[tokio::test]
    async fn data_plane_admission_guard_releases_on_response_error_cancel_and_panic() {
        let runtime = runtime().await;
        let failed = runtime
            .app
            .clone()
            .oneshot(request("error", &runtime.proxy))
            .await
            .unwrap();
        assert_eq!(runtime.lifecycle.snapshot().active_requests, 1);
        assert!(to_bytes(failed.into_body(), 32).await.is_err());
        assert_eq!(runtime.lifecycle.snapshot().active_requests, 0);

        let pending = runtime
            .app
            .clone()
            .oneshot(request("pending", &runtime.proxy))
            .await
            .unwrap();
        let cancelled = tokio::spawn(to_bytes(pending.into_body(), 32));
        assert_eq!(runtime.lifecycle.snapshot().active_requests, 1);
        cancelled.abort();
        assert!(cancelled.await.unwrap_err().is_cancelled());
        assert_eq!(runtime.lifecycle.snapshot().active_requests, 0);

        let panicking = runtime
            .app
            .clone()
            .oneshot(request("panic", &runtime.proxy))
            .await
            .unwrap();
        let panicked = tokio::spawn(to_bytes(panicking.into_body(), 32));
        assert!(panicked.await.unwrap_err().is_panic());
        assert_eq!(runtime.lifecycle.snapshot().active_requests, 0);
    }

    struct PendingStream;

    impl Stream for PendingStream {
        type Item = Result<Bytes, io::Error>;

        fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }

    struct ErrorStream(bool);

    impl Stream for ErrorStream {
        type Item = Result<Bytes, io::Error>;

        fn poll_next(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Option<Self::Item>> {
            if self.0 {
                Poll::Ready(None)
            } else {
                self.0 = true;
                Poll::Ready(Some(Err(io::Error::other("synthetic response failure"))))
            }
        }
    }

    struct PanicStream;

    impl Stream for PanicStream {
        type Item = Result<Bytes, io::Error>;

        fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            panic!("synthetic response body panic")
        }
    }
}
