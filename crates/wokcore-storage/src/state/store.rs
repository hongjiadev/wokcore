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
    pub file_identity: String,
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
    pub result_code: Option<String>,
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
    pub request_id: String,
    pub attempt_id: String,
    pub trace_id: String,
    pub occurred_at: String,
    pub route_fingerprint: String,
    pub provider_fingerprint: String,
    pub account_fingerprint: Option<String>,
    pub retry_decision: String,
    pub failover_decision: String,
    pub queue_ms: u64,
    pub connect_ms: u64,
    pub first_byte_ms: u64,
    pub total_ms: u64,
    pub request_bytes: u64,
    pub response_bytes: u64,
    pub status_code: Option<u16>,
    pub error_code: Option<String>,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionIndexPage {
    pub items: Vec<SessionIndexRecord>,
    pub next_page_key: Option<SessionIndexPageKey>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionUsagePageKey {
    source_key: String,
    generation: u64,
    occurred_at: String,
    usage_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionUsagePage {
    pub items: Vec<SessionUsageRecord>,
    pub next_page_key: Option<SessionUsagePageKey>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplaySignaturePageKey {
    parent_source_key: String,
    parent_generation: u64,
    token_event_ordinal: u64,
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

#[derive(Clone, Copy)]
enum ReadOnlyAccess {
    Offline,
    Live,
}

impl ReadOnlyStateStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::open_path(path.as_ref(), ReadOnlyAccess::Offline)
    }

    pub fn open_live(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::open_path(path.as_ref(), ReadOnlyAccess::Live)
    }

    fn open_path(path: &Path, _access: ReadOnlyAccess) -> Result<Self, StorageError> {
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
        Self::open_uri(read_only_database_uri(&absolute, has_wal)?)
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
                    transaction
                        .execute(
                            "UPDATE session_sources SET staging_generation = ?2
                             WHERE source_key = ?1",
                            params![cursor.source_key, to_i64(cursor.generation, "generation")?],
                        )
                        .map_err(map_database_error)?;
                    insert_cursor(&transaction, cursor)?;
                    CandidateBeginOutcome::Started
                }
            }
        };
        transaction.commit().map_err(map_database_error)?;
        Ok(outcome)
    }

    pub fn commit_candidate_batch(&mut self, batch: &SessionBatch) -> Result<(), StorageError> {
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

    pub fn commit_session_batch(&mut self, batch: &SessionBatch) -> Result<(), StorageError> {
        self.commit_session_batch_internal(batch, None)
    }

    fn commit_session_batch_internal(
        &mut self,
        batch: &SessionBatch,
        required_state: Option<SessionGenerationState>,
    ) -> Result<(), StorageError> {
        validate_session_batch(batch)?;
        if batch.cursor.is_none()
            && batch.index_records.is_empty()
            && batch.usage_records.is_empty()
            && batch.replay_signatures.is_empty()
            && batch.supplemental_metadata.is_empty()
        {
            return Ok(());
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
        for signature in &batch.replay_signatures {
            write_replay_signature(&transaction, signature)?;
        }
        for metadata in &batch.supplemental_metadata {
            write_supplemental_metadata(&transaction, metadata)?;
        }
        transaction.commit().map_err(map_database_error)
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
            transaction
                .execute(
                    "UPDATE session_scan_cursors SET generation_state = 'retired'
                     WHERE source_key = ?1 AND generation = ?2",
                    params![source_key, to_i64(current, "generation")?],
                )
                .map_err(map_database_error)?;
        }
        transaction
            .execute(
                "UPDATE session_scan_cursors SET generation_state = 'current'
                 WHERE source_key = ?1 AND generation = ?2",
                params![source_key, to_i64(generation, "generation")?],
            )
            .map_err(map_database_error)?;
        transaction
            .execute(
                "UPDATE session_sources
                 SET current_generation = ?2,
                     staging_generation = NULL,
                     retired_generation = current_generation,
                     status = 'available',
                     error_code = NULL,
                     last_transition_at = ?3
                 WHERE source_key = ?1",
                params![source_key, to_i64(generation, "generation")?, transition_at],
            )
            .map_err(map_database_error)?;
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

    pub fn load_session_source(
        &self,
        source_key: &str,
    ) -> Result<Option<SessionSourceState>, StorageError> {
        validate_opaque_key("source key", source_key)?;
        let raw = self
            .connection
            .query_row(
                "SELECT source_key, source_kind, current_generation, staging_generation,
                        retired_generation, status, error_code, last_transition_at
                 FROM session_sources WHERE source_key = ?1",
                [source_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, Option<String>>(7)?,
                    ))
                },
            )
            .optional()
            .map_err(map_database_error)?;
        raw.map(|raw| {
            Ok(SessionSourceState {
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
            })
        })
        .transpose()
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
        let source = self
            .load_session_source(source_key)?
            .ok_or(StorageError::CandidateStateConflict)?;
        if source.current_generation != Some(generation)
            && source.staging_generation != Some(generation)
        {
            return Err(StorageError::CandidateStateConflict);
        }
        let status = if error_code.is_resource_limit() {
            SessionSourceStatus::ResourceLimited
        } else if source.current_generation.is_some() {
            SessionSourceStatus::Stale
        } else {
            SessionSourceStatus::Unavailable
        };
        if source.status == status && source.error_code == Some(error_code) {
            return Ok(false);
        }
        self.connection
            .execute(
                "UPDATE session_sources
                 SET status = ?2, error_code = ?3, last_transition_at = ?4
                 WHERE source_key = ?1",
                params![
                    source_key,
                    status.as_str(),
                    error_code.as_str(),
                    transition_at
                ],
            )
            .map_err(map_database_error)?;
        Ok(true)
    }

    pub fn record_source_success(
        &mut self,
        source_key: &str,
        transition_at: &str,
    ) -> Result<bool, StorageError> {
        validate_opaque_key("source key", source_key)?;
        validate_timestamp("source transition timestamp", transition_at)?;
        let source = self
            .load_session_source(source_key)?
            .ok_or(StorageError::CandidateStateConflict)?;
        if source.current_generation.is_none() {
            return Err(StorageError::CandidateStateConflict);
        }
        if source.status == SessionSourceStatus::Available && source.error_code.is_none() {
            return Ok(false);
        }
        self.connection
            .execute(
                "UPDATE session_sources
                 SET status = 'available', error_code = NULL, last_transition_at = ?2
                 WHERE source_key = ?1",
                params![source_key, transition_at],
            )
            .map_err(map_database_error)?;
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
        let mut pending: Vec<&RequestSupplementalMetadata> = Vec::with_capacity(metadata.len());
        let mut pending_bytes = 0_usize;
        for row in metadata {
            if let Some(previous) = pending.iter().copied().find(|previous| {
                previous.request_id == row.request_id && previous.attempt_id == row.attempt_id
            }) {
                if *previous == *row {
                    continue;
                }
                return Err(StorageError::StableRecordConflict {
                    record_kind: "request supplemental metadata",
                });
            }
            match query_supplemental_metadata(&transaction, &row.request_id, &row.attempt_id)? {
                Some(existing) if existing == *row => continue,
                Some(_) => {
                    return Err(StorageError::StableRecordConflict {
                        record_kind: "request supplemental metadata",
                    });
                }
                None => {
                    pending_bytes =
                        pending_bytes.saturating_add(validate_supplemental_metadata(row)?);
                    pending.push(row);
                }
            }
        }
        let stats = supplemental_stats(&transaction)?;
        if stats.rows.saturating_add(pending.len()) > MAX_SUPPLEMENTAL_ROWS
            || stats.logical_bytes.saturating_add(pending_bytes) > MAX_SUPPLEMENTAL_BYTES
        {
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
                           length(session_key) + length(source_key) + length(created_at)
                           + length(last_active_at) + 40 AS logical_bytes
                    FROM session_index WHERE source_key = ?1 AND generation = ?2
                    UNION ALL
                    SELECT 2, rowid,
                           length(usage_id) + length(session_key) + length(source_key)
                           + length(model) + length(occurred_at) + 64
                    FROM session_usage_records WHERE source_key = ?1 AND generation = ?2
                    UNION ALL
                    SELECT 3, rowid,
                           length(parent_source_key) + length(occurred_at) + 48
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
                "SELECT length(source_key) + length(file_identity) + length(modified_at)
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

    pub fn load_current_session_index_page(
        &self,
        source_key: &str,
        after: Option<&SessionIndexPageKey>,
        limit: usize,
    ) -> Result<SessionIndexPage, StorageError> {
        validate_page_limit(limit, 200)?;
        let Some(generation) = self.load_current_generation(source_key)? else {
            if after.is_some() {
                return Err(StorageError::StalePageKey);
            }
            return Ok(SessionIndexPage {
                items: Vec::new(),
                next_page_key: None,
            });
        };
        if after.is_some_and(|key| key.source_key != source_key || key.generation != generation) {
            return Err(StorageError::StalePageKey);
        }
        let raw = query_index_rows(&self.connection, source_key, generation, after, limit + 1)?;
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
        let Some(generation) = self.load_current_generation(source_key)? else {
            if after.is_some() {
                return Err(StorageError::StalePageKey);
            }
            return Ok(SessionUsagePage {
                items: Vec::new(),
                next_page_key: None,
            });
        };
        if after.is_some_and(|key| key.source_key != source_key || key.generation != generation) {
            return Err(StorageError::StalePageKey);
        }
        let raw = query_usage_rows(&self.connection, source_key, generation, after, limit + 1)?;
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
        if self.load_current_generation(parent_source_key)? != Some(parent_generation)
            || after.is_some_and(|key| {
                key.parent_source_key != parent_source_key
                    || key.parent_generation != parent_generation
            })
        {
            return Err(StorageError::StalePageKey);
        }
        let after_ordinal = after.map_or(0, |key| key.token_event_ordinal);
        let mut statement = self
            .connection
            .prepare(
                "SELECT parent_source_key, parent_generation, token_event_ordinal,
                        occurred_at, signature_hash
                 FROM codex_replay_signatures
                 WHERE parent_source_key = ?1 AND parent_generation = ?2
                   AND token_event_ordinal > ?3
                 ORDER BY token_event_ordinal
                 LIMIT ?4",
            )
            .map_err(map_database_error)?;
        let raw = statement
            .query_map(
                params![
                    parent_source_key,
                    to_i64(parent_generation, "generation")?,
                    to_i64(after_ordinal, "replay ordinal")?,
                    usize_to_i64(limit + 1, "page limit")?,
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
            .map_err(map_database_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_database_error)?;
        let mut items = raw
            .into_iter()
            .map(replay_signature_from_database)
            .collect::<Result<Vec<_>, _>>()?;
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
    validate_bounded_text("file identity", &cursor.file_identity, 256, false)?;
    if cursor.file_identity.contains(['/', '\\']) {
        return Err(invalid_state("file identity must be opaque"));
    }
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
    if let Some(result_code) = &cursor.result_code {
        validate_bounded_text("result code", result_code, 128, false)?;
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
    for (name, value, maximum) in [
        ("request identifier", metadata.request_id.as_str(), 256),
        ("attempt identifier", metadata.attempt_id.as_str(), 256),
        ("trace identifier", metadata.trace_id.as_str(), 256),
        ("route fingerprint", metadata.route_fingerprint.as_str(), 64),
        (
            "provider fingerprint",
            metadata.provider_fingerprint.as_str(),
            64,
        ),
        ("retry decision", metadata.retry_decision.as_str(), 256),
        (
            "failover decision",
            metadata.failover_decision.as_str(),
            256,
        ),
    ] {
        validate_bounded_text(name, value, maximum, false)?;
    }
    validate_timestamp("supplemental timestamp", &metadata.occurred_at)?;
    validate_opaque_key("route fingerprint", &metadata.route_fingerprint)?;
    validate_opaque_key("provider fingerprint", &metadata.provider_fingerprint)?;
    if let Some(account) = &metadata.account_fingerprint {
        validate_opaque_key("account fingerprint", account)?;
    }
    if let Some(error_code) = &metadata.error_code {
        validate_bounded_text("supplemental error code", error_code, 256, false)?;
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
    validate_bounded_text(name, value, 64, false)?;
    if !value.ends_with('Z') || !value.contains('T') {
        return Err(invalid_state(&format!("{name} must be normalized UTC")));
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
        + cursor.file_identity.len()
        + cursor.modified_at.len()
        + cursor.result_code.as_ref().map_or(0, String::len)
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
    metadata.request_id.len()
        + metadata.attempt_id.len()
        + metadata.trace_id.len()
        + metadata.occurred_at.len()
        + metadata.route_fingerprint.len()
        + metadata.provider_fingerprint.len()
        + metadata.account_fingerprint.as_ref().map_or(0, String::len)
        + metadata.retry_decision.len()
        + metadata.failover_decision.len()
        + metadata.error_code.as_ref().map_or(0, String::len)
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
                cursor.file_identity,
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
                cursor.result_code,
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
                && cursor.complete_byte_offset >= existing.complete_byte_offset
                && cursor.stable_record_ordinal >= existing.stable_record_ordinal =>
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
                        cursor.result_code,
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
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, Vec<u8>>(9)?,
                    row.get::<_, Vec<u8>>(10)?,
                    row.get::<_, Vec<u8>>(11)?,
                    row.get::<_, Option<String>>(12)?,
                    row.get::<_, Option<i64>>(13)?,
                    row.get::<_, Option<Vec<u8>>>(14)?,
                    row.get::<_, Option<String>>(15)?,
                    row.get::<_, Option<String>>(16)?,
                ))
            },
        )
        .optional()
        .map_err(map_database_error)?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let cursor = SessionScanCursor {
        source_key: raw.0,
        source_kind: SessionSourceKind::from_database(&raw.1)?,
        generation: database_u64(raw.2, "generation")?,
        generation_state: SessionGenerationState::from_database(&raw.3)?,
        file_identity: raw.4,
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
        result_code: raw.15,
        result_changed_at: raw.16,
    };
    validate_cursor(&cursor).map_err(|_| StorageError::StateDatabaseCorrupt {
        message: "Session scan cursor violates its typed bounds".to_owned(),
    })?;
    Ok(Some(cursor))
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

fn write_replay_signature(
    connection: &Connection,
    signature: &CodexReplaySignature,
) -> Result<(), StorageError> {
    let existing = connection
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
        .transpose()?;
    match existing {
        Some(existing) if existing == *signature => return Ok(()),
        Some(_) => {
            return Err(StorageError::StableRecordConflict {
                record_kind: "Codex replay signature",
            });
        }
        None => {}
    }
    let count = connection
        .query_row(
            "SELECT COUNT(*) FROM codex_replay_signatures
             WHERE parent_source_key = ?1 AND parent_generation = ?2",
            params![
                signature.parent_source_key,
                to_i64(signature.parent_generation, "generation")?
            ],
            |row| row.get::<_, i64>(0),
        )
        .map_err(map_database_error)?;
    if database_u64(count, "replay signature count")? >= MAX_CODEX_REPLAY_SIGNATURES {
        return Err(StorageError::ReplaySignatureLimitExceeded);
    }
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
    let existing =
        query_supplemental_metadata(connection, &metadata.request_id, &metadata.attempt_id)?;
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
                metadata.request_id,
                metadata.attempt_id,
                metadata.trace_id,
                metadata.occurred_at,
                metadata.route_fingerprint,
                metadata.provider_fingerprint,
                metadata.account_fingerprint,
                metadata.retry_decision,
                metadata.failover_decision,
                to_i64(metadata.queue_ms, "queue duration")?,
                to_i64(metadata.connect_ms, "connect duration")?,
                to_i64(metadata.first_byte_ms, "first-byte duration")?,
                to_i64(metadata.total_ms, "total duration")?,
                to_i64(metadata.request_bytes, "request size")?,
                to_i64(metadata.response_bytes, "response size")?,
                metadata.status_code.map(i64::from),
                metadata.error_code,
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
    connection
        .query_row(
            "SELECT request_id, attempt_id, trace_id, occurred_at, route_fingerprint,
                    provider_fingerprint, account_fingerprint, retry_decision,
                    failover_decision, queue_ms, connect_ms, first_byte_ms, total_ms,
                    request_bytes, response_bytes, status_code, error_code
             FROM request_supplemental_metadata
             WHERE request_id = ?1 AND attempt_id = ?2",
            params![request_id, attempt_id],
            |row| {
                Ok(RequestSupplementalMetadata {
                    request_id: row.get(0)?,
                    attempt_id: row.get(1)?,
                    trace_id: row.get(2)?,
                    occurred_at: row.get(3)?,
                    route_fingerprint: row.get(4)?,
                    provider_fingerprint: row.get(5)?,
                    account_fingerprint: row.get(6)?,
                    retry_decision: row.get(7)?,
                    failover_decision: row.get(8)?,
                    queue_ms: database_u64_sql(row.get(9)?)?,
                    connect_ms: database_u64_sql(row.get(10)?)?,
                    first_byte_ms: database_u64_sql(row.get(11)?)?,
                    total_ms: database_u64_sql(row.get(12)?)?,
                    request_bytes: database_u64_sql(row.get(13)?)?,
                    response_bytes: database_u64_sql(row.get(14)?)?,
                    status_code: row
                        .get::<_, Option<i64>>(15)?
                        .map(|value| {
                            u16::try_from(value)
                                .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(15, value))
                        })
                        .transpose()?,
                    error_code: row.get(16)?,
                })
            },
        )
        .optional()
        .map_err(map_database_error)
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

fn query_index_rows(
    connection: &Connection,
    source_key: &str,
    generation: u64,
    after: Option<&SessionIndexPageKey>,
    limit: usize,
) -> Result<Vec<IndexDatabaseRow>, StorageError> {
    let (sql, values): (&str, Vec<rusqlite::types::Value>) = match after {
        None => (
            "SELECT session_key, source_key, generation, source_kind, created_at,
                    last_active_at, message_count, usage_event_count, availability
             FROM session_index
             WHERE source_key = ?1 AND generation = ?2
             ORDER BY last_active_at DESC, session_key
             LIMIT ?3",
            vec![
                source_key.to_owned().into(),
                to_i64(generation, "generation")?.into(),
                usize_to_i64(limit, "page limit")?.into(),
            ],
        ),
        Some(key) => (
            "SELECT session_key, source_key, generation, source_kind, created_at,
                    last_active_at, message_count, usage_event_count, availability
             FROM session_index
             WHERE source_key = ?1 AND generation = ?2
               AND (last_active_at < ?3 OR (last_active_at = ?3 AND session_key > ?4))
             ORDER BY last_active_at DESC, session_key
             LIMIT ?5",
            vec![
                source_key.to_owned().into(),
                to_i64(generation, "generation")?.into(),
                key.last_active_at.clone().into(),
                key.session_key.clone().into(),
                usize_to_i64(limit, "page limit")?.into(),
            ],
        ),
    };
    let mut statement = connection.prepare(sql).map_err(map_database_error)?;
    statement
        .query_map(rusqlite::params_from_iter(values), index_database_row)
        .map_err(map_database_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_database_error)
}

fn query_usage_rows(
    connection: &Connection,
    source_key: &str,
    generation: u64,
    after: Option<&SessionUsagePageKey>,
    limit: usize,
) -> Result<Vec<UsageDatabaseRow>, StorageError> {
    let (sql, values): (&str, Vec<rusqlite::types::Value>) = match after {
        None => (
            "SELECT usage_id, session_key, source_key, generation, source_kind, model,
                    occurred_at, input_tokens, output_tokens, cache_read_tokens,
                    cache_write_tokens, reasoning_tokens, record_revision
             FROM session_usage_records
             WHERE source_key = ?1 AND generation = ?2
             ORDER BY occurred_at, usage_id
             LIMIT ?3",
            vec![
                source_key.to_owned().into(),
                to_i64(generation, "generation")?.into(),
                usize_to_i64(limit, "page limit")?.into(),
            ],
        ),
        Some(key) => (
            "SELECT usage_id, session_key, source_key, generation, source_kind, model,
                    occurred_at, input_tokens, output_tokens, cache_read_tokens,
                    cache_write_tokens, reasoning_tokens, record_revision
             FROM session_usage_records
             WHERE source_key = ?1 AND generation = ?2
               AND (occurred_at > ?3 OR (occurred_at = ?3 AND usage_id > ?4))
             ORDER BY occurred_at, usage_id
             LIMIT ?5",
            vec![
                source_key.to_owned().into(),
                to_i64(generation, "generation")?.into(),
                key.occurred_at.clone().into(),
                key.usage_id.clone().into(),
                usize_to_i64(limit, "page limit")?.into(),
            ],
        ),
    };
    let mut statement = connection.prepare(sql).map_err(map_database_error)?;
    statement
        .query_map(rusqlite::params_from_iter(values), usage_database_row)
        .map_err(map_database_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_database_error)
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

fn database_u64_sql(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))
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

fn read_only_database_uri(path: &Path, has_wal: bool) -> Result<String, StorageError> {
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
    let options = match has_wal {
        false => "mode=ro&immutable=1",
        true => "mode=ro&readonly_shm=1",
    };
    Ok(format!("file:{encoded}?{options}"))
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
