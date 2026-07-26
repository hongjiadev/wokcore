use std::time::{SystemTime, UNIX_EPOCH};

use uuid::Uuid;

pub trait TokenMetadataSource: Send + Sync {
    fn new_token_id(&self) -> Result<String, TokenMetadataError>;

    fn now(&self) -> Result<String, TokenMetadataError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemTokenMetadata;

impl TokenMetadataSource for SystemTokenMetadata {
    fn new_token_id(&self) -> Result<String, TokenMetadataError> {
        Ok(Uuid::new_v4().to_string())
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
