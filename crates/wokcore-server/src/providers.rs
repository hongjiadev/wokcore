use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
};

use secrecy::SecretString;
use tokio::sync::Mutex;
use wokcore_core::{
    config::{AccountAuthConfig, ProviderConfig, RoutingConfig},
    secret::{SecretRef, SecretScope},
};
use wokcore_engine::{
    catalog::ProviderCatalog,
    models::PublicModelMetadata,
    snapshot::{RuntimeSnapshot, SnapshotPublisher},
};
use wokcore_storage::{
    AppConfig, ConfigStore, SecretStore, ServerConfig, StorageError, VersionedConfig,
};

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCandidate {
    pub providers: ProviderConfig,
    pub routing: RoutingConfig,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct ProviderValidation {
    pub provider_count: usize,
    pub models: Vec<PublicModelMetadata>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReloadStatus {
    Ready,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct ProviderRuntimeStatus {
    pub revision: u64,
    pub snapshot_revision: u64,
    pub reload_status: ReloadStatus,
    pub provider_count: usize,
    pub models: Vec<PublicModelMetadata>,
    pub providers: ProviderConfig,
    pub routing: RoutingConfig,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct ProviderCommit {
    pub revision: u64,
    pub snapshot_revision: u64,
    pub provider_count: usize,
    pub models: Vec<PublicModelMetadata>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct ProviderSecretMetadata {
    pub secret_ref: SecretRef,
}

struct ActiveConfiguration {
    revision: u64,
    snapshot_revision: u64,
    reload_status: ReloadStatus,
    server: ServerConfig,
    candidate: ProviderCandidate,
}

#[derive(Clone)]
pub struct ProviderManagement {
    catalog: Arc<ProviderCatalog>,
    store: ConfigStore,
    secrets: Arc<dyn SecretStore>,
    publisher: Arc<SnapshotPublisher>,
    active: Arc<RwLock<ActiveConfiguration>>,
    protected_secrets: Arc<RwLock<Vec<SecretRef>>>,
    mutation: Arc<Mutex<()>>,
}

impl ProviderManagement {
    pub fn open(
        config_path: impl Into<PathBuf>,
        secrets: Arc<dyn SecretStore>,
    ) -> Result<Self, ProviderManagementError> {
        let store = ConfigStore::new(config_path);
        let loaded = store.load().map_err(map_config_load_error)?;
        Self::from_loaded(store, loaded, secrets)
    }

    pub fn from_loaded(
        store: ConfigStore,
        loaded: VersionedConfig,
        secrets: Arc<dyn SecretStore>,
    ) -> Result<Self, ProviderManagementError> {
        let catalog =
            ProviderCatalog::bundled().map_err(|_| ProviderManagementError::InvalidCatalog)?;
        let candidate = ProviderCandidate {
            providers: loaded.config.providers.clone(),
            routing: loaded.config.routing.clone(),
        };
        let snapshot = build_snapshot(&catalog, &candidate)?;
        Ok(Self {
            catalog: Arc::new(catalog),
            store,
            secrets,
            publisher: Arc::new(SnapshotPublisher::new(snapshot)),
            active: Arc::new(RwLock::new(ActiveConfiguration {
                revision: loaded.revision,
                snapshot_revision: loaded.revision,
                reload_status: ReloadStatus::Ready,
                server: loaded.config.server,
                candidate,
            })),
            protected_secrets: Arc::new(RwLock::new(Vec::new())),
            mutation: Arc::new(Mutex::new(())),
        })
    }

    pub async fn protect_secret_ref(
        &self,
        secret_ref: SecretRef,
    ) -> Result<(), ProviderManagementError> {
        let owned = self.clone();
        await_owned(tokio::spawn(async move {
            owned.protect_secret_ref_owned(secret_ref).await
        }))
        .await
    }

    async fn protect_secret_ref_owned(
        &self,
        secret_ref: SecretRef,
    ) -> Result<(), ProviderManagementError> {
        let _mutation = self.mutation.lock().await;
        if self
            .active
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .candidate
            .providers
            .accounts
            .iter()
            .any(|account| auth_references(&account.auth, &secret_ref))
        {
            return Err(ProviderManagementError::SecretProtected);
        }
        let mut protected = self
            .protected_secrets
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !protected.contains(&secret_ref) {
            protected.push(secret_ref);
        }
        Ok(())
    }

    pub fn catalog(&self) -> &ProviderCatalog {
        self.catalog.as_ref()
    }

    pub fn status(&self) -> ProviderRuntimeStatus {
        let active = self
            .active
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snapshot = self.publisher.load();
        ProviderRuntimeStatus {
            revision: active.revision,
            snapshot_revision: active.snapshot_revision,
            reload_status: active.reload_status,
            provider_count: snapshot.provider_count(),
            models: snapshot.public_models().to_vec(),
            providers: active.candidate.providers.clone(),
            routing: active.candidate.routing.clone(),
        }
    }

    pub fn models(&self) -> Vec<PublicModelMetadata> {
        self.publisher.load().public_models().to_vec()
    }

    pub fn snapshot(&self) -> Arc<RuntimeSnapshot> {
        self.publisher.load()
    }

    pub fn validate(
        &self,
        candidate: &ProviderCandidate,
    ) -> Result<ProviderValidation, ProviderManagementError> {
        self.reject_protected_candidate(candidate)?;
        let snapshot = build_snapshot(&self.catalog, candidate)?;
        Ok(ProviderValidation {
            provider_count: snapshot.provider_count(),
            models: snapshot.public_models().to_vec(),
        })
    }

    pub async fn commit(
        &self,
        expected_revision: u64,
        candidate: ProviderCandidate,
    ) -> Result<ProviderCommit, ProviderManagementError> {
        let owned = self.clone();
        await_owned(tokio::spawn(async move {
            owned.commit_owned(expected_revision, candidate).await
        }))
        .await
    }

    async fn commit_owned(
        &self,
        expected_revision: u64,
        candidate: ProviderCandidate,
    ) -> Result<ProviderCommit, ProviderManagementError> {
        let snapshot = build_snapshot(&self.catalog, &candidate)?;
        let _mutation = self.mutation.lock().await;
        self.reject_protected_candidate(&candidate)?;
        let server = {
            let active = self
                .active
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if active.revision != expected_revision {
                return Err(ProviderManagementError::RevisionConflict);
            }
            active.server.clone()
        };
        let config = AppConfig {
            server,
            providers: candidate.providers.clone(),
            routing: candidate.routing.clone(),
        };
        let committed_revision = expected_revision
            .checked_add(1)
            .ok_or(ProviderManagementError::InvalidConfiguration)?;
        let store = self.store.clone();
        let write_config = config.clone();
        let commit_result =
            tokio::task::spawn_blocking(move || store.commit(expected_revision, &write_config))
                .await
                .map_err(|_| ProviderManagementError::StorageFailure)?;
        let committed = match commit_result {
            Ok(committed) => committed,
            Err(StorageError::Io { .. }) => {
                self.reconcile_indeterminate_commit(committed_revision, &config)
                    .await?
            }
            Err(error) => return Err(map_commit_error(error)),
        };
        let revision = committed.revision;
        let published = self.install(committed, candidate, ReloadStatus::Ready, snapshot);
        Ok(ProviderCommit {
            revision,
            snapshot_revision: revision,
            provider_count: published.provider_count(),
            models: published.public_models().to_vec(),
        })
    }

    pub async fn reload(&self) -> Result<ProviderCommit, ProviderManagementError> {
        let owned = self.clone();
        await_owned(tokio::spawn(async move { owned.reload_owned().await })).await
    }

    async fn reload_owned(&self) -> Result<ProviderCommit, ProviderManagementError> {
        let _mutation = self.mutation.lock().await;
        let store = self.store.clone();
        let loaded = tokio::task::spawn_blocking(move || store.load())
            .await
            .map_err(|_| {
                self.mark_reload_failed();
                ProviderManagementError::StorageFailure
            })?
            .map_err(|error| {
                self.mark_reload_failed();
                map_config_load_error(error)
            })?;
        let candidate = ProviderCandidate {
            providers: loaded.config.providers.clone(),
            routing: loaded.config.routing.clone(),
        };
        self.reject_protected_candidate(&candidate)
            .inspect_err(|_| {
                self.mark_reload_failed();
            })?;
        let snapshot = build_snapshot(&self.catalog, &candidate).inspect_err(|_| {
            self.mark_reload_failed();
        })?;
        let revision = loaded.revision;
        let published = self.install(loaded, candidate, ReloadStatus::Ready, snapshot);
        Ok(ProviderCommit {
            revision,
            snapshot_revision: revision,
            provider_count: published.provider_count(),
            models: published.public_models().to_vec(),
        })
    }

    pub async fn create_secret(
        &self,
        scope: SecretScope,
        value: SecretString,
    ) -> Result<ProviderSecretMetadata, ProviderManagementError> {
        let owned = self.clone();
        await_owned(tokio::spawn(async move {
            owned.create_secret_owned(scope, value).await
        }))
        .await
    }

    async fn create_secret_owned(
        &self,
        scope: SecretScope,
        value: SecretString,
    ) -> Result<ProviderSecretMetadata, ProviderManagementError> {
        let _mutation = self.mutation.lock().await;
        if scope.provider_id.as_str() == "wokcore-runtime" {
            return Err(ProviderManagementError::SecretProtected);
        }
        let secret_ref = self
            .secrets
            .put_scoped(&scope, value)
            .await
            .map_err(map_secret_error)?;
        Ok(ProviderSecretMetadata { secret_ref })
    }

    pub async fn replace_secret(
        &self,
        secret_ref: &SecretRef,
        value: SecretString,
    ) -> Result<ProviderSecretMetadata, ProviderManagementError> {
        let owned = self.clone();
        let secret_ref = secret_ref.clone();
        await_owned(tokio::spawn(async move {
            owned.replace_secret_owned(secret_ref, value).await
        }))
        .await
    }

    async fn replace_secret_owned(
        &self,
        secret_ref: SecretRef,
        value: SecretString,
    ) -> Result<ProviderSecretMetadata, ProviderManagementError> {
        let _mutation = self.mutation.lock().await;
        if self.secret_is_protected(&secret_ref) {
            return Err(ProviderManagementError::SecretProtected);
        }
        self.secrets
            .replace(&secret_ref, value)
            .await
            .map_err(map_secret_error)?;
        Ok(ProviderSecretMetadata { secret_ref })
    }

    pub async fn delete_secret(
        &self,
        secret_ref: &SecretRef,
    ) -> Result<ProviderSecretMetadata, ProviderManagementError> {
        let owned = self.clone();
        let secret_ref = secret_ref.clone();
        await_owned(tokio::spawn(async move {
            owned.delete_secret_owned(secret_ref).await
        }))
        .await
    }

    async fn delete_secret_owned(
        &self,
        secret_ref: SecretRef,
    ) -> Result<ProviderSecretMetadata, ProviderManagementError> {
        let _mutation = self.mutation.lock().await;
        if self.secret_is_protected(&secret_ref) {
            return Err(ProviderManagementError::SecretProtected);
        }
        if self.secret_is_referenced(&secret_ref) {
            return Err(ProviderManagementError::SecretInUse);
        }
        let existing = self
            .secrets
            .get(&secret_ref)
            .await
            .map_err(map_secret_error)?;
        drop(existing);
        self.secrets
            .delete(&secret_ref)
            .await
            .map_err(map_secret_error)?;
        Ok(ProviderSecretMetadata { secret_ref })
    }

    async fn reconcile_indeterminate_commit(
        &self,
        committed_revision: u64,
        config: &AppConfig,
    ) -> Result<VersionedConfig, ProviderManagementError> {
        let store = self.store.clone();
        let loaded = tokio::task::spawn_blocking(move || store.load())
            .await
            .map_err(|_| ProviderManagementError::StorageFailure)?
            .map_err(|_| ProviderManagementError::StorageFailure)?;
        if loaded.revision == committed_revision && loaded.config == *config {
            Ok(loaded)
        } else {
            Err(ProviderManagementError::StorageFailure)
        }
    }

    fn install(
        &self,
        loaded: VersionedConfig,
        candidate: ProviderCandidate,
        reload_status: ReloadStatus,
        snapshot: RuntimeSnapshot,
    ) -> Arc<RuntimeSnapshot> {
        let mut active = self
            .active
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let published = self.publisher.publish(snapshot);
        *active = ActiveConfiguration {
            revision: loaded.revision,
            snapshot_revision: loaded.revision,
            reload_status,
            server: loaded.config.server,
            candidate,
        };
        published
    }

    fn mark_reload_failed(&self) {
        self.active
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .reload_status = ReloadStatus::Failed;
    }

    fn secret_is_referenced(&self, secret_ref: &SecretRef) -> bool {
        self.active
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .candidate
            .providers
            .accounts
            .iter()
            .any(|account| match &account.auth {
                AccountAuthConfig::Forward { credential } => credential == secret_ref,
                AccountAuthConfig::Oauth { access, refresh } => {
                    access == secret_ref || refresh.as_ref() == Some(secret_ref)
                }
                AccountAuthConfig::ApiKey { secret } => secret == secret_ref,
                AccountAuthConfig::Local => false,
            })
    }

    fn secret_is_protected(&self, secret_ref: &SecretRef) -> bool {
        self.protected_secrets
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(secret_ref)
    }

    fn reject_protected_candidate(
        &self,
        candidate: &ProviderCandidate,
    ) -> Result<(), ProviderManagementError> {
        let protected = self
            .protected_secrets
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let uses_protected = candidate
            .providers
            .accounts
            .iter()
            .any(|account| auth_references_any(&account.auth, &protected));
        if uses_protected {
            Err(ProviderManagementError::SecretProtected)
        } else {
            Ok(())
        }
    }
}

async fn await_owned<T>(
    operation: tokio::task::JoinHandle<Result<T, ProviderManagementError>>,
) -> Result<T, ProviderManagementError> {
    operation
        .await
        .map_err(|_| ProviderManagementError::StorageFailure)?
}

fn auth_references_any(auth: &AccountAuthConfig, protected: &[SecretRef]) -> bool {
    match auth {
        AccountAuthConfig::Forward { credential } => protected.contains(credential),
        AccountAuthConfig::Oauth { access, refresh } => {
            protected.contains(access)
                || refresh
                    .as_ref()
                    .is_some_and(|refresh| protected.contains(refresh))
        }
        AccountAuthConfig::ApiKey { secret } => protected.contains(secret),
        AccountAuthConfig::Local => false,
    }
}

fn auth_references(auth: &AccountAuthConfig, secret_ref: &SecretRef) -> bool {
    match auth {
        AccountAuthConfig::Forward { credential } => credential == secret_ref,
        AccountAuthConfig::Oauth { access, refresh } => {
            access == secret_ref || refresh.as_ref() == Some(secret_ref)
        }
        AccountAuthConfig::ApiKey { secret } => secret == secret_ref,
        AccountAuthConfig::Local => false,
    }
}

fn build_snapshot(
    catalog: &ProviderCatalog,
    candidate: &ProviderCandidate,
) -> Result<RuntimeSnapshot, ProviderManagementError> {
    RuntimeSnapshot::build(catalog, &candidate.providers, &candidate.routing)
        .map_err(|_| ProviderManagementError::InvalidConfiguration)
}

fn map_config_load_error(error: StorageError) -> ProviderManagementError {
    match error {
        StorageError::InvalidConfig { .. } | StorageError::SerializeConfig { .. } => {
            ProviderManagementError::InvalidConfiguration
        }
        _ => ProviderManagementError::StorageFailure,
    }
}

fn map_commit_error(error: StorageError) -> ProviderManagementError {
    match error {
        StorageError::RevisionConflict { .. } => ProviderManagementError::RevisionConflict,
        StorageError::InvalidConfig { .. } | StorageError::SerializeConfig { .. } => {
            ProviderManagementError::InvalidConfiguration
        }
        _ => ProviderManagementError::StorageFailure,
    }
}

fn map_secret_error(error: StorageError) -> ProviderManagementError {
    match error {
        StorageError::SecretNotFound => ProviderManagementError::SecretNotFound,
        StorageError::SecretAlreadyExists => ProviderManagementError::SecretAlreadyExists,
        StorageError::ReadOnlySecretStore => ProviderManagementError::ReadOnlySecretStore,
        _ => ProviderManagementError::StorageFailure,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ProviderManagementError {
    #[error("the bundled Provider catalog is invalid")]
    InvalidCatalog,
    #[error("the Provider configuration is invalid")]
    InvalidConfiguration,
    #[error("the Provider configuration revision conflicts")]
    RevisionConflict,
    #[error("the Provider secret was not found")]
    SecretNotFound,
    #[error("a different Provider secret already exists for this credential scope")]
    SecretAlreadyExists,
    #[error("the Provider secret is referenced by the active configuration")]
    SecretInUse,
    #[error("the secret reference is reserved for WokCore runtime authentication")]
    SecretProtected,
    #[error("the Provider secret store is read-only")]
    ReadOnlySecretStore,
    #[error("the Provider management storage operation failed")]
    StorageFailure,
}
