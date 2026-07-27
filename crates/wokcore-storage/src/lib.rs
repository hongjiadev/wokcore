//! Durable configuration, secret, and state storage for WokCore.

pub mod config;
pub mod secrets;
pub mod state;

pub use config::{AppConfig, ConfigStore, ServerConfig, VersionedConfig};
pub use secrets::{
    EnvironmentSecretStore, HeadlessSecretStoreConfig, MAX_HEADLESS_SECRET_BYTES,
    MemorySecretStore, NativeSecretStore, PermissionedFileSecretStore, SecretStore,
};
pub use state::{
    AccountRuntimeHealth, AccountRuntimeMetadata, AttemptId, CandidateBeginOutcome,
    CheckpointResult, CleanupBatchOutcome, ClientTokenMetadata, ClientTokenScope,
    ClientTokenScopeParseError, CodexReplaySignature, CodexReplaySignaturePage,
    GlobalSessionIndexPage, GlobalSessionIndexPageKey, GlobalSessionUsagePage,
    GlobalSessionUsagePageKey, MAX_CODEX_REPLAY_SIGNATURES, MAX_PARSER_CHECKPOINT_BYTES,
    MAX_REQUEST_METRIC_BATCH_ROWS, MAX_SESSION_BATCH_BYTES, MAX_SESSION_BATCH_ROWS,
    MAX_SUPPLEMENTAL_BYTES, MAX_SUPPLEMENTAL_ROW_BYTES, MAX_SUPPLEMENTAL_ROWS, OpaqueFingerprint,
    ParserCheckpoint, ProviderMetadataBatch, ProviderMetadataBatchOutcome, ProviderRuntimeMetadata,
    ReadOnlyStateStore, ReplaySignaturePageKey, RequestId, RequestMetric,
    RequestSupplementalMetadata, RuntimeSecretBinding, STATE_STORE_WRITER_QUEUE_CAPACITY,
    SUPPLEMENTAL_CLEANUP_INTERVAL, ScopedClientTokenMetadata, SessionAvailability, SessionBatch,
    SessionFileIdentity, SessionGenerationState, SessionIndexPage, SessionIndexPageKey,
    SessionIndexRecord, SessionScanCursor, SessionScanResultCode, SessionSourceErrorCode,
    SessionSourceKind, SessionSourcePage, SessionSourcePageKey, SessionSourceState,
    SessionSourceStatus, SessionUsageAggregateBucket, SessionUsageAggregateFilter,
    SessionUsageAggregatePage, SessionUsageAggregatePageKey, SessionUsageAggregateTotals,
    SessionUsageGroupBy, SessionUsagePage, SessionUsagePageKey, SessionUsageRecord, StateHealth,
    StateStore, StateStoreWriteError, StateStoreWriteReceipt, StateStoreWriter,
    StateStoreWriterClient, StateStoreWriterShutdownError, StateStoreWriterShutdownHandle,
    StateStoreWriterShutdownReceipt, StateStoreWriterSubmitError, SupplementalBatchOutcome,
    SupplementalErrorCode, SupplementalFailoverDecision, SupplementalRetryDecision,
    SupplementalStorageStats, TraceId, WAL_CHECKPOINT_THRESHOLD_BYTES, state_store_writer,
};

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("configuration revision conflict: expected {expected}, found {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("invalid configuration: {message}")]
    InvalidConfig { message: String },
    #[error("failed to serialize configuration: {message}")]
    SerializeConfig { message: String },
    #[error("storage I/O error: {source}")]
    Io {
        #[source]
        source: std::io::Error,
    },
    #[error("state database is corrupt: {message}")]
    StateDatabaseCorrupt { message: String },
    #[error("state database error: {source}")]
    StateDatabase {
        #[source]
        source: rusqlite::Error,
    },
    #[error("runtime secret binding already exists at revision {actual}")]
    RuntimeSecretBindingConflict { actual: u64 },
    #[error("invalid state record: {message}")]
    InvalidStateRecord { message: String },
    #[error("stable {record_kind} identifier conflicts with persisted content")]
    StableRecordConflict { record_kind: &'static str },
    #[error("the Session batch exceeds its bounded row or byte limit")]
    SessionBatchLimitExceeded,
    #[error("the Session candidate is not in the required generation state")]
    CandidateStateConflict,
    #[error("the Session page key is stale")]
    StalePageKey,
    #[error("the StateStore writer is unavailable")]
    StateWriterUnavailable,
    #[error("the Codex replay-signature rollout exceeds its hard limit")]
    ReplaySignatureLimitExceeded,
    #[error("secret was not found")]
    SecretNotFound,
    #[error("a different secret already exists for this credential scope")]
    SecretAlreadyExists,
    #[error("the secret backend failed without exposing secret material")]
    SecretBackendFailure,
    #[error("the selected secret backend is read-only")]
    ReadOnlySecretStore,
    #[error("the explicit headless secret backend configuration does not match this store")]
    InvalidHeadlessSecretStoreConfig,
    #[error("the secret file grants access beyond the current user")]
    InsecureSecretFilePermissions,
    #[error("secret material is not valid UTF-8")]
    InvalidSecretEncoding,
    #[error("secret material exceeds the 64 KiB headless limit")]
    SecretTooLarge,
}
