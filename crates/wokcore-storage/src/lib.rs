//! Durable configuration storage for WokCore.

pub mod config;

pub use config::{AppConfig, ConfigStore, ServerConfig, VersionedConfig};

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
}
