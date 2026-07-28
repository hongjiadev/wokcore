use std::{
    fs::OpenOptions,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use fs4::fs_std::FileExt;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use tower::ServiceExt;
use wokcore_core::{
    config::{
        AccountAuthConfig, AccountConfig, ProviderConfig, ProviderInstanceConfig, RouteTarget,
        RoutingConfig,
    },
    id::{AccountId, ProviderId},
    secret::{SecretPurpose, SecretRef, SecretScope},
};
use wokcore_server::providers::{
    ProviderCandidate, ProviderManagement, ProviderManagementError, ReloadStatus,
};
use wokcore_server::{
    ServerState,
    api::build_router,
    auth::{AuthMetadataStore, AuthRegistry, EntropySource, StateAuthMetadataStore, TokenError},
    lifecycle::ServiceLifecycle,
};
use wokcore_storage::{ClientTokenScope, MemorySecretStore, SecretStore, StateStore};

const AUTHORITY: &str = "127.0.0.1:43129";
const CREATED_AT: &str = "2026-07-27T00:00:00Z";

#[tokio::test]
async fn catalog_runtime_validation_commit_and_reload_are_revisioned_and_fail_safe() {
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("config.toml");
    let secrets = Arc::new(MemorySecretStore::default());
    let management =
        ProviderManagement::open(&config_path, secrets.clone()).expect("provider management");

    assert_eq!(management.catalog().providers().len(), 58);
    assert_eq!(management.status().revision, 0);
    assert_eq!(management.status().snapshot_revision, 0);
    assert_eq!(management.status().reload_status, ReloadStatus::Ready);
    assert!(management.models().is_empty());

    let secret = management
        .create_secret(secret_scope(), SecretString::from("synthetic-secret-value"))
        .await
        .expect("create secret");
    let candidate = candidate(secret.secret_ref.clone());
    let validation = management
        .validate(&candidate)
        .expect("candidate validation");
    assert_eq!(validation.provider_count, 1);
    assert!(!validation.models.is_empty());

    let committed = management
        .commit(0, candidate.clone())
        .await
        .expect("commit");
    assert_eq!(committed.revision, 1);
    assert_eq!(committed.snapshot_revision, 1);
    assert_eq!(management.status().revision, 1);
    assert!(!management.models().is_empty());

    let conflict = management.commit(0, candidate).await.unwrap_err();
    assert_eq!(conflict, ProviderManagementError::RevisionConflict);

    std::fs::write(&config_path, "not valid toml").unwrap();
    let reload = management.reload().await.unwrap_err();
    assert_eq!(reload, ProviderManagementError::InvalidConfiguration);
    assert_eq!(management.status().revision, 1);
    assert_eq!(management.status().snapshot_revision, 1);
    assert_eq!(management.status().reload_status, ReloadStatus::Failed);
    assert!(!management.models().is_empty());
}

#[tokio::test]
async fn secret_lifecycle_keeps_a_stable_reference_and_never_returns_material() {
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("config.toml");
    let secrets = Arc::new(MemorySecretStore::default());
    let management =
        ProviderManagement::open(&config_path, secrets.clone()).expect("provider management");

    let created = management
        .create_secret(secret_scope(), SecretString::from("first-secret-canary"))
        .await
        .expect("create");
    let retried = management
        .create_secret(secret_scope(), SecretString::from("first-secret-canary"))
        .await
        .expect("idempotent retry");
    assert_eq!(retried.secret_ref, created.secret_ref);
    assert_eq!(created.secret_ref, SecretRef::for_scope(&secret_scope()));
    assert_eq!(
        management
            .create_secret(
                secret_scope(),
                SecretString::from("different-secret-canary"),
            )
            .await,
        Err(ProviderManagementError::SecretAlreadyExists)
    );
    assert!(!format!("{created:?}").contains("first-secret-canary"));

    let replaced = management
        .replace_secret(
            &created.secret_ref,
            SecretString::from("second-secret-canary"),
        )
        .await
        .expect("replace");
    assert_eq!(replaced.secret_ref, created.secret_ref);
    assert!(!format!("{replaced:?}").contains("second-secret-canary"));
    assert_eq!(
        secrets
            .get(&created.secret_ref)
            .await
            .unwrap()
            .expose_secret(),
        "second-secret-canary"
    );

    management
        .delete_secret(&created.secret_ref)
        .await
        .expect("delete");
    assert!(secrets.get(&created.secret_ref).await.is_err());
}

#[tokio::test]
async fn an_active_configuration_prevents_secret_deletion() {
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("config.toml");
    let secrets = Arc::new(MemorySecretStore::default());
    let management =
        ProviderManagement::open(&config_path, secrets.clone()).expect("provider management");
    let secret = management
        .create_secret(secret_scope(), SecretString::from("referenced-secret"))
        .await
        .unwrap();
    management
        .commit(0, candidate(secret.secret_ref.clone()))
        .await
        .unwrap();

    let error = management
        .delete_secret(&secret.secret_ref)
        .await
        .unwrap_err();

    assert_eq!(error, ProviderManagementError::SecretInUse);
    assert_eq!(
        secrets
            .get(&secret.secret_ref)
            .await
            .unwrap()
            .expose_secret(),
        "referenced-secret"
    );
}

#[tokio::test]
async fn runtime_auth_secret_is_excluded_from_the_provider_lifecycle() {
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("config.toml");
    let secrets = Arc::new(MemorySecretStore::default());
    let management =
        ProviderManagement::open(&config_path, secrets.clone()).expect("provider management");
    let secret = management
        .create_secret(secret_scope(), SecretString::from("runtime-auth-canary"))
        .await
        .unwrap();
    management
        .protect_secret_ref(secret.secret_ref.clone())
        .await
        .unwrap();

    assert_eq!(
        management.validate(&candidate(secret.secret_ref.clone())),
        Err(ProviderManagementError::SecretProtected)
    );
    assert_eq!(
        management
            .commit(0, candidate(secret.secret_ref.clone()))
            .await,
        Err(ProviderManagementError::SecretProtected)
    );
    assert_eq!(
        management
            .replace_secret(
                &secret.secret_ref,
                SecretString::from("replacement-runtime-auth-canary"),
            )
            .await,
        Err(ProviderManagementError::SecretProtected)
    );
    assert_eq!(
        management.delete_secret(&secret.secret_ref).await,
        Err(ProviderManagementError::SecretProtected)
    );
    assert_eq!(
        secrets
            .get(&secret.secret_ref)
            .await
            .unwrap()
            .expose_secret(),
        "runtime-auth-canary"
    );
}

#[tokio::test]
async fn cancelled_commit_finishes_storage_and_snapshot_publication() {
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("config.toml");
    let secrets = Arc::new(MemorySecretStore::default());
    let management =
        Arc::new(ProviderManagement::open(&config_path, secrets).expect("provider management"));
    let lock_path = config_path.with_file_name("config.toml.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .unwrap();
    lock.lock_exclusive().unwrap();

    let owned = Arc::clone(&management);
    let commit = tokio::spawn(async move { owned.commit(0, empty_candidate()).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    commit.abort();
    assert!(commit.await.unwrap_err().is_cancelled());
    FileExt::unlock(&lock).unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if management.status().revision == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owned commit must publish after caller cancellation");
    assert_eq!(management.status().snapshot_revision, 1);
}

#[tokio::test]
async fn cancelled_secret_create_is_recoverable_by_credential_scope() {
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("config.toml");
    let secrets = Arc::new(MemorySecretStore::default());
    let management = Arc::new(
        ProviderManagement::open(&config_path, secrets.clone()).expect("provider management"),
    );
    let lock_path = config_path.with_file_name("config.toml.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .unwrap();
    lock.lock_exclusive().unwrap();

    let commit_owner = Arc::clone(&management);
    let commit = tokio::spawn(async move { commit_owner.commit(0, empty_candidate()).await });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let scope = secret_scope();
    let create_owner = Arc::clone(&management);
    let create_scope = scope.clone();
    let create = tokio::spawn(async move {
        create_owner
            .create_secret(create_scope, SecretString::from("cancelled-create-canary"))
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    create.abort();
    assert!(create.await.unwrap_err().is_cancelled());
    FileExt::unlock(&lock).unwrap();
    commit.await.unwrap().unwrap();

    let recovered = management
        .create_secret(scope.clone(), SecretString::from("cancelled-create-canary"))
        .await
        .expect("retry must recover the stable reference");
    assert_eq!(recovered.secret_ref, SecretRef::for_scope(&scope));
    assert_eq!(
        secrets
            .get(&recovered.secret_ref)
            .await
            .unwrap()
            .expose_secret(),
        "cancelled-create-canary"
    );
}

#[tokio::test]
async fn http_management_contract_is_strict_scoped_revisioned_and_content_free() {
    let fixture = http_fixture().await;

    let scoped = fixture
        .app
        .clone()
        .oneshot(request(
            Method::GET,
            "/wokcore/v1/providers/catalog",
            Some(&fixture.proxy),
            None,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(scoped.status(), StatusCode::FORBIDDEN);

    let catalog = fixture
        .send(Method::GET, "/wokcore/v1/providers/catalog", None, None)
        .await;
    assert_eq!(catalog.0, StatusCode::OK);
    assert_eq!(catalog.1["schema_version"], 1);
    assert_eq!(catalog.1["providers"].as_array().unwrap().len(), 58);

    let head = fixture
        .send(Method::HEAD, "/wokcore/v1/providers/catalog", None, None)
        .await;
    assert_eq!(head.0, StatusCode::METHOD_NOT_ALLOWED);

    let wrong_content_type = fixture
        .send_raw(
            Method::POST,
            "/wokcore/v1/providers/config/validate",
            Some("text/plain"),
            br#"{}"#.to_vec(),
        )
        .await;
    assert_eq!(wrong_content_type.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);

    let unknown_field = fixture
        .send(
            Method::POST,
            "/wokcore/v1/providers/config/validate",
            Some("application/json"),
            Some(json!({
                "providers": {"instances": [], "accounts": []},
                "routing": {"aliases": [], "rules": [], "default": null},
                "unexpected": "must be rejected"
            })),
        )
        .await;
    assert_eq!(unknown_field.0, StatusCode::BAD_REQUEST);

    let protected_scope = fixture
        .send(
            Method::POST,
            "/wokcore/v1/provider-secrets",
            Some("application/json"),
            Some(json!({
                "provider_id": "wokcore-runtime",
                "purpose": "auxiliary",
                "secret": "must-not-be-created"
            })),
        )
        .await;
    assert_eq!(protected_scope.0, StatusCode::FORBIDDEN);
    assert_eq!(
        protected_scope.1["error"]["code"],
        "provider_secret_protected"
    );

    let created = fixture
        .send(
            Method::POST,
            "/wokcore/v1/provider-secrets",
            Some("application/json"),
            Some(json!({
                "provider_id": "primary",
                "account_id": "primary",
                "purpose": "api_key",
                "secret": "http-secret-canary"
            })),
        )
        .await;
    assert_eq!(created.0, StatusCode::CREATED);
    let rendered = created.1.to_string();
    assert!(!rendered.contains("http-secret-canary"));
    let secret_ref = created.1["secret_ref"].as_str().unwrap();

    let candidate =
        candidate(wokcore_core::secret::SecretRef::parse(secret_ref.to_owned()).unwrap());
    let candidate_json = serde_json::to_value(&candidate).unwrap();
    let validated = fixture
        .send(
            Method::POST,
            "/wokcore/v1/providers/config/validate",
            Some("application/json"),
            Some(candidate_json.clone()),
        )
        .await;
    assert_eq!(validated.0, StatusCode::OK);
    assert_eq!(validated.1["valid"], true);

    let commit_body = json!({
        "expected_revision": 0,
        "providers": candidate_json["providers"],
        "routing": candidate_json["routing"],
    });
    let committed = fixture
        .send(
            Method::PUT,
            "/wokcore/v1/providers/config",
            Some("application/json"),
            Some(commit_body.clone()),
        )
        .await;
    assert_eq!(committed.0, StatusCode::OK);
    assert_eq!(committed.1["revision"], 1);

    let conflict = fixture
        .send(
            Method::PUT,
            "/wokcore/v1/providers/config",
            Some("application/json"),
            Some(commit_body),
        )
        .await;
    assert_eq!(conflict.0, StatusCode::CONFLICT);
    assert_eq!(
        conflict.1["error"]["code"],
        "provider_config_revision_conflict"
    );

    let runtime = fixture
        .send(Method::GET, "/wokcore/v1/providers/runtime", None, None)
        .await;
    assert_eq!(runtime.0, StatusCode::OK);
    assert_eq!(runtime.1["revision"], 1);
    assert_eq!(runtime.1["snapshot_revision"], 1);

    let reloaded = fixture
        .send(Method::POST, "/wokcore/v1/providers/reload", None, None)
        .await;
    assert_eq!(reloaded.0, StatusCode::OK);
    assert_eq!(reloaded.1["revision"], 1);
    assert_eq!(reloaded.1["snapshot_revision"], 1);

    let reload_body = fixture
        .send_raw(
            Method::POST,
            "/wokcore/v1/providers/reload",
            Some("text/plain"),
            b"unexpected".to_vec(),
        )
        .await;
    assert_eq!(reload_body.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);

    let models = fixture
        .send(Method::GET, "/wokcore/v1/providers/models", None, None)
        .await;
    assert_eq!(models.0, StatusCode::OK);
    assert!(!models.1["models"].as_array().unwrap().is_empty());

    let replaced = fixture
        .send(
            Method::PUT,
            &format!("/wokcore/v1/provider-secrets/{secret_ref}"),
            Some("application/json"),
            Some(json!({"secret":"replacement-secret-canary"})),
        )
        .await;
    assert_eq!(replaced.0, StatusCode::OK);
    assert!(!replaced.1.to_string().contains("replacement-secret-canary"));
    assert_eq!(replaced.1["secret_ref"], secret_ref);

    let in_use = fixture
        .send(
            Method::DELETE,
            &format!("/wokcore/v1/provider-secrets/{secret_ref}"),
            None,
            None,
        )
        .await;
    assert_eq!(in_use.0, StatusCode::CONFLICT);
    assert_eq!(in_use.1["error"]["code"], "provider_secret_in_use");

    let unused = fixture
        .send(
            Method::POST,
            "/wokcore/v1/provider-secrets",
            Some("application/json"),
            Some(json!({
                "provider_id": "secondary",
                "purpose": "api_key",
                "secret": "unused-secret-canary"
            })),
        )
        .await;
    assert_eq!(unused.0, StatusCode::CREATED);
    let unused_ref = unused.1["secret_ref"].as_str().unwrap();
    let delete_body = fixture
        .send_raw(
            Method::DELETE,
            &format!("/wokcore/v1/provider-secrets/{unused_ref}"),
            Some("text/plain"),
            b"unexpected".to_vec(),
        )
        .await;
    assert_eq!(delete_body.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let deleted = fixture
        .send(
            Method::DELETE,
            &format!("/wokcore/v1/provider-secrets/{unused_ref}"),
            None,
            None,
        )
        .await;
    assert_eq!(deleted.0, StatusCode::OK);
    assert_eq!(deleted.1["operation"], "deleted");
    assert_eq!(deleted.1["secret_ref"], unused_ref);
    assert!(!deleted.1.to_string().contains("unused-secret-canary"));

    let oversized = fixture
        .send_raw(
            Method::POST,
            "/wokcore/v1/providers/config/validate",
            Some("application/json"),
            vec![b'x'; 16 * 1024 + 1],
        )
        .await;
    assert_eq!(oversized.0, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(!oversized.1.to_string().contains("http-secret-canary"));
}

struct HttpFixture {
    app: Router,
    management: String,
    proxy: String,
}

impl HttpFixture {
    async fn send(
        &self,
        method: Method,
        path: &str,
        content_type: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let bytes = body
            .map(|body| serde_json::to_vec(&body).unwrap())
            .unwrap_or_default();
        self.send_raw(method, path, content_type, bytes).await
    }

    async fn send_raw(
        &self,
        method: Method,
        path: &str,
        content_type: Option<&str>,
        body: Vec<u8>,
    ) -> (StatusCode, Value) {
        let response = self
            .app
            .clone()
            .oneshot(request(
                method,
                path,
                Some(&self.management),
                content_type,
                Some(body),
            ))
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, value)
    }
}

async fn http_fixture() -> HttpFixture {
    let directory = tempfile::tempdir().unwrap().keep();
    let secrets = Arc::new(MemorySecretStore::default());
    let metadata = Arc::new(StateAuthMetadataStore::new(
        StateStore::open(directory.join("state.sqlite3")).unwrap(),
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
            metadata.clone(),
            entropy.clone(),
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
    let proxy = auth
        .issue_client_token_with_scopes(
            "019844f0-4de0-7000-8000-000000000099".to_owned(),
            wokcore_core::id::ClientId::new("wokrouter").unwrap(),
            CREATED_AT.to_owned(),
            vec![ClientTokenScope::ProxyUse],
        )
        .await
        .unwrap()
        .into_response_value()
        .expose_secret()
        .to_owned();
    let providers = Arc::new(
        ProviderManagement::open(directory.join("config.toml"), secrets)
            .expect("provider management"),
    );
    providers
        .protect_secret_ref(binding.secret_ref)
        .await
        .expect("protect runtime management secret");
    let lifecycle = ServiceLifecycle::new();
    lifecycle.mark_running().unwrap();
    let state = ServerState::new(
        AUTHORITY,
        uuid::Uuid::parse_str("019844f0-4de0-7000-8000-000000000098").unwrap(),
        auth,
        lifecycle,
    )
    .with_provider_management(providers);
    HttpFixture {
        app: build_router(state),
        management,
        proxy,
    }
}

#[derive(Default)]
struct IncrementingEntropy(AtomicU8);

impl EntropySource for IncrementingEntropy {
    fn fill(&self, output: &mut [u8; 32]) -> Result<(), TokenError> {
        output.fill(self.0.fetch_add(1, Ordering::SeqCst).wrapping_add(1));
        Ok(())
    }
}

fn request(
    method: Method,
    path: &str,
    bearer: Option<&str>,
    content_type: Option<&str>,
    body: Option<Vec<u8>>,
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
    request.body(Body::from(body.unwrap_or_default())).unwrap()
}

fn candidate(secret_ref: wokcore_core::secret::SecretRef) -> ProviderCandidate {
    ProviderCandidate {
        providers: ProviderConfig {
            instances: vec![ProviderInstanceConfig {
                id: provider("primary"),
                catalog_id: provider("openai-apikey"),
                enabled: true,
                endpoint: None,
                allow_private_network: false,
            }],
            accounts: vec![AccountConfig {
                id: account("primary"),
                provider: provider("primary"),
                enabled: true,
                auth: AccountAuthConfig::ApiKey { secret: secret_ref },
            }],
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

fn empty_candidate() -> ProviderCandidate {
    ProviderCandidate {
        providers: ProviderConfig::default(),
        routing: RoutingConfig::default(),
    }
}

fn secret_scope() -> SecretScope {
    SecretScope {
        provider_id: provider("primary"),
        account_id: Some(account("primary")),
        purpose: SecretPurpose::ApiKey,
    }
}

fn provider(value: &str) -> ProviderId {
    ProviderId::new(value).unwrap()
}

fn account(value: &str) -> AccountId {
    AccountId::new(value).unwrap()
}
