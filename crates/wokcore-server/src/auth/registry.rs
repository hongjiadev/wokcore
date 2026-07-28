use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex},
};

use arc_swap::ArcSwap;
use secrecy::ExposeSecret;
use sha2::{Digest, Sha256};
use tokio::task;
use wokcore_core::{
    id::ClientId,
    secret::{SecretRef, SecretScope},
};
use wokcore_storage::{
    ClientTokenMetadata, ClientTokenScope, RuntimeSecretBinding, ScopedClientTokenMetadata,
    SecretStore, StateStore, StorageError,
};

use super::token::{
    EntropySource, TokenDigest, TokenError, TokenMaterial, is_admin_token, is_proxy_token,
};

const MANAGEMENT_BINDING_NAME: &str = "management";

pub trait AuthMetadataStore: Send + Sync {
    fn runtime_secret_binding(
        &self,
        name: &str,
    ) -> Result<Option<RuntimeSecretBinding>, StorageError>;

    fn bind_runtime_secret_if_absent(
        &self,
        name: &str,
        secret_ref: &SecretRef,
        created_at: &str,
    ) -> Result<RuntimeSecretBinding, StorageError>;

    fn record_orphan_secret(
        &self,
        secret_ref: &SecretRef,
        created_at: &str,
    ) -> Result<(), StorageError>;

    fn load_active_client_tokens(&self) -> Result<Vec<ClientTokenMetadata>, StorageError>;

    fn issue_client_token(&self, token: &ClientTokenMetadata) -> Result<(), StorageError>;

    fn load_active_scoped_client_tokens(
        &self,
    ) -> Result<Vec<ScopedClientTokenMetadata>, StorageError> {
        self.load_active_client_tokens().map(|tokens| {
            tokens
                .into_iter()
                .map(|token| ScopedClientTokenMetadata {
                    token,
                    scopes: vec![ClientTokenScope::ProxyUse],
                })
                .collect()
        })
    }

    fn issue_client_token_with_scopes(
        &self,
        token: &ClientTokenMetadata,
        scopes: &[ClientTokenScope],
    ) -> Result<(), StorageError> {
        if scopes == [ClientTokenScope::ProxyUse] {
            self.issue_client_token(token)
        } else {
            Err(StorageError::InvalidStateRecord {
                message: "authentication metadata store does not support explicit scopes"
                    .to_owned(),
            })
        }
    }

    fn revoke_client_token(
        &self,
        client_id: &ClientId,
        token_id: &str,
        revoked_at: &str,
    ) -> Result<bool, StorageError>;
}

pub struct StateAuthMetadataStore {
    state: Mutex<StateStore>,
}

impl StateAuthMetadataStore {
    pub fn new(state: StateStore) -> Self {
        Self {
            state: Mutex::new(state),
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, StateStore>, StorageError> {
        self.state
            .lock()
            .map_err(|_| StorageError::StateDatabaseCorrupt {
                message: "runtime authentication metadata lock is poisoned".to_owned(),
            })
    }
}

impl fmt::Debug for StateAuthMetadataStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StateAuthMetadataStore")
            .finish_non_exhaustive()
    }
}

impl AuthMetadataStore for StateAuthMetadataStore {
    fn runtime_secret_binding(
        &self,
        name: &str,
    ) -> Result<Option<RuntimeSecretBinding>, StorageError> {
        self.lock()?.runtime_secret_binding(name)
    }

    fn bind_runtime_secret_if_absent(
        &self,
        name: &str,
        secret_ref: &SecretRef,
        created_at: &str,
    ) -> Result<RuntimeSecretBinding, StorageError> {
        self.lock()?
            .bind_runtime_secret_if_absent(name, secret_ref, created_at)
    }

    fn record_orphan_secret(
        &self,
        secret_ref: &SecretRef,
        created_at: &str,
    ) -> Result<(), StorageError> {
        self.lock()?.record_orphan_secret(secret_ref, created_at)
    }

    fn load_active_client_tokens(&self) -> Result<Vec<ClientTokenMetadata>, StorageError> {
        self.lock()?.load_active_client_tokens()
    }

    fn issue_client_token(&self, token: &ClientTokenMetadata) -> Result<(), StorageError> {
        self.lock()?.issue_client_token(token)
    }

    fn load_active_scoped_client_tokens(
        &self,
    ) -> Result<Vec<ScopedClientTokenMetadata>, StorageError> {
        self.lock()?.load_active_scoped_client_tokens()
    }

    fn issue_client_token_with_scopes(
        &self,
        token: &ClientTokenMetadata,
        scopes: &[ClientTokenScope],
    ) -> Result<(), StorageError> {
        self.lock()?.issue_client_token_with_scopes(token, scopes)
    }

    fn revoke_client_token(
        &self,
        client_id: &ClientId,
        token_id: &str,
        revoked_at: &str,
    ) -> Result<bool, StorageError> {
        self.lock()?
            .revoke_client_token(client_id, token_id, revoked_at)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedClient {
    pub token_id: String,
    pub client_id: ClientId,
}

#[derive(Clone, Debug)]
struct ActiveClient {
    token_id: String,
    client_id: ClientId,
    scopes: Vec<ClientTokenScope>,
}

pub struct AuthRegistry {
    state: Arc<AuthRegistryState>,
}

struct AuthRegistryState {
    management_digest: TokenDigest,
    clients: ArcSwap<HashMap<TokenDigest, ActiveClient>>,
    metadata: Arc<dyn AuthMetadataStore>,
    entropy: Arc<dyn EntropySource>,
    mutation: Arc<tokio::sync::Mutex<()>>,
}

impl AuthRegistry {
    pub fn installation_id(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"wokcore.installation-id.v1");
        digest.update(self.state.management_digest.into_bytes());
        format!("{:x}", digest.finalize())
    }

    pub fn session_domain_key(&self) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(b"wokcore.session-domain-key.v1");
        digest.update(self.state.management_digest.into_bytes());
        digest.finalize().into()
    }

    pub async fn bootstrap(
        secrets: Arc<dyn SecretStore>,
        metadata: Arc<dyn AuthMetadataStore>,
        entropy: Arc<dyn EntropySource>,
        management_scope: SecretScope,
        created_at: String,
    ) -> Result<Self, AuthError> {
        let mutation = Arc::new(tokio::sync::Mutex::new(()));
        await_mutation_task(task::spawn(Self::bootstrap_owned(
            secrets,
            metadata,
            entropy,
            management_scope,
            created_at,
            mutation,
        )))
        .await
    }

    async fn bootstrap_owned(
        secrets: Arc<dyn SecretStore>,
        metadata: Arc<dyn AuthMetadataStore>,
        entropy: Arc<dyn EntropySource>,
        management_scope: SecretScope,
        created_at: String,
        mutation: Arc<tokio::sync::Mutex<()>>,
    ) -> Result<Self, AuthError> {
        let _mutation = mutation.lock().await;
        let binding = run_metadata(Arc::clone(&metadata), move |metadata| {
            metadata.runtime_secret_binding(MANAGEMENT_BINDING_NAME)
        })
        .await?;

        let management_digest = if let Some(binding) = binding {
            let value = secrets.get(&binding.secret_ref).await.map_err(|error| {
                AuthError::SecretStoreRead(AuthSecretStoreFailure::classify(&error))
            })?;
            if !is_admin_token(value.expose_secret()) {
                return Err(AuthError::InvalidManagementSecret);
            }
            TokenDigest::of(value.expose_secret())
        } else {
            let material = TokenMaterial::generate_admin(entropy.as_ref())?;
            let secret_ref = secrets
                .put(&management_scope, material.into_secret_value())
                .await
                .map_err(|error| {
                    AuthError::SecretStoreWrite(AuthSecretStoreFailure::classify(&error))
                })?;
            let binding_ref = secret_ref.clone();
            let binding_created_at = created_at.clone();
            let bind_result = run_metadata(Arc::clone(&metadata), move |metadata| {
                metadata.bind_runtime_secret_if_absent(
                    MANAGEMENT_BINDING_NAME,
                    &binding_ref,
                    &binding_created_at,
                )
            })
            .await;
            let binding = match bind_result {
                Ok(binding) => binding,
                Err(_) => {
                    let orphan_ref = secret_ref.clone();
                    let orphan_created_at = created_at.clone();
                    if run_metadata(Arc::clone(&metadata), move |metadata| {
                        metadata.record_orphan_secret(&orphan_ref, &orphan_created_at)
                    })
                    .await
                    .is_err()
                    {
                        return Err(AuthError::OrphanRecording {
                            orphan_ref: secret_ref,
                        });
                    }
                    return Err(AuthError::BootstrapBinding);
                }
            };
            let value = secrets.get(&binding.secret_ref).await.map_err(|error| {
                AuthError::SecretStoreRead(AuthSecretStoreFailure::classify(&error))
            })?;
            if !is_admin_token(value.expose_secret()) {
                return Err(AuthError::InvalidManagementSecret);
            }
            TokenDigest::of(value.expose_secret())
        };

        let active = run_metadata(Arc::clone(&metadata), |metadata| {
            metadata.load_active_scoped_client_tokens()
        })
        .await?;
        let clients = active_client_map(active)?;

        Ok(Self {
            state: Arc::new(AuthRegistryState {
                management_digest,
                clients: ArcSwap::from_pointee(clients),
                metadata,
                entropy,
                mutation: Arc::clone(&mutation),
            }),
        })
    }

    pub fn validate_management(&self, candidate: &str) -> bool {
        if !is_admin_token(candidate) {
            return false;
        }
        self.state
            .management_digest
            .constant_time_matches(&TokenDigest::of(candidate))
    }

    pub fn validate_client(&self, candidate: &str) -> Option<AuthorizedClient> {
        self.validate_client_scope(candidate, ClientTokenScope::ProxyUse)
    }

    pub fn validate_any_client(&self, candidate: &str) -> Option<AuthorizedClient> {
        if !is_proxy_token(candidate) {
            return None;
        }
        let digest = TokenDigest::of(candidate);
        let snapshot = self.state.clients.load();
        let active = snapshot.get(&digest)?;
        Some(AuthorizedClient {
            token_id: active.token_id.clone(),
            client_id: active.client_id.clone(),
        })
    }

    pub fn validate_client_scope(
        &self,
        candidate: &str,
        required_scope: ClientTokenScope,
    ) -> Option<AuthorizedClient> {
        if !is_proxy_token(candidate) {
            return None;
        }
        let digest = TokenDigest::of(candidate);
        let snapshot = self.state.clients.load();
        let active = snapshot.get(&digest)?;
        if !active.scopes.contains(&required_scope) {
            return None;
        }
        Some(AuthorizedClient {
            token_id: active.token_id.clone(),
            client_id: active.client_id.clone(),
        })
    }

    pub async fn client_token_active(&self, client_id: &ClientId, token_id: &str) -> bool {
        let _mutation = self.state.mutation.lock().await;
        self.state
            .clients
            .load()
            .values()
            .any(|active| active.client_id == *client_id && active.token_id == token_id)
    }

    pub async fn issue_client_token(
        &self,
        token_id: String,
        client_id: ClientId,
        issued_at: String,
    ) -> Result<TokenMaterial, AuthError> {
        self.issue_client_token_with_scopes(
            token_id,
            client_id,
            issued_at,
            vec![ClientTokenScope::ProxyUse],
        )
        .await
    }

    pub async fn issue_client_token_with_scopes(
        &self,
        token_id: String,
        client_id: ClientId,
        issued_at: String,
        scopes: Vec<ClientTokenScope>,
    ) -> Result<TokenMaterial, AuthError> {
        validate_scopes(&scopes)?;
        let state = Arc::clone(&self.state);
        let mutation = Arc::clone(&state.mutation).lock_owned().await;
        await_mutation_task(task::spawn(async move {
            Self::issue_client_token_owned(state, mutation, token_id, client_id, issued_at, scopes)
                .await
        }))
        .await
    }

    async fn issue_client_token_owned(
        state: Arc<AuthRegistryState>,
        _mutation: tokio::sync::OwnedMutexGuard<()>,
        token_id: String,
        client_id: ClientId,
        issued_at: String,
        scopes: Vec<ClientTokenScope>,
    ) -> Result<TokenMaterial, AuthError> {
        let material = TokenMaterial::generate_proxy(state.entropy.as_ref())?;
        let digest = material.digest();
        let metadata = ClientTokenMetadata {
            token_id: token_id.clone(),
            client_id: client_id.clone(),
            digest: digest.into_bytes(),
            issued_at,
        };
        let persisted_scopes = scopes.clone();
        run_metadata(Arc::clone(&state.metadata), move |store| {
            store.issue_client_token_with_scopes(&metadata, &persisted_scopes)
        })
        .await?;

        let mut clients = (**state.clients.load()).clone();
        clients.insert(
            digest,
            ActiveClient {
                token_id,
                client_id,
                scopes,
            },
        );
        state.clients.store(Arc::new(clients));
        Ok(material)
    }

    pub async fn revoke_client_token(
        &self,
        client_id: ClientId,
        token_id: String,
        revoked_at: String,
    ) -> Result<bool, AuthError> {
        let state = Arc::clone(&self.state);
        let mutation = Arc::clone(&state.mutation).lock_owned().await;
        await_mutation_task(task::spawn(async move {
            Self::revoke_client_token_owned(state, mutation, client_id, token_id, revoked_at).await
        }))
        .await
    }

    async fn revoke_client_token_owned(
        state: Arc<AuthRegistryState>,
        _mutation: tokio::sync::OwnedMutexGuard<()>,
        client_id: ClientId,
        token_id: String,
        revoked_at: String,
    ) -> Result<bool, AuthError> {
        let persisted_client_id = client_id.clone();
        let persisted_token_id = token_id.clone();
        let changed = run_metadata(Arc::clone(&state.metadata), move |store| {
            store.revoke_client_token(&persisted_client_id, &persisted_token_id, &revoked_at)
        })
        .await?;

        let mut clients = (**state.clients.load()).clone();
        clients.retain(|_, active| active.token_id != token_id || active.client_id != client_id);
        state.clients.store(Arc::new(clients));
        Ok(changed)
    }
}

impl fmt::Debug for AuthRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthRegistry")
            .field("management_digest", &self.state.management_digest)
            .field("active_clients", &self.state.clients.load().len())
            .finish_non_exhaustive()
    }
}

fn active_client_map(
    active: Vec<ScopedClientTokenMetadata>,
) -> Result<HashMap<TokenDigest, ActiveClient>, AuthError> {
    let mut clients = HashMap::with_capacity(active.len());
    for scoped in active {
        validate_scopes(&scoped.scopes)?;
        let token = scoped.token;
        if clients
            .insert(
                TokenDigest::from(token.digest),
                ActiveClient {
                    token_id: token.token_id,
                    client_id: token.client_id,
                    scopes: scoped.scopes,
                },
            )
            .is_some()
        {
            return Err(AuthError::Storage(StorageError::StateDatabaseCorrupt {
                message: "active client token metadata contains a duplicate digest".to_owned(),
            }));
        }
    }
    Ok(clients)
}

fn validate_scopes(scopes: &[ClientTokenScope]) -> Result<(), AuthError> {
    if scopes.is_empty() {
        return Err(AuthError::InvalidScopes);
    }
    let mut sorted = scopes.to_vec();
    sorted.sort_unstable();
    if sorted.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(AuthError::InvalidScopes);
    }
    Ok(())
}

async fn run_metadata<T>(
    metadata: Arc<dyn AuthMetadataStore>,
    operation: impl FnOnce(&dyn AuthMetadataStore) -> Result<T, StorageError> + Send + 'static,
) -> Result<T, AuthError>
where
    T: Send + 'static,
{
    task::spawn_blocking(move || operation(metadata.as_ref()))
        .await
        .map_err(|_| AuthError::BlockingTask)?
        .map_err(AuthError::Storage)
}

async fn await_mutation_task<T>(
    operation: task::JoinHandle<Result<T, AuthError>>,
) -> Result<T, AuthError> {
    operation.await.map_err(|_| AuthError::MutationTask)?
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthSecretStoreFailure {
    Unavailable,
    PlatformFailure,
    Other,
}

impl AuthSecretStoreFailure {
    fn classify(error: &StorageError) -> Self {
        match error {
            StorageError::SecretBackendUnavailable => Self::Unavailable,
            StorageError::SecretBackendPlatformFailure => Self::PlatformFailure,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("runtime authentication metadata operation failed")]
    Storage(#[source] StorageError),
    #[error("runtime secret store read operation failed")]
    SecretStoreRead(AuthSecretStoreFailure),
    #[error("runtime secret store write operation failed")]
    SecretStoreWrite(AuthSecretStoreFailure),
    #[error("runtime management secret has an invalid format")]
    InvalidManagementSecret,
    #[error("runtime management secret binding failed after orphan metadata was recorded")]
    BootstrapBinding,
    #[error("runtime management secret binding and orphan recovery both failed")]
    OrphanRecording { orphan_ref: SecretRef },
    #[error("blocking runtime authentication task failed")]
    BlockingTask,
    #[error("runtime authentication mutation task failed")]
    MutationTask,
    #[error("client token scopes are empty or contain duplicates")]
    InvalidScopes,
    #[error(transparent)]
    Token(#[from] TokenError),
}
