use std::path::PathBuf;

use secrecy::SecretString;
use wokcore_core::secret::{SecretRef, SecretScope};

use crate::StorageError;

#[async_trait::async_trait]
pub trait SecretStore: Send + Sync {
    async fn put(
        &self,
        scope: &SecretScope,
        value: SecretString,
    ) -> Result<SecretRef, StorageError>;

    async fn get(&self, secret_ref: &SecretRef) -> Result<SecretString, StorageError>;

    async fn delete(&self, secret_ref: &SecretRef) -> Result<(), StorageError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HeadlessSecretStoreConfig {
    Environment {
        secret_ref: SecretRef,
        variable_name: String,
    },
    PermissionedFile {
        secret_ref: SecretRef,
        path: PathBuf,
    },
}
