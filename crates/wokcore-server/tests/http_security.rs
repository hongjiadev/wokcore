use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderName, HeaderValue, Request, StatusCode, header},
};
use secrecy::ExposeSecret;
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;
use wokcore_core::{
    id::{ClientId, ProviderId},
    secret::{SecretPurpose, SecretRef, SecretScope},
};
use wokcore_server::{
    ServerState,
    api::build_router,
    auth::{AuthMetadataStore, AuthRegistry, EntropySource, TokenError, TokenMaterial},
    lifecycle::ServiceLifecycle,
    runtime::SystemTokenMetadata,
};
use wokcore_storage::{
    ClientTokenMetadata, ClientTokenScope, MemorySecretStore, RuntimeSecretBinding, StorageError,
};

const AUTHORITY: &str = "127.0.0.1:43127";
const CREATED_AT: &str = "2026-07-26T00:00:00Z";

#[derive(Debug)]
struct FixedEntropy;

impl EntropySource for FixedEntropy {
    fn fill(&self, output: &mut [u8; 32]) -> Result<(), TokenError> {
        output.fill(0x41);
        Ok(())
    }
}

#[derive(Debug)]
struct FailingEntropy;

impl EntropySource for FailingEntropy {
    fn fill(&self, _output: &mut [u8; 32]) -> Result<(), TokenError> {
        Err(TokenError::EntropyUnavailable)
    }
}

#[derive(Debug, Default)]
struct IncrementingEntropy(AtomicUsize);

impl EntropySource for IncrementingEntropy {
    fn fill(&self, output: &mut [u8; 32]) -> Result<(), TokenError> {
        let value = self.0.fetch_add(1, Ordering::AcqRel) + 1;
        output.fill(u8::try_from(value).unwrap());
        Ok(())
    }
}

#[derive(Default)]
struct TestMetadata {
    binding: Mutex<Option<RuntimeSecretBinding>>,
}

impl AuthMetadataStore for TestMetadata {
    fn runtime_secret_binding(
        &self,
        _name: &str,
    ) -> Result<Option<RuntimeSecretBinding>, StorageError> {
        Ok(self.binding.lock().unwrap().clone())
    }

    fn bind_runtime_secret_if_absent(
        &self,
        name: &str,
        secret_ref: &SecretRef,
        created_at: &str,
    ) -> Result<RuntimeSecretBinding, StorageError> {
        let binding = RuntimeSecretBinding {
            name: name.to_owned(),
            secret_ref: secret_ref.clone(),
            revision: 1,
            created_at: created_at.to_owned(),
        };
        *self.binding.lock().unwrap() = Some(binding.clone());
        Ok(binding)
    }

    fn record_orphan_secret(
        &self,
        _secret_ref: &SecretRef,
        _created_at: &str,
    ) -> Result<(), StorageError> {
        Ok(())
    }

    fn load_active_client_tokens(&self) -> Result<Vec<ClientTokenMetadata>, StorageError> {
        Ok(Vec::new())
    }

    fn issue_client_token(&self, _token: &ClientTokenMetadata) -> Result<(), StorageError> {
        Ok(())
    }

    fn issue_client_token_with_scopes(
        &self,
        _token: &ClientTokenMetadata,
        _scopes: &[ClientTokenScope],
    ) -> Result<(), StorageError> {
        Ok(())
    }

    fn revoke_client_token(
        &self,
        _client_id: &ClientId,
        _token_id: &str,
        _revoked_at: &str,
    ) -> Result<bool, StorageError> {
        Ok(false)
    }
}

async fn router() -> Router {
    router_with_request_id_entropy(Arc::new(IncrementingEntropy::default())).await
}

async fn router_with_request_id_entropy(request_id_entropy: Arc<dyn EntropySource>) -> Router {
    let scope = SecretScope {
        provider_id: ProviderId::new("wokcore-runtime").unwrap(),
        account_id: None,
        purpose: SecretPurpose::Auxiliary,
    };
    let auth = AuthRegistry::bootstrap(
        Arc::new(MemorySecretStore::default()),
        Arc::new(TestMetadata::default()),
        Arc::new(FixedEntropy),
        scope,
        CREATED_AT.to_owned(),
    )
    .await
    .unwrap();
    let lifecycle = ServiceLifecycle::new();
    lifecycle.mark_running().unwrap();
    build_router(ServerState::new_with_runtime_sources(
        AUTHORITY,
        Uuid::parse_str("019844f0-4de0-7000-8000-000000000001").unwrap(),
        Arc::new(auth),
        lifecycle,
        Arc::new(SystemTokenMetadata::new(Arc::new(FixedEntropy))),
        request_id_entropy,
    ))
}

async fn router_with_client_scope(scope: ClientTokenScope) -> (Router, String) {
    let secret_scope = SecretScope {
        provider_id: ProviderId::new("wokcore-runtime").unwrap(),
        account_id: None,
        purpose: SecretPurpose::Auxiliary,
    };
    let auth = AuthRegistry::bootstrap(
        Arc::new(MemorySecretStore::default()),
        Arc::new(TestMetadata::default()),
        Arc::new(FixedEntropy),
        secret_scope,
        CREATED_AT.to_owned(),
    )
    .await
    .unwrap();
    let token = auth
        .issue_client_token_with_scopes(
            "scoped-token".to_owned(),
            ClientId::new("wokrouter").unwrap(),
            CREATED_AT.to_owned(),
            vec![scope],
        )
        .await
        .unwrap()
        .into_response_value()
        .expose_secret()
        .to_owned();
    let lifecycle = ServiceLifecycle::new();
    lifecycle.mark_running().unwrap();
    let app = build_router(ServerState::new_with_runtime_sources(
        AUTHORITY,
        Uuid::parse_str("019844f0-4de0-7000-8000-000000000001").unwrap(),
        Arc::new(auth),
        lifecycle,
        Arc::new(SystemTokenMetadata::new(Arc::new(FixedEntropy))),
        Arc::new(IncrementingEntropy::default()),
    ));
    (app, token)
}

fn request(method: &str, host: Option<&str>, origin: Option<&str>) -> Request<Body> {
    request_to(method, "/wokcore/v1/health", host, origin, Body::empty())
}

fn request_to(
    method: &str,
    path: &str,
    host: Option<&str>,
    origin: Option<&str>,
    body: Body,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(host) = host {
        builder = builder.header(header::HOST, host);
    }
    if let Some(origin) = origin {
        builder = builder.header(header::ORIGIN, origin);
    }
    builder.body(body).unwrap()
}

fn management_token() -> String {
    TokenMaterial::generate_admin(&FixedEntropy)
        .unwrap()
        .into_response_value()
        .expose_secret()
        .to_owned()
}

fn proxy_token() -> String {
    TokenMaterial::generate_proxy(&FixedEntropy)
        .unwrap()
        .into_response_value()
        .expose_secret()
        .to_owned()
}

#[tokio::test]
async fn exact_host_and_absent_origin_are_accepted_with_fresh_safe_response_headers() {
    let app = router().await;
    let first = app
        .clone()
        .oneshot(request("GET", Some(AUTHORITY), None))
        .await
        .unwrap();
    let second = app
        .oneshot(request("GET", Some(AUTHORITY), None))
        .await
        .unwrap();

    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(first.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(first.headers()["x-content-type-options"], "nosniff");
    let first_id = first.headers()["x-request-id"].to_str().unwrap();
    let second_id = second.headers()["x-request-id"].to_str().unwrap();
    assert!(Uuid::parse_str(first_id).is_ok());
    assert!(Uuid::parse_str(second_id).is_ok());
    assert_ne!(first_id, second_id);
    assert!(!first.headers().contains_key("access-control-allow-origin"));
}

#[tokio::test]
async fn caller_request_id_is_ignored() {
    let supplied = "00000000-0000-0000-0000-000000000000";
    let mut incoming = request("GET", Some(AUTHORITY), None);
    incoming.headers_mut().insert(
        HeaderName::from_static("x-request-id"),
        HeaderValue::from_static(supplied),
    );

    let response = router().await.oneshot(incoming).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_ne!(response.headers()["x-request-id"], supplied);
}

#[tokio::test]
async fn request_id_entropy_failure_returns_a_stable_safe_error_envelope() {
    let response = router_with_request_id_entropy(Arc::new(FailingEntropy))
        .await
        .oneshot(request("GET", Some(AUTHORITY), None))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    let header_request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(Uuid::parse_str(&header_request_id).is_ok());
    let body = response_body(response).await;
    assert_eq!(body["error"]["code"], "internal_error");
    assert_eq!(body["error"]["request_id"], header_request_id);
    assert!(!body.to_string().contains("entropy"));
}

#[tokio::test]
async fn invalid_or_ambiguous_host_is_rejected_before_routing() {
    for host in [
        None,
        Some("127.0.0.1"),
        Some("127.0.0.1:43128"),
        Some("localhost:43127"),
        Some("user@127.0.0.1:43127"),
        Some("127.0.0.1:43127, localhost:43127"),
    ] {
        let response = router()
            .await
            .oneshot(request("GET", host, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{host:?}");
        assert_eq!(error_code(response).await, "invalid_authority");
    }
}

#[tokio::test]
async fn duplicate_host_is_rejected_and_forwarded_headers_never_override_authority() {
    let mut duplicate = request("GET", Some(AUTHORITY), None);
    duplicate
        .headers_mut()
        .append(header::HOST, HeaderValue::from_static(AUTHORITY));
    let duplicate_response = router().await.oneshot(duplicate).await.unwrap();
    assert_eq!(duplicate_response.status(), StatusCode::BAD_REQUEST);

    let mut bad_host = request("GET", Some("localhost:43127"), None);
    bad_host
        .headers_mut()
        .insert("x-forwarded-host", HeaderValue::from_static(AUTHORITY));
    let bad_host_response = router().await.oneshot(bad_host).await.unwrap();
    assert_eq!(bad_host_response.status(), StatusCode::BAD_REQUEST);

    let mut exact_host = request("GET", Some(AUTHORITY), None);
    exact_host.headers_mut().insert(
        header::FORWARDED,
        HeaderValue::from_static("host=attacker.example"),
    );
    let exact_response = router().await.oneshot(exact_host).await.unwrap();
    assert_eq!(exact_response.status(), StatusCode::OK);
}

#[tokio::test]
async fn absolute_form_request_target_must_use_the_configured_authority() {
    let mismatched = Request::builder()
        .method("GET")
        .uri("http://attacker.invalid/wokcore/v1/health")
        .header(header::HOST, AUTHORITY)
        .body(Body::empty())
        .unwrap();
    let rejected = router().await.oneshot(mismatched).await.unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert_eq!(error_code(rejected).await, "invalid_authority");

    let exact = Request::builder()
        .method("GET")
        .uri(format!("http://{AUTHORITY}/wokcore/v1/health"))
        .header(header::HOST, AUTHORITY)
        .body(Body::empty())
        .unwrap();
    let accepted = router().await.oneshot(exact).await.unwrap();
    assert_eq!(accepted.status(), StatusCode::OK);
}

#[tokio::test]
async fn every_present_origin_is_rejected_without_cors_headers() {
    for origin in [
        "null",
        "http://127.0.0.1:43127",
        "https://localhost:43127",
        "not an origin",
    ] {
        let response = router()
            .await
            .oneshot(request("GET", Some(AUTHORITY), Some(origin)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{origin}");
        assert_eq!(error_code(response).await, "origin_not_allowed");
    }

    let mut duplicate = request("GET", Some(AUTHORITY), Some("null"));
    duplicate.headers_mut().append(
        header::ORIGIN,
        HeaderValue::from_static("http://127.0.0.1:43127"),
    );
    let response = router().await.oneshot(duplicate).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(
        !response
            .headers()
            .contains_key("access-control-allow-origin")
    );
}

#[tokio::test]
async fn options_receives_no_implicit_cors_grant() {
    let response = router()
        .await
        .oneshot(request("OPTIONS", Some(AUTHORITY), None))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert!(
        !response
            .headers()
            .contains_key("access-control-allow-origin")
    );
    assert!(Uuid::parse_str(response.headers()["x-request-id"].to_str().unwrap()).is_ok());
}

#[tokio::test]
async fn management_routes_reject_missing_malformed_wrong_and_proxy_bearer_tokens() {
    for authorization in [
        None,
        Some(String::new()),
        Some("Basic abc".to_owned()),
        Some("Bearer".to_owned()),
        Some("Bearer wrong".to_owned()),
        Some(format!("Bearer {}", proxy_token())),
    ] {
        let mut incoming = request_to(
            "GET",
            "/wokcore/v1/service/status",
            Some(AUTHORITY),
            None,
            Body::empty(),
        );
        if let Some(authorization) = authorization {
            incoming.headers_mut().insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&authorization).unwrap(),
            );
        }
        let response = router().await.oneshot(incoming).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(error_code(response).await, "unauthorized");
    }
}

#[tokio::test]
async fn scoped_management_routes_enforce_allow_forbid_missing_and_management_compatibility() {
    for (scope, method, path) in [
        (
            ClientTokenScope::ServiceRead,
            "GET",
            "/wokcore/v1/service/status",
        ),
        (
            ClientTokenScope::ServiceControl,
            "POST",
            "/wokcore/v1/service/drain/cancel",
        ),
        (
            ClientTokenScope::ProvidersRead,
            "GET",
            "/wokcore/v1/providers/catalog",
        ),
        (
            ClientTokenScope::ProvidersWrite,
            "POST",
            "/wokcore/v1/providers/reload",
        ),
        (
            ClientTokenScope::ClientsManage,
            "DELETE",
            "/wokcore/v1/clients/wokrouter/tokens/unknown",
        ),
    ] {
        let (allowed_app, allowed_token) = router_with_client_scope(scope).await;
        let allowed = allowed_app
            .oneshot(authorized_request(method, path, &allowed_token))
            .await
            .unwrap();
        assert!(
            !matches!(
                allowed.status(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ),
            "allowed scope failed for {method} {path}"
        );

        let (wrong_app, wrong_token) = router_with_client_scope(ClientTokenScope::ProxyUse).await;
        let forbidden = wrong_app
            .oneshot(authorized_request(method, path, &wrong_token))
            .await
            .unwrap();
        assert_eq!(
            forbidden.status(),
            StatusCode::FORBIDDEN,
            "wrong scope for {method} {path}"
        );
        assert_eq!(error_code(forbidden).await, "insufficient_scope");

        let missing = router()
            .await
            .oneshot(request_to(
                method,
                path,
                Some(AUTHORITY),
                None,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(
            missing.status(),
            StatusCode::UNAUTHORIZED,
            "missing token for {method} {path}"
        );

        let management = router()
            .await
            .oneshot(authorized_request(method, path, &management_token()))
            .await
            .unwrap();
        assert!(
            !matches!(
                management.status(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ),
            "management compatibility failed for {method} {path}"
        );
    }
}

#[tokio::test]
async fn unknown_and_scoped_control_plane_routes_never_fall_through_as_public() {
    for path in [
        "/wokcore/v1/sessions",
        "/wokcore/v1/usage",
        "/wokcore/v1/logs",
        "/wokcore/v1/diagnostics/export",
        "/wokcore/v1/future-private-route",
    ] {
        let response = router()
            .await
            .oneshot(request_to(
                "GET",
                path,
                Some(AUTHORITY),
                None,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(error_code(response).await, "unauthorized", "{path}");
    }

    let mut authenticated = request_to(
        "GET",
        "/wokcore/v1/future-private-route",
        Some(AUTHORITY),
        None,
        Body::empty(),
    );
    authenticated.headers_mut().insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", management_token())).unwrap(),
    );
    let response = router().await.oneshot(authenticated).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(error_code(response).await, "not_found");
}

#[tokio::test]
async fn management_authentication_precedes_json_parsing() {
    let invalid_json = ["sensitive", "body", "canary"].join("-");
    let mut incoming = request_to(
        "POST",
        "/wokcore/v1/clients/authorize",
        Some(AUTHORITY),
        None,
        Body::from(invalid_json.clone()),
    );
    incoming.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );

    let response = router().await.oneshot(incoming).await.unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = response_body(response).await;
    assert_eq!(body["error"]["code"], "unauthorized");
    assert!(!body.to_string().contains(&invalid_json));
}

#[tokio::test]
async fn authenticated_management_json_is_bounded_to_sixteen_kibibytes() {
    let oversized = format!(r#"{{"client_id":"{}"}}"#, "a".repeat(16 * 1024));
    let mut incoming = request_to(
        "POST",
        "/wokcore/v1/clients/authorize",
        Some(AUTHORITY),
        None,
        Body::from(oversized.clone()),
    );
    incoming.headers_mut().insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", management_token())).unwrap(),
    );
    incoming.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );

    let response = router().await.oneshot(incoming).await.unwrap();

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let rendered = response_body(response).await.to_string();
    assert!(rendered.contains("payload_too_large"));
    assert!(!rendered.contains(&oversized));

    let mut drain = request_to(
        "POST",
        "/wokcore/v1/service/drain",
        Some(AUTHORITY),
        None,
        Body::from("x".repeat(16 * 1024 + 1)),
    );
    drain.headers_mut().insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", management_token())).unwrap(),
    );
    let drain_response = router().await.oneshot(drain).await.unwrap();
    assert_eq!(drain_response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(error_code(drain_response).await, "payload_too_large");
}

#[tokio::test]
async fn errors_never_echo_headers_body_tokens_paths_or_backend_diagnostics() {
    let token = management_token();
    let canaries = [
        token.as_str(),
        "cookie-canary",
        "body-canary",
        r"C:\Users\secret\state.db",
        "sqlite backend diagnostic",
    ];
    let mut incoming = request_to(
        "POST",
        "/wokcore/v1/clients/authorize",
        Some("localhost:43127"),
        None,
        Body::from(format!("{} {}", canaries[2], canaries[3])),
    );
    incoming.headers_mut().insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    incoming.headers_mut().insert(
        header::COOKIE,
        HeaderValue::from_static("session=cookie-canary"),
    );
    incoming.headers_mut().insert(
        "x-backend-error",
        HeaderValue::from_static("sqlite backend diagnostic"),
    );

    let response = router().await.oneshot(incoming).await.unwrap();
    let rendered = response_body(response).await.to_string();

    for canary in canaries {
        assert!(!rendered.contains(canary), "leaked {canary}");
    }
}

async fn error_code(response: axum::response::Response) -> String {
    let value = response_body(response).await;
    value["error"]["code"].as_str().unwrap().to_owned()
}

async fn response_body(response: axum::response::Response) -> Value {
    let body = to_bytes(response.into_body(), 32 * 1024).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn authorized_request(method: &str, path: &str, token: &str) -> Request<Body> {
    let mut request = request_to(method, path, Some(AUTHORITY), None, Body::empty());
    request.headers_mut().insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    request
}
