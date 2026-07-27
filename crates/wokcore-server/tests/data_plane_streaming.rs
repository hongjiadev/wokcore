use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use tower::ServiceExt;
use wokcore_core::{
    config::{
        AccountAuthConfig, AccountConfig, ProviderConfig, ProviderInstanceConfig, RouteTarget,
        RoutingConfig,
    },
    id::{AccountId, ClientId, ProviderId},
    secret::{SecretPurpose, SecretScope},
};
use wokcore_diagnostics::runtime::StreamRuntimeDiagnostics;
use wokcore_engine::{
    accounts::{AccountHealthPolicy, AccountHealthTable},
    execution::ExecutionCancellation,
};
use wokcore_protocols::{
    canonical::{CanonicalEvent, GatewayError, Usage},
    stream::{SseDecoder, SseFrame},
};
use wokcore_server::{
    ServerState,
    api::build_router,
    auth::{AuthRegistry, EntropySource, StateAuthMetadataStore, TokenError},
    data_plane::{
        SafeUpstreamRequestId, UPSTREAM_STREAM_CHANNEL_CAPACITY, UpstreamExecutionFailure,
        UpstreamExecutionRequest, UpstreamExecutionResult, UpstreamExecutionStream,
        UpstreamExecutor, UpstreamFailureKind, UpstreamStreamSendError,
    },
    lifecycle::ServiceLifecycle,
    providers::{ProviderCandidate, ProviderManagement},
};
use wokcore_storage::{ClientTokenScope, MemorySecretStore, StateStore};

const AUTHORITY: &str = "127.0.0.1:43132";
const CREATED_AT: &str = "2026-07-27T00:00:00Z";
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StreamMode {
    Normal,
    FailBeforeVisibleOnce,
    FailAfterVisible,
    MalformedAfterVisible,
    Rich,
    PendingAfterVisible,
}

struct SyntheticStreamingExecutor {
    mode: Mutex<StreamMode>,
    calls: AtomicUsize,
    cancellations: Mutex<Vec<ExecutionCancellation>>,
}

impl SyntheticStreamingExecutor {
    fn new(mode: StreamMode) -> Self {
        Self {
            mode: Mutex::new(mode),
            calls: AtomicUsize::new(0),
            cancellations: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }

    fn any_cancelled(&self) -> bool {
        self.cancellations
            .lock()
            .unwrap()
            .iter()
            .any(ExecutionCancellation::is_cancelled)
    }
}

#[async_trait]
impl UpstreamExecutor for SyntheticStreamingExecutor {
    async fn execute(
        &self,
        request: UpstreamExecutionRequest,
        cancellation: ExecutionCancellation,
    ) -> UpstreamExecutionResult {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.cancellations
            .lock()
            .unwrap()
            .push(cancellation.clone());
        if !request.canonical().stream {
            return UpstreamExecutionResult::Failed(UpstreamExecutionFailure::new(
                UpstreamFailureKind::InvalidRequest,
            ));
        }

        let mode = *self.mode.lock().unwrap();
        let (sender, stream) = UpstreamExecutionStream::channel(1_722_000_000);
        let mut stream = stream
            .with_initial_usage(Usage {
                input_tokens: 11,
                output_tokens: 0,
                cached_input_tokens: Some(2),
                reasoning_tokens: None,
                extensions: BTreeMap::new(),
            })
            .unwrap()
            .with_upstream_request_id(
                SafeUpstreamRequestId::new(format!(
                    "upstream_stream_{}",
                    request.attempt_ordinal()
                ))
                .unwrap(),
            );
        if mode == StreamMode::Rich {
            stream = stream
                .with_thinking_signatures(BTreeMap::from([(
                    "reasoning_safe_1".to_owned(),
                    "signature_safe_1".to_owned(),
                )]))
                .unwrap();
        }

        let attempt = request.attempt_ordinal();
        tokio::spawn(async move {
            if mode == StreamMode::FailBeforeVisibleOnce && attempt == 1 {
                let _ = sender
                    .send_failure(
                        UpstreamExecutionFailure::new(UpstreamFailureKind::Reset).with_status(502),
                    )
                    .await;
                return;
            }
            if sender
                .send_event(CanonicalEvent::Created {
                    response_id: "resp_stream_safe_1".to_owned(),
                })
                .await
                .is_err()
            {
                return;
            }
            match mode {
                StreamMode::FailAfterVisible => {
                    let _ = sender
                        .send_failure(
                            UpstreamExecutionFailure::new(UpstreamFailureKind::Server)
                                .with_status(503),
                        )
                        .await;
                }
                StreamMode::MalformedAfterVisible => {
                    let _ = sender
                        .send_event(CanonicalEvent::Created {
                            response_id: "duplicate_stream_id".to_owned(),
                        })
                        .await;
                    let _ = sender.send_event(CanonicalEvent::Completed).await;
                }
                StreamMode::PendingAfterVisible => {
                    cancellation.cancelled().await;
                }
                StreamMode::Rich => {
                    send_events(
                        &sender,
                        [
                            CanonicalEvent::ReasoningDelta {
                                item_id: "reasoning_safe_1".to_owned(),
                                delta: "synthetic reasoning".to_owned(),
                            },
                            CanonicalEvent::ToolCallDelta {
                                item_id: "tool_safe_1".to_owned(),
                                call_id: "call_safe_1".to_owned(),
                                name: "lookup".to_owned(),
                                delta: "{\"query\":\"synthetic\"}".to_owned(),
                            },
                            CanonicalEvent::OutputTextDelta {
                                item_id: "text_safe_1".to_owned(),
                                delta: "synthetic reply".to_owned(),
                            },
                            usage_event(),
                            CanonicalEvent::Completed,
                        ],
                    )
                    .await;
                }
                StreamMode::Normal | StreamMode::FailBeforeVisibleOnce => {
                    send_events(
                        &sender,
                        [
                            CanonicalEvent::OutputTextDelta {
                                item_id: "text_safe_1".to_owned(),
                                delta: "synthetic reply".to_owned(),
                            },
                            usage_event(),
                            CanonicalEvent::Completed,
                        ],
                    )
                    .await;
                }
            }
        });
        UpstreamExecutionResult::Streaming(stream)
    }
}

async fn send_events<const N: usize>(
    sender: &wokcore_server::data_plane::UpstreamStreamSender,
    events: [CanonicalEvent; N],
) {
    for event in events {
        if sender.send_event(event).await.is_err() {
            break;
        }
    }
}

fn usage_event() -> CanonicalEvent {
    CanonicalEvent::Usage(Usage {
        input_tokens: 11,
        output_tokens: 3,
        cached_input_tokens: Some(2),
        reasoning_tokens: Some(1),
        extensions: BTreeMap::new(),
    })
}

struct Fixture {
    app: Router,
    proxy: String,
    executor: Arc<SyntheticStreamingExecutor>,
    diagnostics: StreamRuntimeDiagnostics,
    directory: tempfile::TempDir,
}

async fn fixture(mode: StreamMode) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemorySecretStore::default());
    let metadata = Arc::new(StateAuthMetadataStore::new(
        StateStore::open(directory.path().join("state.sqlite3")).unwrap(),
    ));
    let auth = Arc::new(
        AuthRegistry::bootstrap(
            secrets.clone(),
            metadata,
            Arc::new(IncrementingEntropy::default()),
            SecretScope {
                provider_id: provider("wokcore-runtime"),
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
            "019844f0-4de0-7000-8000-000000000132".to_owned(),
            ClientId::new("wokrouter").unwrap(),
            CREATED_AT.to_owned(),
            vec![ClientTokenScope::ProxyUse],
        )
        .await
        .unwrap()
        .into_response_value()
        .expose_secret()
        .to_owned();

    let providers = Arc::new(
        ProviderManagement::open(directory.path().join("config.toml"), secrets)
            .expect("provider management"),
    );
    let first = providers
        .create_secret(secret_scope("first"), SecretString::from("synthetic-first"))
        .await
        .unwrap();
    let second = providers
        .create_secret(
            secret_scope("second"),
            SecretString::from("synthetic-second"),
        )
        .await
        .unwrap();
    providers
        .commit(0, provider_candidate(first.secret_ref, second.secret_ref))
        .await
        .unwrap();

    let health = Arc::new(
        AccountHealthTable::new(
            AccountHealthPolicy::new(1, 100).unwrap(),
            &[account("first"), account("second")],
        )
        .unwrap(),
    );
    let executor = Arc::new(SyntheticStreamingExecutor::new(mode));
    let diagnostics = StreamRuntimeDiagnostics::default();
    let lifecycle = ServiceLifecycle::new();
    lifecycle.mark_running().unwrap();
    let state = ServerState::new(
        AUTHORITY,
        uuid::Uuid::parse_str("019844f0-4de0-7000-8000-000000000132").unwrap(),
        auth,
        lifecycle,
    )
    .with_provider_management(providers)
    .with_upstream_executor(executor.clone(), health)
    .with_stream_diagnostics(diagnostics.clone());
    Fixture {
        app: build_router(state),
        proxy,
        executor,
        diagnostics,
        directory,
    }
}

#[tokio::test]
async fn streaming_all_three_text_protocols_preserve_client_event_order() {
    let fixture = fixture(StreamMode::Normal).await;
    let cases = [
        (
            "/v1/responses",
            json!({"model":"gpt-5.6","input":"hello","stream":true}),
            ["event: response.created", "event: response.completed"],
        ),
        (
            "/v1/chat/completions",
            json!({
                "model":"gpt-5.6",
                "messages":[{"role":"user","content":"hello"}],
                "stream":true
            }),
            ["\"object\":\"chat.completion.chunk\"", "data: [DONE]"],
        ),
        (
            "/v1/messages",
            json!({
                "model":"gpt-5.6",
                "max_tokens":64,
                "messages":[{"role":"user","content":"hello"}],
                "stream":true
            }),
            ["event: message_start", "event: message_stop"],
        ),
    ];

    for (path, request_body, markers) in cases {
        let response = send_json(&fixture, path, request_body).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );
        assert_eq!(
            response.headers()["x-upstream-request-id"],
            "upstream_stream_1"
        );
        let body = response_text(response).await;
        assert_in_order(&body, &markers);
        assert!(body.contains("synthetic reply"), "{path}: {body}");
    }
}

#[tokio::test]
async fn streaming_retries_only_before_the_first_visible_event() {
    let before = fixture(StreamMode::FailBeforeVisibleOnce).await;
    let response = send_json(
        &before,
        "/v1/responses",
        json!({"model":"gpt-5.6","input":"hello","stream":true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response_text(response).await.contains("response.completed"));
    assert_eq!(before.executor.calls(), 2);

    let after = fixture(StreamMode::FailAfterVisible).await;
    let response = send_json(
        &after,
        "/v1/responses",
        json!({"model":"gpt-5.6","input":"hello","stream":true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_text(response).await;
    assert_eq!(body.matches("event: error").count(), 1);
    assert!(!body.contains("response.completed"));
    assert_eq!(after.executor.calls(), 1);
}

#[tokio::test]
async fn streaming_channel_capacity_two_backpressures_a_slow_consumer() {
    let (sender, stream) = UpstreamExecutionStream::channel(1_722_000_000);
    assert_eq!(UPSTREAM_STREAM_CHANNEL_CAPACITY, 2);
    assert_eq!(sender.max_capacity(), 2);
    sender
        .send_event(CanonicalEvent::Created {
            response_id: "resp_safe_1".to_owned(),
        })
        .await
        .unwrap();
    sender
        .send_event(CanonicalEvent::OutputTextDelta {
            item_id: "text_safe_1".to_owned(),
            delta: "first".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(sender.remaining_capacity(), 0);

    let blocked = tokio::time::timeout(
        Duration::from_millis(25),
        sender.send_event(CanonicalEvent::OutputTextDelta {
            item_id: "text_safe_1".to_owned(),
            delta: "blocked".to_owned(),
        }),
    )
    .await;
    assert!(blocked.is_err(), "the third event bypassed backpressure");
    drop(stream);
    assert_eq!(
        sender
            .send_event(CanonicalEvent::Completed)
            .await
            .unwrap_err(),
        UpstreamStreamSendError::Closed
    );
}

#[tokio::test]
async fn streaming_client_body_drop_promptly_cancels_upstream() {
    let fixture = fixture(StreamMode::PendingAfterVisible).await;
    let response = send_json(
        &fixture,
        "/v1/responses",
        json!({"model":"gpt-5.6","input":"hello","stream":true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);

    wait_for(|| fixture.executor.any_cancelled()).await;
    wait_for(|| fixture.diagnostics.snapshot().cancelled == 1).await;
}

#[tokio::test]
async fn streaming_malformed_input_emits_one_protocol_error_then_terminates() {
    let fixture = fixture(StreamMode::MalformedAfterVisible).await;
    let response = send_json(
        &fixture,
        "/v1/responses",
        json!({"model":"gpt-5.6","input":"hello","stream":true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_text(response).await;
    assert_eq!(body.matches("event: error").count(), 1);
    assert!(!body.contains("event: response.completed"));
    let snapshot = fixture.diagnostics.snapshot();
    assert_eq!(snapshot.protocol_errors, 1);
    assert_eq!(snapshot.active, 0);
}

#[tokio::test]
async fn streaming_reasoning_tools_usage_and_finish_preserve_order() {
    let fixture = fixture(StreamMode::Rich).await;
    let response = send_json(
        &fixture,
        "/v1/responses",
        json!({"model":"gpt-5.6","input":"hello","stream":true}),
    )
    .await;
    let body = response_text(response).await;
    assert_in_order(
        &body,
        &[
            "response.reasoning_summary_text.delta",
            "response.function_call_arguments.delta",
            "response.output_text.delta",
            "response.completed",
        ],
    );

    let response = send_json(
        &fixture,
        "/v1/messages",
        json!({
            "model":"gpt-5.6",
            "max_tokens":64,
            "messages":[{"role":"user","content":"hello"}],
            "stream":true
        }),
    )
    .await;
    let body = response_text(response).await;
    assert_in_order(
        &body,
        &[
            "thinking_delta",
            "input_json_delta",
            "text_delta",
            "message_delta",
            "message_stop",
        ],
    );
}

#[tokio::test]
async fn streaming_diagnostics_are_coarse_and_emit_no_file_writes() {
    let fixture = fixture(StreamMode::Normal).await;
    let before = snapshot_files(fixture.directory.path());
    let response = send_json(
        &fixture,
        "/v1/responses",
        json!({
            "model":"gpt-5.6",
            "input":"diagnostic-payload-canary",
            "stream":true
        }),
    )
    .await;
    assert!(response_text(response).await.contains("response.completed"));
    let after = snapshot_files(fixture.directory.path());

    assert_eq!(after, before, "streaming wrote to a runtime file");
    let snapshot = fixture.diagnostics.snapshot();
    assert_eq!(snapshot.active, 0);
    assert_eq!(snapshot.completed, 1);
    assert!(snapshot.frames > 0);
    assert!(snapshot.bytes > 0);
    assert!(!format!("{:?}", fixture.diagnostics).contains("payload-canary"));
}

#[test]
fn streaming_upstream_sse_handles_fragmented_utf8_comments_and_keepalives() {
    let input = ":\u{20}keepalive\n\
                 event: delta\n\
                 data: {\"text\":\"你好\"}\n\n\
                 : heartbeat\n\n\
                 data: [DONE]\n\n";
    let bytes = input.as_bytes();
    let mut decoder = SseDecoder::new(1024);
    let mut frames = Vec::new();
    for byte in bytes {
        frames.extend(decoder.push(std::slice::from_ref(byte)).unwrap());
    }
    decoder.finish().unwrap();

    assert_eq!(
        frames,
        [
            SseFrame {
                event: Some("delta".to_owned()),
                data: "{\"text\":\"你好\"}".to_owned(),
            },
            SseFrame {
                event: None,
                data: "[DONE]".to_owned(),
            },
        ]
    );
}

#[test]
fn streaming_queue_rejects_oversized_or_failed_canonical_events() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (sender, stream) = UpstreamExecutionStream::channel(1_722_000_000);
        assert_eq!(
            sender
                .send_event(CanonicalEvent::Created {
                    response_id: "x".repeat(513),
                })
                .await
                .unwrap_err(),
            UpstreamStreamSendError::InvalidEvent
        );
        assert_eq!(
            sender
                .send_event(CanonicalEvent::OutputTextDelta {
                    item_id: "text_safe_1".to_owned(),
                    delta: "x".repeat(1024 * 1024),
                })
                .await
                .unwrap_err(),
            UpstreamStreamSendError::InvalidEvent
        );
        assert_eq!(
            sender
                .send_event(CanonicalEvent::Failed(GatewayError::transport(
                    "hidden diagnostic",
                )))
                .await
                .unwrap_err(),
            UpstreamStreamSendError::InvalidEvent
        );
        assert!(stream.with_stop_sequence("x".repeat(513)).is_err());
        let (_, stream) = UpstreamExecutionStream::channel(1_722_000_000);
        assert!(
            stream
                .with_thinking_signatures(BTreeMap::from([
                    (String::new(), "signature".to_owned(),)
                ]))
                .is_err()
        );
    });
}

async fn send_json(fixture: &Fixture, path: &str, body: Value) -> axum::response::Response {
    fixture
        .app
        .clone()
        .oneshot(request(
            Method::POST,
            path,
            &fixture.proxy,
            Body::from(body.to_string()),
        ))
        .await
        .unwrap()
}

fn request(method: Method, path: &str, proxy: &str, body: Body) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, AUTHORITY)
        .header(header::AUTHORIZATION, format!("Bearer {proxy}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .unwrap()
}

async fn response_text(response: axum::response::Response) -> String {
    let body = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    String::from_utf8(body.to_vec()).unwrap()
}

fn assert_in_order(body: &str, markers: &[&str]) {
    let mut offset = 0;
    for marker in markers {
        let position = body[offset..]
            .find(marker)
            .unwrap_or_else(|| panic!("missing {marker:?} after byte {offset}: {body}"));
        offset += position + marker.len();
    }
}

async fn wait_for(predicate: impl Fn() -> bool) {
    tokio::time::timeout(TEST_TIMEOUT, async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

fn snapshot_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, snapshot: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, snapshot);
            } else {
                snapshot.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }

    let mut snapshot = BTreeMap::new();
    visit(root, root, &mut snapshot);
    snapshot
}

fn provider_candidate(
    first_secret: wokcore_core::secret::SecretRef,
    second_secret: wokcore_core::secret::SecretRef,
) -> ProviderCandidate {
    ProviderCandidate {
        providers: ProviderConfig {
            instances: vec![ProviderInstanceConfig {
                id: provider("primary"),
                catalog_id: provider("openai-apikey"),
                enabled: true,
                endpoint: None,
                allow_private_network: false,
            }],
            accounts: vec![
                AccountConfig {
                    id: account("first"),
                    provider: provider("primary"),
                    enabled: true,
                    auth: AccountAuthConfig::ApiKey {
                        secret: first_secret,
                    },
                },
                AccountConfig {
                    id: account("second"),
                    provider: provider("primary"),
                    enabled: true,
                    auth: AccountAuthConfig::ApiKey {
                        secret: second_secret,
                    },
                },
            ],
        },
        routing: RoutingConfig {
            aliases: Vec::new(),
            rules: Vec::new(),
            default: Some(RouteTarget {
                provider: provider("primary"),
                model: "gpt-5.6".to_owned(),
            }),
        },
    }
}

fn secret_scope(account_id: &str) -> SecretScope {
    SecretScope {
        provider_id: provider("primary"),
        account_id: Some(account(account_id)),
        purpose: SecretPurpose::ApiKey,
    }
}

fn provider(value: &str) -> ProviderId {
    ProviderId::new(value).unwrap()
}

fn account(value: &str) -> AccountId {
    AccountId::new(value).unwrap()
}

#[derive(Default)]
struct IncrementingEntropy(AtomicU8);

impl EntropySource for IncrementingEntropy {
    fn fill(&self, output: &mut [u8; 32]) -> Result<(), TokenError> {
        output.fill(self.0.fetch_add(1, Ordering::SeqCst).wrapping_add(1));
        Ok(())
    }
}
