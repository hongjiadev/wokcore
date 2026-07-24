use serde::{Deserialize, Deserializer, Serialize};

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
