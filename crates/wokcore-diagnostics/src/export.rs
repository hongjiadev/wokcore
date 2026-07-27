use std::{
    cell::Cell,
    cmp::Reverse,
    collections::BinaryHeap,
    fmt,
    fs::File,
    io::{Read, Seek, Write},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
};

use serde::{
    Deserialize, Serialize,
    de::{DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use wokcore_platform::{diagnostics::DiagnosticReadLease, sessions::PinnedExportDestination};

use crate::event::decode_trusted_prepared_encoding;

const EXPORT_BUFFER_BYTES: usize = 16 * 1024;
const MAX_EXPORT_EVENT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_EXPORT_SOURCE_SCAN_BYTES: u64 = 96 * 1024 * 1024;
const MAX_LEAK_CANARIES: usize = 16;
const MAX_LEAK_CANARY_BYTES: usize = 256;
const MAX_EXPORT_CAPABILITIES: usize = 32;
const MAX_EXPORT_ERROR_SUMMARIES: usize = 16;
const MAX_EXPORT_ERROR_CHAIN: usize = 8;
// One event validation can retain an escaped-string decode, the typed event heap, and canonical
// reserialization. A duplicate merge can additionally retain up to three line-sized buffers.
const EVENT_VALIDATION_ALLOCATION_MULTIPLIER: usize = 5;
const EVENT_MERGE_ALLOCATION_MULTIPLIER: usize = EVENT_VALIDATION_ALLOCATION_MULTIPLIER + 3;
const ZIP_LOCAL_SIGNATURE: u32 = 0x0403_4b50;
const ZIP_CENTRAL_SIGNATURE: u32 = 0x0201_4b50;
const ZIP_END_SIGNATURE: u32 = 0x0605_4b50;
const ZIP_ENTRY_COUNT: u16 = 5;
const ZIP_DOS_DATE_1980_01_01: u16 = 0x0021;
const MANIFEST_NAME: &[u8] = b"manifest.json";
const EVENTS_NAME: &[u8] = b"events.jsonl";
const CONFIGURATION_NAME: &[u8] = b"configuration.json";
const RESOURCES_NAME: &[u8] = b"resources.json";
const CHECKSUMS_NAME: &[u8] = b"checksums.sha256";
const ENTRY_NAMES: [&[u8]; 5] = [
    MANIFEST_NAME,
    EVENTS_NAME,
    CONFIGURATION_NAME,
    RESOURCES_NAME,
    CHECKSUMS_NAME,
];

#[cfg(test)]
thread_local! {
    static PERSISTENT_READ_RANGE_CALLS: Cell<usize> = const { Cell::new(0) };
}

const FIXED_FORBIDDEN_KEYS: [&[u8]; 18] = [
    b"authorization",
    b"proxy_authorization",
    b"cookie",
    b"set_cookie",
    b"api_key",
    b"access_token",
    b"refresh_token",
    b"id_token",
    b"password",
    b"secret",
    b"credential",
    b"body",
    b"headers",
    b"path",
    b"prompt",
    b"tool",
    b"sse",
    b"content",
];

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ExportError {
    #[error("diagnostic export is busy")]
    Busy,
    #[error("diagnostic export was cancelled")]
    Cancelled,
    #[error("invalid diagnostic export input")]
    InvalidInput,
    #[error("diagnostic export boundary is unsafe")]
    Boundary,
    #[error("diagnostic export operation failed")]
    Io,
    #[error("diagnostic export leak scan failed")]
    LeakDetected,
    #[error("invalid diagnostic export package")]
    InvalidPackage,
    #[error("diagnostic export limit reached")]
    Limit,
}

#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExportBuildIdentity {
    wokcore_version: Box<str>,
    git_commit: Box<str>,
    api_major: u16,
    capability_version: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodeExportBuildIdentity {
    wokcore_version: Box<str>,
    git_commit: Box<str>,
    api_major: u16,
    capability_version: u32,
}

impl<'de> Deserialize<'de> for ExportBuildIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let decoded = DecodeExportBuildIdentity::deserialize(deserializer)?;
        Self::new(
            &decoded.wokcore_version,
            &decoded.git_commit,
            decoded.api_major,
            decoded.capability_version,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl ExportBuildIdentity {
    pub fn new(
        wokcore_version: &str,
        git_commit: &str,
        api_major: u16,
        capability_version: u32,
    ) -> Result<Self, ExportError> {
        crate::event::WokcoreVersion::parse(wokcore_version)
            .map_err(|_| ExportError::InvalidInput)?;
        crate::event::GitCommit::parse(git_commit).map_err(|_| ExportError::InvalidInput)?;
        if api_major == 0 || capability_version == 0 {
            return Err(ExportError::InvalidInput);
        }
        Ok(Self {
            wokcore_version: wokcore_version.into(),
            git_commit: git_commit.into(),
            api_major,
            capability_version,
        })
    }

    fn allocated_buffer_bytes(&self) -> Result<usize, ExportError> {
        self.wokcore_version
            .len()
            .checked_add(self.git_commit.len())
            .ok_or(ExportError::Limit)
    }
}

impl fmt::Debug for ExportBuildIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ExportBuildIdentity([redacted])")
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum ExportCapability {
    #[serde(rename = "client_token.issue")]
    ClientTokenIssue,
    #[serde(rename = "client_token.revoke")]
    ClientTokenRevoke,
    #[serde(rename = "diagnostics.export")]
    DiagnosticsExport,
    #[serde(rename = "diagnostics.read")]
    DiagnosticsRead,
    #[serde(rename = "discovery.v1")]
    DiscoveryV1,
    #[serde(rename = "service.drain")]
    ServiceDrain,
    #[serde(rename = "service.status")]
    ServiceStatus,
    #[serde(rename = "sessions.read")]
    SessionsRead,
    #[serde(rename = "usage.read")]
    UsageRead,
}

#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct CapabilitySummary(Box<[ExportCapability]>);

impl CapabilitySummary {
    pub fn new(mut capabilities: Vec<ExportCapability>) -> Result<Self, ExportError> {
        if capabilities.len() > MAX_EXPORT_CAPABILITIES {
            return Err(ExportError::Limit);
        }
        capabilities.sort_unstable();
        capabilities.dedup();
        Ok(Self(capabilities.into_boxed_slice()))
    }

    fn allocated_buffer_bytes(&self) -> Result<usize, ExportError> {
        allocated_slots::<ExportCapability>(self.0.len())
    }
}

impl<'de> Deserialize<'de> for CapabilitySummary {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Vec::<ExportCapability>::deserialize(deserializer)
            .and_then(|capabilities| Self::new(capabilities).map_err(serde::de::Error::custom))
    }
}

impl fmt::Debug for CapabilitySummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapabilitySummary")
            .field("count", &self.0.len())
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExportConfiguration {
    diagnostics_enabled: bool,
    retention_days: u8,
    segment_mib: u8,
    build: ExportBuildIdentity,
    capabilities: CapabilitySummary,
}

impl ExportConfiguration {
    pub fn new(
        diagnostics_enabled: bool,
        retention_days: u8,
        segment_mib: u8,
        build: ExportBuildIdentity,
        capabilities: CapabilitySummary,
    ) -> Result<Self, ExportError> {
        if !(1..=7).contains(&retention_days) || !(1..=4).contains(&segment_mib) {
            return Err(ExportError::InvalidInput);
        }
        Ok(Self {
            diagnostics_enabled,
            retention_days,
            segment_mib,
            build,
            capabilities,
        })
    }

    fn allocated_buffer_bytes(&self) -> Result<usize, ExportError> {
        self.build
            .allocated_buffer_bytes()?
            .checked_add(self.capabilities.allocated_buffer_bytes()?)
            .ok_or(ExportError::Limit)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodeExportConfiguration {
    diagnostics_enabled: bool,
    retention_days: u8,
    segment_mib: u8,
    build: ExportBuildIdentity,
    capabilities: CapabilitySummary,
}

impl<'de> Deserialize<'de> for ExportConfiguration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let decoded = DecodeExportConfiguration::deserialize(deserializer)?;
        Self::new(
            decoded.diagnostics_enabled,
            decoded.retention_days,
            decoded.segment_mib,
            decoded.build,
            decoded.capabilities,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportPlatformCategory {
    None,
    Network,
    Permission,
    Filesystem,
    Process,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StableExportErrorCode {
    UpstreamTimeout,
    UpstreamUnavailable,
    InvalidResponse,
    InternalInvariant,
    ResourceLimit,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StableExportErrorSource {
    Router,
    Provider,
    Protocol,
    Platform,
    Storage,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StableErrorSummary {
    code: StableExportErrorCode,
    source_chain: Box<[StableExportErrorSource]>,
    platform: ExportPlatformCategory,
    count: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodeStableErrorSummary {
    code: StableExportErrorCode,
    source_chain: Vec<StableExportErrorSource>,
    platform: ExportPlatformCategory,
    count: u64,
}

impl<'de> Deserialize<'de> for StableErrorSummary {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let decoded = DecodeStableErrorSummary::deserialize(deserializer)?;
        Self::new(
            decoded.code,
            decoded.source_chain,
            decoded.platform,
            decoded.count,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl StableErrorSummary {
    pub fn new(
        code: StableExportErrorCode,
        source_chain: Vec<StableExportErrorSource>,
        platform: ExportPlatformCategory,
        count: u64,
    ) -> Result<Self, ExportError> {
        if source_chain.len() > MAX_EXPORT_ERROR_CHAIN || count == 0 {
            return Err(ExportError::InvalidInput);
        }
        Ok(Self {
            code,
            source_chain: source_chain.into_boxed_slice(),
            platform,
            count,
        })
    }

    fn allocated_buffer_bytes(&self) -> Result<usize, ExportError> {
        allocated_slots::<StableExportErrorSource>(self.source_chain.len())
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExportRedactionCounters {
    authorization_values_removed: u64,
    cookie_values_removed: u64,
    body_values_removed: u64,
    path_values_removed: u64,
    token_values_removed: u64,
    credential_values_removed: u64,
}

impl ExportRedactionCounters {
    pub const fn new(
        authorization_values_removed: u64,
        cookie_values_removed: u64,
        body_values_removed: u64,
        path_values_removed: u64,
        token_values_removed: u64,
        credential_values_removed: u64,
    ) -> Self {
        Self {
            authorization_values_removed,
            cookie_values_removed,
            body_values_removed,
            path_values_removed,
            token_values_removed,
            credential_values_removed,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceSummary {
    ring_bytes: u64,
    retained_segment_bytes: u64,
    suppressed_snapshot_count: u64,
    platform: ExportPlatformCategory,
    stable_errors: Box<[StableErrorSummary]>,
    redaction: ExportRedactionCounters,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodeResourceSummary {
    ring_bytes: u64,
    retained_segment_bytes: u64,
    suppressed_snapshot_count: u64,
    platform: ExportPlatformCategory,
    stable_errors: Vec<StableErrorSummary>,
    redaction: ExportRedactionCounters,
}

impl<'de> Deserialize<'de> for ResourceSummary {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let decoded = DecodeResourceSummary::deserialize(deserializer)?;
        Self::new(
            decoded.ring_bytes,
            decoded.retained_segment_bytes,
            decoded.suppressed_snapshot_count,
            decoded.platform,
            decoded.stable_errors,
            decoded.redaction,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl ResourceSummary {
    pub fn new(
        ring_bytes: u64,
        retained_segment_bytes: u64,
        suppressed_snapshot_count: u64,
        platform: ExportPlatformCategory,
        mut stable_errors: Vec<StableErrorSummary>,
        redaction: ExportRedactionCounters,
    ) -> Result<Self, ExportError> {
        if stable_errors.len() > MAX_EXPORT_ERROR_SUMMARIES {
            return Err(ExportError::Limit);
        }
        stable_errors.sort_unstable();
        Ok(Self {
            ring_bytes,
            retained_segment_bytes,
            suppressed_snapshot_count,
            platform,
            stable_errors: stable_errors.into_boxed_slice(),
            redaction,
        })
    }

    fn allocated_buffer_bytes(&self) -> Result<usize, ExportError> {
        self.stable_errors.iter().try_fold(
            allocated_slots::<StableErrorSummary>(self.stable_errors.len())?,
            |total, error| {
                total
                    .checked_add(error.allocated_buffer_bytes()?)
                    .ok_or(ExportError::Limit)
            },
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExportSelection {
    truncated: bool,
    omitted_event_count: u64,
}

impl ExportSelection {
    pub const fn complete() -> Self {
        Self {
            truncated: false,
            omitted_event_count: 0,
        }
    }

    pub fn truncated(omitted_event_count: u64) -> Result<Self, ExportError> {
        if omitted_event_count == 0 {
            return Err(ExportError::InvalidInput);
        }
        Ok(Self {
            truncated: true,
            omitted_event_count,
        })
    }
}

pub struct SupportPackage {
    events: Vec<DiagnosticReadLease>,
    configuration: ExportConfiguration,
    resources: ResourceSummary,
    selection: ExportSelection,
}

#[derive(Clone, Copy)]
enum PersistentSourceKind {
    Segment,
    Snapshot,
}

#[derive(Default)]
struct PersistentSourceLimits {
    scan_bytes: u64,
    snapshot_count: usize,
}

impl PersistentSourceLimits {
    fn observe(&mut self, kind: PersistentSourceKind, length: u64) -> Result<(), ExportError> {
        let maximum_file_bytes = match kind {
            PersistentSourceKind::Segment => crate::segment::MAX_SEGMENT_BYTES,
            PersistentSourceKind::Snapshot => crate::snapshot::MAX_FAILURE_SNAPSHOT_BYTES,
        };
        if length == 0 {
            return Err(ExportError::InvalidInput);
        }
        if length > u64::try_from(maximum_file_bytes).map_err(|_| ExportError::Limit)? {
            return Err(ExportError::Limit);
        }
        self.scan_bytes = self
            .scan_bytes
            .checked_add(length)
            .ok_or(ExportError::Limit)?;
        if self.scan_bytes > MAX_EXPORT_SOURCE_SCAN_BYTES {
            return Err(ExportError::Limit);
        }
        if matches!(kind, PersistentSourceKind::Snapshot) {
            self.snapshot_count = self
                .snapshot_count
                .checked_add(1)
                .ok_or(ExportError::Limit)?;
            if self.snapshot_count > crate::snapshot::MAX_FAILURE_SNAPSHOTS {
                return Err(ExportError::Limit);
            }
        }
        Ok(())
    }
}

impl SupportPackage {
    pub fn new(
        mut events: Vec<DiagnosticReadLease>,
        configuration: ExportConfiguration,
        resources: ResourceSummary,
        selection: ExportSelection,
    ) -> Result<Self, ExportError> {
        if events.len() > 4_096 {
            return Err(ExportError::Limit);
        }
        events.sort_by(|left, right| {
            left.name()
                .as_encoded_bytes()
                .cmp(right.name().as_encoded_bytes())
        });
        let mut previous = None;
        let mut limits = PersistentSourceLimits::default();
        for event in &events {
            let name = event.name().to_str().ok_or(ExportError::InvalidInput)?;
            let kind = persistent_source_kind(name).ok_or(ExportError::InvalidInput)?;
            if previous.is_some_and(|previous| previous == name) {
                return Err(ExportError::InvalidInput);
            }
            limits.observe(kind, event.len())?;
            previous = Some(name);
        }
        Ok(Self {
            events,
            configuration,
            resources,
            selection,
        })
    }
}

fn persistent_source_kind(name: &str) -> Option<PersistentSourceKind> {
    [
        ("segment-", PersistentSourceKind::Segment),
        ("snapshot-", PersistentSourceKind::Snapshot),
    ]
    .into_iter()
    .find_map(|(prefix, kind)| {
        name.strip_prefix(prefix)
            .and_then(|suffix| suffix.strip_suffix(".jsonl"))
            .is_some_and(|index| {
                index.len() == 20
                    && index.bytes().all(|byte| byte.is_ascii_digit())
                    && index.parse::<u64>().is_ok_and(|index| index != 0)
            })
            .then_some(kind)
    })
}

impl fmt::Debug for SupportPackage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SupportPackage([redacted])")
    }
}

#[derive(Default)]
pub struct LeakCanarySet {
    canary_count: usize,
    patterns: Vec<Box<[u8]>>,
}

impl LeakCanarySet {
    pub const fn new() -> Self {
        Self {
            canary_count: 0,
            patterns: Vec::new(),
        }
    }

    pub fn push(&mut self, value: &[u8]) -> Result<(), ExportError> {
        if value.is_empty()
            || value.len() > MAX_LEAK_CANARY_BYTES
            || self.canary_count >= MAX_LEAK_CANARIES
        {
            return Err(ExportError::InvalidInput);
        }
        self.canary_count += 1;
        self.patterns.push(value.to_vec().into_boxed_slice());
        Ok(())
    }

    fn max_pattern_len(&self) -> usize {
        self.patterns
            .iter()
            .map(|value| value.len())
            .chain(FIXED_FORBIDDEN_KEYS.iter().map(|value| value.len() + 4))
            .chain(std::iter::once(32))
            .max()
            .unwrap_or(1)
    }

    fn max_raw_canary_len(&self) -> usize {
        self.patterns
            .iter()
            .map(|value| value.len())
            .max()
            .unwrap_or(1)
    }

    fn contains_raw_canary(&self, bytes: &[u8]) -> bool {
        self.patterns
            .iter()
            .map(Box::as_ref)
            .any(|pattern| bytes.windows(pattern.len()).any(|window| window == pattern))
    }

    fn contains_fixed_forbidden(&self, bytes: &[u8]) -> bool {
        contains_forbidden_key(bytes) || contains_forbidden_location(bytes)
    }

    fn contains_decoded_canary(&self, value: &str) -> bool {
        self.contains_raw_canary(value.as_bytes())
    }

    fn validate_json_document(&self, bytes: &[u8]) -> Result<usize, ExportError> {
        if self.contains_fixed_forbidden(bytes) {
            return Err(ExportError::LeakDetected);
        }
        let found = Cell::new(false);
        let mut deserializer = serde_json::Deserializer::from_slice(bytes);
        SemanticCanarySeed {
            canaries: self,
            found: &found,
        }
        .deserialize(&mut deserializer)
        .map_err(|_| ExportError::InvalidInput)?;
        deserializer.end().map_err(|_| ExportError::InvalidInput)?;
        if found.get() {
            Err(ExportError::LeakDetected)
        } else {
            // serde_json borrows unescaped strings. An escaped key or value may require one
            // temporary decoded String, whose UTF-8 allocation cannot exceed its JSON document.
            Ok(bytes.len())
        }
    }

    fn allocated_buffer_bytes(&self) -> Result<usize, ExportError> {
        let pattern_slots = allocated_slots::<Box<[u8]>>(self.patterns.capacity())?;
        self.patterns
            .iter()
            .try_fold(pattern_slots, |total, pattern| {
                total.checked_add(pattern.len()).ok_or(ExportError::Limit)
            })
    }

    pub fn validate_candidate(&self, bytes: &[u8]) -> Result<(), ExportError> {
        if bytes.len() > EXPORT_BUFFER_BYTES {
            return Err(ExportError::Limit);
        }
        if std::str::from_utf8(bytes).is_err() {
            return Err(ExportError::LeakDetected);
        }
        match self.validate_json_document(bytes) {
            Ok(_) => Ok(()),
            Err(ExportError::InvalidInput) => {
                if self.contains_fixed_forbidden(bytes) || self.contains_raw_canary(bytes) {
                    Err(ExportError::LeakDetected)
                } else {
                    Ok(())
                }
            }
            Err(error) => Err(error),
        }
    }
}

#[derive(Clone, Copy)]
struct SemanticCanarySeed<'a> {
    canaries: &'a LeakCanarySet,
    found: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for SemanticCanarySeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(SemanticCanaryVisitor(self))
    }
}

struct SemanticCanaryVisitor<'a>(SemanticCanarySeed<'a>);

impl SemanticCanaryVisitor<'_> {
    fn observe_value(&self, value: &str) {
        if self.0.canaries.contains_decoded_canary(value)
            || contains_decoded_forbidden_location(value)
        {
            self.0.found.set(true);
        }
    }
}

impl<'de> Visitor<'de> for SemanticCanaryVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        self.observe_value(value);
        Ok(())
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        self.observe_value(&value);
        Ok(())
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        self.0.deserialize(deserializer)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element_seed(self.0)?.is_some() {}
        Ok(())
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        while map.next_key_seed(SemanticCanaryKeySeed(self.0))?.is_some() {
            map.next_value_seed(self.0)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct SemanticCanaryKeySeed<'a>(SemanticCanarySeed<'a>);

impl<'de> DeserializeSeed<'de> for SemanticCanaryKeySeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_str(SemanticCanaryKeyVisitor(self.0))
    }
}

struct SemanticCanaryKeyVisitor<'a>(SemanticCanarySeed<'a>);

impl SemanticCanaryKeyVisitor<'_> {
    fn observe(&self, value: &str) {
        if self.0.canaries.contains_decoded_canary(value)
            || FIXED_FORBIDDEN_KEYS
                .iter()
                .any(|key| value.as_bytes().eq_ignore_ascii_case(key))
        {
            self.0.found.set(true);
        }
    }
}

impl<'de> Visitor<'de> for SemanticCanaryKeyVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON object key")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        self.observe(value);
        Ok(())
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        self.observe(&value);
        Ok(())
    }
}

impl fmt::Debug for LeakCanarySet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LeakCanarySet([redacted])")
    }
}

#[derive(Clone)]
pub struct ExportCoordinator {
    admission: Arc<Semaphore>,
}

impl ExportCoordinator {
    pub fn new() -> Self {
        Self {
            admission: Arc::new(Semaphore::new(1)),
        }
    }

    pub fn try_begin(&self) -> Result<ExportOperation, ExportError> {
        let permit = Arc::clone(&self.admission)
            .try_acquire_owned()
            .map_err(|_| ExportError::Busy)?;
        Ok(Self::operation(permit))
    }

    pub async fn begin(&self) -> Result<ExportOperation, ExportError> {
        let permit = Arc::clone(&self.admission)
            .acquire_owned()
            .await
            .map_err(|_| ExportError::Cancelled)?;
        Ok(Self::operation(permit))
    }

    fn operation(permit: OwnedSemaphorePermit) -> ExportOperation {
        ExportOperation {
            state: Arc::new(ExportOperationState {
                phase: AtomicU8::new(EXPORT_READY),
                permit: Mutex::new(Some(permit)),
            }),
            armed: true,
        }
    }
}

impl Default for ExportCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ExportCoordinator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ExportCoordinator([redacted])")
    }
}

struct ExportOperationState {
    phase: AtomicU8,
    permit: Mutex<Option<OwnedSemaphorePermit>>,
}

impl ExportOperationState {
    fn release_admission(&self) {
        let mut permit = self
            .permit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(permit.take());
    }

    fn cancel_owner(&self) {
        loop {
            let phase = self.phase.load(Ordering::Acquire);
            match phase {
                EXPORT_READY => {
                    if self
                        .phase
                        .compare_exchange(
                            EXPORT_READY,
                            EXPORT_CANCELLED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        self.release_admission();
                        return;
                    }
                }
                EXPORT_RUNNING => {
                    if self
                        .phase
                        .compare_exchange(
                            EXPORT_RUNNING,
                            EXPORT_CANCELLED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return;
                    }
                }
                EXPORT_CANCELLED | EXPORT_COMMITTING | EXPORT_FINISHED => return,
                _ => return,
            }
        }
    }
}

const EXPORT_READY: u8 = 0;
const EXPORT_RUNNING: u8 = 1;
const EXPORT_CANCELLED: u8 = 2;
const EXPORT_COMMITTING: u8 = 3;
const EXPORT_FINISHED: u8 = 4;

pub struct ExportOperation {
    state: Arc<ExportOperationState>,
    armed: bool,
}

impl ExportOperation {
    pub fn start_worker(&self) -> Result<ExportWorkerLease, ExportError> {
        self.state
            .phase
            .compare_exchange(
                EXPORT_READY,
                EXPORT_RUNNING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| ExportError::Busy)?;
        Ok(ExportWorkerLease {
            state: Arc::clone(&self.state),
        })
    }

    pub fn split(mut self) -> Result<(ExportRequestOwner, ExportWorkerLease), ExportError> {
        let worker = self.start_worker()?;
        self.armed = false;
        Ok((
            ExportRequestOwner {
                state: Arc::clone(&self.state),
            },
            worker,
        ))
    }
}

impl Drop for ExportOperation {
    fn drop(&mut self) {
        if self.armed {
            self.state.cancel_owner();
        }
    }
}

impl fmt::Debug for ExportOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ExportOperation([redacted])")
    }
}

pub struct ExportRequestOwner {
    state: Arc<ExportOperationState>,
}

impl Drop for ExportRequestOwner {
    fn drop(&mut self) {
        self.state.cancel_owner();
    }
}

impl fmt::Debug for ExportRequestOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ExportRequestOwner([redacted])")
    }
}

pub struct ExportWorkerLease {
    state: Arc<ExportOperationState>,
}

impl ExportWorkerLease {
    fn check_cancelled(&self) -> Result<(), ExportError> {
        match self.state.phase.load(Ordering::Acquire) {
            EXPORT_RUNNING => Ok(()),
            EXPORT_CANCELLED => Err(ExportError::Cancelled),
            _ => Err(ExportError::InvalidInput),
        }
    }

    fn begin_commit(&self) -> Result<(), ExportError> {
        self.state
            .phase
            .compare_exchange(
                EXPORT_RUNNING,
                EXPORT_COMMITTING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|phase| {
                if phase == EXPORT_CANCELLED {
                    ExportError::Cancelled
                } else {
                    ExportError::InvalidInput
                }
            })
    }

    fn finish(&self) {
        self.state.phase.store(EXPORT_FINISHED, Ordering::Release);
        self.state.release_admission();
    }

    fn cancel_worker(&self) {
        loop {
            let phase = self.state.phase.load(Ordering::Acquire);
            match phase {
                EXPORT_RUNNING => {
                    if self
                        .state
                        .phase
                        .compare_exchange(
                            phase,
                            EXPORT_CANCELLED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        self.state.release_admission();
                        return;
                    }
                }
                EXPORT_CANCELLED => {
                    self.state.release_admission();
                    return;
                }
                EXPORT_COMMITTING => {
                    if self
                        .state
                        .phase
                        .compare_exchange(
                            EXPORT_COMMITTING,
                            EXPORT_FINISHED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        self.state.release_admission();
                    }
                    return;
                }
                EXPORT_READY | EXPORT_FINISHED => return,
                _ => return,
            }
        }
    }
}

impl Drop for ExportWorkerLease {
    fn drop(&mut self) {
        self.cancel_worker();
    }
}

impl fmt::Debug for ExportWorkerLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ExportWorkerLease([redacted])")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExportStats {
    package_bytes: u64,
    peak_buffer_bytes: usize,
}

impl ExportStats {
    pub const fn package_bytes(self) -> u64 {
        self.package_bytes
    }

    /// Conservative high-water mark for heap buffers retained by package construction.
    ///
    /// This includes the pinned destination, source lease slots, merge cursors and heap entries,
    /// serialized entries, canary patterns and scan tails, and transient encoded event/header
    /// buffers.
    pub const fn peak_buffer_bytes(self) -> usize {
        self.peak_buffer_bytes
    }
}

pub struct PreparedSupportPackage {
    destination: Option<PinnedExportDestination>,
    worker: Option<ExportWorkerLease>,
    stats: ExportStats,
}

impl PreparedSupportPackage {
    pub const fn stats(&self) -> ExportStats {
        self.stats
    }

    pub fn into_body(self, owner: ExportRequestOwner) -> Result<SupportPackageBody, ExportError> {
        let worker = self.worker.as_ref().ok_or(ExportError::InvalidInput)?;
        if !Arc::ptr_eq(&worker.state, &owner.state) {
            return Err(ExportError::InvalidInput);
        }
        Ok(SupportPackageBody {
            owner: Some(owner),
            package: self,
            offset: 0,
        })
    }

    pub fn publish(mut self) -> Result<ExportStats, ExportError> {
        let worker = self.worker.as_ref().ok_or(ExportError::InvalidInput)?;
        worker.begin_commit()?;
        let destination = self.destination.take().ok_or(ExportError::InvalidInput)?;
        let result = destination.commit().map_err(|_| ExportError::Boundary);
        worker.finish();
        drop(self.worker.take());
        result.map(|()| self.stats)
    }
}

impl fmt::Debug for PreparedSupportPackage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PreparedSupportPackage([redacted])")
    }
}

pub struct SupportPackageBody {
    owner: Option<ExportRequestOwner>,
    package: PreparedSupportPackage,
    offset: u64,
}

impl SupportPackageBody {
    pub fn read_next(&mut self, maximum_bytes: usize) -> Result<Option<Vec<u8>>, ExportError> {
        if maximum_bytes == 0 || maximum_bytes > EXPORT_BUFFER_BYTES {
            return Err(ExportError::InvalidInput);
        }
        let worker = self
            .package
            .worker
            .as_ref()
            .ok_or(ExportError::InvalidInput)?;
        worker.check_cancelled()?;
        let destination = self
            .package
            .destination
            .as_mut()
            .ok_or(ExportError::InvalidInput)?;
        let bytes = destination
            .read_owned_range(self.offset, maximum_bytes)
            .map_err(|_| ExportError::Boundary)?;
        if bytes.is_empty() {
            return Ok(None);
        }
        self.offset = self
            .offset
            .checked_add(u64::try_from(bytes.len()).map_err(|_| ExportError::Limit)?)
            .ok_or(ExportError::Limit)?;
        Ok(Some(bytes))
    }

    pub fn finish(mut self) -> Result<ExportStats, ExportError> {
        let destination = self
            .package
            .destination
            .as_mut()
            .ok_or(ExportError::InvalidInput)?;
        if self.offset != destination.len().map_err(|_| ExportError::Boundary)? {
            return Err(ExportError::InvalidInput);
        }
        drop(self.package.destination.take());
        let worker = self
            .package
            .worker
            .take()
            .ok_or(ExportError::InvalidInput)?;
        worker.finish();
        drop(worker);
        drop(self.owner.take());
        Ok(self.package.stats)
    }
}

impl fmt::Debug for SupportPackageBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SupportPackageBody([redacted])")
    }
}

#[derive(Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    manifest_version: u8,
    diagnostic_schema_version: u8,
    event_count: usize,
    truncated: bool,
    omitted_event_count: u64,
    build: ExportBuildIdentity,
    checksum_algorithm: Box<str>,
}

impl Manifest {
    fn allocated_buffer_bytes(&self) -> Result<usize, ExportError> {
        self.build
            .allocated_buffer_bytes()?
            .checked_add(self.checksum_algorithm.len())
            .ok_or(ExportError::Limit)
    }
}

struct EntryMetadata {
    name: &'static [u8],
    crc32: u32,
    size: u32,
    sha256: [u8; 32],
}

#[derive(Clone, Copy)]
struct CentralRecord {
    name: &'static [u8],
    crc32: u32,
    size: u32,
    offset: u32,
}

pub fn export_support_package(
    worker: ExportWorkerLease,
    destination: PinnedExportDestination,
    package: &mut SupportPackage,
    canaries: &LeakCanarySet,
) -> Result<ExportStats, ExportError> {
    prepare_support_package(worker, destination, package, canaries)?.publish()
}

pub fn prepare_support_package(
    worker: ExportWorkerLease,
    destination: PinnedExportDestination,
    package: &mut SupportPackage,
    canaries: &LeakCanarySet,
) -> Result<PreparedSupportPackage, ExportError> {
    worker.check_cancelled()?;
    let destination_resident_buffer_bytes = destination
        .resident_allocation_bytes()
        .map_err(|_| ExportError::Limit)?;
    let source_lease_buffer_bytes = package.events.iter().try_fold(
        allocated_slots::<DiagnosticReadLease>(package.events.capacity())?,
        |total, lease| {
            total
                .checked_add(
                    lease
                        .resident_allocation_bytes()
                        .map_err(|_| ExportError::Limit)?,
                )
                .ok_or(ExportError::Limit)
        },
    )?;
    let package_resident_buffer_bytes = [
        destination_resident_buffer_bytes,
        source_lease_buffer_bytes,
        package.configuration.allocated_buffer_bytes()?,
        package.resources.allocated_buffer_bytes()?,
        canaries.allocated_buffer_bytes()?,
    ]
    .into_iter()
    .try_fold(0_usize, |total, bytes| {
        total.checked_add(bytes).ok_or(ExportError::Limit)
    })?;
    let events_metadata = metadata_for_events(&mut package.events, &worker, canaries)?;
    let metadata_peak_buffer_bytes = [
        package_resident_buffer_bytes,
        events_metadata.source_merge_buffer_bytes,
        events_metadata.source_merge_transient_buffer_bytes,
    ]
    .into_iter()
    .try_fold(0_usize, |total, bytes| {
        total.checked_add(bytes).ok_or(ExportError::Limit)
    })?;
    let manifest = json_line(&Manifest {
        manifest_version: 1,
        diagnostic_schema_version: 1,
        event_count: events_metadata.event_count,
        truncated: package.selection.truncated,
        omitted_event_count: package.selection.omitted_event_count,
        build: package.configuration.build.clone(),
        checksum_algorithm: "sha256".into(),
    })?;
    let configuration = json_line(&package.configuration)?;
    let resources = json_line(&package.resources)?;
    let semantic_json_scan_buffer_bytes = [
        canaries.validate_json_document(&manifest)?,
        canaries.validate_json_document(&configuration)?,
        canaries.validate_json_document(&resources)?,
    ]
    .into_iter()
    .max()
    .unwrap_or(0);
    let manifest_metadata = metadata_for_bytes(MANIFEST_NAME, &manifest)?;
    let configuration_metadata = metadata_for_bytes(CONFIGURATION_NAME, &configuration)?;
    let resources_metadata = metadata_for_bytes(RESOURCES_NAME, &resources)?;
    let checksums = checksum_document(&[
        &manifest_metadata,
        &events_metadata.entry,
        &configuration_metadata,
        &resources_metadata,
    ]);
    let checksums_metadata = metadata_for_bytes(CHECKSUMS_NAME, &checksums)?;

    let mut central = Vec::with_capacity(usize::from(ZIP_ENTRY_COUNT));
    let resident_buffer_bytes = [
        package_resident_buffer_bytes,
        manifest.capacity(),
        configuration.capacity(),
        resources.capacity(),
        checksums.capacity(),
        allocated_slots::<CentralRecord>(central.capacity())?,
    ]
    .into_iter()
    .try_fold(0_usize, |total, bytes| {
        total.checked_add(bytes).ok_or(ExportError::Limit)
    })?;
    let semantic_json_peak_buffer_bytes = resident_buffer_bytes
        .checked_add(semantic_json_scan_buffer_bytes)
        .ok_or(ExportError::Limit)?;
    let initial_peak_buffer_bytes = metadata_peak_buffer_bytes.max(semantic_json_peak_buffer_bytes);
    let mut writer = PackageWriter::new(
        destination,
        &worker,
        canaries,
        resident_buffer_bytes,
        initial_peak_buffer_bytes,
    )?;
    write_bytes_entry(&mut writer, &manifest_metadata, &manifest, &mut central)?;
    write_events_entry(
        &mut writer,
        &events_metadata.entry,
        &mut package.events,
        &mut central,
    )?;
    write_bytes_entry(
        &mut writer,
        &configuration_metadata,
        &configuration,
        &mut central,
    )?;
    write_bytes_entry(&mut writer, &resources_metadata, &resources, &mut central)?;
    write_bytes_entry(&mut writer, &checksums_metadata, &checksums, &mut central)?;
    write_central_directory(&mut writer, &central)?;
    let (mut destination, mut stats) = writer.complete()?;
    {
        let mut reader = OwnedTemporaryReader::new(&mut destination)?;
        let verification_buffer_bytes =
            verify_support_package_reader(&mut reader, canaries, Some(&worker))?;
        let verification_peak = resident_buffer_bytes
            .checked_add(verification_buffer_bytes)
            .ok_or(ExportError::Limit)?;
        stats.peak_buffer_bytes = stats.peak_buffer_bytes.max(verification_peak);
    }
    Ok(PreparedSupportPackage {
        destination: Some(destination),
        worker: Some(worker),
        stats,
    })
}

fn json_line(value: &impl Serialize) -> Result<Vec<u8>, ExportError> {
    let mut encoded = serde_json::to_vec(value).map_err(|_| ExportError::InvalidInput)?;
    if encoded.len() > EXPORT_BUFFER_BYTES - 1 {
        return Err(ExportError::Limit);
    }
    encoded.push(b'\n');
    Ok(encoded)
}

fn metadata_for_bytes(name: &'static [u8], bytes: &[u8]) -> Result<EntryMetadata, ExportError> {
    let size = u32::try_from(bytes.len()).map_err(|_| ExportError::Limit)?;
    let mut crc = Crc32::new();
    crc.update(bytes);
    let mut sha = Sha256::new();
    sha.update(bytes);
    Ok(EntryMetadata {
        name,
        crc32: crc.finish(),
        size,
        sha256: sha.finalize().into(),
    })
}

struct EventsMetadata {
    entry: EntryMetadata,
    event_count: usize,
    source_merge_buffer_bytes: usize,
    source_merge_transient_buffer_bytes: usize,
}

struct SourceCursor {
    kind: PersistentSourceKind,
    reader: PersistentLineReader,
    current: Vec<u8>,
    sequence: u64,
    event_count: usize,
}

struct PersistentLineReader {
    source_length: u64,
    next_read_offset: u64,
    buffer: Vec<u8>,
    buffer_position: usize,
}

impl PersistentLineReader {
    const fn new(source_length: u64) -> Self {
        Self {
            source_length,
            next_read_offset: 0,
            buffer: Vec::new(),
            buffer_position: 0,
        }
    }

    fn is_exhausted(&self) -> bool {
        self.buffer_position == self.buffer.len() && self.next_read_offset == self.source_length
    }

    fn read_line(
        &mut self,
        lease: &mut DiagnosticReadLease,
        maximum_line_bytes: usize,
        worker: &ExportWorkerLease,
    ) -> Result<Vec<u8>, ExportError> {
        if self.is_exhausted() {
            return Err(ExportError::InvalidInput);
        }
        let mut line = Vec::new();
        loop {
            if self.buffer_position == self.buffer.len() {
                worker.check_cancelled()?;
                let remaining = self
                    .source_length
                    .checked_sub(self.next_read_offset)
                    .ok_or(ExportError::Boundary)?;
                if remaining == 0 {
                    return Err(ExportError::InvalidInput);
                }
                let maximum_bytes = usize::try_from(remaining.min(EXPORT_BUFFER_BYTES as u64))
                    .map_err(|_| ExportError::Limit)?;
                self.buffer = Vec::new();
                self.buffer = read_persistent_range(lease, self.next_read_offset, maximum_bytes)?;
                if self.buffer.is_empty() || self.buffer.len() > maximum_bytes {
                    return Err(ExportError::Boundary);
                }
                self.next_read_offset = self
                    .next_read_offset
                    .checked_add(u64::try_from(self.buffer.len()).map_err(|_| ExportError::Limit)?)
                    .ok_or(ExportError::Limit)?;
                self.buffer_position = 0;
            }
            let available = &self.buffer[self.buffer_position..];
            if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
                if line.len().saturating_add(newline) > maximum_line_bytes {
                    return Err(ExportError::Limit);
                }
                line.extend_from_slice(&available[..newline]);
                self.buffer_position = self
                    .buffer_position
                    .checked_add(newline.checked_add(1).ok_or(ExportError::Limit)?)
                    .ok_or(ExportError::Limit)?;
                return Ok(line);
            }
            if line.len().saturating_add(available.len()) > maximum_line_bytes {
                return Err(ExportError::Limit);
            }
            line.extend_from_slice(available);
            self.buffer_position = self.buffer.len();
        }
    }

    fn allocated_buffer_bytes(&self) -> usize {
        self.buffer.capacity()
    }
}

struct MergedPersistentEvents<'a> {
    leases: &'a mut [DiagnosticReadLease],
    cursors: Vec<Option<SourceCursor>>,
    pending: BinaryHeap<Reverse<(u64, usize)>>,
    worker: &'a ExportWorkerLease,
    canaries: &'a LeakCanarySet,
    last_emitted_sequence: Option<u64>,
    maximum_event_buffer_bytes: usize,
}

impl<'a> MergedPersistentEvents<'a> {
    fn new(
        leases: &'a mut [DiagnosticReadLease],
        worker: &'a ExportWorkerLease,
        canaries: &'a LeakCanarySet,
    ) -> Result<Self, ExportError> {
        let mut cursors = Vec::with_capacity(leases.len());
        let mut pending = BinaryHeap::new();
        let mut maximum_event_buffer_bytes = 0_usize;
        for (index, lease) in leases.iter_mut().enumerate() {
            worker.check_cancelled()?;
            let kind = if lease
                .name()
                .to_str()
                .is_some_and(|name| name.starts_with("snapshot-"))
            {
                PersistentSourceKind::Snapshot
            } else {
                PersistentSourceKind::Segment
            };
            let maximum_source_bytes = match kind {
                PersistentSourceKind::Segment => crate::segment::MAX_SEGMENT_BYTES,
                PersistentSourceKind::Snapshot => crate::snapshot::MAX_FAILURE_SNAPSHOT_BYTES,
            };
            if lease.is_empty()
                || lease.len()
                    > u64::try_from(maximum_source_bytes).map_err(|_| ExportError::Limit)?
            {
                return Err(ExportError::InvalidInput);
            }
            let mut reader = PersistentLineReader::new(lease.len());
            if matches!(kind, PersistentSourceKind::Snapshot) {
                let header = reader.read_line(lease, EXPORT_BUFFER_BYTES, worker)?;
                if !crate::snapshot::validate_snapshot_header_line(&header) {
                    return Err(ExportError::InvalidInput);
                }
            }
            let cursor = next_source_cursor(lease, kind, reader, 1, worker, canaries)?;
            if matches!(kind, PersistentSourceKind::Snapshot) && cursor.is_none() {
                return Err(ExportError::InvalidInput);
            }
            if let Some(cursor) = cursor.as_ref() {
                pending.push(Reverse((cursor.sequence, index)));
                maximum_event_buffer_bytes =
                    maximum_event_buffer_bytes.max(cursor.current.capacity());
            }
            cursors.push(cursor);
        }
        Ok(Self {
            leases,
            cursors,
            pending,
            worker,
            canaries,
            last_emitted_sequence: None,
            maximum_event_buffer_bytes,
        })
    }

    fn allocated_buffer_bytes(&self) -> Result<usize, ExportError> {
        let retained = allocated_slots::<Option<SourceCursor>>(self.cursors.capacity())?
            .checked_add(allocated_slots::<Reverse<(u64, usize)>>(
                self.pending.capacity(),
            )?)
            .ok_or(ExportError::Limit)?;
        self.cursors
            .iter()
            .flatten()
            .try_fold(retained, |total, cursor| {
                total
                    .checked_add(cursor.reader.allocated_buffer_bytes())
                    .and_then(|bytes| bytes.checked_add(cursor.current.capacity()))
                    .ok_or(ExportError::Limit)
            })
    }

    fn next_event(&mut self) -> Result<Option<Vec<u8>>, ExportError> {
        let Some(Reverse((sequence, first_index))) = self.pending.pop() else {
            return Ok(None);
        };
        self.worker.check_cancelled()?;
        if self
            .last_emitted_sequence
            .is_some_and(|previous| sequence <= previous)
        {
            return Err(ExportError::InvalidInput);
        }
        let mut canonical = self.read_current(first_index)?;
        self.advance(first_index)?;
        while self
            .pending
            .peek()
            .is_some_and(|Reverse((candidate, _))| *candidate == sequence)
        {
            let Reverse((_, duplicate_index)) =
                self.pending.pop().ok_or(ExportError::InvalidInput)?;
            let duplicate = self.read_current(duplicate_index)?;
            if duplicate != canonical {
                return Err(ExportError::InvalidInput);
            }
            self.advance(duplicate_index)?;
        }
        canonical.push(b'\n');
        self.last_emitted_sequence = Some(sequence);
        Ok(Some(canonical))
    }

    fn transient_buffer_bytes(&self) -> Result<usize, ExportError> {
        self.maximum_event_buffer_bytes
            .checked_mul(EVENT_MERGE_ALLOCATION_MULTIPLIER)
            .ok_or(ExportError::Limit)
    }

    fn read_current(&self, index: usize) -> Result<Vec<u8>, ExportError> {
        let cursor = self
            .cursors
            .get(index)
            .and_then(Option::as_ref)
            .ok_or(ExportError::InvalidInput)?;
        Ok(cursor.current.clone())
    }

    fn advance(&mut self, index: usize) -> Result<(), ExportError> {
        self.worker.check_cancelled()?;
        let cursor = self
            .cursors
            .get(index)
            .and_then(Option::as_ref)
            .ok_or(ExportError::InvalidInput)?;
        let previous_sequence = cursor.sequence;
        let kind = cursor.kind;
        let next_event_count = cursor
            .event_count
            .checked_add(1)
            .ok_or(ExportError::Limit)?;
        let cursor = self.cursors[index]
            .take()
            .ok_or(ExportError::InvalidInput)?;
        let next = next_source_cursor(
            &mut self.leases[index],
            kind,
            cursor.reader,
            next_event_count,
            self.worker,
            self.canaries,
        )?;
        if let Some(next) = next.as_ref()
            && (next.sequence <= previous_sequence
                || (matches!(next.kind, PersistentSourceKind::Snapshot)
                    && next.event_count > crate::snapshot::MAX_SNAPSHOT_CAUSAL_EVENTS))
        {
            return Err(ExportError::InvalidInput);
        }
        if let Some(next) = next.as_ref() {
            self.maximum_event_buffer_bytes =
                self.maximum_event_buffer_bytes.max(next.current.capacity());
        }
        self.cursors[index] = next;
        if let Some(next) = self.cursors[index].as_ref() {
            self.pending.push(Reverse((next.sequence, index)));
        }
        Ok(())
    }
}

fn next_source_cursor(
    lease: &mut DiagnosticReadLease,
    kind: PersistentSourceKind,
    mut reader: PersistentLineReader,
    event_count: usize,
    worker: &ExportWorkerLease,
    canaries: &LeakCanarySet,
) -> Result<Option<SourceCursor>, ExportError> {
    if reader.is_exhausted() {
        return Ok(None);
    }
    let current = reader.read_line(lease, crate::event::MAX_PREPARED_EVENT_BYTES, worker)?;
    let (sequence, _) = validate_canonical_event_encoding(&current, canaries)?;
    Ok(Some(SourceCursor {
        kind,
        reader,
        current,
        sequence,
        event_count,
    }))
}

fn validate_canonical_event_encoding(
    encoded: &[u8],
    canaries: &LeakCanarySet,
) -> Result<(u64, usize), ExportError> {
    canaries.validate_json_document(encoded)?;
    let event = decode_trusted_prepared_encoding(encoded).map_err(|_| ExportError::InvalidInput)?;
    let canonical = serde_json::to_vec(&event).map_err(|_| ExportError::InvalidInput)?;
    if canonical != encoded {
        return Err(ExportError::InvalidInput);
    }
    Ok((
        event.sequence(),
        encoded
            .len()
            .checked_mul(EVENT_VALIDATION_ALLOCATION_MULTIPLIER)
            .ok_or(ExportError::Limit)?,
    ))
}

fn read_persistent_range(
    lease: &mut DiagnosticReadLease,
    offset: u64,
    maximum_bytes: usize,
) -> Result<Vec<u8>, ExportError> {
    #[cfg(test)]
    PERSISTENT_READ_RANGE_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));
    lease
        .read_range(offset, maximum_bytes)
        .map_err(|_| ExportError::Boundary)
}

fn metadata_for_events(
    leases: &mut [DiagnosticReadLease],
    worker: &ExportWorkerLease,
    canaries: &LeakCanarySet,
) -> Result<EventsMetadata, ExportError> {
    let mut size = 0_u64;
    let mut crc = Crc32::new();
    let mut sha = Sha256::new();
    let mut event_count = 0_usize;
    let mut events = MergedPersistentEvents::new(leases, worker, canaries)?;
    let mut source_merge_buffer_bytes = events.allocated_buffer_bytes()?;
    let mut source_merge_transient_buffer_bytes = events.transient_buffer_bytes()?;
    while let Some(bytes) = events.next_event()? {
        source_merge_buffer_bytes = source_merge_buffer_bytes.max(events.allocated_buffer_bytes()?);
        source_merge_transient_buffer_bytes =
            source_merge_transient_buffer_bytes.max(events.transient_buffer_bytes()?);
        size = size
            .checked_add(u64::try_from(bytes.len()).map_err(|_| ExportError::Limit)?)
            .ok_or(ExportError::Limit)?;
        if size > MAX_EXPORT_EVENT_BYTES {
            return Err(ExportError::Limit);
        }
        crc.update(&bytes);
        sha.update(&bytes);
        event_count = event_count.checked_add(1).ok_or(ExportError::Limit)?;
    }
    Ok(EventsMetadata {
        entry: EntryMetadata {
            name: EVENTS_NAME,
            crc32: crc.finish(),
            size: u32::try_from(size).map_err(|_| ExportError::Limit)?,
            sha256: sha.finalize().into(),
        },
        event_count,
        source_merge_buffer_bytes,
        source_merge_transient_buffer_bytes,
    })
}

fn checksum_document(entries: &[&EntryMetadata]) -> Vec<u8> {
    let mut document = Vec::with_capacity(entries.len() * 96);
    for entry in entries {
        document.extend_from_slice(&hex_sha256(entry.sha256));
        document.extend_from_slice(b"  ");
        document.extend_from_slice(entry.name);
        document.push(b'\n');
    }
    document
}

fn allocated_slots<T>(capacity: usize) -> Result<usize, ExportError> {
    std::mem::size_of::<T>()
        .checked_mul(capacity)
        .ok_or(ExportError::Limit)
}

fn write_bytes_entry(
    writer: &mut PackageWriter<'_>,
    metadata: &EntryMetadata,
    bytes: &[u8],
    central: &mut Vec<CentralRecord>,
) -> Result<(), ExportError> {
    let record = write_local_header(writer, metadata)?;
    writer.begin_logical_entry();
    writer.write_logical_chunk(bytes, 0, metadata.name == CHECKSUMS_NAME)?;
    central.push(record);
    Ok(())
}

fn write_events_entry(
    writer: &mut PackageWriter<'_>,
    metadata: &EntryMetadata,
    leases: &mut [DiagnosticReadLease],
    central: &mut Vec<CentralRecord>,
) -> Result<(), ExportError> {
    let record = write_local_header(writer, metadata)?;
    writer.begin_logical_entry();
    let mut events = MergedPersistentEvents::new(leases, writer.worker, writer.canaries)?;
    writer.observe_transient_buffer(
        events
            .allocated_buffer_bytes()?
            .checked_add(events.transient_buffer_bytes()?)
            .ok_or(ExportError::Limit)?,
    )?;
    while let Some(bytes) = events.next_event()? {
        writer.observe_transient_buffer(
            events
                .allocated_buffer_bytes()?
                .checked_add(events.transient_buffer_bytes()?)
                .ok_or(ExportError::Limit)?,
        )?;
        writer.write_logical_chunk(&bytes, 0, false)?;
    }
    central.push(record);
    Ok(())
}

fn write_local_header(
    writer: &mut PackageWriter<'_>,
    metadata: &EntryMetadata,
) -> Result<CentralRecord, ExportError> {
    let offset = u32::try_from(writer.bytes_written()).map_err(|_| ExportError::Limit)?;
    let name_len = u16::try_from(metadata.name.len()).map_err(|_| ExportError::Limit)?;
    let mut header = Vec::with_capacity(30 + metadata.name.len());
    push_u32(&mut header, ZIP_LOCAL_SIGNATURE);
    push_u16(&mut header, 20);
    push_u16(&mut header, 0);
    push_u16(&mut header, 0);
    push_u16(&mut header, 0);
    push_u16(&mut header, ZIP_DOS_DATE_1980_01_01);
    push_u32(&mut header, metadata.crc32);
    push_u32(&mut header, metadata.size);
    push_u32(&mut header, metadata.size);
    push_u16(&mut header, name_len);
    push_u16(&mut header, 0);
    header.extend_from_slice(metadata.name);
    writer.observe_transient_buffer(header.capacity())?;
    writer.write_chunk(&header)?;
    Ok(CentralRecord {
        name: metadata.name,
        crc32: metadata.crc32,
        size: metadata.size,
        offset,
    })
}

fn write_central_directory(
    writer: &mut PackageWriter<'_>,
    records: &[CentralRecord],
) -> Result<(), ExportError> {
    if records.len() != usize::from(ZIP_ENTRY_COUNT) {
        return Err(ExportError::InvalidPackage);
    }
    let central_offset = u32::try_from(writer.bytes_written()).map_err(|_| ExportError::Limit)?;
    for record in records {
        let name_len = u16::try_from(record.name.len()).map_err(|_| ExportError::Limit)?;
        let mut header = Vec::with_capacity(46 + record.name.len());
        push_u32(&mut header, ZIP_CENTRAL_SIGNATURE);
        push_u16(&mut header, 20);
        push_u16(&mut header, 20);
        push_u16(&mut header, 0);
        push_u16(&mut header, 0);
        push_u16(&mut header, 0);
        push_u16(&mut header, ZIP_DOS_DATE_1980_01_01);
        push_u32(&mut header, record.crc32);
        push_u32(&mut header, record.size);
        push_u32(&mut header, record.size);
        push_u16(&mut header, name_len);
        push_u16(&mut header, 0);
        push_u16(&mut header, 0);
        push_u16(&mut header, 0);
        push_u16(&mut header, 0);
        push_u32(&mut header, 0);
        push_u32(&mut header, record.offset);
        header.extend_from_slice(record.name);
        writer.observe_transient_buffer(header.capacity())?;
        writer.write_chunk(&header)?;
    }
    let central_size = u32::try_from(
        writer
            .bytes_written()
            .checked_sub(u64::from(central_offset))
            .ok_or(ExportError::InvalidPackage)?,
    )
    .map_err(|_| ExportError::Limit)?;
    let mut end = Vec::with_capacity(22);
    push_u32(&mut end, ZIP_END_SIGNATURE);
    push_u16(&mut end, 0);
    push_u16(&mut end, 0);
    push_u16(&mut end, ZIP_ENTRY_COUNT);
    push_u16(&mut end, ZIP_ENTRY_COUNT);
    push_u32(&mut end, central_size);
    push_u32(&mut end, central_offset);
    push_u16(&mut end, 0);
    writer.observe_transient_buffer(end.capacity())?;
    writer.write_chunk(&end)
}

struct PackageWriter<'a> {
    destination: PinnedExportDestination,
    worker: &'a ExportWorkerLease,
    canaries: &'a LeakCanarySet,
    tail: Vec<u8>,
    bytes_written: u64,
    resident_buffer_bytes: usize,
    peak_buffer_bytes: usize,
}

impl<'a> PackageWriter<'a> {
    fn new(
        destination: PinnedExportDestination,
        worker: &'a ExportWorkerLease,
        canaries: &'a LeakCanarySet,
        resident_buffer_bytes: usize,
        initial_peak_buffer_bytes: usize,
    ) -> Result<Self, ExportError> {
        let tail = Vec::with_capacity(canaries.max_pattern_len().saturating_sub(1));
        let resident_buffer_bytes = resident_buffer_bytes
            .checked_add(tail.capacity())
            .ok_or(ExportError::Limit)?;
        Ok(Self {
            destination,
            worker,
            canaries,
            tail,
            bytes_written: 0,
            resident_buffer_bytes,
            peak_buffer_bytes: initial_peak_buffer_bytes.max(resident_buffer_bytes),
        })
    }

    fn begin_logical_entry(&mut self) {
        self.tail.clear();
    }

    fn write_logical_chunk(
        &mut self,
        bytes: &[u8],
        transient_buffer_bytes: usize,
        scan_raw_canaries: bool,
    ) -> Result<(), ExportError> {
        self.worker.check_cancelled()?;
        let scan_buffer_bytes =
            scan_logical_bytes(self.canaries, &mut self.tail, bytes, scan_raw_canaries)?;
        let transient_buffer_bytes = transient_buffer_bytes
            .checked_add(scan_buffer_bytes)
            .ok_or(ExportError::Limit)?;
        self.observe_transient_buffer(transient_buffer_bytes)?;
        self.write_chunk(bytes)
    }

    fn write_chunk(&mut self, bytes: &[u8]) -> Result<(), ExportError> {
        self.worker.check_cancelled()?;
        self.destination
            .write_all(bytes)
            .map_err(|_| ExportError::Io)?;
        self.bytes_written = self
            .bytes_written
            .checked_add(u64::try_from(bytes.len()).map_err(|_| ExportError::Limit)?)
            .ok_or(ExportError::Limit)?;
        Ok(())
    }

    fn observe_transient_buffer(&mut self, bytes: usize) -> Result<(), ExportError> {
        let working_bytes = self
            .resident_buffer_bytes
            .checked_add(bytes)
            .ok_or(ExportError::Limit)?;
        self.peak_buffer_bytes = self.peak_buffer_bytes.max(working_bytes);
        Ok(())
    }

    const fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    fn complete(mut self) -> Result<(PinnedExportDestination, ExportStats), ExportError> {
        self.worker.check_cancelled()?;
        self.destination
            .sync_data()
            .map_err(|_| ExportError::Boundary)?;
        let stats = ExportStats {
            package_bytes: self.bytes_written,
            peak_buffer_bytes: self.peak_buffer_bytes,
        };
        Ok((self.destination, stats))
    }
}

struct OwnedTemporaryReader<'a> {
    destination: &'a mut PinnedExportDestination,
    position: u64,
    length: u64,
}

impl<'a> OwnedTemporaryReader<'a> {
    fn new(destination: &'a mut PinnedExportDestination) -> Result<Self, ExportError> {
        let length = destination.len().map_err(|_| ExportError::Boundary)?;
        Ok(Self {
            destination,
            position: 0,
            length,
        })
    }
}

impl Read for OwnedTemporaryReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let maximum_bytes = buffer.len().min(EXPORT_BUFFER_BYTES);
        let bytes = self
            .destination
            .read_owned_range(self.position, maximum_bytes)
            .map_err(|_| std::io::Error::other("diagnostic export read failed"))?;
        buffer[..bytes.len()].copy_from_slice(&bytes);
        self.position = self
            .position
            .checked_add(u64::try_from(bytes.len()).map_err(std::io::Error::other)?)
            .ok_or_else(|| std::io::Error::other("diagnostic export read overflow"))?;
        Ok(bytes.len())
    }
}

impl Seek for OwnedTemporaryReader<'_> {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        let next = match position {
            std::io::SeekFrom::Start(position) => i128::from(position),
            std::io::SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
            std::io::SeekFrom::End(delta) => i128::from(self.length) + i128::from(delta),
        };
        if next < 0 || next > i128::from(self.length) {
            return Err(std::io::Error::other("invalid diagnostic export seek"));
        }
        self.position =
            u64::try_from(next).map_err(|_| std::io::Error::other("invalid export seek"))?;
        Ok(self.position)
    }
}

pub fn verify_support_package(path: impl AsRef<Path>) -> Result<(), ExportError> {
    let mut file = File::open(path).map_err(|_| ExportError::Io)?;
    verify_support_package_reader(&mut file, &LeakCanarySet::new(), None).map(|_| ())
}

fn verification_retained_buffer_bytes(
    hash_capacity: usize,
    local_record_capacity: usize,
    byte_buffer_capacities: impl IntoIterator<Item = usize>,
) -> Result<usize, ExportError> {
    let retained = allocated_slots::<[u8; 32]>(hash_capacity)?
        .checked_add(allocated_slots::<(u32, u32, u32)>(local_record_capacity)?)
        .ok_or(ExportError::Limit)?;
    byte_buffer_capacities
        .into_iter()
        .try_fold(retained, |total, capacity| {
            total.checked_add(capacity).ok_or(ExportError::Limit)
        })
}

fn observe_buffer_peak(
    peak: &mut usize,
    retained: usize,
    transient_buffers: impl IntoIterator<Item = usize>,
) -> Result<(), ExportError> {
    let working = transient_buffers
        .into_iter()
        .try_fold(retained, |total, bytes| {
            total.checked_add(bytes).ok_or(ExportError::Limit)
        })?;
    *peak = (*peak).max(working);
    Ok(())
}

fn verify_support_package_reader(
    mut file: &mut (impl Read + Seek),
    canaries: &LeakCanarySet,
    worker: Option<&ExportWorkerLease>,
) -> Result<usize, ExportError> {
    let raw_archive_scan_buffer_bytes = scan_raw_archive_canaries(&mut file, canaries, worker)?;
    let mut hashes = Vec::with_capacity(4);
    let mut local_records = Vec::with_capacity(usize::from(ZIP_ENTRY_COUNT));
    let mut checksum_bytes = Vec::new();
    let mut manifest_bytes = Vec::new();
    let mut configuration_bytes = Vec::new();
    let mut resources_bytes = Vec::new();
    let mut event_line = Vec::new();
    let mut event_count = 0_usize;
    let mut last_event_sequence = None;
    let mut scan_tail = Vec::with_capacity(canaries.max_pattern_len().saturating_sub(1));
    let mut peak_buffer_bytes =
        raw_archive_scan_buffer_bytes.max(verification_retained_buffer_bytes(
            hashes.capacity(),
            local_records.capacity(),
            [
                checksum_bytes.capacity(),
                manifest_bytes.capacity(),
                configuration_bytes.capacity(),
                resources_bytes.capacity(),
                event_line.capacity(),
                scan_tail.capacity(),
            ],
        )?);
    for (index, expected_name) in ENTRY_NAMES.iter().enumerate() {
        check_verification_cancelled(worker)?;
        let offset = u32::try_from(file.stream_position().map_err(|_| ExportError::Io)?)
            .map_err(|_| ExportError::Limit)?;
        let signature = read_u32(&mut file)?;
        if signature != ZIP_LOCAL_SIGNATURE {
            return Err(ExportError::InvalidPackage);
        }
        let header = read_exact_array::<26>(&mut file)?;
        let version_needed = le_u16(&header[0..2]);
        let flags = le_u16(&header[2..4]);
        let compression = le_u16(&header[4..6]);
        let modified_time = le_u16(&header[6..8]);
        let modified_date = le_u16(&header[8..10]);
        let crc_expected = le_u32(&header[10..14]);
        let compressed_size = le_u32(&header[14..18]);
        let size = le_u32(&header[18..22]);
        let name_len = usize::from(le_u16(&header[22..24]));
        let extra_len = usize::from(le_u16(&header[24..26]));
        if version_needed != 20
            || flags != 0
            || compression != 0
            || modified_time != 0
            || modified_date != ZIP_DOS_DATE_1980_01_01
            || compressed_size != size
            || extra_len != 0
            || name_len > 64
            || size_limit(index, size).is_err()
        {
            return Err(ExportError::InvalidPackage);
        }
        let mut name = vec![0_u8; name_len];
        file.read_exact(&mut name)
            .map_err(|_| ExportError::InvalidPackage)?;
        if name != *expected_name {
            return Err(ExportError::InvalidPackage);
        }
        let mut crc = Crc32::new();
        let mut sha = Sha256::new();
        let mut remaining = usize::try_from(size).map_err(|_| ExportError::Limit)?;
        let mut buffer = [0_u8; EXPORT_BUFFER_BYTES];
        scan_tail.clear();
        while remaining > 0 {
            let mut event_validation_transient_bytes = 0_usize;
            check_verification_cancelled(worker)?;
            let read = remaining.min(buffer.len());
            file.read_exact(&mut buffer[..read])
                .map_err(|_| ExportError::InvalidPackage)?;
            crc.update(&buffer[..read]);
            sha.update(&buffer[..read]);
            let scan_buffer_bytes =
                scan_logical_bytes(canaries, &mut scan_tail, &buffer[..read], index == 4)
                    .map_err(|_| ExportError::InvalidPackage)?;
            let retained_buffer_bytes = verification_retained_buffer_bytes(
                hashes.capacity(),
                local_records.capacity(),
                [
                    checksum_bytes.capacity(),
                    manifest_bytes.capacity(),
                    configuration_bytes.capacity(),
                    resources_bytes.capacity(),
                    event_line.capacity(),
                    scan_tail.capacity(),
                ],
            )?;
            observe_buffer_peak(
                &mut peak_buffer_bytes,
                retained_buffer_bytes,
                [
                    buffer.len(),
                    name.capacity(),
                    scan_buffer_bytes,
                    std::mem::size_of_val(&header),
                ],
            )?;
            match index {
                0 => manifest_bytes.extend_from_slice(&buffer[..read]),
                1 => {
                    for byte in &buffer[..read] {
                        if *byte == b'\n' {
                            if event_line.is_empty() {
                                return Err(ExportError::InvalidPackage);
                            }
                            let (sequence, validation_transient_bytes) =
                                validate_canonical_event_encoding(&event_line, canaries)
                                    .map_err(|_| ExportError::InvalidPackage)?;
                            if last_event_sequence.is_some_and(|previous| sequence <= previous) {
                                return Err(ExportError::InvalidPackage);
                            }
                            last_event_sequence = Some(sequence);
                            event_validation_transient_bytes =
                                event_validation_transient_bytes.max(validation_transient_bytes);
                            event_count = event_count.checked_add(1).ok_or(ExportError::Limit)?;
                            event_line.clear();
                        } else {
                            if event_line.len() >= crate::event::MAX_PREPARED_EVENT_BYTES {
                                return Err(ExportError::InvalidPackage);
                            }
                            event_line.push(*byte);
                        }
                    }
                }
                2 => configuration_bytes.extend_from_slice(&buffer[..read]),
                3 => resources_bytes.extend_from_slice(&buffer[..read]),
                4 => checksum_bytes.extend_from_slice(&buffer[..read]),
                _ => return Err(ExportError::InvalidPackage),
            }
            let retained_buffer_bytes = verification_retained_buffer_bytes(
                hashes.capacity(),
                local_records.capacity(),
                [
                    checksum_bytes.capacity(),
                    manifest_bytes.capacity(),
                    configuration_bytes.capacity(),
                    resources_bytes.capacity(),
                    event_line.capacity(),
                    scan_tail.capacity(),
                ],
            )?;
            observe_buffer_peak(
                &mut peak_buffer_bytes,
                retained_buffer_bytes,
                [
                    buffer.len(),
                    name.capacity(),
                    std::mem::size_of_val(&header),
                    event_validation_transient_bytes,
                ],
            )?;
            remaining -= read;
        }
        if index == 1 && !event_line.is_empty() {
            return Err(ExportError::InvalidPackage);
        }
        if crc.finish() != crc_expected {
            return Err(ExportError::InvalidPackage);
        }
        local_records.push((offset, crc_expected, size));
        if index < 4 {
            hashes.push(sha.finalize().into());
        }
        peak_buffer_bytes = peak_buffer_bytes.max(verification_retained_buffer_bytes(
            hashes.capacity(),
            local_records.capacity(),
            [
                checksum_bytes.capacity(),
                manifest_bytes.capacity(),
                configuration_bytes.capacity(),
                resources_bytes.capacity(),
                event_line.capacity(),
                scan_tail.capacity(),
            ],
        )?);
    }
    let expected_checksums = checksum_document_from_hashes(&hashes);
    observe_buffer_peak(
        &mut peak_buffer_bytes,
        verification_retained_buffer_bytes(
            hashes.capacity(),
            local_records.capacity(),
            [
                checksum_bytes.capacity(),
                manifest_bytes.capacity(),
                configuration_bytes.capacity(),
                resources_bytes.capacity(),
                event_line.capacity(),
                scan_tail.capacity(),
            ],
        )?,
        [expected_checksums.capacity()],
    )?;
    if checksum_bytes != expected_checksums {
        return Err(ExportError::InvalidPackage);
    }
    let semantic_json_scan_buffer_bytes = [
        canaries
            .validate_json_document(&manifest_bytes)
            .map_err(|_| ExportError::InvalidPackage)?,
        canaries
            .validate_json_document(&configuration_bytes)
            .map_err(|_| ExportError::InvalidPackage)?,
        canaries
            .validate_json_document(&resources_bytes)
            .map_err(|_| ExportError::InvalidPackage)?,
    ]
    .into_iter()
    .max()
    .unwrap_or(0);
    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).map_err(|_| ExportError::InvalidPackage)?;
    let configuration: ExportConfiguration =
        serde_json::from_slice(&configuration_bytes).map_err(|_| ExportError::InvalidPackage)?;
    let resources: ResourceSummary =
        serde_json::from_slice(&resources_bytes).map_err(|_| ExportError::InvalidPackage)?;
    let decoded_json_allocation_bytes = [
        manifest.allocated_buffer_bytes()?,
        configuration.allocated_buffer_bytes()?,
        resources.allocated_buffer_bytes()?,
    ]
    .into_iter()
    .try_fold(0_usize, |total, bytes| {
        total.checked_add(bytes).ok_or(ExportError::Limit)
    })?;
    let canonical_manifest = json_line(&manifest).map_err(|_| ExportError::InvalidPackage)?;
    let canonical_configuration =
        json_line(&configuration).map_err(|_| ExportError::InvalidPackage)?;
    let canonical_resources = json_line(&resources).map_err(|_| ExportError::InvalidPackage)?;
    let canonical_json_allocation_bytes = [
        canonical_manifest.capacity(),
        canonical_configuration.capacity(),
        canonical_resources.capacity(),
    ]
    .into_iter()
    .try_fold(0_usize, |total, bytes| {
        total.checked_add(bytes).ok_or(ExportError::Limit)
    })?;
    observe_buffer_peak(
        &mut peak_buffer_bytes,
        verification_retained_buffer_bytes(
            hashes.capacity(),
            local_records.capacity(),
            [
                checksum_bytes.capacity(),
                manifest_bytes.capacity(),
                configuration_bytes.capacity(),
                resources_bytes.capacity(),
                event_line.capacity(),
                scan_tail.capacity(),
            ],
        )?,
        [
            expected_checksums.capacity(),
            decoded_json_allocation_bytes,
            canonical_json_allocation_bytes,
            semantic_json_scan_buffer_bytes,
        ],
    )?;
    if manifest.manifest_version != 1
        || manifest.diagnostic_schema_version != 1
        || manifest.event_count != event_count
        || manifest.checksum_algorithm.as_ref() != "sha256"
        || manifest.build != configuration.build
        || (manifest.truncated && manifest.omitted_event_count == 0)
        || (!manifest.truncated && manifest.omitted_event_count != 0)
        || canonical_manifest != manifest_bytes
        || canonical_configuration != configuration_bytes
        || canonical_resources != resources_bytes
    {
        return Err(ExportError::InvalidPackage);
    }
    drop(canonical_manifest);
    drop(canonical_configuration);
    drop(canonical_resources);
    drop(manifest);
    drop(configuration);
    drop(resources);

    let central_offset = u32::try_from(file.stream_position().map_err(|_| ExportError::Io)?)
        .map_err(|_| ExportError::Limit)?;
    for (index, expected_name) in ENTRY_NAMES.iter().enumerate() {
        check_verification_cancelled(worker)?;
        if read_u32(&mut file)? != ZIP_CENTRAL_SIGNATURE {
            return Err(ExportError::InvalidPackage);
        }
        let header = read_exact_array::<42>(&mut file)?;
        let name_len = usize::from(le_u16(&header[24..26]));
        let extra_len = le_u16(&header[26..28]);
        let comment_len = le_u16(&header[28..30]);
        let crc32 = le_u32(&header[12..16]);
        let compressed_size = le_u32(&header[16..20]);
        let size = le_u32(&header[20..24]);
        let local_offset = le_u32(&header[38..42]);
        let local = local_records[index];
        if le_u16(&header[0..2]) != 20
            || le_u16(&header[2..4]) != 20
            || name_len > 64
            || extra_len != 0
            || comment_len != 0
            || le_u16(&header[4..6]) != 0
            || le_u16(&header[6..8]) != 0
            || le_u16(&header[8..10]) != 0
            || le_u16(&header[10..12]) != ZIP_DOS_DATE_1980_01_01
            || le_u16(&header[30..32]) != 0
            || le_u16(&header[32..34]) != 0
            || le_u32(&header[34..38]) != 0
            || compressed_size != size
            || local_offset != local.0
            || crc32 != local.1
            || size != local.2
        {
            return Err(ExportError::InvalidPackage);
        }
        let mut name = vec![0_u8; name_len];
        observe_buffer_peak(
            &mut peak_buffer_bytes,
            verification_retained_buffer_bytes(
                hashes.capacity(),
                local_records.capacity(),
                [
                    checksum_bytes.capacity(),
                    manifest_bytes.capacity(),
                    configuration_bytes.capacity(),
                    resources_bytes.capacity(),
                    event_line.capacity(),
                    scan_tail.capacity(),
                ],
            )?,
            [
                expected_checksums.capacity(),
                name.capacity(),
                std::mem::size_of_val(&header),
            ],
        )?;
        file.read_exact(&mut name)
            .map_err(|_| ExportError::InvalidPackage)?;
        if name != *expected_name {
            return Err(ExportError::InvalidPackage);
        }
    }
    check_verification_cancelled(worker)?;
    let central_end = file.stream_position().map_err(|_| ExportError::Io)?;
    if read_u32(&mut file)? != ZIP_END_SIGNATURE {
        return Err(ExportError::InvalidPackage);
    }
    let end = read_exact_array::<18>(&mut file)?;
    if le_u16(&end[0..2]) != 0
        || le_u16(&end[2..4]) != 0
        || le_u16(&end[4..6]) != ZIP_ENTRY_COUNT
        || le_u16(&end[6..8]) != ZIP_ENTRY_COUNT
        || le_u32(&end[12..16]) != central_offset
        || le_u16(&end[16..18]) != 0
        || u64::from(le_u32(&end[8..12])) != central_end - u64::from(central_offset)
    {
        return Err(ExportError::InvalidPackage);
    }
    let mut trailing = [0_u8; 1];
    if file.read(&mut trailing).map_err(|_| ExportError::Io)? != 0 {
        return Err(ExportError::InvalidPackage);
    }
    Ok(peak_buffer_bytes)
}

fn check_verification_cancelled(worker: Option<&ExportWorkerLease>) -> Result<(), ExportError> {
    worker.map_or(Ok(()), ExportWorkerLease::check_cancelled)
}

fn checksum_document_from_hashes(hashes: &[[u8; 32]]) -> Vec<u8> {
    let mut document = Vec::with_capacity(hashes.len() * 96);
    for (hash, name) in hashes.iter().zip(ENTRY_NAMES.iter().take(4)) {
        document.extend_from_slice(&hex_sha256(*hash));
        document.extend_from_slice(b"  ");
        document.extend_from_slice(name);
        document.push(b'\n');
    }
    document
}

fn size_limit(index: usize, size: u32) -> Result<(), ExportError> {
    let maximum = if index == 1 {
        MAX_EXPORT_EVENT_BYTES
    } else if index == 4 {
        1_024
    } else {
        EXPORT_BUFFER_BYTES as u64
    };
    if u64::from(size) > maximum {
        Err(ExportError::Limit)
    } else {
        Ok(())
    }
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    needle.len() <= haystack.len()
        && haystack
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
}

fn contains_forbidden_key(bytes: &[u8]) -> bool {
    FIXED_FORBIDDEN_KEYS.iter().any(|key| {
        bytes.windows(key.len().saturating_add(2)).any(|window| {
            window.first() == Some(&b'"')
                && window.last() == Some(&b'"')
                && window[1..window.len() - 1].eq_ignore_ascii_case(key)
        })
    })
}

fn contains_forbidden_location(bytes: &[u8]) -> bool {
    const LOCATION_MARKERS: [&[u8]; 9] = [
        b"file://",
        b"http://",
        b"https://",
        b"%2f",
        b"%5c",
        b"%3a",
        b"%40",
        b"%3f",
        b"%25",
    ];
    if LOCATION_MARKERS
        .iter()
        .any(|marker| contains_ascii_case_insensitive(bytes, marker))
    {
        return true;
    }
    for (index, byte) in bytes.iter().copied().enumerate() {
        if byte != b'"' || index + 1 >= bytes.len() {
            continue;
        }
        let value = &bytes[index + 1..];
        if value.starts_with(b"/")
            || value.starts_with(br"\\\\")
            || (value.len() >= 4
                && value[0].is_ascii_alphabetic()
                && value[1] == b':'
                && (value[2] == b'/' || (value[2] == b'\\' && value[3] == b'\\')))
        {
            return true;
        }
    }
    false
}

fn contains_decoded_forbidden_location(value: &str) -> bool {
    const LOCATION_MARKERS: [&[u8]; 9] = [
        b"file://",
        b"http://",
        b"https://",
        b"%2f",
        b"%5c",
        b"%3a",
        b"%40",
        b"%3f",
        b"%25",
    ];
    let bytes = value.as_bytes();
    LOCATION_MARKERS
        .iter()
        .any(|marker| contains_ascii_case_insensitive(bytes, marker))
        || bytes.starts_with(b"/")
        || bytes.starts_with(br"\\")
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'))
}

fn scan_raw_archive_canaries(
    file: &mut (impl Read + Seek),
    canaries: &LeakCanarySet,
    worker: Option<&ExportWorkerLease>,
) -> Result<usize, ExportError> {
    file.seek(std::io::SeekFrom::Start(0))
        .map_err(|_| ExportError::Io)?;
    if canaries.patterns.is_empty() {
        return Ok(0);
    }
    let mut buffer = [0_u8; EXPORT_BUFFER_BYTES];
    let mut tail = Vec::with_capacity(canaries.max_raw_canary_len().saturating_sub(1));
    let mut peak_buffer_bytes = buffer.len().saturating_add(tail.capacity());
    loop {
        check_verification_cancelled(worker)?;
        let read = file
            .read(&mut buffer)
            .map_err(|_| ExportError::InvalidPackage)?;
        if read == 0 {
            break;
        }
        let mut scan = Vec::with_capacity(tail.len().saturating_add(read));
        scan.extend_from_slice(&tail);
        scan.extend_from_slice(&buffer[..read]);
        peak_buffer_bytes = peak_buffer_bytes.max(
            buffer
                .len()
                .saturating_add(tail.capacity())
                .saturating_add(scan.capacity()),
        );
        if canaries.contains_raw_canary(&scan) {
            return Err(ExportError::LeakDetected);
        }
        let tail_len = canaries.max_raw_canary_len().saturating_sub(1);
        tail.clear();
        let start = scan.len().saturating_sub(tail_len);
        tail.extend_from_slice(&scan[start..]);
    }
    file.seek(std::io::SeekFrom::Start(0))
        .map_err(|_| ExportError::Io)?;
    Ok(peak_buffer_bytes)
}

fn scan_logical_bytes(
    canaries: &LeakCanarySet,
    tail: &mut Vec<u8>,
    bytes: &[u8],
    scan_raw_canaries: bool,
) -> Result<usize, ExportError> {
    let mut scan = Vec::with_capacity(tail.len().saturating_add(bytes.len()));
    scan.extend_from_slice(tail);
    scan.extend_from_slice(bytes);
    if canaries.contains_fixed_forbidden(&scan)
        || (scan_raw_canaries && canaries.contains_raw_canary(&scan))
    {
        return Err(ExportError::LeakDetected);
    }
    let tail_len = canaries.max_pattern_len().saturating_sub(1);
    tail.clear();
    if tail_len > 0 {
        let start = scan.len().saturating_sub(tail_len);
        tail.extend_from_slice(&scan[start..]);
    }
    Ok(scan.capacity())
}

fn hex_sha256(hash: [u8; 32]) -> [u8; 64] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = [0_u8; 64];
    for (index, byte) in hash.into_iter().enumerate() {
        encoded[index * 2] = HEX[usize::from(byte >> 4)];
        encoded[index * 2 + 1] = HEX[usize::from(byte & 0x0f)];
    }
    encoded
}

struct Crc32(u32);

impl Crc32 {
    const fn new() -> Self {
        Self(u32::MAX)
    }

    fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = 0_u32.wrapping_sub(self.0 & 1);
                self.0 = (self.0 >> 1) ^ (0xedb8_8320 & mask);
            }
        }
    }

    const fn finish(self) -> u32 {
        !self.0
    }
}

fn push_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn read_u32(reader: &mut impl Read) -> Result<u32, ExportError> {
    Ok(u32::from_le_bytes(read_exact_array(reader)?))
}

fn read_exact_array<const N: usize>(reader: &mut impl Read) -> Result<[u8; N], ExportError> {
    let mut bytes = [0_u8; N];
    reader
        .read_exact(&mut bytes)
        .map_err(|_| ExportError::InvalidPackage)?;
    Ok(bytes)
}

fn le_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{Read, Seek},
    };

    use tempfile::tempdir;

    use super::*;

    struct CancellingReader {
        file: File,
        owner: Option<ExportRequestOwner>,
    }

    impl Read for CancellingReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let read = self.file.read(buffer)?;
            if read != 0 {
                drop(self.owner.take());
            }
            Ok(read)
        }
    }

    impl Seek for CancellingReader {
        fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
            self.file.seek(position)
        }
    }

    fn test_package() -> SupportPackage {
        SupportPackage::new(
            Vec::new(),
            ExportConfiguration::new(
                true,
                7,
                4,
                ExportBuildIdentity::new("0.1.0", "0123456789abcdef0123456789abcdef01234567", 1, 3)
                    .unwrap(),
                CapabilitySummary::new(vec![ExportCapability::DiagnosticsExport]).unwrap(),
            )
            .unwrap(),
            ResourceSummary::new(
                0,
                0,
                0,
                ExportPlatformCategory::None,
                Vec::new(),
                ExportRedactionCounters::new(0, 0, 0, 0, 0, 0),
            )
            .unwrap(),
            ExportSelection::complete(),
        )
        .unwrap()
    }

    #[test]
    fn persistent_source_metadata_and_write_passes_read_each_window_once() {
        use crate::event::{
            BuildIdentity, CapabilityVersion, DiagnosticComponent, DiagnosticEventCode,
            DiagnosticEventDraft, DiagnosticLevel, EventId, GitCommit, UtcTimestamp,
            WokcoreVersion,
        };
        use wokcore_platform::diagnostics::DiagnosticDirectory;

        let fixture = tempdir().unwrap();
        let diagnostics = fixture.path().join("diagnostics");
        let exports = fixture.path().join("exports");
        fs::create_dir(&diagnostics).unwrap();
        fs::create_dir(&exports).unwrap();
        let mut source = Vec::new();
        for sequence in 1..=96_u64 {
            let draft = DiagnosticEventDraft::new(
                EventId::parse(&format!("018f47a2-4c1d-7a8f-9b2d-{sequence:012x}")).unwrap(),
                UtcTimestamp::parse("2026-07-26T12:30:00Z").unwrap(),
                DiagnosticLevel::Info,
                DiagnosticComponent::Diagnostics,
                DiagnosticEventCode::RequestCompleted,
                BuildIdentity::new(
                    WokcoreVersion::parse("0.1.0").unwrap(),
                    GitCommit::parse("0123456789abcdef0123456789abcdef01234567").unwrap(),
                    1,
                    CapabilityVersion::new(3),
                ),
            );
            let event = draft
                .prepare_template()
                .unwrap()
                .finalize(sequence)
                .unwrap();
            source.extend_from_slice(event.encoded());
            source.push(b'\n');
        }
        assert!(source.len() > 2 * EXPORT_BUFFER_BYTES);
        let directory = DiagnosticDirectory::open(&diagnostics).unwrap();
        drop(
            directory
                .create_new(
                    "segment-00000000000000000001.jsonl".as_ref(),
                    &source,
                    u64::try_from(crate::segment::MAX_SEGMENT_BYTES).unwrap(),
                )
                .unwrap(),
        );
        let lease = directory
            .open_name_read(
                "segment-00000000000000000001.jsonl".as_ref(),
                u64::try_from(crate::segment::MAX_SEGMENT_BYTES).unwrap(),
            )
            .unwrap();
        let mut package = test_package();
        package.events.push(lease);
        let coordinator = ExportCoordinator::new();
        let operation = coordinator.try_begin().unwrap();
        let worker = operation.start_worker().unwrap();
        let destination = exports.join("package.zip");
        PERSISTENT_READ_RANGE_CALLS.with(|calls| calls.set(0));
        export_support_package(
            worker,
            PinnedExportDestination::create(&destination, &[]).unwrap(),
            &mut package,
            &LeakCanarySet::new(),
        )
        .unwrap();
        let read_calls = PERSISTENT_READ_RANGE_CALLS.with(Cell::get);
        let chunks_per_pass = source.len().div_ceil(EXPORT_BUFFER_BYTES);

        assert!(
            read_calls <= chunks_per_pass * 2,
            "metadata and write passes used {read_calls} read_range calls for {chunks_per_pass} windows per pass"
        );
    }

    #[test]
    fn final_verifier_observes_request_drop_before_its_next_bounded_read() {
        let fixture = tempdir().unwrap();
        let exports = fixture.path().join("exports");
        fs::create_dir(&exports).unwrap();
        let path = exports.join("fixture.zip");
        let coordinator = ExportCoordinator::new();
        let operation = coordinator.try_begin().unwrap();
        let worker = operation.start_worker().unwrap();
        let mut package = test_package();
        export_support_package(
            worker,
            PinnedExportDestination::create(&path, &[]).unwrap(),
            &mut package,
            &LeakCanarySet::new(),
        )
        .unwrap();
        drop(operation);

        let verification = coordinator.try_begin().unwrap();
        let (owner, worker) = verification.split().unwrap();
        let mut reader = CancellingReader {
            file: File::open(&path).unwrap(),
            owner: Some(owner),
        };
        assert_eq!(
            verify_support_package_reader(&mut reader, &LeakCanarySet::new(), Some(&worker),)
                .unwrap_err(),
            ExportError::Cancelled
        );
        assert_eq!(coordinator.try_begin().unwrap_err(), ExportError::Busy);
        drop(worker);
        assert!(coordinator.try_begin().is_ok());
    }

    #[test]
    fn persistent_source_limits_bound_total_scan_and_snapshot_count() {
        let mut aggregate = PersistentSourceLimits::default();
        for _ in 0..24 {
            aggregate
                .observe(PersistentSourceKind::Segment, 4 * 1024 * 1024)
                .unwrap();
        }
        assert_eq!(
            aggregate
                .observe(PersistentSourceKind::Segment, 1)
                .unwrap_err(),
            ExportError::Limit
        );

        let mut snapshots = PersistentSourceLimits::default();
        for _ in 0..crate::snapshot::MAX_FAILURE_SNAPSHOTS {
            snapshots
                .observe(PersistentSourceKind::Snapshot, 1)
                .unwrap();
        }
        assert_eq!(
            snapshots
                .observe(PersistentSourceKind::Snapshot, 1)
                .unwrap_err(),
            ExportError::Limit
        );
        assert_eq!(
            PersistentSourceLimits::default()
                .observe(
                    PersistentSourceKind::Segment,
                    u64::try_from(crate::segment::MAX_SEGMENT_BYTES).unwrap() + 1,
                )
                .unwrap_err(),
            ExportError::Limit
        );
        assert_eq!(
            PersistentSourceLimits::default()
                .observe(
                    PersistentSourceKind::Snapshot,
                    u64::try_from(crate::snapshot::MAX_FAILURE_SNAPSHOT_BYTES).unwrap() + 1,
                )
                .unwrap_err(),
            ExportError::Limit
        );
    }

    #[test]
    fn verifier_reports_a_final_raw_archive_canary_match() {
        let fixture = tempdir().unwrap();
        let exports = fixture.path().join("exports");
        fs::create_dir(&exports).unwrap();
        let path = exports.join("fixture.zip");
        let coordinator = ExportCoordinator::new();
        let operation = coordinator.try_begin().unwrap();
        let worker = operation.start_worker().unwrap();
        export_support_package(
            worker,
            PinnedExportDestination::create(&path, &[]).unwrap(),
            &mut test_package(),
            &LeakCanarySet::new(),
        )
        .unwrap();
        drop(operation);
        let mut canaries = LeakCanarySet::new();
        canaries.push(b"manifest_version").unwrap();
        let mut file = File::open(&path).unwrap();

        assert_eq!(
            verify_support_package_reader(&mut file, &canaries, None).unwrap_err(),
            ExportError::LeakDetected
        );
    }

    #[test]
    fn raw_canary_scan_detects_a_match_split_across_bounded_chunks() {
        let mut canaries = LeakCanarySet::new();
        canaries.push(b"split-sensitive-canary").unwrap();
        let mut tail = Vec::new();

        scan_logical_bytes(&canaries, &mut tail, b"safe split-sensitive-", true).unwrap();
        assert_eq!(
            scan_logical_bytes(&canaries, &mut tail, b"canary safe", true).unwrap_err(),
            ExportError::LeakDetected
        );
    }
}
