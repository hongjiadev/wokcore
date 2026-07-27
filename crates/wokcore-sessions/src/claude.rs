use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fmt,
    path::{Path, PathBuf},
};

use serde_json::{Map, Value};
use wokcore_platform::sessions::{
    SessionDirectoryLease, SessionError, SessionFile, SessionFileIdentity as PlatformFileIdentity,
    SessionFileKind, SessionRootLease,
};
use wokcore_storage::{
    CandidateBeginOutcome, MAX_SESSION_BATCH_BYTES, MAX_SESSION_BATCH_ROWS, ParserCheckpoint,
    SessionAvailability, SessionBatch, SessionFileIdentity, SessionGenerationState,
    SessionIndexRecord, SessionScanCursor, SessionScanResultCode, SessionSourceErrorCode,
    SessionSourceKind, SessionSourcePageKey, SessionSourceStatus, SessionUsagePageKey,
    SessionUsageRecord, StateStore, StateStoreWriterClient, StorageError,
};

use crate::{
    cursor::{JsonlCursor, JsonlError, JsonlReader, JsonlRecordStatus},
    discovery::{
        DiscoveryLimits, SessionDiscoveryClock, SessionDiscoveryCursor, SessionDiscoveryEntry,
        SessionDiscoveryKind, SessionDiscoverySliceBudget, SessionDiscoverySliceError,
        SessionDiscoverySliceOutcome, SessionDiscoverySourceFormat, SystemSessionDiscoveryClock,
        discover_claude_sessions_slice_with_clock,
    },
    model::{
        EXTERNAL_ID_LIMIT_BYTES, ExternalSessionTitle, MAX_ACTIVE_MESSAGES,
        SESSION_BATCH_ROW_TARGET, SessionScanControl, SessionScanOutcome, SessionScanSummary,
        SessionScannerMetrics, SessionSourceScanSummary, fingerprints, fingerprints_with_extent,
        maximum_timestamp, normalize_external_id, normalize_external_model, normalize_timestamp,
        opaque_hash, opaque_hex, opaque_platform_identity, system_time_utc,
    },
    state::SessionState,
};

const PARSER_CHECKPOINT_VERSION: u16 = 1;
const FILE_IDENTITY_DOMAIN: &[u8] = b"wokcore.claude.file-identity.v1";
const SOURCE_KEY_DOMAIN: &[u8] = b"wokcore.claude.source-key.v1";
const SESSION_KEY_DOMAIN: &[u8] = b"wokcore.claude.session-key.v1";
const USAGE_ID_DOMAIN: &[u8] = b"wokcore.claude.usage-id.v1";
const LOGICAL_MESSAGE_DOMAIN: &[u8] = b"wokcore.claude.logical-message.v1";
const FINGERPRINT_DOMAIN: &[u8] = b"wokcore.claude.source-fingerprint.v1";
pub const MAX_CLAUDE_LOGICAL_WORKING_BYTES: usize = 512 * 1024;
pub const MAX_CLAUDE_JSONL_SOURCE_WORK_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TITLE_SOURCE_WORK_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CLAUDE_SUBAGENT_DIRECTORY_DEPTH: usize = 16;
const MAX_CLAUDE_SUBAGENT_DIRECTORIES: usize = 16_384;
const MAX_CLAUDE_JSONL_SOURCE_BYTES: u64 = MAX_CLAUDE_JSONL_SOURCE_WORK_BYTES;

#[derive(Debug, thiserror::Error)]
pub enum ClaudeScannerError {
    #[error("Claude Session storage failed")]
    Storage(#[from] StorageError),
    #[error("Claude Session root is unavailable")]
    Root,
    #[error("Claude Session discovery failed")]
    Discovery,
    #[error("Claude Session read failed")]
    Read,
    #[error("Claude Session record failed structural validation")]
    Parse,
    #[error("Claude Session record exceeds its resource bound")]
    ResourceLimit,
    #[error("Claude Session generation cleanup remains pending")]
    CleanupPending,
}

#[derive(Clone)]
struct DiscoveredClaudeSession {
    relative_path: PathBuf,
    identity: PlatformFileIdentity,
}

impl DiscoveredClaudeSession {
    fn from_slice(entry: &SessionDiscoveryEntry) -> Option<Self> {
        (entry.format() == SessionDiscoverySourceFormat::ClaudeJsonl).then(|| Self {
            relative_path: entry.relative_path().to_path_buf(),
            identity: entry.identity(),
        })
    }

    fn open(
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

#[derive(Clone, Copy, Eq, PartialEq)]
enum ScannerSlicePhase {
    Discovering,
    Processing,
}

struct ClaudeSliceCycle {
    transition_at: String,
    phase: ScannerSlicePhase,
    cursor: SessionDiscoveryCursor,
    persisted_sources: HashSet<String>,
    persisted_identities: HashMap<String, String>,
    reserved_keys: HashMap<String, String>,
    source_keys_by_identity: HashMap<String, String>,
    current_sources: HashSet<String>,
    processed_identities: HashSet<String>,
}

impl ClaudeSliceCycle {
    fn new(scanner: &ClaudeScanner, transition_at: &str) -> Result<Self, ClaudeScannerError> {
        Ok(Self {
            transition_at: transition_at.to_owned(),
            phase: ScannerSlicePhase::Discovering,
            cursor: SessionDiscoveryCursor::with_limits(
                SessionDiscoveryKind::Claude,
                scanner.discovery_limits,
            )
            .map_err(|_| ClaudeScannerError::ResourceLimit)?,
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
        scanner: &ClaudeScanner,
        source: &DiscoveredClaudeSession,
    ) -> Result<(), ClaudeScannerError> {
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
                        b"wokcore.claude.source-collision.v1",
                        &[
                            &path_bytes(&source.relative_path),
                            identity.as_bytes(),
                            &counter.to_be_bytes(),
                        ],
                    );
                    if !self.reserved_keys.contains_key(&candidate)
                        && !self.persisted_sources.contains(&candidate)
                    {
                        break candidate;
                    }
                    counter = counter.checked_add(1).ok_or(ClaudeScannerError::Parse)?;
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

fn map_claude_slice_error(error: SessionDiscoverySliceError) -> ClaudeScannerError {
    match error {
        SessionDiscoverySliceError::Discovery(crate::discovery::DiscoveryError::Limit) => {
            ClaudeScannerError::ResourceLimit
        }
        SessionDiscoverySliceError::CursorKindMismatch
        | SessionDiscoverySliceError::Discovery(_) => ClaudeScannerError::Discovery,
    }
}

impl fmt::Debug for DiscoveredClaudeSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiscoveredClaudeSession")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct ClaudeUsage {
    model: String,
    occurred_at: String,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    revision: u64,
}

#[derive(Clone)]
struct ClaudeLogicalMessage {
    message_id: String,
    occurred_at: String,
    usage: Option<ClaudeUsage>,
    first_byte_start: u64,
}

struct ClaudeAggregate {
    session_id: String,
    created_at: String,
    last_active_at: String,
    messages: Vec<ClaudeLogicalMessage>,
    complete_byte_offset: u64,
    record_ordinal: u64,
    appended_replacement: bool,
    parser_read_bytes: u64,
    peak_parser_buffer_bytes: usize,
    peak_logical_working_bytes: usize,
    usage_count: u64,
    usage_count_inspections: u64,
}

impl ClaudeAggregate {
    fn usage_count(&self) -> u64 {
        self.usage_count
    }

    fn usage(&self) -> impl Iterator<Item = (&ClaudeLogicalMessage, &ClaudeUsage)> {
        self.messages
            .iter()
            .filter_map(|message| message.usage.as_ref().map(|usage| (message, usage)))
    }
}

pub struct ClaudeScanner {
    root: SessionRootLease,
    state: SessionState,
    domain_key: [u8; 32],
    discovery_limits: DiscoveryLimits,
    metrics: SessionScannerMetrics,
    slice_cycle: Option<ClaudeSliceCycle>,
}

impl ClaudeScanner {
    pub fn open(
        root_path: impl AsRef<Path>,
        state_path: impl AsRef<Path>,
        domain_key: [u8; 32],
    ) -> Result<Self, ClaudeScannerError> {
        Self::open_internal(root_path, state_path, domain_key, None)
    }

    pub fn open_with_writer(
        root_path: impl AsRef<Path>,
        state_path: impl AsRef<Path>,
        domain_key: [u8; 32],
        writer: StateStoreWriterClient,
    ) -> Result<Self, ClaudeScannerError> {
        Self::open_internal(root_path, state_path, domain_key, Some(writer))
    }

    fn open_internal(
        root_path: impl AsRef<Path>,
        state_path: impl AsRef<Path>,
        domain_key: [u8; 32],
        writer: Option<StateStoreWriterClient>,
    ) -> Result<Self, ClaudeScannerError> {
        let root = SessionRootLease::open(root_path).map_err(|_| ClaudeScannerError::Root)?;
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
    ) -> Result<SessionScanSummary, ClaudeScannerError> {
        self.scan_slice_with_clock(transition_at, control, budget, &SystemSessionDiscoveryClock)
    }

    pub fn scan_slice_with_clock<C>(
        &mut self,
        transition_at: &str,
        control: SessionScanControl,
        budget: SessionDiscoverySliceBudget,
        clock: &C,
    ) -> Result<SessionScanSummary, ClaudeScannerError>
    where
        C: SessionDiscoveryClock + ?Sized,
    {
        self.metrics = SessionScannerMetrics::default();
        let mut cycle = match self.slice_cycle.take() {
            Some(cycle) => cycle,
            None => ClaudeSliceCycle::new(self, transition_at)?,
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
        cycle: &mut ClaudeSliceCycle,
        control: SessionScanControl,
        budget: SessionDiscoverySliceBudget,
        clock: &C,
    ) -> Result<(SessionScanSummary, bool), ClaudeScannerError>
    where
        C: SessionDiscoveryClock + ?Sized,
    {
        match cycle.phase {
            ScannerSlicePhase::Discovering => {
                let slice = discover_claude_sessions_slice_with_clock(
                    &self.root,
                    &mut cycle.cursor,
                    budget,
                    clock,
                )
                .map_err(map_claude_slice_error)?;
                for entry in &slice.entries {
                    let Some(source) = DiscoveredClaudeSession::from_slice(entry) else {
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
                    cycle.phase = ScannerSlicePhase::Processing;
                    cycle.cursor = SessionDiscoveryCursor::with_limits(
                        SessionDiscoveryKind::Claude,
                        self.discovery_limits,
                    )
                    .map_err(|_| ClaudeScannerError::ResourceLimit)?;
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
            ScannerSlicePhase::Processing => {
                let slice = discover_claude_sessions_slice_with_clock(
                    &self.root,
                    &mut cycle.cursor,
                    budget,
                    clock,
                )
                .map_err(map_claude_slice_error)?;
                let mut summaries = Vec::new();
                let mut advanced_sources = 0;
                let mut unchanged_sources = 0;
                let mut committed_batches = 0;
                let mut restart_processing = false;
                for entry in &slice.entries {
                    let Some(source) = DiscoveredClaudeSession::from_slice(entry) else {
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
                        SessionDiscoveryKind::Claude,
                        self.discovery_limits,
                    )
                    .map_err(|_| ClaudeScannerError::ResourceLimit)?;
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
        source: &DiscoveredClaudeSession,
        source_key: String,
        transition_at: &str,
        control: SessionScanControl,
        committed_batches: &mut usize,
    ) -> Result<(SessionSourceScanSummary, Option<ProcessOutcome>), ClaudeScannerError> {
        match self.process_source(
            source,
            &source_key,
            transition_at,
            control,
            committed_batches,
        ) {
            Ok(result) => Ok((result.summary, Some(result.process))),
            Err(
                error @ (ClaudeScannerError::Read
                | ClaudeScannerError::Parse
                | ClaudeScannerError::ResourceLimit),
            ) => {
                let code = match error {
                    ClaudeScannerError::Read => SessionSourceErrorCode::SourceIoFailed,
                    ClaudeScannerError::Parse => SessionSourceErrorCode::SourceParseInvalid,
                    ClaudeScannerError::ResourceLimit => {
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
            Err(ClaudeScannerError::CleanupPending) => {
                let source_state = self.state.load_session_source(&source_key)?;
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
                        status: source_state
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

    pub fn scan(
        &mut self,
        transition_at: &str,
        control: SessionScanControl,
    ) -> Result<SessionScanSummary, ClaudeScannerError> {
        self.slice_cycle = None;
        self.metrics = SessionScannerMetrics::default();
        let discovered = discover_claude_sessions(&self.root, self.discovery_limits)?;
        let persisted = self.persisted_sources()?;
        let persisted_identities = self.persisted_identity_sources()?;
        let source_keys =
            self.resolve_source_keys(&discovered, &persisted, &persisted_identities)?;
        let current_keys = source_keys.iter().cloned().collect::<HashSet<_>>();
        let mut deleted_sources = 0usize;
        for source_key in persisted.difference(&current_keys) {
            let _ = self.state.mark_source_unavailable(
                source_key,
                SessionSourceErrorCode::SourceSessionsAbsent,
                transition_at,
            )?;
            deleted_sources += 1;
        }

        let mut summaries = Vec::with_capacity(discovered.len());
        let mut advanced_sources = 0usize;
        let mut unchanged_sources = 0usize;
        let mut outcome = SessionScanOutcome::Complete;
        let mut committed_batches = 0usize;
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
                    error @ (ClaudeScannerError::Read
                    | ClaudeScannerError::Parse
                    | ClaudeScannerError::ResourceLimit),
                ) => {
                    let code = match error {
                        ClaudeScannerError::Read => SessionSourceErrorCode::SourceIoFailed,
                        ClaudeScannerError::Parse => SessionSourceErrorCode::SourceParseInvalid,
                        ClaudeScannerError::ResourceLimit => {
                            SessionSourceErrorCode::SourceRecordTooLarge
                        }
                        _ => unreachable!(),
                    };
                    self.record_failure(source, &source_key, code, transition_at)?;
                    let state = self.state.load_session_source(&source_key)?;
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
                        complete_byte_offset: self
                            .state
                            .load_current_session_scan_cursor(
                                state
                                    .as_ref()
                                    .map_or("", |source| source.source_key.as_str()),
                            )
                            .ok()
                            .flatten()
                            .map_or(0, |cursor| cursor.complete_byte_offset),
                    });
                }
                Err(ClaudeScannerError::CleanupPending) => {
                    outcome = SessionScanOutcome::Interrupted;
                    let source_state = self.state.load_session_source(&source_key)?;
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
                        status: source_state
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
        source: &DiscoveredClaudeSession,
        source_key: &str,
        transition_at: &str,
        control: SessionScanControl,
        committed_batches: &mut usize,
    ) -> Result<ProcessResult, ClaudeScannerError> {
        self.metrics.source_opens = self.metrics.source_opens.saturating_add(1);
        let mut file = source
            .open(&self.root, MAX_CLAUDE_JSONL_SOURCE_BYTES)
            .map_err(|error| match error {
                SessionError::ReadLimitExceeded => ClaudeScannerError::ResourceLimit,
                _ => ClaudeScannerError::Read,
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
            return Err(ClaudeScannerError::CleanupPending);
        }
        let source_state = self.state.load_session_source(source_key)?;

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
            if cursor.file_identity.as_str() == self.file_identity(source).as_str()
                && cursor.observed_size == snapshot.size
                && cursor.modified_at == modified_at
                && cursor.head_fingerprint == head
                && cursor.boundary_fingerprint == boundary
                && cursor.parser_checkpoint.version == PARSER_CHECKPOINT_VERSION
                && cursor.result_code == Some(SessionScanResultCode::Advanced)
                && state_is_healthy
            {
                let _ = self.state.record_source_success(
                    source_key,
                    cursor.generation,
                    transition_at,
                )?;
                let summary =
                    self.summary_from_current(source_key, SessionSourceStatus::Available)?;
                return Ok(ProcessResult {
                    process: ProcessOutcome::Unchanged,
                    summary,
                });
            }
        }

        self.metrics.full_source_scans = self.metrics.full_source_scans.saturating_add(1);
        let append_boundary = current.as_ref().and_then(|cursor| {
            (cursor.file_identity.as_str() == self.file_identity(source).as_str()
                && snapshot.size >= cursor.complete_byte_offset
                && cursor.parser_checkpoint.version == PARSER_CHECKPOINT_VERSION)
                .then_some(cursor.complete_byte_offset)
        });
        let aggregate = parse_claude_aggregate(&mut file, append_boundary, &self.domain_key)?;
        validate_usage_storage_bounds(&aggregate)?;
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
        let continuation_usage = append_boundary.map_or(usize::MAX, |boundary| {
            aggregate
                .usage()
                .filter(|(message, _)| message.first_byte_start >= boundary)
                .count()
        });
        let continuation = current.is_some()
            && append_boundary.is_some()
            && !aggregate.appended_replacement
            && continuation_usage <= SESSION_BATCH_ROW_TARGET
            && self.current_aggregate_is_prefix(
                source_key,
                &aggregate,
                append_boundary.unwrap(),
            )?;
        if continuation {
            let generation = current
                .as_ref()
                .expect("continuation has a current cursor")
                .generation;
            let boundary = append_boundary.expect("continuation has a boundary");
            let mut new_usage = aggregate
                .usage()
                .filter(|(message, _)| message.first_byte_start >= boundary)
                .collect::<Vec<_>>();
            new_usage.sort_unstable_by_key(|(_, usage)| usage.revision);
            let cursor = self.completed_cursor(
                source,
                source_key,
                generation,
                SessionGenerationState::Current,
                &snapshot,
                &modified_at,
                &aggregate,
                &mut file,
                aggregate.usage_count(),
                aggregate.record_ordinal,
                transition_at,
            )?;
            let batch = SessionBatch {
                cursor: Some(cursor),
                index_records: vec![self.index_record(source_key, generation, &aggregate)],
                usage_records: new_usage
                    .iter()
                    .map(|(message, usage)| {
                        self.usage_record(source_key, generation, &aggregate, message, usage)
                    })
                    .collect(),
                replay_signatures: Vec::new(),
                supplemental_metadata: Vec::new(),
            };
            self.state.commit_session_batch(&batch)?;
            let _ = self
                .state
                .record_source_success(source_key, generation, transition_at)?;
            self.metrics.committed_batches = self.metrics.committed_batches.saturating_add(1);
            *committed_batches += 1;
            return Ok(ProcessResult {
                process: ProcessOutcome::Advanced,
                summary: SessionSourceScanSummary {
                    source_key: source_key.to_owned(),
                    session_key: Some(self.session_key(&aggregate.session_id)),
                    status: SessionSourceStatus::Available,
                    error_code: None,
                    complete_byte_offset: aggregate.complete_byte_offset,
                },
            });
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
        )?;
        let emitted = usize::try_from(cursor.parser_checkpoint.event_ordinal)
            .map_err(|_| ClaudeScannerError::Parse)?;
        let mut usage_entries = aggregate.usage().collect::<Vec<_>>();
        usage_entries.sort_unstable_by_key(|(_, usage)| usage.revision);
        if emitted > usage_entries.len() {
            return Err(ClaudeScannerError::Parse);
        }
        for chunk in usage_entries[emitted..].chunks(SESSION_BATCH_ROW_TARGET) {
            let next = cursor
                .parser_checkpoint
                .event_ordinal
                .checked_add(chunk.len() as u64)
                .ok_or(ClaudeScannerError::Parse)?;
            cursor = self.completed_cursor(
                source,
                source_key,
                generation,
                SessionGenerationState::Staging,
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
                    summary: self.summary_from_current_or_staging(
                        source_key,
                        &aggregate,
                        SessionSourceErrorCode::SourceCandidateInterrupted,
                    )?,
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
                SessionGenerationState::Staging,
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
                    summary: self.summary_from_current_or_staging(
                        source_key,
                        &aggregate,
                        SessionSourceErrorCode::SourceCandidateInterrupted,
                    )?,
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
            return Err(ClaudeScannerError::CleanupPending);
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

    fn current_aggregate_is_prefix(
        &self,
        source_key: &str,
        aggregate: &ClaudeAggregate,
        append_boundary: u64,
    ) -> Result<bool, ClaudeScannerError> {
        let current_index = self
            .state
            .load_current_session_index_page(source_key, None, 1)?
            .items
            .into_iter()
            .next();
        let Some(current_index) = current_index else {
            return Ok(false);
        };
        let prefix_messages = aggregate
            .messages
            .iter()
            .filter(|message| message.first_byte_start < append_boundary)
            .count() as u64;
        let prefix_usage = aggregate
            .usage()
            .filter(|(message, _)| message.first_byte_start < append_boundary)
            .count() as u64;
        if current_index.message_count != prefix_messages
            || current_index.usage_event_count != prefix_usage
        {
            return Ok(false);
        }
        let generation = current_index.generation;
        let mut persisted = HashMap::new();
        let mut page_key: Option<SessionUsagePageKey> = None;
        loop {
            let page =
                self.state
                    .load_current_session_usage_page(source_key, page_key.as_ref(), 500)?;
            for record in page.items {
                persisted.insert(record.usage_id.clone(), record);
            }
            page_key = page.next_page_key;
            if page_key.is_none() {
                break;
            }
        }
        for (message, usage) in aggregate
            .usage()
            .filter(|(message, _)| message.first_byte_start < append_boundary)
        {
            let expected = self.usage_record(source_key, generation, aggregate, message, usage);
            let Some(actual) = persisted.remove(&expected.usage_id) else {
                return Ok(false);
            };
            if actual != expected {
                return Ok(false);
            }
        }
        Ok(persisted.is_empty())
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_candidate(
        &mut self,
        source: &DiscoveredClaudeSession,
        source_key: &str,
        generation: u64,
        snapshot: &wokcore_platform::sessions::SessionFileSnapshot,
        modified_at: &str,
        file: &mut SessionFile,
        transition_at: &str,
        cleanup_performed: &mut bool,
    ) -> Result<SessionScanCursor, ClaudeScannerError> {
        if let Some(staging) = self.state.load_staging_session_scan_cursor(source_key)? {
            if staging.generation == generation
                && staging.file_identity.as_str() == self.file_identity(source).as_str()
                && staging.observed_size == snapshot.size
                && staging.modified_at == modified_at
                && staging.parser_checkpoint.version == PARSER_CHECKPOINT_VERSION
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
                                return Err(ClaudeScannerError::CleanupPending);
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
                            )
                        }
                    };
                }
            }
            if !self.cleanup_generation_once(source_key, staging.generation, cleanup_performed)? {
                return Err(ClaudeScannerError::CleanupPending);
            }
        }
        let (head, boundary) =
            fingerprints(file, 0, &self.domain_key, FINGERPRINT_DOMAIN).map_err(map_jsonl_error)?;
        let cursor = SessionScanCursor {
            source_key: source_key.to_owned(),
            source_kind: SessionSourceKind::Claude,
            generation,
            generation_state: SessionGenerationState::Staging,
            file_identity: self.file_identity(source),
            observed_size: snapshot.size,
            modified_at: modified_at.to_owned(),
            complete_byte_offset: 0,
            stable_record_ordinal: 0,
            parser_checkpoint: parser_checkpoint(0),
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
                    return Err(ClaudeScannerError::CleanupPending);
                }
                match self.state.begin_or_resume_candidate(&cursor)? {
                    CandidateBeginOutcome::Started => Ok(cursor),
                    CandidateBeginOutcome::Resumed(cursor) => Ok(*cursor),
                    CandidateBeginOutcome::CleanupRequired { .. } => Err(ClaudeScannerError::Parse),
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn completed_cursor(
        &self,
        source: &DiscoveredClaudeSession,
        source_key: &str,
        generation: u64,
        state: SessionGenerationState,
        snapshot: &wokcore_platform::sessions::SessionFileSnapshot,
        modified_at: &str,
        aggregate: &ClaudeAggregate,
        file: &mut SessionFile,
        emitted_usage: u64,
        stable_record_ordinal: u64,
        transition_at: &str,
    ) -> Result<SessionScanCursor, ClaudeScannerError> {
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
            source_kind: SessionSourceKind::Claude,
            generation,
            generation_state: state,
            file_identity: self.file_identity(source),
            observed_size: snapshot.size,
            modified_at: modified_at.to_owned(),
            complete_byte_offset,
            stable_record_ordinal,
            parser_checkpoint: parser_checkpoint(emitted_usage),
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
        aggregate: &ClaudeAggregate,
    ) -> SessionIndexRecord {
        SessionIndexRecord {
            session_key: self.session_key(&aggregate.session_id),
            source_key: source_key.to_owned(),
            generation,
            source_kind: SessionSourceKind::Claude,
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
        aggregate: &ClaudeAggregate,
        message: &ClaudeLogicalMessage,
        usage: &ClaudeUsage,
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
            source_kind: SessionSourceKind::Claude,
            model: usage.model.clone(),
            occurred_at: usage.occurred_at.clone(),
            input_tokens: usage.input,
            output_tokens: usage.output,
            cache_read_tokens: usage.cache_read,
            cache_write_tokens: usage.cache_write,
            reasoning_tokens: 0,
            record_revision: usage.revision,
        }
    }

    fn summary_from_current(
        &self,
        source_key: &str,
        status: SessionSourceStatus,
    ) -> Result<SessionSourceScanSummary, ClaudeScannerError> {
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
            status,
            error_code: None,
            complete_byte_offset: cursor.map_or(0, |cursor| cursor.complete_byte_offset),
        })
    }

    fn summary_from_current_or_staging(
        &self,
        source_key: &str,
        aggregate: &ClaudeAggregate,
        code: SessionSourceErrorCode,
    ) -> Result<SessionSourceScanSummary, ClaudeScannerError> {
        let source = self.state.load_session_source(source_key)?;
        let current = self.state.load_current_session_scan_cursor(source_key)?;
        Ok(SessionSourceScanSummary {
            source_key: source_key.to_owned(),
            session_key: Some(self.session_key(&aggregate.session_id)),
            status: source.map_or(SessionSourceStatus::Unavailable, |source| source.status),
            error_code: Some(code),
            complete_byte_offset: current.map_or(0, |cursor| cursor.complete_byte_offset),
        })
    }

    fn record_failure(
        &mut self,
        source: &DiscoveredClaudeSession,
        source_key: &str,
        code: SessionSourceErrorCode,
        transition_at: &str,
    ) -> Result<(), ClaudeScannerError> {
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
        let mut file = match source.open(&self.root, MAX_CLAUDE_JSONL_SOURCE_BYTES) {
            Ok(file) => file,
            Err(_) => return Ok(()),
        };
        let snapshot = file.snapshot().clone();
        let (head, boundary) = fingerprints(&mut file, 0, &self.domain_key, FINGERPRINT_DOMAIN)
            .map_err(map_jsonl_error)?;
        let cursor = SessionScanCursor {
            source_key: source_key.to_owned(),
            source_kind: SessionSourceKind::Claude,
            generation: 1,
            generation_state: SessionGenerationState::Staging,
            file_identity: self.file_identity(source),
            observed_size: snapshot.size,
            modified_at: system_time_utc(snapshot.modified),
            complete_byte_offset: 0,
            stable_record_ordinal: 0,
            parser_checkpoint: parser_checkpoint(0),
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
                if self.cleanup_generation_once(source_key, generation, &mut cleanup_performed)? {
                    let _ = self.state.begin_or_resume_candidate(&cursor)?;
                } else {
                    return Ok(());
                }
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
    ) -> Result<bool, ClaudeScannerError> {
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
    ) -> Result<bool, ClaudeScannerError> {
        if *cleanup_performed {
            return Ok(false);
        }
        *cleanup_performed = true;
        let outcome = self.state.cleanup_generation_batch(
            source_key,
            generation,
            MAX_SESSION_BATCH_ROWS,
            MAX_SESSION_BATCH_BYTES,
        )?;
        Ok(outcome.complete)
    }

    fn persisted_sources(&self) -> Result<HashSet<String>, ClaudeScannerError> {
        let mut output = HashSet::new();
        let mut page_key: Option<SessionSourcePageKey> = None;
        loop {
            let page = self
                .state
                .load_session_sources_page(page_key.as_ref(), MAX_SESSION_BATCH_ROWS)?;
            for source in page.items {
                if source.source_kind == SessionSourceKind::Claude {
                    output.insert(source.source_key);
                }
            }
            page_key = page.next_page_key;
            if page_key.is_none() {
                return Ok(output);
            }
        }
    }

    fn persisted_identity_sources(&self) -> Result<HashMap<String, String>, ClaudeScannerError> {
        let mut output = HashMap::new();
        let mut page_key: Option<SessionSourcePageKey> = None;
        loop {
            let page = self
                .state
                .load_session_sources_page(page_key.as_ref(), MAX_SESSION_BATCH_ROWS)?;
            for source in page.items {
                if source.source_kind != SessionSourceKind::Claude {
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
        discovered: &[DiscoveredClaudeSession],
        persisted: &HashSet<String>,
        persisted_identities: &HashMap<String, String>,
    ) -> Result<Vec<String>, ClaudeScannerError> {
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
                        b"wokcore.claude.source-collision.v1",
                        &[
                            &path_bytes(&source.relative_path),
                            identity.as_str().as_bytes(),
                            &counter.to_be_bytes(),
                        ],
                    );
                    if !reserved.contains_key(&candidate) && !persisted.contains(&candidate) {
                        break candidate;
                    }
                    counter = counter.checked_add(1).ok_or(ClaudeScannerError::Parse)?;
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

    fn source_key(&self, source: &DiscoveredClaudeSession) -> String {
        opaque_hex(
            &self.domain_key,
            SOURCE_KEY_DOMAIN,
            &[&path_bytes(&source.relative_path)],
        )
    }

    fn file_identity(&self, source: &DiscoveredClaudeSession) -> SessionFileIdentity {
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
    ) -> Result<Option<ExternalSessionTitle>, ClaudeScannerError> {
        let Some(state) = self.state.load_session_source(source_key)? else {
            return Ok(None);
        };
        if state.source_kind != SessionSourceKind::Claude {
            return Ok(None);
        }
        let Some(cursor) = self.state.load_current_session_scan_cursor(source_key)? else {
            return Ok(None);
        };
        let mut file = match open_source_for_paging(
            &self.root,
            &self.domain_key,
            &cursor,
            MAX_TITLE_SOURCE_WORK_BYTES,
        ) {
            Ok(file) => file,
            Err(ClaudeScannerError::Storage(error)) => {
                return Err(ClaudeScannerError::Storage(error));
            }
            Err(_) => return Ok(None),
        };
        read_claude_title(&mut file)
    }
}

fn validate_usage_storage_bounds(aggregate: &ClaudeAggregate) -> Result<(), ClaudeScannerError> {
    let maximum = i64::MAX as u64;
    if aggregate.usage().any(|(_, usage)| {
        usage.input > maximum
            || usage.output > maximum
            || usage.cache_read > maximum
            || usage.cache_write > maximum
    }) {
        Err(ClaudeScannerError::ResourceLimit)
    } else {
        Ok(())
    }
}

fn next_generation(current: Option<u64>) -> Result<u64, ClaudeScannerError> {
    current.map_or(Ok(1), |generation| {
        if generation >= i64::MAX as u64 {
            Err(ClaudeScannerError::ResourceLimit)
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

fn parser_checkpoint(emitted_usage: u64) -> ParserCheckpoint {
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
        structural_hash: None,
    }
}

pub(crate) fn open_source_for_paging(
    root: &SessionRootLease,
    domain_key: &[u8; 32],
    cursor: &SessionScanCursor,
    maximum_size: u64,
) -> Result<SessionFile, ClaudeScannerError> {
    let discovered = discover_claude_sessions(root, DiscoveryLimits::default())?;
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
    let mut file = target
        .ok_or(ClaudeScannerError::Read)?
        .open(root, maximum_size)
        .map_err(|error| match error {
            SessionError::ReadLimitExceeded => ClaudeScannerError::ResourceLimit,
            _ => ClaudeScannerError::Read,
        })?;
    let snapshot = file.snapshot();
    if snapshot.size != cursor.observed_size
        || system_time_utc(snapshot.modified) != cursor.modified_at
        || cursor.complete_byte_offset > snapshot.size
    {
        return Err(ClaudeScannerError::Parse);
    }
    let (head, boundary) = fingerprints(
        &mut file,
        cursor.complete_byte_offset,
        domain_key,
        FINGERPRINT_DOMAIN,
    )
    .map_err(map_jsonl_error)?;
    if head != cursor.head_fingerprint || boundary != cursor.boundary_fingerprint {
        return Err(ClaudeScannerError::Parse);
    }
    Ok(file)
}

fn discover_claude_sessions(
    root: &SessionRootLease,
    limits: DiscoveryLimits,
) -> Result<Vec<DiscoveredClaudeSession>, ClaudeScannerError> {
    let mut output = Vec::new();
    let mut identities = HashSet::new();
    let mut entry_budget = limits.maximum_total_entries;
    let mut directory_budget = MAX_CLAUDE_SUBAGENT_DIRECTORIES.min(limits.maximum_total_entries);
    let Some(projects) = optional_directory(root, Path::new("projects"))? else {
        return Ok(output);
    };
    for project in directory_entries(&projects, limits, &mut entry_budget)? {
        if project.snapshot().kind != SessionFileKind::Directory {
            continue;
        }
        let project_path = Path::new("projects").join(project.name());
        let Some(project_directory) = optional_directory(root, &project_path)? else {
            continue;
        };
        collect_jsonl(
            &project_directory,
            &project_path,
            limits,
            &mut entry_budget,
            &mut identities,
            &mut output,
        )?;
        for session in directory_entries(&project_directory, limits, &mut entry_budget)? {
            if session.snapshot().kind != SessionFileKind::Directory {
                continue;
            }
            let subagents_path = project_path.join(session.name()).join("subagents");
            let Some(subagents) = optional_directory(root, &subagents_path)? else {
                continue;
            };
            collect_subagent_jsonl_recursive(
                root,
                &subagents,
                &subagents_path,
                0,
                limits,
                &mut entry_budget,
                &mut directory_budget,
                &mut identities,
                &mut output,
            )?;
        }
    }
    output.sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(output)
}

#[allow(clippy::too_many_arguments)]
fn collect_subagent_jsonl_recursive(
    root: &SessionRootLease,
    directory: &SessionDirectoryLease,
    base: &Path,
    depth: usize,
    limits: DiscoveryLimits,
    entry_budget: &mut usize,
    directory_budget: &mut usize,
    identities: &mut HashSet<PlatformFileIdentity>,
    output: &mut Vec<DiscoveredClaudeSession>,
) -> Result<(), ClaudeScannerError> {
    if depth > MAX_CLAUDE_SUBAGENT_DIRECTORY_DEPTH {
        return Err(ClaudeScannerError::ResourceLimit);
    }
    *directory_budget = directory_budget
        .checked_sub(1)
        .ok_or(ClaudeScannerError::ResourceLimit)?;
    for entry in directory_entries(directory, limits, entry_budget)? {
        match entry.snapshot().kind {
            SessionFileKind::RegularFile if is_jsonl(entry.name()) => {
                if identities.insert(entry.snapshot().identity) {
                    if output.len() >= limits.maximum_total_sessions {
                        return Err(ClaudeScannerError::ResourceLimit);
                    }
                    output.push(DiscoveredClaudeSession {
                        relative_path: base.join(entry.name()),
                        identity: entry.snapshot().identity,
                    });
                }
            }
            SessionFileKind::Directory => {
                if depth == MAX_CLAUDE_SUBAGENT_DIRECTORY_DEPTH {
                    return Err(ClaudeScannerError::ResourceLimit);
                }
                let child_path = base.join(entry.name());
                let Some(child) = optional_directory(root, &child_path)? else {
                    continue;
                };
                collect_subagent_jsonl_recursive(
                    root,
                    &child,
                    &child_path,
                    depth + 1,
                    limits,
                    entry_budget,
                    directory_budget,
                    identities,
                    output,
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn optional_directory(
    root: &SessionRootLease,
    relative: &Path,
) -> Result<Option<SessionDirectoryLease>, ClaudeScannerError> {
    match root.open_directory(relative) {
        Ok(directory) => Ok(Some(directory)),
        Err(SessionError::Io { source }) if source.kind() == std::io::ErrorKind::NotFound => {
            Ok(None)
        }
        Err(_) => Err(ClaudeScannerError::Discovery),
    }
}

fn directory_entries(
    directory: &SessionDirectoryLease,
    limits: DiscoveryLimits,
    entry_budget: &mut usize,
) -> Result<Vec<wokcore_platform::sessions::SessionDirectoryEntry>, ClaudeScannerError> {
    let entries = directory
        .entries(limits.maximum_entries_per_directory)
        .map_err(|_| ClaudeScannerError::Discovery)?;
    *entry_budget = entry_budget
        .checked_sub(entries.len())
        .ok_or(ClaudeScannerError::ResourceLimit)?;
    Ok(entries)
}

fn collect_jsonl(
    directory: &SessionDirectoryLease,
    base: &Path,
    limits: DiscoveryLimits,
    entry_budget: &mut usize,
    identities: &mut HashSet<PlatformFileIdentity>,
    output: &mut Vec<DiscoveredClaudeSession>,
) -> Result<(), ClaudeScannerError> {
    for entry in directory_entries(directory, limits, entry_budget)? {
        if entry.snapshot().kind != SessionFileKind::RegularFile || !is_jsonl(entry.name()) {
            continue;
        }
        if identities.insert(entry.snapshot().identity) {
            if output.len() >= limits.maximum_total_sessions {
                return Err(ClaudeScannerError::ResourceLimit);
            }
            output.push(DiscoveredClaudeSession {
                relative_path: base.join(entry.name()),
                identity: entry.snapshot().identity,
            });
        }
    }
    Ok(())
}

fn is_jsonl(name: &OsStr) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
}

fn parse_claude_aggregate(
    file: &mut SessionFile,
    append_boundary: Option<u64>,
    domain_key: &[u8; 32],
) -> Result<ClaudeAggregate, ClaudeScannerError> {
    let mut reader = JsonlReader::new(JsonlCursor::new(0, 1));
    let mut messages = Vec::<ClaudeLogicalMessage>::new();
    let mut positions = HashMap::<[u8; 32], usize>::new();
    let mut session_id: Option<String> = None;
    let mut created_at: Option<String> = None;
    let mut last_active_at: Option<String> = None;
    let mut previous_byte_end = 0u64;
    let mut appended_replacement = false;
    let mut parser_read_bytes = 0u64;
    let mut peak_parser_buffer_bytes = 0usize;
    let mut peak_logical_working_bytes = 0usize;
    let mut logical_dynamic_bytes = 0usize;
    let (complete_byte_offset, record_ordinal) = loop {
        let scan = reader.scan(file).map_err(map_jsonl_error)?;
        parser_read_bytes = parser_read_bytes.saturating_add(scan.read_bytes);
        peak_parser_buffer_bytes = peak_parser_buffer_bytes.max(scan.peak_buffer_bytes);
        for record in &scan.records {
            let byte_start = previous_byte_end;
            previous_byte_end = record.byte_end;
            if record.status != JsonlRecordStatus::Valid {
                continue;
            }
            let Some(parsed) = parse_claude_message(record.value(), record.ordinal, byte_start)
            else {
                continue;
            };
            if let Some(found_session) = parsed.session_id.as_deref() {
                match &session_id {
                    Some(existing) if existing != found_session => {
                        return Err(ClaudeScannerError::Parse);
                    }
                    None => session_id = Some(found_session.to_owned()),
                    _ => {}
                }
            }
            created_at.get_or_insert_with(|| parsed.message.occurred_at.clone());
            last_active_at = maximum_timestamp(last_active_at, &parsed.message.occurred_at);
            let logical_key = opaque_hash(
                domain_key,
                LOGICAL_MESSAGE_DOMAIN,
                &[parsed.message.message_id.as_bytes()],
            );
            if let Some(&position) = positions.get(&logical_key) {
                if append_boundary.is_some_and(|boundary| byte_start >= boundary)
                    && messages[position].first_byte_start < append_boundary.unwrap()
                {
                    appended_replacement = true;
                }
                let mut replacement = parsed.message;
                replacement.first_byte_start = messages[position].first_byte_start;
                if replacement.usage.is_none() {
                    replacement.usage = messages[position].usage.clone();
                }
                logical_dynamic_bytes = logical_dynamic_bytes
                    .saturating_sub(claude_message_dynamic_bytes(&messages[position]))
                    .saturating_add(claude_message_dynamic_bytes(&replacement));
                messages[position] = replacement;
            } else {
                if messages.len() >= MAX_ACTIVE_MESSAGES {
                    return Err(ClaudeScannerError::ResourceLimit);
                }
                positions.insert(logical_key, messages.len());
                logical_dynamic_bytes = logical_dynamic_bytes
                    .saturating_add(claude_message_dynamic_bytes(&parsed.message));
                messages.push(parsed.message);
            }
            update_claude_working_peak(
                &messages,
                &positions,
                logical_dynamic_bytes,
                &session_id,
                &created_at,
                &last_active_at,
                &mut peak_logical_working_bytes,
            )?;
        }
        if scan.reached_end || scan.records.is_empty() {
            break (
                scan.complete_byte_offset,
                scan.next_record_ordinal.saturating_sub(1),
            );
        }
    };
    if complete_byte_offset != file.snapshot().size {
        return Err(ClaudeScannerError::Parse);
    }
    let session_id = session_id.ok_or(ClaudeScannerError::Parse)?;
    let created_at = created_at.unwrap_or_else(|| "1970-01-01T00:00:00Z".to_owned());
    let last_active_at = last_active_at.clone().unwrap_or_else(|| created_at.clone());
    let final_working_bytes = claude_logical_working_bytes(
        &messages,
        positions.capacity(),
        logical_dynamic_bytes,
        Some(&session_id),
        Some(&created_at),
        Some(&last_active_at),
    );
    peak_logical_working_bytes = peak_logical_working_bytes.max(final_working_bytes);
    if peak_logical_working_bytes > MAX_CLAUDE_LOGICAL_WORKING_BYTES {
        return Err(ClaudeScannerError::ResourceLimit);
    }
    let usage_count = u64::try_from(
        messages
            .iter()
            .filter(|message| message.usage.is_some())
            .count(),
    )
    .map_err(|_| ClaudeScannerError::ResourceLimit)?;
    let usage_count_inspections =
        u64::try_from(messages.len()).map_err(|_| ClaudeScannerError::ResourceLimit)?;
    Ok(ClaudeAggregate {
        session_id,
        created_at,
        last_active_at,
        messages,
        complete_byte_offset,
        record_ordinal,
        appended_replacement,
        parser_read_bytes,
        peak_parser_buffer_bytes,
        peak_logical_working_bytes,
        usage_count,
        usage_count_inspections,
    })
}

fn update_claude_working_peak(
    messages: &Vec<ClaudeLogicalMessage>,
    positions: &HashMap<[u8; 32], usize>,
    logical_dynamic_bytes: usize,
    session_id: &Option<String>,
    created_at: &Option<String>,
    last_active_at: &Option<String>,
    peak: &mut usize,
) -> Result<(), ClaudeScannerError> {
    let current = claude_logical_working_bytes(
        messages,
        positions.capacity(),
        logical_dynamic_bytes,
        session_id.as_ref(),
        created_at.as_ref(),
        last_active_at.as_ref(),
    );
    *peak = (*peak).max(current);
    if current > MAX_CLAUDE_LOGICAL_WORKING_BYTES {
        Err(ClaudeScannerError::ResourceLimit)
    } else {
        Ok(())
    }
}

fn claude_logical_working_bytes(
    messages: &Vec<ClaudeLogicalMessage>,
    position_capacity: usize,
    logical_dynamic_bytes: usize,
    session_id: Option<&String>,
    created_at: Option<&String>,
    last_active_at: Option<&String>,
) -> usize {
    const HASH_MAP_BUCKET_CONTROL_OVERHEAD: usize = 16;
    messages
        .capacity()
        .saturating_mul(std::mem::size_of::<ClaudeLogicalMessage>())
        .saturating_add(
            position_capacity.saturating_mul(
                std::mem::size_of::<([u8; 32], usize)>()
                    .saturating_add(HASH_MAP_BUCKET_CONTROL_OVERHEAD),
            ),
        )
        .saturating_add(logical_dynamic_bytes)
        .saturating_add(session_id.map_or(0, String::capacity))
        .saturating_add(created_at.map_or(0, String::capacity))
        .saturating_add(last_active_at.map_or(0, String::capacity))
}

fn claude_message_dynamic_bytes(message: &ClaudeLogicalMessage) -> usize {
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

struct ParsedClaudeMessage {
    session_id: Option<String>,
    message: ClaudeLogicalMessage,
}

fn parse_claude_message(
    value: &Value,
    revision: u64,
    byte_start: u64,
) -> Option<ParsedClaudeMessage> {
    let object = value.as_object()?;
    if !is_visible_message_record(value) {
        return None;
    }
    let record_type = object.get("type")?.as_str()?;
    let message = object.get("message")?.as_object()?;
    let occurred_at = normalize_timestamp(object.get("timestamp")?)?;
    let session_id = object
        .get("sessionId")
        .and_then(Value::as_str)
        .and_then(normalize_external_id);
    let fallback_id = format!("{record_type}:{revision}");
    let message_id = if record_type == "assistant" {
        message
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| object.get("uuid").and_then(Value::as_str))
    } else {
        object
            .get("uuid")
            .and_then(Value::as_str)
            .or_else(|| message.get("id").and_then(Value::as_str))
    }
    .and_then(normalize_external_id)
    .unwrap_or(fallback_id);
    if message_id.len() > EXTERNAL_ID_LIMIT_BYTES {
        return None;
    }
    let usage = if record_type == "assistant" {
        message
            .get("usage")
            .and_then(Value::as_object)
            .and_then(|usage| {
                let input = optional_u64(usage, "input_tokens")?;
                let output = optional_u64(usage, "output_tokens")?;
                let cache_read = optional_u64(usage, "cache_read_input_tokens")?;
                let cache_write = optional_u64(usage, "cache_creation_input_tokens")?;
                Some(ClaudeUsage {
                    model: normalize_external_model(message.get("model").and_then(Value::as_str)),
                    occurred_at: occurred_at.clone(),
                    input,
                    output,
                    cache_read,
                    cache_write,
                    revision,
                })
                .filter(|usage| {
                    usage.input != 0
                        || usage.output != 0
                        || usage.cache_read != 0
                        || usage.cache_write != 0
                })
            })
    } else {
        None
    };
    Some(ParsedClaudeMessage {
        session_id,
        message: ClaudeLogicalMessage {
            message_id,
            occurred_at,
            usage,
            first_byte_start: byte_start,
        },
    })
}

pub(crate) fn is_visible_message_record(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if !matches!(
        object.get("type").and_then(Value::as_str),
        Some("user" | "assistant")
    ) {
        return false;
    }
    !["isMeta", "isSidechain", "teamName"]
        .iter()
        .any(|field| object.get(*field).is_some_and(json_truthy))
}

fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn optional_u64(object: &Map<String, Value>, name: &str) -> Option<u64> {
    object.get(name).map_or(Some(0), Value::as_u64)
}

fn read_claude_title(
    file: &mut SessionFile,
) -> Result<Option<ExternalSessionTitle>, ClaudeScannerError> {
    let mut reader = JsonlReader::new(JsonlCursor::new(0, 1));
    let mut first_user = None;
    let mut summary = None;
    let mut last_prompt = None;
    let mut ai_title = None;
    let mut custom_title = None;
    loop {
        let scan = reader.scan(file).map_err(map_jsonl_error)?;
        let no_records = scan.records.is_empty();
        for record in &scan.records {
            if record.status != JsonlRecordStatus::Valid {
                continue;
            }
            let Some(object) = record.value().as_object() else {
                continue;
            };
            if matches!(
                object.get("type").and_then(Value::as_str),
                Some("user" | "assistant")
            ) && !is_visible_message_record(record.value())
            {
                continue;
            }
            for (field, target) in [
                ("summary", &mut summary),
                ("lastPrompt", &mut last_prompt),
                ("aiTitle", &mut ai_title),
                ("customTitle", &mut custom_title),
            ] {
                if let Some(title) = object
                    .get(field)
                    .and_then(Value::as_str)
                    .and_then(ExternalSessionTitle::from_str)
                {
                    *target = Some(title);
                }
            }
            if first_user.is_none() && object.get("type").and_then(Value::as_str) == Some("user") {
                first_user = object
                    .get("message")
                    .and_then(Value::as_object)
                    .and_then(|message| message.get("content"))
                    .and_then(text_content)
                    .and_then(ExternalSessionTitle::from_str);
            }
        }
        if scan.reached_end || no_records {
            return Ok(custom_title
                .or(ai_title)
                .or(last_prompt)
                .or(summary)
                .or(first_user));
        }
    }
}

fn text_content(value: &Value) -> Option<&str> {
    match value {
        Value::String(value) => Some(value),
        Value::Array(items) => items.iter().find_map(|item| {
            let object = item.as_object()?;
            (object.get("type")?.as_str()? == "text")
                .then(|| object.get("text")?.as_str())
                .flatten()
        }),
        _ => None,
    }
}

fn map_jsonl_error(error: JsonlError) -> ClaudeScannerError {
    match error {
        JsonlError::RecordTooLarge { .. } => ClaudeScannerError::ResourceLimit,
        JsonlError::SourceChanged
        | JsonlError::SourceUnavailable
        | JsonlError::ReadFailed
        | JsonlError::CursorOverflow => ClaudeScannerError::Read,
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
    use super::{ClaudeScannerError, next_generation};

    #[test]
    fn generation_overflow_is_rejected() {
        assert_eq!(next_generation(None).unwrap(), 1);
        assert_eq!(next_generation(Some(41)).unwrap(), 42);
        assert!(matches!(
            next_generation(Some(i64::MAX as u64)),
            Err(ClaudeScannerError::ResourceLimit)
        ));
        assert!(matches!(
            next_generation(Some(u64::MAX)),
            Err(ClaudeScannerError::ResourceLimit)
        ));
    }
}
