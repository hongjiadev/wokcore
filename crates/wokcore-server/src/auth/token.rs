use std::fmt;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

const ADMIN_PREFIX: &str = "wok_admin_v1_";
const PROXY_PREFIX: &str = "wok_proxy_v1_";
const ENTROPY_BYTES: usize = 32;
const ENCODED_ENTROPY_BYTES: usize = 43;

pub trait EntropySource: Send + Sync {
    fn fill(&self, output: &mut [u8; ENTROPY_BYTES]) -> Result<(), TokenError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct OsEntropy;

impl EntropySource for OsEntropy {
    fn fill(&self, output: &mut [u8; ENTROPY_BYTES]) -> Result<(), TokenError> {
        getrandom::fill(output).map_err(|_| TokenError::EntropyUnavailable)
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct TokenDigest([u8; 32]);

impl TokenDigest {
    pub fn of(candidate: &str) -> Self {
        Self(Sha256::digest(candidate.as_bytes()).into())
    }

    pub fn constant_time_matches(&self, candidate: &Self) -> bool {
        bool::from(self.0.ct_eq(&candidate.0))
    }

    pub fn into_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl From<[u8; 32]> for TokenDigest {
    fn from(value: [u8; 32]) -> Self {
        Self(value)
    }
}

impl fmt::Debug for TokenDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TokenDigest([redacted])")
    }
}

pub struct TokenMaterial {
    value: SecretString,
}

impl TokenMaterial {
    pub fn generate_admin(entropy: &dyn EntropySource) -> Result<Self, TokenError> {
        Self::generate(ADMIN_PREFIX, entropy)
    }

    pub fn generate_proxy(entropy: &dyn EntropySource) -> Result<Self, TokenError> {
        Self::generate(PROXY_PREFIX, entropy)
    }

    fn generate(prefix: &str, entropy: &dyn EntropySource) -> Result<Self, TokenError> {
        let mut bytes = Zeroizing::new([0_u8; ENTROPY_BYTES]);
        entropy.fill(&mut bytes)?;
        let mut value = Zeroizing::new(String::with_capacity(prefix.len() + ENCODED_ENTROPY_BYTES));
        value.push_str(prefix);
        URL_SAFE_NO_PAD.encode_string(bytes.as_slice(), &mut value);
        debug_assert_eq!(value.len(), prefix.len() + ENCODED_ENTROPY_BYTES);
        Ok(Self {
            value: SecretString::from(std::mem::take(&mut *value)),
        })
    }

    pub fn digest(&self) -> TokenDigest {
        TokenDigest::of(self.value.expose_secret())
    }

    pub fn into_response_value(self) -> SecretString {
        self.value
    }

    pub(crate) fn into_secret_value(self) -> SecretString {
        self.value
    }
}

impl fmt::Debug for TokenMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TokenMaterial([redacted])")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum TokenError {
    #[error("operating-system entropy is unavailable")]
    EntropyUnavailable,
}

pub(crate) fn is_admin_token(candidate: &str) -> bool {
    has_token_shape(candidate, ADMIN_PREFIX)
}

pub(crate) fn is_proxy_token(candidate: &str) -> bool {
    has_token_shape(candidate, PROXY_PREFIX)
}

fn has_token_shape(candidate: &str, prefix: &str) -> bool {
    let Some(encoded) = candidate.strip_prefix(prefix) else {
        return false;
    };
    encoded.len() == ENCODED_ENTROPY_BYTES
        && encoded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}
