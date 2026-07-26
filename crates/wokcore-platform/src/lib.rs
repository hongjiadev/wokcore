//! Platform-specific WokCore path discovery.

pub mod runtime;
pub mod system;

pub use runtime::{DiscoveryRecord, DiscoveryStore, MAX_DISCOVERY_BYTES, RuntimeLease};
pub use system::paths::{AppPaths, EnvironmentSnapshot, Platform};
pub use system::process::is_process_running;

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("missing platform data: {name}")]
    MissingPlatformData { name: &'static str },
    #[error("another WokCore instance owns the runtime")]
    AlreadyRunning,
    #[error("runtime path is not a secure current-user filesystem object")]
    UnsafeRuntimePath,
    #[error("discovery document is invalid")]
    InvalidDiscovery,
    #[error("discovery document exceeds the maximum size")]
    DiscoveryTooLarge,
    #[error("platform I/O failed")]
    Io {
        #[from]
        source: std::io::Error,
    },
}
