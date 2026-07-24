use std::{collections::HashMap, fmt, sync::RwLock};

use secrecy::{ExposeSecret, SecretString};
use wokcore_core::secret::{SecretRef, SecretScope};

use crate::{SecretStore, StorageError};

#[derive(Default)]
pub struct MemorySecretStore {
    secrets: RwLock<HashMap<SecretRef, SecretString>>,
}

impl fmt::Debug for MemorySecretStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemorySecretStore")
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl SecretStore for MemorySecretStore {
    async fn put(
        &self,
        _scope: &SecretScope,
        value: SecretString,
    ) -> Result<SecretRef, StorageError> {
        let secret_ref = SecretRef::new();
        self.secrets
            .write()
            .map_err(|_| StorageError::SecretBackendFailure)?
            .insert(secret_ref.clone(), value);
        Ok(secret_ref)
    }

    async fn get(&self, secret_ref: &SecretRef) -> Result<SecretString, StorageError> {
        let secrets = self
            .secrets
            .read()
            .map_err(|_| StorageError::SecretBackendFailure)?;
        let value = secrets
            .get(secret_ref)
            .ok_or(StorageError::SecretNotFound)?;
        Ok(SecretString::from(value.expose_secret().to_owned()))
    }

    async fn delete(&self, secret_ref: &SecretRef) -> Result<(), StorageError> {
        self.secrets
            .write()
            .map_err(|_| StorageError::SecretBackendFailure)?
            .remove(secret_ref);
        Ok(())
    }
}
