use std::{
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};
use wokcore_platform::sessions::{SessionFile, SessionFileIdentity as PlatformFileIdentity};
use wokcore_storage::{SessionSourceErrorCode, SessionSourceStatus};

use crate::{codex::parse_timestamp_utc, cursor::JsonlError};

pub(crate) const SESSION_BATCH_ROW_TARGET: usize = 384;
pub(crate) const EXTERNAL_ID_LIMIT_BYTES: usize = 512;
pub(crate) const MODEL_LIMIT_BYTES: usize = 256;
pub(crate) const TITLE_LIMIT_BYTES: usize = 512;
pub(crate) const MAX_ACTIVE_MESSAGES: usize = 16_384;
pub(crate) const FINGERPRINT_WINDOW_BYTES: usize = 4 * 1024;
pub(crate) const HEAD_FINGERPRINT_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TokenTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

impl TokenTotals {
    pub fn clamp_cache(mut self) -> Self {
        self.cache_read = self.cache_read.min(self.input);
        self.cache_write = self.cache_write.min(self.input);
        self
    }

    pub fn apply_cumulative(&mut self, current: Self) -> Option<Self> {
        let current = current.clamp_cache();
        let delta = Self {
            input: current.input.saturating_sub(self.input),
            output: current.output.saturating_sub(self.output),
            cache_read: current.cache_read.saturating_sub(self.cache_read),
            cache_write: current.cache_write.saturating_sub(self.cache_write),
            reasoning: current.reasoning.saturating_sub(self.reasoning),
        };
        *self = current;
        (!delta.is_zero()).then_some(delta)
    }

    pub fn add_last(&mut self, last: Self) -> Option<Self> {
        let last = last.clamp_cache();
        self.input = self.input.saturating_add(last.input);
        self.output = self.output.saturating_add(last.output);
        self.cache_read = self.cache_read.saturating_add(last.cache_read);
        self.cache_write = self.cache_write.saturating_add(last.cache_write);
        self.reasoning = self.reasoning.saturating_add(last.reasoning);
        (!last.is_zero()).then_some(last)
    }

    pub fn is_zero(self) -> bool {
        self.input == 0
            && self.output == 0
            && self.cache_read == 0
            && self.cache_write == 0
            && self.reasoning == 0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplayResolution {
    NotForked,
    Resolved { replayed_events: u64 },
    Deferred(SessionSourceErrorCode),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SessionScanControl {
    pub stop_after_committed_batches: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionScanOutcome {
    Complete,
    Interrupted,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionScannerMetrics {
    pub source_opens: u64,
    pub parser_read_bytes: u64,
    pub peak_parser_buffer_bytes: usize,
    pub aggregate_message_inspections: u64,
    pub full_source_scans: u64,
    pub committed_batches: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSourceScanSummary {
    pub source_key: String,
    pub session_key: Option<String>,
    pub status: SessionSourceStatus,
    pub error_code: Option<SessionSourceErrorCode>,
    pub complete_byte_offset: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionScanSummary {
    pub outcome: SessionScanOutcome,
    pub advanced_sources: usize,
    pub unchanged_sources: usize,
    pub deleted_sources: usize,
    pub sources: Vec<SessionSourceScanSummary>,
    pub metrics: SessionScannerMetrics,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ExternalSessionTitle(String);

impl ExternalSessionTitle {
    pub(crate) fn new(value: String) -> Option<Self> {
        let trimmed = value.trim();
        if !valid_title(trimmed) {
            return None;
        }
        if trimmed.len() == value.len() {
            Some(Self(value))
        } else {
            Some(Self(trimmed.to_owned()))
        }
    }

    pub(crate) fn from_str(value: &str) -> Option<Self> {
        let value = value.trim();
        if !valid_title(value) {
            None
        } else {
            Some(Self(value.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn valid_title(value: &str) -> bool {
    !value.is_empty() && value.len() <= TITLE_LIMIT_BYTES && !value.chars().any(char::is_control)
}

impl fmt::Debug for ExternalSessionTitle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ExternalSessionTitle(<redacted>)")
    }
}

pub(crate) fn normalize_external_id(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > EXTERNAL_ID_LIMIT_BYTES
        || value.chars().any(char::is_control)
    {
        None
    } else {
        Some(value.to_owned())
    }
}

pub(crate) fn normalize_external_model(value: Option<&str>) -> String {
    let Some(value) = value else {
        return "unknown".to_owned();
    };
    let normalized = value.trim().to_lowercase();
    let model = normalized
        .rsplit_once('/')
        .map_or(normalized.as_str(), |(_, model)| model);
    if model.is_empty() || model.len() > MODEL_LIMIT_BYTES || model.chars().any(char::is_control) {
        "unknown".to_owned()
    } else {
        model.to_owned()
    }
}

pub(crate) fn normalize_timestamp(value: &serde_json::Value) -> Option<String> {
    parse_timestamp_utc(value).ok()
}

pub(crate) fn maximum_timestamp(current: Option<String>, candidate: &str) -> Option<String> {
    match current {
        Some(current) if current.as_str() >= candidate => Some(current),
        _ => Some(candidate.to_owned()),
    }
}

pub(crate) fn system_time_utc(value: Option<SystemTime>) -> String {
    let seconds = value
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_secs().min(i64::MAX as u64) as i64);
    parse_timestamp_utc(&serde_json::Value::from(seconds))
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

pub(crate) fn opaque_hex(key: &[u8; 32], domain: &[u8], fields: &[&[u8]]) -> String {
    hex(&opaque_hash(key, domain, fields))
}

pub(crate) fn opaque_hash(key: &[u8; 32], domain: &[u8], fields: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update((domain.len() as u64).to_be_bytes());
    hasher.update(domain);
    hasher.update(key);
    for field in fields {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    hasher.finalize().into()
}

pub(crate) struct OpaqueStreamHash {
    inner: Sha256,
    outer_pad: [u8; 64],
    raw_bytes: u64,
}

impl OpaqueStreamHash {
    pub(crate) fn new(key: &[u8; 32], domain: &[u8], fixed_fields: &[&[u8]]) -> Self {
        let mut inner_pad = [0x36; 64];
        let mut outer_pad = [0x5c; 64];
        for (index, byte) in key.iter().copied().enumerate() {
            inner_pad[index] ^= byte;
            outer_pad[index] ^= byte;
        }
        let mut inner = Sha256::new();
        inner.update(inner_pad);
        update_framed(&mut inner, 1, domain);
        for field in fixed_fields {
            update_framed(&mut inner, 2, field);
        }
        inner.update([3]);
        Self {
            inner,
            outer_pad,
            raw_bytes: 0,
        }
    }

    pub(crate) fn update(&mut self, bytes: &[u8]) -> bool {
        let Ok(length) = u64::try_from(bytes.len()) else {
            return false;
        };
        let Some(raw_bytes) = self.raw_bytes.checked_add(length) else {
            return false;
        };
        self.inner.update(bytes);
        self.raw_bytes = raw_bytes;
        true
    }

    pub(crate) fn finalize(mut self, promoted_extent: u64) -> Option<[u8; 32]> {
        if self.raw_bytes != promoted_extent {
            return None;
        }
        self.inner.update([4]);
        self.inner.update(promoted_extent.to_be_bytes());
        self.inner.update([5]);
        self.inner.update(self.raw_bytes.to_be_bytes());
        let inner = self.inner.finalize();
        let mut outer = Sha256::new();
        outer.update(self.outer_pad);
        outer.update(inner);
        Some(outer.finalize().into())
    }
}

fn update_framed(hasher: &mut Sha256, tag: u8, bytes: &[u8]) {
    hasher.update([tag]);
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

pub(crate) fn opaque_platform_identity(
    key: &[u8; 32],
    domain: &[u8],
    identity: PlatformFileIdentity,
) -> String {
    let bytes = platform_identity_bytes(identity);
    opaque_hex(key, domain, &[&bytes])
}

fn platform_identity_bytes(identity: PlatformFileIdentity) -> Vec<u8> {
    match identity {
        #[cfg(unix)]
        PlatformFileIdentity::Unix { device, inode } => {
            [device.to_be_bytes(), inode.to_be_bytes()].concat()
        }
        #[cfg(windows)]
        PlatformFileIdentity::Windows {
            volume_serial,
            file_index,
        } => [volume_serial.to_be_bytes(), file_index.to_be_bytes()].concat(),
    }
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

pub(crate) fn fingerprints(
    file: &mut SessionFile,
    boundary_offset: u64,
    key: &[u8; 32],
    domain: &[u8],
) -> Result<([u8; 32], [u8; 32]), JsonlError> {
    fingerprints_with_extent(file, boundary_offset, file.snapshot().size, key, domain)
}

pub(crate) fn fingerprints_with_extent(
    file: &mut SessionFile,
    boundary_offset: u64,
    observed_size: u64,
    key: &[u8; 32],
    domain: &[u8],
) -> Result<([u8; 32], [u8; 32]), JsonlError> {
    let mut head = file.read_range_bounded(0, HEAD_FINGERPRINT_BYTES)?;
    head.resize(HEAD_FINGERPRINT_BYTES, 0);
    let half_window = (FINGERPRINT_WINDOW_BYTES / 2) as u64;
    let start = boundary_offset.saturating_sub(half_window);
    let end = boundary_offset
        .saturating_add(half_window)
        .min(observed_size)
        .min(file.snapshot().size);
    let length = usize::try_from(end.saturating_sub(start)).unwrap_or(FINGERPRINT_WINDOW_BYTES);
    let boundary = file.read_range_bounded(start, length)?;
    let mut head_domain = Vec::with_capacity(domain.len() + 5);
    head_domain.extend_from_slice(domain);
    head_domain.extend_from_slice(b".head");
    let mut boundary_domain = Vec::with_capacity(domain.len() + 9);
    boundary_domain.extend_from_slice(domain);
    boundary_domain.extend_from_slice(b".boundary");
    Ok((
        opaque_hash(key, &head_domain, &[&head]),
        opaque_hash(
            key,
            &boundary_domain,
            &[&boundary_offset.to_be_bytes(), &boundary],
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::OpaqueStreamHash;

    fn digest(key: [u8; 32], domain: &[u8], format: &[u8], raw: &[u8]) -> [u8; 32] {
        let mut hash = OpaqueStreamHash::new(&key, domain, &[format]);
        assert!(hash.update(raw));
        hash.finalize(raw.len() as u64).unwrap()
    }

    #[test]
    fn stream_hash_binds_key_domain_format_and_exact_extent() {
        let raw = b"same raw bytes";
        let baseline = digest([1; 32], b"domain-a", b"current", raw);
        assert_ne!(baseline, digest([2; 32], b"domain-a", b"current", raw));
        assert_ne!(baseline, digest([1; 32], b"domain-b", b"current", raw));
        assert_ne!(baseline, digest([1; 32], b"domain-a", b"legacy", raw));

        let mut short = OpaqueStreamHash::new(&[1; 32], b"domain-a", &[b"current"]);
        assert!(short.update(raw));
        assert!(short.finalize(raw.len() as u64 + 1).is_none());

        let mut long = OpaqueStreamHash::new(&[1; 32], b"domain-a", &[b"current"]);
        assert!(long.update(raw));
        assert!(long.finalize(raw.len() as u64 - 1).is_none());
    }
}
