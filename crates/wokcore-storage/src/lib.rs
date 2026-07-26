//! Durable configuration, secret, and state storage for WokCore.

pub mod config;
pub mod secrets;
pub mod state;

pub use config::{AppConfig, ConfigStore, ServerConfig, VersionedConfig};
pub use secrets::{
    EnvironmentSecretStore, HeadlessSecretStoreConfig, MAX_HEADLESS_SECRET_BYTES,
    MemorySecretStore, NativeSecretStore, PermissionedFileSecretStore, SecretStore,
};
pub use state::{
    CheckpointResult, ClientTokenMetadata, ReadOnlyStateStore, RequestMetric, RuntimeSecretBinding,
    StateHealth, StateStore, WAL_CHECKPOINT_THRESHOLD_BYTES,
};

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("configuration revision conflict: expected {expected}, found {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("invalid configuration: {message}")]
    InvalidConfig { message: String },
    #[error("failed to serialize configuration: {message}")]
    SerializeConfig { message: String },
    #[error("storage I/O error: {source}")]
    Io {
        #[source]
        source: std::io::Error,
    },
    #[error("state database is corrupt: {message}")]
    StateDatabaseCorrupt { message: String },
    #[error("state database error: {source}")]
    StateDatabase {
        #[source]
        source: rusqlite::Error,
    },
    #[error("runtime secret binding already exists at revision {actual}")]
    RuntimeSecretBindingConflict { actual: u64 },
    #[error("secret was not found")]
    SecretNotFound,
    #[error("the secret backend failed without exposing secret material")]
    SecretBackendFailure,
    #[error("the selected secret backend is read-only")]
    ReadOnlySecretStore,
    #[error("the explicit headless secret backend configuration does not match this store")]
    InvalidHeadlessSecretStoreConfig,
    #[error("the secret file grants access beyond the current user")]
    InsecureSecretFilePermissions,
    #[error("secret material is not valid UTF-8")]
    InvalidSecretEncoding,
    #[error("secret material exceeds the 64 KiB headless limit")]
    SecretTooLarge,
}
