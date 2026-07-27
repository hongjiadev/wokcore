use keyring::{Entry, Error as KeyringError};
use secrecy::{ExposeSecret, SecretString};
use subtle::ConstantTimeEq;
use wokcore_core::secret::{SecretRef, SecretScope};
use zeroize::Zeroize;

use crate::{SecretStore, StorageError};

const NATIVE_SERVICE_NAME: &str = "dev.wokcore.credentials";

#[derive(Clone, Copy, Debug, Default)]
pub struct NativeSecretStore;

impl NativeSecretStore {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl SecretStore for NativeSecretStore {
    async fn put(
        &self,
        _scope: &SecretScope,
        value: SecretString,
    ) -> Result<SecretRef, StorageError> {
        let secret_ref = SecretRef::new();
        let account = secret_ref.as_str().to_owned();
        tokio::task::spawn_blocking(move || {
            let entry = Entry::new(NATIVE_SERVICE_NAME, &account).map_err(map_keyring_error)?;
            entry
                .set_password(value.expose_secret())
                .map_err(map_keyring_error)
        })
        .await
        .map_err(|_| map_join_failure())??;
        Ok(secret_ref)
    }

    async fn get(&self, secret_ref: &SecretRef) -> Result<SecretString, StorageError> {
        let account = secret_ref.as_str().to_owned();
        let value = tokio::task::spawn_blocking(move || {
            let entry = Entry::new(NATIVE_SERVICE_NAME, &account).map_err(map_keyring_error)?;
            entry.get_password().map_err(map_keyring_error)
        })
        .await
        .map_err(|_| map_join_failure())??;
        Ok(SecretString::from(value))
    }

    async fn put_scoped(
        &self,
        scope: &SecretScope,
        value: SecretString,
    ) -> Result<SecretRef, StorageError> {
        let secret_ref = SecretRef::for_scope(scope);
        let account = secret_ref.as_str().to_owned();
        tokio::task::spawn_blocking(move || {
            let entry = Entry::new(NATIVE_SERVICE_NAME, &account).map_err(map_keyring_error)?;
            match entry.get_password() {
                Ok(mut previous) => {
                    let same =
                        bool::from(previous.as_bytes().ct_eq(value.expose_secret().as_bytes()));
                    previous.zeroize();
                    if same {
                        Ok(())
                    } else {
                        Err(StorageError::SecretAlreadyExists)
                    }
                }
                Err(KeyringError::NoEntry) => entry
                    .set_password(value.expose_secret())
                    .map_err(map_keyring_error),
                Err(error) => Err(map_keyring_error(error)),
            }
        })
        .await
        .map_err(|_| map_join_failure())??;
        Ok(secret_ref)
    }

    async fn replace(
        &self,
        secret_ref: &SecretRef,
        value: SecretString,
    ) -> Result<(), StorageError> {
        let account = secret_ref.as_str().to_owned();
        tokio::task::spawn_blocking(move || {
            let entry = Entry::new(NATIVE_SERVICE_NAME, &account).map_err(map_keyring_error)?;
            let mut previous = entry.get_password().map_err(map_keyring_error)?;
            previous.zeroize();
            entry
                .set_password(value.expose_secret())
                .map_err(map_keyring_error)
        })
        .await
        .map_err(|_| map_join_failure())?
    }

    async fn delete(&self, secret_ref: &SecretRef) -> Result<(), StorageError> {
        let account = secret_ref.as_str().to_owned();
        tokio::task::spawn_blocking(move || {
            let entry = Entry::new(NATIVE_SERVICE_NAME, &account).map_err(map_keyring_error)?;
            match entry.delete_credential() {
                Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
                Err(error) => Err(map_keyring_error(error)),
            }
        })
        .await
        .map_err(|_| map_join_failure())?
    }
}

fn map_join_failure() -> StorageError {
    StorageError::SecretBackendFailure
}

fn map_keyring_error(error: KeyringError) -> StorageError {
    match error {
        KeyringError::NoEntry => StorageError::SecretNotFound,
        KeyringError::BadEncoding(mut bytes) => {
            bytes.zeroize();
            StorageError::SecretBackendFailure
        }
        KeyringError::BadDataFormat(mut bytes, _) => {
            bytes.zeroize();
            StorageError::SecretBackendFailure
        }
        _ => StorageError::SecretBackendFailure,
    }
}

#[cfg(test)]
mod tests {
    use super::{NATIVE_SERVICE_NAME, map_join_failure, map_keyring_error};
    use crate::StorageError;

    #[derive(Debug, thiserror::Error)]
    #[error("backend diagnostic: {0}")]
    struct BackendDiagnostic(String);

    #[test]
    fn native_service_name_is_exactly_wokcore_credentials() {
        assert_eq!(NATIVE_SERVICE_NAME, "dev.wokcore.credentials");
    }

    #[test]
    fn native_backend_diagnostics_and_secret_bytes_map_to_a_generic_error() {
        let diagnostic = ["private", "backend", "diagnostic"].join("-");
        let secret = ["native", "secret", "bytes"].join("-").into_bytes();
        let errors = [
            map_keyring_error(keyring::Error::PlatformFailure(Box::new(
                BackendDiagnostic(diagnostic.clone()),
            ))),
            map_keyring_error(keyring::Error::BadEncoding(secret)),
            map_join_failure(),
        ];

        for error in errors {
            assert!(matches!(error, StorageError::SecretBackendFailure));
            let rendered = format!("{error:?} {error}");
            assert!(!rendered.contains(&diagnostic));
            assert!(!rendered.contains("native-secret-bytes"));
        }
    }

    #[test]
    fn bad_data_format_bytes_and_diagnostic_map_to_a_generic_error() {
        let bytes_canary = ["bad", "data", "secret"].join("-");
        let diagnostic_canary = ["bad", "data", "diagnostic"].join("-");

        let error = map_keyring_error(keyring::Error::BadDataFormat(
            bytes_canary.clone().into_bytes(),
            Box::new(BackendDiagnostic(diagnostic_canary.clone())),
        ));

        assert!(matches!(error, StorageError::SecretBackendFailure));
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains(&bytes_canary));
        assert!(!rendered.contains(&diagnostic_canary));
    }

    #[test]
    fn native_no_entry_retains_not_found_semantics_without_a_backend_message() {
        let error = map_keyring_error(keyring::Error::NoEntry);

        assert!(matches!(error, StorageError::SecretNotFound));
    }
}
