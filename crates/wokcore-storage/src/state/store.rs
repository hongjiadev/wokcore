use std::{
    fmt,
    fs::{self, OpenOptions},
    io,
    path::{Path, PathBuf},
    str::FromStr,
};

use fs4::fs_std::FileExt;
use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, TransactionBehavior, params};
use wokcore_core::{id::ClientId, secret::SecretRef};

use crate::StorageError;

use super::wal;

const INITIAL_MIGRATION: &str = include_str!("../../migrations/0001_initial.sql");
const RUNTIME_AUTH_MIGRATION: &str = include_str!("../../migrations/0002_runtime_auth.sql");
const SESSION_DIAGNOSTICS_MIGRATION: &str =
    include_str!("../../migrations/0003_session_diagnostics.sql");
const LATEST_SCHEMA_VERSION: i64 = 3;
const GLOBAL_CURRENT_SESSION_INDEX_FIRST_PAGE_SQL: &str =
    "SELECT i.session_key, i.source_key, i.generation, i.source_kind,
            i.created_at, i.last_active_at, i.message_count,
            i.usage_event_count,
            CASE WHEN s.status = 'available'
                 THEN i.availability ELSE 'unavailable' END
     FROM session_sources s
     CROSS JOIN session_index i INDEXED BY session_index_current_order
     WHERE i.source_key = s.source_key
       AND i.generation = s.current_generation
     ORDER BY i.last_active_at DESC, i.session_key, i.source_key
     LIMIT ?1";
const GLOBAL_CURRENT_SESSION_INDEX_AFTER_PAGE_SQL: &str =
    "SELECT i.session_key, i.source_key, i.generation, i.source_kind,
            i.created_at, i.last_active_at, i.message_count,
            i.usage_event_count,
            CASE WHEN s.status = 'available'
                 THEN i.availability ELSE 'unavailable' END
     FROM session_sources s
     CROSS JOIN session_index i INDEXED BY session_index_current_order
     WHERE i.source_key = s.source_key
       AND i.generation = s.current_generation
       AND (i.last_active_at < ?1
         OR (i.last_active_at = ?1 AND i.session_key > ?2)
         OR (i.last_active_at = ?1 AND i.session_key = ?2
             AND i.source_key > ?3))
     ORDER BY i.last_active_at DESC, i.session_key, i.source_key
     LIMIT ?4";
const GLOBAL_CURRENT_SESSION_USAGE_FIRST_PAGE_SQL: &str =
    "SELECT u.usage_id, u.session_key, u.source_key, u.generation,
            u.source_kind, u.model, u.occurred_at, u.input_tokens,
            u.output_tokens, u.cache_read_tokens, u.cache_write_tokens,
            u.reasoning_tokens, u.record_revision
     FROM session_sources s
     CROSS JOIN session_usage_records u INDEXED BY session_usage_current_order
     WHERE u.source_key = s.source_key
       AND u.generation = s.current_generation
     ORDER BY u.occurred_at, u.usage_id, u.source_key
     LIMIT ?1";
const GLOBAL_CURRENT_SESSION_USAGE_AFTER_PAGE_SQL: &str =
    "SELECT u.usage_id, u.session_key, u.source_key, u.generation,
            u.source_kind, u.model, u.occurred_at, u.input_tokens,
            u.output_tokens, u.cache_read_tokens, u.cache_write_tokens,
            u.reasoning_tokens, u.record_revision
     FROM session_sources s
     CROSS JOIN session_usage_records u INDEXED BY session_usage_current_order
     WHERE u.source_key = s.source_key
       AND u.generation = s.current_generation
       AND (u.occurred_at > ?1
         OR (u.occurred_at = ?1 AND u.usage_id > ?2)
         OR (u.occurred_at = ?1 AND u.usage_id = ?2
             AND u.source_key > ?3))
     ORDER BY u.occurred_at, u.usage_id, u.source_key
     LIMIT ?4";

pub const WAL_CHECKPOINT_THRESHOLD_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestMetric {
    pub request_id: String,
    pub provider_id: String,
    pub model: String,
    pub started_at: String,
    pub latency_ms: i64,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub status_code: i64,
    pub error_code: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateHealth {
    pub schema_version: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckpointResult {
    pub busy: bool,
    pub log_frames: i64,
    pub checkpointed_frames: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSecretBinding {
    pub name: String,
    pub secret_ref: SecretRef,
    pub revision: u64,
    pub created_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientTokenMetadata {
    pub token_id: String,
    pub client_id: ClientId,
    pub digest: [u8; 32],
    pub issued_at: String,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ClientTokenScope {
    ProxyUse,
    SessionsRead,
    UsageRead,
    DiagnosticsRead,
    DiagnosticsExport,
}

impl ClientTokenScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProxyUse => "proxy.use",
            Self::SessionsRead => "sessions.read",
            Self::UsageRead => "usage.read",
            Self::DiagnosticsRead => "diagnostics.read",
            Self::DiagnosticsExport => "diagnostics.export",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::ProxyUse => 0,
            Self::SessionsRead => 1,
            Self::UsageRead => 2,
            Self::DiagnosticsRead => 3,
            Self::DiagnosticsExport => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientTokenScopeParseError;

impl fmt::Display for ClientTokenScopeParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("unknown client token scope")
    }
}

impl std::error::Error for ClientTokenScopeParseError {}

impl FromStr for ClientTokenScope {
    type Err = ClientTokenScopeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "proxy.use" => Ok(Self::ProxyUse),
            "sessions.read" => Ok(Self::SessionsRead),
            "usage.read" => Ok(Self::UsageRead),
            "diagnostics.read" => Ok(Self::DiagnosticsRead),
            "diagnostics.export" => Ok(Self::DiagnosticsExport),
            _ => Err(ClientTokenScopeParseError),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopedClientTokenMetadata {
    pub token: ClientTokenMetadata,
    pub scopes: Vec<ClientTokenScope>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionSourceKind {
    Codex,
    Claude,
    Gemini,
}

impl SessionSourceKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Gemini => "gemini",
        }
    }

    fn from_database(value: &str) -> Result<Self, StorageError> {
        match value {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            "gemini" => Ok(Self::Gemini),
            _ => Err(StorageError::StateDatabaseCorrupt {
                message: "session state contains an invalid source kind".to_owned(),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionGenerationState {
    Staging,
    Current,
    Retired,
}

impl SessionGenerationState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Staging => "staging",
            Self::Current => "current",
            Self::Retired => "retired",
        }
    }

    fn from_database(value: &str) -> Result<Self, StorageError> {
        match value {
            "staging" => Ok(Self::Staging),
            "current" => Ok(Self::Current),
            "retired" => Ok(Self::Retired),
            _ => Err(StorageError::StateDatabaseCorrupt {
                message: "session cursor contains an invalid generation state".to_owned(),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionAvailability {
    Available,
    Unavailable,
}

impl SessionAvailability {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Unavailable => "unavailable",
        }
    }

    fn from_database(value: &str) -> Result<Self, StorageError> {
        match value {
            "available" => Ok(Self::Available),
            "unavailable" => Ok(Self::Unavailable),
            _ => Err(StorageError::StateDatabaseCorrupt {
                message: "session index contains an invalid availability".to_owned(),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionSourceStatus {
    Undiscovered,
    Available,
    Stale,
    Unavailable,
    ResourceLimited,
}

impl SessionSourceStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Undiscovered => "undiscovered",
            Self::Available => "available",
            Self::Stale => "stale",
            Self::Unavailable => "unavailable",
            Self::ResourceLimited => "resource_limited",
        }
    }

    fn from_database(value: &str) -> Result<Self, StorageError> {
        match value {
            "undiscovered" => Ok(Self::Undiscovered),
            "available" => Ok(Self::Available),
            "stale" => Ok(Self::Stale),
            "unavailable" => Ok(Self::Unavailable),
            "resource_limited" => Ok(Self::ResourceLimited),
            _ => Err(StorageError::StateDatabaseCorrupt {
                message: "Session source contains an invalid status".to_owned(),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionSourceErrorCode {
    SourceRootMissing,
    SourceRootUnreadable,
    SourceSessionsAbsent,
    SourceEntryUnsafe,
    SourceIoFailed,
    SourceParseInvalid,
    SourceRecordTooLarge,
    SourceReplayParentMissing,
    SourceReplayParentAmbiguous,
    SourceReplayInconsistent,
    SourceReplayLimit,
    SourceCandidateInterrupted,
}

impl SessionSourceErrorCode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::SourceRootMissing => "source_root_missing",
            Self::SourceRootUnreadable => "source_root_unreadable",
            Self::SourceSessionsAbsent => "source_sessions_absent",
            Self::SourceEntryUnsafe => "source_entry_unsafe",
            Self::SourceIoFailed => "source_io_failed",
            Self::SourceParseInvalid => "source_parse_invalid",
            Self::SourceRecordTooLarge => "source_record_too_large",
            Self::SourceReplayParentMissing => "source_replay_parent_missing",
            Self::SourceReplayParentAmbiguous => "source_replay_parent_ambiguous",
            Self::SourceReplayInconsistent => "source_replay_inconsistent",
            Self::SourceReplayLimit => "source_replay_limit",
            Self::SourceCandidateInterrupted => "source_candidate_interrupted",
        }
    }

    fn from_database(value: &str) -> Result<Self, StorageError> {
        match value {
            "source_root_missing" => Ok(Self::SourceRootMissing),
            "source_root_unreadable" => Ok(Self::SourceRootUnreadable),
            "source_sessions_absent" => Ok(Self::SourceSessionsAbsent),
            "source_entry_unsafe" => Ok(Self::SourceEntryUnsafe),
            "source_io_failed" => Ok(Self::SourceIoFailed),
            "source_parse_invalid" => Ok(Self::SourceParseInvalid),
            "source_record_too_large" => Ok(Self::SourceRecordTooLarge),
            "source_replay_parent_missing" => Ok(Self::SourceReplayParentMissing),
            "source_replay_parent_ambiguous" => Ok(Self::SourceReplayParentAmbiguous),
            "source_replay_inconsistent" => Ok(Self::SourceReplayInconsistent),
            "source_replay_limit" => Ok(Self::SourceReplayLimit),
            "source_candidate_interrupted" => Ok(Self::SourceCandidateInterrupted),
            _ => Err(StorageError::StateDatabaseCorrupt {
                message: "Session source contains an invalid error code".to_owned(),
            }),
        }
    }

    const fn is_resource_limit(self) -> bool {
        matches!(self, Self::SourceRecordTooLarge | Self::SourceReplayLimit)
    }
}

macro_rules! correlation_id {
    ($name:ident, $label:literal) => {
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, StorageError> {
                let value = value.into();
                validate_correlation_id($label, &value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            fn from_database(value: String) -> Result<Self, StorageError> {
                Self::new(value).map_err(|_| StorageError::StateDatabaseCorrupt {
                    message: concat!($label, " is invalid").to_owned(),
                })
            }
        }
    };
}

correlation_id!(RequestId, "request identifier");
correlation_id!(AttemptId, "attempt identifier");
correlation_id!(TraceId, "trace identifier");

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpaqueFingerprint(String);

impl OpaqueFingerprint {
    pub fn new(value: impl Into<String>) -> Result<Self, StorageError> {
        let value = value.into();
        validate_opaque_key("fingerprint", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn from_database(value: String) -> Result<Self, StorageError> {
        Self::new(value).map_err(|_| StorageError::StateDatabaseCorrupt {
            message: "supplemental metadata contains an invalid fingerprint".to_owned(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionFileIdentity(String);

impl SessionFileIdentity {
    pub fn new(value: impl Into<String>) -> Result<Self, StorageError> {
        let value = value.into();
        validate_opaque_key("file identity", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn from_database(value: String) -> Result<Self, StorageError> {
        Self::new(value).map_err(|_| StorageError::StateDatabaseCorrupt {
            message: "Session scan cursor contains an invalid file identity".to_owned(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionScanResultCode {
    Advanced,
    Unchanged,
    Deferred,
}

impl SessionScanResultCode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Advanced => "advanced",
            Self::Unchanged => "unchanged",
            Self::Deferred => "deferred",
        }
    }

    fn from_database(value: &str) -> Result<Self, StorageError> {
        match value {
            "advanced" => Ok(Self::Advanced),
            "unchanged" => Ok(Self::Unchanged),
            "deferred" => Ok(Self::Deferred),
            _ => Err(StorageError::StateDatabaseCorrupt {
                message: "Session scan cursor contains an invalid result code".to_owned(),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupplementalRetryDecision {
    None,
    Retry,
    Exhausted,
}

impl SupplementalRetryDecision {
    const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Retry => "retry",
            Self::Exhausted => "exhausted",
        }
    }

    fn from_database(value: &str) -> Result<Self, StorageError> {
        match value {
            "none" => Ok(Self::None),
            "retry" => Ok(Self::Retry),
            "exhausted" => Ok(Self::Exhausted),
            _ => Err(StorageError::StateDatabaseCorrupt {
                message: "supplemental metadata contains an invalid retry decision".to_owned(),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupplementalFailoverDecision {
    None,
    Failover,
    Unavailable,
}

impl SupplementalFailoverDecision {
    const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Failover => "failover",
            Self::Unavailable => "unavailable",
        }
    }

    fn from_database(value: &str) -> Result<Self, StorageError> {
        match value {
            "none" => Ok(Self::None),
            "failover" => Ok(Self::Failover),
            "unavailable" => Ok(Self::Unavailable),
            _ => Err(StorageError::StateDatabaseCorrupt {
                message: "supplemental metadata contains an invalid failover decision".to_owned(),
            }),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SupplementalErrorCode(String);

impl SupplementalErrorCode {
    pub fn new(value: impl Into<String>) -> Result<Self, StorageError> {
        let value = value.into();
        validate_stable_code("supplemental error code", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn from_database(value: String) -> Result<Self, StorageError> {
        Self::new(value).map_err(|_| StorageError::StateDatabaseCorrupt {
            message: "supplemental metadata contains an invalid error code".to_owned(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSourceState {
    pub source_key: String,
    pub source_kind: SessionSourceKind,
    pub current_generation: Option<u64>,
    pub staging_generation: Option<u64>,
    pub retired_generation: Option<u64>,
    pub status: SessionSourceStatus,
    pub error_code: Option<SessionSourceErrorCode>,
    pub last_transition_at: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CleanupBatchOutcome {
    pub deleted_rows: usize,
    pub deleted_bytes: usize,
    pub complete: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupplementalBatchOutcome {
    pub inserted_rows: usize,
    pub dropped_rows: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SupplementalStorageStats {
    pub rows: usize,
    pub logical_bytes: usize,
    pub oldest_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParserCheckpoint {
    pub version: u16,
    pub previous_input_tokens: u64,
    pub previous_output_tokens: u64,
    pub previous_cache_read_tokens: u64,
    pub previous_cache_write_tokens: u64,
    pub previous_reasoning_tokens: u64,
    pub current_model: Option<String>,
    pub event_ordinal: u64,
    pub lineage_source_key: Option<String>,
    pub lineage_generation: Option<u64>,
    pub lineage_record_ordinal: u64,
    pub structural_hash: Option<[u8; 32]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionScanCursor {
    pub source_key: String,
    pub source_kind: SessionSourceKind,
    pub generation: u64,
    pub generation_state: SessionGenerationState,
    pub file_identity: SessionFileIdentity,
    pub observed_size: u64,
    pub modified_at: String,
    pub complete_byte_offset: u64,
    pub stable_record_ordinal: u64,
    pub parser_checkpoint: ParserCheckpoint,
    pub head_fingerprint: [u8; 32],
    pub boundary_fingerprint: [u8; 32],
    pub parent_source_key: Option<String>,
    pub parent_generation: Option<u64>,
    pub replay_boundary_fingerprint: Option<[u8; 32]>,
    pub result_code: Option<SessionScanResultCode>,
    pub result_changed_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionIndexRecord {
    pub session_key: String,
    pub source_key: String,
    pub generation: u64,
    pub source_kind: SessionSourceKind,
    pub created_at: String,
    pub last_active_at: String,
    pub message_count: u64,
    pub usage_event_count: u64,
    pub availability: SessionAvailability,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionUsageRecord {
    pub usage_id: String,
    pub session_key: String,
    pub source_key: String,
    pub generation: u64,
    pub source_kind: SessionSourceKind,
    pub model: String,
    pub occurred_at: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub record_revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexReplaySignature {
    pub parent_source_key: String,
    pub parent_generation: u64,
    pub token_event_ordinal: u64,
    pub occurred_at: String,
    pub signature_hash: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestSupplementalMetadata {
    pub request_id: RequestId,
    pub attempt_id: AttemptId,
    pub trace_id: TraceId,
    pub occurred_at: String,
    pub route_fingerprint: OpaqueFingerprint,
    pub provider_fingerprint: OpaqueFingerprint,
    pub account_fingerprint: Option<OpaqueFingerprint>,
    pub retry_decision: SupplementalRetryDecision,
    pub failover_decision: SupplementalFailoverDecision,
    pub queue_ms: u64,
    pub connect_ms: u64,
    pub first_byte_ms: u64,
    pub total_ms: u64,
    pub request_bytes: u64,
    pub response_bytes: u64,
    pub status_code: Option<u16>,
    pub error_code: Option<SupplementalErrorCode>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionBatch {
    pub cursor: Option<SessionScanCursor>,
    pub index_records: Vec<SessionIndexRecord>,
    pub usage_records: Vec<SessionUsageRecord>,
    pub replay_signatures: Vec<CodexReplaySignature>,
    pub supplemental_metadata: Vec<RequestSupplementalMetadata>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CandidateBeginOutcome {
    Started,
    Resumed(Box<SessionScanCursor>),
    CleanupRequired { generation: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionIndexPageKey {
    source_key: String,
    generation: u64,
    last_active_at: String,
    session_key: String,
}

impl SessionIndexPageKey {
    pub fn new(
        source_key: impl Into<String>,
        generation: u64,
        last_active_at: impl Into<String>,
        session_key: impl Into<String>,
    ) -> Result<Self, StorageError> {
        let key = Self {
            source_key: source_key.into(),
            generation,
            last_active_at: last_active_at.into(),
            session_key: session_key.into(),
        };
        validate_opaque_key("source key", &key.source_key)?;
        validate_generation(key.generation)?;
        validate_timestamp("Session page activity timestamp", &key.last_active_at)?;
        validate_opaque_key("session key", &key.session_key)?;
        Ok(key)
    }

    pub fn source_key(&self) -> &str {
        &self.source_key
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn last_active_at(&self) -> &str {
        &self.last_active_at
    }

    pub fn session_key(&self) -> &str {
        &self.session_key
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionIndexPage {
    pub items: Vec<SessionIndexRecord>,
    pub next_page_key: Option<SessionIndexPageKey>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSourcePageKey {
    source_key: String,
}

impl SessionSourcePageKey {
    pub fn new(source_key: impl Into<String>) -> Result<Self, StorageError> {
        let source_key = source_key.into();
        validate_opaque_key("source key", &source_key)?;
        Ok(Self { source_key })
    }

    pub fn source_key(&self) -> &str {
        &self.source_key
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionSourcePage {
    pub items: Vec<SessionSourceState>,
    pub next_page_key: Option<SessionSourcePageKey>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlobalSessionIndexPageKey {
    last_active_at: String,
    session_key: String,
    source_key: String,
}

impl GlobalSessionIndexPageKey {
    pub fn new(
        last_active_at: impl Into<String>,
        session_key: impl Into<String>,
        source_key: impl Into<String>,
    ) -> Result<Self, StorageError> {
        let key = Self {
            last_active_at: last_active_at.into(),
            session_key: session_key.into(),
            source_key: source_key.into(),
        };
        validate_timestamp(
            "global Session page activity timestamp",
            &key.last_active_at,
        )?;
        validate_opaque_key("session key", &key.session_key)?;
        validate_opaque_key("source key", &key.source_key)?;
        Ok(key)
    }

    pub fn last_active_at(&self) -> &str {
        &self.last_active_at
    }

    pub fn session_key(&self) -> &str {
        &self.session_key
    }

    pub fn source_key(&self) -> &str {
        &self.source_key
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlobalSessionIndexPage {
    pub items: Vec<SessionIndexRecord>,
    pub next_page_key: Option<GlobalSessionIndexPageKey>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionUsagePageKey {
    source_key: String,
    generation: u64,
    occurred_at: String,
    usage_id: String,
}

impl SessionUsagePageKey {
    pub fn new(
        source_key: impl Into<String>,
        generation: u64,
        occurred_at: impl Into<String>,
        usage_id: impl Into<String>,
    ) -> Result<Self, StorageError> {
        let key = Self {
            source_key: source_key.into(),
            generation,
            occurred_at: occurred_at.into(),
            usage_id: usage_id.into(),
        };
        validate_opaque_key("source key", &key.source_key)?;
        validate_generation(key.generation)?;
        validate_timestamp("Session usage page timestamp", &key.occurred_at)?;
        validate_opaque_key("usage identifier", &key.usage_id)?;
        Ok(key)
    }

    pub fn source_key(&self) -> &str {
        &self.source_key
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn occurred_at(&self) -> &str {
        &self.occurred_at
    }

    pub fn usage_id(&self) -> &str {
        &self.usage_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionUsagePage {
    pub items: Vec<SessionUsageRecord>,
    pub next_page_key: Option<SessionUsagePageKey>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlobalSessionUsagePageKey {
    occurred_at: String,
    usage_id: String,
    source_key: String,
}

impl GlobalSessionUsagePageKey {
    pub fn new(
        occurred_at: impl Into<String>,
        usage_id: impl Into<String>,
        source_key: impl Into<String>,
    ) -> Result<Self, StorageError> {
        let key = Self {
            occurred_at: occurred_at.into(),
            usage_id: usage_id.into(),
            source_key: source_key.into(),
        };
        validate_timestamp("global Session usage page timestamp", &key.occurred_at)?;
        validate_opaque_key("usage identifier", &key.usage_id)?;
        validate_opaque_key("source key", &key.source_key)?;
        Ok(key)
    }

    pub fn occurred_at(&self) -> &str {
        &self.occurred_at
    }

    pub fn usage_id(&self) -> &str {
        &self.usage_id
    }

    pub fn source_key(&self) -> &str {
        &self.source_key
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GlobalSessionUsagePage {
    pub items: Vec<SessionUsageRecord>,
    pub next_page_key: Option<GlobalSessionUsagePageKey>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplaySignaturePageKey {
    parent_source_key: String,
    parent_generation: u64,
    token_event_ordinal: u64,
}

impl ReplaySignaturePageKey {
    pub fn new(
        parent_source_key: impl Into<String>,
        parent_generation: u64,
        token_event_ordinal: u64,
    ) -> Result<Self, StorageError> {
        let key = Self {
            parent_source_key: parent_source_key.into(),
            parent_generation,
            token_event_ordinal,
        };
        validate_opaque_key("parent source key", &key.parent_source_key)?;
        validate_generation(key.parent_generation)?;
        if key.token_event_ordinal == 0 {
            return Err(invalid_state("replay signature ordinal must be positive"));
        }
        to_i64(key.token_event_ordinal, "replay ordinal")?;
        Ok(key)
    }

    pub fn parent_source_key(&self) -> &str {
        &self.parent_source_key
    }

    pub const fn parent_generation(&self) -> u64 {
        self.parent_generation
    }

    pub const fn token_event_ordinal(&self) -> u64 {
        self.token_event_ordinal
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexReplaySignaturePage {
    pub items: Vec<CodexReplaySignature>,
    pub next_page_key: Option<ReplaySignaturePageKey>,
}

pub const MAX_SESSION_BATCH_ROWS: usize = 512;
pub const MAX_SESSION_BATCH_BYTES: usize = 512 * 1024;
pub const MAX_PARSER_CHECKPOINT_BYTES: usize = 64 * 1024;
pub const MAX_CODEX_REPLAY_SIGNATURES: u64 = 262_144;
pub const MAX_SUPPLEMENTAL_ROW_BYTES: usize = 2 * 1024;
pub const MAX_SUPPLEMENTAL_ROWS: usize = 32_768;
pub const MAX_SUPPLEMENTAL_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug)]
pub struct StateStore {
    connection: Connection,
    database_path: PathBuf,
}

pub struct ReadOnlyStateStore {
    connection: Connection,
}

impl ReadOnlyStateStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::open_path(path.as_ref())
    }

    pub fn open_live(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::open_path(path.as_ref())
    }

    fn open_path(path: &Path) -> Result<Self, StorageError> {
        let absolute = path
            .canonicalize()
            .map_err(|source| StorageError::Io { source })?;
        let wal_path = sqlite_sidecar_path(&absolute, "-wal");
        let has_wal = match fs::metadata(&wal_path) {
            Ok(metadata) => metadata.len() > 0,
            Err(source) if source.kind() == io::ErrorKind::NotFound => false,
            Err(source) => return Err(StorageError::Io { source }),
        };
        if has_wal {
            return Self::from_connection(wal::open_replayed(&absolute, &wal_path)?);
        }
        Self::open_uri(read_only_database_uri(&absolute)?)
    }

    fn open_uri(uri: String) -> Result<Self, StorageError> {
        let connection = Connection::open_with_flags(
            uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(map_database_error)?;
        Self::from_connection(connection)
    }

    fn from_connection(connection: Connection) -> Result<Self, StorageError> {
        connection
            .execute_batch("PRAGMA query_only = ON;")
            .map_err(map_database_error)?;
        let store = Self { connection };
        store.validate()?;
        Ok(store)
    }

    pub fn health(&self) -> Result<StateHealth, StorageError> {
        let versions = schema_versions(&self.connection)?;
        if versions != [1, 2, LATEST_SCHEMA_VERSION] {
            return Err(StorageError::StateDatabaseCorrupt {
                message: "state database has an incompatible migration history".to_owned(),
            });
        }
        Ok(StateHealth {
            schema_version: LATEST_SCHEMA_VERSION,
        })
    }

    pub fn runtime_secret_binding(
        &self,
        name: &str,
    ) -> Result<Option<RuntimeSecretBinding>, StorageError> {
        query_runtime_secret_binding(&self.connection, name)
    }

    fn validate(&self) -> Result<(), StorageError> {
        let quick_check = self
            .connection
            .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
            .map_err(map_database_error)?;
        if quick_check != "ok" {
            return Err(StorageError::StateDatabaseCorrupt {
                message: "state database failed read-only integrity inspection".to_owned(),
            });
        }
        self.health().map(|_| ())
    }
}

impl StateStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref();
        let setup_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(Self::setup_lock_path(path))
            .map_err(|source| StorageError::Io { source })?;
        setup_lock
            .lock_exclusive()
            .map_err(|source| StorageError::Io { source })?;
        let result = Self::open_locked(path);
        let unlock_result =
            FileExt::unlock(&setup_lock).map_err(|source| StorageError::Io { source });

        match (result, unlock_result) {
            (Ok(store), Ok(())) => Ok(store),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    pub fn open_live_reader(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path
            .as_ref()
            .canonicalize()
            .map_err(|source| StorageError::Io { source })?;
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(map_database_error)?;
        connection
            .execute_batch(
                "PRAGMA busy_timeout = 5000;\
                 PRAGMA foreign_keys = ON;\
                 PRAGMA query_only = ON;",
            )
            .map_err(map_database_error)?;
        if schema_versions(&connection)? != [1, 2, LATEST_SCHEMA_VERSION] {
            return Err(StorageError::StateDatabaseCorrupt {
                message: "state database has an incompatible migration history".to_owned(),
            });
        }
        Ok(Self {
            connection,
            database_path: path,
        })
    }

    fn open_locked(path: &Path) -> Result<Self, StorageError> {
        let mut connection = Connection::open(path).map_err(map_database_error)?;
        connection
            .execute_batch(
                "PRAGMA busy_timeout = 5000;\
                 PRAGMA foreign_keys = ON;\
                 PRAGMA journal_mode = WAL;\
                 PRAGMA wal_autocheckpoint = 0;",
            )
            .map_err(map_database_error)?;

        apply_ordered_migrations(&mut connection)?;

        Ok(Self {
            connection,
            database_path: path.to_path_buf(),
        })
    }

    fn setup_lock_path(path: &Path) -> PathBuf {
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".lock");
        lock_path.into()
    }

    fn wal_path(&self) -> PathBuf {
        let mut wal_path = self.database_path.as_os_str().to_os_string();
        wal_path.push("-wal");
        wal_path.into()
    }

    pub fn health(&self) -> Result<StateHealth, StorageError> {
        let schema_version = self
            .connection
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get::<_, Option<i64>>(0)
            })
            .map_err(map_database_error)?
            .unwrap_or_default();
        Ok(StateHealth { schema_version })
    }

    pub fn record_request_metrics(
        &mut self,
        metrics: &[RequestMetric],
    ) -> Result<(), StorageError> {
        if metrics.is_empty() {
            return Ok(());
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_database_error)?;
        {
            let mut statement = transaction
                .prepare(
                    "INSERT INTO request_metrics (request_id, provider_id, model, started_at, latency_ms, input_tokens, output_tokens, status_code, error_code) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )
                .map_err(map_database_error)?;
            for metric in metrics {
                statement
                    .execute(params![
                        metric.request_id,
                        metric.provider_id,
                        metric.model,
                        metric.started_at,
                        metric.latency_ms,
                        metric.input_tokens,
                        metric.output_tokens,
                        metric.status_code,
                        metric.error_code,
                    ])
                    .map_err(map_database_error)?;
            }
        }
        transaction.commit().map_err(map_database_error)?;
        Ok(())
    }

    pub fn record_orphan_secret(
        &self,
        secret_ref: &SecretRef,
        created_at: &str,
    ) -> Result<(), StorageError> {
        self.connection
            .execute(
                "INSERT INTO orphan_secrets (secret_ref, created_at) VALUES (?1, ?2) ON CONFLICT(secret_ref) DO UPDATE SET created_at = excluded.created_at",
                params![secret_ref.as_str(), created_at],
            )
            .map_err(map_database_error)?;
        Ok(())
    }

    pub fn runtime_secret_binding(
        &self,
        name: &str,
    ) -> Result<Option<RuntimeSecretBinding>, StorageError> {
        query_runtime_secret_binding(&self.connection, name)
    }

    pub fn bind_runtime_secret_if_absent(
        &mut self,
        name: &str,
        secret_ref: &SecretRef,
        created_at: &str,
    ) -> Result<RuntimeSecretBinding, StorageError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_database_error)?;
        let existing_revision = transaction
            .query_row(
                "SELECT revision FROM runtime_secret_bindings WHERE binding_name = ?1",
                [name],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(map_database_error)?;
        if let Some(existing_revision) = existing_revision {
            let actual = u64::try_from(existing_revision).map_err(|_| {
                StorageError::StateDatabaseCorrupt {
                    message: "runtime secret binding contains an invalid revision".to_owned(),
                }
            })?;
            return Err(StorageError::RuntimeSecretBindingConflict { actual });
        }

        transaction
            .execute(
                "INSERT INTO runtime_secret_bindings(binding_name, secret_ref, revision, created_at)
                 VALUES (?1, ?2, 1, ?3)",
                params![name, secret_ref.as_str(), created_at],
            )
            .map_err(map_database_error)?;
        transaction.commit().map_err(map_database_error)?;

        Ok(RuntimeSecretBinding {
            name: name.to_owned(),
            secret_ref: secret_ref.clone(),
            revision: 1,
            created_at: created_at.to_owned(),
        })
    }

    pub fn issue_client_token(&mut self, token: &ClientTokenMetadata) -> Result<(), StorageError> {
        self.issue_client_token_with_scopes(token, &[ClientTokenScope::ProxyUse])
    }

    pub fn issue_client_token_with_scopes(
        &mut self,
        token: &ClientTokenMetadata,
        scopes: &[ClientTokenScope],
    ) -> Result<(), StorageError> {
        if scopes.is_empty() {
            return Err(StorageError::InvalidStateRecord {
                message: "client token scopes must not be empty".to_owned(),
            });
        }
        let mut seen = [false; 5];
        for scope in scopes {
            if std::mem::replace(&mut seen[scope.index()], true) {
                return Err(StorageError::InvalidStateRecord {
                    message: "client token scopes must be unique".to_owned(),
                });
            }
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_database_error)?;
        transaction
            .execute(
                "INSERT INTO client_tokens(token_id, client_id, token_digest, issued_at, revoked_at)
                 VALUES (?1, ?2, ?3, ?4, NULL)",
                params![
                    token.token_id,
                    token.client_id.as_str(),
                    token.digest.as_slice(),
                    token.issued_at,
                ],
            )
            .map_err(map_database_error)?;
        {
            let mut statement = transaction
                .prepare("INSERT INTO client_token_scopes(token_id, scope) VALUES (?1, ?2)")
                .map_err(map_database_error)?;
            for scope in scopes {
                statement
                    .execute(params![token.token_id, scope.as_str()])
                    .map_err(map_database_error)?;
            }
        }
        transaction.commit().map_err(map_database_error)?;
        Ok(())
    }

    pub fn revoke_client_token(
        &mut self,
        client_id: &ClientId,
        token_id: &str,
        revoked_at: &str,
    ) -> Result<bool, StorageError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_database_error)?;
        let changed = transaction
            .execute(
                "UPDATE client_tokens
                 SET revoked_at = ?3
                 WHERE token_id = ?1 AND client_id = ?2 AND revoked_at IS NULL",
                params![token_id, client_id.as_str(), revoked_at],
            )
            .map_err(map_database_error)?
            != 0;
        transaction.commit().map_err(map_database_error)?;
        Ok(changed)
    }

    pub fn load_active_client_tokens(&self) -> Result<Vec<ClientTokenMetadata>, StorageError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT token_id, client_id, token_digest, issued_at
                 FROM client_tokens
                 WHERE revoked_at IS NULL
                 ORDER BY token_id",
            )
            .map_err(map_database_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(map_database_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_database_error)?;

        rows.into_iter()
            .map(|(token_id, client_id, digest, issued_at)| {
                let client_id =
                    ClientId::new(client_id).map_err(|_| StorageError::StateDatabaseCorrupt {
                        message: "client token metadata contains an invalid client identifier"
                            .to_owned(),
                    })?;
                let digest = digest
                    .try_into()
                    .map_err(|_| StorageError::StateDatabaseCorrupt {
                        message: "client token metadata contains an invalid digest".to_owned(),
                    })?;
                Ok(ClientTokenMetadata {
                    token_id,
                    client_id,
                    digest,
                    issued_at,
                })
            })
            .collect()
    }

    pub fn load_active_scoped_client_tokens(
        &self,
    ) -> Result<Vec<ScopedClientTokenMetadata>, StorageError> {
        let tokens = self.load_active_client_tokens()?;
        let mut scoped = Vec::with_capacity(tokens.len());
        let mut statement = self
            .connection
            .prepare(
                "SELECT scope
                 FROM client_token_scopes
                 WHERE token_id = ?1
                 ORDER BY CASE scope
                    WHEN 'proxy.use' THEN 0
                    WHEN 'sessions.read' THEN 1
                    WHEN 'usage.read' THEN 2
                    WHEN 'diagnostics.read' THEN 3
                    WHEN 'diagnostics.export' THEN 4
                 END",
            )
            .map_err(map_database_error)?;
        for token in tokens {
            let scopes = statement
                .query_map([&token.token_id], |row| row.get::<_, String>(0))
                .map_err(map_database_error)?
                .map(|result| {
                    let scope = result.map_err(map_database_error)?;
                    ClientTokenScope::from_str(&scope).map_err(|_| {
                        StorageError::StateDatabaseCorrupt {
                            message: "client token metadata contains an invalid scope".to_owned(),
                        }
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            if scopes.is_empty() {
                return Err(StorageError::StateDatabaseCorrupt {
                    message: "client token metadata contains no scope".to_owned(),
                });
            }
            scoped.push(ScopedClientTokenMetadata { token, scopes });
        }
        Ok(scoped)
    }

    pub fn begin_or_resume_candidate(
        &mut self,
        cursor: &SessionScanCursor,
    ) -> Result<CandidateBeginOutcome, StorageError> {
        validate_cursor(cursor)?;
        if cursor.generation_state != SessionGenerationState::Staging {
            return Err(invalid_state(
                "a candidate cursor must have staging generation state",
            ));
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_database_error)?;
        let source = transaction
            .query_row(
                "SELECT source_kind, current_generation, staging_generation, retired_generation
                 FROM session_sources WHERE source_key = ?1",
                [&cursor.source_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(map_database_error)?;

        let outcome = match source {
            None => {
                transaction
                    .execute(
                        "INSERT INTO session_sources(
                            source_key, source_kind, current_generation, staging_generation,
                            retired_generation, status, error_code, last_transition_at
                         ) VALUES (?1, ?2, NULL, ?3, NULL, 'undiscovered', NULL, NULL)",
                        params![
                            cursor.source_key,
                            cursor.source_kind.as_str(),
                            to_i64(cursor.generation, "generation")?,
                        ],
                    )
                    .map_err(map_database_error)?;
                insert_cursor(&transaction, cursor)?;
                CandidateBeginOutcome::Started
            }
            Some((source_kind, current, staging, retired)) => {
                if SessionSourceKind::from_database(&source_kind)? != cursor.source_kind {
                    return Err(StorageError::StableRecordConflict {
                        record_kind: "session source",
                    });
                }
                let current = optional_u64(current, "current generation")?;
                let staging = optional_u64(staging, "staging generation")?;
                let retired = optional_u64(retired, "retired generation")?;
                if let Some(retired) = retired {
                    CandidateBeginOutcome::CleanupRequired {
                        generation: retired,
                    }
                } else if let Some(staging) = staging {
                    if staging != cursor.generation {
                        CandidateBeginOutcome::CleanupRequired {
                            generation: staging,
                        }
                    } else {
                        let persisted = query_cursor(&transaction, &cursor.source_key, staging)?
                            .ok_or_else(|| StorageError::StateDatabaseCorrupt {
                                message:
                                    "staging generation points to a missing Session scan cursor"
                                        .to_owned(),
                            })?;
                        if persisted.source_kind == cursor.source_kind
                            && persisted.file_identity == cursor.file_identity
                            && persisted.head_fingerprint == cursor.head_fingerprint
                            && persisted.boundary_fingerprint == cursor.boundary_fingerprint
                            && persisted.parent_source_key == cursor.parent_source_key
                            && persisted.parent_generation == cursor.parent_generation
                            && persisted.replay_boundary_fingerprint
                                == cursor.replay_boundary_fingerprint
                            && cursor.observed_size >= persisted.complete_byte_offset
                        {
                            CandidateBeginOutcome::Resumed(Box::new(persisted))
                        } else {
                            CandidateBeginOutcome::CleanupRequired {
                                generation: staging,
                            }
                        }
                    }
                } else if current == Some(cursor.generation) {
                    return Err(StorageError::CandidateStateConflict);
                } else {
                    let changed = transaction
                        .execute(
                            "UPDATE session_sources SET staging_generation = ?2
                             WHERE source_key = ?1
                               AND staging_generation IS NULL
                               AND retired_generation IS NULL",
                            params![cursor.source_key, to_i64(cursor.generation, "generation")?],
                        )
                        .map_err(map_database_error)?;
                    if changed != 1 {
                        return Err(StorageError::CandidateStateConflict);
                    }
                    insert_cursor(&transaction, cursor)?;
                    CandidateBeginOutcome::Started
                }
            }
        };
        transaction.commit().map_err(map_database_error)?;
        Ok(outcome)
    }

    pub fn commit_candidate_batch(
        &mut self,
        batch: &SessionBatch,
    ) -> Result<SupplementalBatchOutcome, StorageError> {
        let cursor = batch
            .cursor
            .as_ref()
            .ok_or_else(|| invalid_state("a candidate batch requires a cursor"))?;
        if cursor.generation_state != SessionGenerationState::Staging {
            return Err(invalid_state(
                "a candidate batch cursor must have staging generation state",
            ));
        }
        self.commit_session_batch_internal(batch, Some(SessionGenerationState::Staging))
    }

    pub fn commit_session_batch(
        &mut self,
        batch: &SessionBatch,
    ) -> Result<SupplementalBatchOutcome, StorageError> {
        self.commit_session_batch_internal(batch, None)
    }

    fn commit_session_batch_internal(
        &mut self,
        batch: &SessionBatch,
        required_state: Option<SessionGenerationState>,
    ) -> Result<SupplementalBatchOutcome, StorageError> {
        validate_session_batch(batch)?;
        if batch.cursor.is_none()
            && batch.index_records.is_empty()
            && batch.usage_records.is_empty()
            && batch.replay_signatures.is_empty()
            && batch.supplemental_metadata.is_empty()
        {
            return Ok(SupplementalBatchOutcome {
                inserted_rows: 0,
                dropped_rows: 0,
            });
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_database_error)?;
        if let Some(cursor) = &batch.cursor {
            let pointer = match required_state.unwrap_or(cursor.generation_state) {
                SessionGenerationState::Staging => "staging_generation",
                SessionGenerationState::Current => "current_generation",
                SessionGenerationState::Retired => {
                    return Err(invalid_state("retired generations are read-only"));
                }
            };
            let pointed_generation = transaction
                .query_row(
                    &format!(
                        "SELECT {pointer} FROM session_sources
                         WHERE source_key = ?1 AND source_kind = ?2"
                    ),
                    params![cursor.source_key, cursor.source_kind.as_str()],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .optional()
                .map_err(map_database_error)?
                .flatten();
            if optional_u64(pointed_generation, "pointed generation")? != Some(cursor.generation) {
                return Err(StorageError::CandidateStateConflict);
            }
            upsert_cursor(&transaction, cursor)?;
        }

        for record in &batch.index_records {
            write_index_record(&transaction, record)?;
        }
        for record in &batch.usage_records {
            write_usage_record(&transaction, record)?;
        }
        let pending_replay_signatures =
            pending_replay_signatures(&transaction, &batch.replay_signatures)?;
        for signature in pending_replay_signatures {
            insert_replay_signature(&transaction, signature)?;
        }
        let (pending_supplemental, pending_supplemental_bytes) =
            pending_supplemental_metadata(&transaction, &batch.supplemental_metadata)?;
        let supplemental_outcome = if supplemental_batch_fits(
            &transaction,
            pending_supplemental.len(),
            pending_supplemental_bytes,
        )? {
            let inserted_rows = pending_supplemental.len();
            for metadata in pending_supplemental {
                write_supplemental_metadata(&transaction, metadata)?;
            }
            SupplementalBatchOutcome {
                inserted_rows,
                dropped_rows: 0,
            }
        } else {
            SupplementalBatchOutcome {
                inserted_rows: 0,
                dropped_rows: pending_supplemental.len(),
            }
        };
        transaction.commit().map_err(map_database_error)?;
        Ok(supplemental_outcome)
    }

    pub fn promote_candidate(
        &mut self,
        source_key: &str,
        generation: u64,
        transition_at: &str,
    ) -> Result<(), StorageError> {
        validate_opaque_key("source key", source_key)?;
        validate_timestamp("transition timestamp", transition_at)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_database_error)?;
        let (current, staging, retired) = transaction
            .query_row(
                "SELECT current_generation, staging_generation, retired_generation
                 FROM session_sources WHERE source_key = ?1",
                [source_key],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(map_database_error)?
            .ok_or(StorageError::CandidateStateConflict)?;
        let current = optional_u64(current, "current generation")?;
        let staging = optional_u64(staging, "staging generation")?;
        let retired = optional_u64(retired, "retired generation")?;
        if staging != Some(generation) || retired.is_some() {
            return Err(StorageError::CandidateStateConflict);
        }
        let cursor_exists = transaction
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM session_scan_cursors
                    WHERE source_key = ?1 AND generation = ?2 AND generation_state = 'staging'
                 )",
                params![source_key, to_i64(generation, "generation")?],
                |row| row.get::<_, bool>(0),
            )
            .map_err(map_database_error)?;
        if !cursor_exists {
            return Err(StorageError::CandidateStateConflict);
        }
        if let Some(current) = current {
            let changed = transaction
                .execute(
                    "UPDATE session_scan_cursors SET generation_state = 'retired'
                     WHERE source_key = ?1 AND generation = ?2
                       AND generation_state = 'current'",
                    params![source_key, to_i64(current, "generation")?],
                )
                .map_err(map_database_error)?;
            if changed != 1 {
                return Err(StorageError::CandidateStateConflict);
            }
        }
        let changed = transaction
            .execute(
                "UPDATE session_scan_cursors SET generation_state = 'current'
                 WHERE source_key = ?1 AND generation = ?2
                   AND generation_state = 'staging'",
                params![source_key, to_i64(generation, "generation")?],
            )
            .map_err(map_database_error)?;
        if changed != 1 {
            return Err(StorageError::CandidateStateConflict);
        }
        let changed = transaction
            .execute(
                "UPDATE session_sources
                 SET current_generation = ?2,
                     staging_generation = NULL,
                     retired_generation = current_generation,
                     status = 'available',
                     error_code = NULL,
                     last_transition_at = ?3
                 WHERE source_key = ?1
                   AND current_generation IS ?4
                   AND staging_generation = ?2
                   AND retired_generation IS NULL",
                params![
                    source_key,
                    to_i64(generation, "generation")?,
                    transition_at,
                    current
                        .map(|generation| to_i64(generation, "generation"))
                        .transpose()?,
                ],
            )
            .map_err(map_database_error)?;
        if changed != 1 {
            return Err(StorageError::CandidateStateConflict);
        }
        transaction.commit().map_err(map_database_error)
    }

    pub fn load_current_generation(&self, source_key: &str) -> Result<Option<u64>, StorageError> {
        validate_opaque_key("source key", source_key)?;
        let generation = self
            .connection
            .query_row(
                "SELECT current_generation FROM session_sources WHERE source_key = ?1",
                [source_key],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()
            .map_err(map_database_error)?
            .flatten();
        optional_u64(generation, "current generation")
    }

    pub fn load_current_session_scan_cursor(
        &self,
        source_key: &str,
    ) -> Result<Option<SessionScanCursor>, StorageError> {
        validate_opaque_key("source key", source_key)?;
        self.connection
            .query_row(
                "SELECT c.source_key, c.source_kind, c.generation, c.generation_state,
                        c.file_identity, c.observed_size, c.modified_at,
                        c.complete_byte_offset, c.stable_record_ordinal,
                        c.parser_checkpoint, c.head_fingerprint, c.boundary_fingerprint,
                        c.parent_source_key, c.parent_generation,
                        c.replay_boundary_fingerprint, c.result_code, c.result_changed_at
                 FROM session_sources s
                 JOIN session_scan_cursors c
                   ON c.source_key = s.source_key
                  AND c.generation = s.current_generation
                 WHERE s.source_key = ?1",
                [source_key],
                cursor_database_row,
            )
            .optional()
            .map_err(map_database_error)?
            .map(cursor_from_database)
            .transpose()
    }

    pub fn load_staging_session_scan_cursor(
        &self,
        source_key: &str,
    ) -> Result<Option<SessionScanCursor>, StorageError> {
        validate_opaque_key("source key", source_key)?;
        self.connection
            .query_row(
                "SELECT c.source_key, c.source_kind, c.generation, c.generation_state,
                        c.file_identity, c.observed_size, c.modified_at,
                        c.complete_byte_offset, c.stable_record_ordinal,
                        c.parser_checkpoint, c.head_fingerprint, c.boundary_fingerprint,
                        c.parent_source_key, c.parent_generation,
                        c.replay_boundary_fingerprint, c.result_code, c.result_changed_at
                 FROM session_sources s
                 JOIN session_scan_cursors c
                   ON c.source_key = s.source_key
                  AND c.generation = s.staging_generation
                 WHERE s.source_key = ?1",
                [source_key],
                cursor_database_row,
            )
            .optional()
            .map_err(map_database_error)?
            .map(cursor_from_database)
            .transpose()
    }

    pub fn load_staging_session_index_record(
        &self,
        source_key: &str,
    ) -> Result<Option<SessionIndexRecord>, StorageError> {
        validate_opaque_key("source key", source_key)?;
        self.connection
            .query_row(
                "SELECT i.session_key, i.source_key, i.generation, i.source_kind,
                        i.created_at, i.last_active_at, i.message_count,
                        i.usage_event_count, i.availability
                 FROM session_sources s
                 JOIN session_index i
                   ON i.source_key = s.source_key
                  AND i.generation = s.staging_generation
                 WHERE s.source_key = ?1",
                [source_key],
                index_database_row,
            )
            .optional()
            .map_err(map_database_error)?
            .map(index_record_from_database)
            .transpose()
    }

    pub fn load_session_source(
        &self,
        source_key: &str,
    ) -> Result<Option<SessionSourceState>, StorageError> {
        validate_opaque_key("source key", source_key)?;
        query_session_source(&self.connection, source_key)
    }

    pub fn fail_candidate(
        &mut self,
        source_key: &str,
        generation: u64,
        error_code: SessionSourceErrorCode,
        transition_at: &str,
    ) -> Result<bool, StorageError> {
        validate_opaque_key("source key", source_key)?;
        validate_generation(generation)?;
        validate_timestamp("source transition timestamp", transition_at)?;
        let source = query_session_source(&self.connection, source_key)?
            .ok_or(StorageError::CandidateStateConflict)?;
        if source.current_generation != Some(generation)
            && source.staging_generation != Some(generation)
        {
            return Err(StorageError::CandidateStateConflict);
        }
        let status = failed_source_status(&source, error_code);
        if source.status == status && source.error_code == Some(error_code) {
            return Ok(false);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_database_error)?;
        let source = query_session_source(&transaction, source_key)?
            .ok_or(StorageError::CandidateStateConflict)?;
        if source.current_generation != Some(generation)
            && source.staging_generation != Some(generation)
        {
            return Err(StorageError::CandidateStateConflict);
        }
        let status = failed_source_status(&source, error_code);
        if source.status == status && source.error_code == Some(error_code) {
            return Ok(false);
        }
        let changed = transaction
            .execute(
                "UPDATE session_sources
                 SET status = ?2, error_code = ?3, last_transition_at = ?4
                 WHERE source_key = ?1
                   AND current_generation IS ?5
                   AND staging_generation IS ?6
                   AND status = ?7
                   AND error_code IS ?8",
                params![
                    source_key,
                    status.as_str(),
                    error_code.as_str(),
                    transition_at,
                    source
                        .current_generation
                        .map(|generation| to_i64(generation, "generation"))
                        .transpose()?,
                    source
                        .staging_generation
                        .map(|generation| to_i64(generation, "generation"))
                        .transpose()?,
                    source.status.as_str(),
                    source.error_code.map(SessionSourceErrorCode::as_str),
                ],
            )
            .map_err(map_database_error)?;
        if changed != 1 {
            return Err(StorageError::CandidateStateConflict);
        }
        transaction.commit().map_err(map_database_error)?;
        Ok(true)
    }

    pub fn record_source_success(
        &mut self,
        source_key: &str,
        generation: u64,
        transition_at: &str,
    ) -> Result<bool, StorageError> {
        validate_opaque_key("source key", source_key)?;
        validate_generation(generation)?;
        validate_timestamp("source transition timestamp", transition_at)?;
        let source = query_session_source(&self.connection, source_key)?
            .ok_or(StorageError::CandidateStateConflict)?;
        if source.current_generation != Some(generation) {
            return Err(StorageError::CandidateStateConflict);
        }
        if source.status == SessionSourceStatus::Available && source.error_code.is_none() {
            return Ok(false);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_database_error)?;
        let source = query_session_source(&transaction, source_key)?
            .ok_or(StorageError::CandidateStateConflict)?;
        if source.current_generation != Some(generation) {
            return Err(StorageError::CandidateStateConflict);
        }
        if source.status == SessionSourceStatus::Available && source.error_code.is_none() {
            return Ok(false);
        }
        let changed = transaction
            .execute(
                "UPDATE session_sources
                 SET status = 'available', error_code = NULL, last_transition_at = ?2
                 WHERE source_key = ?1
                   AND current_generation = ?3
                   AND status = ?4
                   AND error_code IS ?5",
                params![
                    source_key,
                    transition_at,
                    to_i64(generation, "generation")?,
                    source.status.as_str(),
                    source.error_code.map(SessionSourceErrorCode::as_str),
                ],
            )
            .map_err(map_database_error)?;
        if changed != 1 {
            return Err(StorageError::CandidateStateConflict);
        }
        transaction.commit().map_err(map_database_error)?;
        Ok(true)
    }

    pub fn mark_source_unavailable(
        &mut self,
        source_key: &str,
        error_code: SessionSourceErrorCode,
        transition_at: &str,
    ) -> Result<bool, StorageError> {
        validate_opaque_key("source key", source_key)?;
        validate_timestamp("source transition timestamp", transition_at)?;
        if error_code.is_resource_limit() {
            return Err(invalid_state(
                "an unavailable source cannot use a resource-limit error code",
            ));
        }
        let source = query_session_source(&self.connection, source_key)?
            .ok_or(StorageError::CandidateStateConflict)?;
        let status = unavailable_source_status(&source);
        if source.status == status && source.error_code == Some(error_code) {
            return Ok(false);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_database_error)?;
        let source = query_session_source(&transaction, source_key)?
            .ok_or(StorageError::CandidateStateConflict)?;
        let status = unavailable_source_status(&source);
        if source.status == status && source.error_code == Some(error_code) {
            return Ok(false);
        }
        let changed = transaction
            .execute(
                "UPDATE session_sources
                 SET status = ?2, error_code = ?3, last_transition_at = ?4
                 WHERE source_key = ?1
                   AND current_generation IS ?5
                   AND staging_generation IS ?6
                   AND status = ?7
                   AND error_code IS ?8",
                params![
                    source_key,
                    status.as_str(),
                    error_code.as_str(),
                    transition_at,
                    source
                        .current_generation
                        .map(|generation| to_i64(generation, "generation"))
                        .transpose()?,
                    source
                        .staging_generation
                        .map(|generation| to_i64(generation, "generation"))
                        .transpose()?,
                    source.status.as_str(),
                    source.error_code.map(SessionSourceErrorCode::as_str),
                ],
            )
            .map_err(map_database_error)?;
        if changed != 1 {
            return Err(StorageError::CandidateStateConflict);
        }
        transaction.commit().map_err(map_database_error)?;
        Ok(true)
    }

    pub fn record_request_supplemental_batch(
        &mut self,
        metadata: &[RequestSupplementalMetadata],
    ) -> Result<SupplementalBatchOutcome, StorageError> {
        if metadata.is_empty() {
            return Ok(SupplementalBatchOutcome {
                inserted_rows: 0,
                dropped_rows: 0,
            });
        }
        if metadata.len() > MAX_SESSION_BATCH_ROWS {
            return Err(StorageError::SessionBatchLimitExceeded);
        }
        let mut batch_bytes = 0_usize;
        for row in metadata {
            batch_bytes = batch_bytes.saturating_add(validate_supplemental_metadata(row)?);
        }
        if batch_bytes > MAX_SESSION_BATCH_BYTES {
            return Err(StorageError::SessionBatchLimitExceeded);
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_database_error)?;
        let (pending, pending_bytes) = pending_supplemental_metadata(&transaction, metadata)?;
        if !supplemental_batch_fits(&transaction, pending.len(), pending_bytes)? {
            return Ok(SupplementalBatchOutcome {
                inserted_rows: 0,
                dropped_rows: pending.len(),
            });
        }
        for row in &pending {
            write_supplemental_metadata(&transaction, row)?;
        }
        transaction.commit().map_err(map_database_error)?;
        Ok(SupplementalBatchOutcome {
            inserted_rows: pending.len(),
            dropped_rows: 0,
        })
    }

    pub fn inspect_request_supplemental(&self) -> Result<SupplementalStorageStats, StorageError> {
        supplemental_stats(&self.connection)
    }

    pub fn cleanup_request_supplemental(
        &mut self,
        now: &str,
        max_rows: usize,
    ) -> Result<CleanupBatchOutcome, StorageError> {
        validate_timestamp("supplemental cleanup timestamp", now)?;
        if max_rows == 0 || max_rows > MAX_SESSION_BATCH_ROWS {
            return Err(StorageError::SessionBatchLimitExceeded);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_database_error)?;
        let cutoff = transaction
            .query_row(
                "SELECT strftime('%Y-%m-%dT%H:%M:%SZ', ?1, '-24 hours')",
                [now],
                |row| row.get::<_, Option<String>>(0),
            )
            .map_err(map_database_error)?
            .ok_or_else(|| invalid_state("supplemental cleanup timestamp is invalid"))?;
        let mut stats = supplemental_stats(&transaction)?;
        let mut statement = transaction
            .prepare(
                "SELECT rowid, occurred_at, logical_bytes
                 FROM request_supplemental_metadata
                 ORDER BY occurred_at, request_id, attempt_id
                 LIMIT ?1",
            )
            .map_err(map_database_error)?;
        let candidates = statement
            .query_map([usize_to_i64(max_rows, "cleanup row limit")?], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(map_database_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_database_error)?;
        drop(statement);
        let mut deleted_rows = 0_usize;
        let mut deleted_bytes = 0_usize;
        for (row_id, occurred_at, logical_bytes) in candidates {
            let logical_bytes =
                usize::try_from(logical_bytes).map_err(|_| StorageError::StateDatabaseCorrupt {
                    message: "supplemental metadata contains an invalid logical size".to_owned(),
                })?;
            let expired = occurred_at < cutoff;
            let over_capacity =
                stats.rows > MAX_SUPPLEMENTAL_ROWS || stats.logical_bytes > MAX_SUPPLEMENTAL_BYTES;
            if !expired && !over_capacity {
                break;
            }
            if deleted_bytes.saturating_add(logical_bytes) > MAX_SESSION_BATCH_BYTES {
                break;
            }
            transaction
                .execute(
                    "DELETE FROM request_supplemental_metadata WHERE rowid = ?1",
                    [row_id],
                )
                .map_err(map_database_error)?;
            deleted_rows += 1;
            deleted_bytes += logical_bytes;
            stats.rows = stats.rows.saturating_sub(1);
            stats.logical_bytes = stats.logical_bytes.saturating_sub(logical_bytes);
        }
        let complete = !transaction
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM request_supplemental_metadata
                    WHERE occurred_at < ?1
                 )",
                [&cutoff],
                |row| row.get::<_, bool>(0),
            )
            .map_err(map_database_error)?
            && stats.rows <= MAX_SUPPLEMENTAL_ROWS
            && stats.logical_bytes <= MAX_SUPPLEMENTAL_BYTES;
        transaction.commit().map_err(map_database_error)?;
        Ok(CleanupBatchOutcome {
            deleted_rows,
            deleted_bytes,
            complete,
        })
    }

    pub fn cleanup_generation_batch(
        &mut self,
        source_key: &str,
        generation: u64,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<CleanupBatchOutcome, StorageError> {
        validate_opaque_key("source key", source_key)?;
        validate_generation(generation)?;
        if max_rows == 0
            || max_rows > MAX_SESSION_BATCH_ROWS
            || max_bytes == 0
            || max_bytes > MAX_SESSION_BATCH_BYTES
        {
            return Err(StorageError::SessionBatchLimitExceeded);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_database_error)?;
        let pointers = transaction
            .query_row(
                "SELECT current_generation, staging_generation, retired_generation
                 FROM session_sources WHERE source_key = ?1",
                [source_key],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(map_database_error)?;
        let Some((current, staging, retired)) = pointers else {
            transaction.commit().map_err(map_database_error)?;
            return Ok(CleanupBatchOutcome {
                deleted_rows: 0,
                deleted_bytes: 0,
                complete: true,
            });
        };
        let current = optional_u64(current, "current generation")?;
        let staging = optional_u64(staging, "staging generation")?;
        let retired = optional_u64(retired, "retired generation")?;
        if current == Some(generation)
            || (staging != Some(generation) && retired != Some(generation))
        {
            return Err(StorageError::CandidateStateConflict);
        }

        let generation = to_i64(generation, "generation")?;
        let mut statement = transaction
            .prepare(
                "SELECT table_kind, row_id, logical_bytes FROM (
                    SELECT 1 AS table_kind, rowid AS row_id,
                           length(CAST(session_key AS BLOB))
                           + length(CAST(source_key AS BLOB))
                           + length(CAST(created_at AS BLOB))
                           + length(CAST(last_active_at AS BLOB)) + 40 AS logical_bytes
                    FROM session_index WHERE source_key = ?1 AND generation = ?2
                    UNION ALL
                    SELECT 2, rowid,
                           length(CAST(usage_id AS BLOB))
                           + length(CAST(session_key AS BLOB))
                           + length(CAST(source_key AS BLOB))
                           + length(CAST(model AS BLOB))
                           + length(CAST(occurred_at AS BLOB)) + 64
                    FROM session_usage_records WHERE source_key = ?1 AND generation = ?2
                    UNION ALL
                    SELECT 3, rowid,
                           length(CAST(parent_source_key AS BLOB))
                           + length(CAST(occurred_at AS BLOB)) + 48
                    FROM codex_replay_signatures
                    WHERE parent_source_key = ?1 AND parent_generation = ?2
                 )
                 ORDER BY table_kind, row_id
                 LIMIT ?3",
            )
            .map_err(map_database_error)?;
        let candidates = statement
            .query_map(
                params![
                    source_key,
                    generation,
                    usize_to_i64(max_rows, "cleanup row limit")?
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .map_err(map_database_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_database_error)?;
        drop(statement);

        let mut deleted_rows: usize = 0;
        let mut deleted_bytes: usize = 0;
        for (table_kind, row_id, logical_bytes) in candidates {
            let logical_bytes =
                usize::try_from(logical_bytes).map_err(|_| StorageError::StateDatabaseCorrupt {
                    message: "generation cleanup found an invalid logical row size".to_owned(),
                })?;
            if deleted_bytes.saturating_add(logical_bytes) > max_bytes {
                break;
            }
            let table = match table_kind {
                1 => "session_index",
                2 => "session_usage_records",
                3 => "codex_replay_signatures",
                _ => {
                    return Err(StorageError::StateDatabaseCorrupt {
                        message: "generation cleanup found an invalid table kind".to_owned(),
                    });
                }
            };
            transaction
                .execute(&format!("DELETE FROM {table} WHERE rowid = ?1"), [row_id])
                .map_err(map_database_error)?;
            deleted_rows += 1;
            deleted_bytes += logical_bytes;
        }

        let remaining = transaction
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM session_index
                     WHERE source_key = ?1 AND generation = ?2)
                  + (SELECT COUNT(*) FROM session_usage_records
                     WHERE source_key = ?1 AND generation = ?2)
                  + (SELECT COUNT(*) FROM codex_replay_signatures
                     WHERE parent_source_key = ?1 AND parent_generation = ?2)",
                params![source_key, generation],
                |row| row.get::<_, i64>(0),
            )
            .map_err(map_database_error)?;
        let cursor_bytes = transaction
            .query_row(
                "SELECT length(CAST(source_key AS BLOB))
                        + length(CAST(file_identity AS BLOB))
                        + length(CAST(modified_at AS BLOB))
                        + length(CAST(COALESCE(result_code, '') AS BLOB))
                        + length(CAST(COALESCE(result_changed_at, '') AS BLOB))
                        + length(CAST(COALESCE(parent_source_key, '') AS BLOB))
                        + length(parser_checkpoint) + 128
                 FROM session_scan_cursors WHERE source_key = ?1 AND generation = ?2",
                params![source_key, generation],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(map_database_error)?
            .map(usize::try_from)
            .transpose()
            .map_err(|_| StorageError::StateDatabaseCorrupt {
                message: "generation cleanup found an invalid cursor size".to_owned(),
            })?;
        let can_remove_cursor = remaining == 0
            && deleted_rows < max_rows
            && cursor_bytes.is_none_or(|bytes| deleted_bytes.saturating_add(bytes) <= max_bytes);
        let complete = if can_remove_cursor {
            if let Some(cursor_bytes) = cursor_bytes {
                transaction
                    .execute(
                        "DELETE FROM session_scan_cursors
                         WHERE source_key = ?1 AND generation = ?2",
                        params![source_key, generation],
                    )
                    .map_err(map_database_error)?;
                deleted_rows += 1;
                deleted_bytes += cursor_bytes;
            }
            let pointer = if staging == Some(database_u64(generation, "generation")?) {
                "staging_generation"
            } else {
                "retired_generation"
            };
            transaction
                .execute(
                    &format!(
                        "UPDATE session_sources SET {pointer} = NULL
                         WHERE source_key = ?1 AND {pointer} = ?2"
                    ),
                    params![source_key, generation],
                )
                .map_err(map_database_error)?;
            true
        } else {
            false
        };
        transaction.commit().map_err(map_database_error)?;
        Ok(CleanupBatchOutcome {
            deleted_rows,
            deleted_bytes,
            complete,
        })
    }

    pub fn load_session_sources_page(
        &self,
        after: Option<&SessionSourcePageKey>,
        limit: usize,
    ) -> Result<SessionSourcePage, StorageError> {
        validate_page_limit(limit, MAX_SESSION_BATCH_ROWS)?;
        let after_source_key = after.map_or("", |key| key.source_key.as_str());
        let mut statement = self
            .connection
            .prepare(
                "SELECT source_key, source_kind, current_generation, staging_generation,
                        retired_generation, status, error_code, last_transition_at
                 FROM session_sources
                 WHERE source_key > ?1
                 ORDER BY source_key
                 LIMIT ?2",
            )
            .map_err(map_database_error)?;
        let raw = statement
            .query_map(
                params![after_source_key, usize_to_i64(limit + 1, "page limit")?],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .map_err(map_database_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_database_error)?;
        let mut items = raw
            .into_iter()
            .map(session_source_from_database)
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = items.len() > limit;
        items.truncate(limit);
        let next_page_key = has_more.then(|| SessionSourcePageKey {
            source_key: items
                .last()
                .expect("a source page with more rows is not empty")
                .source_key
                .clone(),
        });
        Ok(SessionSourcePage {
            items,
            next_page_key,
        })
    }

    pub fn load_global_current_session_index_page(
        &self,
        after: Option<&GlobalSessionIndexPageKey>,
        limit: usize,
    ) -> Result<GlobalSessionIndexPage, StorageError> {
        validate_page_limit(limit, 200)?;
        let (sql, values): (&str, Vec<rusqlite::types::Value>) = match after {
            None => (
                GLOBAL_CURRENT_SESSION_INDEX_FIRST_PAGE_SQL,
                vec![usize_to_i64(limit + 1, "page limit")?.into()],
            ),
            Some(key) => (
                GLOBAL_CURRENT_SESSION_INDEX_AFTER_PAGE_SQL,
                vec![
                    key.last_active_at.clone().into(),
                    key.session_key.clone().into(),
                    key.source_key.clone().into(),
                    usize_to_i64(limit + 1, "page limit")?.into(),
                ],
            ),
        };
        let mut statement = self.connection.prepare(sql).map_err(map_database_error)?;
        let raw = statement
            .query_map(rusqlite::params_from_iter(values), index_database_row)
            .map_err(map_database_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_database_error)?;
        let mut items = raw
            .into_iter()
            .map(index_record_from_database)
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = items.len() > limit;
        items.truncate(limit);
        let next_page_key = has_more.then(|| {
            let last = items
                .last()
                .expect("a global index page with more rows is not empty");
            GlobalSessionIndexPageKey {
                last_active_at: last.last_active_at.clone(),
                session_key: last.session_key.clone(),
                source_key: last.source_key.clone(),
            }
        });
        Ok(GlobalSessionIndexPage {
            items,
            next_page_key,
        })
    }

    pub fn load_global_current_session_usage_page(
        &self,
        after: Option<&GlobalSessionUsagePageKey>,
        limit: usize,
    ) -> Result<GlobalSessionUsagePage, StorageError> {
        validate_page_limit(limit, 500)?;
        let (sql, values): (&str, Vec<rusqlite::types::Value>) = match after {
            None => (
                GLOBAL_CURRENT_SESSION_USAGE_FIRST_PAGE_SQL,
                vec![usize_to_i64(limit + 1, "page limit")?.into()],
            ),
            Some(key) => (
                GLOBAL_CURRENT_SESSION_USAGE_AFTER_PAGE_SQL,
                vec![
                    key.occurred_at.clone().into(),
                    key.usage_id.clone().into(),
                    key.source_key.clone().into(),
                    usize_to_i64(limit + 1, "page limit")?.into(),
                ],
            ),
        };
        let mut statement = self.connection.prepare(sql).map_err(map_database_error)?;
        let raw = statement
            .query_map(rusqlite::params_from_iter(values), usage_database_row)
            .map_err(map_database_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_database_error)?;
        let mut items = raw
            .into_iter()
            .map(usage_record_from_database)
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = items.len() > limit;
        items.truncate(limit);
        let next_page_key = has_more.then(|| {
            let last = items
                .last()
                .expect("a global usage page with more rows is not empty");
            GlobalSessionUsagePageKey {
                occurred_at: last.occurred_at.clone(),
                usage_id: last.usage_id.clone(),
                source_key: last.source_key.clone(),
            }
        });
        Ok(GlobalSessionUsagePage {
            items,
            next_page_key,
        })
    }

    pub fn load_current_session_index_page(
        &self,
        source_key: &str,
        after: Option<&SessionIndexPageKey>,
        limit: usize,
    ) -> Result<SessionIndexPage, StorageError> {
        validate_page_limit(limit, 200)?;
        validate_opaque_key("source key", source_key)?;
        if after.is_some_and(|key| key.source_key != source_key) {
            return Err(StorageError::StalePageKey);
        }
        let (sql, values): (&str, Vec<rusqlite::types::Value>) = match after {
            None => (
                "SELECT s.current_generation, i.session_key, i.source_key, i.generation,
                        i.source_kind, i.created_at, i.last_active_at, i.message_count,
                        i.usage_event_count,
                        CASE WHEN s.status = 'available'
                             THEN i.availability ELSE 'unavailable' END
                 FROM session_sources s
                 LEFT JOIN session_index i
                   ON i.source_key = s.source_key
                  AND i.generation = s.current_generation
                 WHERE s.source_key = ?1
                 ORDER BY i.last_active_at DESC, i.session_key
                 LIMIT ?2",
                vec![
                    source_key.to_owned().into(),
                    usize_to_i64(limit + 1, "page limit")?.into(),
                ],
            ),
            Some(key) => (
                "SELECT s.current_generation, i.session_key, i.source_key, i.generation,
                        i.source_kind, i.created_at, i.last_active_at, i.message_count,
                        i.usage_event_count,
                        CASE WHEN s.status = 'available'
                             THEN i.availability ELSE 'unavailable' END
                 FROM session_sources s
                 LEFT JOIN session_index i
                   ON i.source_key = s.source_key
                  AND i.generation = s.current_generation
                  AND (i.last_active_at < ?2
                       OR (i.last_active_at = ?2 AND i.session_key > ?3))
                 WHERE s.source_key = ?1
                 ORDER BY i.last_active_at DESC, i.session_key
                 LIMIT ?4",
                vec![
                    source_key.to_owned().into(),
                    key.last_active_at.clone().into(),
                    key.session_key.clone().into(),
                    usize_to_i64(limit + 1, "page limit")?.into(),
                ],
            ),
        };
        let mut statement = self.connection.prepare(sql).map_err(map_database_error)?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(values), |row| {
                let generation = row.get::<_, Option<i64>>(0)?;
                let record = row
                    .get::<_, Option<String>>(1)?
                    .map(|session_key| {
                        Ok::<IndexDatabaseRow, rusqlite::Error>((
                            session_key,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                            row.get(8)?,
                            row.get(9)?,
                        ))
                    })
                    .transpose()?;
                Ok((generation, record))
            })
            .map_err(map_database_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_database_error)?;
        let generation = rows
            .first()
            .and_then(|(generation, _)| *generation)
            .map(|generation| database_u64(generation, "current generation"))
            .transpose()?;
        let Some(generation) = generation else {
            if after.is_some() {
                return Err(StorageError::StalePageKey);
            }
            return Ok(SessionIndexPage {
                items: Vec::new(),
                next_page_key: None,
            });
        };
        if after.is_some_and(|key| key.generation != generation) {
            return Err(StorageError::StalePageKey);
        }
        let raw = rows
            .into_iter()
            .filter_map(|(_, record)| record)
            .collect::<Vec<_>>();
        let mut items = raw
            .into_iter()
            .map(index_record_from_database)
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = items.len() > limit;
        items.truncate(limit);
        let next_page_key = has_more.then(|| {
            let last = items.last().expect("a page with more rows is not empty");
            SessionIndexPageKey {
                source_key: source_key.to_owned(),
                generation,
                last_active_at: last.last_active_at.clone(),
                session_key: last.session_key.clone(),
            }
        });
        Ok(SessionIndexPage {
            items,
            next_page_key,
        })
    }

    pub fn load_current_session_usage_page(
        &self,
        source_key: &str,
        after: Option<&SessionUsagePageKey>,
        limit: usize,
    ) -> Result<SessionUsagePage, StorageError> {
        validate_page_limit(limit, 500)?;
        validate_opaque_key("source key", source_key)?;
        if after.is_some_and(|key| key.source_key != source_key) {
            return Err(StorageError::StalePageKey);
        }
        let (sql, values): (&str, Vec<rusqlite::types::Value>) = match after {
            None => (
                "SELECT s.current_generation, u.usage_id, u.session_key, u.source_key,
                        u.generation, u.source_kind, u.model, u.occurred_at,
                        u.input_tokens, u.output_tokens, u.cache_read_tokens,
                        u.cache_write_tokens, u.reasoning_tokens, u.record_revision
                 FROM session_sources s
                 LEFT JOIN session_usage_records u
                   ON u.source_key = s.source_key
                  AND u.generation = s.current_generation
                 WHERE s.source_key = ?1
                 ORDER BY u.occurred_at, u.usage_id
                 LIMIT ?2",
                vec![
                    source_key.to_owned().into(),
                    usize_to_i64(limit + 1, "page limit")?.into(),
                ],
            ),
            Some(key) => (
                "SELECT s.current_generation, u.usage_id, u.session_key, u.source_key,
                        u.generation, u.source_kind, u.model, u.occurred_at,
                        u.input_tokens, u.output_tokens, u.cache_read_tokens,
                        u.cache_write_tokens, u.reasoning_tokens, u.record_revision
                 FROM session_sources s
                 LEFT JOIN session_usage_records u
                   ON u.source_key = s.source_key
                  AND u.generation = s.current_generation
                  AND (u.occurred_at > ?2
                       OR (u.occurred_at = ?2 AND u.usage_id > ?3))
                 WHERE s.source_key = ?1
                 ORDER BY u.occurred_at, u.usage_id
                 LIMIT ?4",
                vec![
                    source_key.to_owned().into(),
                    key.occurred_at.clone().into(),
                    key.usage_id.clone().into(),
                    usize_to_i64(limit + 1, "page limit")?.into(),
                ],
            ),
        };
        let mut statement = self.connection.prepare(sql).map_err(map_database_error)?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(values), |row| {
                let generation = row.get::<_, Option<i64>>(0)?;
                let record = row
                    .get::<_, Option<String>>(1)?
                    .map(|usage_id| {
                        Ok::<UsageDatabaseRow, rusqlite::Error>((
                            usage_id,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                            row.get(8)?,
                            row.get(9)?,
                            row.get(10)?,
                            row.get(11)?,
                            row.get(12)?,
                            row.get(13)?,
                        ))
                    })
                    .transpose()?;
                Ok((generation, record))
            })
            .map_err(map_database_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_database_error)?;
        let generation = rows
            .first()
            .and_then(|(generation, _)| *generation)
            .map(|generation| database_u64(generation, "current generation"))
            .transpose()?;
        let Some(generation) = generation else {
            if after.is_some() {
                return Err(StorageError::StalePageKey);
            }
            return Ok(SessionUsagePage {
                items: Vec::new(),
                next_page_key: None,
            });
        };
        if after.is_some_and(|key| key.generation != generation) {
            return Err(StorageError::StalePageKey);
        }
        let raw = rows
            .into_iter()
            .filter_map(|(_, record)| record)
            .collect::<Vec<_>>();
        let mut items = raw
            .into_iter()
            .map(usage_record_from_database)
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = items.len() > limit;
        items.truncate(limit);
        let next_page_key = has_more.then(|| {
            let last = items.last().expect("a page with more rows is not empty");
            SessionUsagePageKey {
                source_key: source_key.to_owned(),
                generation,
                occurred_at: last.occurred_at.clone(),
                usage_id: last.usage_id.clone(),
            }
        });
        Ok(SessionUsagePage {
            items,
            next_page_key,
        })
    }

    pub fn load_codex_replay_signature_page(
        &self,
        parent_source_key: &str,
        parent_generation: u64,
        after: Option<&ReplaySignaturePageKey>,
        limit: usize,
    ) -> Result<CodexReplaySignaturePage, StorageError> {
        validate_page_limit(limit, MAX_SESSION_BATCH_ROWS)?;
        validate_opaque_key("parent source key", parent_source_key)?;
        if after.is_some_and(|key| {
            key.parent_source_key != parent_source_key || key.parent_generation != parent_generation
        }) {
            return Err(StorageError::StalePageKey);
        }
        let after_ordinal = after.map_or(0, |key| key.token_event_ordinal);
        let mut statement = self
            .connection
            .prepare(
                "SELECT s.current_generation, r.parent_source_key, r.parent_generation,
                        r.token_event_ordinal, r.occurred_at, r.signature_hash
                 FROM session_sources s
                 LEFT JOIN codex_replay_signatures r
                   ON r.parent_source_key = s.source_key
                  AND r.parent_generation = s.current_generation
                  AND r.parent_generation = ?2
                  AND r.token_event_ordinal > ?3
                 WHERE s.source_key = ?1
                 ORDER BY r.token_event_ordinal
                 LIMIT ?4",
            )
            .map_err(map_database_error)?;
        let mut rows = statement
            .query(params![
                parent_source_key,
                to_i64(parent_generation, "generation")?,
                to_i64(after_ordinal, "replay ordinal")?,
                usize_to_i64(limit + 1, "page limit")?,
            ])
            .map_err(map_database_error)?;
        let mut current_generation = None;
        let mut items = Vec::with_capacity(limit.saturating_add(1));
        while let Some(row) = rows.next().map_err(map_database_error)? {
            let generation = row.get::<_, Option<i64>>(0).map_err(map_database_error)?;
            if current_generation.is_none() {
                current_generation = generation
                    .map(|generation| database_u64(generation, "current generation"))
                    .transpose()?;
            }
            let signature = row
                .get::<_, Option<String>>(1)
                .map_err(map_database_error)?
                .map(|parent_source_key| {
                    Ok::<ReplayDatabaseRow, rusqlite::Error>((
                        parent_source_key,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                })
                .transpose()
                .map_err(map_database_error)?;
            if let Some(signature) = signature {
                items.push(replay_signature_from_database(signature)?);
            }
        }
        drop(rows);
        drop(statement);
        if current_generation != Some(parent_generation) {
            return Err(StorageError::StalePageKey);
        }
        let has_more = items.len() > limit;
        items.truncate(limit);
        let next_page_key = has_more.then(|| {
            let last = items.last().expect("a page with more rows is not empty");
            ReplaySignaturePageKey {
                parent_source_key: parent_source_key.to_owned(),
                parent_generation,
                token_event_ordinal: last.token_event_ordinal,
            }
        });
        Ok(CodexReplaySignaturePage {
            items,
            next_page_key,
        })
    }

    pub fn codex_replay_index_is_complete(
        &self,
        parent_source_key: &str,
        parent_generation: u64,
        expected_events: u64,
    ) -> Result<bool, StorageError> {
        validate_opaque_key("parent source key", parent_source_key)?;
        validate_generation(parent_generation)?;
        let (count, minimum, maximum) = self
            .connection
            .query_row(
                "SELECT COUNT(*), MIN(token_event_ordinal), MAX(token_event_ordinal)
                 FROM codex_replay_signatures
                 WHERE parent_source_key = ?1 AND parent_generation = ?2",
                params![
                    parent_source_key,
                    to_i64(parent_generation, "parent generation")?
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .map_err(map_database_error)?;
        let count = database_u64(count, "replay signature count")?;
        let minimum = optional_u64(minimum, "minimum replay ordinal")?;
        let maximum = optional_u64(maximum, "maximum replay ordinal")?;
        Ok(if expected_events == 0 {
            count == 0 && minimum.is_none() && maximum.is_none()
        } else {
            count == expected_events && minimum == Some(1) && maximum == Some(expected_events)
        })
    }

    pub fn orphan_secret_refs(&self) -> Result<Vec<SecretRef>, StorageError> {
        let mut statement = self
            .connection
            .prepare("SELECT secret_ref FROM orphan_secrets ORDER BY secret_ref")
            .map_err(map_database_error)?;
        let references = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(map_database_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_database_error)?;

        references
            .into_iter()
            .map(|secret_ref| {
                SecretRef::parse(secret_ref).map_err(|_| StorageError::StateDatabaseCorrupt {
                    message: "orphan secret metadata contains an invalid reference".to_owned(),
                })
            })
            .collect()
    }

    pub fn wal_size_bytes(&self) -> Result<u64, StorageError> {
        match fs::metadata(self.wal_path()) {
            Ok(metadata) => Ok(metadata.len()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(source) => Err(StorageError::Io { source }),
        }
    }

    pub fn checkpoint_passive_if_at_least(
        &self,
        threshold_bytes: u64,
    ) -> Result<Option<CheckpointResult>, StorageError> {
        if self.wal_size_bytes()? < threshold_bytes {
            return Ok(None);
        }

        self.checkpoint("PRAGMA wal_checkpoint(PASSIVE)").map(Some)
    }

    pub fn checkpoint_truncate(&self) -> Result<CheckpointResult, StorageError> {
        self.checkpoint("PRAGMA wal_checkpoint(TRUNCATE)")
    }

    fn checkpoint(&self, pragma: &str) -> Result<CheckpointResult, StorageError> {
        self.connection
            .query_row(pragma, [], |row| {
                Ok(CheckpointResult {
                    busy: row.get::<_, i64>(0)? != 0,
                    log_frames: row.get(1)?,
                    checkpointed_frames: row.get(2)?,
                })
            })
            .map_err(map_database_error)
    }

    pub fn pragma_journal_mode(&self) -> Result<String, StorageError> {
        self.pragma_value("journal_mode")
    }

    pub fn pragma_foreign_keys(&self) -> Result<i64, StorageError> {
        self.pragma_value("foreign_keys")
    }

    pub fn pragma_busy_timeout(&self) -> Result<i64, StorageError> {
        self.pragma_value("busy_timeout")
    }

    pub fn pragma_wal_autocheckpoint(&self) -> Result<i64, StorageError> {
        self.pragma_value("wal_autocheckpoint")
    }

    fn pragma_value<T>(&self, name: &str) -> Result<T, StorageError>
    where
        T: rusqlite::types::FromSql,
    {
        self.connection
            .query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
            .map_err(map_database_error)
    }
}

type IndexDatabaseRow = (
    String,
    String,
    i64,
    String,
    String,
    String,
    i64,
    i64,
    String,
);

type UsageDatabaseRow = (
    String,
    String,
    String,
    i64,
    String,
    String,
    String,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
);

type ReplayDatabaseRow = (String, i64, i64, String, Vec<u8>);

type SupplementalDatabaseRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    Option<i64>,
    Option<String>,
);

type SourceDatabaseRow = (
    String,
    String,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    String,
    Option<String>,
    Option<String>,
);

type CursorDatabaseRow = (
    String,
    String,
    i64,
    String,
    String,
    i64,
    String,
    i64,
    i64,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    Option<String>,
    Option<i64>,
    Option<Vec<u8>>,
    Option<String>,
    Option<String>,
);

fn invalid_state(message: &str) -> StorageError {
    StorageError::InvalidStateRecord {
        message: message.to_owned(),
    }
}

fn validate_session_batch(batch: &SessionBatch) -> Result<(), StorageError> {
    let rows = batch.index_records.len()
        + batch.usage_records.len()
        + batch.replay_signatures.len()
        + batch.supplemental_metadata.len();
    if rows > MAX_SESSION_BATCH_ROWS {
        return Err(StorageError::SessionBatchLimitExceeded);
    }
    if let Some(cursor) = &batch.cursor {
        validate_cursor(cursor)?;
    } else if !batch.index_records.is_empty()
        || !batch.usage_records.is_empty()
        || !batch.replay_signatures.is_empty()
    {
        return Err(invalid_state(
            "Session-derived records require an atomic cursor",
        ));
    }

    let mut logical_bytes = batch.cursor.as_ref().map_or(0, cursor_logical_bytes);
    for record in &batch.index_records {
        validate_index_record(record, batch.cursor.as_ref())?;
        logical_bytes = logical_bytes.saturating_add(index_logical_bytes(record));
    }
    for record in &batch.usage_records {
        validate_usage_record(record, batch.cursor.as_ref())?;
        logical_bytes = logical_bytes.saturating_add(usage_logical_bytes(record));
    }
    for signature in &batch.replay_signatures {
        validate_replay_signature(signature, batch.cursor.as_ref())?;
        logical_bytes = logical_bytes.saturating_add(replay_logical_bytes(signature));
    }
    for metadata in &batch.supplemental_metadata {
        let bytes = validate_supplemental_metadata(metadata)?;
        logical_bytes = logical_bytes.saturating_add(bytes);
    }
    if logical_bytes > MAX_SESSION_BATCH_BYTES {
        return Err(StorageError::SessionBatchLimitExceeded);
    }
    Ok(())
}

fn validate_cursor(cursor: &SessionScanCursor) -> Result<(), StorageError> {
    validate_opaque_key("source key", &cursor.source_key)?;
    validate_generation(cursor.generation)?;
    validate_opaque_key("file identity", cursor.file_identity.as_str())?;
    validate_timestamp("cursor modification timestamp", &cursor.modified_at)?;
    to_i64(cursor.observed_size, "observed size")?;
    to_i64(cursor.complete_byte_offset, "complete byte offset")?;
    to_i64(cursor.stable_record_ordinal, "stable record ordinal")?;
    if cursor.complete_byte_offset > cursor.observed_size {
        return Err(invalid_state(
            "complete byte offset exceeds the observed size",
        ));
    }
    validate_checkpoint(&cursor.parser_checkpoint)?;
    match (
        &cursor.parent_source_key,
        cursor.parent_generation,
        cursor.replay_boundary_fingerprint,
    ) {
        (None, None, None) => {}
        (Some(parent_source_key), Some(parent_generation), Some(_)) => {
            validate_opaque_key("parent source key", parent_source_key)?;
            validate_generation(parent_generation)?;
        }
        _ => {
            return Err(invalid_state(
                "parent source, generation, and replay anchor must be present together",
            ));
        }
    }
    if cursor.parser_checkpoint.lineage_source_key != cursor.parent_source_key
        || cursor.parser_checkpoint.lineage_generation != cursor.parent_generation
    {
        return Err(invalid_state(
            "parser checkpoint lineage must match the cursor parent lineage",
        ));
    }
    if let Some(result_code) = &cursor.result_code {
        validate_stable_code("result code", result_code.as_str())?;
    }
    match (&cursor.result_code, &cursor.result_changed_at) {
        (None, None) => {}
        (Some(_), Some(timestamp)) => validate_timestamp("result transition timestamp", timestamp)?,
        _ => {
            return Err(invalid_state(
                "result code and transition timestamp must be present together",
            ));
        }
    }
    Ok(())
}

fn validate_checkpoint(checkpoint: &ParserCheckpoint) -> Result<(), StorageError> {
    if checkpoint.version == 0 {
        return Err(invalid_state("parser checkpoint version must be positive"));
    }
    if let Some(model) = &checkpoint.current_model {
        validate_bounded_text("checkpoint model", model, 256, false)?;
    }
    match (
        &checkpoint.lineage_source_key,
        checkpoint.lineage_generation,
    ) {
        (None, None) => {
            if checkpoint.lineage_record_ordinal != 0 {
                return Err(invalid_state(
                    "lineage ordinal requires a lineage source and generation",
                ));
            }
        }
        (Some(source_key), Some(generation)) => {
            validate_opaque_key("checkpoint lineage source key", source_key)?;
            validate_generation(generation)?;
        }
        _ => {
            return Err(invalid_state(
                "checkpoint lineage source and generation must be present together",
            ));
        }
    }
    if encode_checkpoint(checkpoint)?.len() > MAX_PARSER_CHECKPOINT_BYTES {
        return Err(invalid_state("parser checkpoint exceeds 64 KiB"));
    }
    Ok(())
}

fn validate_index_record(
    record: &SessionIndexRecord,
    cursor: Option<&SessionScanCursor>,
) -> Result<(), StorageError> {
    validate_opaque_key("session key", &record.session_key)?;
    validate_opaque_key("source key", &record.source_key)?;
    validate_generation(record.generation)?;
    validate_timestamp("Session creation timestamp", &record.created_at)?;
    validate_timestamp("Session activity timestamp", &record.last_active_at)?;
    to_i64(record.message_count, "message count")?;
    to_i64(record.usage_event_count, "usage event count")?;
    validate_record_owner(
        &record.source_key,
        record.generation,
        record.source_kind,
        cursor,
    )
}

fn validate_usage_record(
    record: &SessionUsageRecord,
    cursor: Option<&SessionScanCursor>,
) -> Result<(), StorageError> {
    validate_opaque_key("usage identifier", &record.usage_id)?;
    validate_opaque_key("session key", &record.session_key)?;
    validate_opaque_key("source key", &record.source_key)?;
    validate_generation(record.generation)?;
    validate_bounded_text("normalized model", &record.model, 256, false)?;
    validate_timestamp("usage timestamp", &record.occurred_at)?;
    for (name, value) in [
        ("input tokens", record.input_tokens),
        ("output tokens", record.output_tokens),
        ("cache read tokens", record.cache_read_tokens),
        ("cache write tokens", record.cache_write_tokens),
        ("reasoning tokens", record.reasoning_tokens),
        ("record revision", record.record_revision),
    ] {
        to_i64(value, name)?;
    }
    if record.record_revision == 0 {
        return Err(invalid_state("usage record revision must be positive"));
    }
    validate_record_owner(
        &record.source_key,
        record.generation,
        record.source_kind,
        cursor,
    )
}

fn validate_replay_signature(
    signature: &CodexReplaySignature,
    cursor: Option<&SessionScanCursor>,
) -> Result<(), StorageError> {
    validate_opaque_key("parent source key", &signature.parent_source_key)?;
    validate_generation(signature.parent_generation)?;
    if signature.token_event_ordinal == 0 {
        return Err(invalid_state("replay signature ordinal must be positive"));
    }
    to_i64(signature.token_event_ordinal, "replay ordinal")?;
    validate_timestamp("replay signature timestamp", &signature.occurred_at)?;
    let cursor = cursor.ok_or_else(|| {
        invalid_state("replay signatures require an atomic cursor for their generation")
    })?;
    if cursor.source_key != signature.parent_source_key
        || cursor.generation != signature.parent_generation
        || cursor.source_kind != SessionSourceKind::Codex
    {
        return Err(invalid_state(
            "replay signature owner does not match the batch cursor",
        ));
    }
    Ok(())
}

fn validate_supplemental_metadata(
    metadata: &RequestSupplementalMetadata,
) -> Result<usize, StorageError> {
    validate_correlation_id("request identifier", metadata.request_id.as_str())?;
    validate_correlation_id("attempt identifier", metadata.attempt_id.as_str())?;
    validate_correlation_id("trace identifier", metadata.trace_id.as_str())?;
    validate_timestamp("supplemental timestamp", &metadata.occurred_at)?;
    validate_opaque_key("route fingerprint", metadata.route_fingerprint.as_str())?;
    validate_opaque_key(
        "provider fingerprint",
        metadata.provider_fingerprint.as_str(),
    )?;
    if let Some(account) = &metadata.account_fingerprint {
        validate_opaque_key("account fingerprint", account.as_str())?;
    }
    if let Some(error_code) = &metadata.error_code {
        validate_stable_code("supplemental error code", error_code.as_str())?;
    }
    if metadata
        .status_code
        .is_some_and(|status| !(100..=599).contains(&status))
    {
        return Err(invalid_state("HTTP status is outside 100 through 599"));
    }
    for (name, value) in [
        ("queue duration", metadata.queue_ms),
        ("connect duration", metadata.connect_ms),
        ("first-byte duration", metadata.first_byte_ms),
        ("total duration", metadata.total_ms),
        ("request size", metadata.request_bytes),
        ("response size", metadata.response_bytes),
    ] {
        to_i64(value, name)?;
    }
    let bytes = supplemental_logical_bytes(metadata);
    if bytes > MAX_SUPPLEMENTAL_ROW_BYTES {
        return Err(invalid_state("supplemental metadata row exceeds 2 KiB"));
    }
    Ok(bytes)
}

fn validate_record_owner(
    source_key: &str,
    generation: u64,
    source_kind: SessionSourceKind,
    cursor: Option<&SessionScanCursor>,
) -> Result<(), StorageError> {
    let cursor =
        cursor.ok_or_else(|| invalid_state("Session-derived records require an atomic cursor"))?;
    if cursor.source_key != source_key
        || cursor.generation != generation
        || cursor.source_kind != source_kind
    {
        return Err(invalid_state(
            "Session-derived record owner does not match the batch cursor",
        ));
    }
    Ok(())
}

fn validate_opaque_key(name: &str, value: &str) -> Result<(), StorageError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(invalid_state(&format!(
            "{name} must be a 64-character lowercase hexadecimal identifier"
        )));
    }
    Ok(())
}

fn validate_generation(generation: u64) -> Result<(), StorageError> {
    if generation == 0 {
        return Err(invalid_state("generation must be positive"));
    }
    to_i64(generation, "generation").map(|_| ())
}

fn validate_timestamp(name: &str, value: &str) -> Result<(), StorageError> {
    let bytes = value.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
        || bytes.iter().enumerate().any(|(index, byte)| {
            !matches!(index, 4 | 7 | 10 | 13 | 16 | 19) && !byte.is_ascii_digit()
        })
    {
        return Err(invalid_state(&format!(
            "{name} must use YYYY-MM-DDTHH:MM:SSZ"
        )));
    }
    let year = parse_timestamp_component(&bytes[0..4]);
    let month = parse_timestamp_component(&bytes[5..7]);
    let day = parse_timestamp_component(&bytes[8..10]);
    let hour = parse_timestamp_component(&bytes[11..13]);
    let minute = parse_timestamp_component(&bytes[14..16]);
    let second = parse_timestamp_component(&bytes[17..19]);
    let maximum_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    };
    if year == 0 || day == 0 || day > maximum_day || hour > 23 || minute > 59 || second > 59 {
        return Err(invalid_state(&format!(
            "{name} contains an impossible UTC date or time"
        )));
    }
    Ok(())
}

fn parse_timestamp_component(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .fold(0, |value, byte| value * 10 + u32::from(byte - b'0'))
}

const fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

fn validate_correlation_id(name: &str, value: &str) -> Result<(), StorageError> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(invalid_state(&format!(
            "{name} must be an opaque correlation identifier"
        )));
    }
    Ok(())
}

fn validate_stable_code(name: &str, value: &str) -> Result<(), StorageError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(invalid_state(&format!(
            "{name} must be a lowercase stable code"
        )));
    }
    Ok(())
}

fn validate_bounded_text(
    name: &str,
    value: &str,
    maximum: usize,
    allow_empty: bool,
) -> Result<(), StorageError> {
    if (!allow_empty && value.is_empty())
        || value.len() > maximum
        || value.chars().any(char::is_control)
    {
        return Err(invalid_state(&format!("{name} is outside its safe bound")));
    }
    Ok(())
}

fn validate_page_limit(limit: usize, maximum: usize) -> Result<(), StorageError> {
    if limit == 0 || limit > maximum {
        return Err(invalid_state("page limit is outside its bound"));
    }
    Ok(())
}

fn cursor_logical_bytes(cursor: &SessionScanCursor) -> usize {
    cursor.source_key.len()
        + cursor.file_identity.as_str().len()
        + cursor.modified_at.len()
        + cursor
            .result_code
            .as_ref()
            .map_or(0, |code| code.as_str().len())
        + cursor.result_changed_at.as_ref().map_or(0, String::len)
        + cursor.parent_source_key.as_ref().map_or(0, String::len)
        + encode_checkpoint(&cursor.parser_checkpoint).map_or(usize::MAX, |bytes| bytes.len())
        + 128
}

fn index_logical_bytes(record: &SessionIndexRecord) -> usize {
    record.session_key.len()
        + record.source_key.len()
        + record.created_at.len()
        + record.last_active_at.len()
        + 40
}

fn usage_logical_bytes(record: &SessionUsageRecord) -> usize {
    record.usage_id.len()
        + record.session_key.len()
        + record.source_key.len()
        + record.model.len()
        + record.occurred_at.len()
        + 64
}

fn replay_logical_bytes(signature: &CodexReplaySignature) -> usize {
    signature.parent_source_key.len() + signature.occurred_at.len() + 48
}

fn supplemental_logical_bytes(metadata: &RequestSupplementalMetadata) -> usize {
    metadata.request_id.as_str().len()
        + metadata.attempt_id.as_str().len()
        + metadata.trace_id.as_str().len()
        + metadata.occurred_at.len()
        + metadata.route_fingerprint.as_str().len()
        + metadata.provider_fingerprint.as_str().len()
        + metadata
            .account_fingerprint
            .as_ref()
            .map_or(0, |fingerprint| fingerprint.as_str().len())
        + metadata.retry_decision.as_str().len()
        + metadata.failover_decision.as_str().len()
        + metadata
            .error_code
            .as_ref()
            .map_or(0, |code| code.as_str().len())
        + 64
}

fn encode_checkpoint(checkpoint: &ParserCheckpoint) -> Result<Vec<u8>, StorageError> {
    let model = checkpoint.current_model.as_deref().unwrap_or("");
    let lineage_source = checkpoint.lineage_source_key.as_deref().unwrap_or("");
    let lineage_generation = checkpoint
        .lineage_generation
        .map_or_else(String::new, |value| value.to_string());
    let structural_hash = checkpoint
        .structural_hash
        .as_ref()
        .map_or_else(String::new, hex_encode);
    Ok(format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        checkpoint.version,
        checkpoint.previous_input_tokens,
        checkpoint.previous_output_tokens,
        checkpoint.previous_cache_read_tokens,
        checkpoint.previous_cache_write_tokens,
        checkpoint.previous_reasoning_tokens,
        model,
        checkpoint.event_ordinal,
        lineage_source,
        lineage_generation,
        checkpoint.lineage_record_ordinal,
        structural_hash,
    )
    .into_bytes())
}

fn decode_checkpoint(bytes: &[u8]) -> Result<ParserCheckpoint, StorageError> {
    if bytes.len() > MAX_PARSER_CHECKPOINT_BYTES {
        return Err(StorageError::StateDatabaseCorrupt {
            message: "parser checkpoint exceeds its bound".to_owned(),
        });
    }
    let text = std::str::from_utf8(bytes).map_err(|_| StorageError::StateDatabaseCorrupt {
        message: "parser checkpoint is not valid UTF-8".to_owned(),
    })?;
    let fields = text.split('\n').collect::<Vec<_>>();
    if fields.len() != 12 {
        return Err(StorageError::StateDatabaseCorrupt {
            message: "parser checkpoint has an invalid field count".to_owned(),
        });
    }
    let parse_u64 = |index: usize, name: &str| {
        fields[index]
            .parse::<u64>()
            .map_err(|_| StorageError::StateDatabaseCorrupt {
                message: format!("parser checkpoint contains an invalid {name}"),
            })
    };
    let version = fields[0]
        .parse::<u16>()
        .map_err(|_| StorageError::StateDatabaseCorrupt {
            message: "parser checkpoint contains an invalid version".to_owned(),
        })?;
    let current_model = (!fields[6].is_empty()).then(|| fields[6].to_owned());
    let lineage_source_key = (!fields[8].is_empty()).then(|| fields[8].to_owned());
    let lineage_generation = if fields[9].is_empty() {
        None
    } else {
        Some(parse_u64(9, "lineage generation")?)
    };
    let structural_hash = if fields[11].is_empty() {
        None
    } else {
        Some(hex_decode_32(fields[11])?)
    };
    let checkpoint = ParserCheckpoint {
        version,
        previous_input_tokens: parse_u64(1, "input counter")?,
        previous_output_tokens: parse_u64(2, "output counter")?,
        previous_cache_read_tokens: parse_u64(3, "cache-read counter")?,
        previous_cache_write_tokens: parse_u64(4, "cache-write counter")?,
        previous_reasoning_tokens: parse_u64(5, "reasoning counter")?,
        current_model,
        event_ordinal: parse_u64(7, "event ordinal")?,
        lineage_source_key,
        lineage_generation,
        lineage_record_ordinal: parse_u64(10, "lineage ordinal")?,
        structural_hash,
    };
    validate_checkpoint(&checkpoint).map_err(|_| StorageError::StateDatabaseCorrupt {
        message: "parser checkpoint violates its typed bounds".to_owned(),
    })?;
    Ok(checkpoint)
}

fn hex_encode(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(64);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn hex_decode_32(value: &str) -> Result<[u8; 32], StorageError> {
    if value.len() != 64 {
        return Err(StorageError::StateDatabaseCorrupt {
            message: "parser checkpoint contains an invalid structural hash".to_owned(),
        });
    }
    let mut decoded = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(pair[0])?;
        let low = hex_nibble(pair[1])?;
        decoded[index] = (high << 4) | low;
    }
    Ok(decoded)
}

fn hex_nibble(value: u8) -> Result<u8, StorageError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(StorageError::StateDatabaseCorrupt {
            message: "parser checkpoint contains an invalid structural hash".to_owned(),
        }),
    }
}

fn insert_cursor(connection: &Connection, cursor: &SessionScanCursor) -> Result<(), StorageError> {
    let checkpoint = encode_checkpoint(&cursor.parser_checkpoint)?;
    connection
        .execute(
            "INSERT INTO session_scan_cursors(
                source_key, generation, source_kind, generation_state, file_identity,
                observed_size, modified_at, complete_byte_offset, stable_record_ordinal,
                parser_checkpoint, head_fingerprint, boundary_fingerprint,
                parent_source_key, parent_generation, replay_boundary_fingerprint,
                result_code, result_changed_at
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17
             )",
            params![
                cursor.source_key,
                to_i64(cursor.generation, "generation")?,
                cursor.source_kind.as_str(),
                cursor.generation_state.as_str(),
                cursor.file_identity.as_str(),
                to_i64(cursor.observed_size, "observed size")?,
                cursor.modified_at,
                to_i64(cursor.complete_byte_offset, "complete byte offset")?,
                to_i64(cursor.stable_record_ordinal, "record ordinal")?,
                checkpoint,
                cursor.head_fingerprint.as_slice(),
                cursor.boundary_fingerprint.as_slice(),
                cursor.parent_source_key,
                cursor
                    .parent_generation
                    .map(|value| to_i64(value, "parent generation"))
                    .transpose()?,
                cursor
                    .replay_boundary_fingerprint
                    .as_ref()
                    .map(<[u8; 32]>::as_slice),
                cursor.result_code.map(SessionScanResultCode::as_str),
                cursor.result_changed_at,
            ],
        )
        .map_err(map_database_error)?;
    Ok(())
}

fn upsert_cursor(connection: &Connection, cursor: &SessionScanCursor) -> Result<(), StorageError> {
    match query_cursor(connection, &cursor.source_key, cursor.generation)? {
        None => insert_cursor(connection, cursor),
        Some(existing) if existing == *cursor => Ok(()),
        Some(existing)
            if existing.source_kind == cursor.source_kind
                && existing.generation_state == cursor.generation_state
                && existing.file_identity == cursor.file_identity
                && existing.head_fingerprint == cursor.head_fingerprint
                && existing.parent_source_key == cursor.parent_source_key
                && existing.parent_generation == cursor.parent_generation
                && existing.replay_boundary_fingerprint == cursor.replay_boundary_fingerprint
                && cursor.observed_size >= existing.observed_size
                && cursor.modified_at >= existing.modified_at
                && cursor.complete_byte_offset >= existing.complete_byte_offset
                && cursor.stable_record_ordinal >= existing.stable_record_ordinal
                && (cursor.complete_byte_offset > existing.complete_byte_offset
                    || cursor.stable_record_ordinal > existing.stable_record_ordinal
                    || cursor.parser_checkpoint == existing.parser_checkpoint)
                && !(existing.result_code.is_some() && cursor.result_code.is_none())
                && (!matches!(
                    (&existing.result_changed_at, &cursor.result_changed_at),
                    (Some(existing), Some(current)) if current < existing
                ))
                && (cursor.boundary_fingerprint == existing.boundary_fingerprint
                    || cursor.complete_byte_offset > existing.complete_byte_offset
                    || cursor.stable_record_ordinal > existing.stable_record_ordinal) =>
        {
            let checkpoint = encode_checkpoint(&cursor.parser_checkpoint)?;
            connection
                .execute(
                    "UPDATE session_scan_cursors SET
                        observed_size = ?3, modified_at = ?4, complete_byte_offset = ?5,
                        stable_record_ordinal = ?6, parser_checkpoint = ?7,
                        boundary_fingerprint = ?8, parent_source_key = ?9,
                        parent_generation = ?10, replay_boundary_fingerprint = ?11,
                        result_code = ?12, result_changed_at = ?13
                     WHERE source_key = ?1 AND generation = ?2",
                    params![
                        cursor.source_key,
                        to_i64(cursor.generation, "generation")?,
                        to_i64(cursor.observed_size, "observed size")?,
                        cursor.modified_at,
                        to_i64(cursor.complete_byte_offset, "complete byte offset")?,
                        to_i64(cursor.stable_record_ordinal, "record ordinal")?,
                        checkpoint,
                        cursor.boundary_fingerprint.as_slice(),
                        cursor.parent_source_key,
                        cursor
                            .parent_generation
                            .map(|value| to_i64(value, "parent generation"))
                            .transpose()?,
                        cursor
                            .replay_boundary_fingerprint
                            .as_ref()
                            .map(<[u8; 32]>::as_slice),
                        cursor.result_code.map(SessionScanResultCode::as_str),
                        cursor.result_changed_at,
                    ],
                )
                .map_err(map_database_error)?;
            Ok(())
        }
        Some(_) => Err(StorageError::StableRecordConflict {
            record_kind: "Session scan cursor",
        }),
    }
}

fn query_cursor(
    connection: &Connection,
    source_key: &str,
    generation: u64,
) -> Result<Option<SessionScanCursor>, StorageError> {
    let raw = connection
        .query_row(
            "SELECT source_key, source_kind, generation, generation_state, file_identity,
                    observed_size, modified_at, complete_byte_offset, stable_record_ordinal,
                    parser_checkpoint, head_fingerprint, boundary_fingerprint,
                    parent_source_key, parent_generation, replay_boundary_fingerprint,
                    result_code, result_changed_at
             FROM session_scan_cursors WHERE source_key = ?1 AND generation = ?2",
            params![source_key, to_i64(generation, "generation")?],
            cursor_database_row,
        )
        .optional()
        .map_err(map_database_error)?;
    raw.map(cursor_from_database).transpose()
}

fn query_session_source(
    connection: &Connection,
    source_key: &str,
) -> Result<Option<SessionSourceState>, StorageError> {
    let raw = connection
        .query_row(
            "SELECT source_key, source_kind, current_generation, staging_generation,
                    retired_generation, status, error_code, last_transition_at
             FROM session_sources WHERE source_key = ?1",
            [source_key],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .optional()
        .map_err(map_database_error)?;
    raw.map(session_source_from_database).transpose()
}

fn failed_source_status(
    source: &SessionSourceState,
    error_code: SessionSourceErrorCode,
) -> SessionSourceStatus {
    if source.current_generation.is_some() {
        SessionSourceStatus::Stale
    } else if error_code.is_resource_limit() {
        SessionSourceStatus::ResourceLimited
    } else {
        SessionSourceStatus::Unavailable
    }
}

fn unavailable_source_status(source: &SessionSourceState) -> SessionSourceStatus {
    if source.current_generation.is_some() {
        SessionSourceStatus::Stale
    } else {
        SessionSourceStatus::Unavailable
    }
}

fn session_source_from_database(
    raw: SourceDatabaseRow,
) -> Result<SessionSourceState, StorageError> {
    let source = SessionSourceState {
        source_key: raw.0,
        source_kind: SessionSourceKind::from_database(&raw.1)?,
        current_generation: optional_u64(raw.2, "current generation")?,
        staging_generation: optional_u64(raw.3, "staging generation")?,
        retired_generation: optional_u64(raw.4, "retired generation")?,
        status: SessionSourceStatus::from_database(&raw.5)?,
        error_code: raw
            .6
            .as_deref()
            .map(SessionSourceErrorCode::from_database)
            .transpose()?,
        last_transition_at: raw.7,
    };
    validate_opaque_key("source key", &source.source_key).map_err(|_| {
        StorageError::StateDatabaseCorrupt {
            message: "Session source contains an invalid source key".to_owned(),
        }
    })?;
    if let Some(timestamp) = &source.last_transition_at {
        validate_timestamp("source transition timestamp", timestamp).map_err(|_| {
            StorageError::StateDatabaseCorrupt {
                message: "Session source contains an invalid transition timestamp".to_owned(),
            }
        })?;
    }
    Ok(source)
}

fn cursor_database_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CursorDatabaseRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
        row.get(14)?,
        row.get(15)?,
        row.get(16)?,
    ))
}

fn cursor_from_database(raw: CursorDatabaseRow) -> Result<SessionScanCursor, StorageError> {
    let cursor = SessionScanCursor {
        source_key: raw.0,
        source_kind: SessionSourceKind::from_database(&raw.1)?,
        generation: database_u64(raw.2, "generation")?,
        generation_state: SessionGenerationState::from_database(&raw.3)?,
        file_identity: SessionFileIdentity::from_database(raw.4)?,
        observed_size: database_u64(raw.5, "observed size")?,
        modified_at: raw.6,
        complete_byte_offset: database_u64(raw.7, "complete byte offset")?,
        stable_record_ordinal: database_u64(raw.8, "record ordinal")?,
        parser_checkpoint: decode_checkpoint(&raw.9)?,
        head_fingerprint: vec_to_array(raw.10, "head fingerprint")?,
        boundary_fingerprint: vec_to_array(raw.11, "boundary fingerprint")?,
        parent_source_key: raw.12,
        parent_generation: optional_u64(raw.13, "parent generation")?,
        replay_boundary_fingerprint: raw
            .14
            .map(|bytes| vec_to_array(bytes, "replay boundary fingerprint"))
            .transpose()?,
        result_code: raw
            .15
            .as_deref()
            .map(SessionScanResultCode::from_database)
            .transpose()?,
        result_changed_at: raw.16,
    };
    validate_cursor(&cursor).map_err(|_| StorageError::StateDatabaseCorrupt {
        message: "Session scan cursor violates its typed bounds".to_owned(),
    })?;
    Ok(cursor)
}

fn write_index_record(
    connection: &Connection,
    record: &SessionIndexRecord,
) -> Result<(), StorageError> {
    let existing = connection
        .query_row(
            "SELECT session_key, source_key, generation, source_kind, created_at,
                    last_active_at, message_count, usage_event_count, availability
             FROM session_index
             WHERE source_key = ?1 AND generation = ?2 AND session_key = ?3",
            params![
                record.source_key,
                to_i64(record.generation, "generation")?,
                record.session_key
            ],
            index_database_row,
        )
        .optional()
        .map_err(map_database_error)?
        .map(index_record_from_database)
        .transpose()?;
    match existing {
        Some(existing) if existing == *record => return Ok(()),
        Some(existing)
            if existing.source_kind == record.source_kind
                && existing.created_at == record.created_at
                && existing.availability == record.availability
                && record.last_active_at >= existing.last_active_at
                && record.message_count >= existing.message_count
                && record.usage_event_count >= existing.usage_event_count =>
        {
            connection
                .execute(
                    "UPDATE session_index SET
                        last_active_at = ?4, message_count = ?5, usage_event_count = ?6
                     WHERE source_key = ?1 AND generation = ?2 AND session_key = ?3",
                    params![
                        record.source_key,
                        to_i64(record.generation, "generation")?,
                        record.session_key,
                        record.last_active_at,
                        to_i64(record.message_count, "message count")?,
                        to_i64(record.usage_event_count, "usage event count")?,
                    ],
                )
                .map_err(map_database_error)?;
            return Ok(());
        }
        Some(_) => {
            return Err(StorageError::StableRecordConflict {
                record_kind: "Session index",
            });
        }
        None => {}
    }
    connection
        .execute(
            "INSERT INTO session_index(
                session_key, source_key, generation, source_kind, created_at, last_active_at,
                message_count, usage_event_count, availability
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                record.session_key,
                record.source_key,
                to_i64(record.generation, "generation")?,
                record.source_kind.as_str(),
                record.created_at,
                record.last_active_at,
                to_i64(record.message_count, "message count")?,
                to_i64(record.usage_event_count, "usage event count")?,
                record.availability.as_str(),
            ],
        )
        .map_err(map_database_error)?;
    Ok(())
}

fn write_usage_record(
    connection: &Connection,
    record: &SessionUsageRecord,
) -> Result<(), StorageError> {
    let existing = connection
        .query_row(
            "SELECT usage_id, session_key, source_key, generation, source_kind, model,
                    occurred_at, input_tokens, output_tokens, cache_read_tokens,
                    cache_write_tokens, reasoning_tokens, record_revision
             FROM session_usage_records
             WHERE source_key = ?1 AND generation = ?2 AND usage_id = ?3",
            params![
                record.source_key,
                to_i64(record.generation, "generation")?,
                record.usage_id
            ],
            usage_database_row,
        )
        .optional()
        .map_err(map_database_error)?
        .map(usage_record_from_database)
        .transpose()?;
    match existing {
        Some(existing) if existing == *record => return Ok(()),
        Some(existing) if record.record_revision <= existing.record_revision => {
            return Err(StorageError::StableRecordConflict {
                record_kind: "Session usage",
            });
        }
        Some(_) => {
            connection
                .execute(
                    "UPDATE session_usage_records SET
                        session_key = ?4, source_kind = ?5, model = ?6, occurred_at = ?7,
                        input_tokens = ?8, output_tokens = ?9, cache_read_tokens = ?10,
                        cache_write_tokens = ?11, reasoning_tokens = ?12, record_revision = ?13
                     WHERE source_key = ?1 AND generation = ?2 AND usage_id = ?3",
                    params![
                        record.source_key,
                        to_i64(record.generation, "generation")?,
                        record.usage_id,
                        record.session_key,
                        record.source_kind.as_str(),
                        record.model,
                        record.occurred_at,
                        to_i64(record.input_tokens, "input tokens")?,
                        to_i64(record.output_tokens, "output tokens")?,
                        to_i64(record.cache_read_tokens, "cache read tokens")?,
                        to_i64(record.cache_write_tokens, "cache write tokens")?,
                        to_i64(record.reasoning_tokens, "reasoning tokens")?,
                        to_i64(record.record_revision, "record revision")?,
                    ],
                )
                .map_err(map_database_error)?;
            return Ok(());
        }
        None => {}
    }
    connection
        .execute(
            "INSERT INTO session_usage_records(
                usage_id, session_key, source_key, generation, source_kind, model,
                occurred_at, input_tokens, output_tokens, cache_read_tokens,
                cache_write_tokens, reasoning_tokens, record_revision
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                record.usage_id,
                record.session_key,
                record.source_key,
                to_i64(record.generation, "generation")?,
                record.source_kind.as_str(),
                record.model,
                record.occurred_at,
                to_i64(record.input_tokens, "input tokens")?,
                to_i64(record.output_tokens, "output tokens")?,
                to_i64(record.cache_read_tokens, "cache read tokens")?,
                to_i64(record.cache_write_tokens, "cache write tokens")?,
                to_i64(record.reasoning_tokens, "reasoning tokens")?,
                to_i64(record.record_revision, "record revision")?,
            ],
        )
        .map_err(map_database_error)?;
    Ok(())
}

fn query_replay_signature(
    connection: &Connection,
    signature: &CodexReplaySignature,
) -> Result<Option<CodexReplaySignature>, StorageError> {
    connection
        .query_row(
            "SELECT parent_source_key, parent_generation, token_event_ordinal,
                    occurred_at, signature_hash
             FROM codex_replay_signatures
             WHERE parent_source_key = ?1 AND parent_generation = ?2
               AND token_event_ordinal = ?3",
            params![
                signature.parent_source_key,
                to_i64(signature.parent_generation, "generation")?,
                to_i64(signature.token_event_ordinal, "replay ordinal")?,
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            },
        )
        .optional()
        .map_err(map_database_error)?
        .map(replay_signature_from_database)
        .transpose()
}

fn insert_replay_signature(
    connection: &Connection,
    signature: &CodexReplaySignature,
) -> Result<(), StorageError> {
    connection
        .execute(
            "INSERT INTO codex_replay_signatures(
                parent_source_key, parent_generation, token_event_ordinal,
                occurred_at, signature_hash
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                signature.parent_source_key,
                to_i64(signature.parent_generation, "generation")?,
                to_i64(signature.token_event_ordinal, "replay ordinal")?,
                signature.occurred_at,
                signature.signature_hash.as_slice(),
            ],
        )
        .map_err(map_database_error)?;
    Ok(())
}

fn write_supplemental_metadata(
    connection: &Connection,
    metadata: &RequestSupplementalMetadata,
) -> Result<(), StorageError> {
    let logical_bytes = validate_supplemental_metadata(metadata)?;
    let existing = query_supplemental_metadata(
        connection,
        metadata.request_id.as_str(),
        metadata.attempt_id.as_str(),
    )?;
    match existing {
        Some(existing) if existing == *metadata => return Ok(()),
        Some(_) => {
            return Err(StorageError::StableRecordConflict {
                record_kind: "request supplemental metadata",
            });
        }
        None => {}
    }
    connection
        .execute(
            "INSERT INTO request_supplemental_metadata(
                request_id, attempt_id, trace_id, occurred_at, route_fingerprint,
                provider_fingerprint, account_fingerprint, retry_decision, failover_decision,
                queue_ms, connect_ms, first_byte_ms, total_ms, request_bytes, response_bytes,
                status_code, error_code, logical_bytes
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18
            )",
            params![
                metadata.request_id.as_str(),
                metadata.attempt_id.as_str(),
                metadata.trace_id.as_str(),
                metadata.occurred_at,
                metadata.route_fingerprint.as_str(),
                metadata.provider_fingerprint.as_str(),
                metadata
                    .account_fingerprint
                    .as_ref()
                    .map(OpaqueFingerprint::as_str),
                metadata.retry_decision.as_str(),
                metadata.failover_decision.as_str(),
                to_i64(metadata.queue_ms, "queue duration")?,
                to_i64(metadata.connect_ms, "connect duration")?,
                to_i64(metadata.first_byte_ms, "first-byte duration")?,
                to_i64(metadata.total_ms, "total duration")?,
                to_i64(metadata.request_bytes, "request size")?,
                to_i64(metadata.response_bytes, "response size")?,
                metadata.status_code.map(i64::from),
                metadata
                    .error_code
                    .as_ref()
                    .map(SupplementalErrorCode::as_str),
                usize_to_i64(logical_bytes, "logical bytes")?,
            ],
        )
        .map_err(map_database_error)?;
    Ok(())
}

fn query_supplemental_metadata(
    connection: &Connection,
    request_id: &str,
    attempt_id: &str,
) -> Result<Option<RequestSupplementalMetadata>, StorageError> {
    let raw = connection
        .query_row(
            "SELECT request_id, attempt_id, trace_id, occurred_at, route_fingerprint,
                    provider_fingerprint, account_fingerprint, retry_decision,
                    failover_decision, queue_ms, connect_ms, first_byte_ms, total_ms,
                    request_bytes, response_bytes, status_code, error_code
             FROM request_supplemental_metadata
            WHERE request_id = ?1 AND attempt_id = ?2",
            params![request_id, attempt_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                    row.get(14)?,
                    row.get(15)?,
                    row.get(16)?,
                ))
            },
        )
        .optional()
        .map_err(map_database_error)?;
    raw.map(supplemental_from_database).transpose()
}

fn supplemental_from_database(
    raw: SupplementalDatabaseRow,
) -> Result<RequestSupplementalMetadata, StorageError> {
    let status_code =
        raw.15
            .map(u16::try_from)
            .transpose()
            .map_err(|_| StorageError::StateDatabaseCorrupt {
                message: "supplemental metadata contains an invalid HTTP status".to_owned(),
            })?;
    let metadata = RequestSupplementalMetadata {
        request_id: RequestId::from_database(raw.0)?,
        attempt_id: AttemptId::from_database(raw.1)?,
        trace_id: TraceId::from_database(raw.2)?,
        occurred_at: raw.3,
        route_fingerprint: OpaqueFingerprint::from_database(raw.4)?,
        provider_fingerprint: OpaqueFingerprint::from_database(raw.5)?,
        account_fingerprint: raw.6.map(OpaqueFingerprint::from_database).transpose()?,
        retry_decision: SupplementalRetryDecision::from_database(&raw.7)?,
        failover_decision: SupplementalFailoverDecision::from_database(&raw.8)?,
        queue_ms: database_u64(raw.9, "queue duration")?,
        connect_ms: database_u64(raw.10, "connect duration")?,
        first_byte_ms: database_u64(raw.11, "first-byte duration")?,
        total_ms: database_u64(raw.12, "total duration")?,
        request_bytes: database_u64(raw.13, "request size")?,
        response_bytes: database_u64(raw.14, "response size")?,
        status_code,
        error_code: raw
            .16
            .map(SupplementalErrorCode::from_database)
            .transpose()?,
    };
    validate_supplemental_metadata(&metadata).map_err(|_| StorageError::StateDatabaseCorrupt {
        message: "supplemental metadata violates its typed bounds".to_owned(),
    })?;
    Ok(metadata)
}

fn supplemental_stats(connection: &Connection) -> Result<SupplementalStorageStats, StorageError> {
    let (rows, logical_bytes, oldest_at) = connection
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(logical_bytes), 0), MIN(occurred_at)
             FROM request_supplemental_metadata",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .map_err(map_database_error)?;
    Ok(SupplementalStorageStats {
        rows: usize::try_from(rows).map_err(|_| StorageError::StateDatabaseCorrupt {
            message: "supplemental metadata contains an invalid row count".to_owned(),
        })?,
        logical_bytes: usize::try_from(logical_bytes).map_err(|_| {
            StorageError::StateDatabaseCorrupt {
                message: "supplemental metadata contains an invalid logical size".to_owned(),
            }
        })?,
        oldest_at,
    })
}

fn pending_replay_signatures<'a>(
    connection: &Connection,
    signatures: &'a [CodexReplaySignature],
) -> Result<Vec<&'a CodexReplaySignature>, StorageError> {
    if signatures.is_empty() {
        return Ok(Vec::new());
    }
    let first = &signatures[0];
    let count = connection
        .query_row(
            "SELECT COUNT(*) FROM codex_replay_signatures
             WHERE parent_source_key = ?1 AND parent_generation = ?2",
            params![
                first.parent_source_key,
                to_i64(first.parent_generation, "generation")?
            ],
            |row| row.get::<_, i64>(0),
        )
        .map_err(map_database_error)?;
    let existing_count = database_u64(count, "replay signature count")?;
    let mut pending = Vec::with_capacity(signatures.len());
    for signature in signatures {
        if let Some(previous) = pending
            .iter()
            .copied()
            .find(|previous: &&CodexReplaySignature| {
                previous.token_event_ordinal == signature.token_event_ordinal
            })
        {
            if *previous == *signature {
                continue;
            }
            return Err(StorageError::StableRecordConflict {
                record_kind: "Codex replay signature",
            });
        }
        match query_replay_signature(connection, signature)? {
            Some(existing) if existing == *signature => continue,
            Some(_) => {
                return Err(StorageError::StableRecordConflict {
                    record_kind: "Codex replay signature",
                });
            }
            None => pending.push(signature),
        }
    }
    if existing_count.saturating_add(pending.len() as u64) > MAX_CODEX_REPLAY_SIGNATURES {
        return Err(StorageError::ReplaySignatureLimitExceeded);
    }
    Ok(pending)
}

fn pending_supplemental_metadata<'a>(
    connection: &Connection,
    metadata: &'a [RequestSupplementalMetadata],
) -> Result<(Vec<&'a RequestSupplementalMetadata>, usize), StorageError> {
    let mut pending = Vec::with_capacity(metadata.len());
    let mut pending_bytes = 0_usize;
    for row in metadata {
        if let Some(previous) =
            pending
                .iter()
                .copied()
                .find(|previous: &&RequestSupplementalMetadata| {
                    previous.request_id == row.request_id && previous.attempt_id == row.attempt_id
                })
        {
            if *previous == *row {
                continue;
            }
            return Err(StorageError::StableRecordConflict {
                record_kind: "request supplemental metadata",
            });
        }
        match query_supplemental_metadata(
            connection,
            row.request_id.as_str(),
            row.attempt_id.as_str(),
        )? {
            Some(existing) if existing == *row => continue,
            Some(_) => {
                return Err(StorageError::StableRecordConflict {
                    record_kind: "request supplemental metadata",
                });
            }
            None => {
                pending_bytes = pending_bytes.saturating_add(validate_supplemental_metadata(row)?);
                pending.push(row);
            }
        }
    }
    Ok((pending, pending_bytes))
}

fn supplemental_batch_fits(
    connection: &Connection,
    pending_rows: usize,
    pending_bytes: usize,
) -> Result<bool, StorageError> {
    let stats = supplemental_stats(connection)?;
    Ok(
        stats.rows.saturating_add(pending_rows) <= MAX_SUPPLEMENTAL_ROWS
            && stats.logical_bytes.saturating_add(pending_bytes) <= MAX_SUPPLEMENTAL_BYTES,
    )
}

fn index_database_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexDatabaseRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
    ))
}

fn usage_database_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UsageDatabaseRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
    ))
}

fn index_record_from_database(row: IndexDatabaseRow) -> Result<SessionIndexRecord, StorageError> {
    Ok(SessionIndexRecord {
        session_key: row.0,
        source_key: row.1,
        generation: database_u64(row.2, "generation")?,
        source_kind: SessionSourceKind::from_database(&row.3)?,
        created_at: row.4,
        last_active_at: row.5,
        message_count: database_u64(row.6, "message count")?,
        usage_event_count: database_u64(row.7, "usage event count")?,
        availability: SessionAvailability::from_database(&row.8)?,
    })
}

fn usage_record_from_database(row: UsageDatabaseRow) -> Result<SessionUsageRecord, StorageError> {
    Ok(SessionUsageRecord {
        usage_id: row.0,
        session_key: row.1,
        source_key: row.2,
        generation: database_u64(row.3, "generation")?,
        source_kind: SessionSourceKind::from_database(&row.4)?,
        model: row.5,
        occurred_at: row.6,
        input_tokens: database_u64(row.7, "input tokens")?,
        output_tokens: database_u64(row.8, "output tokens")?,
        cache_read_tokens: database_u64(row.9, "cache read tokens")?,
        cache_write_tokens: database_u64(row.10, "cache write tokens")?,
        reasoning_tokens: database_u64(row.11, "reasoning tokens")?,
        record_revision: database_u64(row.12, "record revision")?,
    })
}

fn replay_signature_from_database(
    row: ReplayDatabaseRow,
) -> Result<CodexReplaySignature, StorageError> {
    Ok(CodexReplaySignature {
        parent_source_key: row.0,
        parent_generation: database_u64(row.1, "generation")?,
        token_event_ordinal: database_u64(row.2, "replay ordinal")?,
        occurred_at: row.3,
        signature_hash: vec_to_array(row.4, "replay signature hash")?,
    })
}

fn to_i64(value: u64, name: &str) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| invalid_state(&format!("{name} exceeds SQLite range")))
}

fn usize_to_i64(value: usize, name: &str) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| invalid_state(&format!("{name} exceeds SQLite range")))
}

fn database_u64(value: i64, name: &str) -> Result<u64, StorageError> {
    u64::try_from(value).map_err(|_| StorageError::StateDatabaseCorrupt {
        message: format!("Session state contains an invalid {name}"),
    })
}

fn optional_u64(value: Option<i64>, name: &str) -> Result<Option<u64>, StorageError> {
    value.map(|value| database_u64(value, name)).transpose()
}

fn vec_to_array(bytes: Vec<u8>, name: &str) -> Result<[u8; 32], StorageError> {
    bytes
        .try_into()
        .map_err(|_| StorageError::StateDatabaseCorrupt {
            message: format!("Session state contains an invalid {name}"),
        })
}

fn query_runtime_secret_binding(
    connection: &Connection,
    name: &str,
) -> Result<Option<RuntimeSecretBinding>, StorageError> {
    let result = connection.query_row(
        "SELECT binding_name, secret_ref, revision, created_at
             FROM runtime_secret_bindings
             WHERE binding_name = ?1",
        [name],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        },
    );
    let (name, secret_ref, revision, created_at) = match result {
        Ok(binding) => binding,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
        Err(error) => return Err(map_database_error(error)),
    };
    let secret_ref =
        SecretRef::parse(secret_ref).map_err(|_| StorageError::StateDatabaseCorrupt {
            message: "runtime secret binding contains an invalid reference".to_owned(),
        })?;
    let revision = u64::try_from(revision).map_err(|_| StorageError::StateDatabaseCorrupt {
        message: "runtime secret binding contains an invalid revision".to_owned(),
    })?;

    Ok(Some(RuntimeSecretBinding {
        name,
        secret_ref,
        revision,
        created_at,
    }))
}

fn read_only_database_uri(path: &Path) -> Result<String, StorageError> {
    let value = path
        .to_str()
        .ok_or_else(|| StorageError::Io {
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "state database path is not valid UTF-8",
            ),
        })?
        .replace('\\', "/");
    #[cfg(windows)]
    let value = value.strip_prefix("//?/").unwrap_or(&value);
    #[cfg(not(windows))]
    let value = value.as_str();
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/' | b':') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(&mut encoded, "%{byte:02X}").expect("writing to a string cannot fail");
        }
    }
    #[cfg(windows)]
    if !encoded.starts_with('/') {
        encoded.insert(0, '/');
    }
    Ok(format!("file:{encoded}?mode=ro&immutable=1"))
}

fn sqlite_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(suffix);
    sidecar.into()
}

fn apply_ordered_migrations(connection: &mut Connection) -> Result<(), StorageError> {
    let versions = schema_versions(connection)?;
    if versions
        .iter()
        .enumerate()
        .any(|(index, version)| *version != (index as i64) + 1)
        || versions
            .last()
            .is_some_and(|version| *version > LATEST_SCHEMA_VERSION)
    {
        return Err(StorageError::StateDatabaseCorrupt {
            message: "state database has an incompatible migration history".to_owned(),
        });
    }

    for (version, migration) in [
        (1, INITIAL_MIGRATION),
        (2, RUNTIME_AUTH_MIGRATION),
        (3, SESSION_DIAGNOSTICS_MIGRATION),
    ] {
        if versions.contains(&version) {
            continue;
        }
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_database_error)?;
        transaction
            .execute_batch(migration)
            .map_err(map_database_error)?;
        transaction.commit().map_err(map_database_error)?;
    }
    Ok(())
}

fn schema_versions(connection: &Connection) -> Result<Vec<i64>, StorageError> {
    let has_migration_table = connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master
                WHERE type = 'table' AND name = 'schema_migrations'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(map_database_error)?;
    if !has_migration_table {
        let existing_tables = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(map_database_error)?;
        if existing_tables != 0 {
            return Err(StorageError::StateDatabaseCorrupt {
                message: "state database has tables without migration metadata".to_owned(),
            });
        }
        return Ok(Vec::new());
    }

    let mut statement = connection
        .prepare("SELECT version FROM schema_migrations ORDER BY version")
        .map_err(map_database_error)?;
    statement
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(map_database_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_database_error)
}

fn map_database_error(error: rusqlite::Error) -> StorageError {
    match &error {
        rusqlite::Error::SqliteFailure(sqlite_error, _)
            if matches!(
                sqlite_error.code,
                ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase
            ) =>
        {
            StorageError::StateDatabaseCorrupt {
                message: error.to_string(),
            }
        }
        _ => StorageError::StateDatabase { source: error },
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::StatementStatus;

    use super::*;

    fn opaque(value: u64) -> String {
        format!("{value:064x}")
    }

    fn query_vm_steps(connection: &Connection, sql: &str) -> i32 {
        let mut statement = connection.prepare(sql).unwrap();
        let rows = statement
            .query_map([2_i64], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(rows.len(), 1);
        statement.get_status(StatementStatus::VmStep)
    }

    #[test]
    fn global_current_query_vm_work_does_not_scale_with_hidden_generations() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore::open(directory.path().join("state.db")).unwrap();
        let source_key = opaque(1);
        store
            .connection
            .execute(
                "INSERT INTO session_sources(
                    source_key, source_kind, current_generation, staging_generation,
                    retired_generation, status, error_code, last_transition_at
                 ) VALUES (?1, 'codex', 1, NULL, NULL, 'available', NULL, ?2)",
                params![source_key, "2026-07-26T00:00:00Z"],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO session_scan_cursors(
                    source_key, generation, source_kind, generation_state, file_identity,
                    observed_size, modified_at, complete_byte_offset, stable_record_ordinal,
                    parser_checkpoint, head_fingerprint, boundary_fingerprint,
                    parent_source_key, parent_generation, replay_boundary_fingerprint,
                    result_code, result_changed_at
                 ) VALUES (
                    ?1, 1, 'codex', 'current', ?2, 0, ?3, 0, 0,
                    X'31', zeroblob(32), zeroblob(32), NULL, NULL, NULL, NULL, NULL
                 )",
                params![source_key, opaque(2), "2026-07-26T00:00:00Z"],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO session_index(
                    session_key, source_key, generation, source_kind, created_at,
                    last_active_at, message_count, usage_event_count, availability
                 ) VALUES (?1, ?2, 1, 'codex', ?3, ?3, 1, 1, 'available')",
                params![opaque(3), source_key, "2026-07-26T00:00:00Z"],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO session_usage_records(
                    usage_id, session_key, source_key, generation, source_kind, model,
                    occurred_at, input_tokens, output_tokens, cache_read_tokens,
                    cache_write_tokens, reasoning_tokens, record_revision
                 ) VALUES (?1, ?2, ?3, 1, 'codex', 'gpt-5.6', ?4, 1, 1, 0, 0, 0, 1)",
                params![opaque(4), opaque(3), source_key, "2026-07-26T00:00:00Z"],
            )
            .unwrap();

        let baseline_index_steps = query_vm_steps(
            &store.connection,
            GLOBAL_CURRENT_SESSION_INDEX_FIRST_PAGE_SQL,
        );
        let baseline_usage_steps = query_vm_steps(
            &store.connection,
            GLOBAL_CURRENT_SESSION_USAGE_FIRST_PAGE_SQL,
        );

        store
            .connection
            .execute_batch(
                "WITH RECURSIVE generations(value) AS (
                    VALUES(2)
                    UNION ALL
                    SELECT value + 1 FROM generations WHERE value < 20001
                 )
                 INSERT INTO session_scan_cursors(
                    source_key, generation, source_kind, generation_state, file_identity,
                    observed_size, modified_at, complete_byte_offset, stable_record_ordinal,
                    parser_checkpoint, head_fingerprint, boundary_fingerprint,
                    parent_source_key, parent_generation, replay_boundary_fingerprint,
                    result_code, result_changed_at
                 )
                 SELECT
                    '0000000000000000000000000000000000000000000000000000000000000001',
                    value, 'codex', 'retired',
                    '0000000000000000000000000000000000000000000000000000000000000002',
                    0, '2026-07-26T00:00:00Z', 0, 0, X'31',
                    zeroblob(32), zeroblob(32), NULL, NULL, NULL, NULL, NULL
                 FROM generations;

                 INSERT INTO session_index(
                    session_key, source_key, generation, source_kind, created_at,
                    last_active_at, message_count, usage_event_count, availability
                 )
                 SELECT printf('%064x', generation),
                        '0000000000000000000000000000000000000000000000000000000000000001',
                        generation, 'codex', '2026-07-26T00:00:00Z',
                        '9999-12-31T23:59:59Z', 1, 1, 'available'
                 FROM session_scan_cursors WHERE generation > 1;

                 INSERT INTO session_usage_records(
                    usage_id, session_key, source_key, generation, source_kind, model,
                    occurred_at, input_tokens, output_tokens, cache_read_tokens,
                    cache_write_tokens, reasoning_tokens, record_revision
                 )
                 SELECT printf('%064x', generation), printf('%064x', generation),
                        '0000000000000000000000000000000000000000000000000000000000000001',
                        generation, 'codex', 'gpt-5.6', '0001-01-01T00:00:00Z',
                        1, 1, 0, 0, 0, 1
                 FROM session_scan_cursors WHERE generation > 1;",
            )
            .unwrap();

        let hidden_index_steps = query_vm_steps(
            &store.connection,
            GLOBAL_CURRENT_SESSION_INDEX_FIRST_PAGE_SQL,
        );
        let hidden_usage_steps = query_vm_steps(
            &store.connection,
            GLOBAL_CURRENT_SESSION_USAGE_FIRST_PAGE_SQL,
        );
        assert!(
            hidden_index_steps <= baseline_index_steps + 1_000,
            "index VM steps scaled from {baseline_index_steps} to {hidden_index_steps}"
        );
        assert!(
            hidden_usage_steps <= baseline_usage_steps + 1_000,
            "usage VM steps scaled from {baseline_usage_steps} to {hidden_usage_steps}"
        );
    }
}
