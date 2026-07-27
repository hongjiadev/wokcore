use std::{
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use futures_core::Stream;
use secrecy::ExposeSecret;
use serde_json::Value;
use tower::ServiceExt;
use wokcore_core::{
    id::{ClientId, ProviderId},
    secret::{SecretPurpose, SecretScope},
};
use wokcore_server::{
    ServerState,
    api::build_router,
    auth::{AuthMetadataStore, AuthRegistry, EntropySource, StateAuthMetadataStore, TokenError},
    lifecycle::{LifecyclePhase, ServiceLifecycle},
};
use wokcore_storage::{ClientTokenScope, MemorySecretStore, SecretStore, StateStore};

const AUTHORITY: &str = "127.0.0.1:43130";
const CREATED_AT: &str = "2026-07-27T00:00:00Z";
const JSON_BODY_LIMIT: usize = 16 * 1024 * 1024;
const IMAGE_PART_LIMIT: usize = 20 * 1024 * 1024;
const MULTIPART_BODY_LIMIT: usize = 50 * 1024 * 1024;
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

const DATA_PLANE_ROUTES: [(&str, Method, Option<&str>); 7] = [
    ("/v1/responses", Method::POST, Some("application/json")),
    (
        "/v1/chat/completions",
        Method::POST,
        Some("application/json"),
    ),
    ("/v1/messages", Method::POST, Some("application/json")),
    (
        "/v1/messages/count_tokens",
        Method::POST,
        Some("application/json"),
    ),
    ("/v1/models", Method::GET, None),
    (
        "/v1/images/generations",
        Method::POST,
        Some("application/json"),
    ),
    (
        "/v1/images/edits",
        Method::POST,
        Some("multipart/form-data; boundary=wokcore-boundary"),
    ),
];

struct Fixture {
    app: Router,
    management: String,
    proxy: String,
    non_proxy: Vec<String>,
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

async fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemorySecretStore::default());
    let metadata = Arc::new(StateAuthMetadataStore::new(
        StateStore::open(directory.path().join("state.sqlite3")).unwrap(),
    ));
    let entropy = Arc::new(IncrementingEntropy::default());
    let management_scope = SecretScope {
        provider_id: ProviderId::new("wokcore-runtime").unwrap(),
        account_id: None,
        purpose: SecretPurpose::Auxiliary,
    };
    let auth = Arc::new(
        AuthRegistry::bootstrap(
            secrets.clone(),
            metadata.clone(),
            entropy,
            management_scope,
            CREATED_AT.to_owned(),
        )
        .await
        .unwrap(),
    );
    let binding = metadata
        .runtime_secret_binding("management")
        .unwrap()
        .unwrap();
    let management = secrets
        .get(&binding.secret_ref)
        .await
        .unwrap()
        .expose_secret()
        .to_owned();
    let proxy = issue_token(&auth, 1, ClientTokenScope::ProxyUse).await;
    let mut non_proxy = Vec::new();
    for (index, scope) in [
        ClientTokenScope::SessionsRead,
        ClientTokenScope::UsageRead,
        ClientTokenScope::DiagnosticsRead,
        ClientTokenScope::DiagnosticsExport,
    ]
    .into_iter()
    .enumerate()
    {
        non_proxy.push(issue_token(&auth, index + 2, scope).await);
    }

    let lifecycle = ServiceLifecycle::new();
    lifecycle.mark_running().unwrap();
    let state = ServerState::new(
        AUTHORITY,
        uuid::Uuid::parse_str("019844f0-4de0-7000-8000-000000000130").unwrap(),
        auth,
        lifecycle.clone(),
    );
    Fixture {
        app: build_router(state),
        management,
        proxy,
        non_proxy,
        lifecycle,
        _directory: directory,
    }
}

async fn issue_token(auth: &AuthRegistry, index: usize, scope: ClientTokenScope) -> String {
    auth.issue_client_token_with_scopes(
        format!("019844f0-4de0-7000-8000-{index:012}"),
        ClientId::new(format!("client-{index}")).unwrap(),
        CREATED_AT.to_owned(),
        vec![scope],
    )
    .await
    .unwrap()
    .into_response_value()
    .expose_secret()
    .to_owned()
}

fn request(
    method: Method,
    path: &str,
    bearer: Option<&str>,
    content_type: Option<&str>,
    body: Body,
) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, AUTHORITY);
    if let Some(bearer) = bearer {
        request = request.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
    }
    if let Some(content_type) = content_type {
        request = request.header(header::CONTENT_TYPE, content_type);
    }
    request.body(body).unwrap()
}

fn valid_body(path: &str) -> Body {
    if path == "/v1/images/edits" {
        Body::from(multipart_body(&[1]))
    } else if path == "/v1/models" {
        Body::empty()
    } else {
        Body::from("{}")
    }
}

#[tokio::test]
async fn all_data_plane_routes_are_private_and_accept_only_proxy_scope() {
    let fixture = fixture().await;
    for (path, method, content_type) in DATA_PLANE_ROUTES {
        let unauthenticated = fixture
            .app
            .clone()
            .oneshot(request(
                method.clone(),
                path,
                None,
                content_type,
                valid_body(path),
            ))
            .await
            .unwrap();
        assert_protocol_error(
            unauthenticated,
            path,
            StatusCode::UNAUTHORIZED,
            "unauthorized",
        )
        .await;

        let accepted = fixture
            .app
            .clone()
            .oneshot(request(
                method,
                path,
                Some(&fixture.proxy),
                content_type,
                valid_body(path),
            ))
            .await
            .unwrap();
        assert_protocol_error(
            accepted,
            path,
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported_capability",
        )
        .await;
    }

    for token in &fixture.non_proxy {
        let response = fixture
            .app
            .clone()
            .oneshot(request(
                Method::POST,
                "/v1/responses",
                Some(token),
                Some("application/json"),
                Body::from("{}"),
            ))
            .await
            .unwrap();
        assert_protocol_error(
            response,
            "/v1/responses",
            StatusCode::FORBIDDEN,
            "insufficient_scope",
        )
        .await;
    }

    let management = fixture
        .app
        .oneshot(request(
            Method::POST,
            "/v1/responses",
            Some(&fixture.management),
            Some("application/json"),
            Body::from("{}"),
        ))
        .await
        .unwrap();
    assert_protocol_error(
        management,
        "/v1/responses",
        StatusCode::UNAUTHORIZED,
        "unauthorized",
    )
    .await;
}

#[tokio::test]
async fn authority_origin_method_head_and_content_type_are_strict() {
    let fixture = fixture().await;

    let invalid_host = Request::builder()
        .method(Method::POST)
        .uri("/v1/responses")
        .header(header::HOST, "localhost:43130")
        .header(header::AUTHORIZATION, format!("Bearer {}", fixture.proxy))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let response = fixture.app.clone().oneshot(invalid_host).await.unwrap();
    assert_protocol_error(
        response,
        "/v1/responses",
        StatusCode::BAD_REQUEST,
        "invalid_authority",
    )
    .await;

    let mut origin = request(
        Method::POST,
        "/v1/messages",
        Some(&fixture.proxy),
        Some("application/json"),
        Body::from("{}"),
    );
    origin
        .headers_mut()
        .insert(header::ORIGIN, "null".parse().unwrap());
    let response = fixture.app.clone().oneshot(origin).await.unwrap();
    assert_protocol_error(
        response,
        "/v1/messages",
        StatusCode::FORBIDDEN,
        "origin_not_allowed",
    )
    .await;

    for (method, path) in [
        (Method::GET, "/v1/responses"),
        (Method::HEAD, "/v1/models"),
        (Method::POST, "/v1/models"),
    ] {
        let is_head = method == Method::HEAD;
        let response = fixture
            .app
            .clone()
            .oneshot(request(
                method,
                path,
                Some(&fixture.proxy),
                None,
                Body::empty(),
            ))
            .await
            .unwrap();
        if is_head {
            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
            assert!(response.headers().contains_key("x-request-id"));
            assert!(to_bytes(response.into_body(), 1).await.unwrap().is_empty());
        } else {
            assert_protocol_error(
                response,
                path,
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
            )
            .await;
        }
    }

    for content_type in [
        None,
        Some("text/plain"),
        Some("application/vnd.example+json"),
        Some("application/json; charset=iso-8859-1"),
    ] {
        let response = fixture
            .app
            .clone()
            .oneshot(request(
                Method::POST,
                "/v1/responses",
                Some(&fixture.proxy),
                content_type,
                Body::from("{}"),
            ))
            .await
            .unwrap();
        assert_protocol_error(
            response,
            "/v1/responses",
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
        )
        .await;
    }

    let accepted_json = fixture
        .app
        .clone()
        .oneshot(request(
            Method::POST,
            "/v1/responses",
            Some(&fixture.proxy),
            Some("application/json; charset=utf-8"),
            Body::from("{}"),
        ))
        .await
        .unwrap();
    assert_eq!(accepted_json.status(), StatusCode::UNPROCESSABLE_ENTITY);

    for content_type in [None, Some("application/json"), Some("multipart/mixed")] {
        let response = fixture
            .app
            .clone()
            .oneshot(request(
                Method::POST,
                "/v1/images/edits",
                Some(&fixture.proxy),
                content_type,
                Body::from(multipart_body(&[1])),
            ))
            .await
            .unwrap();
        assert_protocol_error(
            response,
            "/v1/images/edits",
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
        )
        .await;
    }

    let models_with_body = fixture
        .app
        .oneshot(request(
            Method::GET,
            "/v1/models",
            Some(&fixture.proxy),
            Some("application/json"),
            Body::from("{}"),
        ))
        .await
        .unwrap();
    assert_protocol_error(
        models_with_body,
        "/v1/models",
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "unsupported_media_type",
    )
    .await;
}

#[tokio::test]
async fn data_plane_and_management_body_limits_remain_separate() {
    let fixture = fixture().await;
    let accepted_json = format!("\"{}\"", "a".repeat(JSON_BODY_LIMIT - 2));
    let accepted = fixture
        .app
        .clone()
        .oneshot(request(
            Method::POST,
            "/v1/responses",
            Some(&fixture.proxy),
            Some("application/json"),
            Body::from(accepted_json),
        ))
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let oversized_json = format!("\"{}\"", "a".repeat(JSON_BODY_LIMIT - 1));
    let oversized = fixture
        .app
        .clone()
        .oneshot(request(
            Method::POST,
            "/v1/responses",
            Some(&fixture.proxy),
            Some("application/json"),
            Body::from(oversized_json),
        ))
        .await
        .unwrap();
    assert_protocol_error(
        oversized,
        "/v1/responses",
        StatusCode::PAYLOAD_TOO_LARGE,
        "payload_too_large",
    )
    .await;

    let management = fixture
        .app
        .oneshot(request(
            Method::POST,
            "/wokcore/v1/clients/authorize",
            Some(&fixture.management),
            Some("application/json"),
            Body::from(format!(r#"{{"client_id":"{}"}}"#, "a".repeat(16 * 1024))),
        ))
        .await
        .unwrap();
    assert_eq!(management.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn image_parts_and_total_multipart_body_are_independently_bounded() {
    let fixture = fixture().await;
    let accepted = fixture
        .app
        .clone()
        .oneshot(request(
            Method::POST,
            "/v1/images/edits",
            Some(&fixture.proxy),
            Some("multipart/form-data; boundary=wokcore-boundary"),
            Body::from(multipart_body(&[IMAGE_PART_LIMIT])),
        ))
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let oversized_part = fixture
        .app
        .clone()
        .oneshot(request(
            Method::POST,
            "/v1/images/edits",
            Some(&fixture.proxy),
            Some("multipart/form-data; boundary=wokcore-boundary"),
            Body::from(multipart_body(&[IMAGE_PART_LIMIT + 1])),
        ))
        .await
        .unwrap();
    assert_protocol_error(
        oversized_part,
        "/v1/images/edits",
        StatusCode::PAYLOAD_TOO_LARGE,
        "payload_too_large",
    )
    .await;

    for field_name in ["mask", "future_image_field"] {
        let oversized_file_part = fixture
            .app
            .clone()
            .oneshot(request(
                Method::POST,
                "/v1/images/edits",
                Some(&fixture.proxy),
                Some("multipart/form-data; boundary=wokcore-boundary"),
                Body::from(multipart_fields(&[(field_name, IMAGE_PART_LIMIT + 1)])),
            ))
            .await
            .unwrap();
        assert_protocol_error(
            oversized_file_part,
            "/v1/images/edits",
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
        )
        .await;
    }

    let case_variant_filename = fixture
        .app
        .clone()
        .oneshot(request(
            Method::POST,
            "/v1/images/edits",
            Some(&fixture.proxy),
            Some("multipart/form-data; boundary=wokcore-boundary"),
            Body::from(multipart_case_variant_filename(IMAGE_PART_LIMIT + 1)),
        ))
        .await
        .unwrap();
    assert_protocol_error(
        case_variant_filename,
        "/v1/images/edits",
        StatusCode::PAYLOAD_TOO_LARGE,
        "payload_too_large",
    )
    .await;

    let over_total = multipart_body(&[
        MULTIPART_BODY_LIMIT / 3 + 1,
        MULTIPART_BODY_LIMIT / 3 + 1,
        MULTIPART_BODY_LIMIT / 3 + 1,
    ]);
    let oversized_total = fixture
        .app
        .oneshot(request(
            Method::POST,
            "/v1/images/edits",
            Some(&fixture.proxy),
            Some("multipart/form-data; boundary=wokcore-boundary"),
            Body::from(over_total),
        ))
        .await
        .unwrap();
    assert_protocol_error(
        oversized_total,
        "/v1/images/edits",
        StatusCode::PAYLOAD_TOO_LARGE,
        "payload_too_large",
    )
    .await;
}

#[tokio::test]
async fn maintenance_rejection_uses_the_active_client_protocol_shape() {
    let fixture = fixture().await;
    let existing = fixture
        .lifecycle
        .admission_controller()
        .try_enter()
        .unwrap();
    let lifecycle = fixture.lifecycle.clone();
    let drain = tokio::spawn(async move {
        lifecycle
            .begin_drain(Duration::from_millis(1))
            .await
            .unwrap()
    });
    wait_for_phase(&fixture.lifecycle, LifecyclePhase::Draining).await;

    for path in ["/v1/responses", "/v1/messages"] {
        let response = fixture
            .app
            .clone()
            .oneshot(request(
                Method::POST,
                path,
                Some(&fixture.proxy),
                Some("application/json"),
                Body::from("{}"),
            ))
            .await
            .unwrap();
        assert_protocol_error(
            response,
            path,
            StatusCode::SERVICE_UNAVAILABLE,
            "service_maintenance",
        )
        .await;
    }

    drop(existing);
    drain.await.unwrap();
    fixture.lifecycle.wait_for_zero_active().await;
}

#[tokio::test]
async fn admission_guard_covers_normal_decode_disconnect_cancellation_and_panic() {
    let fixture = fixture().await;

    let normal = fixture
        .app
        .clone()
        .oneshot(request(
            Method::POST,
            "/v1/responses",
            Some(&fixture.proxy),
            Some("application/json"),
            Body::from("{}"),
        ))
        .await
        .unwrap();
    assert_eq!(normal.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(fixture.lifecycle.snapshot().active_requests, 1);
    response_json(normal).await;
    assert_eq!(fixture.lifecycle.snapshot().active_requests, 0);

    let invalid = fixture
        .app
        .clone()
        .oneshot(request(
            Method::POST,
            "/v1/responses",
            Some(&fixture.proxy),
            Some("application/json"),
            Body::from("{"),
        ))
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert_eq!(fixture.lifecycle.snapshot().active_requests, 1);
    response_json(invalid).await;
    assert_eq!(fixture.lifecycle.snapshot().active_requests, 0);

    let disconnected = fixture
        .app
        .clone()
        .oneshot(request(
            Method::POST,
            "/v1/responses",
            Some(&fixture.proxy),
            Some("application/json"),
            Body::from_stream(DisconnectStream(false)),
        ))
        .await
        .unwrap();
    assert_eq!(disconnected.status(), StatusCode::BAD_REQUEST);
    assert_eq!(fixture.lifecycle.snapshot().active_requests, 1);
    response_json(disconnected).await;
    assert_eq!(fixture.lifecycle.snapshot().active_requests, 0);

    let pending = tokio::spawn(fixture.app.clone().oneshot(request(
        Method::POST,
        "/v1/responses",
        Some(&fixture.proxy),
        Some("application/json"),
        Body::from_stream(PendingStream),
    )));
    wait_for_active(&fixture.lifecycle, 1).await;
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    wait_for_active(&fixture.lifecycle, 0).await;

    let panicked = tokio::spawn(fixture.app.clone().oneshot(request(
        Method::POST,
        "/v1/responses",
        Some(&fixture.proxy),
        Some("application/json"),
        Body::from_stream(PanicStream),
    )));
    assert!(panicked.await.unwrap_err().is_panic());
    wait_for_active(&fixture.lifecycle, 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn proxy_admission_has_no_semaphore_or_global_wait_queue() {
    const REQUEST_COUNT: usize = 1_000;

    let fixture = fixture().await;
    let mut requests = Vec::with_capacity(REQUEST_COUNT);
    for _ in 0..REQUEST_COUNT {
        requests.push(tokio::spawn(fixture.app.clone().oneshot(request(
            Method::POST,
            "/v1/responses",
            Some(&fixture.proxy),
            Some("application/json"),
            Body::from_stream(PendingStream),
        ))));
    }

    wait_for_active(&fixture.lifecycle, REQUEST_COUNT).await;
    for request in requests {
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
    }
    wait_for_active(&fixture.lifecycle, 0).await;
}

fn multipart_body(image_sizes: &[usize]) -> Vec<u8> {
    let fields = image_sizes
        .iter()
        .copied()
        .map(|size| ("image", size))
        .collect::<Vec<_>>();
    multipart_fields(&fields)
}

fn multipart_fields(fields: &[(&str, usize)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (index, (name, image_size)) in fields.iter().copied().enumerate() {
        body.extend_from_slice(b"--wokcore-boundary\r\n");
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{name}\"; filename=\"image-{index}.png\"\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
        body.resize(body.len() + image_size, b'x');
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"--wokcore-boundary--\r\n");
    body
}

fn multipart_case_variant_filename(size: usize) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(b"--wokcore-boundary\r\n");
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"future_image_field\"; Filename=\"image.png\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
    body.resize(body.len() + size, b'x');
    body.extend_from_slice(b"\r\n--wokcore-boundary--\r\n");
    body
}

async fn assert_protocol_error(
    response: axum::response::Response,
    path: &str,
    status: StatusCode,
    code: &str,
) {
    assert_eq!(response.status(), status, "{path}");
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let body = response_json(response).await;
    if path == "/v1/messages" || path == "/v1/messages/count_tokens" {
        assert_eq!(body["type"], "error", "{path}: {body}");
        assert_eq!(body["error"]["type"], code, "{path}: {body}");
        assert_eq!(body["request_id"], request_id, "{path}: {body}");
    } else {
        assert_eq!(body["error"]["type"], "gateway_error", "{path}: {body}");
        assert_eq!(body["error"]["code"], code, "{path}: {body}");
        assert_eq!(body["error"]["request_id"], request_id, "{path}: {body}");
    }
}

async fn response_json(response: axum::response::Response) -> Value {
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

async fn wait_for_active(lifecycle: &ServiceLifecycle, expected: usize) {
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if lifecycle.snapshot().active_requests == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn wait_for_phase(lifecycle: &ServiceLifecycle, expected: LifecyclePhase) {
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if lifecycle.snapshot().phase == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

struct PendingStream;

impl Stream for PendingStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Pending
    }
}

struct DisconnectStream(bool);

impl Stream for DisconnectStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.0 {
            Poll::Ready(None)
        } else {
            self.0 = true;
            Poll::Ready(Some(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "synthetic client disconnect",
            ))))
        }
    }
}

struct PanicStream;

impl Stream for PanicStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        panic!("synthetic request body panic")
    }
}
