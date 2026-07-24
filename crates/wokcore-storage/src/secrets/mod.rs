mod environment;
mod memory;
mod native;
mod permissioned_file;
mod store;

pub use environment::EnvironmentSecretStore;
pub use memory::MemorySecretStore;
pub use native::NativeSecretStore;
pub use permissioned_file::PermissionedFileSecretStore;
pub use store::{HeadlessSecretStoreConfig, SecretStore};

pub const MAX_HEADLESS_SECRET_BYTES: usize = 64 * 1024;
