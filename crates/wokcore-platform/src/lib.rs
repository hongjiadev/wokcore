//! Platform-specific WokCore path discovery.

pub mod system;

pub use system::paths::{AppPaths, EnvironmentSnapshot, Platform};

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("missing platform data: {name}")]
    MissingPlatformData { name: &'static str },
}
