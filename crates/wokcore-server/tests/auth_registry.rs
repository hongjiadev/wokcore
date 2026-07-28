use std::{
    sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use wokcore_core::{
    id::{ClientId, ProviderId},
    secret::{SecretPurpose, SecretRef, SecretScope},
};
use wokcore_server::auth::{
    AuthMetadataStore, AuthRegistry, EntropySource, TokenError, TokenMaterial,
};
use wokcore_storage::{
    ClientTokenMetadata, ClientTokenScope, MemorySecretStore, RuntimeSecretBinding, SecretStore,
    StorageError,
};

const CREATED_AT: &str = "2026-07-26T00:00:00Z";
const REVOKED_AT: &str = "2026-07-26T01:00:00Z";

fn token_digest(candidate: &str) -> [u8; 32] {
    Sha256::digest(candidate.as_bytes()).into()
}

#[derive(Debug)]
struct DeterministicEntropy {
    bytes: [u8; 32],
    fills: AtomicUsize,
}

impl DeterministicEntropy {
    fn repeated(byte: u8) -> Self {
        Self {
            bytes: [byte; 32],
            fills: AtomicUsize::new(0),
        }
    }
}

impl EntropySource for DeterministicEntropy {
    fn fill(&self, output: &mut [u8; 32]) -> Result<(), TokenError> {
        let fill = self.fills.fetch_add(1, Ordering::SeqCst);
        output.fill(self.bytes[0].wrapping_add(fill as u8));
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

#[derive(Clone)]
enum SecretReadBack {
    Value(String),
    Failure,
}

#[derive(Default)]
struct RecordingSecretStore {
    inner: MemorySecretStore,
    events: Arc<Mutex<Vec<&'static str>>>,
    gets: AtomicUsize,
    read_back: Mutex<Option<SecretReadBack>>,
    put_gate: Mutex<Option<BlockingGate>>,
}

impl RecordingSecretStore {
    fn return_on_get(&self, value: String) {
        *self.read_back.lock().unwrap() = Some(SecretReadBack::Value(value));
    }

    fn fail_on_get(&self) {
        *self.read_back.lock().unwrap() = Some(SecretReadBack::Failure);
    }

    fn arm_put_gate(&self) -> BlockingGateHandle {
        let (gate, handle) = blocking_gate();
        *self.put_gate.lock().unwrap() = Some(gate);
        handle
    }
}

#[async_trait::async_trait]
impl SecretStore for RecordingSecretStore {
    async fn put(
        &self,
        scope: &SecretScope,
        value: SecretString,
    ) -> Result<SecretRef, StorageError> {
        self.events.lock().unwrap().push("secret.put");
        let secret_ref = self.inner.put(scope, value).await?;
        if let Some((started, release, completed)) = self.put_gate.lock().unwrap().take() {
            let _ = started.send(());
            release.wait();
            let _ = completed.send(());
        }
        Ok(secret_ref)
    }

    async fn get(&self, secret_ref: &SecretRef) -> Result<SecretString, StorageError> {
        self.events.lock().unwrap().push("secret.get");
        self.gets.fetch_add(1, Ordering::SeqCst);
        let read_back = self.read_back.lock().unwrap().clone();
        match read_back {
            Some(SecretReadBack::Value(value)) => Ok(SecretString::from(value)),
            Some(SecretReadBack::Failure) => Err(StorageError::SecretBackendFailure),
            None => self.inner.get(secret_ref).await,
        }
    }

    async fn delete(&self, secret_ref: &SecretRef) -> Result<(), StorageError> {
        self.events.lock().unwrap().push("secret.delete");
        self.inner.delete(secret_ref).await
    }
}

#[derive(Debug, Default)]
struct FakeMetadataStore {
    binding: Mutex<Option<RuntimeSecretBinding>>,
    active: Mutex<Vec<ClientTokenMetadata>>,
    orphans: Mutex<Vec<SecretRef>>,
    events: Arc<Mutex<Vec<&'static str>>>,
    calls: AtomicUsize,
    fail_bind: AtomicBool,
    fail_issue: AtomicBool,
    issue_gate: Mutex<Option<BlockingGate>>,
    revoke_gate: Mutex<Option<BlockingGate>>,
}

type BlockingGate = (mpsc::Sender<()>, Arc<Barrier>, mpsc::Sender<()>);
type BlockingGateHandle = (mpsc::Receiver<()>, Arc<Barrier>, mpsc::Receiver<()>);

fn blocking_gate() -> (BlockingGate, BlockingGateHandle) {
    let (started_tx, started_rx) = mpsc::channel();
    let (completed_tx, completed_rx) = mpsc::channel();
    let release = Arc::new(Barrier::new(2));
    (
        (started_tx, Arc::clone(&release), completed_tx),
        (started_rx, release, completed_rx),
    )
}

impl FakeMetadataStore {
    fn with_events(events: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
            events,
            ..Self::default()
        }
    }

    fn set_binding(&self, secret_ref: SecretRef) {
        *self.binding.lock().unwrap() = Some(RuntimeSecretBinding {
            name: "management".to_owned(),
            secret_ref,
            revision: 1,
            created_at: CREATED_AT.to_owned(),
        });
    }

    fn arm_issue_gate(&self) -> BlockingGateHandle {
        let (gate, handle) = blocking_gate();
        *self.issue_gate.lock().unwrap() = Some(gate);
        handle
    }

    fn arm_revoke_gate(&self) -> BlockingGateHandle {
        let (gate, handle) = blocking_gate();
        *self.revoke_gate.lock().unwrap() = Some(gate);
        handle
    }
}

impl AuthMetadataStore for FakeMetadataStore {
    fn runtime_secret_binding(
        &self,
        _name: &str,
    ) -> Result<Option<RuntimeSecretBinding>, StorageError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.events.lock().unwrap().push("metadata.binding");
        Ok(self.binding.lock().unwrap().clone())
    }

    fn bind_runtime_secret_if_absent(
        &self,
        name: &str,
        secret_ref: &SecretRef,
        created_at: &str,
    ) -> Result<RuntimeSecretBinding, StorageError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.events.lock().unwrap().push("metadata.bind");
        if self.fail_bind.load(Ordering::SeqCst) {
            return Err(StorageError::SecretBackendFailure);
        }
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
        secret_ref: &SecretRef,
        _created_at: &str,
    ) -> Result<(), StorageError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.events.lock().unwrap().push("metadata.orphan");
        self.orphans.lock().unwrap().push(secret_ref.clone());
        Ok(())
    }

    fn load_active_client_tokens(&self) -> Result<Vec<ClientTokenMetadata>, StorageError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.events.lock().unwrap().push("metadata.load");
        Ok(self.active.lock().unwrap().clone())
    }

    fn issue_client_token(&self, token: &ClientTokenMetadata) -> Result<(), StorageError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.events.lock().unwrap().push("metadata.issue");
        let gate = self.issue_gate.lock().unwrap().take();
        if let Some((started, release, _)) = &gate {
            let _ = started.send(());
            release.wait();
        }
        if self.fail_issue.load(Ordering::SeqCst) {
            return Err(StorageError::SecretBackendFailure);
        }
        self.active.lock().unwrap().push(token.clone());
        if let Some((_, _, completed)) = gate {
            let _ = completed.send(());
        }
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.events.lock().unwrap().push("metadata.revoke");
        let gate = self.revoke_gate.lock().unwrap().take();
        if let Some((started, release, _)) = &gate {
            let _ = started.send(());
            release.wait();
        }
        let mut active = self.active.lock().unwrap();
        let before = active.len();
        active.retain(|token| token.token_id != token_id || token.client_id != *client_id);
        let changed = active.len() != before;
        drop(active);
        if let Some((_, _, completed)) = gate {
            let _ = completed.send(());
        }
        Ok(changed)
    }
}

async fn wait_for_event(events: &Mutex<Vec<&'static str>>, expected: &'static str) {
    for _ in 0..10_000 {
        if events.lock().unwrap().contains(&expected) {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("event {expected:?} was not observed");
}

async fn wait_for_client_presence(registry: &AuthRegistry, raw: &str, expected: bool) {
    for _ in 0..10_000 {
        if registry.validate_client(raw).is_some() == expected {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("client snapshot did not reach expected presence {expected}");
}

fn management_scope() -> SecretScope {
    SecretScope {
        provider_id: ProviderId::new("wokcore-runtime").unwrap(),
        account_id: None,
        purpose: SecretPurpose::Auxiliary,
    }
}

async fn existing_registry(
    entropy_byte: u8,
) -> (
    Arc<AuthRegistry>,
    Arc<RecordingSecretStore>,
    Arc<FakeMetadataStore>,
    String,
) {
    let entropy = DeterministicEntropy::repeated(entropy_byte);
    let management = TokenMaterial::generate_admin(&entropy).unwrap();
    let management_value = management.into_response_value();
    let raw = management_value.expose_secret().to_owned();
    let secrets = Arc::new(RecordingSecretStore::default());
    let secret_ref = secrets
        .put(&management_scope(), management_value)
        .await
        .unwrap();
    secrets.events.lock().unwrap().clear();
    let metadata = Arc::new(FakeMetadataStore::with_events(Arc::clone(&secrets.events)));
    metadata.set_binding(secret_ref);
    let registry = AuthRegistry::bootstrap(
        secrets.clone(),
        metadata.clone(),
        Arc::new(DeterministicEntropy::repeated(entropy_byte.wrapping_add(1))),
        management_scope(),
        CREATED_AT.to_owned(),
    )
    .await
    .unwrap();
    (Arc::new(registry), secrets, metadata, raw)
}

#[test]
fn token_material_uses_exact_prefixes_and_32_bytes_of_base64url_no_pad_entropy() {
    let entropy = DeterministicEntropy::repeated(0xfb);
    let admin = TokenMaterial::generate_admin(&entropy)
        .unwrap()
        .into_response_value();
    let proxy = TokenMaterial::generate_proxy(&entropy)
        .unwrap()
        .into_response_value();

    for (value, prefix) in [
        (admin.expose_secret(), "wok_admin_v1_"),
        (proxy.expose_secret(), "wok_proxy_v1_"),
    ] {
        let encoded = value.strip_prefix(prefix).unwrap();
        assert_eq!(encoded.len(), 43);
        assert!(
            encoded
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        );
        assert!(!encoded.contains('='));
    }
    assert_eq!(entropy.fills.load(Ordering::SeqCst), 2);
}

#[test]
fn token_debug_and_errors_never_expose_material() {
    let entropy = DeterministicEntropy::repeated(0x7a);
    let material = TokenMaterial::generate_proxy(&entropy).unwrap();
    assert_eq!(format!("{material:?}"), "TokenMaterial([redacted])");
    let response = material.into_response_value();
    let canary = response.expose_secret().to_owned();

    assert!(!format!("{response:?}").contains(&canary));
    let error = TokenMaterial::generate_proxy(&FailingEntropy).unwrap_err();
    assert!(!format!("{error:?} {error}").contains(&canary));
}

#[test]
fn client_ids_are_distinct_lowercase_bounded_identifiers() {
    assert_eq!(
        ClientId::new("wokrouter.v1").unwrap().as_str(),
        "wokrouter.v1"
    );
    for invalid in [
        "WokRouter",
        ".",
        "..",
        "../router",
        "router/client",
        r"router\client",
        "router client",
        "router:client",
        "-_.",
    ] {
        assert!(ClientId::new(invalid).is_err(), "accepted {invalid:?}");
    }
    assert!(ClientId::new("a".repeat(128)).is_ok());
    assert!(ClientId::new("a".repeat(129)).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_reads_once_and_registry_retains_only_digests_and_active_metadata() {
    let (_registry, secrets, metadata, management_raw) = existing_registry(0x11).await;
    let proxy = TokenMaterial::generate_proxy(&DeterministicEntropy::repeated(0x22))
        .unwrap()
        .into_response_value();
    let proxy_raw = proxy.expose_secret().to_owned();
    metadata.active.lock().unwrap().push(ClientTokenMetadata {
        token_id: "token-existing".to_owned(),
        client_id: ClientId::new("wokrouter").unwrap(),
        digest: token_digest(&proxy_raw),
        issued_at: CREATED_AT.to_owned(),
    });
    let registry = AuthRegistry::bootstrap(
        secrets.clone(),
        metadata.clone(),
        Arc::new(DeterministicEntropy::repeated(0x33)),
        management_scope(),
        CREATED_AT.to_owned(),
    )
    .await
    .unwrap();
    let calls_after_bootstrap = metadata.calls.load(Ordering::SeqCst);

    assert!(registry.validate_management(&management_raw));
    assert_eq!(
        registry.validate_client(&proxy_raw).unwrap().client_id,
        ClientId::new("wokrouter").unwrap()
    );
    assert_eq!(metadata.calls.load(Ordering::SeqCst), calls_after_bootstrap);
    assert_eq!(secrets.gets.load(Ordering::SeqCst), 2);
    let rendered = format!("{registry:?}");
    assert!(!rendered.contains(&management_raw));
    assert!(!rendered.contains(&proxy_raw));
    drop(registry);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_writes_secret_before_binding_and_records_orphan_on_bind_failure() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let secrets = Arc::new(RecordingSecretStore {
        events: Arc::clone(&events),
        ..RecordingSecretStore::default()
    });
    let metadata = Arc::new(FakeMetadataStore::with_events(Arc::clone(&events)));
    metadata.fail_bind.store(true, Ordering::SeqCst);
    let entropy = Arc::new(DeterministicEntropy::repeated(0x44));
    let expected = TokenMaterial::generate_admin(&DeterministicEntropy::repeated(0x44))
        .unwrap()
        .into_response_value();
    let expected_raw = expected.expose_secret().to_owned();
    events.lock().unwrap().clear();

    let error = AuthRegistry::bootstrap(
        secrets,
        metadata.clone(),
        entropy,
        management_scope(),
        CREATED_AT.to_owned(),
    )
    .await
    .unwrap_err();

    assert_eq!(
        *events.lock().unwrap(),
        [
            "metadata.binding",
            "secret.put",
            "metadata.bind",
            "metadata.orphan",
        ]
    );
    assert_eq!(metadata.orphans.lock().unwrap().len(), 1);
    assert!(!format!("{error:?} {error}").contains(&expected_raw));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_bootstrap_uses_the_bound_secret_read_back_for_its_digest() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let secrets = Arc::new(RecordingSecretStore {
        events: Arc::clone(&events),
        ..RecordingSecretStore::default()
    });
    let metadata = Arc::new(FakeMetadataStore::with_events(Arc::clone(&events)));
    let generated = TokenMaterial::generate_admin(&DeterministicEntropy::repeated(0x45))
        .unwrap()
        .into_response_value();
    let generated_raw = generated.expose_secret().to_owned();
    let persisted = TokenMaterial::generate_admin(&DeterministicEntropy::repeated(0x46))
        .unwrap()
        .into_response_value();
    let persisted_raw = persisted.expose_secret().to_owned();
    secrets.return_on_get(persisted_raw.clone());

    let registry = AuthRegistry::bootstrap(
        secrets.clone(),
        metadata,
        Arc::new(DeterministicEntropy::repeated(0x45)),
        management_scope(),
        CREATED_AT.to_owned(),
    )
    .await
    .unwrap();

    assert_eq!(
        *events.lock().unwrap(),
        [
            "metadata.binding",
            "secret.put",
            "metadata.bind",
            "secret.get",
            "metadata.load",
        ]
    );
    assert_eq!(secrets.gets.load(Ordering::SeqCst), 1);
    assert!(registry.validate_management(&persisted_raw));
    assert!(!registry.validate_management(&generated_raw));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_bootstrap_round_trips_the_generated_value_through_memory_store() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let secrets = Arc::new(RecordingSecretStore {
        events: Arc::clone(&events),
        ..RecordingSecretStore::default()
    });
    let metadata = Arc::new(FakeMetadataStore::with_events(Arc::clone(&events)));
    let expected = TokenMaterial::generate_admin(&DeterministicEntropy::repeated(0x47))
        .unwrap()
        .into_response_value();
    let expected_raw = expected.expose_secret().to_owned();

    let registry = AuthRegistry::bootstrap(
        secrets.clone(),
        metadata,
        Arc::new(DeterministicEntropy::repeated(0x47)),
        management_scope(),
        CREATED_AT.to_owned(),
    )
    .await
    .unwrap();

    assert_eq!(
        *events.lock().unwrap(),
        [
            "metadata.binding",
            "secret.put",
            "metadata.bind",
            "secret.get",
            "metadata.load",
        ]
    );
    assert_eq!(secrets.gets.load(Ordering::SeqCst), 1);
    assert!(registry.validate_management(&expected_raw));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_bootstrap_fails_closed_when_bound_secret_read_back_fails() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let secrets = Arc::new(RecordingSecretStore {
        events: Arc::clone(&events),
        ..RecordingSecretStore::default()
    });
    secrets.fail_on_get();
    let metadata = Arc::new(FakeMetadataStore::with_events(Arc::clone(&events)));
    let generated = TokenMaterial::generate_admin(&DeterministicEntropy::repeated(0x48))
        .unwrap()
        .into_response_value();
    let generated_raw = generated.expose_secret().to_owned();

    let error = AuthRegistry::bootstrap(
        secrets.clone(),
        metadata,
        Arc::new(DeterministicEntropy::repeated(0x48)),
        management_scope(),
        CREATED_AT.to_owned(),
    )
    .await
    .unwrap_err();

    assert_eq!(
        *events.lock().unwrap(),
        [
            "metadata.binding",
            "secret.put",
            "metadata.bind",
            "secret.get",
        ]
    );
    assert_eq!(secrets.gets.load(Ordering::SeqCst), 1);
    assert!(!format!("{error:?} {error}").contains(&generated_raw));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_bootstrap_rejects_invalid_bound_secret_read_back_without_exposing_it() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let secrets = Arc::new(RecordingSecretStore {
        events: Arc::clone(&events),
        ..RecordingSecretStore::default()
    });
    let invalid_read_back = ["invalid", "management", "read-back"].join("-");
    secrets.return_on_get(invalid_read_back.clone());
    let metadata = Arc::new(FakeMetadataStore::with_events(Arc::clone(&events)));
    let generated = TokenMaterial::generate_admin(&DeterministicEntropy::repeated(0x49))
        .unwrap()
        .into_response_value();
    let generated_raw = generated.expose_secret().to_owned();

    let error = AuthRegistry::bootstrap(
        secrets.clone(),
        metadata,
        Arc::new(DeterministicEntropy::repeated(0x49)),
        management_scope(),
        CREATED_AT.to_owned(),
    )
    .await
    .unwrap_err();

    assert_eq!(
        *events.lock().unwrap(),
        [
            "metadata.binding",
            "secret.put",
            "metadata.bind",
            "secret.get",
        ]
    );
    assert_eq!(secrets.gets.load(Ordering::SeqCst), 1);
    let rendered = format!("{error:?} {error}");
    assert!(!rendered.contains(&generated_raw));
    assert!(!rendered.contains(&invalid_read_back));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validation_rejects_noncanonical_base64url_trailing_bits() {
    let noncanonical_admin = format!("wok_admin_v1_{}B", "A".repeat(42));
    let admin_secrets = Arc::new(RecordingSecretStore::default());
    let stored = TokenMaterial::generate_admin(&DeterministicEntropy::repeated(0x4a))
        .unwrap()
        .into_response_value();
    let admin_ref = admin_secrets
        .put(&management_scope(), stored)
        .await
        .unwrap();
    admin_secrets.return_on_get(noncanonical_admin);
    let admin_metadata = Arc::new(FakeMetadataStore::default());
    admin_metadata.set_binding(admin_ref);

    let admin_error = AuthRegistry::bootstrap(
        admin_secrets,
        admin_metadata,
        Arc::new(DeterministicEntropy::repeated(0x4b)),
        management_scope(),
        CREATED_AT.to_owned(),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        admin_error,
        wokcore_server::auth::AuthError::InvalidManagementSecret
    ));

    let noncanonical_proxy = format!("wok_proxy_v1_{}B", "A".repeat(42));
    let (_registry, secrets, metadata, _) = existing_registry(0x4c).await;
    metadata.active.lock().unwrap().push(ClientTokenMetadata {
        token_id: "token-noncanonical".to_owned(),
        client_id: ClientId::new("wokrouter").unwrap(),
        digest: token_digest(&noncanonical_proxy),
        issued_at: CREATED_AT.to_owned(),
    });
    let registry = AuthRegistry::bootstrap(
        secrets,
        metadata,
        Arc::new(DeterministicEntropy::repeated(0x4d)),
        management_scope(),
        CREATED_AT.to_owned(),
    )
    .await
    .unwrap();

    assert!(registry.validate_client(&noncanonical_proxy).is_none());
    drop(registry);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issue_persists_before_exposing_material_and_failure_exposes_nothing() {
    let (registry, _secrets, metadata, _management_raw) = existing_registry(0x51).await;
    metadata.events.lock().unwrap().clear();

    let issued = registry
        .issue_client_token(
            "token-success".to_owned(),
            ClientId::new("wokrouter").unwrap(),
            CREATED_AT.to_owned(),
        )
        .await
        .unwrap();

    assert_eq!(*metadata.events.lock().unwrap(), ["metadata.issue"]);
    let response = issued.into_response_value();
    assert!(registry.validate_client(response.expose_secret()).is_some());

    metadata.fail_issue.store(true, Ordering::SeqCst);
    let error = registry
        .issue_client_token(
            "token-failure".to_owned(),
            ClientId::new("other-client").unwrap(),
            CREATED_AT.to_owned(),
        )
        .await
        .unwrap_err();
    let expected = TokenMaterial::generate_proxy(&DeterministicEntropy::repeated(0x53))
        .unwrap()
        .into_response_value();
    assert!(!format!("{error:?} {error}").contains(expected.expose_secret()));
    assert!(registry.validate_client(expected.expose_secret()).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoke_commits_before_replacing_the_immutable_snapshot() {
    let (registry, _secrets, metadata, _management_raw) = existing_registry(0x61).await;
    let issued = registry
        .issue_client_token(
            "token-revoke".to_owned(),
            ClientId::new("wokrouter").unwrap(),
            CREATED_AT.to_owned(),
        )
        .await
        .unwrap()
        .into_response_value();
    let raw = issued.expose_secret().to_owned();
    assert!(
        !registry
            .revoke_client_token(
                ClientId::new("other-client").unwrap(),
                "token-revoke".to_owned(),
                REVOKED_AT.to_owned(),
            )
            .await
            .unwrap()
    );
    assert!(registry.validate_client(&raw).is_some());
    let (started, release, completed) = metadata.arm_revoke_gate();
    let registry_for_revoke = Arc::clone(&registry);
    let revoke = tokio::spawn(async move {
        registry_for_revoke
            .revoke_client_token(
                ClientId::new("wokrouter").unwrap(),
                "token-revoke".to_owned(),
                REVOKED_AT.to_owned(),
            )
            .await
    });

    started.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(registry.validate_client(&raw).is_some());
    release.wait();
    completed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(revoke.await.unwrap().unwrap());
    assert!(registry.validate_client(&raw).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_issue_finishes_persistence_and_snapshot_without_delivering_material() {
    let (registry, _secrets, metadata, _management_raw) = existing_registry(0x81).await;
    let expected = TokenMaterial::generate_proxy(&DeterministicEntropy::repeated(0x82))
        .unwrap()
        .into_response_value();
    let expected_raw = expected.expose_secret().to_owned();
    let (started, release, completed) = metadata.arm_issue_gate();
    let registry_for_issue = Arc::clone(&registry);
    let issue = tokio::spawn(async move {
        registry_for_issue
            .issue_client_token(
                "token-cancelled-issue".to_owned(),
                ClientId::new("wokrouter").unwrap(),
                CREATED_AT.to_owned(),
            )
            .await
    });

    started.recv_timeout(Duration::from_secs(5)).unwrap();
    issue.abort();
    release.wait();
    assert!(issue.await.unwrap_err().is_cancelled());
    completed.recv_timeout(Duration::from_secs(5)).unwrap();
    wait_for_client_presence(&registry, &expected_raw, true).await;

    let persisted = metadata
        .active
        .lock()
        .unwrap()
        .iter()
        .find(|token| token.token_id == "token-cancelled-issue")
        .cloned()
        .unwrap();
    assert_eq!(persisted.digest, token_digest(&expected_raw));
    assert_eq!(
        registry.validate_client(&expected_raw).unwrap().client_id,
        ClientId::new("wokrouter").unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_status_waits_for_a_cancelled_owned_issue_to_finish() {
    let (registry, _secrets, metadata, _management_raw) = existing_registry(0xa1).await;
    let (started, release, completed) = metadata.arm_issue_gate();
    let registry_for_issue = Arc::clone(&registry);
    let issue = tokio::spawn(async move {
        registry_for_issue
            .issue_client_token(
                "token-status-race".to_owned(),
                ClientId::new("wokrouter").unwrap(),
                CREATED_AT.to_owned(),
            )
            .await
    });

    started.recv_timeout(Duration::from_secs(5)).unwrap();
    issue.abort();
    let registry_for_status = Arc::clone(&registry);
    let status = tokio::spawn(async move {
        registry_for_status
            .client_token_active(&ClientId::new("wokrouter").unwrap(), "token-status-race")
            .await
    });
    tokio::task::yield_now().await;
    assert!(!status.is_finished());

    release.wait();
    assert!(issue.await.unwrap_err().is_cancelled());
    completed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(status.await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_revoke_finishes_commit_and_snapshot_then_retry_is_idempotent() {
    let (registry, _secrets, metadata, _management_raw) = existing_registry(0x91).await;
    let raw = registry
        .issue_client_token(
            "token-cancelled-revoke".to_owned(),
            ClientId::new("wokrouter").unwrap(),
            CREATED_AT.to_owned(),
        )
        .await
        .unwrap()
        .into_response_value();
    let raw = raw.expose_secret().to_owned();
    let (started, release, completed) = metadata.arm_revoke_gate();
    let registry_for_revoke = Arc::clone(&registry);
    let revoke = tokio::spawn(async move {
        registry_for_revoke
            .revoke_client_token(
                ClientId::new("wokrouter").unwrap(),
                "token-cancelled-revoke".to_owned(),
                REVOKED_AT.to_owned(),
            )
            .await
    });

    started.recv_timeout(Duration::from_secs(5)).unwrap();
    revoke.abort();
    release.wait();
    assert!(revoke.await.unwrap_err().is_cancelled());
    completed.recv_timeout(Duration::from_secs(5)).unwrap();
    wait_for_client_presence(&registry, &raw, false).await;
    assert!(
        !registry
            .revoke_client_token(
                ClientId::new("wokrouter").unwrap(),
                "token-cancelled-revoke".to_owned(),
                REVOKED_AT.to_owned(),
            )
            .await
            .unwrap()
    );
    assert!(registry.validate_client(&raw).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unchanged_revoke_reconciles_a_stale_active_snapshot() {
    let (registry, _secrets, metadata, _management_raw) = existing_registry(0xa1).await;
    let raw = registry
        .issue_client_token(
            "token-stale-snapshot".to_owned(),
            ClientId::new("wokrouter").unwrap(),
            CREATED_AT.to_owned(),
        )
        .await
        .unwrap()
        .into_response_value();
    let raw = raw.expose_secret().to_owned();
    metadata.active.lock().unwrap().clear();
    assert!(registry.validate_client(&raw).is_some());

    assert!(
        !registry
            .revoke_client_token(
                ClientId::new("wokrouter").unwrap(),
                "token-stale-snapshot".to_owned(),
                REVOKED_AT.to_owned(),
            )
            .await
            .unwrap()
    );
    assert!(registry.validate_client(&raw).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_bootstrap_after_put_finishes_binding_readback_and_load() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let secrets = Arc::new(RecordingSecretStore {
        events: Arc::clone(&events),
        ..RecordingSecretStore::default()
    });
    let metadata = Arc::new(FakeMetadataStore::with_events(Arc::clone(&events)));
    let expected = TokenMaterial::generate_admin(&DeterministicEntropy::repeated(0xb1))
        .unwrap()
        .into_response_value();
    let expected_raw = expected.expose_secret().to_owned();
    let (started, release, completed) = secrets.arm_put_gate();
    let secrets_for_bootstrap = Arc::clone(&secrets);
    let metadata_for_bootstrap = Arc::clone(&metadata);
    let bootstrap = tokio::spawn(async move {
        AuthRegistry::bootstrap(
            secrets_for_bootstrap,
            metadata_for_bootstrap,
            Arc::new(DeterministicEntropy::repeated(0xb1)),
            management_scope(),
            CREATED_AT.to_owned(),
        )
        .await
    });

    started.recv_timeout(Duration::from_secs(5)).unwrap();
    bootstrap.abort();
    release.wait();
    assert!(bootstrap.await.unwrap_err().is_cancelled());
    completed.recv_timeout(Duration::from_secs(5)).unwrap();
    wait_for_event(&events, "metadata.load").await;

    let binding = metadata.binding.lock().unwrap().clone().unwrap();
    let persisted = secrets.get(&binding.secret_ref).await.unwrap();
    assert_eq!(persisted.expose_secret(), &expected_raw);
    assert!(metadata.orphans.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_bootstrap_bind_failure_tracks_the_written_secret_as_an_orphan() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let secrets = Arc::new(RecordingSecretStore {
        events: Arc::clone(&events),
        ..RecordingSecretStore::default()
    });
    let metadata = Arc::new(FakeMetadataStore::with_events(Arc::clone(&events)));
    metadata.fail_bind.store(true, Ordering::SeqCst);
    let (started, release, completed) = secrets.arm_put_gate();
    let secrets_for_bootstrap = Arc::clone(&secrets);
    let metadata_for_bootstrap = Arc::clone(&metadata);
    let bootstrap = tokio::spawn(async move {
        AuthRegistry::bootstrap(
            secrets_for_bootstrap,
            metadata_for_bootstrap,
            Arc::new(DeterministicEntropy::repeated(0xc1)),
            management_scope(),
            CREATED_AT.to_owned(),
        )
        .await
    });

    started.recv_timeout(Duration::from_secs(5)).unwrap();
    bootstrap.abort();
    release.wait();
    assert!(bootstrap.await.unwrap_err().is_cancelled());
    completed.recv_timeout(Duration::from_secs(5)).unwrap();
    wait_for_event(&events, "metadata.orphan").await;

    assert!(metadata.binding.lock().unwrap().is_none());
    let orphan = metadata.orphans.lock().unwrap().first().cloned().unwrap();
    assert!(secrets.get(&orphan).await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_thousand_parallel_validations_perform_no_metadata_io() {
    let (registry, _secrets, metadata, _management_raw) = existing_registry(0x71).await;
    let issued = registry
        .issue_client_token(
            "token-parallel".to_owned(),
            ClientId::new("wokrouter").unwrap(),
            CREATED_AT.to_owned(),
        )
        .await
        .unwrap()
        .into_response_value();
    let raw = Arc::new(issued.expose_secret().to_owned());
    let calls_before = metadata.calls.load(Ordering::SeqCst);
    let mut validations = tokio::task::JoinSet::new();

    for _ in 0..1_000 {
        let registry = Arc::clone(&registry);
        let raw = Arc::clone(&raw);
        validations.spawn(async move {
            assert!(registry.validate_client(&raw).is_some());
        });
    }
    while let Some(result) = validations.join_next().await {
        result.unwrap();
    }

    assert_eq!(metadata.calls.load(Ordering::SeqCst), calls_before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_client_tokens_are_valid_only_for_their_explicit_scopes() {
    let (registry, _secrets, _metadata, _management_raw) = existing_registry(0xd1).await;
    let issued = registry
        .issue_client_token_with_scopes(
            "token-session-reader".to_owned(),
            ClientId::new("wokrouter").unwrap(),
            CREATED_AT.to_owned(),
            vec![
                ClientTokenScope::SessionsRead,
                ClientTokenScope::DiagnosticsRead,
            ],
        )
        .await
        .unwrap()
        .into_response_value();
    let raw = issued.expose_secret();

    assert!(
        registry
            .validate_client_scope(raw, ClientTokenScope::SessionsRead)
            .is_some()
    );
    assert!(
        registry
            .validate_client_scope(raw, ClientTokenScope::DiagnosticsRead)
            .is_some()
    );
    assert!(
        registry
            .validate_client_scope(raw, ClientTokenScope::UsageRead)
            .is_none()
    );
    assert!(registry.validate_client(raw).is_none());
}
