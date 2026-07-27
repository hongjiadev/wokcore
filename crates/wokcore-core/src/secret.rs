use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use crate::id::{AccountId, ProviderId};

const SECRET_REF_PREFIX: &str = "secret:";

#[derive(Clone, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SecretRef(String);

impl SecretRef {
    pub fn new() -> Self {
        Self(format!("{SECRET_REF_PREFIX}{}", uuid::Uuid::new_v4()))
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidSecretRef> {
        let value = value.into();
        let identifier = value
            .strip_prefix(SECRET_REF_PREFIX)
            .ok_or(InvalidSecretRef)?;
        uuid::Uuid::parse_str(identifier).map_err(|_| InvalidSecretRef)?;
        Ok(Self(value))
    }

    pub fn for_scope(scope: &SecretScope) -> Self {
        let mut digest = Sha256::new();
        digest.update(b"wokcore.provider-secret-scope.v1\0");
        digest.update(scope.provider_id.as_str().as_bytes());
        digest.update([0]);
        if let Some(account_id) = &scope.account_id {
            digest.update([1]);
            digest.update(account_id.as_str().as_bytes());
        } else {
            digest.update([0]);
        }
        digest.update([0, secret_purpose_code(scope.purpose)]);
        let digest = digest.finalize();
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x80;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Self(format!(
            "{SECRET_REF_PREFIX}{}",
            uuid::Uuid::from_bytes(bytes)
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for SecretRef {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SecretRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretRef([redacted])")
    }
}

impl<'de> Deserialize<'de> for SecretRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("secret reference is not a valid opaque identifier")]
pub struct InvalidSecretRef;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretPurpose {
    ApiKey,
    OAuthAccess,
    OAuthRefresh,
    LanToken,
    Auxiliary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretScope {
    pub provider_id: ProviderId,
    pub account_id: Option<AccountId>,
    pub purpose: SecretPurpose,
}

const fn secret_purpose_code(purpose: SecretPurpose) -> u8 {
    match purpose {
        SecretPurpose::ApiKey => 1,
        SecretPurpose::OAuthAccess => 2,
        SecretPurpose::OAuthRefresh => 3,
        SecretPurpose::LanToken => 4,
        SecretPurpose::Auxiliary => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::{SecretPurpose, SecretRef, SecretScope};
    use crate::id::{AccountId, ProviderId};

    #[test]
    fn scoped_reference_is_stable_and_separates_credential_slots() {
        let scope = SecretScope {
            provider_id: ProviderId::new("primary").unwrap(),
            account_id: Some(AccountId::new("work").unwrap()),
            purpose: SecretPurpose::ApiKey,
        };
        assert_eq!(SecretRef::for_scope(&scope), SecretRef::for_scope(&scope));

        let other = SecretScope {
            purpose: SecretPurpose::OAuthAccess,
            ..scope.clone()
        };
        assert_ne!(SecretRef::for_scope(&scope), SecretRef::for_scope(&other));
        SecretRef::parse(SecretRef::for_scope(&scope).as_str()).unwrap();
    }
}
