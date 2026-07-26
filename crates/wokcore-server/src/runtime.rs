use std::{
    fmt,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use uuid::Uuid;

use crate::auth::{EntropySource, OsEntropy, TokenError};

pub trait TokenMetadataSource: Send + Sync {
    fn new_token_id(&self) -> Result<String, TokenMetadataError>;

    fn now(&self) -> Result<String, TokenMetadataError>;
}

#[derive(Clone)]
pub struct SystemTokenMetadata {
    entropy: Arc<dyn EntropySource>,
}

impl SystemTokenMetadata {
    pub fn new(entropy: Arc<dyn EntropySource>) -> Self {
        Self { entropy }
    }
}

impl Default for SystemTokenMetadata {
    fn default() -> Self {
        Self::new(Arc::new(OsEntropy))
    }
}

impl fmt::Debug for SystemTokenMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SystemTokenMetadata")
            .finish_non_exhaustive()
    }
}

impl TokenMetadataSource for SystemTokenMetadata {
    fn new_token_id(&self) -> Result<String, TokenMetadataError> {
        generate_uuid_v4(self.entropy.as_ref())
            .map(|uuid| uuid.to_string())
            .map_err(|_| TokenMetadataError)
    }

    fn now(&self) -> Result<String, TokenMetadataError> {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| TokenMetadataError)?
            .as_secs();
        Ok(seconds.to_string())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("runtime token metadata is unavailable")]
pub struct TokenMetadataError;

pub fn generate_uuid_v4(entropy: &dyn EntropySource) -> Result<Uuid, TokenError> {
    let mut entropy_bytes = [0_u8; 32];
    entropy.fill(&mut entropy_bytes)?;
    let mut uuid_bytes = [0_u8; 16];
    uuid_bytes.copy_from_slice(&entropy_bytes[..16]);
    uuid_bytes[6] = (uuid_bytes[6] & 0x0f) | 0x40;
    uuid_bytes[8] = (uuid_bytes[8] & 0x3f) | 0x80;
    Ok(Uuid::from_bytes(uuid_bytes))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::auth::{EntropySource, TokenError};

    use super::{SystemTokenMetadata, TokenMetadataSource};

    struct FailingEntropy;

    impl EntropySource for FailingEntropy {
        fn fill(&self, _output: &mut [u8; 32]) -> Result<(), TokenError> {
            Err(TokenError::EntropyUnavailable)
        }
    }

    #[test]
    fn system_token_metadata_maps_entropy_failure_without_panicking() {
        let metadata = SystemTokenMetadata::new(Arc::new(FailingEntropy));

        assert!(metadata.new_token_id().is_err());
    }
}
