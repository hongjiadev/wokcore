use serde::{Deserialize, Deserializer, Serialize};

const MAX_IDENTIFIER_LENGTH: usize = 128;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ProviderId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct AccountId(String);

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error(
    "identifier must contain an ASCII letter or digit and use only lowercase ASCII letters, digits, '.', '_' or '-'"
)]
pub struct InvalidId;

macro_rules! identifier {
    ($type:ty) => {
        impl $type {
            pub fn new(value: impl Into<String>) -> Result<Self, InvalidId> {
                let value = value.into();
                if !is_valid_identifier(&value) {
                    return Err(InvalidId);
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }

        impl AsRef<str> for $type {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }
    };
}

identifier!(ProviderId);
identifier!(AccountId);

fn is_valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_LENGTH
        && value != "."
        && value != ".."
        && value.bytes().any(|byte| byte.is_ascii_alphanumeric())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::{AccountId, ProviderId};

    #[test]
    fn identifiers_reject_path_segments_and_separator_only_values() {
        for invalid in [".", "..", "-", "_", "-_.", "...---___"] {
            assert!(ProviderId::new(invalid).is_err());
            assert!(AccountId::new(invalid).is_err());
        }

        assert!(ProviderId::new("openai-compatible.v1").is_ok());
        assert!(AccountId::new("primary-account_01").is_ok());
    }
}
