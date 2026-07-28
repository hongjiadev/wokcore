use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicU8, Ordering},
};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Notify,
    task::yield_now,
    time::{Duration, timeout},
};
use tower::ServiceExt;
use uuid::Uuid;
use wokcore_core::{
    id::{ClientId, ProviderId},
    secret::{SecretPurpose, SecretRef, SecretScope},
};
use wokcore_server::{
    RunningServer, ServerState,
    api::build_router,
    auth::{AuthMetadataStore, AuthRegistry, EntropySource, TokenError, TokenMaterial},
    lifecycle::{DrainOutcome, ServiceLifecycle},
    runtime::{TokenMetadataError, TokenMetadataSource},
};
use wokcore_storage::{
    ClientTokenMetadata, ClientTokenScope, MemorySecretStore, RuntimeSecretBinding, SecretStore,
    StorageError,
};

const AUTHORITY: &str = "127.0.0.1:43128";
const CREATED_AT: &str = "2026-07-26T00:00:00Z";
const INSTANCE_ID: &str = "019844f0-4de0-7000-8000-000000000002";
const TOKEN_ID: &str = "019844f0-4de0-7000-8000-000000000003";

#[derive(Debug, Default)]
struct IncrementingEntropy(AtomicU8);

impl EntropySource for IncrementingEntropy {
    fn fill(&self, output: &mut [u8; 32]) -> Result<(), TokenError> {
        output.fill(self.0.fetch_add(1, Ordering::SeqCst).wrapping_add(1));
        Ok(())
    }
}

#[derive(Debug)]
struct FixedTokenMetadata;

impl TokenMetadataSource for FixedTokenMetadata {
    fn new_token_id(&self) -> Result<String, TokenMetadataError> {
        Ok(TOKEN_ID.to_owned())
    }

    fn now(&self) -> Result<String, TokenMetadataError> {
        Ok(CREATED_AT.to_owned())
    }
}

#[derive(Default)]
struct TestMetadata {
    binding: Mutex<Option<RuntimeSecretBinding>>,
    active: Mutex<Vec<ClientTokenMetadata>>,
    issue_gate: MutationGate,
    revoke_gate: MutationGate,
}

#[derive(Default)]
struct MutationGate {
    armed: AtomicBool,
    entered: Notify,
    released: Mutex<bool>,
    release: Condvar,
}

impl MutationGate {
    fn arm(&self) {
        *self.released.lock().unwrap() = false;
        self.armed.store(true, Ordering::SeqCst);
    }

    async fn wait_until_entered(&self) {
        timeout(Duration::from_secs(5), self.entered.notified())
            .await
            .unwrap();
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }

    fn block_if_armed(&self) {
        if !self.armed.swap(false, Ordering::SeqCst) {
            return;
        }
        self.entered.notify_one();
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.release.wait(released).unwrap();
        }
    }
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
        Ok(self.active.lock().unwrap().clone())
    }

    fn issue_client_token(&self, token: &ClientTokenMetadata) -> Result<(), StorageError> {
        self.issue_gate.block_if_armed();
        self.active.lock().unwrap().push(token.clone());
        Ok(())
    }

    fn issue_client_token_with_scopes(
        &self,
        token: &ClientTokenMetadata,
        _scopes: &[ClientTokenScope],
    ) -> Result<(), StorageError> {
        self.issue_client_token(token)
    }

    fn revoke_client_token(
        &self,
        client_id: &ClientId,
        token_id: &str,
        _revoked_at: &str,
    ) -> Result<bool, StorageError> {
        self.revoke_gate.block_if_armed();
        let mut active = self.active.lock().unwrap();
        let before = active.len();
        active.retain(|token| token.client_id != *client_id || token.token_id != token_id);
        Ok(active.len() != before)
    }
}

struct Fixture {
    app: Router,
    management: String,
    lifecycle: ServiceLifecycle,
    metadata: Arc<TestMetadata>,
}

async fn fixture() -> Fixture {
    let (state, management, lifecycle, metadata) = state_fixture(AUTHORITY).await;
    Fixture {
        app: build_router(state),
        management,
        lifecycle,
        metadata,
    }
}

async fn state_fixture(
    authority: &str,
) -> (ServerState, String, ServiceLifecycle, Arc<TestMetadata>) {
    let entropy = Arc::new(IncrementingEntropy::default());
    let expected_management = TokenMaterial::generate_admin(entropy.as_ref())
        .unwrap()
        .into_response_value();
    let management = expected_management.expose_secret().to_owned();
    let secrets = Arc::new(MemorySecretStore::default());
    let scope = SecretScope {
        provider_id: ProviderId::new("wokcore-runtime").unwrap(),
        account_id: None,
        purpose: SecretPurpose::Auxiliary,
    };
    let secret_ref = secrets.put(&scope, expected_management).await.unwrap();
    let metadata = Arc::new(TestMetadata::default());
    *metadata.binding.lock().unwrap() = Some(RuntimeSecretBinding {
        name: "management".to_owned(),
        secret_ref,
        revision: 1,
        created_at: CREATED_AT.to_owned(),
    });
    let auth = AuthRegistry::bootstrap(
        secrets,
        metadata.clone(),
        entropy,
        scope,
        CREATED_AT.to_owned(),
    )
    .await
    .unwrap();
    let lifecycle = ServiceLifecycle::new();
    lifecycle.mark_running().unwrap();
    let state = ServerState::new_with_token_metadata(
        authority.to_owned(),
        Uuid::parse_str(INSTANCE_ID).unwrap(),
        Arc::new(auth),
        lifecycle.clone(),
        Arc::new(FixedTokenMetadata),
    );
    (state, management, lifecycle, metadata)
}

fn request(
    method: &str,
    path: &str,
    management: Option<&str>,
    body: Option<Value>,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, AUTHORITY);
    if let Some(management) = management {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {management}"));
    }
    if body.is_some() {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    builder
        .body(match body {
            Some(value) => Body::from(serde_json::to_vec(&value).unwrap()),
            None => Body::empty(),
        })
        .unwrap()
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn assert_listener_closes(address: std::net::SocketAddr) {
    timeout(Duration::from_secs(5), async {
        loop {
            match TcpStream::connect(address).await {
                Ok(connection) => {
                    drop(connection);
                    yield_now().await;
                }
                Err(_) => return,
            }
        }
    })
    .await
    .unwrap();
}

async fn wait_for_active_token_count(metadata: &TestMetadata, expected: usize) {
    timeout(Duration::from_secs(5), async {
        loop {
            if metadata.active.lock().unwrap().len() == expected {
                return;
            }
            yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn health_and_capabilities_are_public_minimal_and_versioned() {
    let fixture = fixture().await;
    let health = fixture
        .app
        .clone()
        .oneshot(request("GET", "/wokcore/v1/health", None, None))
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    assert_eq!(
        json_body(health).await,
        json!({"status":"ok","instance_id":INSTANCE_ID})
    );

    let capabilities = fixture
        .app
        .oneshot(request("GET", "/wokcore/v1/capabilities", None, None))
        .await
        .unwrap();
    assert_eq!(capabilities.status(), StatusCode::OK);
    let body = json_body(capabilities).await;
    assert_eq!(body["management_api_major"], 1);
    assert_eq!(body["minimum_management_api_major"], 1);
    assert_eq!(body["maximum_management_api_major"], 1);
    assert_eq!(body["instance_id"], INSTANCE_ID);
    let installation_id = body["installation_id"].as_str().unwrap();
    assert_eq!(installation_id.len(), 64);
    assert!(
        installation_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    assert_eq!(
        body["capabilities"],
        json!([
            "client_token.issue",
            "client_token.inspect",
            "client_token.revoke",
            "diagnostics.events.v1",
            "diagnostics.export.v1",
            "discovery.v1",
            "provider.catalog.v1",
            "provider.config.v1",
            "provider.models.v1",
            "provider.secrets.v1",
            "service.drain",
            "service.status",
            "sessions.index.v1",
            "sessions.messages.v1",
            "usage.session.v1"
        ])
    );
    assert_eq!(
        body["provider_protocols"],
        json!([
            "anthropic.messages.v1",
            "azure.openai.v1",
            "cursor.connect.v1",
            "google.gemini.v1",
            "openai.chat_completions.v1",
            "openai.responses.v1"
        ])
    );
    assert!(!body.to_string().contains("account"));
    assert!(!body.to_string().contains("configured_provider"));
}

#[tokio::test]
async fn management_status_drain_cancel_and_maintenance_rules_follow_lifecycle() {
    let fixture = fixture().await;
    let status = fixture
        .app
        .clone()
        .oneshot(request(
            "GET",
            "/wokcore/v1/service/status",
            Some(&fixture.management),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(json_body(status).await["phase"], "running");

    let drain = fixture
        .app
        .clone()
        .oneshot(request(
            "POST",
            "/wokcore/v1/service/drain",
            Some(&fixture.management),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(drain.status(), StatusCode::OK);
    assert_eq!(json_body(drain).await["phase"], "draining");

    let blocked = fixture
        .app
        .clone()
        .oneshot(request(
            "POST",
            "/wokcore/v1/clients/authorize",
            Some(&fixture.management),
            Some(json!({"client_id":"wokrouter"})),
        ))
        .await
        .unwrap();
    assert_eq!(blocked.status(), StatusCode::CONFLICT);
    assert_eq!(
        json_body(blocked).await["error"]["code"],
        "service_maintenance"
    );

    let cancel = fixture
        .app
        .oneshot(request(
            "POST",
            "/wokcore/v1/service/drain/cancel",
            Some(&fixture.management),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(cancel.status(), StatusCode::OK);
    assert_eq!(json_body(cancel).await["phase"], "running");
}

#[tokio::test]
async fn authorize_returns_raw_proxy_once_and_revoke_removes_it() {
    let fixture = fixture().await;
    let authorize = fixture
        .app
        .clone()
        .oneshot(request(
            "POST",
            "/wokcore/v1/clients/authorize",
            Some(&fixture.management),
            Some(json!({"client_id":"wokrouter"})),
        ))
        .await
        .unwrap();
    assert_eq!(authorize.status(), StatusCode::CREATED);
    assert_eq!(authorize.headers()[header::CACHE_CONTROL], "no-store");
    let authorized = json_body(authorize).await;
    let token = authorized["token"].as_str().unwrap();
    let token_id = authorized["token_id"].as_str().unwrap();
    assert!(token.starts_with("wok_proxy_v1_"));
    assert_eq!(authorized["client_id"], "wokrouter");
    assert_eq!(token_id, TOKEN_ID);
    assert_eq!(
        fixture.metadata.active.lock().unwrap()[0].issued_at,
        CREATED_AT
    );

    let path = format!("/wokcore/v1/clients/wokrouter/tokens/{token_id}");
    let revoked = fixture
        .app
        .oneshot(request("DELETE", &path, Some(&fixture.management), None))
        .await
        .unwrap();
    assert_eq!(revoked.status(), StatusCode::OK);
    assert_eq!(json_body(revoked).await, json!({"revoked":true}));
}

#[tokio::test]
async fn authorize_accepts_a_preallocated_token_id_and_reports_its_active_state() {
    let fixture = fixture().await;
    let requested_token_id = "019844f0-4de0-7000-8000-000000000099";
    let authorize = fixture
        .app
        .clone()
        .oneshot(request(
            "POST",
            "/wokcore/v1/clients/authorize",
            Some(&fixture.management),
            Some(json!({
                "client_id": "wokrouter",
                "token_id": requested_token_id
            })),
        ))
        .await
        .unwrap();
    assert_eq!(authorize.status(), StatusCode::CREATED);
    assert_eq!(json_body(authorize).await["token_id"], requested_token_id);

    let path = format!("/wokcore/v1/clients/wokrouter/tokens/{requested_token_id}");
    let active = fixture
        .app
        .clone()
        .oneshot(request("GET", &path, Some(&fixture.management), None))
        .await
        .unwrap();
    assert_eq!(active.status(), StatusCode::OK);
    assert_eq!(json_body(active).await, json!({"active":true}));

    let revoked = fixture
        .app
        .clone()
        .oneshot(request("DELETE", &path, Some(&fixture.management), None))
        .await
        .unwrap();
    assert_eq!(revoked.status(), StatusCode::OK);
    let inactive = fixture
        .app
        .oneshot(request("GET", &path, Some(&fixture.management), None))
        .await
        .unwrap();
    assert_eq!(inactive.status(), StatusCode::OK);
    assert_eq!(json_body(inactive).await, json!({"active":false}));
}

#[tokio::test]
async fn authorize_accepts_explicit_scopes_and_enforces_them_independently() {
    let fixture = fixture().await;
    let authorize = fixture
        .app
        .clone()
        .oneshot(request(
            "POST",
            "/wokcore/v1/clients/authorize",
            Some(&fixture.management),
            Some(json!({
                "client_id": "wokrouter",
                "scopes": ["sessions.read", "diagnostics.read"]
            })),
        ))
        .await
        .unwrap();
    assert_eq!(authorize.status(), StatusCode::CREATED);
    let authorized = json_body(authorize).await;
    assert_eq!(
        authorized["scopes"],
        json!(["sessions.read", "diagnostics.read"])
    );
    let token = authorized["token"].as_str().unwrap();

    let sessions = fixture
        .app
        .clone()
        .oneshot(request("GET", "/wokcore/v1/sessions", Some(token), None))
        .await
        .unwrap();
    assert_eq!(sessions.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json_body(sessions).await["error"]["code"], "query_busy");

    let usage = fixture
        .app
        .oneshot(request("GET", "/wokcore/v1/usage", Some(token), None))
        .await
        .unwrap();
    assert_eq!(usage.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        json_body(usage).await["error"]["code"],
        "insufficient_scope"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_authorize_stays_admitted_until_owned_issue_finishes() {
    let fixture = fixture().await;
    fixture.metadata.issue_gate.arm();
    let incoming = request(
        "POST",
        "/wokcore/v1/clients/authorize",
        Some(&fixture.management),
        Some(json!({"client_id":"wokrouter"})),
    );
    let app = fixture.app.clone();
    let caller = tokio::spawn(async move { app.oneshot(incoming).await.unwrap() });
    fixture.metadata.issue_gate.wait_until_entered().await;

    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let drain = fixture
        .lifecycle
        .begin_drain(Duration::from_millis(25))
        .await
        .unwrap();
    let stop_while_blocked = fixture.lifecycle.request_stop();
    fixture.metadata.issue_gate.release();
    wait_for_active_token_count(&fixture.metadata, 1).await;
    timeout(
        Duration::from_secs(5),
        fixture.lifecycle.wait_for_zero_active(),
    )
    .await
    .unwrap();

    assert_eq!(drain, DrainOutcome::TimedOutAwaitingCancellation);
    assert!(stop_while_blocked.is_err());
    assert!(fixture.lifecycle.request_stop().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_revoke_stays_admitted_until_owned_revoke_finishes() {
    let fixture = fixture().await;
    let authorize = fixture
        .app
        .clone()
        .oneshot(request(
            "POST",
            "/wokcore/v1/clients/authorize",
            Some(&fixture.management),
            Some(json!({"client_id":"wokrouter"})),
        ))
        .await
        .unwrap();
    let token_id = json_body(authorize).await["token_id"]
        .as_str()
        .unwrap()
        .to_owned();
    fixture.metadata.revoke_gate.arm();
    let incoming = request(
        "DELETE",
        &format!("/wokcore/v1/clients/wokrouter/tokens/{token_id}"),
        Some(&fixture.management),
        None,
    );
    let app = fixture.app.clone();
    let caller = tokio::spawn(async move { app.oneshot(incoming).await.unwrap() });
    fixture.metadata.revoke_gate.wait_until_entered().await;

    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let drain = fixture
        .lifecycle
        .begin_drain(Duration::from_millis(25))
        .await
        .unwrap();
    let stop_while_blocked = fixture.lifecycle.request_stop();
    fixture.metadata.revoke_gate.release();
    wait_for_active_token_count(&fixture.metadata, 0).await;
    timeout(
        Duration::from_secs(5),
        fixture.lifecycle.wait_for_zero_active(),
    )
    .await
    .unwrap();

    assert_eq!(drain, DrainOutcome::TimedOutAwaitingCancellation);
    assert!(stop_while_blocked.is_err());
    assert!(fixture.lifecycle.request_stop().is_ok());
}

#[tokio::test]
async fn authorize_rejects_unknown_json_fields_without_issuing_a_token() {
    let fixture = fixture().await;
    let metadata = fixture.metadata.clone();
    let response = fixture
        .app
        .oneshot(request(
            "POST",
            "/wokcore/v1/clients/authorize",
            Some(&fixture.management),
            Some(json!({"client_id":"wokrouter","unexpected":"value"})),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(response).await["error"]["code"],
        "invalid_request_body"
    );
    assert!(metadata.active.lock().unwrap().is_empty());
}

#[tokio::test]
async fn malformed_dynamic_path_uses_the_stable_json_error_envelope() {
    let fixture = fixture().await;
    let response = fixture
        .app
        .oneshot(request(
            "DELETE",
            "/wokcore/v1/clients/wokrouter/tokens/%FF",
            Some(&fixture.management),
            None,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let body = json_body(response).await;
    assert_eq!(body["error"]["code"], "invalid_path_parameters");
    assert_eq!(body["error"]["request_id"], request_id);
}

#[tokio::test]
async fn unknown_routes_and_wrong_methods_use_stable_errors() {
    let fixture = fixture().await;
    for (method, path, management, status, code) in [
        (
            "GET",
            "/wokcore/v1/missing",
            Some(fixture.management.as_str()),
            404,
            "not_found",
        ),
        (
            "POST",
            "/wokcore/v1/health",
            None,
            405,
            "method_not_allowed",
        ),
    ] {
        let response = fixture
            .app
            .clone()
            .oneshot(request(method, path, management, None))
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
        assert_eq!(json_body(response).await["error"]["code"], code);
    }
}

#[tokio::test]
async fn non_contract_revoke_like_paths_are_plain_not_found_routes() {
    let fixture = fixture().await;
    let response = fixture
        .app
        .oneshot(request(
            "DELETE",
            "/wokcore/v1/clients/a/tokens/b/extra",
            Some(&fixture.management),
            None,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(json_body(response).await["error"]["code"], "not_found");
}

#[tokio::test]
async fn head_is_rejected_for_every_get_only_operation() {
    let fixture = fixture().await;
    for (path, management) in [
        ("/wokcore/v1/health", None),
        ("/wokcore/v1/capabilities", None),
        (
            "/wokcore/v1/service/status",
            Some(fixture.management.as_str()),
        ),
        ("/wokcore/v1/sessions", Some(fixture.management.as_str())),
        (
            "/wokcore/v1/sessions/example/messages",
            Some(fixture.management.as_str()),
        ),
        ("/wokcore/v1/usage", Some(fixture.management.as_str())),
        ("/wokcore/v1/logs", Some(fixture.management.as_str())),
        (
            "/wokcore/v1/diagnostics/export",
            Some(fixture.management.as_str()),
        ),
    ] {
        let response = fixture
            .app
            .clone()
            .oneshot(request("HEAD", path, management, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED, "{path}");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }
}

#[tokio::test]
async fn loopback_listener_stop_response_is_complete_before_graceful_shutdown() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (state, management, _, _) = state_fixture(&address.to_string()).await;
    let running = RunningServer::start(listener, state).await.unwrap();
    let mut connection = TcpStream::connect(address).await.unwrap();
    let request = format!(
        "POST /wokcore/v1/service/stop HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {management}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    connection.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    connection.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8(response).unwrap();

    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains(r#""phase":"stopping""#), "{response}");
    assert!(response.contains("x-request-id:"), "{response}");
    running.wait().await.unwrap();
}

#[tokio::test]
async fn coordinated_stop_keeps_listener_owned_until_runtime_flushes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (state, management, _, _) = state_fixture(&address.to_string()).await;
    let (state, mut stop_requests) = state.with_coordinated_shutdown();
    let running = RunningServer::start(listener, state).await.unwrap();

    let mut connection = TcpStream::connect(address).await.unwrap();
    let stop = format!(
        "POST /wokcore/v1/service/stop HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {management}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    connection.write_all(stop.as_bytes()).await.unwrap();
    let mut stop_response = Vec::new();
    connection.read_to_end(&mut stop_response).await.unwrap();
    assert!(
        String::from_utf8(stop_response)
            .unwrap()
            .starts_with("HTTP/1.1 200 OK")
    );
    timeout(Duration::from_secs(5), stop_requests.changed())
        .await
        .unwrap()
        .unwrap();
    assert!(*stop_requests.borrow_and_update());

    let mut health = TcpStream::connect(address).await.unwrap();
    health
        .write_all(
            format!(
                "GET /wokcore/v1/health HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut health_response = Vec::new();
    health.read_to_end(&mut health_response).await.unwrap();
    assert!(
        String::from_utf8(health_response)
            .unwrap()
            .starts_with("HTTP/1.1 200 OK")
    );

    running.shutdown().await.unwrap();
    assert_listener_closes(address).await;
}

#[tokio::test]
async fn listener_rejects_unspecified_ipv4_bindings() {
    let listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (state, _, _, _) = state_fixture(&address.to_string()).await;

    assert!(RunningServer::start(listener, state).await.is_err());
}

#[tokio::test]
async fn listener_rejects_other_addresses_in_the_ipv4_loopback_block() {
    let listener = TcpListener::bind("127.0.0.2:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (state, _, _, _) = state_fixture(&address.to_string()).await;

    assert!(RunningServer::start(listener, state).await.is_err());
}

#[tokio::test]
async fn server_owner_can_explicitly_shutdown_and_join_the_listener() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (state, _, _, _) = state_fixture(&address.to_string()).await;
    let running = RunningServer::start(listener, state).await.unwrap();

    running.shutdown().await.unwrap();

    assert_listener_closes(address).await;
}

#[tokio::test]
async fn dropping_server_owner_signals_listener_cleanup() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (state, _, _, _) = state_fixture(&address.to_string()).await;
    let running = RunningServer::start(listener, state).await.unwrap();

    drop(running);

    assert_listener_closes(address).await;
}
