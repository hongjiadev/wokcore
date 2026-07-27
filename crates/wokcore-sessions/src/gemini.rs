use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fmt,
    io::{self, Read},
    path::{Path, PathBuf},
};

use serde::{
    Deserializer,
    de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor},
};
use serde_json::Value;
use wokcore_platform::sessions::{
    SessionDirectoryLease, SessionError, SessionFile, SessionFileIdentity as PlatformFileIdentity,
    SessionFileKind, SessionRootLease,
};
use wokcore_storage::{
    CandidateBeginOutcome, MAX_SESSION_BATCH_BYTES, MAX_SESSION_BATCH_ROWS, ParserCheckpoint,
    SessionAvailability, SessionBatch, SessionFileIdentity, SessionGenerationState,
    SessionIndexRecord, SessionScanCursor, SessionScanResultCode, SessionSourceErrorCode,
    SessionSourceKind, SessionSourcePageKey, SessionSourceStatus, SessionUsageRecord, StateStore,
    StateStoreWriterClient, StorageError,
};

use crate::{
    cursor::{JsonlCursor, JsonlError, JsonlReader, JsonlRecordStatus},
    discovery::{
        DiscoveryLimits, SessionDiscoveryClock, SessionDiscoveryCursor, SessionDiscoveryEntry,
        SessionDiscoveryKind, SessionDiscoverySliceBudget, SessionDiscoverySliceError,
        SessionDiscoverySliceOutcome, SessionDiscoverySourceFormat, SystemSessionDiscoveryClock,
        discover_gemini_sessions_slice_with_clock,
    },
    model::{
        ExternalSessionTitle, MAX_ACTIVE_MESSAGES, OpaqueStreamHash, SESSION_BATCH_ROW_TARGET,
        SessionScanControl, SessionScanOutcome, SessionScanSummary, SessionScannerMetrics,
        SessionSourceScanSummary, fingerprints, fingerprints_with_extent, maximum_timestamp,
        normalize_external_id, normalize_external_model, normalize_timestamp, opaque_hash,
        opaque_hex, opaque_platform_identity, system_time_utc,
    },
    state::SessionState,
};

pub const MAX_LEGACY_JSON_PARSER_BYTES: usize = 256 * 1024;
pub const MAX_LEGACY_JSON_SOURCE_WORK_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_GEMINI_CURRENT_JSONL_SOURCE_WORK_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_GEMINI_LOGICAL_WORKING_BYTES: usize = 512 * 1024;

const LEGACY_READ_CHUNK_BYTES: usize = 64 * 1024;
const MAX_TITLE_SOURCE_WORK_BYTES: u64 = 64 * 1024 * 1024;
const MAX_GEMINI_CURRENT_JSONL_SOURCE_BYTES: u64 = MAX_GEMINI_CURRENT_JSONL_SOURCE_WORK_BYTES;
const MAX_LEGACY_JSON_STRING_BYTES: usize = 64 * 1024;
const MAX_LEGACY_MAP_KEY_BYTES: usize = 64;
const MAX_LEGACY_TIMESTAMP_BYTES: usize = 64;
const MAX_LEGACY_MESSAGE_TYPE_BYTES: usize = 16;
const PARSER_CHECKPOINT_VERSION: u16 = 1;
const FILE_IDENTITY_DOMAIN: &[u8] = b"wokcore.gemini.file-identity.v1";
const SOURCE_KEY_DOMAIN: &[u8] = b"wokcore.gemini.source-key.v1";
const SESSION_KEY_DOMAIN: &[u8] = b"wokcore.gemini.session-key.v1";
const USAGE_ID_DOMAIN: &[u8] = b"wokcore.gemini.usage-id.v1";
const LOGICAL_MESSAGE_DOMAIN: &[u8] = b"wokcore.gemini.logical-message.v1";
const FINGERPRINT_DOMAIN: &[u8] = b"wokcore.gemini.source-fingerprint.v1";
const STRUCTURAL_FINGERPRINT_DOMAIN: &[u8] = b"wokcore.gemini.structural-fingerprint.v1";
const CURRENT_FINGERPRINT_FORMAT: &[u8] = b"current-jsonl";
const LEGACY_FINGERPRINT_FORMAT: &[u8] = b"legacy-json";

#[derive(Debug, thiserror::Error)]
pub enum GeminiScannerError {
    #[error("Gemini Session storage failed")]
    Storage(#[from] StorageError),
    #[error("Gemini Session root is unavailable")]
    Root,
    #[error("Gemini Session discovery failed")]
    Discovery,
    #[error("Gemini Session read failed")]
    Read,
    #[error("Gemini Session record failed structural validation")]
    Parse,
    #[error("Gemini Session record exceeds its resource bound")]
    ResourceLimit,
    #[error("Gemini Session generation cleanup remains pending")]
    CleanupPending,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GeminiFormat {
    LegacyJson,
    CurrentJsonl,
}

pub(crate) fn structural_fingerprint(
    domain_key: &[u8; 32],
    format: GeminiFormat,
) -> OpaqueStreamHash {
    let format = match format {
        GeminiFormat::CurrentJsonl => CURRENT_FINGERPRINT_FORMAT,
        GeminiFormat::LegacyJson => LEGACY_FINGERPRINT_FORMAT,
    };
    OpaqueStreamHash::new(domain_key, STRUCTURAL_FINGERPRINT_DOMAIN, &[format])
}

#[derive(Clone)]
pub(crate) struct DiscoveredGeminiSession {
    pub(crate) relative_path: PathBuf,
    pub(crate) identity: PlatformFileIdentity,
    pub(crate) format: GeminiFormat,
}

impl DiscoveredGeminiSession {
    fn from_slice(entry: &SessionDiscoveryEntry) -> Option<Self> {
        let format = match entry.format() {
            SessionDiscoverySourceFormat::GeminiCurrentJsonl => GeminiFormat::CurrentJsonl,
            SessionDiscoverySourceFormat::GeminiLegacyJson => GeminiFormat::LegacyJson,
            _ => return None,
        };
        Some(Self {
            relative_path: entry.relative_path().to_path_buf(),
            identity: entry.identity(),
            format,
        })
    }

    pub(crate) fn open(&self, root: &SessionRootLease) -> Result<SessionFile, SessionError> {
        let maximum_size = match self.format {
            GeminiFormat::CurrentJsonl => MAX_GEMINI_CURRENT_JSONL_SOURCE_BYTES,
            GeminiFormat::LegacyJson => MAX_LEGACY_JSON_SOURCE_WORK_BYTES,
        };
        self.open_bounded(root, maximum_size)
    }

    fn open_bounded(
        &self,
        root: &SessionRootLease,
        maximum_size: u64,
    ) -> Result<SessionFile, SessionError> {
        let file = root.open_file(&self.relative_path, maximum_size)?;
        if file.snapshot().identity != self.identity {
            return Err(SessionError::SessionFileChanged);
        }
        Ok(file)
    }
}

impl fmt::Debug for DiscoveredGeminiSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiscoveredGeminiSession")
            .field("identity", &self.identity)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum GeminiScannerSlicePhase {
    Discovering,
    Processing,
}

struct GeminiSliceCycle {
    transition_at: String,
    phase: GeminiScannerSlicePhase,
    cursor: SessionDiscoveryCursor,
    persisted_sources: HashSet<String>,
    persisted_identities: HashMap<String, String>,
    reserved_keys: HashMap<String, String>,
    source_keys_by_identity: HashMap<String, String>,
    current_sources: HashSet<String>,
    processed_identities: HashSet<String>,
}

impl GeminiSliceCycle {
    fn new(scanner: &GeminiScanner, transition_at: &str) -> Result<Self, GeminiScannerError> {
        Ok(Self {
            transition_at: transition_at.to_owned(),
            phase: GeminiScannerSlicePhase::Discovering,
            cursor: SessionDiscoveryCursor::with_limits(
                SessionDiscoveryKind::Gemini,
                scanner.discovery_limits,
            )
            .map_err(|_| GeminiScannerError::ResourceLimit)?,
            persisted_sources: scanner.persisted_sources()?,
            persisted_identities: scanner.persisted_identity_sources()?,
            reserved_keys: HashMap::new(),
            source_keys_by_identity: HashMap::new(),
            current_sources: HashSet::new(),
            processed_identities: HashSet::new(),
        })
    }

    fn observe(
        &mut self,
        scanner: &GeminiScanner,
        source: &DiscoveredGeminiSession,
    ) -> Result<(), GeminiScannerError> {
        let identity = scanner.file_identity(source).as_str().to_owned();
        if self.source_keys_by_identity.contains_key(&identity) {
            return Ok(());
        }
        let source_key = if let Some(source_key) = self.persisted_identities.get(&identity) {
            source_key.clone()
        } else {
            let path_key = scanner.source_key(source);
            if !self.reserved_keys.contains_key(&path_key) {
                path_key
            } else {
                let mut counter = 0u64;
                loop {
                    let candidate = opaque_hex(
                        &scanner.domain_key,
                        b"wokcore.gemini.source-collision.v1",
                        &[
                            &path_bytes(&canonical_relative_path(&source.relative_path)),
                            identity.as_bytes(),
                            &counter.to_be_bytes(),
                        ],
                    );
                    if !self.reserved_keys.contains_key(&candidate)
                        && !self.persisted_sources.contains(&candidate)
                    {
                        break candidate;
                    }
                    counter = counter.checked_add(1).ok_or(GeminiScannerError::Parse)?;
                }
            }
        };
        if self
            .reserved_keys
            .insert(source_key.clone(), identity.clone())
            .is_some_and(|existing| existing != identity)
        {
            return Err(StorageError::StableRecordConflict {
                record_kind: "Session source key",
            }
            .into());
        }
        self.current_sources.insert(source_key.clone());
        self.source_keys_by_identity.insert(identity, source_key);
        Ok(())
    }
}

fn map_gemini_slice_error(error: SessionDiscoverySliceError) -> GeminiScannerError {
    match error {
        SessionDiscoverySliceError::Discovery(crate::discovery::DiscoveryError::Limit) => {
            GeminiScannerError::ResourceLimit
        }
        SessionDiscoverySliceError::CursorKindMismatch
        | SessionDiscoverySliceError::Discovery(_) => GeminiScannerError::Discovery,
    }
}

#[derive(Clone)]
struct GeminiUsage {
    model: String,
    occurred_at: String,
    input: u64,
    output: u64,
    cached: u64,
    thoughts: u64,
    revision: u64,
}

#[derive(Clone)]
struct GeminiLogicalMessage {
    message_id: String,
    occurred_at: String,
    usage: Option<GeminiUsage>,
    usage_snapshot_valid: bool,
}

struct GeminiAggregate {
    session_id: String,
    created_at: String,
    last_active_at: String,
    messages: Vec<GeminiLogicalMessage>,
    complete_byte_offset: u64,
    record_ordinal: u64,
    parser_read_bytes: u64,
    peak_parser_buffer_bytes: usize,
    peak_logical_working_bytes: usize,
    structural_hash: [u8; 32],
    usage_count: u64,
    usage_count_inspections: u64,
}

impl GeminiAggregate {
    fn usage(&self) -> impl Iterator<Item = (&GeminiLogicalMessage, &GeminiUsage)> {
        self.messages
            .iter()
            .filter_map(|message| message.usage.as_ref().map(|usage| (message, usage)))
    }

    fn usage_count(&self) -> u64 {
        self.usage_count
    }
}

pub struct GeminiScanner {
    root: SessionRootLease,
    state: SessionState,
    domain_key: [u8; 32],
    discovery_limits: DiscoveryLimits,
    metrics: SessionScannerMetrics,
    slice_cycle: Option<GeminiSliceCycle>,
}

impl GeminiScanner {
    pub fn open(
        root_path: impl AsRef<Path>,
        state_path: impl AsRef<Path>,
        domain_key: [u8; 32],
    ) -> Result<Self, GeminiScannerError> {
        Self::open_internal(root_path, state_path, domain_key, None)
    }

    pub fn open_with_writer(
        root_path: impl AsRef<Path>,
        state_path: impl AsRef<Path>,
        domain_key: [u8; 32],
        writer: StateStoreWriterClient,
    ) -> Result<Self, GeminiScannerError> {
        Self::open_internal(root_path, state_path, domain_key, Some(writer))
    }

    fn open_internal(
        root_path: impl AsRef<Path>,
        state_path: impl AsRef<Path>,
        domain_key: [u8; 32],
        writer: Option<StateStoreWriterClient>,
    ) -> Result<Self, GeminiScannerError> {
        let root = SessionRootLease::open(root_path).map_err(|_| GeminiScannerError::Root)?;
        let state = SessionState::open(state_path, writer)?;
        Ok(Self {
            root,
            state,
            domain_key,
            discovery_limits: DiscoveryLimits::default(),
            metrics: SessionScannerMetrics::default(),
            slice_cycle: None,
        })
    }

    pub fn state(&self) -> &StateStore {
        self.state.reader()
    }

    pub fn scan_slice(
        &mut self,
        transition_at: &str,
        control: SessionScanControl,
        budget: SessionDiscoverySliceBudget,
    ) -> Result<SessionScanSummary, GeminiScannerError> {
        self.scan_slice_with_clock(transition_at, control, budget, &SystemSessionDiscoveryClock)
    }

    pub fn scan_slice_with_clock<C>(
        &mut self,
        transition_at: &str,
        control: SessionScanControl,
        budget: SessionDiscoverySliceBudget,
        clock: &C,
    ) -> Result<SessionScanSummary, GeminiScannerError>
    where
        C: SessionDiscoveryClock + ?Sized,
    {
        self.metrics = SessionScannerMetrics::default();
        let mut cycle = match self.slice_cycle.take() {
            Some(cycle) => cycle,
            None => GeminiSliceCycle::new(self, transition_at)?,
        };
        let result = self.scan_slice_cycle(&mut cycle, control, budget, clock);
        match result {
            Ok((summary, complete)) => {
                if !complete {
                    self.slice_cycle = Some(cycle);
                }
                Ok(summary)
            }
            Err(error) => Err(error),
        }
    }

    fn scan_slice_cycle<C>(
        &mut self,
        cycle: &mut GeminiSliceCycle,
        control: SessionScanControl,
        budget: SessionDiscoverySliceBudget,
        clock: &C,
    ) -> Result<(SessionScanSummary, bool), GeminiScannerError>
    where
        C: SessionDiscoveryClock + ?Sized,
    {
        match cycle.phase {
            GeminiScannerSlicePhase::Discovering => {
                let slice = discover_gemini_sessions_slice_with_clock(
                    &self.root,
                    &mut cycle.cursor,
                    budget,
                    clock,
                )
                .map_err(map_gemini_slice_error)?;
                for entry in &slice.entries {
                    let Some(source) = DiscoveredGeminiSession::from_slice(entry) else {
                        continue;
                    };
                    cycle.observe(self, &source)?;
                }
                let mut deleted_sources = 0;
                if slice.outcome == SessionDiscoverySliceOutcome::Complete {
                    for source_key in cycle.persisted_sources.difference(&cycle.current_sources) {
                        let _ = self.state.mark_source_unavailable(
                            source_key,
                            SessionSourceErrorCode::SourceSessionsAbsent,
                            &cycle.transition_at,
                        )?;
                        deleted_sources += 1;
                    }
                    cycle.phase = GeminiScannerSlicePhase::Processing;
                    cycle.cursor = SessionDiscoveryCursor::with_limits(
                        SessionDiscoveryKind::Gemini,
                        self.discovery_limits,
                    )
                    .map_err(|_| GeminiScannerError::ResourceLimit)?;
                    cycle.persisted_sources = HashSet::new();
                    cycle.persisted_identities = HashMap::new();
                    cycle.reserved_keys = HashMap::new();
                    cycle.current_sources = HashSet::new();
                }
                Ok((
                    SessionScanSummary {
                        outcome: SessionScanOutcome::Interrupted,
                        advanced_sources: 0,
                        unchanged_sources: 0,
                        deleted_sources,
                        sources: Vec::new(),
                        metrics: self.metrics.clone(),
                    },
                    false,
                ))
            }
            GeminiScannerSlicePhase::Processing => {
                let slice = discover_gemini_sessions_slice_with_clock(
                    &self.root,
                    &mut cycle.cursor,
                    budget,
                    clock,
                )
                .map_err(map_gemini_slice_error)?;
                let mut summaries = Vec::new();
                let mut advanced_sources = 0;
                let mut unchanged_sources = 0;
                let mut committed_batches = 0;
                let mut restart_processing = false;
                for entry in &slice.entries {
                    let Some(source) = DiscoveredGeminiSession::from_slice(entry) else {
                        continue;
                    };
                    let identity = self.file_identity(&source).as_str().to_owned();
                    if cycle.processed_identities.contains(&identity) {
                        continue;
                    }
                    let Some(source_key) = cycle.source_keys_by_identity.get(&identity).cloned()
                    else {
                        continue;
                    };
                    let (summary, process) = self.process_slice_source(
                        &source,
                        source_key,
                        &cycle.transition_at,
                        control,
                        &mut committed_batches,
                    )?;
                    match process {
                        Some(ProcessOutcome::Advanced) => advanced_sources += 1,
                        Some(ProcessOutcome::Unchanged) => unchanged_sources += 1,
                        Some(ProcessOutcome::Interrupted) => restart_processing = true,
                        None => {}
                    }
                    summaries.push(summary);
                    if restart_processing {
                        break;
                    }
                    cycle.processed_identities.insert(identity);
                }
                if restart_processing {
                    cycle.cursor = SessionDiscoveryCursor::with_limits(
                        SessionDiscoveryKind::Gemini,
                        self.discovery_limits,
                    )
                    .map_err(|_| GeminiScannerError::ResourceLimit)?;
                }
                let complete =
                    slice.outcome == SessionDiscoverySliceOutcome::Complete && !restart_processing;
                Ok((
                    SessionScanSummary {
                        outcome: if complete {
                            SessionScanOutcome::Complete
                        } else {
                            SessionScanOutcome::Interrupted
                        },
                        advanced_sources,
                        unchanged_sources,
                        deleted_sources: 0,
                        sources: summaries,
                        metrics: self.metrics.clone(),
                    },
                    complete,
                ))
            }
        }
    }

    fn process_slice_source(
        &mut self,
        source: &DiscoveredGeminiSession,
        source_key: String,
        transition_at: &str,
        control: SessionScanControl,
        committed_batches: &mut usize,
    ) -> Result<(SessionSourceScanSummary, Option<ProcessOutcome>), GeminiScannerError> {
        match self.process_source(
            source,
            &source_key,
            transition_at,
            control,
            committed_batches,
        ) {
            Ok(result) => Ok((result.summary, Some(result.process))),
            Err(
                error @ (GeminiScannerError::Read
                | GeminiScannerError::Parse
                | GeminiScannerError::ResourceLimit),
            ) => {
                let code = match error {
                    GeminiScannerError::Read => SessionSourceErrorCode::SourceIoFailed,
                    GeminiScannerError::Parse => SessionSourceErrorCode::SourceParseInvalid,
                    GeminiScannerError::ResourceLimit => {
                        SessionSourceErrorCode::SourceRecordTooLarge
                    }
                    _ => unreachable!(),
                };
                self.record_failure(source, &source_key, code, transition_at)?;
                let state = self.state.load_session_source(&source_key)?;
                let current = self.state.load_current_session_scan_cursor(&source_key)?;
                Ok((
                    SessionSourceScanSummary {
                        source_key,
                        session_key: None,
                        status: state.as_ref().map_or_else(
                            || {
                                if code == SessionSourceErrorCode::SourceRecordTooLarge {
                                    SessionSourceStatus::ResourceLimited
                                } else {
                                    SessionSourceStatus::Unavailable
                                }
                            },
                            |state| state.status,
                        ),
                        error_code: Some(code),
                        complete_byte_offset: current
                            .map_or(0, |cursor| cursor.complete_byte_offset),
                    },
                    None,
                ))
            }
            Err(GeminiScannerError::CleanupPending) => {
                let state = self.state.load_session_source(&source_key)?;
                let current = self.state.load_current_session_scan_cursor(&source_key)?;
                let session_key = self
                    .state
                    .load_current_session_index_page(&source_key, None, 1)?
                    .items
                    .into_iter()
                    .next()
                    .map(|index| index.session_key);
                Ok((
                    SessionSourceScanSummary {
                        source_key,
                        session_key,
                        status: state
                            .map_or(SessionSourceStatus::Unavailable, |state| state.status),
                        error_code: Some(SessionSourceErrorCode::SourceCandidateInterrupted),
                        complete_byte_offset: current
                            .map_or(0, |cursor| cursor.complete_byte_offset),
                    },
                    Some(ProcessOutcome::Interrupted),
                ))
            }
            Err(error) => Err(error),
        }
    }

    fn record_aggregate_metrics(&mut self, aggregate: &GeminiAggregate) {
        self.metrics.parser_read_bytes = self
            .metrics
            .parser_read_bytes
            .saturating_add(aggregate.parser_read_bytes);
        self.metrics.peak_parser_buffer_bytes = self.metrics.peak_parser_buffer_bytes.max(
            aggregate
                .peak_parser_buffer_bytes
                .saturating_add(aggregate.peak_logical_working_bytes),
        );
        self.metrics.aggregate_message_inspections = self
            .metrics
            .aggregate_message_inspections
            .saturating_add(aggregate.usage_count_inspections);
    }

    pub fn scan(
        &mut self,
        transition_at: &str,
        control: SessionScanControl,
    ) -> Result<SessionScanSummary, GeminiScannerError> {
        self.slice_cycle = None;
        self.metrics = SessionScannerMetrics::default();
        let discovered = discover_gemini_sessions(&self.root, self.discovery_limits)?;
        let persisted = self.persisted_sources()?;
        let persisted_identities = self.persisted_identity_sources()?;
        let source_keys =
            self.resolve_source_keys(&discovered, &persisted, &persisted_identities)?;
        let current_keys = source_keys.iter().cloned().collect::<HashSet<_>>();
        let mut deleted_sources = 0;
        for source_key in persisted.difference(&current_keys) {
            let _ = self.state.mark_source_unavailable(
                source_key,
                SessionSourceErrorCode::SourceSessionsAbsent,
                transition_at,
            )?;
            deleted_sources += 1;
        }

        let mut summaries = Vec::with_capacity(discovered.len());
        let mut advanced_sources = 0;
        let mut unchanged_sources = 0;
        let mut outcome = SessionScanOutcome::Complete;
        let mut committed_batches = 0;
        for (source, source_key) in discovered.iter().zip(source_keys) {
            match self.process_source(
                source,
                &source_key,
                transition_at,
                control,
                &mut committed_batches,
            ) {
                Ok(result) => {
                    match result.process {
                        ProcessOutcome::Advanced => advanced_sources += 1,
                        ProcessOutcome::Unchanged => unchanged_sources += 1,
                        ProcessOutcome::Interrupted => outcome = SessionScanOutcome::Interrupted,
                    }
                    summaries.push(result.summary);
                    if result.process == ProcessOutcome::Interrupted {
                        break;
                    }
                }
                Err(
                    error @ (GeminiScannerError::Read
                    | GeminiScannerError::Parse
                    | GeminiScannerError::ResourceLimit),
                ) => {
                    let code = match error {
                        GeminiScannerError::Read => SessionSourceErrorCode::SourceIoFailed,
                        GeminiScannerError::Parse => SessionSourceErrorCode::SourceParseInvalid,
                        GeminiScannerError::ResourceLimit => {
                            SessionSourceErrorCode::SourceRecordTooLarge
                        }
                        _ => unreachable!(),
                    };
                    self.record_failure(source, &source_key, code, transition_at)?;
                    let state = self.state.load_session_source(&source_key)?;
                    let current = self.state.load_current_session_scan_cursor(&source_key)?;
                    summaries.push(SessionSourceScanSummary {
                        source_key,
                        session_key: None,
                        status: state.as_ref().map_or_else(
                            || {
                                if code == SessionSourceErrorCode::SourceRecordTooLarge {
                                    SessionSourceStatus::ResourceLimited
                                } else {
                                    SessionSourceStatus::Unavailable
                                }
                            },
                            |state| state.status,
                        ),
                        error_code: Some(code),
                        complete_byte_offset: current
                            .map_or(0, |cursor| cursor.complete_byte_offset),
                    });
                }
                Err(GeminiScannerError::CleanupPending) => {
                    outcome = SessionScanOutcome::Interrupted;
                    let state = self.state.load_session_source(&source_key)?;
                    let current = self.state.load_current_session_scan_cursor(&source_key)?;
                    let session_key = self
                        .state
                        .load_current_session_index_page(&source_key, None, 1)?
                        .items
                        .into_iter()
                        .next()
                        .map(|index| index.session_key);
                    summaries.push(SessionSourceScanSummary {
                        source_key,
                        session_key,
                        status: state
                            .map_or(SessionSourceStatus::Unavailable, |state| state.status),
                        error_code: Some(SessionSourceErrorCode::SourceCandidateInterrupted),
                        complete_byte_offset: current
                            .map_or(0, |cursor| cursor.complete_byte_offset),
                    });
                }
                Err(error) => return Err(error),
            }
        }
        summaries.sort_unstable_by(|left, right| left.source_key.cmp(&right.source_key));
        Ok(SessionScanSummary {
            outcome,
            advanced_sources,
            unchanged_sources,
            deleted_sources,
            sources: summaries,
            metrics: self.metrics.clone(),
        })
    }

    fn process_source(
        &mut self,
        source: &DiscoveredGeminiSession,
        source_key: &str,
        transition_at: &str,
        control: SessionScanControl,
        committed_batches: &mut usize,
    ) -> Result<ProcessResult, GeminiScannerError> {
        self.metrics.source_opens = self.metrics.source_opens.saturating_add(1);
        let mut file = source.open(&self.root).map_err(|error| match error {
            SessionError::ReadLimitExceeded => GeminiScannerError::ResourceLimit,
            _ => GeminiScannerError::Read,
        })?;
        let snapshot = file.snapshot().clone();
        let modified_at = system_time_utc(snapshot.modified);
        let current = self.state.load_current_session_scan_cursor(source_key)?;
        let source_state = self.state.load_session_source(source_key)?;
        let mut cleanup_performed = false;
        if !self.cleanup_pending_generation_once(source_key, &mut cleanup_performed)? {
            if let Some(generation) = source_state
                .as_ref()
                .and_then(|state| state.current_generation)
            {
                let _ = self.state.fail_candidate(
                    source_key,
                    generation,
                    SessionSourceErrorCode::SourceCandidateInterrupted,
                    transition_at,
                )?;
            }
            return Err(GeminiScannerError::CleanupPending);
        }
        let source_state = self.state.load_session_source(source_key)?;

        let mut verified_aggregate = None;
        if let Some(cursor) = &current {
            let (head, boundary) = fingerprints_with_extent(
                &mut file,
                cursor.complete_byte_offset,
                cursor.observed_size,
                &self.domain_key,
                FINGERPRINT_DOMAIN,
            )
            .map_err(map_jsonl_error)?;
            let state_is_healthy = source_state.as_ref().is_some_and(|state| {
                (state.status == SessionSourceStatus::Available && state.error_code.is_none())
                    || (state.error_code
                        == Some(SessionSourceErrorCode::SourceCandidateInterrupted)
                        && state.current_generation == Some(cursor.generation)
                        && state.staging_generation.is_none()
                        && state.retired_generation.is_none())
            });
            if cursor.file_identity == self.file_identity(source)
                && cursor.observed_size == snapshot.size
                && cursor.modified_at == modified_at
                && cursor.head_fingerprint == head
                && cursor.boundary_fingerprint == boundary
                && cursor.parser_checkpoint.version == PARSER_CHECKPOINT_VERSION
                && cursor.result_code == Some(SessionScanResultCode::Advanced)
                && state_is_healthy
            {
                let aggregate = match source.format {
                    GeminiFormat::CurrentJsonl => parse_current_jsonl(&mut file, &self.domain_key)?,
                    GeminiFormat::LegacyJson => parse_legacy_json(&mut file, &self.domain_key)?,
                };
                validate_usage_storage_bounds(&aggregate)?;
                self.record_aggregate_metrics(&aggregate);
                if cursor.parser_checkpoint.structural_hash == Some(aggregate.structural_hash) {
                    return Ok(ProcessResult {
                        process: ProcessOutcome::Unchanged,
                        summary: self.summary_from_current(source_key)?,
                    });
                }
                verified_aggregate = Some(aggregate);
            }
        }

        self.metrics.full_source_scans = self.metrics.full_source_scans.saturating_add(1);
        let was_preverified = verified_aggregate.is_some();
        let aggregate = match verified_aggregate {
            Some(aggregate) => aggregate,
            None => match source.format {
                GeminiFormat::CurrentJsonl => parse_current_jsonl(&mut file, &self.domain_key)?,
                GeminiFormat::LegacyJson => parse_legacy_json(&mut file, &self.domain_key)?,
            },
        };
        validate_usage_storage_bounds(&aggregate)?;
        if !was_preverified {
            self.record_aggregate_metrics(&aggregate);
        }
        let generation = next_generation(current.as_ref().map(|cursor| cursor.generation))?;
        let mut cursor = self.prepare_candidate(
            source,
            source_key,
            generation,
            &snapshot,
            &modified_at,
            &mut file,
            transition_at,
            &mut cleanup_performed,
            aggregate.structural_hash,
        )?;
        let mut usage_entries = aggregate.usage().collect::<Vec<_>>();
        usage_entries.sort_unstable_by_key(|(_, usage)| usage.revision);
        let emitted = usize::try_from(cursor.parser_checkpoint.event_ordinal)
            .map_err(|_| GeminiScannerError::Parse)?;
        if emitted > usage_entries.len() {
            return Err(GeminiScannerError::Parse);
        }
        for chunk in usage_entries[emitted..].chunks(SESSION_BATCH_ROW_TARGET) {
            let next = cursor
                .parser_checkpoint
                .event_ordinal
                .checked_add(chunk.len() as u64)
                .ok_or(GeminiScannerError::Parse)?;
            cursor = self.completed_cursor(
                source,
                source_key,
                generation,
                &snapshot,
                &modified_at,
                &aggregate,
                &mut file,
                next,
                chunk
                    .last()
                    .map_or(aggregate.record_ordinal, |(_, usage)| usage.revision),
                transition_at,
            )?;
            self.state.commit_candidate_batch(&SessionBatch {
                cursor: Some(cursor.clone()),
                index_records: vec![self.index_record(source_key, generation, &aggregate)],
                usage_records: chunk
                    .iter()
                    .map(|(message, usage)| {
                        self.usage_record(source_key, generation, &aggregate, message, usage)
                    })
                    .collect(),
                replay_signatures: Vec::new(),
                supplemental_metadata: Vec::new(),
            })?;
            self.metrics.committed_batches = self.metrics.committed_batches.saturating_add(1);
            *committed_batches += 1;
            if control
                .stop_after_committed_batches
                .is_some_and(|maximum| *committed_batches >= maximum)
            {
                let _ = self.state.fail_candidate(
                    source_key,
                    generation,
                    SessionSourceErrorCode::SourceCandidateInterrupted,
                    transition_at,
                )?;
                return Ok(ProcessResult {
                    process: ProcessOutcome::Interrupted,
                    summary: self.interrupted_summary(source_key, &aggregate)?,
                });
            }
        }
        if usage_entries.is_empty()
            || cursor.parser_checkpoint.event_ordinal < usage_entries.len() as u64
        {
            cursor = self.completed_cursor(
                source,
                source_key,
                generation,
                &snapshot,
                &modified_at,
                &aggregate,
                &mut file,
                usage_entries.len() as u64,
                aggregate.record_ordinal,
                transition_at,
            )?;
            self.state.commit_candidate_batch(&SessionBatch {
                cursor: Some(cursor),
                index_records: vec![self.index_record(source_key, generation, &aggregate)],
                usage_records: Vec::new(),
                replay_signatures: Vec::new(),
                supplemental_metadata: Vec::new(),
            })?;
            self.metrics.committed_batches = self.metrics.committed_batches.saturating_add(1);
            *committed_batches += 1;
            if control
                .stop_after_committed_batches
                .is_some_and(|maximum| *committed_batches >= maximum)
            {
                let _ = self.state.fail_candidate(
                    source_key,
                    generation,
                    SessionSourceErrorCode::SourceCandidateInterrupted,
                    transition_at,
                )?;
                return Ok(ProcessResult {
                    process: ProcessOutcome::Interrupted,
                    summary: self.interrupted_summary(source_key, &aggregate)?,
                });
            }
        }
        self.state
            .promote_candidate(source_key, generation, transition_at)?;
        if !self.cleanup_pending_generation_once(source_key, &mut cleanup_performed)? {
            let _ = self.state.fail_candidate(
                source_key,
                generation,
                SessionSourceErrorCode::SourceCandidateInterrupted,
                transition_at,
            )?;
            return Err(GeminiScannerError::CleanupPending);
        }
        Ok(ProcessResult {
            process: ProcessOutcome::Advanced,
            summary: SessionSourceScanSummary {
                source_key: source_key.to_owned(),
                session_key: Some(self.session_key(&aggregate.session_id)),
                status: SessionSourceStatus::Available,
                error_code: None,
                complete_byte_offset: aggregate.complete_byte_offset,
            },
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_candidate(
        &mut self,
        source: &DiscoveredGeminiSession,
        source_key: &str,
        generation: u64,
        snapshot: &wokcore_platform::sessions::SessionFileSnapshot,
        modified_at: &str,
        file: &mut SessionFile,
        transition_at: &str,
        cleanup_performed: &mut bool,
        structural_hash: [u8; 32],
    ) -> Result<SessionScanCursor, GeminiScannerError> {
        if let Some(staging) = self.state.load_staging_session_scan_cursor(source_key)? {
            if staging.generation == generation
                && staging.file_identity == self.file_identity(source)
                && staging.observed_size == snapshot.size
                && staging.modified_at == modified_at
                && staging.parser_checkpoint.version == PARSER_CHECKPOINT_VERSION
                && staging.parser_checkpoint.structural_hash == Some(structural_hash)
            {
                let (head, boundary) = fingerprints_with_extent(
                    file,
                    staging.complete_byte_offset,
                    staging.observed_size,
                    &self.domain_key,
                    FINGERPRINT_DOMAIN,
                )
                .map_err(map_jsonl_error)?;
                if head == staging.head_fingerprint && boundary == staging.boundary_fingerprint {
                    return match self.state.begin_or_resume_candidate(&staging)? {
                        CandidateBeginOutcome::Resumed(cursor) => Ok(*cursor),
                        CandidateBeginOutcome::Started => Ok(staging),
                        CandidateBeginOutcome::CleanupRequired { generation } => {
                            if !self.cleanup_generation_once(
                                source_key,
                                generation,
                                cleanup_performed,
                            )? {
                                return Err(GeminiScannerError::CleanupPending);
                            }
                            self.prepare_candidate(
                                source,
                                source_key,
                                generation,
                                snapshot,
                                modified_at,
                                file,
                                transition_at,
                                cleanup_performed,
                                structural_hash,
                            )
                        }
                    };
                }
            }
            if !self.cleanup_generation_once(source_key, staging.generation, cleanup_performed)? {
                return Err(GeminiScannerError::CleanupPending);
            }
        }
        let (head, boundary) =
            fingerprints(file, 0, &self.domain_key, FINGERPRINT_DOMAIN).map_err(map_jsonl_error)?;
        let cursor = SessionScanCursor {
            source_key: source_key.to_owned(),
            source_kind: SessionSourceKind::Gemini,
            generation,
            generation_state: SessionGenerationState::Staging,
            file_identity: self.file_identity(source),
            observed_size: snapshot.size,
            modified_at: modified_at.to_owned(),
            complete_byte_offset: 0,
            stable_record_ordinal: 0,
            parser_checkpoint: parser_checkpoint(0, Some(structural_hash)),
            head_fingerprint: head,
            boundary_fingerprint: boundary,
            parent_source_key: None,
            parent_generation: None,
            replay_boundary_fingerprint: None,
            result_code: Some(SessionScanResultCode::Deferred),
            result_changed_at: Some(transition_at.to_owned()),
        };
        match self.state.begin_or_resume_candidate(&cursor)? {
            CandidateBeginOutcome::Started => Ok(cursor),
            CandidateBeginOutcome::Resumed(cursor) => Ok(*cursor),
            CandidateBeginOutcome::CleanupRequired { generation } => {
                if !self.cleanup_generation_once(source_key, generation, cleanup_performed)? {
                    return Err(GeminiScannerError::CleanupPending);
                }
                match self.state.begin_or_resume_candidate(&cursor)? {
                    CandidateBeginOutcome::Started => Ok(cursor),
                    CandidateBeginOutcome::Resumed(cursor) => Ok(*cursor),
                    CandidateBeginOutcome::CleanupRequired { .. } => {
                        Err(GeminiScannerError::CleanupPending)
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn completed_cursor(
        &self,
        source: &DiscoveredGeminiSession,
        source_key: &str,
        generation: u64,
        snapshot: &wokcore_platform::sessions::SessionFileSnapshot,
        modified_at: &str,
        aggregate: &GeminiAggregate,
        file: &mut SessionFile,
        emitted_usage: u64,
        stable_record_ordinal: u64,
        transition_at: &str,
    ) -> Result<SessionScanCursor, GeminiScannerError> {
        let complete = emitted_usage >= aggregate.usage_count();
        let complete_byte_offset = if complete {
            aggregate.complete_byte_offset
        } else {
            0
        };
        let stable_record_ordinal = if complete {
            aggregate.record_ordinal
        } else {
            stable_record_ordinal
        };
        let (head, boundary) = fingerprints(
            file,
            complete_byte_offset,
            &self.domain_key,
            FINGERPRINT_DOMAIN,
        )
        .map_err(map_jsonl_error)?;
        Ok(SessionScanCursor {
            source_key: source_key.to_owned(),
            source_kind: SessionSourceKind::Gemini,
            generation,
            generation_state: SessionGenerationState::Staging,
            file_identity: self.file_identity(source),
            observed_size: snapshot.size,
            modified_at: modified_at.to_owned(),
            complete_byte_offset,
            stable_record_ordinal,
            parser_checkpoint: parser_checkpoint(emitted_usage, Some(aggregate.structural_hash)),
            head_fingerprint: head,
            boundary_fingerprint: boundary,
            parent_source_key: None,
            parent_generation: None,
            replay_boundary_fingerprint: None,
            result_code: Some(if complete {
                SessionScanResultCode::Advanced
            } else {
                SessionScanResultCode::Deferred
            }),
            result_changed_at: Some(transition_at.to_owned()),
        })
    }

    fn index_record(
        &self,
        source_key: &str,
        generation: u64,
        aggregate: &GeminiAggregate,
    ) -> SessionIndexRecord {
        SessionIndexRecord {
            session_key: self.session_key(&aggregate.session_id),
            source_key: source_key.to_owned(),
            generation,
            source_kind: SessionSourceKind::Gemini,
            created_at: aggregate.created_at.clone(),
            last_active_at: aggregate.last_active_at.clone(),
            message_count: aggregate.messages.len() as u64,
            usage_event_count: aggregate.usage_count(),
            availability: SessionAvailability::Available,
        }
    }

    fn usage_record(
        &self,
        source_key: &str,
        generation: u64,
        aggregate: &GeminiAggregate,
        message: &GeminiLogicalMessage,
        usage: &GeminiUsage,
    ) -> SessionUsageRecord {
        SessionUsageRecord {
            usage_id: opaque_hex(
                &self.domain_key,
                USAGE_ID_DOMAIN,
                &[source_key.as_bytes(), message.message_id.as_bytes()],
            ),
            session_key: self.session_key(&aggregate.session_id),
            source_key: source_key.to_owned(),
            generation,
            source_kind: SessionSourceKind::Gemini,
            model: usage.model.clone(),
            occurred_at: usage.occurred_at.clone(),
            input_tokens: usage.input,
            output_tokens: usage.output,
            cache_read_tokens: usage.cached,
            cache_write_tokens: 0,
            reasoning_tokens: usage.thoughts,
            record_revision: usage.revision,
        }
    }

    fn summary_from_current(
        &self,
        source_key: &str,
    ) -> Result<SessionSourceScanSummary, GeminiScannerError> {
        let cursor = self.state.load_current_session_scan_cursor(source_key)?;
        let index = self
            .state
            .load_current_session_index_page(source_key, None, 1)?
            .items
            .into_iter()
            .next();
        Ok(SessionSourceScanSummary {
            source_key: source_key.to_owned(),
            session_key: index.map(|index| index.session_key),
            status: SessionSourceStatus::Available,
            error_code: None,
            complete_byte_offset: cursor.map_or(0, |cursor| cursor.complete_byte_offset),
        })
    }

    fn interrupted_summary(
        &self,
        source_key: &str,
        aggregate: &GeminiAggregate,
    ) -> Result<SessionSourceScanSummary, GeminiScannerError> {
        let source = self.state.load_session_source(source_key)?;
        let current = self.state.load_current_session_scan_cursor(source_key)?;
        Ok(SessionSourceScanSummary {
            source_key: source_key.to_owned(),
            session_key: Some(self.session_key(&aggregate.session_id)),
            status: source.map_or(SessionSourceStatus::Unavailable, |source| source.status),
            error_code: Some(SessionSourceErrorCode::SourceCandidateInterrupted),
            complete_byte_offset: current.map_or(0, |cursor| cursor.complete_byte_offset),
        })
    }

    fn record_failure(
        &mut self,
        source: &DiscoveredGeminiSession,
        source_key: &str,
        code: SessionSourceErrorCode,
        transition_at: &str,
    ) -> Result<(), GeminiScannerError> {
        if let Some(source_state) = self.state.load_session_source(source_key)?
            && let Some(generation) = source_state
                .staging_generation
                .or(source_state.current_generation)
        {
            let _ = self
                .state
                .fail_candidate(source_key, generation, code, transition_at)?;
            return Ok(());
        }
        let mut file = match source.open(&self.root) {
            Ok(file) => file,
            Err(_) => return Ok(()),
        };
        let snapshot = file.snapshot().clone();
        let (head, boundary) = fingerprints(&mut file, 0, &self.domain_key, FINGERPRINT_DOMAIN)
            .map_err(map_jsonl_error)?;
        let cursor = SessionScanCursor {
            source_key: source_key.to_owned(),
            source_kind: SessionSourceKind::Gemini,
            generation: 1,
            generation_state: SessionGenerationState::Staging,
            file_identity: self.file_identity(source),
            observed_size: snapshot.size,
            modified_at: system_time_utc(snapshot.modified),
            complete_byte_offset: 0,
            stable_record_ordinal: 0,
            parser_checkpoint: parser_checkpoint(0, None),
            head_fingerprint: head,
            boundary_fingerprint: boundary,
            parent_source_key: None,
            parent_generation: None,
            replay_boundary_fingerprint: None,
            result_code: Some(SessionScanResultCode::Deferred),
            result_changed_at: Some(transition_at.to_owned()),
        };
        match self.state.begin_or_resume_candidate(&cursor)? {
            CandidateBeginOutcome::CleanupRequired { generation } => {
                let mut cleanup_performed = false;
                if !self.cleanup_generation_once(source_key, generation, &mut cleanup_performed)? {
                    return Ok(());
                }
                let _ = self.state.begin_or_resume_candidate(&cursor)?;
            }
            CandidateBeginOutcome::Started | CandidateBeginOutcome::Resumed(_) => {}
        }
        let _ = self
            .state
            .fail_candidate(source_key, 1, code, transition_at)?;
        Ok(())
    }

    fn cleanup_pending_generation_once(
        &mut self,
        source_key: &str,
        cleanup_performed: &mut bool,
    ) -> Result<bool, GeminiScannerError> {
        let Some(generation) = self
            .state
            .load_session_source(source_key)?
            .and_then(|state| state.retired_generation)
        else {
            return Ok(true);
        };
        self.cleanup_generation_once(source_key, generation, cleanup_performed)
    }

    fn cleanup_generation_once(
        &mut self,
        source_key: &str,
        generation: u64,
        cleanup_performed: &mut bool,
    ) -> Result<bool, GeminiScannerError> {
        if *cleanup_performed {
            return Ok(false);
        }
        *cleanup_performed = true;
        Ok(self
            .state
            .cleanup_generation_batch(
                source_key,
                generation,
                MAX_SESSION_BATCH_ROWS,
                MAX_SESSION_BATCH_BYTES,
            )?
            .complete)
    }

    fn persisted_sources(&self) -> Result<HashSet<String>, GeminiScannerError> {
        let mut output = HashSet::new();
        let mut page_key: Option<SessionSourcePageKey> = None;
        loop {
            let page = self
                .state
                .load_session_sources_page(page_key.as_ref(), MAX_SESSION_BATCH_ROWS)?;
            for source in page.items {
                if source.source_kind == SessionSourceKind::Gemini {
                    output.insert(source.source_key);
                }
            }
            page_key = page.next_page_key;
            if page_key.is_none() {
                return Ok(output);
            }
        }
    }

    fn persisted_identity_sources(&self) -> Result<HashMap<String, String>, GeminiScannerError> {
        let mut output = HashMap::new();
        let mut page_key: Option<SessionSourcePageKey> = None;
        loop {
            let page = self
                .state
                .load_session_sources_page(page_key.as_ref(), MAX_SESSION_BATCH_ROWS)?;
            for source in page.items {
                if source.source_kind != SessionSourceKind::Gemini {
                    continue;
                }
                let current = self
                    .state
                    .load_current_session_scan_cursor(&source.source_key)?;
                let staging = self
                    .state
                    .load_staging_session_scan_cursor(&source.source_key)?;
                for cursor in current.into_iter().chain(staging) {
                    if output
                        .insert(
                            cursor.file_identity.as_str().to_owned(),
                            source.source_key.clone(),
                        )
                        .is_some_and(|existing| existing != source.source_key)
                    {
                        return Err(StorageError::StableRecordConflict {
                            record_kind: "Session file identity",
                        }
                        .into());
                    }
                }
            }
            page_key = page.next_page_key;
            if page_key.is_none() {
                return Ok(output);
            }
        }
    }

    fn resolve_source_keys(
        &self,
        discovered: &[DiscoveredGeminiSession],
        persisted: &HashSet<String>,
        persisted_identities: &HashMap<String, String>,
    ) -> Result<Vec<String>, GeminiScannerError> {
        let identities = discovered
            .iter()
            .map(|source| self.file_identity(source))
            .collect::<Vec<_>>();
        let mut output = vec![None; discovered.len()];
        let mut reserved = HashMap::<String, SessionFileIdentity>::new();
        for (index, identity) in identities.iter().enumerate() {
            let Some(source_key) = persisted_identities.get(identity.as_str()) else {
                continue;
            };
            if reserved
                .insert(source_key.clone(), identity.clone())
                .is_some()
            {
                return Err(StorageError::StableRecordConflict {
                    record_kind: "Session source key",
                }
                .into());
            }
            output[index] = Some(source_key.clone());
        }
        for (index, source) in discovered.iter().enumerate() {
            if output[index].is_some() {
                continue;
            }
            let identity = &identities[index];
            let path_key = self.source_key(source);
            let source_key = if !reserved.contains_key(&path_key) {
                path_key
            } else {
                let mut counter = 0u64;
                loop {
                    let candidate = opaque_hex(
                        &self.domain_key,
                        b"wokcore.gemini.source-collision.v1",
                        &[
                            &path_bytes(&canonical_relative_path(&source.relative_path)),
                            identity.as_str().as_bytes(),
                            &counter.to_be_bytes(),
                        ],
                    );
                    if !reserved.contains_key(&candidate) && !persisted.contains(&candidate) {
                        break candidate;
                    }
                    counter = counter.checked_add(1).ok_or(GeminiScannerError::Parse)?;
                }
            };
            reserved.insert(source_key.clone(), identity.clone());
            output[index] = Some(source_key);
        }
        Ok(output
            .into_iter()
            .map(|source_key| source_key.expect("every discovered source receives a source key"))
            .collect())
    }

    pub(crate) fn source_key(&self, source: &DiscoveredGeminiSession) -> String {
        let canonical = canonical_relative_path(&source.relative_path);
        opaque_hex(
            &self.domain_key,
            SOURCE_KEY_DOMAIN,
            &[&path_bytes(&canonical)],
        )
    }

    pub(crate) fn file_identity(&self, source: &DiscoveredGeminiSession) -> SessionFileIdentity {
        SessionFileIdentity::new(opaque_platform_identity(
            &self.domain_key,
            FILE_IDENTITY_DOMAIN,
            source.identity,
        ))
        .expect("opaque platform identity is a valid storage key")
    }

    fn session_key(&self, session_id: &str) -> String {
        opaque_hex(
            &self.domain_key,
            SESSION_KEY_DOMAIN,
            &[session_id.as_bytes()],
        )
    }

    pub fn title_for_source(
        &mut self,
        source_key: &str,
    ) -> Result<Option<ExternalSessionTitle>, GeminiScannerError> {
        let Some(state) = self.state.load_session_source(source_key)? else {
            return Ok(None);
        };
        if state.source_kind != SessionSourceKind::Gemini {
            return Ok(None);
        }
        let Some(cursor) = self.state.load_current_session_scan_cursor(source_key)? else {
            return Ok(None);
        };
        let (mut file, format) = match open_source_for_paging(
            &self.root,
            &self.domain_key,
            &cursor,
            MAX_TITLE_SOURCE_WORK_BYTES,
        ) {
            Ok(opened) => opened,
            Err(GeminiScannerError::Storage(error)) => {
                return Err(GeminiScannerError::Storage(error));
            }
            Err(_) => return Ok(None),
        };
        let title = match format {
            GeminiFormat::CurrentJsonl => read_current_title(&mut file, &self.domain_key),
            GeminiFormat::LegacyJson => read_legacy_title(&mut file),
        };
        match title {
            Ok(title) => Ok(title),
            Err(GeminiScannerError::Storage(error)) => Err(GeminiScannerError::Storage(error)),
            Err(_) => Ok(None),
        }
    }
}

fn validate_usage_storage_bounds(aggregate: &GeminiAggregate) -> Result<(), GeminiScannerError> {
    let maximum = i64::MAX as u64;
    if aggregate.usage().any(|(_, usage)| {
        usage.input > maximum
            || usage.output > maximum
            || usage.cached > maximum
            || usage.thoughts > maximum
    }) {
        Err(GeminiScannerError::ResourceLimit)
    } else {
        Ok(())
    }
}

fn next_generation(current: Option<u64>) -> Result<u64, GeminiScannerError> {
    current.map_or(Ok(1), |generation| {
        if generation >= i64::MAX as u64 {
            Err(GeminiScannerError::ResourceLimit)
        } else {
            Ok(generation + 1)
        }
    })
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ProcessOutcome {
    Advanced,
    Unchanged,
    Interrupted,
}

struct ProcessResult {
    process: ProcessOutcome,
    summary: SessionSourceScanSummary,
}

fn parser_checkpoint(emitted_usage: u64, structural_hash: Option<[u8; 32]>) -> ParserCheckpoint {
    ParserCheckpoint {
        version: PARSER_CHECKPOINT_VERSION,
        previous_input_tokens: 0,
        previous_output_tokens: 0,
        previous_cache_read_tokens: 0,
        previous_cache_write_tokens: 0,
        previous_reasoning_tokens: 0,
        current_model: None,
        event_ordinal: emitted_usage,
        lineage_source_key: None,
        lineage_generation: None,
        lineage_record_ordinal: 0,
        structural_hash,
    }
}

pub(crate) fn discover_gemini_sessions(
    root: &SessionRootLease,
    limits: DiscoveryLimits,
) -> Result<Vec<DiscoveredGeminiSession>, GeminiScannerError> {
    let mut output = Vec::new();
    let mut entry_budget = limits.maximum_total_entries;
    let Some(tmp) = optional_directory(root, Path::new("tmp"))? else {
        return Ok(output);
    };
    for project in directory_entries(&tmp, limits, &mut entry_budget)? {
        if project.snapshot().kind != SessionFileKind::Directory {
            continue;
        }
        let chats_path = Path::new("tmp").join(project.name()).join("chats");
        let Some(chats) = optional_directory(root, &chats_path)? else {
            continue;
        };
        collect_gemini_files(
            &chats,
            &chats_path,
            true,
            limits,
            &mut entry_budget,
            &mut output,
        )?;
        for parent in directory_entries(&chats, limits, &mut entry_budget)? {
            if parent.snapshot().kind != SessionFileKind::Directory {
                continue;
            }
            let parent_path = chats_path.join(parent.name());
            let Some(parent_directory) = optional_directory(root, &parent_path)? else {
                continue;
            };
            collect_gemini_files(
                &parent_directory,
                &parent_path,
                false,
                limits,
                &mut entry_budget,
                &mut output,
            )?;
        }
    }
    let mut canonical = HashMap::<PathBuf, DiscoveredGeminiSession>::new();
    for source in output {
        let key = canonical_relative_path(&source.relative_path);
        match canonical.get(&key) {
            Some(existing)
                if existing.format == GeminiFormat::CurrentJsonl
                    || source.format == GeminiFormat::LegacyJson => {}
            _ => {
                canonical.insert(key, source);
            }
        }
    }
    let mut output = canonical.into_values().collect::<Vec<_>>();
    output.sort_unstable_by(|left, right| {
        let left_priority = usize::from(left.format == GeminiFormat::LegacyJson);
        let right_priority = usize::from(right.format == GeminiFormat::LegacyJson);
        left_priority
            .cmp(&right_priority)
            .then_with(|| left.relative_path.cmp(&right.relative_path))
    });
    let mut identities = HashSet::new();
    output.retain(|source| identities.insert(source.identity));
    if output.len() > limits.maximum_total_sessions {
        return Err(GeminiScannerError::ResourceLimit);
    }
    output.sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(output)
}

pub(crate) fn open_source_for_paging(
    root: &SessionRootLease,
    domain_key: &[u8; 32],
    cursor: &SessionScanCursor,
    maximum_size: u64,
) -> Result<(SessionFile, GeminiFormat), GeminiScannerError> {
    let discovered = discover_gemini_sessions(root, DiscoveryLimits::default())?;
    let mut target = None;
    for source in &discovered {
        let identity = SessionFileIdentity::new(opaque_platform_identity(
            domain_key,
            FILE_IDENTITY_DOMAIN,
            source.identity,
        ))
        .expect("opaque platform identity is a valid storage key");
        if identity != cursor.file_identity {
            continue;
        }
        if target.replace(source).is_some() {
            return Err(StorageError::StableRecordConflict {
                record_kind: "Session paging source",
            }
            .into());
        }
    }
    let target = target.ok_or(GeminiScannerError::Read)?;
    let mut file = target
        .open_bounded(root, maximum_size)
        .map_err(|error| match error {
            SessionError::ReadLimitExceeded => GeminiScannerError::ResourceLimit,
            _ => GeminiScannerError::Read,
        })?;
    let snapshot = file.snapshot();
    if snapshot.size != cursor.observed_size
        || system_time_utc(snapshot.modified) != cursor.modified_at
        || cursor.complete_byte_offset > snapshot.size
    {
        return Err(GeminiScannerError::Parse);
    }
    let (head, boundary) = fingerprints(
        &mut file,
        cursor.complete_byte_offset,
        domain_key,
        FINGERPRINT_DOMAIN,
    )
    .map_err(map_jsonl_error)?;
    if head != cursor.head_fingerprint || boundary != cursor.boundary_fingerprint {
        return Err(GeminiScannerError::Parse);
    }
    Ok((file, target.format))
}

fn optional_directory(
    root: &SessionRootLease,
    relative: &Path,
) -> Result<Option<SessionDirectoryLease>, GeminiScannerError> {
    match root.open_directory(relative) {
        Ok(directory) => Ok(Some(directory)),
        Err(SessionError::Io { source }) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(GeminiScannerError::Discovery),
    }
}

fn directory_entries(
    directory: &SessionDirectoryLease,
    limits: DiscoveryLimits,
    entry_budget: &mut usize,
) -> Result<Vec<wokcore_platform::sessions::SessionDirectoryEntry>, GeminiScannerError> {
    let entries = directory
        .entries(limits.maximum_entries_per_directory)
        .map_err(|_| GeminiScannerError::Discovery)?;
    *entry_budget = entry_budget
        .checked_sub(entries.len())
        .ok_or(GeminiScannerError::ResourceLimit)?;
    Ok(entries)
}

fn collect_gemini_files(
    directory: &SessionDirectoryLease,
    base: &Path,
    allow_legacy: bool,
    limits: DiscoveryLimits,
    entry_budget: &mut usize,
    output: &mut Vec<DiscoveredGeminiSession>,
) -> Result<(), GeminiScannerError> {
    for entry in directory_entries(directory, limits, entry_budget)? {
        if entry.snapshot().kind != SessionFileKind::RegularFile {
            continue;
        }
        let path = Path::new(entry.name());
        let format = match path.extension().and_then(OsStr::to_str) {
            Some(extension)
                if extension.eq_ignore_ascii_case("jsonl")
                    && (!allow_legacy
                        || path
                            .file_stem()
                            .and_then(OsStr::to_str)
                            .is_some_and(|stem| stem.starts_with("session-"))) =>
            {
                GeminiFormat::CurrentJsonl
            }
            Some(extension)
                if allow_legacy
                    && extension.eq_ignore_ascii_case("json")
                    && path
                        .file_stem()
                        .and_then(OsStr::to_str)
                        .is_some_and(|stem| stem.starts_with("session-")) =>
            {
                GeminiFormat::LegacyJson
            }
            _ => continue,
        };
        output.push(DiscoveredGeminiSession {
            relative_path: base.join(entry.name()),
            identity: entry.snapshot().identity,
            format,
        });
    }
    Ok(())
}

fn canonical_relative_path(path: &Path) -> PathBuf {
    let mut canonical = path.to_path_buf();
    if path.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("json") || extension.eq_ignore_ascii_case("jsonl")
    }) {
        canonical.set_extension("jsonl");
    }
    canonical
}

fn parse_current_jsonl(
    file: &mut SessionFile,
    domain_key: &[u8; 32],
) -> Result<GeminiAggregate, GeminiScannerError> {
    let mut reader = JsonlReader::new(JsonlCursor::new(0, 1)).with_raw_fingerprint(
        structural_fingerprint(domain_key, GeminiFormat::CurrentJsonl),
    );
    let mut messages = Vec::new();
    let mut positions = HashMap::<[u8; 32], usize>::new();
    let mut session_id = None;
    let mut created_at = None;
    let mut last_active_at = None;
    let mut parser_read_bytes = 0u64;
    let mut peak_parser_buffer_bytes = 0usize;
    let mut peak_logical_working_bytes = 0usize;
    let mut logical_dynamic_bytes = 0usize;
    let mut maximum_record_revision = 0u64;
    let mut metadata_validated = false;
    let (complete_byte_offset, _physical_record_ordinal) = loop {
        let scan = reader.scan(file).map_err(map_jsonl_error)?;
        parser_read_bytes = parser_read_bytes.saturating_add(scan.read_bytes);
        peak_parser_buffer_bytes = peak_parser_buffer_bytes.max(scan.peak_buffer_bytes);
        for record in &scan.records {
            if !metadata_validated {
                if record.status != JsonlRecordStatus::Valid {
                    return Err(GeminiScannerError::Parse);
                }
                let object = record
                    .value()
                    .as_object()
                    .ok_or(GeminiScannerError::Parse)?;
                validate_current_metadata_header(object)?;
                metadata_validated = true;
            }
            maximum_record_revision =
                maximum_record_revision.max(current_record_revision(record.ordinal, 0)?);
            if record.status != JsonlRecordStatus::Valid {
                continue;
            }
            let Some(object) = record.value().as_object() else {
                continue;
            };
            if let Some(rewind) = object.get("$rewindTo").and_then(Value::as_str) {
                let target = opaque_hash(domain_key, LOGICAL_MESSAGE_DOMAIN, &[rewind.as_bytes()]);
                let keep = positions.get(&target).copied().unwrap_or(0);
                truncate_gemini_messages(
                    &mut messages,
                    &mut positions,
                    keep,
                    &mut logical_dynamic_bytes,
                    domain_key,
                );
                update_current_working_peak(
                    &messages,
                    &positions,
                    logical_dynamic_bytes,
                    &session_id,
                    &created_at,
                    &last_active_at,
                    &mut peak_logical_working_bytes,
                )?;
                continue;
            }
            if let Some(set) = object.get("$set").and_then(Value::as_object) {
                if let Some(messages_value) = set.get("messages") {
                    let checkpoint = messages_value.as_array().ok_or(GeminiScannerError::Parse)?;
                    messages.clear();
                    positions.clear();
                    logical_dynamic_bytes = 0;
                    for (index, value) in checkpoint.iter().enumerate() {
                        let revision =
                            current_record_revision(record.ordinal, index.saturating_add(1))?;
                        maximum_record_revision = maximum_record_revision.max(revision);
                        if let Some(message) = parse_gemini_message(value, revision) {
                            insert_gemini_message(
                                message,
                                &mut messages,
                                &mut positions,
                                &mut logical_dynamic_bytes,
                                domain_key,
                            )?;
                        }
                    }
                }
                merge_gemini_metadata(set, &mut session_id, &mut created_at, &mut last_active_at)?;
                update_current_working_peak(
                    &messages,
                    &positions,
                    logical_dynamic_bytes,
                    &session_id,
                    &created_at,
                    &last_active_at,
                    &mut peak_logical_working_bytes,
                )?;
                continue;
            }
            if object.contains_key("sessionId") && !object.contains_key("id") {
                merge_gemini_metadata(
                    object,
                    &mut session_id,
                    &mut created_at,
                    &mut last_active_at,
                )?;
                if let Some(messages_value) = object.get("messages") {
                    let checkpoint = messages_value.as_array().ok_or(GeminiScannerError::Parse)?;
                    messages.clear();
                    positions.clear();
                    logical_dynamic_bytes = 0;
                    for (index, value) in checkpoint.iter().enumerate() {
                        let revision =
                            current_record_revision(record.ordinal, index.saturating_add(1))?;
                        maximum_record_revision = maximum_record_revision.max(revision);
                        if let Some(message) = parse_gemini_message(value, revision) {
                            insert_gemini_message(
                                message,
                                &mut messages,
                                &mut positions,
                                &mut logical_dynamic_bytes,
                                domain_key,
                            )?;
                        }
                    }
                }
                update_current_working_peak(
                    &messages,
                    &positions,
                    logical_dynamic_bytes,
                    &session_id,
                    &created_at,
                    &last_active_at,
                    &mut peak_logical_working_bytes,
                )?;
                continue;
            }
            let revision = current_record_revision(record.ordinal, 0)?;
            if let Some(message) = parse_gemini_message(record.value(), revision) {
                last_active_at = maximum_timestamp(last_active_at, &message.occurred_at);
                insert_gemini_message(
                    message,
                    &mut messages,
                    &mut positions,
                    &mut logical_dynamic_bytes,
                    domain_key,
                )?;
                update_current_working_peak(
                    &messages,
                    &positions,
                    logical_dynamic_bytes,
                    &session_id,
                    &created_at,
                    &last_active_at,
                    &mut peak_logical_working_bytes,
                )?;
            }
        }
        if scan.reached_end || scan.records.is_empty() {
            break (
                scan.complete_byte_offset,
                scan.next_record_ordinal.saturating_sub(1),
            );
        }
    };
    if !metadata_validated {
        return Err(GeminiScannerError::Parse);
    }
    let structural_hash = reader
        .finish_raw_fingerprint(complete_byte_offset)
        .ok_or(GeminiScannerError::Parse)?;
    finalize_aggregate(
        session_id,
        created_at,
        last_active_at,
        messages,
        complete_byte_offset,
        maximum_record_revision,
        parser_read_bytes,
        peak_parser_buffer_bytes,
        peak_logical_working_bytes,
        structural_hash,
    )
}

fn validate_current_metadata_header(
    object: &serde_json::Map<String, Value>,
) -> Result<(), GeminiScannerError> {
    if object.contains_key("id")
        || object
            .get("sessionId")
            .and_then(Value::as_str)
            .and_then(normalize_external_id)
            .is_none()
        || object
            .get("projectHash")
            .and_then(Value::as_str)
            .and_then(normalize_external_id)
            .is_none()
    {
        return Err(GeminiScannerError::Parse);
    }
    Ok(())
}

fn current_record_revision(
    record_ordinal: u64,
    embedded_index: usize,
) -> Result<u64, GeminiScannerError> {
    const EMBEDDED_REVISION_BITS: u32 = 16;
    let embedded = u64::try_from(embedded_index).map_err(|_| GeminiScannerError::ResourceLimit)?;
    if embedded >= (1u64 << EMBEDDED_REVISION_BITS) {
        return Err(GeminiScannerError::ResourceLimit);
    }
    record_ordinal
        .checked_shl(EMBEDDED_REVISION_BITS)
        .and_then(|base| base.checked_add(embedded))
        .ok_or(GeminiScannerError::ResourceLimit)
}

fn update_current_working_peak(
    messages: &Vec<GeminiLogicalMessage>,
    positions: &HashMap<[u8; 32], usize>,
    logical_dynamic_bytes: usize,
    session_id: &Option<String>,
    created_at: &Option<String>,
    last_active_at: &Option<String>,
    peak: &mut usize,
) -> Result<(), GeminiScannerError> {
    let current = current_logical_working_bytes(
        messages,
        positions.capacity(),
        logical_dynamic_bytes,
        session_id,
        created_at,
        last_active_at,
    );
    *peak = (*peak).max(current);
    if current > MAX_GEMINI_LOGICAL_WORKING_BYTES {
        Err(GeminiScannerError::ResourceLimit)
    } else {
        Ok(())
    }
}

fn merge_gemini_metadata(
    object: &serde_json::Map<String, Value>,
    session_id: &mut Option<String>,
    created_at: &mut Option<String>,
    last_active_at: &mut Option<String>,
) -> Result<(), GeminiScannerError> {
    if let Some(found) = object
        .get("sessionId")
        .and_then(Value::as_str)
        .and_then(normalize_external_id)
    {
        match session_id {
            Some(existing) if existing != &found => return Err(GeminiScannerError::Parse),
            None => *session_id = Some(found),
            _ => {}
        }
    }
    if let Some(timestamp) = object.get("startTime").and_then(normalize_timestamp) {
        created_at.get_or_insert(timestamp);
    }
    if let Some(timestamp) = object.get("lastUpdated").and_then(normalize_timestamp) {
        *last_active_at = maximum_timestamp(last_active_at.take(), &timestamp);
    }
    Ok(())
}

fn parse_gemini_message(value: &Value, revision: u64) -> Option<GeminiLogicalMessage> {
    let object = value.as_object()?;
    let message_id = object
        .get("id")
        .and_then(Value::as_str)
        .and_then(normalize_external_id)?;
    let occurred_at = object.get("timestamp").and_then(normalize_timestamp)?;
    let message_type = object.get("type")?.as_str()?;
    let (usage, usage_snapshot_valid) = if message_type == "gemini" {
        match object.get("tokens") {
            Some(Value::Object(tokens)) => {
                let input = optional_number(tokens, "input")?;
                let output = optional_number(tokens, "output")?;
                let cached = optional_number(tokens, "cached")?;
                let thoughts = optional_number(tokens, "thoughts")?;
                let usage = Some(GeminiUsage {
                    model: normalize_external_model(object.get("model").and_then(Value::as_str)),
                    occurred_at: occurred_at.clone(),
                    input,
                    output,
                    cached,
                    thoughts,
                    revision,
                })
                .filter(|usage| {
                    usage.input != 0
                        || usage.output != 0
                        || usage.cached != 0
                        || usage.thoughts != 0
                        || optional_number(tokens, "tool").unwrap_or(0) != 0
                });
                (usage, true)
            }
            Some(_) | None => (None, false),
        }
    } else if matches!(message_type, "user" | "info" | "error" | "warning") {
        (None, true)
    } else {
        return None;
    };
    Some(GeminiLogicalMessage {
        message_id,
        occurred_at,
        usage,
        usage_snapshot_valid,
    })
}

fn optional_number(object: &serde_json::Map<String, Value>, name: &str) -> Option<u64> {
    object.get(name).map_or(Some(0), Value::as_u64)
}

fn insert_gemini_message(
    message: GeminiLogicalMessage,
    messages: &mut Vec<GeminiLogicalMessage>,
    positions: &mut HashMap<[u8; 32], usize>,
    logical_dynamic_bytes: &mut usize,
    domain_key: &[u8; 32],
) -> Result<(), GeminiScannerError> {
    let key = opaque_hash(
        domain_key,
        LOGICAL_MESSAGE_DOMAIN,
        &[message.message_id.as_bytes()],
    );
    if let Some(&position) = positions.get(&key) {
        let mut message = message;
        if !message.usage_snapshot_valid {
            message.usage = messages[position].usage.clone();
        }
        *logical_dynamic_bytes = logical_dynamic_bytes
            .saturating_sub(gemini_message_dynamic_bytes(&messages[position]))
            .saturating_add(gemini_message_dynamic_bytes(&message));
        messages[position] = message;
    } else {
        if messages.len() >= MAX_ACTIVE_MESSAGES {
            return Err(GeminiScannerError::ResourceLimit);
        }
        positions.insert(key, messages.len());
        *logical_dynamic_bytes =
            logical_dynamic_bytes.saturating_add(gemini_message_dynamic_bytes(&message));
        messages.push(message);
        if logical_working_bytes(messages, positions.capacity(), *logical_dynamic_bytes)
            > MAX_GEMINI_LOGICAL_WORKING_BYTES
        {
            return Err(GeminiScannerError::ResourceLimit);
        }
    }
    Ok(())
}

fn gemini_message_dynamic_bytes(message: &GeminiLogicalMessage) -> usize {
    message
        .message_id
        .capacity()
        .saturating_add(message.occurred_at.capacity())
        .saturating_add(message.usage.as_ref().map_or(0, |usage| {
            usage
                .model
                .capacity()
                .saturating_add(usage.occurred_at.capacity())
        }))
}

fn logical_dynamic_bytes(messages: &[GeminiLogicalMessage]) -> usize {
    messages.iter().fold(0usize, |total, message| {
        total.saturating_add(gemini_message_dynamic_bytes(message))
    })
}

fn logical_working_bytes(
    messages: &Vec<GeminiLogicalMessage>,
    position_capacity: usize,
    logical_dynamic_bytes: usize,
) -> usize {
    const HASH_MAP_BUCKET_CONTROL_OVERHEAD: usize = 16;
    messages
        .capacity()
        .saturating_mul(std::mem::size_of::<GeminiLogicalMessage>())
        .saturating_add(
            position_capacity.saturating_mul(
                std::mem::size_of::<([u8; 32], usize)>()
                    .saturating_add(HASH_MAP_BUCKET_CONTROL_OVERHEAD),
            ),
        )
        .saturating_add(logical_dynamic_bytes)
}

fn current_logical_working_bytes(
    messages: &Vec<GeminiLogicalMessage>,
    position_capacity: usize,
    logical_dynamic_bytes: usize,
    session_id: &Option<String>,
    created_at: &Option<String>,
    last_active_at: &Option<String>,
) -> usize {
    logical_working_bytes(messages, position_capacity, logical_dynamic_bytes)
        .saturating_add(session_id.as_ref().map_or(0, String::capacity))
        .saturating_add(created_at.as_ref().map_or(0, String::capacity))
        .saturating_add(last_active_at.as_ref().map_or(0, String::capacity))
}

fn truncate_gemini_messages(
    messages: &mut Vec<GeminiLogicalMessage>,
    positions: &mut HashMap<[u8; 32], usize>,
    keep: usize,
    logical_dynamic_bytes: &mut usize,
    domain_key: &[u8; 32],
) {
    for message in messages.drain(keep..) {
        *logical_dynamic_bytes =
            logical_dynamic_bytes.saturating_sub(gemini_message_dynamic_bytes(&message));
        positions.remove(&opaque_hash(
            domain_key,
            LOGICAL_MESSAGE_DOMAIN,
            &[message.message_id.as_bytes()],
        ));
    }
}

struct LegacyMessageMetadata {
    id: String,
    timestamp: String,
    message_type: String,
    model: Option<String>,
    tokens: Option<LegacyTokens>,
}

#[derive(Default)]
struct LegacyTokens {
    input: u64,
    output: u64,
    cached: u64,
    thoughts: u64,
    tool: u64,
}

struct LegacyAccumulator {
    session_id: Option<String>,
    created_at: Option<String>,
    last_active_at: Option<String>,
    messages: Vec<GeminiLogicalMessage>,
    peak_logical_working_bytes: usize,
}

struct BoundedStringSeed<'a, const MAXIMUM: usize> {
    resource_limit: &'a Cell<bool>,
}

struct BoundedStringVisitor<'a, const MAXIMUM: usize> {
    resource_limit: &'a Cell<bool>,
}

impl<'de, const MAXIMUM: usize> DeserializeSeed<'de> for BoundedStringSeed<'_, MAXIMUM> {
    type Value = String;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_string(BoundedStringVisitor::<MAXIMUM> {
            resource_limit: self.resource_limit,
        })
    }
}

impl<const MAXIMUM: usize> Visitor<'_> for BoundedStringVisitor<'_, MAXIMUM> {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "a UTF-8 string no longer than {MAXIMUM} bytes")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.accept(value)
    }

    fn visit_borrowed_str<E>(self, value: &'_ str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.accept(value)
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if value.len() > MAXIMUM {
            self.resource_limit.set(true);
            Err(E::custom("Gemini legacy string exceeds its bound"))
        } else {
            Ok(value)
        }
    }
}

impl<const MAXIMUM: usize> BoundedStringVisitor<'_, MAXIMUM> {
    fn accept<E>(self, value: &str) -> Result<String, E>
    where
        E: serde::de::Error,
    {
        if value.len() > MAXIMUM {
            self.resource_limit.set(true);
            Err(E::custom("Gemini legacy string exceeds its bound"))
        } else {
            Ok(value.to_owned())
        }
    }
}

struct LegacyDocumentSeed<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for LegacyDocumentSeed<'_> {
    type Value = LegacyAccumulator;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(LegacyDocumentVisitor {
            resource_limit: self.resource_limit,
        })
    }
}

struct LegacyDocumentVisitor<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> Visitor<'de> for LegacyDocumentVisitor<'_> {
    type Value = LegacyAccumulator;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a Gemini legacy Session object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut accumulator = LegacyAccumulator {
            session_id: None,
            created_at: None,
            last_active_at: None,
            messages: Vec::new(),
            peak_logical_working_bytes: 0,
        };
        let mut messages_value_valid = true;
        while let Some(key) = map.next_key_seed(BoundedStringSeed::<MAX_LEGACY_MAP_KEY_BYTES> {
            resource_limit: self.resource_limit,
        })? {
            match key.as_str() {
                "sessionId" => {
                    accumulator.session_id = Some(map.next_value_seed(BoundedStringSeed::<
                        { crate::model::EXTERNAL_ID_LIMIT_BYTES },
                    > {
                        resource_limit: self.resource_limit,
                    })?)
                }
                "startTime" => {
                    let value =
                        map.next_value_seed(BoundedStringSeed::<MAX_LEGACY_TIMESTAMP_BYTES> {
                            resource_limit: self.resource_limit,
                        })?;
                    accumulator.created_at = normalize_timestamp(&Value::String(value));
                }
                "lastUpdated" => {
                    let value =
                        map.next_value_seed(BoundedStringSeed::<MAX_LEGACY_TIMESTAMP_BYTES> {
                            resource_limit: self.resource_limit,
                        })?;
                    accumulator.last_active_at = normalize_timestamp(&Value::String(value));
                }
                "messages" => {
                    let result = map.next_value_seed(LegacyMessagesValueSeed {
                        resource_limit: self.resource_limit,
                    })?;
                    if let Some(result) = result {
                        accumulator.messages = result.messages;
                        accumulator.peak_logical_working_bytes = result.peak_logical_working_bytes;
                        messages_value_valid = true;
                    } else {
                        messages_value_valid = false;
                    }
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        if !messages_value_valid {
            return Err(serde::de::Error::custom(
                "the final Gemini legacy messages value is not an array",
            ));
        }
        Ok(accumulator)
    }
}

struct LegacyMessagesValueSeed<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for LegacyMessagesValueSeed<'_> {
    type Value = Option<LegacyMessagesResult>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(LegacyMessagesValueVisitor {
            resource_limit: self.resource_limit,
        })
    }
}

struct LegacyMessagesValueVisitor<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> Visitor<'de> for LegacyMessagesValueVisitor<'_> {
    type Value = Option<LegacyMessagesResult>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the effective Gemini legacy messages value")
    }

    fn visit_seq<A>(self, sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        LegacyMessagesVisitor {
            resource_limit: self.resource_limit,
        }
        .visit_seq(sequence)
        .map(Some)
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(None)
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_borrowed_str<E>(self, _value: &'de str) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_string<E>(self, _value: String) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(None)
    }
}

struct LegacyMessagesResult {
    messages: Vec<GeminiLogicalMessage>,
    peak_logical_working_bytes: usize,
}

struct LegacyMessagesVisitor<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> Visitor<'de> for LegacyMessagesVisitor<'_> {
    type Value = LegacyMessagesResult;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded Gemini legacy message sequence")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut messages = Vec::new();
        let mut peak_logical_working_bytes = 0usize;
        let mut logical_dynamic_bytes = 0usize;
        while let Some(message) = sequence.next_element_seed(LegacyMessageSeed {
            resource_limit: self.resource_limit,
        })? {
            if messages.len() >= MAX_ACTIVE_MESSAGES {
                self.resource_limit.set(true);
                return Err(serde::de::Error::custom(
                    "Gemini message count exceeds its bound",
                ));
            }
            let Some(message_id) = normalize_external_id(&message.id) else {
                continue;
            };
            let Some(occurred_at) = normalize_timestamp(&Value::String(message.timestamp)) else {
                continue;
            };
            let revision = messages.len() as u64 + 1;
            let usage = if message.message_type == "gemini" {
                message.tokens.and_then(|tokens| {
                    Some(GeminiUsage {
                        model: normalize_external_model(message.model.as_deref()),
                        occurred_at: occurred_at.clone(),
                        input: tokens.input,
                        output: tokens.output,
                        cached: tokens.cached,
                        thoughts: tokens.thoughts,
                        revision,
                    })
                    .filter(|usage| {
                        usage.input != 0
                            || usage.output != 0
                            || usage.cached != 0
                            || usage.thoughts != 0
                            || tokens.tool != 0
                    })
                })
            } else {
                None
            };
            let message = GeminiLogicalMessage {
                message_id,
                occurred_at,
                usage,
                usage_snapshot_valid: true,
            };
            logical_dynamic_bytes =
                logical_dynamic_bytes.saturating_add(gemini_message_dynamic_bytes(&message));
            messages.push(message);
            let current = logical_working_bytes(&messages, 0, logical_dynamic_bytes);
            peak_logical_working_bytes = peak_logical_working_bytes.max(current);
            if current > MAX_GEMINI_LOGICAL_WORKING_BYTES {
                self.resource_limit.set(true);
                return Err(serde::de::Error::custom(
                    "Gemini logical Session working set exceeds its bound",
                ));
            }
        }
        Ok(LegacyMessagesResult {
            messages,
            peak_logical_working_bytes,
        })
    }
}

struct LegacyMessageSeed<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for LegacyMessageSeed<'_> {
    type Value = LegacyMessageMetadata;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(LegacyMessageVisitor {
            resource_limit: self.resource_limit,
        })
    }
}

struct LegacyMessageVisitor<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> Visitor<'de> for LegacyMessageVisitor<'_> {
    type Value = LegacyMessageMetadata;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded Gemini legacy message object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut id = None;
        let mut timestamp = None;
        let mut message_type = None;
        let mut model = None;
        let mut tokens = None;
        while let Some(key) = map.next_key_seed(BoundedStringSeed::<MAX_LEGACY_MAP_KEY_BYTES> {
            resource_limit: self.resource_limit,
        })? {
            match key.as_str() {
                "id" => {
                    id = Some(map.next_value_seed(BoundedStringSeed::<
                        { crate::model::EXTERNAL_ID_LIMIT_BYTES },
                    > {
                        resource_limit: self.resource_limit,
                    })?)
                }
                "timestamp" => {
                    timestamp = Some(map.next_value_seed(BoundedStringSeed::<
                        MAX_LEGACY_TIMESTAMP_BYTES,
                    > {
                        resource_limit: self.resource_limit,
                    })?)
                }
                "type" => {
                    message_type = Some(map.next_value_seed(BoundedStringSeed::<
                        MAX_LEGACY_MESSAGE_TYPE_BYTES,
                    > {
                        resource_limit: self.resource_limit,
                    })?)
                }
                "model" => {
                    model = Some(map.next_value_seed(BoundedStringSeed::<
                        { crate::model::MODEL_LIMIT_BYTES },
                    > {
                        resource_limit: self.resource_limit,
                    })?)
                }
                "tokens" => {
                    tokens = Some(map.next_value_seed(LegacyTokensSeed {
                        resource_limit: self.resource_limit,
                    })?)
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(LegacyMessageMetadata {
            id: id.ok_or_else(|| serde::de::Error::missing_field("id"))?,
            timestamp: timestamp.ok_or_else(|| serde::de::Error::missing_field("timestamp"))?,
            message_type: message_type.ok_or_else(|| serde::de::Error::missing_field("type"))?,
            model,
            tokens,
        })
    }
}

struct LegacyTokensSeed<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for LegacyTokensSeed<'_> {
    type Value = LegacyTokens;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(LegacyTokensVisitor {
            resource_limit: self.resource_limit,
        })
    }
}

struct LegacyTokensVisitor<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> Visitor<'de> for LegacyTokensVisitor<'_> {
    type Value = LegacyTokens;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded Gemini token object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut output = LegacyTokens::default();
        while let Some(key) = map.next_key_seed(BoundedStringSeed::<MAX_LEGACY_MAP_KEY_BYTES> {
            resource_limit: self.resource_limit,
        })? {
            match key.as_str() {
                "input" => output.input = map.next_value()?,
                "output" => output.output = map.next_value()?,
                "cached" => output.cached = map.next_value()?,
                "thoughts" => output.thoughts = map.next_value()?,
                "tool" => output.tool = map.next_value()?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(output)
    }
}

struct PinnedSessionReader<'a> {
    file: &'a mut SessionFile,
    offset: u64,
    read_bytes: u64,
    peak_chunk_bytes: usize,
    work_remaining: u64,
    resource_limit: bool,
    in_string: bool,
    string_escape: bool,
    string_bytes: usize,
    buffer: Vec<u8>,
    buffer_position: usize,
    raw_fingerprint: Option<OpaqueStreamHash>,
}

impl<'a> PinnedSessionReader<'a> {
    fn new(file: &'a mut SessionFile) -> Self {
        Self {
            file,
            offset: 0,
            read_bytes: 0,
            peak_chunk_bytes: 0,
            work_remaining: MAX_LEGACY_JSON_SOURCE_WORK_BYTES,
            resource_limit: false,
            in_string: false,
            string_escape: false,
            string_bytes: 0,
            buffer: Vec::new(),
            buffer_position: 0,
            raw_fingerprint: None,
        }
    }

    fn with_raw_fingerprint(mut self, fingerprint: OpaqueStreamHash) -> Self {
        self.raw_fingerprint = Some(fingerprint);
        self
    }

    fn finish_raw_fingerprint(&mut self, promoted_extent: u64) -> Option<[u8; 32]> {
        self.raw_fingerprint.take()?.finalize(promoted_extent)
    }

    fn validate_string_bounds(&mut self, bytes: &[u8]) -> io::Result<()> {
        for byte in bytes {
            if self.in_string {
                if self.string_escape {
                    self.string_escape = false;
                    self.string_bytes = self.string_bytes.saturating_add(1);
                } else if *byte == b'\\' {
                    self.string_escape = true;
                    self.string_bytes = self.string_bytes.saturating_add(1);
                } else if *byte == b'"' {
                    self.in_string = false;
                    self.string_bytes = 0;
                    continue;
                } else {
                    self.string_bytes = self.string_bytes.saturating_add(1);
                }
                if self.string_bytes > MAX_LEGACY_JSON_STRING_BYTES {
                    self.resource_limit = true;
                    return Err(io::Error::other("legacy Session string bound exceeded"));
                }
            } else if *byte == b'"' {
                self.in_string = true;
                self.string_bytes = 0;
            }
        }
        Ok(())
    }
}

impl Read for PinnedSessionReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.buffer_position == self.buffer.len() {
            if self.offset >= self.file.snapshot().size {
                return Ok(0);
            }
            if self.work_remaining == 0 {
                self.resource_limit = true;
                return Err(io::Error::other(
                    "legacy Session source-work bound exceeded",
                ));
            }
            let maximum = LEGACY_READ_CHUNK_BYTES
                .min(usize::try_from(self.work_remaining).unwrap_or(usize::MAX));
            let bytes = self
                .file
                .read_range_bounded(self.offset, maximum)
                .map_err(|_| io::Error::other("pinned Session read failed"))?;
            self.validate_string_bounds(&bytes)?;
            if self
                .raw_fingerprint
                .as_mut()
                .is_some_and(|fingerprint| !fingerprint.update(&bytes))
            {
                self.resource_limit = true;
                return Err(io::Error::other(
                    "legacy Session fingerprint extent exceeded",
                ));
            }
            self.offset = self.offset.saturating_add(bytes.len() as u64);
            self.read_bytes = self.read_bytes.saturating_add(bytes.len() as u64);
            self.work_remaining = self.work_remaining.saturating_sub(bytes.len() as u64);
            self.peak_chunk_bytes = self.peak_chunk_bytes.max(bytes.len());
            self.buffer = bytes;
            self.buffer_position = 0;
            if self.buffer.is_empty() {
                return Ok(0);
            }
        }
        let length = output
            .len()
            .min(self.buffer.len().saturating_sub(self.buffer_position));
        output[..length]
            .copy_from_slice(&self.buffer[self.buffer_position..self.buffer_position + length]);
        self.buffer_position += length;
        Ok(length)
    }
}

fn parse_legacy_json(
    file: &mut SessionFile,
    domain_key: &[u8; 32],
) -> Result<GeminiAggregate, GeminiScannerError> {
    let size = file.snapshot().size;
    let resource_limit = Cell::new(false);
    let (result, read_bytes, peak_chunk_bytes, reader_resource_limit, structural_hash) = {
        let mut reader = PinnedSessionReader::new(file)
            .with_raw_fingerprint(structural_fingerprint(domain_key, GeminiFormat::LegacyJson));
        let mut deserializer = serde_json::Deserializer::from_reader(&mut reader);
        let result = LegacyDocumentSeed {
            resource_limit: &resource_limit,
        }
        .deserialize(&mut deserializer)
        .and_then(|accumulator| deserializer.end().map(|()| accumulator));
        (
            result,
            reader.read_bytes,
            reader.peak_chunk_bytes,
            reader.resource_limit,
            reader.finish_raw_fingerprint(size),
        )
    };
    let accumulator = result.map_err(|_| {
        if resource_limit.get() || reader_resource_limit {
            GeminiScannerError::ResourceLimit
        } else {
            GeminiScannerError::Parse
        }
    })?;
    let structural_hash = structural_hash.ok_or(GeminiScannerError::Parse)?;
    let session_id = accumulator
        .session_id
        .as_deref()
        .and_then(normalize_external_id);
    let record_ordinal = u64::try_from(accumulator.messages.len())
        .map_err(|_| GeminiScannerError::ResourceLimit)?
        .max(1);
    finalize_aggregate(
        session_id,
        accumulator.created_at,
        accumulator.last_active_at,
        accumulator.messages,
        size,
        record_ordinal,
        read_bytes,
        peak_chunk_bytes,
        accumulator.peak_logical_working_bytes,
        structural_hash,
    )
}

#[allow(clippy::too_many_arguments)]
fn finalize_aggregate(
    session_id: Option<String>,
    created_at: Option<String>,
    mut last_active_at: Option<String>,
    messages: Vec<GeminiLogicalMessage>,
    complete_byte_offset: u64,
    record_ordinal: u64,
    parser_read_bytes: u64,
    peak_parser_buffer_bytes: usize,
    mut peak_logical_working_bytes: usize,
    structural_hash: [u8; 32],
) -> Result<GeminiAggregate, GeminiScannerError> {
    let session_id = session_id.ok_or(GeminiScannerError::Parse)?;
    let created_at = created_at
        .or_else(|| messages.first().map(|message| message.occurred_at.clone()))
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_owned());
    let mut usage_count = 0u64;
    let mut usage_count_inspections = 0u64;
    for message in &messages {
        usage_count_inspections = usage_count_inspections.saturating_add(1);
        if message.usage.is_some() {
            usage_count = usage_count.saturating_add(1);
        }
        last_active_at = maximum_timestamp(last_active_at, &message.occurred_at);
    }
    let last_active_at = last_active_at.unwrap_or_else(|| created_at.clone());
    let final_working_bytes = logical_working_bytes(&messages, 0, logical_dynamic_bytes(&messages))
        .saturating_add(session_id.capacity())
        .saturating_add(created_at.capacity())
        .saturating_add(last_active_at.capacity());
    peak_logical_working_bytes = peak_logical_working_bytes.max(final_working_bytes);
    if peak_logical_working_bytes > MAX_GEMINI_LOGICAL_WORKING_BYTES {
        return Err(GeminiScannerError::ResourceLimit);
    }
    Ok(GeminiAggregate {
        session_id,
        created_at,
        last_active_at,
        messages,
        complete_byte_offset,
        record_ordinal,
        parser_read_bytes,
        peak_parser_buffer_bytes,
        peak_logical_working_bytes,
        structural_hash,
        usage_count,
        usage_count_inspections,
    })
}

fn read_current_title(
    file: &mut SessionFile,
    domain_key: &[u8; 32],
) -> Result<Option<ExternalSessionTitle>, GeminiScannerError> {
    let mut reader = JsonlReader::new(JsonlCursor::new(0, 1));
    let mut messages = Vec::<GeminiTitleMessage>::new();
    let mut positions = HashMap::<[u8; 32], usize>::new();
    let mut dynamic_title_bytes = 0usize;
    let mut summary = None;
    loop {
        let scan = reader.scan(file).map_err(map_jsonl_error)?;
        for record in &scan.records {
            if record.status != JsonlRecordStatus::Valid {
                continue;
            }
            let Some(object) = record.value().as_object() else {
                continue;
            };
            if let Some(rewind) = object.get("$rewindTo").and_then(Value::as_str) {
                let target = opaque_hash(domain_key, LOGICAL_MESSAGE_DOMAIN, &[rewind.as_bytes()]);
                let keep = positions.get(&target).copied().unwrap_or(0);
                for message in messages.drain(keep..) {
                    dynamic_title_bytes =
                        dynamic_title_bytes.saturating_sub(title_message_dynamic_bytes(&message));
                    positions.remove(&message.key);
                }
                continue;
            }
            if let Some(set) = object.get("$set").and_then(Value::as_object) {
                if let Some(messages_value) = set.get("messages") {
                    let checkpoint = messages_value.as_array().ok_or(GeminiScannerError::Parse)?;
                    messages.clear();
                    positions.clear();
                    dynamic_title_bytes = 0;
                    for message in checkpoint {
                        insert_title_message(
                            message,
                            &mut messages,
                            &mut positions,
                            &mut dynamic_title_bytes,
                            domain_key,
                        )?;
                    }
                }
                if let Some(value) = set
                    .get("summary")
                    .and_then(Value::as_str)
                    .and_then(ExternalSessionTitle::from_str)
                {
                    summary = Some(value);
                }
                continue;
            }
            if object.contains_key("sessionId") && !object.contains_key("id") {
                if let Some(messages_value) = object.get("messages") {
                    let checkpoint = messages_value.as_array().ok_or(GeminiScannerError::Parse)?;
                    messages.clear();
                    positions.clear();
                    dynamic_title_bytes = 0;
                    for message in checkpoint {
                        insert_title_message(
                            message,
                            &mut messages,
                            &mut positions,
                            &mut dynamic_title_bytes,
                            domain_key,
                        )?;
                    }
                }
                if let Some(value) = object
                    .get("summary")
                    .and_then(Value::as_str)
                    .and_then(ExternalSessionTitle::from_str)
                {
                    summary = Some(value);
                }
                continue;
            }
            insert_title_message(
                record.value(),
                &mut messages,
                &mut positions,
                &mut dynamic_title_bytes,
                domain_key,
            )?;
        }
        if scan.reached_end || scan.records.is_empty() {
            let first_user = messages.into_iter().find_map(|message| message.user_title);
            return Ok(summary.or(first_user));
        }
    }
}

struct GeminiTitleMessage {
    key: [u8; 32],
    user_title: Option<ExternalSessionTitle>,
}

fn insert_title_message(
    value: &Value,
    messages: &mut Vec<GeminiTitleMessage>,
    positions: &mut HashMap<[u8; 32], usize>,
    dynamic_title_bytes: &mut usize,
    domain_key: &[u8; 32],
) -> Result<(), GeminiScannerError> {
    let Some(object) = value.as_object() else {
        return Ok(());
    };
    let Some(id) = object
        .get("id")
        .and_then(Value::as_str)
        .and_then(normalize_external_id)
    else {
        return Ok(());
    };
    if !matches!(
        object.get("type").and_then(Value::as_str),
        Some("user" | "gemini" | "info" | "warning" | "error")
    ) {
        return Ok(());
    }
    let key = opaque_hash(domain_key, LOGICAL_MESSAGE_DOMAIN, &[id.as_bytes()]);
    let user_title = (object.get("type").and_then(Value::as_str) == Some("user"))
        .then(|| {
            object
                .get("content")
                .and_then(gemini_text_content)
                .and_then(ExternalSessionTitle::from_str)
        })
        .flatten();
    let message = GeminiTitleMessage { key, user_title };
    if let Some(&position) = positions.get(&key) {
        *dynamic_title_bytes = dynamic_title_bytes
            .saturating_sub(title_message_dynamic_bytes(&messages[position]))
            .saturating_add(title_message_dynamic_bytes(&message));
        messages[position] = message;
    } else {
        if messages.len() >= MAX_ACTIVE_MESSAGES {
            return Err(GeminiScannerError::ResourceLimit);
        }
        positions.insert(key, messages.len());
        *dynamic_title_bytes =
            dynamic_title_bytes.saturating_add(title_message_dynamic_bytes(&message));
        messages.push(message);
    }
    let working_bytes = messages
        .capacity()
        .saturating_mul(std::mem::size_of::<GeminiTitleMessage>())
        .saturating_add(
            positions
                .capacity()
                .saturating_mul(std::mem::size_of::<([u8; 32], usize)>().saturating_add(16)),
        )
        .saturating_add(*dynamic_title_bytes);
    if working_bytes > MAX_GEMINI_LOGICAL_WORKING_BYTES {
        return Err(GeminiScannerError::ResourceLimit);
    }
    Ok(())
}

fn title_message_dynamic_bytes(message: &GeminiTitleMessage) -> usize {
    message
        .user_title
        .as_ref()
        .map_or(0, |title| title.as_str().len())
}

fn read_legacy_title(
    file: &mut SessionFile,
) -> Result<Option<ExternalSessionTitle>, GeminiScannerError> {
    let resource_limit = Cell::new(false);
    let (result, reader_resource_limit) = {
        let mut reader = PinnedSessionReader::new(file);
        let mut deserializer = serde_json::Deserializer::from_reader(&mut reader);
        let result = LegacyTitleDocumentSeed {
            resource_limit: &resource_limit,
        }
        .deserialize(&mut deserializer)
        .and_then(|title| deserializer.end().map(|()| title));
        (result, reader.resource_limit)
    };
    result.map_err(|_| {
        if resource_limit.get() || reader_resource_limit {
            GeminiScannerError::ResourceLimit
        } else {
            GeminiScannerError::Parse
        }
    })
}

struct LegacyTitleDocumentSeed<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for LegacyTitleDocumentSeed<'_> {
    type Value = Option<ExternalSessionTitle>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(LegacyTitleDocumentVisitor {
            resource_limit: self.resource_limit,
        })
    }
}

struct LegacyTitleDocumentVisitor<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> Visitor<'de> for LegacyTitleDocumentVisitor<'_> {
    type Value = Option<ExternalSessionTitle>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a Gemini legacy Session title source")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut summary = None;
        let mut first_user = None;
        while let Some(key) = map.next_key_seed(BoundedStringSeed::<MAX_LEGACY_MAP_KEY_BYTES> {
            resource_limit: self.resource_limit,
        })? {
            match key.as_str() {
                "summary" => {
                    let value =
                        map.next_value_seed(BoundedStringSeed::<MAX_LEGACY_JSON_STRING_BYTES> {
                            resource_limit: self.resource_limit,
                        })?;
                    summary = ExternalSessionTitle::new(value);
                }
                "messages" => {
                    first_user = map.next_value_seed(LegacyTitleMessagesSeed {
                        resource_limit: self.resource_limit,
                    })?;
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(summary.or(first_user))
    }
}

struct LegacyTitleMessagesSeed<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for LegacyTitleMessagesSeed<'_> {
    type Value = Option<ExternalSessionTitle>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(LegacyTitleMessagesVisitor {
            resource_limit: self.resource_limit,
        })
    }
}

struct LegacyTitleMessagesVisitor<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> Visitor<'de> for LegacyTitleMessagesVisitor<'_> {
    type Value = Option<ExternalSessionTitle>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a Gemini legacy Session message sequence")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut first_user = None;
        let mut count = 0usize;
        loop {
            if count >= MAX_ACTIVE_MESSAGES {
                self.resource_limit.set(true);
                return Err(serde::de::Error::custom(
                    "Gemini title message count exceeds its bound",
                ));
            }
            let has_value = if first_user.is_some() {
                sequence.next_element::<IgnoredAny>()?.is_some()
            } else {
                let candidate = sequence.next_element_seed(LegacyTitleMessageSeed {
                    resource_limit: self.resource_limit,
                })?;
                if let Some(candidate) = candidate {
                    if candidate.is_user {
                        first_user = candidate.content.and_then(ExternalSessionTitle::new);
                    }
                    true
                } else {
                    false
                }
            };
            if !has_value {
                return Ok(first_user);
            }
            count += 1;
        }
    }
}

struct LegacyTitleMessage {
    is_user: bool,
    content: Option<String>,
}

struct LegacyTitleMessageSeed<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for LegacyTitleMessageSeed<'_> {
    type Value = LegacyTitleMessage;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(LegacyTitleMessageVisitor {
            resource_limit: self.resource_limit,
        })
    }
}

struct LegacyTitleMessageVisitor<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> Visitor<'de> for LegacyTitleMessageVisitor<'_> {
    type Value = LegacyTitleMessage;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a Gemini legacy title message")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut message_type = None;
        let mut content = None;
        while let Some(key) = map.next_key_seed(BoundedStringSeed::<MAX_LEGACY_MAP_KEY_BYTES> {
            resource_limit: self.resource_limit,
        })? {
            match key.as_str() {
                "type" => {
                    message_type = Some(map.next_value_seed(BoundedStringSeed::<
                        MAX_LEGACY_MESSAGE_TYPE_BYTES,
                    > {
                        resource_limit: self.resource_limit,
                    })?)
                }
                "content" => {
                    content = map.next_value_seed(LegacyTitleContentSeed {
                        resource_limit: self.resource_limit,
                    })?
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(LegacyTitleMessage {
            is_user: message_type.as_deref() == Some("user"),
            content,
        })
    }
}

struct LegacyTitleContentSeed<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for LegacyTitleContentSeed<'_> {
    type Value = Option<String>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(LegacyTitleContentVisitor {
            resource_limit: self.resource_limit,
        })
    }
}

struct LegacyTitleContentVisitor<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> Visitor<'de> for LegacyTitleContentVisitor<'_> {
    type Value = Option<String>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bounded Gemini text content")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok((value.len() <= crate::model::TITLE_LIMIT_BYTES).then(|| value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok((value.len() <= crate::model::TITLE_LIMIT_BYTES).then_some(value))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut first = None;
        let mut count = 0usize;
        while let Some(candidate) = sequence.next_element_seed(LegacyTitlePartSeed {
            resource_limit: self.resource_limit,
        })? {
            if count >= MAX_MESSAGE_TOOLS_FOR_TITLE {
                self.resource_limit.set(true);
                return Err(serde::de::Error::custom(
                    "Gemini title content array exceeds its bound",
                ));
            }
            first = first.or(candidate);
            count += 1;
        }
        Ok(first)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(None)
    }
}

const MAX_MESSAGE_TOOLS_FOR_TITLE: usize = 128;

struct LegacyTitlePartSeed<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for LegacyTitlePartSeed<'_> {
    type Value = Option<String>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(LegacyTitlePartVisitor {
            resource_limit: self.resource_limit,
        })
    }
}

struct LegacyTitlePartVisitor<'a> {
    resource_limit: &'a Cell<bool>,
}

impl<'de> Visitor<'de> for LegacyTitlePartVisitor<'_> {
    type Value = Option<String>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a Gemini content part")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut text = None;
        while let Some(key) = map.next_key_seed(BoundedStringSeed::<MAX_LEGACY_MAP_KEY_BYTES> {
            resource_limit: self.resource_limit,
        })? {
            if key == "text" {
                let value =
                    map.next_value_seed(BoundedStringSeed::<MAX_LEGACY_JSON_STRING_BYTES> {
                        resource_limit: self.resource_limit,
                    })?;
                if value.len() <= crate::model::TITLE_LIMIT_BYTES {
                    text = Some(value);
                }
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(text)
    }
}

fn gemini_text_content(value: &Value) -> Option<&str> {
    match value {
        Value::String(value) => Some(value),
        Value::Array(items) => items
            .iter()
            .find_map(|item| item.get("text").and_then(Value::as_str)),
        _ => None,
    }
}

fn map_jsonl_error(error: JsonlError) -> GeminiScannerError {
    match error {
        JsonlError::RecordTooLarge { .. } => GeminiScannerError::ResourceLimit,
        JsonlError::SourceChanged
        | JsonlError::SourceUnavailable
        | JsonlError::ReadFailed
        | JsonlError::CursorOverflow => GeminiScannerError::Read,
    }
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_be_bytes)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use wokcore_platform::sessions::SessionRootLease;

    use super::{GeminiScannerError, next_generation, read_current_title};

    #[test]
    fn generation_overflow_is_rejected() {
        assert_eq!(next_generation(None).unwrap(), 1);
        assert_eq!(next_generation(Some(41)).unwrap(), 42);
        assert!(matches!(
            next_generation(Some(i64::MAX as u64)),
            Err(GeminiScannerError::ResourceLimit)
        ));
        assert!(matches!(
            next_generation(Some(u64::MAX)),
            Err(GeminiScannerError::ResourceLimit)
        ));
    }

    #[test]
    fn current_title_rejects_non_array_last_checkpoint_duplicate() {
        let root = tempfile::tempdir().unwrap();
        let relative = Path::new("tmp/project/chats/session-invalid.jsonl");
        let path = root.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{"sessionId":"invalid","projectHash":"synthetic","startTime":"2026-07-26T12:00:00Z","messages":[{"id":"old","timestamp":"2026-07-26T12:00:00Z","type":"user","content":"OLD TITLE"}]}
{"$set":{"messages":[{"id":"new","timestamp":"2026-07-26T12:00:01Z","type":"user","content":"new"}],"m\u0065ssages":null}}
"#,
        )
        .unwrap();
        let lease = SessionRootLease::open(root.path()).unwrap();
        let mut file = lease.open_file(relative, u64::MAX).unwrap();

        assert!(matches!(
            read_current_title(&mut file, &[0x61; 32]),
            Err(GeminiScannerError::Parse)
        ));
    }
}
