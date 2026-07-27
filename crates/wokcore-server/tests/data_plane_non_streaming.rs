use std::{
    collections::BTreeMap,
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
use wokcore_engine::{
    accounts::{AccountHealthPolicy, AccountHealthTable, AccountObservation},
    execution::ExecutionCancellation,
};
use wokcore_protocols::canonical::{CanonicalEvent, GatewayError, Usage};
use wokcore_server::{
    ServerState,
    api::build_router,
    auth::{AuthRegistry, EntropySource, StateAuthMetadataStore, TokenError},
    data_plane::{
        SafeUpstreamRequestId, UpstreamExecutionFailure, UpstreamExecutionRequest,
        UpstreamExecutionResponse, UpstreamExecutionResult, UpstreamExecutionStream,
        UpstreamExecutor, UpstreamFailureKind, UpstreamOperation,
    },
    lifecycle::ServiceLifecycle,
    providers::{ProviderCandidate, ProviderManagement},
};
use wokcore_storage::{ClientTokenScope, MemorySecretStore, StateStore};

const AUTHORITY: &str = "127.0.0.1:43131";
const CREATED_AT: &str = "2026-07-27T00:00:00Z";
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Success,
    Length,
    Timeout,
    Cancelled,
    Transport,
    Malformed,
    RateLimited,
    Server,
    Pending,
}

struct SyntheticExecutor {
    mode: Mutex<Mode>,
    calls: AtomicUsize,
    cancellation: Mutex<Option<ExecutionCancellation>>,
    requests: Mutex<Vec<(String, UpstreamOperation, String, String)>>,
}

impl SyntheticExecutor {
    fn new(mode: Mode) -> Self {
        Self {
            mode: Mutex::new(mode),
            calls: AtomicUsize::new(0),
            cancellation: Mutex::new(None),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn set_mode(&self, mode: Mode) {
        *self.mode.lock().unwrap() = mode;
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }
}

#[async_trait]
impl UpstreamExecutor for SyntheticExecutor {
    async fn execute(
        &self,
        request: UpstreamExecutionRequest,
        cancellation: ExecutionCancellation,
    ) -> UpstreamExecutionResult {
        self.calls.fetch_add(1, Ordering::AcqRel);
        *self.cancellation.lock().unwrap() = Some(cancellation.clone());
        self.requests.lock().unwrap().push((
            request.request_id().to_owned(),
            request.operation(),
            request.model().to_owned(),
            request.account_id().as_str().to_owned(),
        ));
        let mode = *self.mode.lock().unwrap();
        match mode {
            Mode::Success if request.operation() == UpstreamOperation::CountTokens => {
                UpstreamExecutionResult::Succeeded(UpstreamExecutionResponse::token_count(37))
            }
            Mode::Success | Mode::Length => {
                let usage = Usage {
                    input_tokens: 11,
                    output_tokens: 3,
                    cached_input_tokens: Some(2),
                    reasoning_tokens: None,
                    extensions: BTreeMap::new(),
                };
                let events = vec![
                    CanonicalEvent::Created {
                        response_id: "resp_safe_1".to_owned(),
                    },
                    CanonicalEvent::OutputTextDelta {
                        item_id: "item_safe_1".to_owned(),
                        delta: "synthetic reply".to_owned(),
                    },
                    CanonicalEvent::Usage(usage.clone()),
                    CanonicalEvent::Completed,
                ];
                if request.canonical().stream {
                    let (sender, stream) = UpstreamExecutionStream::channel(1_722_000_000);
                    let mut stream = stream
                        .with_initial_usage(Usage {
                            output_tokens: 0,
                            reasoning_tokens: None,
                            ..usage
                        })
                        .unwrap()
                        .with_upstream_request_id(
                            SafeUpstreamRequestId::new("upstream_safe_1").unwrap(),
                        );
                    if mode == Mode::Length {
                        stream = stream.with_finish_reason(
                            wokcore_server::data_plane::UpstreamFinishReason::Length,
                        );
                    }
                    tokio::spawn(async move {
                        for event in events {
                            if sender.send_event(event).await.is_err() {
                                break;
                            }
                        }
                    });
                    return UpstreamExecutionResult::Streaming(stream);
                }
                let mut response = UpstreamExecutionResponse::events(events, 1_722_000_000)
                    .unwrap()
                    .with_upstream_request_id(
                        SafeUpstreamRequestId::new("upstream_safe_1").unwrap(),
                    );
                if mode == Mode::Length {
                    response = response.with_finish_reason(
                        wokcore_server::data_plane::UpstreamFinishReason::Length,
                    );
                }
                UpstreamExecutionResult::Succeeded(response)
            }
            Mode::Timeout => UpstreamExecutionResult::Failed(
                UpstreamExecutionFailure::new(UpstreamFailureKind::Timeout)
                    .with_upstream_request_id(
                        SafeUpstreamRequestId::new("upstream_timeout").unwrap(),
                    ),
            ),
            Mode::Cancelled => UpstreamExecutionResult::Failed(UpstreamExecutionFailure::new(
                UpstreamFailureKind::Cancelled,
            )),
            Mode::Transport => UpstreamExecutionResult::Failed(UpstreamExecutionFailure::new(
                UpstreamFailureKind::Transport,
            )),
            Mode::Malformed => UpstreamExecutionResult::Failed(
                UpstreamExecutionFailure::new(UpstreamFailureKind::MalformedResponse)
                    .with_status(502),
            ),
            Mode::RateLimited => UpstreamExecutionResult::Failed(
                UpstreamExecutionFailure::new(UpstreamFailureKind::RateLimited)
                    .with_status(429)
                    .with_retry_after_ms(1),
            ),
            Mode::Server => UpstreamExecutionResult::Failed(
                UpstreamExecutionFailure::new(UpstreamFailureKind::Server).with_status(503),
            ),
            Mode::Pending => {
                cancellation.cancelled().await;
                UpstreamExecutionResult::Failed(UpstreamExecutionFailure::new(
                    UpstreamFailureKind::Cancelled,
                ))
            }
        }
    }
}

struct Fixture {
    app: Router,
    proxy: String,
    executor: Arc<SyntheticExecutor>,
    health: Arc<AccountHealthTable>,
    providers: Arc<ProviderManagement>,
    _directory: tempfile::TempDir,
}

async fn fixture(catalog_id: &str) -> Fixture {
    configured_fixture(catalog_id, true).await
}

async fn accountless_fixture(catalog_id: &str) -> Fixture {
    configured_fixture(catalog_id, false).await
}

async fn configured_fixture(catalog_id: &str, with_accounts: bool) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let secrets = Arc::new(MemorySecretStore::default());
    let metadata = Arc::new(StateAuthMetadataStore::new(
        StateStore::open(directory.path().join("state.sqlite3")).unwrap(),
    ));
    let entropy = Arc::new(IncrementingEntropy::default());
    let management_scope = SecretScope {
        provider_id: provider("wokcore-runtime"),
        account_id: None,
        purpose: SecretPurpose::Auxiliary,
    };
    let auth = Arc::new(
        AuthRegistry::bootstrap(
            secrets.clone(),
            metadata,
            entropy,
            management_scope,
            CREATED_AT.to_owned(),
        )
        .await
        .unwrap(),
    );
    let proxy = auth
        .issue_client_token_with_scopes(
            "019844f0-4de0-7000-8000-000000000131".to_owned(),
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
    let first_secret = providers
        .create_secret(secret_scope("first"), SecretString::from("synthetic-first"))
        .await
        .unwrap();
    let second_secret = providers
        .create_secret(
            secret_scope("second"),
            SecretString::from("synthetic-second"),
        )
        .await
        .unwrap();
    providers
        .commit(
            0,
            provider_candidate(
                catalog_id,
                first_secret.secret_ref,
                second_secret.secret_ref,
                with_accounts,
            ),
        )
        .await
        .unwrap();

    let health_accounts = if with_accounts {
        vec![account("first"), account("second")]
    } else {
        Vec::new()
    };
    let health = Arc::new(
        AccountHealthTable::new(AccountHealthPolicy::new(1, 100).unwrap(), &health_accounts)
            .unwrap(),
    );
    let executor = Arc::new(SyntheticExecutor::new(Mode::Success));
    let lifecycle = ServiceLifecycle::new();
    lifecycle.mark_running().unwrap();
    let state = ServerState::new(
        AUTHORITY,
        uuid::Uuid::parse_str("019844f0-4de0-7000-8000-000000000131").unwrap(),
        auth,
        lifecycle,
    )
    .with_provider_management(providers.clone())
    .with_upstream_executor(executor.clone(), health.clone());
    Fixture {
        app: build_router(state),
        proxy,
        executor,
        health,
        providers,
        _directory: directory,
    }
}

#[tokio::test]
async fn all_three_text_protocols_decode_route_execute_and_encode_non_streaming_responses() {
    let fixture = fixture("openai-apikey").await;
    let cases = [
        (
            "/v1/responses",
            json!({"model":"gpt-5.6","input":"hello","stream":false}),
        ),
        (
            "/v1/chat/completions",
            json!({
                "model":"gpt-5.6",
                "messages":[{"role":"user","content":"hello"}],
                "stream":false
            }),
        ),
        (
            "/v1/messages",
            json!({
                "model":"gpt-5.6",
                "max_tokens":64,
                "messages":[{"role":"user","content":"hello"}],
                "stream":false
            }),
        ),
    ];

    for (path, body) in cases {
        let response = send_json(&fixture, path, body).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let client_request_id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            response.headers()["x-upstream-request-id"],
            "upstream_safe_1"
        );
        let body = response_json(response).await;
        assert_eq!(body_text(path, &body), "synthetic reply", "{path}: {body}");
        let observed = fixture.executor.requests.lock().unwrap();
        let (executor_request_id, operation, model, _) = observed.last().unwrap();
        assert_eq!(executor_request_id, &client_request_id);
        assert_eq!(*operation, UpstreamOperation::Text);
        assert_eq!(model, "gpt-5.6");
    }
    assert_eq!(fixture.executor.calls(), 3);
}

#[tokio::test]
async fn streaming_text_requests_return_protocol_sse_instead_of_a_capability_error() {
    let fixture = fixture("openai-apikey").await;
    let response = send_json(
        &fixture,
        "/v1/responses",
        json!({"model":"gpt-5.6","input":"hello","stream":true}),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    let body = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    let body = std::str::from_utf8(&body).unwrap();
    assert!(body.contains("event: response.created"));
    assert!(body.contains("event: response.completed"));
}

#[tokio::test]
async fn count_tokens_uses_local_bounded_estimation_without_an_adapter_endpoint() {
    let fixture = fixture("openai-apikey").await;
    let response = send_json(
        &fixture,
        "/v1/messages/count_tokens",
        json!({
            "model":"gpt-5.6",
            "messages":[{"role":"user","content":"a bounded local estimate"}],
            "stream":false
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert!(
        body["input_tokens"]
            .as_u64()
            .is_some_and(|tokens| tokens > 0)
    );
    assert_eq!(fixture.executor.calls(), 0);
}

#[tokio::test]
async fn count_tokens_uses_the_injected_endpoint_for_the_anthropic_adapter() {
    let fixture = fixture("anthropic-apikey").await;
    let response = send_json(
        &fixture,
        "/v1/messages/count_tokens",
        json!({
            "model":"claude-sonnet-4-5",
            "messages":[{"role":"user","content":"hello"}],
            "stream":false
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_json(response).await["input_tokens"], 37);
    assert_eq!(fixture.executor.calls(), 1);
    assert_eq!(
        fixture.executor.requests.lock().unwrap()[0].1,
        UpstreamOperation::CountTokens
    );
}

#[tokio::test]
async fn anthropic_compatible_provider_without_count_capability_uses_local_estimation() {
    let fixture = fixture("umans").await;
    let response = send_json(
        &fixture,
        "/v1/messages/count_tokens",
        json!({
            "model":"umans-coder",
            "messages":[{"role":"user","content":"hello"}],
            "stream":false
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response_json(response).await["input_tokens"]
            .as_u64()
            .is_some_and(|tokens| tokens > 0)
    );
    assert_eq!(fixture.executor.calls(), 0);
}

#[tokio::test]
async fn an_unavailable_preferred_auth_group_falls_back_before_the_first_attempt() {
    let fixture = fixture("xai").await;
    fixture
        .health
        .observe(&account("first"), AccountObservation::InvalidCredentials, 0)
        .unwrap();

    let response = send_json(
        &fixture,
        "/v1/chat/completions",
        json!({
            "model":"grok-4.5",
            "messages":[{"role":"user","content":"hello"}],
            "stream":false
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fixture.executor.calls(), 1);
    assert_eq!(fixture.executor.requests.lock().unwrap()[0].3, "second");
}

#[tokio::test]
async fn accountless_local_provider_executes_through_the_coordinator() {
    let fixture = accountless_fixture("ollama").await;
    let response = send_json(
        &fixture,
        "/v1/chat/completions",
        json!({
            "model":"synthetic-local",
            "messages":[{"role":"user","content":"hello"}],
            "stream":false
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fixture.executor.calls(), 1);
    assert_eq!(
        fixture.executor.requests.lock().unwrap()[0].3,
        "accountless"
    );
}

#[tokio::test]
async fn provider_commit_atomically_refreshes_the_execution_health_snapshot() {
    let fixture = fixture("openai-apikey").await;
    let secret = fixture
        .providers
        .create_secret(secret_scope("third"), SecretString::from("synthetic-third"))
        .await
        .unwrap();
    let status = fixture.providers.status();
    let mut candidate = ProviderCandidate {
        providers: status.providers,
        routing: status.routing,
    };
    candidate.providers.accounts = vec![AccountConfig {
        id: account("third"),
        provider: provider("primary"),
        enabled: true,
        auth: AccountAuthConfig::ApiKey {
            secret: secret.secret_ref,
        },
    }];
    fixture
        .providers
        .commit(status.revision, candidate)
        .await
        .unwrap();

    let response = send_json(
        &fixture,
        "/v1/responses",
        json!({"model":"gpt-5.6","input":"hello","stream":false}),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fixture.executor.calls(), 1);
    assert_eq!(fixture.executor.requests.lock().unwrap()[0].3, "third");
}

#[tokio::test]
async fn responses_reflect_request_metadata_and_length_termination() {
    let fixture = fixture("openai-apikey").await;
    fixture.executor.set_mode(Mode::Length);
    let response = send_json(
        &fixture,
        "/v1/responses",
        json!({
            "model":"gpt-5.6",
            "input":"hello",
            "instructions":"be concise",
            "max_output_tokens":128,
            "metadata":{"suite":"synthetic"},
            "parallel_tool_calls":true,
            "previous_response_id":"resp_previous",
            "reasoning":{"effort":"low","summary":"auto"},
            "store":true,
            "temperature":0.25,
            "text":{"format":{"type":"text"},"verbosity":"low"},
            "tool_choice":"required",
            "tools":[{
                "type":"function",
                "name":"lookup",
                "description":"synthetic lookup",
                "parameters":{"type":"object","properties":{}}
            }],
            "top_p":0.75,
            "truncation":"auto",
            "user":"synthetic-user",
            "stream":false
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["status"], "incomplete");
    assert!(body["completed_at"].is_null());
    assert_eq!(body["incomplete_details"]["reason"], "max_output_tokens");
    assert_eq!(body["instructions"], "be concise");
    assert_eq!(body["max_output_tokens"], 128);
    assert_eq!(body["metadata"]["suite"], "synthetic");
    assert_eq!(body["parallel_tool_calls"], true);
    assert_eq!(body["previous_response_id"], "resp_previous");
    assert_eq!(body["reasoning"]["effort"], "low");
    assert_eq!(body["reasoning"]["summary"], "auto");
    assert_eq!(body["store"], true);
    assert_eq!(body["temperature"], 0.25);
    assert_eq!(body["text"]["verbosity"], "low");
    assert_eq!(body["tool_choice"], "required");
    assert_eq!(body["tools"][0]["name"], "lookup");
    assert_eq!(body["top_p"], 0.75);
    assert_eq!(body["truncation"], "auto");
    assert_eq!(body["user"], "synthetic-user");
}

#[tokio::test]
async fn invalid_responses_metadata_is_rejected_before_executor_invocation() {
    let fixture = fixture("openai-apikey").await;
    let response = send_json(
        &fixture,
        "/v1/responses",
        json!({
            "model":"gpt-5.6",
            "input":"must not execute",
            "store":"invalid",
            "stream":false
        }),
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(fixture.executor.calls(), 0);
}

#[tokio::test]
async fn models_reads_one_local_snapshot_and_never_calls_the_executor() {
    let fixture = fixture("openai-apikey").await;
    let response = fixture
        .app
        .clone()
        .oneshot(request(
            Method::GET,
            "/v1/models",
            &fixture.proxy,
            Body::empty(),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["object"], "list");
    assert!(
        body["data"]
            .as_array()
            .is_some_and(|models| !models.is_empty())
    );
    assert_eq!(fixture.executor.calls(), 0);
}

#[tokio::test]
async fn stable_errors_cover_timeout_malformed_rate_limit_and_server_failures() {
    let fixture = fixture("openai-apikey").await;
    for (mode, status, code, upstream_request_id) in [
        (
            Mode::Timeout,
            StatusCode::BAD_GATEWAY,
            "upstream_unavailable",
            Some("upstream_timeout"),
        ),
        (
            Mode::Cancelled,
            StatusCode::BAD_GATEWAY,
            "upstream_unavailable",
            None,
        ),
        (
            Mode::Transport,
            StatusCode::BAD_GATEWAY,
            "upstream_unavailable",
            None,
        ),
        (
            Mode::Malformed,
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            None,
        ),
        (
            Mode::RateLimited,
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            None,
        ),
        (
            Mode::Server,
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            None,
        ),
    ] {
        fixture.executor.set_mode(mode);
        let response = send_json(
            &fixture,
            "/v1/responses",
            json!({
                "model":"gpt-5.6",
                "input":"request-body-canary-must-not-escape",
                "stream":false
            }),
        )
        .await;
        assert_eq!(response.status(), status, "{mode:?}");
        assert_eq!(
            response
                .headers()
                .get("x-upstream-request-id")
                .and_then(|value| value.to_str().ok()),
            upstream_request_id,
            "{mode:?}"
        );
        let body = response_json(response).await;
        assert_eq!(body["error"]["code"], code, "{mode:?}: {body}");
        assert!(!body.to_string().contains("request-body-canary"));
    }
}

#[tokio::test]
async fn dropping_the_client_request_cancels_the_executor() {
    let fixture = fixture("openai-apikey").await;
    fixture.executor.set_mode(Mode::Pending);
    let request = request(
        Method::POST,
        "/v1/responses",
        &fixture.proxy,
        Body::from(json!({"model":"gpt-5.6","input":"hello","stream":false}).to_string()),
    );
    let task = tokio::spawn(fixture.app.clone().oneshot(request));
    wait_for(|| fixture.executor.calls() == 1).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    wait_for(|| {
        fixture
            .executor
            .cancellation
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(ExecutionCancellation::is_cancelled)
    })
    .await;
}

#[test]
fn executor_output_rejects_oversized_non_streaming_event_batches() {
    let response = UpstreamExecutionResponse::events(
        vec![CanonicalEvent::OutputTextDelta {
            item_id: "item_safe_1".to_owned(),
            delta: "x".repeat(16 * 1024 * 1024),
        }],
        1_722_000_000,
    );

    assert!(response.is_err());
}

#[test]
fn executor_success_output_rejects_failed_events_with_hidden_diagnostics() {
    let response = UpstreamExecutionResponse::events(
        vec![CanonicalEvent::Failed(GatewayError::transport(
            "x".repeat(17 * 1024 * 1024),
        ))],
        1_722_000_000,
    );

    assert!(response.is_err());
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
    let has_json_body = method == Method::POST;
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, AUTHORITY)
        .header(header::AUTHORIZATION, format!("Bearer {proxy}"));
    if has_json_body {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    request.body(body).unwrap()
}

async fn response_json(response: axum::response::Response) -> Value {
    let body = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn body_text<'a>(path: &str, body: &'a Value) -> &'a str {
    match path {
        "/v1/responses" => body["output"][0]["content"][0]["text"].as_str().unwrap(),
        "/v1/chat/completions" => body["choices"][0]["message"]["content"].as_str().unwrap(),
        "/v1/messages" => body["content"][0]["text"].as_str().unwrap(),
        _ => unreachable!(),
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

fn provider_candidate(
    catalog_id: &str,
    first_secret: wokcore_core::secret::SecretRef,
    second_secret: wokcore_core::secret::SecretRef,
    with_accounts: bool,
) -> ProviderCandidate {
    let accounts = if with_accounts {
        vec![
            AccountConfig {
                id: account("first"),
                provider: provider("primary"),
                enabled: true,
                auth: if catalog_id == "xai" {
                    AccountAuthConfig::Oauth {
                        access: first_secret,
                        refresh: None,
                    }
                } else {
                    AccountAuthConfig::ApiKey {
                        secret: first_secret,
                    }
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
        ]
    } else {
        Vec::new()
    };
    ProviderCandidate {
        providers: ProviderConfig {
            instances: vec![ProviderInstanceConfig {
                id: provider("primary"),
                catalog_id: provider(catalog_id),
                enabled: true,
                endpoint: None,
                allow_private_network: false,
            }],
            accounts,
        },
        routing: RoutingConfig {
            aliases: Vec::new(),
            rules: Vec::new(),
            default: Some(RouteTarget {
                provider: provider("primary"),
                model: match catalog_id {
                    "anthropic-apikey" => "claude-sonnet-4-5",
                    "umans" => "umans-coder",
                    "xai" => "grok-4.5",
                    "ollama" => "synthetic-local",
                    _ => "gpt-5.6",
                }
                .to_owned(),
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
