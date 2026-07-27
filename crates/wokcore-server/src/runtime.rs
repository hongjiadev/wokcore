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
        utc_timestamp_from_epoch_seconds(seconds).ok_or(TokenMetadataError)
    }
}

pub fn utc_timestamp_from_epoch_seconds(seconds: u64) -> Option<String> {
    let seconds = i64::try_from(seconds).ok()?;
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    if !(1..=9999).contains(&year) {
        return None;
    }
    let hour = second_of_day / 3_600;
    let minute = second_of_day % 3_600 / 60;
    let second = second_of_day % 60;
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
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

    use super::{SystemTokenMetadata, TokenMetadataSource, utc_timestamp_from_epoch_seconds};

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

    #[test]
    fn epoch_seconds_are_rendered_as_canonical_utc() {
        assert_eq!(
            utc_timestamp_from_epoch_seconds(0).as_deref(),
            Some("1970-01-01T00:00:00Z")
        );
        assert_eq!(
            utc_timestamp_from_epoch_seconds(951_782_400).as_deref(),
            Some("2000-02-29T00:00:00Z")
        );
        assert_eq!(
            utc_timestamp_from_epoch_seconds(1_785_155_696).as_deref(),
            Some("2026-07-27T12:34:56Z")
        );
        assert_eq!(utc_timestamp_from_epoch_seconds(u64::MAX), None);
    }
}
