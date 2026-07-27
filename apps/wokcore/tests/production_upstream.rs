use std::{
    collections::BTreeMap,
    io::Write,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use secrecy::SecretString;
use tokio::{net::TcpListener, sync::oneshot};
use wokcore::runtime::ProductionUpstreamExecutor;
use wokcore_core::{
    config::AccountAuthConfig,
    id::{AccountId, ProviderId},
    secret::SecretRef,
};
use wokcore_engine::{
    auth::{SecretResolutionError, SecretResolver},
    catalog::AdapterFamily,
    execution::ExecutionCancellation,
    routing::EndpointAccess,
    transport::{PooledTransport, TransportLimits, TransportTimeouts},
};
use wokcore_protocols::{
    canonical::{CanonicalEvent, CanonicalRequest, InputItem, PublicModelId, RequestId},
    images::{ImageEditMetadata, ImageGenerationRequest},
};
use wokcore_server::data_plane::{
    ImageEditRequest, ImageExecutionInput, ImageExecutionRequest, ImageExecutionResult,
    ImageInputFile, UpstreamExecutionOutput, UpstreamExecutionRequest, UpstreamExecutionResult,
    UpstreamExecutor, UpstreamFailureKind, UpstreamOperation,
};

const SECRET_CANARY: &str = "production-upstream-secret-canary";

struct LoopbackServer {
    address: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
}

impl LoopbackServer {
    async fn start(router: Router) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, receiver) = oneshot::channel();
        tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = receiver.await;
                })
                .await
                .unwrap();
        });
        Self {
            address,
            shutdown: Some(shutdown),
        }
    }

    fn endpoint(&self) -> String {
        format!("http://{}/v1", self.address)
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

#[derive(Clone, Default)]
struct Captured {
    headers: Arc<Mutex<Option<HeaderMap>>>,
    body: Arc<Mutex<Option<serde_json::Value>>>,
}

async fn responses(
    State(captured): State<Captured>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    *captured
        .headers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(headers);
    *captured
        .body
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(body);
    axum::Json(serde_json::json!({
        "id": "resp_loopback",
        "output": [{
            "id": "message_0",
            "type": "message",
            "content": [{"type": "output_text", "text": "offline hello"}]
        }],
        "usage": {"input_tokens": 4, "output_tokens": 2}
    }))
}

async fn rate_limited() -> (StatusCode, [(&'static str, &'static str); 1]) {
    (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "7")])
}

async fn image_generation(
    State(captured): State<Captured>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    *captured
        .headers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(headers);
    *captured
        .body
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(body);
    axum::Json(serde_json::json!({
        "created": 1722000000,
        "data": [{"b64_json":"aGVsbG8="}]
    }))
}

async fn image_edit(
    State(captured): State<Captured>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::Json<serde_json::Value> {
    *captured
        .headers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(headers);
    *captured
        .body
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some(serde_json::json!({"multipart": String::from_utf8(body.to_vec()).unwrap()}));
    axum::Json(serde_json::json!({
        "created": 1722000000,
        "data": [{"url":"https://example.invalid/edited.png"}]
    }))
}

struct FixedResolver {
    expected: SecretRef,
    reads: Arc<Mutex<usize>>,
}

#[async_trait]
impl SecretResolver for FixedResolver {
    async fn resolve(&self, secret_ref: &SecretRef) -> Result<SecretString, SecretResolutionError> {
        if secret_ref != &self.expected {
            return Err(SecretResolutionError);
        }
        *self
            .reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        Ok(SecretString::from(SECRET_CANARY))
    }
}

fn canonical(stream: bool) -> CanonicalRequest {
    CanonicalRequest {
        request_id: RequestId::new("req_loopback"),
        model: PublicModelId::new("model-loopback"),
        thread_key: None,
        input: vec![InputItem::Text {
            text: "hello".to_owned(),
        }],
        tools: Vec::new(),
        stream,
        reasoning: None,
        extensions: BTreeMap::new(),
    }
}

fn execution_request(
    endpoint: String,
    secret_ref: SecretRef,
    stream: bool,
) -> UpstreamExecutionRequest {
    execution_request_with_canonical(endpoint, secret_ref, canonical(stream))
}

fn execution_request_with_canonical(
    endpoint: String,
    secret_ref: SecretRef,
    canonical: CanonicalRequest,
) -> UpstreamExecutionRequest {
    UpstreamExecutionRequest::new(
        "req_loopback",
        1,
        UpstreamOperation::Text,
        ProviderId::new("loopback-provider").unwrap(),
        AccountId::new("loopback-account").unwrap(),
        AdapterFamily::OpenAiResponses,
        endpoint,
        EndpointAccess::LoopbackOnly,
        "model-loopback",
        AccountAuthConfig::ApiKey { secret: secret_ref },
        canonical,
    )
    .unwrap()
}

fn executor(secret_ref: SecretRef, reads: Arc<Mutex<usize>>) -> ProductionUpstreamExecutor {
    ProductionUpstreamExecutor::new(
        PooledTransport::new(TransportTimeouts::default(), TransportLimits::default()).unwrap(),
        Arc::new(FixedResolver {
            expected: secret_ref,
            reads,
        }),
    )
}

#[tokio::test]
async fn production_upstream_executes_loopback_without_leaking_credentials() {
    let captured = Captured::default();
    let server = LoopbackServer::start(
        Router::new()
            .route("/v1/responses", post(responses))
            .with_state(captured.clone()),
    )
    .await;
    let secret_ref = SecretRef::new();
    let reads = Arc::new(Mutex::new(0));
    let executor = executor(secret_ref.clone(), Arc::clone(&reads));

    let result = executor
        .execute(
            execution_request(server.endpoint(), secret_ref, false),
            ExecutionCancellation::new(),
        )
        .await;

    let response = match result {
        UpstreamExecutionResult::Succeeded(response) => response,
        other => panic!("unexpected execution result: {other:?}"),
    };
    let events = match response.output() {
        UpstreamExecutionOutput::Events(events) => events,
        UpstreamExecutionOutput::TokenCount(_) => panic!("expected text events"),
    };
    assert!(events.iter().any(|event| matches!(
        event,
        CanonicalEvent::OutputTextDelta { delta, .. } if delta == "offline hello"
    )));
    assert_eq!(
        *reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        1
    );

    let headers = captured
        .headers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .unwrap();
    assert_eq!(
        headers.get("authorization").unwrap(),
        &format!("Bearer {SECRET_CANARY}")
    );
    let body = captured
        .body
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .unwrap();
    assert_eq!(body["model"], "model-loopback");

    let rendered = format!("{response:?}");
    assert!(!rendered.contains(SECRET_CANARY));
}

#[tokio::test]
async fn production_upstream_executes_bounded_image_generation_on_loopback() {
    let captured = Captured::default();
    let server = LoopbackServer::start(
        Router::new()
            .route("/v1/images/generations", post(image_generation))
            .with_state(captured.clone()),
    )
    .await;
    let secret_ref = SecretRef::new();
    let reads = Arc::new(Mutex::new(0));
    let executor = executor(secret_ref.clone(), Arc::clone(&reads));
    let input = ImageExecutionInput::Generation(
        ImageGenerationRequest::decode(
            br#"{"model":"public-image","prompt":"offline image","response_format":"b64_json"}"#,
        )
        .unwrap(),
    );
    let request = ImageExecutionRequest::new(
        "req_image_loopback",
        ProviderId::new("loopback-provider").unwrap(),
        AccountId::new("loopback-account").unwrap(),
        AdapterFamily::OpenAiResponses,
        server.endpoint(),
        EndpointAccess::LoopbackOnly,
        "routed-image-model",
        AccountAuthConfig::ApiKey { secret: secret_ref },
        input,
    )
    .unwrap();

    let result = executor
        .execute_image(request, ExecutionCancellation::new())
        .await;

    let response = match result {
        ImageExecutionResult::Succeeded(response) => response,
        other => panic!("unexpected image execution result: {other:?}"),
    };
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(response.body()).unwrap()["data"][0]["b64_json"],
        "aGVsbG8="
    );
    assert_eq!(
        captured
            .body
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap()["model"],
        "routed-image-model"
    );
    assert_eq!(
        *reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        1
    );
}

#[tokio::test]
async fn production_upstream_streams_image_edit_and_cleans_the_temporary_file() {
    let captured = Captured::default();
    let server = LoopbackServer::start(
        Router::new()
            .route("/v1/images/edits", post(image_edit))
            .with_state(captured.clone()),
    )
    .await;
    let secret_ref = SecretRef::new();
    let executor = executor(secret_ref.clone(), Arc::new(Mutex::new(0)));
    let mut temporary = tempfile::NamedTempFile::new().unwrap();
    temporary.write_all(b"offline-image-bytes").unwrap();
    temporary.flush().unwrap();
    let temporary_path = temporary.path().to_owned();
    let input_file =
        ImageInputFile::from_named_temp("image", "input.png", "image/png", temporary).unwrap();
    let metadata = ImageEditMetadata::from_fields([
        ("model", "public-image"),
        ("prompt", "remove background"),
    ])
    .unwrap();
    let input =
        ImageExecutionInput::Edit(ImageEditRequest::new(metadata, vec![input_file]).unwrap());
    let request = ImageExecutionRequest::new(
        "req_image_edit_loopback",
        ProviderId::new("loopback-provider").unwrap(),
        AccountId::new("loopback-account").unwrap(),
        AdapterFamily::OpenAiResponses,
        server.endpoint(),
        EndpointAccess::LoopbackOnly,
        "routed-image-model",
        AccountAuthConfig::ApiKey { secret: secret_ref },
        input,
    )
    .unwrap();

    let result = executor
        .execute_image(request, ExecutionCancellation::new())
        .await;

    assert!(matches!(result, ImageExecutionResult::Succeeded(_)));
    let body = captured
        .body
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .unwrap()["multipart"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(body.contains("name=\"model\"\r\n\r\nrouted-image-model"));
    assert!(body.contains("name=\"prompt\"\r\n\r\nremove background"));
    assert!(body.contains("filename=\"input.png\""));
    assert!(body.contains("offline-image-bytes"));
    assert!(!temporary_path.exists());
}

#[tokio::test]
async fn production_upstream_maps_bounded_http_failures() {
    let server =
        LoopbackServer::start(Router::new().route("/v1/responses", post(rate_limited))).await;
    let secret_ref = SecretRef::new();
    let executor = executor(secret_ref.clone(), Arc::new(Mutex::new(0)));

    let result = executor
        .execute(
            execution_request(server.endpoint(), secret_ref, false),
            ExecutionCancellation::new(),
        )
        .await;

    let failure = match result {
        UpstreamExecutionResult::Failed(failure) => failure,
        other => panic!("unexpected execution result: {other:?}"),
    };
    assert_eq!(failure.kind(), UpstreamFailureKind::RateLimited);
    assert_eq!(failure.status(), Some(429));
    assert_eq!(failure.retry_after_ms(), Some(7_000));
}

#[tokio::test]
async fn production_upstream_classifies_oversized_input_as_invalid_request() {
    let secret_ref = SecretRef::new();
    let executor = executor(secret_ref.clone(), Arc::new(Mutex::new(0)));
    let mut request = canonical(false);
    request.input = vec![InputItem::Text {
        text: "x".repeat(1024 * 1024 + 1),
    }];

    let result = executor
        .execute(
            execution_request_with_canonical(
                "http://127.0.0.1:9/v1".to_owned(),
                secret_ref,
                request,
            ),
            ExecutionCancellation::new(),
        )
        .await;

    let failure = match result {
        UpstreamExecutionResult::Failed(failure) => failure,
        other => panic!("unexpected execution result: {other:?}"),
    };
    assert_eq!(failure.kind(), UpstreamFailureKind::InvalidRequest);
}
