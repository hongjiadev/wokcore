use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU8, Ordering},
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
    lifecycle::ServiceLifecycle,
};
use wokcore_storage::{
    ClientTokenMetadata, MemorySecretStore, RuntimeSecretBinding, SecretStore, StorageError,
};

const AUTHORITY: &str = "127.0.0.1:43128";
const CREATED_AT: &str = "2026-07-26T00:00:00Z";
const INSTANCE_ID: &str = "019844f0-4de0-7000-8000-000000000002";

#[derive(Debug, Default)]
struct IncrementingEntropy(AtomicU8);

impl EntropySource for IncrementingEntropy {
    fn fill(&self, output: &mut [u8; 32]) -> Result<(), TokenError> {
        output.fill(self.0.fetch_add(1, Ordering::SeqCst).wrapping_add(1));
        Ok(())
    }
}

#[derive(Default)]
struct TestMetadata {
    binding: Mutex<Option<RuntimeSecretBinding>>,
    active: Mutex<Vec<ClientTokenMetadata>>,
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
        self.active.lock().unwrap().push(token.clone());
        Ok(())
    }

    fn revoke_client_token(
        &self,
        client_id: &ClientId,
        token_id: &str,
        _revoked_at: &str,
    ) -> Result<bool, StorageError> {
        let mut active = self.active.lock().unwrap();
        let before = active.len();
        active.retain(|token| token.client_id != *client_id || token.token_id != token_id);
        Ok(active.len() != before)
    }
}

struct Fixture {
    app: Router,
    management: String,
}

async fn fixture() -> Fixture {
    let (state, management) = state_fixture(AUTHORITY).await;
    Fixture {
        app: build_router(state),
        management,
    }
}

async fn state_fixture(authority: &str) -> (ServerState, String) {
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
    let auth = AuthRegistry::bootstrap(secrets, metadata, entropy, scope, CREATED_AT.to_owned())
        .await
        .unwrap();
    let lifecycle = ServiceLifecycle::new();
    lifecycle.mark_running().unwrap();
    let state = ServerState::new(
        authority.to_owned(),
        Uuid::parse_str(INSTANCE_ID).unwrap(),
        Arc::new(auth),
        lifecycle,
    );
    (state, management)
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
    assert_eq!(
        body["capabilities"],
        json!([
            "client_token.issue",
            "client_token.revoke",
            "discovery.v1",
            "service.drain",
            "service.status"
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
async fn unknown_routes_and_wrong_methods_use_stable_errors() {
    let fixture = fixture().await;
    for (method, path, status, code) in [
        ("GET", "/wokcore/v1/missing", 404, "not_found"),
        ("POST", "/wokcore/v1/health", 405, "method_not_allowed"),
    ] {
        let response = fixture
            .app
            .clone()
            .oneshot(request(method, path, None, None))
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
        assert_eq!(json_body(response).await["error"]["code"], code);
    }
}

#[tokio::test]
async fn loopback_listener_stop_response_is_complete_before_graceful_shutdown() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (state, management) = state_fixture(&address.to_string()).await;
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
async fn listener_rejects_unspecified_ipv4_bindings() {
    let listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (state, _) = state_fixture(&address.to_string()).await;

    assert!(RunningServer::start(listener, state).await.is_err());
}
