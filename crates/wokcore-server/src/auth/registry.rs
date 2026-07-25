use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex},
};

use arc_swap::ArcSwap;
use secrecy::ExposeSecret;
use tokio::task;
use wokcore_core::{
    id::ClientId,
    secret::{SecretRef, SecretScope},
};
use wokcore_storage::{
    ClientTokenMetadata, RuntimeSecretBinding, SecretStore, StateStore, StorageError,
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
}

pub struct AuthRegistry {
    management_digest: TokenDigest,
    clients: ArcSwap<HashMap<TokenDigest, ActiveClient>>,
    metadata: Arc<dyn AuthMetadataStore>,
    entropy: Arc<dyn EntropySource>,
    mutation: tokio::sync::Mutex<()>,
}

impl AuthRegistry {
    pub async fn bootstrap(
        secrets: Arc<dyn SecretStore>,
        metadata: Arc<dyn AuthMetadataStore>,
        entropy: Arc<dyn EntropySource>,
        management_scope: SecretScope,
        created_at: String,
    ) -> Result<Self, AuthError> {
        let binding = run_metadata(Arc::clone(&metadata), move |metadata| {
            metadata.runtime_secret_binding(MANAGEMENT_BINDING_NAME)
        })
        .await?;

        let management_digest = if let Some(binding) = binding {
            let value = secrets
                .get(&binding.secret_ref)
                .await
                .map_err(|_| AuthError::SecretStore)?;
            if !is_admin_token(value.expose_secret()) {
                return Err(AuthError::InvalidManagementSecret);
            }
            TokenDigest::of(value.expose_secret())
        } else {
            let material = TokenMaterial::generate_admin(entropy.as_ref())?;
            let secret_ref = secrets
                .put(&management_scope, material.into_secret_value())
                .await
                .map_err(|_| AuthError::SecretStore)?;
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
            let value = secrets
                .get(&binding.secret_ref)
                .await
                .map_err(|_| AuthError::SecretStore)?;
            if !is_admin_token(value.expose_secret()) {
                return Err(AuthError::InvalidManagementSecret);
            }
            TokenDigest::of(value.expose_secret())
        };

        let active = run_metadata(Arc::clone(&metadata), |metadata| {
            metadata.load_active_client_tokens()
        })
        .await?;
        let clients = active_client_map(active)?;

        Ok(Self {
            management_digest,
            clients: ArcSwap::from_pointee(clients),
            metadata,
            entropy,
            mutation: tokio::sync::Mutex::new(()),
        })
    }

    pub fn validate_management(&self, candidate: &str) -> bool {
        if !is_admin_token(candidate) {
            return false;
        }
        self.management_digest
            .constant_time_matches(&TokenDigest::of(candidate))
    }

    pub fn validate_client(&self, candidate: &str) -> Option<AuthorizedClient> {
        if !is_proxy_token(candidate) {
            return None;
        }
        let digest = TokenDigest::of(candidate);
        let snapshot = self.clients.load();
        let active = snapshot.get(&digest)?;
        Some(AuthorizedClient {
            token_id: active.token_id.clone(),
            client_id: active.client_id.clone(),
        })
    }

    pub async fn issue_client_token(
        &self,
        token_id: String,
        client_id: ClientId,
        issued_at: String,
    ) -> Result<TokenMaterial, AuthError> {
        let _mutation = self.mutation.lock().await;
        let material = TokenMaterial::generate_proxy(self.entropy.as_ref())?;
        let digest = material.digest();
        let metadata = ClientTokenMetadata {
            token_id: token_id.clone(),
            client_id: client_id.clone(),
            digest: digest.into_bytes(),
            issued_at,
        };
        run_metadata(Arc::clone(&self.metadata), move |store| {
            store.issue_client_token(&metadata)
        })
        .await?;

        let mut clients = (**self.clients.load()).clone();
        clients.insert(
            digest,
            ActiveClient {
                token_id,
                client_id,
            },
        );
        self.clients.store(Arc::new(clients));
        Ok(material)
    }

    pub async fn revoke_client_token(
        &self,
        client_id: ClientId,
        token_id: String,
        revoked_at: String,
    ) -> Result<bool, AuthError> {
        let _mutation = self.mutation.lock().await;
        let persisted_client_id = client_id.clone();
        let persisted_token_id = token_id.clone();
        let changed = run_metadata(Arc::clone(&self.metadata), move |store| {
            store.revoke_client_token(&persisted_client_id, &persisted_token_id, &revoked_at)
        })
        .await?;
        if !changed {
            return Ok(false);
        }

        let mut clients = (**self.clients.load()).clone();
        clients.retain(|_, active| active.token_id != token_id || active.client_id != client_id);
        self.clients.store(Arc::new(clients));
        Ok(true)
    }
}

impl fmt::Debug for AuthRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthRegistry")
            .field("management_digest", &self.management_digest)
            .field("active_clients", &self.clients.load().len())
            .finish_non_exhaustive()
    }
}

fn active_client_map(
    active: Vec<ClientTokenMetadata>,
) -> Result<HashMap<TokenDigest, ActiveClient>, AuthError> {
    let mut clients = HashMap::with_capacity(active.len());
    for token in active {
        if clients
            .insert(
                TokenDigest::from(token.digest),
                ActiveClient {
                    token_id: token.token_id,
                    client_id: token.client_id,
                },
            )
            .is_some()
        {
            return Err(AuthError::Storage);
        }
    }
    Ok(clients)
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
        .map_err(|_| AuthError::Storage)
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("runtime authentication metadata operation failed")]
    Storage,
    #[error("runtime secret store operation failed")]
    SecretStore,
    #[error("runtime management secret has an invalid format")]
    InvalidManagementSecret,
    #[error("runtime management secret binding failed after orphan metadata was recorded")]
    BootstrapBinding,
    #[error("runtime management secret binding and orphan recovery both failed")]
    OrphanRecording { orphan_ref: SecretRef },
    #[error("blocking runtime authentication task failed")]
    BlockingTask,
    #[error(transparent)]
    Token(#[from] TokenError),
}
