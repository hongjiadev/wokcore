pub const MAX_TOTAL_ATTEMPTS: u8 = 2;
const MAX_RETRY_DELAY_MS: u64 = 60_000;
const MAX_RETAINED_REQUEST_BODY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryClass {
    RateLimited,
    Temporary,
    Never,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    base_delay_ms: u64,
    maximum_delay_ms: u64,
    maximum_body_bytes: usize,
}

impl RetryPolicy {
    pub fn new(
        base_delay_ms: u64,
        maximum_delay_ms: u64,
        maximum_body_bytes: usize,
    ) -> Result<Self, RetryPolicyError> {
        if base_delay_ms == 0
            || maximum_delay_ms < base_delay_ms
            || maximum_delay_ms > MAX_RETRY_DELAY_MS
            || maximum_body_bytes == 0
            || maximum_body_bytes > MAX_RETAINED_REQUEST_BODY_BYTES
        {
            return Err(RetryPolicyError::InvalidPolicy);
        }
        Ok(Self {
            base_delay_ms,
            maximum_delay_ms,
            maximum_body_bytes,
        })
    }

    pub const fn maximum_body_bytes(self) -> usize {
        self.maximum_body_bytes
    }

    pub fn delay_ms(self, class: RetryClass, server_hint_ms: Option<u64>) -> Option<u64> {
        match class {
            RetryClass::Never => None,
            RetryClass::RateLimited => Some(
                self.base_delay_ms.max(
                    server_hint_ms
                        .unwrap_or_default()
                        .min(self.maximum_delay_ms),
                ),
            ),
            RetryClass::Temporary => Some(self.base_delay_ms),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RetryPolicyError {
    #[error("the retry policy is invalid")]
    InvalidPolicy,
}
