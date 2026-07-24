use std::ffi::OsString;

use secrecy::SecretString;
use wokcore_core::secret::{SecretRef, SecretScope};
use zeroize::Zeroize;

use crate::{HeadlessSecretStoreConfig, MAX_HEADLESS_SECRET_BYTES, SecretStore, StorageError};

#[derive(Clone, Debug)]
pub struct EnvironmentSecretStore {
    secret_ref: SecretRef,
    variable_name: String,
}

impl EnvironmentSecretStore {
    pub fn from_config(config: HeadlessSecretStoreConfig) -> Result<Self, StorageError> {
        let HeadlessSecretStoreConfig::Environment {
            secret_ref,
            variable_name,
        } = config
        else {
            return Err(StorageError::InvalidHeadlessSecretStoreConfig);
        };
        if variable_name.is_empty() || variable_name.contains('=') || variable_name.contains('\0') {
            return Err(StorageError::InvalidHeadlessSecretStoreConfig);
        }
        Ok(Self {
            secret_ref,
            variable_name,
        })
    }
}

#[async_trait::async_trait]
impl SecretStore for EnvironmentSecretStore {
    async fn put(
        &self,
        _scope: &SecretScope,
        _value: SecretString,
    ) -> Result<SecretRef, StorageError> {
        Err(StorageError::ReadOnlySecretStore)
    }

    async fn get(&self, secret_ref: &SecretRef) -> Result<SecretString, StorageError> {
        if secret_ref != &self.secret_ref {
            return Err(StorageError::SecretNotFound);
        }
        let value = std::env::var_os(&self.variable_name).ok_or(StorageError::SecretNotFound)?;
        secret_from_os_string(value)
    }

    async fn delete(&self, secret_ref: &SecretRef) -> Result<(), StorageError> {
        if secret_ref != &self.secret_ref {
            return Err(StorageError::SecretNotFound);
        }
        Err(StorageError::ReadOnlySecretStore)
    }
}

fn secret_from_os_string(value: OsString) -> Result<SecretString, StorageError> {
    if value.as_encoded_bytes().len() > MAX_HEADLESS_SECRET_BYTES {
        let mut bytes = value.into_encoded_bytes();
        bytes.zeroize();
        return Err(StorageError::SecretTooLarge);
    }
    match value.into_string() {
        Ok(value) => Ok(SecretString::from(value)),
        Err(value) => {
            let mut bytes = value.into_encoded_bytes();
            bytes.zeroize();
            Err(StorageError::InvalidSecretEncoding)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use secrecy::ExposeSecret;

    use super::secret_from_os_string;
    use crate::{MAX_HEADLESS_SECRET_BYTES, StorageError};

    #[test]
    fn environment_value_accepts_the_exact_limit_and_rejects_one_more_byte() {
        let accepted =
            secret_from_os_string(OsString::from("x".repeat(MAX_HEADLESS_SECRET_BYTES))).unwrap();
        let rejected = OsString::from("x".repeat(MAX_HEADLESS_SECRET_BYTES + 1));

        assert_eq!(accepted.expose_secret().len(), MAX_HEADLESS_SECRET_BYTES);
        assert!(matches!(
            secret_from_os_string(rejected),
            Err(StorageError::SecretTooLarge)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_environment_value_is_rejected_without_formatting_its_bytes() {
        use std::os::unix::ffi::OsStringExt;

        let error = secret_from_os_string(OsString::from_vec(vec![0xff])).unwrap_err();

        assert!(matches!(error, StorageError::InvalidSecretEncoding));
        assert!(!format!("{error:?} {error}").contains("255"));
    }

    #[cfg(windows)]
    #[test]
    fn non_unicode_environment_value_is_rejected_without_formatting_its_bytes() {
        use std::os::windows::ffi::OsStringExt;

        let error = secret_from_os_string(OsString::from_wide(&[0xd800])).unwrap_err();

        assert!(matches!(error, StorageError::InvalidSecretEncoding));
        assert!(!format!("{error:?} {error}").contains("55296"));
    }
}
