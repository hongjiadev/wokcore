use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fmt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering as AtomicOrdering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OpenFlags, OptionalExtension, limits::Limit};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use wokcore_platform::sessions::{
    SessionError, SessionFile, SessionFileIdentity as PlatformFileIdentity, SessionRootLease,
};
use wokcore_storage::{
    CandidateBeginOutcome, CodexReplaySignature, CodexReplaySignaturePage,
    MAX_CODEX_REPLAY_SIGNATURES, MAX_SESSION_BATCH_BYTES, MAX_SESSION_BATCH_ROWS, ParserCheckpoint,
    ReplaySignaturePageKey, SessionAvailability, SessionBatch, SessionFileIdentity,
    SessionGenerationState, SessionIndexRecord, SessionScanCursor, SessionScanResultCode,
    SessionSourceErrorCode, SessionSourceKind, SessionSourcePageKey, SessionSourceStatus,
    SessionUsageRecord, StateStore, StateStoreWriterClient, StorageError,
};

use crate::{
    cursor::{JsonlCursor, JsonlError, JsonlReader, JsonlRecordStatus, MAX_JSONL_LINE_BYTES},
    discovery::{
        DiscoveredSession, DiscoveryError, DiscoveryLimits, SessionDiscoveryClock,
        SessionDiscoveryCursor, SessionDiscoveryKind, SessionDiscoverySliceBudget,
        SessionDiscoverySliceError, SessionDiscoverySliceOutcome, SessionLocation,
        SystemSessionDiscoveryClock, discover_codex_sessions,
        discover_codex_sessions_slice_with_clock,
    },
    model::{ReplayResolution, TokenTotals},
    state::SessionState,
};

pub const REPLAY_PAGE_SIZE: usize = 512;
const REPLAY_GROUP_PAGE_SIZE: usize = 400;
const MAX_REPLAY_CHILDREN_PER_PARENT: usize = 4_096;
const MAX_REPLAY_GROUP_WORKING_BYTES: usize = 256 * 1024;
const PARSER_CHECKPOINT_VERSION: u16 = 2;
const FINGERPRINT_WINDOW_BYTES: usize = 4 * 1024;
const HEAD_FINGERPRINT_BYTES: usize = 64;
const TITLE_INDEX_LIMIT_BYTES: u64 = 4 * 1024 * 1024;
const TITLE_DATABASE_LIMIT_BYTES: u64 = 64 * 1024 * 1024;
const TITLE_LIMIT_BYTES: usize = 512;
const TITLE_ROW_LIMIT: usize = 4_096;
const TITLE_SQLITE_LENGTH_LIMIT_BYTES: i32 = 1024 * 1024;
const TITLE_SQLITE_PLAN_ROW_LIMIT: usize = 8;
const TITLE_SQLITE_VM_BUDGET: usize = 20_000;
const TITLE_SQLITE_VM_GRANULARITY: usize = 100;
const SESSION_BATCH_ROW_TARGET: usize = 384;
const THREAD_ID_LIMIT_BYTES: usize = 512;
const MODEL_LIMIT_BYTES: usize = 256;
const METADATA_PROBE_RECORDS: usize = 64;

#[derive(Clone, Eq, PartialEq)]
pub struct CodexSessionMeta {
    pub root_thread_id: String,
    pub created_at: String,
    pub parent_thread_id: Option<String>,
}

impl fmt::Debug for CodexSessionMeta {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexSessionMeta")
            .field("has_parent", &self.parent_thread_id.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexTurnContext {
    pub model: String,
    pub occurred_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexTokenCount {
    pub occurred_at: String,
    pub total: Option<TokenTotals>,
    pub last: Option<TokenTotals>,
    pub model: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexResponseItem {
    pub occurred_at: Option<String>,
    pub is_message: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CodexStructuralRecord {
    SessionMeta(CodexSessionMeta),
    TurnContext(CodexTurnContext),
    TokenCount(CodexTokenCount),
    ResponseItem(CodexResponseItem),
    Unknown,
}

#[derive(Debug, thiserror::Error)]
pub enum CodexError {
    #[error("Codex Session metadata is structurally inconsistent")]
    ParentInconsistent,
    #[error("Codex Session record is structurally invalid")]
    InvalidRecord,
    #[error("Codex Session timestamp is invalid")]
    InvalidTimestamp,
}

impl CodexError {
    pub fn stable_code(&self) -> &'static str {
        match self {
            Self::ParentInconsistent => "codex_parent_inconsistent",
            Self::InvalidRecord => "codex_record_invalid",
            Self::InvalidTimestamp => "codex_timestamp_invalid",
        }
    }
}

pub fn parse_codex_record(record: &Value) -> Result<CodexStructuralRecord, CodexError> {
    let object = record.as_object().ok_or(CodexError::InvalidRecord)?;
    let record_type = object.get("type").and_then(Value::as_str).unwrap_or("");
    let payload = object.get("payload").and_then(Value::as_object);
    match record_type {
        "session_meta" => {
            let payload = payload.ok_or(CodexError::InvalidRecord)?;
            let root_thread_id =
                consistent_strings(payload, &["id", "session_id", "thread_id", "threadId"])?
                    .ok_or(CodexError::InvalidRecord)?;
            let parent_thread_id = consistent_parent(payload)?;
            let timestamp = object
                .get("timestamp")
                .or_else(|| payload.get("timestamp"))
                .ok_or(CodexError::InvalidTimestamp)?;
            Ok(CodexStructuralRecord::SessionMeta(CodexSessionMeta {
                root_thread_id,
                created_at: parse_timestamp_utc(timestamp)?,
                parent_thread_id,
            }))
        }
        "turn_context" => {
            let payload = payload.ok_or(CodexError::InvalidRecord)?;
            let mut models = BTreeSet::new();
            push_optional_string(payload, "model", &mut models)?;
            if let Some(info) = optional_object(payload, "info")? {
                push_optional_string(info, "model", &mut models)?;
            }
            if models.len() > 1 || models.iter().any(|model| model.len() > MODEL_LIMIT_BYTES) {
                return Err(CodexError::InvalidRecord);
            }
            let model = models
                .into_iter()
                .next()
                .map(normalize_model)
                .unwrap_or_else(|| "unknown".to_owned());
            let occurred_at = object
                .get("timestamp")
                .or_else(|| payload.get("timestamp"))
                .map(parse_timestamp_utc)
                .transpose()?;
            Ok(CodexStructuralRecord::TurnContext(CodexTurnContext {
                model,
                occurred_at,
            }))
        }
        "event_msg"
            if payload
                .and_then(|payload| payload.get("type"))
                .and_then(Value::as_str)
                == Some("token_count") =>
        {
            let payload = payload.ok_or(CodexError::InvalidRecord)?;
            let info = optional_object(payload, "info")?;
            let total = info
                .map(|info| optional_object(info, "total_token_usage"))
                .transpose()?
                .flatten()
                .map(parse_token_totals)
                .transpose()?;
            let last = info
                .map(|info| optional_object(info, "last_token_usage"))
                .transpose()?
                .flatten()
                .map(parse_token_totals)
                .transpose()?;
            let mut models = BTreeSet::new();
            push_optional_string(payload, "model", &mut models)?;
            if let Some(info) = info {
                push_optional_string(info, "model", &mut models)?;
                push_optional_string(info, "model_name", &mut models)?;
            }
            if models.len() > 1 || models.iter().any(|model| model.len() > MODEL_LIMIT_BYTES) {
                return Err(CodexError::InvalidRecord);
            }
            let model = models.into_iter().next().map(normalize_model);
            let occurred_at = object
                .get("timestamp")
                .or_else(|| payload.get("timestamp"))
                .ok_or(CodexError::InvalidTimestamp)
                .and_then(parse_timestamp_utc)?;
            Ok(CodexStructuralRecord::TokenCount(CodexTokenCount {
                occurred_at,
                total,
                last,
                model,
            }))
        }
        "response_item" => {
            let payload = payload.ok_or(CodexError::InvalidRecord)?;
            let item_type = match payload.get("type") {
                Some(Value::String(item_type)) => Some(item_type.as_str()),
                Some(_) => return Err(CodexError::InvalidRecord),
                None => None,
            };
            let occurred_at = object
                .get("timestamp")
                .or_else(|| payload.get("timestamp"))
                .map(parse_timestamp_utc)
                .transpose()?;
            Ok(CodexStructuralRecord::ResponseItem(CodexResponseItem {
                occurred_at,
                is_message: item_type == Some("message"),
            }))
        }
        _ => Ok(CodexStructuralRecord::Unknown),
    }
}

fn consistent_strings(
    object: &Map<String, Value>,
    names: &[&str],
) -> Result<Option<String>, CodexError> {
    let mut values = BTreeSet::new();
    for name in names {
        push_optional_string(object, name, &mut values)?;
    }
    if values.len() > 1
        || values
            .iter()
            .any(|value| value.len() > THREAD_ID_LIMIT_BYTES)
    {
        return Err(CodexError::InvalidRecord);
    }
    Ok(values.into_iter().next().map(ToOwned::to_owned))
}

fn consistent_parent(payload: &Map<String, Value>) -> Result<Option<String>, CodexError> {
    let mut values = BTreeSet::new();
    for name in ["forked_from_id", "parent_thread_id"] {
        push_optional_string(payload, name, &mut values)?;
    }
    if let Some(legacy) = legacy_parent(payload)? {
        values.insert(legacy);
    }
    if values.len() > 1
        || values
            .iter()
            .any(|value| value.len() > THREAD_ID_LIMIT_BYTES)
    {
        return Err(CodexError::ParentInconsistent);
    }
    Ok(values.into_iter().next().map(ToOwned::to_owned))
}

fn legacy_parent(payload: &Map<String, Value>) -> Result<Option<&str>, CodexError> {
    let Some(Value::Object(source)) = payload.get("source") else {
        return Ok(None);
    };
    let Some(subagent) = source.get("subagent") else {
        return Ok(None);
    };
    let subagent = subagent.as_object().ok_or(CodexError::ParentInconsistent)?;
    let Some(spawn) = subagent.get("thread_spawn") else {
        return Ok(None);
    };
    let spawn = spawn.as_object().ok_or(CodexError::ParentInconsistent)?;
    let Some(parent) = spawn.get("parent_thread_id") else {
        return Ok(None);
    };
    let parent = parent.as_str().ok_or(CodexError::ParentInconsistent)?;
    if parent.is_empty() || parent.len() > THREAD_ID_LIMIT_BYTES {
        return Err(CodexError::ParentInconsistent);
    }
    Ok(Some(parent))
}

fn push_optional_string<'a>(
    object: &'a Map<String, Value>,
    name: &str,
    output: &mut BTreeSet<&'a str>,
) -> Result<(), CodexError> {
    let Some(value) = object.get(name) else {
        return Ok(());
    };
    let value = value.as_str().ok_or(CodexError::InvalidRecord)?;
    if value.is_empty() {
        return Err(CodexError::InvalidRecord);
    }
    output.insert(value);
    Ok(())
}

fn optional_object<'a>(
    object: &'a Map<String, Value>,
    name: &str,
) -> Result<Option<&'a Map<String, Value>>, CodexError> {
    object
        .get(name)
        .map(|value| value.as_object().ok_or(CodexError::InvalidRecord))
        .transpose()
}

fn parse_token_totals(object: &Map<String, Value>) -> Result<TokenTotals, CodexError> {
    let number = |names: &[&str]| -> Result<u64, CodexError> {
        for name in names {
            if let Some(value) = object.get(*name) {
                return value.as_u64().ok_or(CodexError::InvalidRecord);
            }
        }
        Ok(0)
    };
    Ok(TokenTotals {
        input: number(&["input_tokens"])?,
        output: number(&["output_tokens"])?,
        cache_read: number(&["cached_input_tokens", "cache_read_input_tokens"])?,
        cache_write: number(&["cache_write_input_tokens", "cache_creation_input_tokens"])?,
        reasoning: number(&["reasoning_output_tokens", "reasoning_tokens"])?,
    }
    .clamp_cache())
}

pub fn normalize_model(value: &str) -> String {
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

pub fn parse_timestamp_utc(value: &Value) -> Result<String, CodexError> {
    let seconds = match value {
        Value::String(value) => parse_rfc3339_seconds(value)?,
        Value::Number(value) => {
            let raw = value.as_i64().ok_or(CodexError::InvalidTimestamp)?;
            normalize_integer_epoch(raw)
        }
        _ => return Err(CodexError::InvalidTimestamp),
    };
    format_epoch_seconds(seconds)
}

fn normalize_integer_epoch(raw: i64) -> i64 {
    let magnitude = raw.unsigned_abs();
    if magnitude < 100_000_000_000 {
        raw
    } else if magnitude < 100_000_000_000_000 {
        raw / 1_000
    } else if magnitude < 100_000_000_000_000_000 {
        raw / 1_000_000
    } else {
        raw / 1_000_000_000
    }
}

fn parse_rfc3339_seconds(value: &str) -> Result<i64, CodexError> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return Err(CodexError::InvalidTimestamp);
    }
    let year = parse_digits(bytes, 0, 4)? as i32;
    let month = parse_digits(bytes, 5, 2)?;
    let day = parse_digits(bytes, 8, 2)?;
    let hour = parse_digits(bytes, 11, 2)? as i64;
    let minute = parse_digits(bytes, 14, 2)? as i64;
    let second = parse_digits(bytes, 17, 2)? as i64;
    if !valid_date(year, month, day) || hour > 23 || minute > 59 || second > 59 {
        return Err(CodexError::InvalidTimestamp);
    }
    let mut timezone_index = 19;
    if bytes.get(timezone_index) == Some(&b'.') {
        timezone_index += 1;
        let fraction_start = timezone_index;
        while bytes.get(timezone_index).is_some_and(u8::is_ascii_digit) {
            timezone_index += 1;
        }
        if timezone_index == fraction_start {
            return Err(CodexError::InvalidTimestamp);
        }
    }
    let offset = match bytes.get(timezone_index) {
        Some(b'Z') if timezone_index + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-'))
            if timezone_index + 6 == bytes.len()
                && bytes.get(timezone_index + 3) == Some(&b':') =>
        {
            let offset_hour = parse_digits(bytes, timezone_index + 1, 2)? as i64;
            let offset_minute = parse_digits(bytes, timezone_index + 4, 2)? as i64;
            if offset_hour > 23 || offset_minute > 59 {
                return Err(CodexError::InvalidTimestamp);
            }
            let seconds = offset_hour * 3_600 + offset_minute * 60;
            if *sign == b'+' { seconds } else { -seconds }
        }
        _ => return Err(CodexError::InvalidTimestamp),
    };
    let days = days_from_civil(year, month, day);
    days.checked_mul(86_400)
        .and_then(|base| base.checked_add(hour * 3_600 + minute * 60 + second))
        .and_then(|local| local.checked_sub(offset))
        .ok_or(CodexError::InvalidTimestamp)
}

fn parse_digits(bytes: &[u8], start: usize, length: usize) -> Result<u32, CodexError> {
    let slice = bytes
        .get(start..start + length)
        .ok_or(CodexError::InvalidTimestamp)?;
    if !slice.iter().all(u8::is_ascii_digit) {
        return Err(CodexError::InvalidTimestamp);
    }
    Ok(slice
        .iter()
        .fold(0, |value, byte| value * 10 + u32::from(byte - b'0')))
}

fn valid_date(year: i32, month: u32, day: u32) -> bool {
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let maximum = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    year >= 1 && day >= 1 && day <= maximum
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = i64::from(year) - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let adjusted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn format_epoch_seconds(seconds: i64) -> Result<String, CodexError> {
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    if !(1..=9999).contains(&year) {
        return Err(CodexError::InvalidTimestamp);
    }
    let hour = second_of_day / 3_600;
    let minute = second_of_day % 3_600 / 60;
    let second = second_of_day % 60;
    Ok(format!(
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScanControl {
    pub stop_after_committed_batches: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanOutcome {
    Complete,
    Interrupted,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ScannerMetrics {
    pub source_opens: u64,
    pub metadata_probe_bytes: u64,
    pub parser_read_bytes: u64,
    pub full_source_scans: u64,
    pub replay_child_scans: u64,
    pub parent_index_builds: u64,
    pub replay_pages_loaded: u64,
    pub maximum_replay_page_rows: usize,
    pub maximum_replay_group_working_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceScanSummary {
    pub source_key: String,
    pub session_key: Option<String>,
    pub status: SessionSourceStatus,
    pub error_code: Option<SessionSourceErrorCode>,
    pub replay_resolution: ReplayResolution,
    pub complete_byte_offset: u64,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SessionTitle(String);

impl SessionTitle {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SessionTitle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionTitle(<redacted>)")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexScanSummary {
    pub outcome: ScanOutcome,
    pub advanced_sources: usize,
    pub unchanged_sources: usize,
    pub deleted_sources: usize,
    pub sources: Vec<SourceScanSummary>,
    pub metrics: ScannerMetrics,
}

#[derive(Debug, thiserror::Error)]
pub enum CodexScannerError {
    #[error("Codex Session storage failed")]
    Storage(#[from] StorageError),
    #[error("Codex Session discovery failed")]
    Discovery(#[from] DiscoveryError),
    #[error("Codex Session root is unavailable")]
    Root,
    #[error("Codex Session read failed")]
    Read,
    #[error("Codex Session record failed structural validation")]
    Parse,
    #[error("Codex replay prefix is inconsistent with its parent")]
    ReplayInconsistent,
    #[error("Codex Session record exceeds its resource bound")]
    RecordTooLarge,
    #[error("Codex replay history exceeds its resource bound")]
    ReplayLimit,
    #[error("Codex Session generation cleanup is still pending")]
    CleanupPending,
}

pub struct CodexScanner {
    root: SessionRootLease,
    root_path: PathBuf,
    state: SessionState,
    domain_key: [u8; 32],
    discovery_limits: DiscoveryLimits,
    replay_limit: u64,
    title_database_identities: HashMap<String, PlatformFileIdentity>,
    metrics: ScannerMetrics,
    slice_cycle: Option<CodexSliceCycle>,
}

impl CodexScanner {
    pub fn open(
        root_path: impl AsRef<Path>,
        state_path: impl AsRef<Path>,
        domain_key: [u8; 32],
    ) -> Result<Self, CodexScannerError> {
        Self::open_internal(root_path, state_path, domain_key, None)
    }

    pub fn open_with_writer(
        root_path: impl AsRef<Path>,
        state_path: impl AsRef<Path>,
        domain_key: [u8; 32],
        writer: StateStoreWriterClient,
    ) -> Result<Self, CodexScannerError> {
        Self::open_internal(root_path, state_path, domain_key, Some(writer))
    }

    fn open_internal(
        root_path: impl AsRef<Path>,
        state_path: impl AsRef<Path>,
        domain_key: [u8; 32],
        writer: Option<StateStoreWriterClient>,
    ) -> Result<Self, CodexScannerError> {
        let root_path = root_path.as_ref().to_path_buf();
        let root = SessionRootLease::open(&root_path).map_err(|_| CodexScannerError::Root)?;
        let state = SessionState::open(state_path, writer)?;
        Ok(Self {
            root,
            root_path,
            state,
            domain_key,
            discovery_limits: DiscoveryLimits::default(),
            replay_limit: MAX_CODEX_REPLAY_SIGNATURES,
            title_database_identities: HashMap::new(),
            metrics: ScannerMetrics::default(),
            slice_cycle: None,
        })
    }

    pub fn state(&self) -> &StateStore {
        self.state.reader()
    }

    pub fn scan_slice(
        &mut self,
        transition_at: &str,
        control: ScanControl,
        budget: SessionDiscoverySliceBudget,
    ) -> Result<CodexScanSummary, CodexScannerError> {
        self.scan_slice_with_clock(transition_at, control, budget, &SystemSessionDiscoveryClock)
    }

    pub fn scan_slice_with_clock<C>(
        &mut self,
        transition_at: &str,
        control: ScanControl,
        budget: SessionDiscoverySliceBudget,
        clock: &C,
    ) -> Result<CodexScanSummary, CodexScannerError>
    where
        C: SessionDiscoveryClock + ?Sized,
    {
        self.metrics = ScannerMetrics::default();
        let mut cycle = match self.slice_cycle.take() {
            Some(cycle) => cycle,
            None => CodexSliceCycle::new(self, transition_at)?,
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
        cycle: &mut CodexSliceCycle,
        control: ScanControl,
        budget: SessionDiscoverySliceBudget,
        clock: &C,
    ) -> Result<(CodexScanSummary, bool), CodexScannerError>
    where
        C: SessionDiscoveryClock + ?Sized,
    {
        match cycle.phase {
            CodexScannerSlicePhase::Discovering => {
                let slice = discover_codex_sessions_slice_with_clock(
                    &self.root,
                    &mut cycle.cursor,
                    budget,
                    clock,
                )
                .map_err(map_codex_slice_error)?;
                for entry in &slice.entries {
                    let Some(source) = DiscoveredSession::from_slice(entry) else {
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
                    cycle.prepare_processing();
                    cycle.cursor = SessionDiscoveryCursor::with_limits(
                        SessionDiscoveryKind::Codex,
                        self.discovery_limits,
                    )
                    .map_err(CodexScannerError::Discovery)?;
                    cycle.persisted_sources = HashSet::new();
                    cycle.persisted_identities = HashMap::new();
                    cycle.reserved_keys = HashMap::new();
                    cycle.current_sources = HashSet::new();
                }
                Ok((
                    CodexScanSummary {
                        outcome: ScanOutcome::Interrupted,
                        advanced_sources: 0,
                        unchanged_sources: 0,
                        deleted_sources,
                        sources: Vec::new(),
                        metrics: self.metrics.clone(),
                    },
                    false,
                ))
            }
            CodexScannerSlicePhase::Processing => {
                if cycle.processing_depths.is_empty() {
                    return Ok((
                        CodexScanSummary {
                            outcome: ScanOutcome::Complete,
                            advanced_sources: 0,
                            unchanged_sources: 0,
                            deleted_sources: 0,
                            sources: Vec::new(),
                            metrics: self.metrics.clone(),
                        },
                        true,
                    ));
                }
                let slice = discover_codex_sessions_slice_with_clock(
                    &self.root,
                    &mut cycle.cursor,
                    budget,
                    clock,
                )
                .map_err(map_codex_slice_error)?;
                let current_depth = cycle.processing_depths[cycle.processing_depth_index];
                let mut summaries = Vec::new();
                let mut advanced_sources = 0;
                let mut unchanged_sources = 0;
                let mut committed_batches = 0;
                let mut restart_processing = false;
                for entry in &slice.entries {
                    let Some(discovered) = DiscoveredSession::from_slice(entry) else {
                        continue;
                    };
                    let identity = opaque_file_identity(&self.domain_key, discovered.identity());
                    let Some(&work_index) = cycle.identity_to_index.get(&identity) else {
                        continue;
                    };
                    if cycle.processed_indices.contains(&work_index)
                        || cycle.replay_topology.depths[work_index] != current_depth
                    {
                        continue;
                    }
                    let (summary, process) = self.process_codex_slice_source(
                        cycle,
                        work_index,
                        &discovered,
                        control,
                        &mut committed_batches,
                    )?;
                    match process {
                        Some(SourceProcessOutcome::Advanced) => advanced_sources += 1,
                        Some(SourceProcessOutcome::Unchanged) => unchanged_sources += 1,
                        Some(SourceProcessOutcome::Interrupted) => restart_processing = true,
                        Some(SourceProcessOutcome::CleanupPending) => {
                            cycle.needs_rescan = true;
                        }
                        Some(SourceProcessOutcome::Failed) | None => {}
                    }
                    summaries.push(summary);
                    if restart_processing {
                        break;
                    }
                    cycle.processed_indices.insert(work_index);
                }
                if restart_processing {
                    cycle.cursor = SessionDiscoveryCursor::with_limits(
                        SessionDiscoveryKind::Codex,
                        self.discovery_limits,
                    )
                    .map_err(CodexScannerError::Discovery)?;
                }

                let mut complete = false;
                if slice.outcome == SessionDiscoverySliceOutcome::Complete && !restart_processing {
                    cycle.processing_depth_index += 1;
                    if cycle.processing_depth_index == cycle.processing_depths.len() {
                        complete = true;
                    } else {
                        cycle.cursor = SessionDiscoveryCursor::with_limits(
                            SessionDiscoveryKind::Codex,
                            self.discovery_limits,
                        )
                        .map_err(CodexScannerError::Discovery)?;
                    }
                }
                let outcome = if complete && !cycle.needs_rescan {
                    ScanOutcome::Complete
                } else {
                    ScanOutcome::Interrupted
                };
                Ok((
                    CodexScanSummary {
                        outcome,
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

    fn process_codex_slice_source(
        &mut self,
        cycle: &mut CodexSliceCycle,
        work_index: usize,
        discovered_source: &DiscoveredSession,
        control: ScanControl,
        committed_batches: &mut usize,
    ) -> Result<(SourceScanSummary, Option<SourceProcessOutcome>), CodexScannerError> {
        let source = &cycle.inspected[work_index];
        let Some(metadata) = source.metadata.as_ref() else {
            let error = source
                .inspection_error
                .as_ref()
                .expect("failed inspection has an error");
            let (code, default_status) = match error {
                SourceInspectionError::RecordTooLarge => (
                    SessionSourceErrorCode::SourceRecordTooLarge,
                    SessionSourceStatus::ResourceLimited,
                ),
                SourceInspectionError::Read => (
                    SessionSourceErrorCode::SourceIoFailed,
                    SessionSourceStatus::Unavailable,
                ),
                SourceInspectionError::Parse => (
                    SessionSourceErrorCode::SourceParseInvalid,
                    SessionSourceStatus::Unavailable,
                ),
            };
            self.record_source_failure_if_possible(
                source,
                discovered_source,
                code,
                &cycle.transition_at,
            )?;
            let status = self
                .state
                .load_session_source(&source.source_key)?
                .map_or(default_status, |source| source.status);
            return Ok((
                SourceScanSummary {
                    source_key: source.source_key.clone(),
                    session_key: None,
                    status,
                    error_code: Some(code),
                    replay_resolution: ReplayResolution::NotForked,
                    complete_byte_offset: 0,
                },
                None,
            ));
        };

        let singleton = [work_index];
        let replay_group_indices =
            metadata
                .parent_thread_id
                .as_ref()
                .map_or(singleton.as_slice(), |parent_thread_id| {
                    cycle
                        .replay_groups
                        .get(&(
                            cycle.replay_topology.depths[work_index],
                            parent_thread_id.clone(),
                        ))
                        .map_or(singleton.as_slice(), Vec::as_slice)
                });
        let replay_result = if let Some(code) = cycle.replay_topology.errors[work_index] {
            Ok(ReplayState::deferred(code))
        } else {
            self.resolve_replay(
                work_index,
                metadata,
                &cycle.inspected,
                &cycle.thread_sources,
                replay_group_indices,
                &mut cycle.replay_cache,
            )
        };
        let replay = match replay_result {
            Ok(replay) => replay,
            Err(error) => {
                let (code, default_status) = match error {
                    CodexScannerError::Read => (
                        SessionSourceErrorCode::SourceIoFailed,
                        SessionSourceStatus::Unavailable,
                    ),
                    CodexScannerError::Parse | CodexScannerError::ReplayInconsistent => (
                        SessionSourceErrorCode::SourceReplayInconsistent,
                        SessionSourceStatus::Unavailable,
                    ),
                    CodexScannerError::RecordTooLarge => (
                        SessionSourceErrorCode::SourceRecordTooLarge,
                        SessionSourceStatus::ResourceLimited,
                    ),
                    CodexScannerError::ReplayLimit => (
                        SessionSourceErrorCode::SourceReplayLimit,
                        SessionSourceStatus::ResourceLimited,
                    ),
                    error => return Err(error),
                };
                self.record_source_failure_if_possible(
                    source,
                    discovered_source,
                    code,
                    &cycle.transition_at,
                )?;
                let status = self
                    .state
                    .load_session_source(&source.source_key)?
                    .map_or(default_status, |source| source.status);
                return Ok((
                    SourceScanSummary {
                        source_key: source.source_key.clone(),
                        session_key: Some(session_key(&self.domain_key, &metadata.root_thread_id)),
                        status,
                        error_code: Some(code),
                        replay_resolution: ReplayResolution::Deferred(code),
                        complete_byte_offset: self
                            .state
                            .load_current_session_scan_cursor(&source.source_key)?
                            .map_or(0, |cursor| cursor.complete_byte_offset),
                    },
                    None,
                ));
            }
        };
        if let ReplayResolution::Deferred(code) = replay.resolution {
            self.record_source_failure_if_possible(
                source,
                discovered_source,
                code,
                &cycle.transition_at,
            )?;
            let persisted = self.state.load_session_source(&source.source_key)?;
            let status = persisted.as_ref().map_or_else(
                || {
                    if code == SessionSourceErrorCode::SourceReplayLimit {
                        SessionSourceStatus::ResourceLimited
                    } else {
                        SessionSourceStatus::Unavailable
                    }
                },
                |source| source.status,
            );
            return Ok((
                SourceScanSummary {
                    source_key: source.source_key.clone(),
                    session_key: Some(session_key(&self.domain_key, &metadata.root_thread_id)),
                    status,
                    error_code: Some(code),
                    replay_resolution: ReplayResolution::Deferred(code),
                    complete_byte_offset: self
                        .state
                        .load_current_session_scan_cursor(&source.source_key)?
                        .map_or(0, |cursor| cursor.complete_byte_offset),
                },
                None,
            ));
        }

        let (process, process_error) = match self.process_source(
            source,
            discovered_source,
            metadata,
            &replay,
            &cycle.transition_at,
            control,
            committed_batches,
            cycle.referenced_parent_indices.contains(&work_index),
        ) {
            Ok(process) => {
                let code = match process.outcome {
                    SourceProcessOutcome::Interrupted | SourceProcessOutcome::CleanupPending => {
                        Some(SessionSourceErrorCode::SourceCandidateInterrupted)
                    }
                    _ if process.status == SessionSourceStatus::ResourceLimited => {
                        Some(SessionSourceErrorCode::SourceReplayLimit)
                    }
                    SourceProcessOutcome::Failed => self
                        .state
                        .load_session_source(&source.source_key)?
                        .and_then(|source| source.error_code),
                    _ => None,
                };
                (process, code)
            }
            Err(error) => {
                let (code, default_status, complete_byte_offset) = match error {
                    CodexScannerError::Read => (
                        SessionSourceErrorCode::SourceIoFailed,
                        SessionSourceStatus::Unavailable,
                        None,
                    ),
                    CodexScannerError::Parse => (
                        SessionSourceErrorCode::SourceParseInvalid,
                        SessionSourceStatus::Unavailable,
                        None,
                    ),
                    CodexScannerError::ReplayInconsistent => (
                        SessionSourceErrorCode::SourceReplayInconsistent,
                        SessionSourceStatus::Unavailable,
                        Some(0),
                    ),
                    CodexScannerError::RecordTooLarge => (
                        SessionSourceErrorCode::SourceRecordTooLarge,
                        SessionSourceStatus::ResourceLimited,
                        None,
                    ),
                    CodexScannerError::ReplayLimit => (
                        SessionSourceErrorCode::SourceReplayLimit,
                        SessionSourceStatus::ResourceLimited,
                        None,
                    ),
                    CodexScannerError::CleanupPending => (
                        SessionSourceErrorCode::SourceCandidateInterrupted,
                        SessionSourceStatus::Unavailable,
                        None,
                    ),
                    error => return Err(error),
                };
                self.record_source_failure_if_possible(
                    source,
                    discovered_source,
                    code,
                    &cycle.transition_at,
                )?;
                let status = self
                    .state
                    .load_session_source(&source.source_key)?
                    .map_or(default_status, |source| source.status);
                let complete_byte_offset = complete_byte_offset.unwrap_or(
                    self.state
                        .load_current_session_scan_cursor(&source.source_key)?
                        .map_or(0, |cursor| cursor.complete_byte_offset),
                );
                (
                    SourceProcessResult {
                        outcome: if matches!(error, CodexScannerError::CleanupPending) {
                            SourceProcessOutcome::CleanupPending
                        } else {
                            SourceProcessOutcome::Failed
                        },
                        status,
                        complete_byte_offset,
                    },
                    Some(code),
                )
            }
        };
        let outcome = process.outcome;
        Ok((
            SourceScanSummary {
                source_key: source.source_key.clone(),
                session_key: Some(session_key(&self.domain_key, &metadata.root_thread_id)),
                status: process.status,
                error_code: process_error,
                replay_resolution: if process_error
                    == Some(SessionSourceErrorCode::SourceReplayInconsistent)
                {
                    ReplayResolution::Deferred(SessionSourceErrorCode::SourceReplayInconsistent)
                } else {
                    replay.resolution
                },
                complete_byte_offset: process.complete_byte_offset,
            },
            Some(outcome),
        ))
    }

    pub fn scan(
        &mut self,
        transition_at: &str,
        control: ScanControl,
    ) -> Result<CodexScanSummary, CodexScannerError> {
        self.slice_cycle = None;
        self.metrics = ScannerMetrics::default();
        let discovered = discover_codex_sessions(&self.root, self.discovery_limits)?;
        #[cfg(test)]
        test_hooks::run_after_discovery();
        let persisted_identities = self.persisted_identity_sources()?;
        let persisted_sources = self.persisted_codex_source_keys()?;
        let identities = discovered
            .iter()
            .map(|source| opaque_file_identity(&self.domain_key, source.identity()))
            .collect::<Vec<_>>();
        let mut source_keys = vec![None; discovered.len()];
        let mut reserved_keys: HashMap<String, String> = HashMap::new();
        for (index, identity) in identities.iter().enumerate() {
            let Some(source_key) = persisted_identities.get(identity) else {
                continue;
            };
            if reserved_keys
                .insert(source_key.clone(), identity.clone())
                .is_some()
            {
                return Err(StorageError::StableRecordConflict {
                    record_kind: "Session source key",
                }
                .into());
            }
            source_keys[index] = Some(source_key.clone());
        }
        for (index, source) in discovered.iter().enumerate() {
            if source_keys[index].is_some() {
                continue;
            }
            let identity = &identities[index];
            let path_key = self.path_source_key(source);
            let source_key = if !reserved_keys.contains_key(&path_key) {
                path_key
            } else {
                let mut counter = 0u64;
                loop {
                    let candidate = self.collision_source_key(source, identity, counter);
                    if !reserved_keys.contains_key(&candidate)
                        && !persisted_sources.contains(&candidate)
                    {
                        break candidate;
                    }
                    counter = counter.checked_add(1).ok_or(CodexScannerError::Parse)?;
                }
            };
            if reserved_keys
                .insert(source_key.clone(), identity.clone())
                .is_some()
            {
                return Err(StorageError::StableRecordConflict {
                    record_kind: "Session source key",
                }
                .into());
            }
            source_keys[index] = Some(source_key);
        }
        let mut inspected = Vec::with_capacity(discovered.len());
        for (index, source) in discovered.iter().enumerate() {
            let identity = identities[index].clone();
            let source_key = source_keys[index]
                .take()
                .expect("every discovered source receives one source key");
            match self.inspect_source(source) {
                Ok(metadata) => inspected.push(InspectedSource {
                    discovered_index: index,
                    source_key,
                    identity,
                    metadata: Some(metadata),
                    inspection_error: None,
                }),
                Err(error) => inspected.push(InspectedSource {
                    discovered_index: index,
                    source_key,
                    identity,
                    metadata: None,
                    inspection_error: Some(error),
                }),
            }
        }

        let mut thread_sources: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, source) in inspected.iter().enumerate() {
            if let Some(metadata) = &source.metadata {
                thread_sources
                    .entry(metadata.root_thread_id.clone())
                    .or_default()
                    .push(index);
            }
        }
        let replay_topology = ReplayTopology::build(&inspected, &thread_sources);

        let mut order = (0..inspected.len()).collect::<Vec<_>>();
        order.sort_unstable_by(|left, right| {
            replay_topology.depths[*left]
                .cmp(&replay_topology.depths[*right])
                .then_with(|| {
                    inspected[*left]
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.parent_thread_id.as_deref())
                        .cmp(
                            &inspected[*right]
                                .metadata
                                .as_ref()
                                .and_then(|metadata| metadata.parent_thread_id.as_deref()),
                        )
                })
                .then_with(|| {
                    inspected[*left]
                        .source_key
                        .cmp(&inspected[*right].source_key)
                })
        });
        let referenced_parent_indices = inspected
            .iter()
            .filter_map(|source| source.metadata.as_ref()?.parent_thread_id.as_ref())
            .filter_map(|parent_id| thread_sources.get(parent_id))
            .flatten()
            .copied()
            .collect::<HashSet<_>>();

        let current_keys = inspected
            .iter()
            .map(|source| source.source_key.clone())
            .collect::<HashSet<_>>();
        let deleted_keys = persisted_sources
            .iter()
            .filter(|source_key| !current_keys.contains(*source_key))
            .cloned()
            .collect::<Vec<_>>();
        for source_key in &deleted_keys {
            let _ = self.state.mark_source_unavailable(
                source_key,
                SessionSourceErrorCode::SourceSessionsAbsent,
                transition_at,
            )?;
        }
        let deleted_sources = deleted_keys.len();

        let mut summaries = Vec::with_capacity(inspected.len());
        let mut advanced_sources = 0;
        let mut unchanged_sources = 0;
        let mut committed_batches = 0;
        let mut overall_outcome = ScanOutcome::Complete;
        let mut replay_cache = ReplayGroupCache::default();
        let mut replay_group_start = 0usize;
        let mut replay_group_end = 0usize;

        for (order_position, &work_index) in order.iter().enumerate() {
            let source = &inspected[work_index];
            let discovered_source = &discovered[source.discovered_index];
            let Some(metadata) = source.metadata.as_ref() else {
                let error = source
                    .inspection_error
                    .as_ref()
                    .expect("failed inspection has an error");
                let (code, default_status) = match error {
                    SourceInspectionError::RecordTooLarge => (
                        SessionSourceErrorCode::SourceRecordTooLarge,
                        SessionSourceStatus::ResourceLimited,
                    ),
                    SourceInspectionError::Read => (
                        SessionSourceErrorCode::SourceIoFailed,
                        SessionSourceStatus::Unavailable,
                    ),
                    SourceInspectionError::Parse => (
                        SessionSourceErrorCode::SourceParseInvalid,
                        SessionSourceStatus::Unavailable,
                    ),
                };
                self.record_source_failure_if_possible(
                    source,
                    discovered_source,
                    code,
                    transition_at,
                )?;
                let status = self
                    .state
                    .load_session_source(&source.source_key)?
                    .map_or(default_status, |source| source.status);
                summaries.push(SourceScanSummary {
                    source_key: source.source_key.clone(),
                    session_key: None,
                    status,
                    error_code: Some(code),
                    replay_resolution: ReplayResolution::NotForked,
                    complete_byte_offset: 0,
                });
                continue;
            };
            let replay_group_indices = if metadata.parent_thread_id.is_some() {
                if order_position >= replay_group_end {
                    replay_group_start = order_position;
                    replay_group_end = order_position + 1;
                    while replay_group_end < order.len() {
                        let next = order[replay_group_end];
                        if replay_topology.depths[next] != replay_topology.depths[work_index]
                            || inspected[next]
                                .metadata
                                .as_ref()
                                .and_then(|metadata| metadata.parent_thread_id.as_deref())
                                != metadata.parent_thread_id.as_deref()
                        {
                            break;
                        }
                        replay_group_end += 1;
                    }
                }
                &order[replay_group_start..replay_group_end]
            } else {
                &order[order_position..order_position + 1]
            };
            let replay_result = if let Some(code) = replay_topology.errors[work_index] {
                Ok(ReplayState::deferred(code))
            } else {
                self.resolve_replay(
                    work_index,
                    metadata,
                    &inspected,
                    &thread_sources,
                    replay_group_indices,
                    &mut replay_cache,
                )
            };
            let replay = match replay_result {
                Ok(replay) => replay,
                Err(error) => {
                    let (code, default_status) = match error {
                        CodexScannerError::Read => (
                            SessionSourceErrorCode::SourceIoFailed,
                            SessionSourceStatus::Unavailable,
                        ),
                        CodexScannerError::Parse => (
                            SessionSourceErrorCode::SourceReplayInconsistent,
                            SessionSourceStatus::Unavailable,
                        ),
                        CodexScannerError::ReplayInconsistent => (
                            SessionSourceErrorCode::SourceReplayInconsistent,
                            SessionSourceStatus::Unavailable,
                        ),
                        CodexScannerError::RecordTooLarge => (
                            SessionSourceErrorCode::SourceRecordTooLarge,
                            SessionSourceStatus::ResourceLimited,
                        ),
                        CodexScannerError::ReplayLimit => (
                            SessionSourceErrorCode::SourceReplayLimit,
                            SessionSourceStatus::ResourceLimited,
                        ),
                        error => return Err(error),
                    };
                    self.record_source_failure_if_possible(
                        source,
                        discovered_source,
                        code,
                        transition_at,
                    )?;
                    let status = self
                        .state
                        .load_session_source(&source.source_key)?
                        .map_or(default_status, |source| source.status);
                    summaries.push(SourceScanSummary {
                        source_key: source.source_key.clone(),
                        session_key: Some(session_key(&self.domain_key, &metadata.root_thread_id)),
                        status,
                        error_code: Some(code),
                        replay_resolution: ReplayResolution::Deferred(code),
                        complete_byte_offset: self
                            .state
                            .load_current_session_scan_cursor(&source.source_key)?
                            .map_or(0, |cursor| cursor.complete_byte_offset),
                    });
                    continue;
                }
            };
            if let ReplayResolution::Deferred(code) = replay.resolution {
                self.record_source_failure_if_possible(
                    source,
                    discovered_source,
                    code,
                    transition_at,
                )?;
                let persisted = self.state.load_session_source(&source.source_key)?;
                let status = persisted.as_ref().map_or_else(
                    || {
                        if code == SessionSourceErrorCode::SourceReplayLimit {
                            SessionSourceStatus::ResourceLimited
                        } else {
                            SessionSourceStatus::Unavailable
                        }
                    },
                    |source| source.status,
                );
                summaries.push(SourceScanSummary {
                    source_key: source.source_key.clone(),
                    session_key: Some(session_key(&self.domain_key, &metadata.root_thread_id)),
                    status,
                    error_code: Some(code),
                    replay_resolution: ReplayResolution::Deferred(code),
                    complete_byte_offset: self
                        .state
                        .load_current_session_scan_cursor(&source.source_key)?
                        .map_or(0, |cursor| cursor.complete_byte_offset),
                });
                continue;
            }

            let (process, process_error) = match self.process_source(
                source,
                discovered_source,
                metadata,
                &replay,
                transition_at,
                control,
                &mut committed_batches,
                referenced_parent_indices.contains(&work_index),
            ) {
                Ok(process) => {
                    let code = match process.outcome {
                        SourceProcessOutcome::Interrupted
                        | SourceProcessOutcome::CleanupPending => {
                            Some(SessionSourceErrorCode::SourceCandidateInterrupted)
                        }
                        _ if process.status == SessionSourceStatus::ResourceLimited => {
                            Some(SessionSourceErrorCode::SourceReplayLimit)
                        }
                        SourceProcessOutcome::Failed => self
                            .state
                            .load_session_source(&source.source_key)?
                            .and_then(|source| source.error_code),
                        _ => None,
                    };
                    (process, code)
                }
                Err(CodexScannerError::Read) => {
                    let code = SessionSourceErrorCode::SourceIoFailed;
                    self.record_source_failure_if_possible(
                        source,
                        discovered_source,
                        code,
                        transition_at,
                    )?;
                    let status = self
                        .state
                        .load_session_source(&source.source_key)?
                        .map_or(SessionSourceStatus::Unavailable, |source| source.status);
                    (
                        SourceProcessResult {
                            outcome: SourceProcessOutcome::Failed,
                            status,
                            complete_byte_offset: self
                                .state
                                .load_current_session_scan_cursor(&source.source_key)?
                                .map_or(0, |cursor| cursor.complete_byte_offset),
                        },
                        Some(code),
                    )
                }
                Err(CodexScannerError::Parse) => {
                    let code = SessionSourceErrorCode::SourceParseInvalid;
                    self.record_source_failure_if_possible(
                        source,
                        discovered_source,
                        code,
                        transition_at,
                    )?;
                    let status = self
                        .state
                        .load_session_source(&source.source_key)?
                        .map_or(SessionSourceStatus::Unavailable, |source| source.status);
                    (
                        SourceProcessResult {
                            outcome: SourceProcessOutcome::Failed,
                            status,
                            complete_byte_offset: self
                                .state
                                .load_current_session_scan_cursor(&source.source_key)?
                                .map_or(0, |cursor| cursor.complete_byte_offset),
                        },
                        Some(code),
                    )
                }
                Err(CodexScannerError::ReplayInconsistent) => {
                    let code = SessionSourceErrorCode::SourceReplayInconsistent;
                    self.record_source_failure_if_possible(
                        source,
                        discovered_source,
                        code,
                        transition_at,
                    )?;
                    let status = self
                        .state
                        .load_session_source(&source.source_key)?
                        .map_or(SessionSourceStatus::Unavailable, |source| source.status);
                    (
                        SourceProcessResult {
                            outcome: SourceProcessOutcome::Failed,
                            status,
                            complete_byte_offset: 0,
                        },
                        Some(code),
                    )
                }
                Err(CodexScannerError::RecordTooLarge) => {
                    let code = SessionSourceErrorCode::SourceRecordTooLarge;
                    self.record_source_failure_if_possible(
                        source,
                        discovered_source,
                        code,
                        transition_at,
                    )?;
                    let status = self
                        .state
                        .load_session_source(&source.source_key)?
                        .map_or(SessionSourceStatus::ResourceLimited, |source| source.status);
                    (
                        SourceProcessResult {
                            outcome: SourceProcessOutcome::Failed,
                            status,
                            complete_byte_offset: self
                                .state
                                .load_current_session_scan_cursor(&source.source_key)?
                                .map_or(0, |cursor| cursor.complete_byte_offset),
                        },
                        Some(code),
                    )
                }
                Err(CodexScannerError::ReplayLimit) => {
                    let code = SessionSourceErrorCode::SourceReplayLimit;
                    self.record_source_failure_if_possible(
                        source,
                        discovered_source,
                        code,
                        transition_at,
                    )?;
                    let status = self
                        .state
                        .load_session_source(&source.source_key)?
                        .map_or(SessionSourceStatus::ResourceLimited, |source| source.status);
                    (
                        SourceProcessResult {
                            outcome: SourceProcessOutcome::Failed,
                            status,
                            complete_byte_offset: self
                                .state
                                .load_current_session_scan_cursor(&source.source_key)?
                                .map_or(0, |cursor| cursor.complete_byte_offset),
                        },
                        Some(code),
                    )
                }
                Err(CodexScannerError::CleanupPending) => {
                    let code = SessionSourceErrorCode::SourceCandidateInterrupted;
                    self.record_source_failure_if_possible(
                        source,
                        discovered_source,
                        code,
                        transition_at,
                    )?;
                    let status = self
                        .state
                        .load_session_source(&source.source_key)?
                        .map_or(SessionSourceStatus::Unavailable, |source| source.status);
                    (
                        SourceProcessResult {
                            outcome: SourceProcessOutcome::CleanupPending,
                            status,
                            complete_byte_offset: self
                                .state
                                .load_current_session_scan_cursor(&source.source_key)?
                                .map_or(0, |cursor| cursor.complete_byte_offset),
                        },
                        Some(code),
                    )
                }
                Err(error) => return Err(error),
            };
            match process.outcome {
                SourceProcessOutcome::Advanced => advanced_sources += 1,
                SourceProcessOutcome::Unchanged => unchanged_sources += 1,
                SourceProcessOutcome::Failed => {}
                SourceProcessOutcome::Interrupted => {
                    overall_outcome = ScanOutcome::Interrupted;
                }
                SourceProcessOutcome::CleanupPending => {
                    overall_outcome = ScanOutcome::Interrupted;
                }
            }
            summaries.push(SourceScanSummary {
                source_key: source.source_key.clone(),
                session_key: Some(session_key(&self.domain_key, &metadata.root_thread_id)),
                status: process.status,
                error_code: process_error,
                replay_resolution: if process_error
                    == Some(SessionSourceErrorCode::SourceReplayInconsistent)
                {
                    ReplayResolution::Deferred(SessionSourceErrorCode::SourceReplayInconsistent)
                } else {
                    replay.resolution
                },
                complete_byte_offset: process.complete_byte_offset,
            });
            if process.outcome == SourceProcessOutcome::Interrupted {
                break;
            }
        }

        summaries.sort_unstable_by(|left, right| left.source_key.cmp(&right.source_key));
        Ok(CodexScanSummary {
            outcome: overall_outcome,
            advanced_sources,
            unchanged_sources,
            deleted_sources,
            sources: summaries,
            metrics: self.metrics.clone(),
        })
    }

    fn persisted_identity_sources(&self) -> Result<HashMap<String, String>, StorageError> {
        let mut output = HashMap::new();
        let mut page_key: Option<SessionSourcePageKey> = None;
        loop {
            let page = self
                .state
                .load_session_sources_page(page_key.as_ref(), REPLAY_PAGE_SIZE)?;
            for source in page.items {
                if source.source_kind != SessionSourceKind::Codex {
                    continue;
                }
                let current = self
                    .state
                    .load_current_session_scan_cursor(&source.source_key)?;
                let staging = self
                    .state
                    .load_staging_session_scan_cursor(&source.source_key)?;
                for cursor in current.into_iter().chain(staging) {
                    let identity = cursor.file_identity.as_str().to_owned();
                    if output
                        .insert(identity, source.source_key.clone())
                        .is_some_and(|existing| existing != source.source_key)
                    {
                        return Err(StorageError::StableRecordConflict {
                            record_kind: "Session file identity",
                        });
                    }
                }
            }
            page_key = page.next_page_key;
            if page_key.is_none() {
                break;
            }
        }
        Ok(output)
    }

    fn persisted_codex_source_keys(&self) -> Result<HashSet<String>, StorageError> {
        let mut output = HashSet::new();
        let mut page_key: Option<SessionSourcePageKey> = None;
        loop {
            let page = self
                .state
                .load_session_sources_page(page_key.as_ref(), REPLAY_PAGE_SIZE)?;
            for source in page.items {
                if source.source_kind == SessionSourceKind::Codex {
                    output.insert(source.source_key);
                }
            }
            page_key = page.next_page_key;
            if page_key.is_none() {
                break;
            }
        }
        Ok(output)
    }

    fn path_source_key(&self, source: &DiscoveredSession) -> String {
        let location = match source.location() {
            SessionLocation::Live => b"live".as_slice(),
            SessionLocation::Archive => b"archive".as_slice(),
        };
        let root_identity = platform_identity_bytes(self.root.identity());
        let relative = path_bytes(source.relative_path());
        opaque_hex(
            &self.domain_key,
            b"wokcore.codex.source.v1",
            &[&root_identity, location, &relative],
        )
    }

    fn collision_source_key(
        &self,
        source: &DiscoveredSession,
        identity: &str,
        counter: u64,
    ) -> String {
        let location = match source.location() {
            SessionLocation::Live => b"live".as_slice(),
            SessionLocation::Archive => b"archive".as_slice(),
        };
        let root_identity = platform_identity_bytes(self.root.identity());
        let relative = path_bytes(source.relative_path());
        opaque_hex(
            &self.domain_key,
            b"wokcore.codex.source-collision.v1",
            &[
                &root_identity,
                location,
                &relative,
                identity.as_bytes(),
                &counter.to_be_bytes(),
            ],
        )
    }

    fn inspect_source(
        &mut self,
        source: &DiscoveredSession,
    ) -> Result<SourceMetadata, SourceInspectionError> {
        self.metrics.source_opens = self.metrics.source_opens.saturating_add(1);
        let mut file = source
            .open(&self.root, u64::MAX)
            .map_err(|_| SourceInspectionError::Read)?;
        self.inspect_pinned_source(&mut file)
    }

    fn inspect_pinned_source(
        &mut self,
        file: &mut SessionFile,
    ) -> Result<SourceMetadata, SourceInspectionError> {
        let mut reader = JsonlReader::new(JsonlCursor::new(0, 1));
        for _ in 0..METADATA_PROBE_RECORDS {
            let scan = reader
                .scan_bounded(file, 1, MAX_JSONL_LINE_BYTES)
                .map_err(SourceInspectionError::from)?;
            self.metrics.metadata_probe_bytes = self
                .metrics
                .metadata_probe_bytes
                .saturating_add(scan.read_bytes);
            let empty = scan.records.is_empty();
            for record in scan.records {
                if record.status != JsonlRecordStatus::Valid {
                    continue;
                }
                let value = record.value();
                if value.get("type").and_then(Value::as_str) != Some("session_meta") {
                    continue;
                }
                if let CodexStructuralRecord::SessionMeta(found) =
                    parse_codex_record(value).map_err(|_| SourceInspectionError::Parse)?
                {
                    return Ok(SourceMetadata {
                        root_thread_id: found.root_thread_id,
                        created_at: found.created_at,
                        parent_thread_id: found.parent_thread_id,
                    });
                }
            }
            if scan.reached_end || empty {
                break;
            }
        }
        Err(SourceInspectionError::Parse)
    }

    fn resolve_replay(
        &mut self,
        work_index: usize,
        metadata: &SourceMetadata,
        inspected: &[InspectedSource],
        thread_sources: &HashMap<String, Vec<usize>>,
        group_indices: &[usize],
        cache: &mut ReplayGroupCache,
    ) -> Result<ReplayState, CodexScannerError> {
        let Some(parent_thread_id) = &metadata.parent_thread_id else {
            return Ok(ReplayState::not_forked());
        };
        let Some(parent_indices) = thread_sources.get(parent_thread_id) else {
            return Ok(ReplayState::deferred(
                SessionSourceErrorCode::SourceReplayParentMissing,
            ));
        };
        if parent_indices.len() != 1 || parent_indices[0] == work_index {
            return Ok(ReplayState::deferred(
                SessionSourceErrorCode::SourceReplayParentAmbiguous,
            ));
        }
        let parent_index = parent_indices[0];
        if cache.parent_index != Some(parent_index) {
            self.load_replay_group(parent_index, inspected, group_indices, cache)?;
        }
        if cache.group_error.is_none()
            && inspected[parent_index]
                .metadata
                .as_ref()
                .is_none_or(|parent| parent.created_at > metadata.created_at)
        {
            return Ok(ReplayState::deferred(
                SessionSourceErrorCode::SourceReplayInconsistent,
            ));
        }
        Ok(cache.resolution_for(work_index))
    }

    fn load_replay_group(
        &mut self,
        parent_index: usize,
        inspected: &[InspectedSource],
        group_indices: &[usize],
        cache: &mut ReplayGroupCache,
    ) -> Result<(), CodexScannerError> {
        cache.reset(parent_index);
        let parent = &inspected[parent_index];
        let Some(parent_metadata) = parent.metadata.as_ref() else {
            return Ok(());
        };

        let mut boundaries =
            Vec::with_capacity(group_indices.len().min(MAX_REPLAY_CHILDREN_PER_PARENT));
        for &work_index in group_indices {
            #[cfg(test)]
            test_hooks::note_replay_group_child_visit();
            let child = &inspected[work_index];
            let Some(metadata) = child.metadata.as_ref() else {
                continue;
            };
            if metadata.parent_thread_id.as_deref() != Some(&parent_metadata.root_thread_id) {
                continue;
            }
            if boundaries.len() == MAX_REPLAY_CHILDREN_PER_PARENT {
                cache.group_error = Some(SessionSourceErrorCode::SourceReplayLimit);
                self.metrics.maximum_replay_group_working_bytes =
                    self.metrics.maximum_replay_group_working_bytes.max(
                        boundaries
                            .capacity()
                            .saturating_mul(std::mem::size_of::<CachedReplayBoundary>()),
                    );
                return Ok(());
            }
            boundaries.push(CachedReplayBoundary {
                work_index: u32::try_from(work_index).map_err(|_| CodexScannerError::Parse)?,
                replayed_events: 0,
                boundary_fingerprint: [0; 32],
            });
        }
        boundaries.sort_unstable_by(|left, right| {
            let left_index =
                usize::try_from(left.work_index).expect("a cached replay work index fits usize");
            let right_index =
                usize::try_from(right.work_index).expect("a cached replay work index fits usize");
            let left_created = &inspected[left_index]
                .metadata
                .as_ref()
                .expect("replay requests have metadata")
                .created_at;
            let right_created = &inspected[right_index]
                .metadata
                .as_ref()
                .expect("replay requests have metadata")
                .created_at;
            left_created
                .cmp(right_created)
                .then_with(|| left.work_index.cmp(&right.work_index))
        });

        let Some(parent_state) = self.state.load_session_source(&parent.source_key)? else {
            cache.group_error = Some(SessionSourceErrorCode::SourceReplayParentMissing);
            return Ok(());
        };
        if parent_state.status != SessionSourceStatus::Available {
            return Ok(());
        }
        let Some(parent_generation) = parent_state.current_generation else {
            cache.group_error = Some(SessionSourceErrorCode::SourceReplayParentMissing);
            return Ok(());
        };
        let parent_cursor = self
            .state
            .load_current_session_scan_cursor(&parent.source_key)?;
        if parent_cursor
            .as_ref()
            .is_none_or(|cursor| cursor.result_code != Some(SessionScanResultCode::Advanced))
        {
            return Ok(());
        }
        let expected_events = parent_cursor
            .as_ref()
            .map_or(0, |cursor| cursor.parser_checkpoint.event_ordinal);
        let mut page_key: Option<ReplaySignaturePageKey> = None;
        let mut indexed_events = 0u64;
        let mut request_index = 0usize;
        let mut boundary_fingerprint = replay_chain_seed(&self.domain_key, parent_generation);
        let retained_boundary_bytes = boundaries
            .capacity()
            .saturating_mul(std::mem::size_of::<CachedReplayBoundary>());
        let working_bytes = retained_boundary_bytes.saturating_add(parent.source_key.len());
        let maximum_page_bytes = maximum_replay_page_retained_bytes(REPLAY_GROUP_PAGE_SIZE);
        if working_bytes.saturating_add(maximum_page_bytes) > MAX_REPLAY_GROUP_WORKING_BYTES {
            cache.group_error = Some(SessionSourceErrorCode::SourceReplayLimit);
            return Ok(());
        }
        self.metrics.maximum_replay_group_working_bytes = self
            .metrics
            .maximum_replay_group_working_bytes
            .max(working_bytes.saturating_add(maximum_page_bytes));
        loop {
            let page = self.state.load_codex_replay_signature_page(
                &parent.source_key,
                parent_generation,
                page_key.as_ref(),
                REPLAY_GROUP_PAGE_SIZE,
            )?;
            self.metrics.replay_pages_loaded = self.metrics.replay_pages_loaded.saturating_add(1);
            self.metrics.maximum_replay_page_rows =
                self.metrics.maximum_replay_page_rows.max(page.items.len());
            let page_working_bytes =
                working_bytes.saturating_add(replay_page_retained_bytes(&page));
            self.metrics.maximum_replay_group_working_bytes = self
                .metrics
                .maximum_replay_group_working_bytes
                .max(page_working_bytes);
            if page_working_bytes > MAX_REPLAY_GROUP_WORKING_BYTES {
                cache.group_error = Some(SessionSourceErrorCode::SourceReplayLimit);
                return Ok(());
            }
            for signature in &page.items {
                while request_index < boundaries.len()
                    && inspected[usize::try_from(boundaries[request_index].work_index)
                        .expect("a cached replay work index fits usize")]
                    .metadata
                    .as_ref()
                    .expect("replay requests have metadata")
                    .created_at
                    .as_str()
                        < signature.occurred_at.as_str()
                {
                    boundaries[request_index].replayed_events =
                        u32::try_from(indexed_events).map_err(|_| CodexScannerError::Parse)?;
                    boundaries[request_index].boundary_fingerprint = boundary_fingerprint;
                    request_index += 1;
                }
                indexed_events = indexed_events
                    .checked_add(1)
                    .ok_or(CodexScannerError::Parse)?;
                if indexed_events > self.replay_limit {
                    cache.group_error = Some(SessionSourceErrorCode::SourceReplayLimit);
                    return Ok(());
                }
                if signature.token_event_ordinal != indexed_events {
                    return Ok(());
                }
                boundary_fingerprint = replay_chain_step(
                    &self.domain_key,
                    parent_generation,
                    indexed_events,
                    boundary_fingerprint,
                    signature.signature_hash,
                );
            }
            page_key = page.next_page_key;
            if page_key.is_none() {
                break;
            }
        }
        if indexed_events != expected_events {
            return Ok(());
        }
        while request_index < boundaries.len() {
            boundaries[request_index].replayed_events =
                u32::try_from(indexed_events).map_err(|_| CodexScannerError::Parse)?;
            boundaries[request_index].boundary_fingerprint = boundary_fingerprint;
            request_index += 1;
        }
        boundaries.sort_unstable_by_key(|boundary| boundary.work_index);
        cache.parent_source_key = Some(parent.source_key.clone());
        cache.parent_generation = Some(parent_generation);
        cache.initial_fingerprint = Some(replay_chain_seed(&self.domain_key, parent_generation));
        cache.group_error = None;
        cache.boundaries = boundaries;
        debug_assert!(cache.retained_bytes() <= MAX_REPLAY_GROUP_WORKING_BYTES);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn process_source(
        &mut self,
        source: &InspectedSource,
        discovered: &DiscoveredSession,
        metadata: &SourceMetadata,
        replay: &ReplayState,
        transition_at: &str,
        control: ScanControl,
        committed_batches: &mut usize,
        builds_parent_index: bool,
    ) -> Result<SourceProcessResult, CodexScannerError> {
        self.metrics.source_opens = self.metrics.source_opens.saturating_add(1);
        let mut file = discovered
            .open(&self.root, u64::MAX)
            .map_err(|_| CodexScannerError::Read)?;
        let snapshot = file.snapshot().clone();
        let current = self
            .state
            .load_current_session_scan_cursor(&source.source_key)?;
        let persisted_source = self.state.load_session_source(&source.source_key)?;
        let mut cleanup_performed = false;
        let cleanup_pending = match persisted_source
            .as_ref()
            .and_then(|source| source.retired_generation)
        {
            Some(retired_generation) => !self.cleanup_generation_once(
                &source.source_key,
                retired_generation,
                &mut cleanup_performed,
            )?,
            None => false,
        };
        if cleanup_pending {
            let current_generation = persisted_source
                .as_ref()
                .and_then(|source| source.current_generation)
                .ok_or(CodexScannerError::CleanupPending)?;
            self.state.fail_candidate(
                &source.source_key,
                current_generation,
                SessionSourceErrorCode::SourceCandidateInterrupted,
                transition_at,
            )?;
            let status = self
                .state
                .load_session_source(&source.source_key)?
                .map_or(SessionSourceStatus::Unavailable, |source| source.status);
            return Ok(SourceProcessResult {
                outcome: SourceProcessOutcome::CleanupPending,
                status,
                complete_byte_offset: current
                    .as_ref()
                    .map_or(0, |cursor| cursor.complete_byte_offset),
            });
        }
        let force_replay_rebuild = if builds_parent_index {
            match &current {
                Some(cursor) if cursor.parser_checkpoint.event_ordinal > 0 => {
                    !self.state.codex_replay_index_is_complete(
                        &source.source_key,
                        cursor.generation,
                        cursor.parser_checkpoint.event_ordinal,
                    )?
                }
                _ => false,
            }
        } else {
            false
        };
        let modified_at = system_time_utc(snapshot.modified);

        if let Some(cursor) = &current {
            let (current_head, current_boundary) = fingerprints_with_extent(
                &mut file,
                cursor.complete_byte_offset,
                cursor.observed_size,
                &self.domain_key,
            )
            .map_err(|_| CodexScannerError::Read)?;
            let lineage_matches = cursor.parent_source_key == replay.parent_source_key
                && cursor.parent_generation == replay.parent_generation
                && cursor.replay_boundary_fingerprint == replay.boundary_fingerprint;
            if cursor.file_identity.as_str() == source.identity
                && cursor.observed_size == snapshot.size
                && cursor.modified_at == modified_at
                && cursor.head_fingerprint == current_head
                && cursor.boundary_fingerprint == current_boundary
                && lineage_matches
                && !force_replay_rebuild
                && cursor.parser_checkpoint.version == PARSER_CHECKPOINT_VERSION
                && cursor.result_code == Some(SessionScanResultCode::Advanced)
                && persisted_source.as_ref().is_none_or(|state| {
                    state.status == SessionSourceStatus::Available
                        || state.error_code == Some(SessionSourceErrorCode::SourceSessionsAbsent)
                        || state.error_code
                            == Some(SessionSourceErrorCode::SourceCandidateInterrupted)
                })
            {
                if persisted_source.as_ref().is_none_or(|state| {
                    state.status != SessionSourceStatus::Available || state.error_code.is_some()
                }) {
                    let _ = self.state.record_source_success(
                        &source.source_key,
                        cursor.generation,
                        transition_at,
                    )?;
                }
                return Ok(SourceProcessResult {
                    outcome: SourceProcessOutcome::Unchanged,
                    status: SessionSourceStatus::Available,
                    complete_byte_offset: cursor.complete_byte_offset,
                });
            }
        }

        let mut replacement = current.is_none() || force_replay_rebuild;
        if let Some(cursor) = &current {
            if cursor.file_identity.as_str() != source.identity
                || snapshot.size < cursor.complete_byte_offset
                || cursor.parser_checkpoint.version != PARSER_CHECKPOINT_VERSION
                || cursor.parent_source_key != replay.parent_source_key
                || cursor.parent_generation != replay.parent_generation
                || cursor.replay_boundary_fingerprint != replay.boundary_fingerprint
            {
                replacement = true;
            } else {
                let (current_head, boundary) = fingerprints_with_extent(
                    &mut file,
                    cursor.complete_byte_offset,
                    cursor.observed_size,
                    &self.domain_key,
                )
                .map_err(|_| CodexScannerError::Read)?;
                if current_head != cursor.head_fingerprint
                    || boundary != cursor.boundary_fingerprint
                {
                    replacement = true;
                }
            }
        }

        let generation = if replacement {
            match current.as_ref() {
                Some(cursor) => cursor
                    .generation
                    .checked_add(1)
                    .ok_or(CodexScannerError::Parse)?,
                None => 1,
            }
        } else {
            current
                .as_ref()
                .expect("append has a current cursor")
                .generation
        };
        if generation == 0 {
            return Err(CodexScannerError::Parse);
        }

        let mut cursor = if replacement {
            let (head_fingerprint, current_boundary) = fingerprints(&mut file, 0, &self.domain_key)
                .map_err(|_| CodexScannerError::Read)?;
            self.prepare_candidate_cursor(
                source,
                generation,
                &mut file,
                &snapshot,
                &modified_at,
                head_fingerprint,
                current_boundary,
                replay,
                transition_at,
                &mut cleanup_performed,
            )?
        } else {
            current.clone().expect("append has a current cursor")
        };
        let is_candidate = replacement;
        self.metrics.full_source_scans = self.metrics.full_source_scans.saturating_add(1);
        if is_candidate && builds_parent_index {
            self.metrics.parent_index_builds = self.metrics.parent_index_builds.saturating_add(1);
        }
        let persisted_index = if cursor.complete_byte_offset == 0 {
            None
        } else if is_candidate {
            self.state
                .load_staging_session_index_record(&source.source_key)?
        } else {
            self.state
                .load_current_session_index_page(&source.source_key, None, 1)?
                .items
                .into_iter()
                .next()
        };
        let mut totals = TokenTotals {
            input: cursor.parser_checkpoint.previous_input_tokens,
            output: cursor.parser_checkpoint.previous_output_tokens,
            cache_read: cursor.parser_checkpoint.previous_cache_read_tokens,
            cache_write: cursor.parser_checkpoint.previous_cache_write_tokens,
            reasoning: cursor.parser_checkpoint.previous_reasoning_tokens,
        };
        let mut model = cursor
            .parser_checkpoint
            .current_model
            .clone()
            .unwrap_or_else(|| "unknown".to_owned());
        let mut event_ordinal = cursor.parser_checkpoint.event_ordinal;
        let mut offset = cursor.complete_byte_offset;
        let mut record_ordinal = cursor
            .stable_record_ordinal
            .checked_add(1)
            .ok_or(CodexScannerError::Parse)?
            .max(1);
        let replay_verified = cursor.parser_checkpoint.lineage_record_ordinal;
        if replay_verified > replay.replayed_events {
            return Err(CodexScannerError::ReplayInconsistent);
        }
        let mut replay_remaining = replay
            .replayed_events
            .checked_sub(replay_verified)
            .ok_or(CodexScannerError::Parse)?;
        let mut replay_fingerprint = cursor.parser_checkpoint.structural_hash;
        if replay.parent_source_key.is_some() {
            if replay_fingerprint.is_none() {
                return Err(CodexScannerError::ReplayInconsistent);
            }
        } else if replay_fingerprint.is_some() {
            return Err(CodexScannerError::ReplayInconsistent);
        }
        if replay_remaining > 0 {
            self.metrics.replay_child_scans = self.metrics.replay_child_scans.saturating_add(1);
        }
        let mut message_count = persisted_index
            .as_ref()
            .map_or(0, |index| index.message_count);
        let mut usage_event_count = persisted_index
            .as_ref()
            .map_or(0, |index| index.usage_event_count);
        let mut last_active_at = persisted_index.as_ref().map_or_else(
            || metadata.created_at.clone(),
            |index| index.last_active_at.clone(),
        );
        let mut pending_usage = Vec::new();
        let mut pending_replay = Vec::new();
        let mut reader = JsonlReader::new(JsonlCursor::new(offset, record_ordinal));

        loop {
            let scan = reader.scan(&mut file).map_err(map_jsonl_scanner_error)?;
            self.metrics.parser_read_bytes = self
                .metrics
                .parser_read_bytes
                .saturating_add(scan.read_bytes);
            for record in &scan.records {
                if record.status != JsonlRecordStatus::Valid {
                    continue;
                }
                match parse_codex_record(record.value()).map_err(|_| CodexScannerError::Parse)? {
                    CodexStructuralRecord::SessionMeta(found) => {
                        if !metadata_matches(&found, metadata) {
                            return Err(CodexScannerError::Parse);
                        }
                    }
                    CodexStructuralRecord::Unknown => {}
                    CodexStructuralRecord::TurnContext(context) => {
                        if let Some(occurred_at) = context.occurred_at {
                            last_active_at = maximum_timestamp(Some(last_active_at), &occurred_at)
                                .expect("maximum timestamp always returns a value");
                        }
                        model = context.model;
                    }
                    CodexStructuralRecord::ResponseItem(item) => {
                        if let Some(occurred_at) = item.occurred_at {
                            last_active_at = maximum_timestamp(Some(last_active_at), &occurred_at)
                                .expect("maximum timestamp always returns a value");
                        }
                        if item.is_message {
                            message_count = message_count.saturating_add(1);
                        }
                    }
                    CodexStructuralRecord::TokenCount(token) => {
                        event_ordinal = event_ordinal
                            .checked_add(1)
                            .ok_or(CodexScannerError::Parse)?;
                        if event_ordinal > self.replay_limit {
                            self.state.fail_candidate(
                                &source.source_key,
                                generation,
                                SessionSourceErrorCode::SourceReplayLimit,
                                transition_at,
                            )?;
                            let status = self
                                .state
                                .load_session_source(&source.source_key)?
                                .map_or(SessionSourceStatus::ResourceLimited, |source| {
                                    source.status
                                });
                            return Ok(SourceProcessResult {
                                outcome: SourceProcessOutcome::Failed,
                                status,
                                complete_byte_offset: 0,
                            });
                        }
                        if let Some(token_model) = &token.model {
                            model = token_model.clone();
                        }
                        let delta = match token.total {
                            Some(current) => totals.apply_cumulative(current),
                            None => token.last.and_then(|last| totals.add_last(last)),
                        };
                        let signature_hash = token_signature_hash(&self.domain_key, &token, &model);
                        if replay_remaining > 0 {
                            let parent_generation = replay
                                .parent_generation
                                .ok_or(CodexScannerError::ReplayInconsistent)?;
                            let verified_ordinal = replay
                                .replayed_events
                                .checked_sub(replay_remaining)
                                .and_then(|ordinal| ordinal.checked_add(1))
                                .ok_or(CodexScannerError::ReplayInconsistent)?;
                            replay_fingerprint = Some(replay_chain_step(
                                &self.domain_key,
                                parent_generation,
                                verified_ordinal,
                                replay_fingerprint.ok_or(CodexScannerError::ReplayInconsistent)?,
                                signature_hash,
                            ));
                        }
                        pending_replay.push(CodexReplaySignature {
                            parent_source_key: source.source_key.clone(),
                            parent_generation: generation,
                            token_event_ordinal: event_ordinal,
                            occurred_at: token.occurred_at.clone(),
                            signature_hash,
                        });
                        last_active_at =
                            maximum_timestamp(Some(last_active_at), &token.occurred_at)
                                .expect("maximum timestamp always returns a value");
                        if replay_remaining > 0 {
                            replay_remaining = replay_remaining
                                .checked_sub(1)
                                .ok_or(CodexScannerError::ReplayInconsistent)?;
                            if replay_remaining == 0
                                && replay_fingerprint != replay.boundary_fingerprint
                            {
                                return Err(CodexScannerError::ReplayInconsistent);
                            }
                        } else if let Some(delta) = delta {
                            usage_event_count = usage_event_count.saturating_add(1);
                            pending_usage.push(SessionUsageRecord {
                                usage_id: usage_id(
                                    &self.domain_key,
                                    &metadata.root_thread_id,
                                    generation,
                                    event_ordinal,
                                ),
                                session_key: session_key(
                                    &self.domain_key,
                                    &metadata.root_thread_id,
                                ),
                                source_key: source.source_key.clone(),
                                generation,
                                source_kind: SessionSourceKind::Codex,
                                model: model.clone(),
                                occurred_at: token.occurred_at,
                                input_tokens: delta.input,
                                output_tokens: delta.output,
                                cache_read_tokens: delta.cache_read,
                                cache_write_tokens: delta.cache_write,
                                reasoning_tokens: delta.reasoning,
                                record_revision: event_ordinal,
                            });
                        }
                    }
                }
                offset = record.byte_end;
                record_ordinal = record
                    .ordinal
                    .checked_add(1)
                    .ok_or(CodexScannerError::Parse)?;
                if pending_usage.len() + pending_replay.len() >= SESSION_BATCH_ROW_TARGET {
                    cursor = self.commit_parser_batch(
                        cursor,
                        &mut file,
                        source,
                        &snapshot,
                        &modified_at,
                        offset,
                        record_ordinal,
                        totals,
                        &model,
                        event_ordinal,
                        replay.replayed_events - replay_remaining,
                        replay_fingerprint,
                        replay,
                        transition_at,
                        is_candidate,
                        false,
                        std::mem::take(&mut pending_usage),
                        std::mem::take(&mut pending_replay),
                        Some(session_index(
                            &self.domain_key,
                            source,
                            metadata,
                            generation,
                            &last_active_at,
                            message_count,
                            usage_event_count,
                        )),
                    )?;
                    *committed_batches += 1;
                    if control
                        .stop_after_committed_batches
                        .is_some_and(|maximum| *committed_batches >= maximum)
                    {
                        self.state.fail_candidate(
                            &source.source_key,
                            generation,
                            SessionSourceErrorCode::SourceCandidateInterrupted,
                            transition_at,
                        )?;
                        let status = self
                            .state
                            .load_session_source(&source.source_key)?
                            .map_or(SessionSourceStatus::Unavailable, |source| source.status);
                        return Ok(SourceProcessResult {
                            outcome: SourceProcessOutcome::Interrupted,
                            status,
                            complete_byte_offset: offset,
                        });
                    }
                }
            }
            offset = scan.complete_byte_offset;
            record_ordinal = scan.next_record_ordinal;
            if scan.reached_end || scan.records.is_empty() {
                break;
            }
        }
        if replay_remaining != 0 {
            return Err(CodexScannerError::ReplayInconsistent);
        }
        if replay_fingerprint != replay.boundary_fingerprint {
            return Err(CodexScannerError::ReplayInconsistent);
        }

        let index = session_index(
            &self.domain_key,
            source,
            metadata,
            generation,
            &last_active_at,
            message_count,
            usage_event_count,
        );
        cursor = self.commit_parser_batch(
            cursor,
            &mut file,
            source,
            &snapshot,
            &modified_at,
            offset,
            record_ordinal,
            totals,
            &model,
            event_ordinal,
            replay.replayed_events - replay_remaining,
            replay_fingerprint,
            replay,
            transition_at,
            is_candidate,
            true,
            pending_usage,
            pending_replay,
            Some(index),
        )?;
        *committed_batches += 1;
        if control
            .stop_after_committed_batches
            .is_some_and(|maximum| *committed_batches >= maximum)
        {
            self.state.fail_candidate(
                &source.source_key,
                generation,
                SessionSourceErrorCode::SourceCandidateInterrupted,
                transition_at,
            )?;
            let status = self
                .state
                .load_session_source(&source.source_key)?
                .map_or(SessionSourceStatus::Unavailable, |source| source.status);
            return Ok(SourceProcessResult {
                outcome: SourceProcessOutcome::Interrupted,
                status,
                complete_byte_offset: cursor.complete_byte_offset,
            });
        }

        if is_candidate {
            self.state
                .promote_candidate(&source.source_key, generation, transition_at)?;
            if !self.cleanup_retired(&source.source_key, &mut cleanup_performed)? {
                self.state.fail_candidate(
                    &source.source_key,
                    generation,
                    SessionSourceErrorCode::SourceCandidateInterrupted,
                    transition_at,
                )?;
                let status = self
                    .state
                    .load_session_source(&source.source_key)?
                    .map_or(SessionSourceStatus::Unavailable, |source| source.status);
                return Ok(SourceProcessResult {
                    outcome: SourceProcessOutcome::CleanupPending,
                    status,
                    complete_byte_offset: cursor.complete_byte_offset,
                });
            }
        } else {
            let _ =
                self.state
                    .record_source_success(&source.source_key, generation, transition_at)?;
        }
        Ok(SourceProcessResult {
            outcome: SourceProcessOutcome::Advanced,
            status: SessionSourceStatus::Available,
            complete_byte_offset: cursor.complete_byte_offset,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_candidate_cursor(
        &mut self,
        source: &InspectedSource,
        generation: u64,
        file: &mut SessionFile,
        snapshot: &wokcore_platform::sessions::SessionFileSnapshot,
        modified_at: &str,
        head_fingerprint: [u8; 32],
        boundary_fingerprint: [u8; 32],
        replay: &ReplayState,
        transition_at: &str,
        cleanup_performed: &mut bool,
    ) -> Result<SessionScanCursor, CodexScannerError> {
        loop {
            if let Some(staging) = self
                .state
                .load_staging_session_scan_cursor(&source.source_key)?
            {
                if staging.generation == generation
                    && staging.file_identity.as_str() == source.identity
                    && staging.parser_checkpoint.version == PARSER_CHECKPOINT_VERSION
                    && snapshot.size >= staging.complete_byte_offset
                    && staging.parent_source_key == replay.parent_source_key
                    && staging.parent_generation == replay.parent_generation
                    && staging.replay_boundary_fingerprint == replay.boundary_fingerprint
                {
                    let (current_head, current_boundary) = fingerprints_with_extent(
                        file,
                        staging.complete_byte_offset,
                        staging.observed_size,
                        &self.domain_key,
                    )
                    .map_err(|_| CodexScannerError::Read)?;
                    if current_head == staging.head_fingerprint
                        && current_boundary == staging.boundary_fingerprint
                    {
                        match self.state.begin_or_resume_candidate(&staging)? {
                            CandidateBeginOutcome::Resumed(cursor) => return Ok(*cursor),
                            CandidateBeginOutcome::CleanupRequired { generation } => {
                                if !self.cleanup_generation_once(
                                    &source.source_key,
                                    generation,
                                    cleanup_performed,
                                )? {
                                    return Err(CodexScannerError::CleanupPending);
                                }
                                continue;
                            }
                            CandidateBeginOutcome::Started => return Ok(staging),
                        }
                    }
                }
                if !self.cleanup_generation_once(
                    &source.source_key,
                    staging.generation,
                    cleanup_performed,
                )? {
                    return Err(CodexScannerError::CleanupPending);
                }
                continue;
            }
            let cursor = SessionScanCursor {
                source_key: source.source_key.clone(),
                source_kind: SessionSourceKind::Codex,
                generation,
                generation_state: SessionGenerationState::Staging,
                file_identity: SessionFileIdentity::new(source.identity.clone())?,
                observed_size: snapshot.size,
                modified_at: modified_at.to_owned(),
                complete_byte_offset: 0,
                stable_record_ordinal: 0,
                parser_checkpoint: parser_checkpoint(
                    TokenTotals::default(),
                    None,
                    0,
                    0,
                    replay.initial_fingerprint,
                    replay,
                ),
                head_fingerprint,
                boundary_fingerprint,
                parent_source_key: replay.parent_source_key.clone(),
                parent_generation: replay.parent_generation,
                replay_boundary_fingerprint: replay.boundary_fingerprint,
                result_code: Some(SessionScanResultCode::Deferred),
                result_changed_at: Some(transition_at.to_owned()),
            };
            match self.state.begin_or_resume_candidate(&cursor)? {
                CandidateBeginOutcome::Started => return Ok(cursor),
                CandidateBeginOutcome::Resumed(cursor) => return Ok(*cursor),
                CandidateBeginOutcome::CleanupRequired { generation } => {
                    if !self.cleanup_generation_once(
                        &source.source_key,
                        generation,
                        cleanup_performed,
                    )? {
                        return Err(CodexScannerError::CleanupPending);
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_parser_batch(
        &mut self,
        mut cursor: SessionScanCursor,
        file: &mut SessionFile,
        source: &InspectedSource,
        snapshot: &wokcore_platform::sessions::SessionFileSnapshot,
        modified_at: &str,
        offset: u64,
        next_record_ordinal: u64,
        totals: TokenTotals,
        model: &str,
        event_ordinal: u64,
        verified_replay_events: u64,
        verified_replay_fingerprint: Option<[u8; 32]>,
        replay: &ReplayState,
        transition_at: &str,
        candidate: bool,
        terminal: bool,
        usage_records: Vec<SessionUsageRecord>,
        replay_signatures: Vec<CodexReplaySignature>,
        index: Option<SessionIndexRecord>,
    ) -> Result<SessionScanCursor, CodexScannerError> {
        let (head_fingerprint, boundary_fingerprint) =
            fingerprints(file, offset, &self.domain_key).map_err(|_| CodexScannerError::Read)?;
        cursor.observed_size = snapshot.size;
        cursor.modified_at = modified_at.to_owned();
        cursor.complete_byte_offset = offset;
        cursor.stable_record_ordinal = next_record_ordinal
            .checked_sub(1)
            .ok_or(CodexScannerError::Parse)?;
        cursor.parser_checkpoint = parser_checkpoint(
            totals,
            Some(model.to_owned()),
            event_ordinal,
            verified_replay_events,
            verified_replay_fingerprint,
            replay,
        );
        cursor.head_fingerprint = head_fingerprint;
        cursor.boundary_fingerprint = boundary_fingerprint;
        cursor.result_code = Some(if terminal {
            SessionScanResultCode::Advanced
        } else {
            SessionScanResultCode::Deferred
        });
        cursor.result_changed_at = Some(transition_at.to_owned());
        cursor.parent_source_key = replay.parent_source_key.clone();
        cursor.parent_generation = replay.parent_generation;
        cursor.replay_boundary_fingerprint = replay.boundary_fingerprint;
        let batch = SessionBatch {
            cursor: Some(cursor.clone()),
            index_records: index.into_iter().collect(),
            usage_records,
            replay_signatures,
            supplemental_metadata: Vec::new(),
        };
        if candidate {
            self.state.commit_candidate_batch(&batch)?;
        } else {
            self.state.commit_session_batch(&batch)?;
        }
        let _ = source;
        Ok(cursor)
    }

    fn cleanup_retired(
        &mut self,
        source_key: &str,
        cleanup_performed: &mut bool,
    ) -> Result<bool, CodexScannerError> {
        if let Some(generation) = self
            .state
            .load_session_source(source_key)?
            .and_then(|source| source.retired_generation)
        {
            return self.cleanup_generation_once(source_key, generation, cleanup_performed);
        }
        Ok(true)
    }

    fn cleanup_generation_once(
        &mut self,
        source_key: &str,
        generation: u64,
        cleanup_performed: &mut bool,
    ) -> Result<bool, CodexScannerError> {
        if *cleanup_performed {
            return Ok(false);
        }
        *cleanup_performed = true;
        self.cleanup_generation(source_key, generation)
    }

    fn cleanup_generation(
        &mut self,
        source_key: &str,
        generation: u64,
    ) -> Result<bool, CodexScannerError> {
        let outcome = self.state.cleanup_generation_batch(
            source_key,
            generation,
            MAX_SESSION_BATCH_ROWS,
            MAX_SESSION_BATCH_BYTES,
        )?;
        Ok(outcome.complete)
    }

    fn record_source_failure(
        &mut self,
        source: &InspectedSource,
        discovered: &DiscoveredSession,
        code: SessionSourceErrorCode,
        transition_at: &str,
    ) -> Result<(), CodexScannerError> {
        if let Some(generation) = self
            .state
            .load_session_source(&source.source_key)?
            .and_then(|state| state.staging_generation.or(state.current_generation))
        {
            let _ =
                self.state
                    .fail_candidate(&source.source_key, generation, code, transition_at)?;
            return Ok(());
        }
        self.metrics.source_opens = self.metrics.source_opens.saturating_add(1);
        let mut file = discovered
            .open(&self.root, u64::MAX)
            .map_err(|_| CodexScannerError::Read)?;
        let snapshot = file.snapshot().clone();
        let modified_at = system_time_utc(snapshot.modified);
        let (head, boundary) =
            fingerprints(&mut file, 0, &self.domain_key).map_err(|_| CodexScannerError::Read)?;
        let generation = 1;
        let replay = ReplayState::deferred(code);
        let cursor = SessionScanCursor {
            source_key: source.source_key.clone(),
            source_kind: SessionSourceKind::Codex,
            generation,
            generation_state: SessionGenerationState::Staging,
            file_identity: SessionFileIdentity::new(source.identity.clone())?,
            observed_size: snapshot.size,
            modified_at,
            complete_byte_offset: 0,
            stable_record_ordinal: 0,
            parser_checkpoint: parser_checkpoint(
                TokenTotals::default(),
                None,
                0,
                0,
                replay.initial_fingerprint,
                &replay,
            ),
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
                if !self.cleanup_generation(&source.source_key, generation)? {
                    return Err(CodexScannerError::CleanupPending);
                }
                let _ = self.state.begin_or_resume_candidate(&cursor)?;
            }
            CandidateBeginOutcome::Started | CandidateBeginOutcome::Resumed(_) => {}
        }
        let _ = self
            .state
            .fail_candidate(&source.source_key, generation, code, transition_at)?;
        Ok(())
    }

    fn record_source_failure_if_possible(
        &mut self,
        source: &InspectedSource,
        discovered: &DiscoveredSession,
        code: SessionSourceErrorCode,
        transition_at: &str,
    ) -> Result<(), CodexScannerError> {
        match self.record_source_failure(source, discovered, code, transition_at) {
            Err(CodexScannerError::Read) => Ok(()),
            result => result,
        }
    }

    pub fn title_for_source(
        &mut self,
        source_key: &str,
    ) -> Result<Option<SessionTitle>, CodexScannerError> {
        let Some(source_state) = self.state.load_session_source(source_key)? else {
            return Ok(None);
        };
        if source_state.source_kind != SessionSourceKind::Codex {
            return Ok(None);
        }
        let Some(cursor) = self.state.load_current_session_scan_cursor(source_key)? else {
            return Ok(None);
        };
        let mut file = match open_source_for_paging(&self.root, &self.domain_key, &cursor, u64::MAX)
        {
            Ok(file) => file,
            Err(CodexScannerError::Storage(error)) => {
                return Err(CodexScannerError::Storage(error));
            }
            Err(_) => return Ok(None),
        };
        self.metrics.source_opens = self.metrics.source_opens.saturating_add(1);
        let metadata = match self.inspect_pinned_source(&mut file) {
            Ok(metadata) => metadata,
            Err(
                SourceInspectionError::RecordTooLarge
                | SourceInspectionError::Read
                | SourceInspectionError::Parse,
            ) => return Ok(None),
        };
        if let Some(title) = self.read_session_index_title(&metadata.root_thread_id)? {
            return Ok(Some(title));
        }
        self.read_immutable_database_title(&metadata.root_thread_id)
    }

    fn read_session_index_title(
        &mut self,
        target_thread_id: &str,
    ) -> Result<Option<SessionTitle>, CodexScannerError> {
        let mut file = match self
            .root
            .open_file("session_index.jsonl", TITLE_INDEX_LIMIT_BYTES)
        {
            Ok(file) => file,
            Err(_) => return Ok(None),
        };
        self.metrics.source_opens = self.metrics.source_opens.saturating_add(1);
        let mut reader = JsonlReader::new(JsonlCursor::new(0, 1));
        let mut output = None;
        let mut rows = 0usize;
        loop {
            let scan = reader
                .scan(&mut file)
                .map_err(|_| CodexScannerError::Read)?;
            let empty = scan.records.is_empty();
            for record in scan.records {
                if rows >= TITLE_ROW_LIMIT {
                    return Ok(None);
                }
                rows += 1;
                if record.status != JsonlRecordStatus::Valid {
                    continue;
                }
                let object = match record.value().as_object() {
                    Some(object) => object,
                    None => continue,
                };
                let Some(id) = object.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let Some(title) = object.get("thread_name").and_then(Value::as_str) else {
                    continue;
                };
                if id != target_thread_id || title.is_empty() || title.len() > TITLE_LIMIT_BYTES {
                    continue;
                }
                output = Some(SessionTitle(title.to_owned()));
            }
            if scan.reached_end || empty {
                break;
            }
        }
        Ok(output)
    }

    fn read_immutable_database_title(
        &mut self,
        target_thread_id: &str,
    ) -> Result<Option<SessionTitle>, CodexScannerError> {
        for name in ["state_5.sqlite", "state.sqlite"] {
            let mut pinned = match self.root.open_file(name, TITLE_DATABASE_LIMIT_BYTES) {
                Ok(file) => file,
                Err(_) => continue,
            };
            let identity = pinned.snapshot().identity;
            if let Some(expected) = self.title_database_identities.get(name) {
                if *expected != identity {
                    continue;
                }
            } else {
                self.title_database_identities
                    .insert(name.to_owned(), identity);
            }
            #[cfg(test)]
            test_hooks::run_before_title_sqlite_open();
            let path = self.root_path.join(name);
            let uri = immutable_sqlite_uri(&path);
            let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX;
            let output = Connection::open_with_flags(uri, flags)
                .ok()
                .and_then(|connection| {
                    query_indexed_title(&connection, target_thread_id, TITLE_SQLITE_VM_BUDGET)
                        .ok()
                        .flatten()
                });
            #[cfg(test)]
            test_hooks::run_before_title_sqlite_revalidate();
            pinned
                .read_range_bounded(0, 0)
                .map_err(|_| CodexScannerError::Read)?;
            if output.is_some() {
                return Ok(output);
            }
        }
        Ok(None)
    }
}

fn query_indexed_title(
    connection: &Connection,
    target_thread_id: &str,
    vm_budget: usize,
) -> rusqlite::Result<Option<SessionTitle>> {
    connection.execute_batch(
        "PRAGMA query_only = ON;
         PRAGMA cache_size = -256;
         PRAGMA automatic_index = OFF;
         PRAGMA temp_store = MEMORY;",
    )?;
    connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, TITLE_SQLITE_LENGTH_LIMIT_BYTES)?;
    connection.set_limit(Limit::SQLITE_LIMIT_WORKER_THREADS, 0)?;
    let progress_granularity = TITLE_SQLITE_VM_GRANULARITY.min(vm_budget.max(1));
    let executed = Arc::new(AtomicUsize::new(0));
    let progress = Arc::clone(&executed);
    connection.progress_handler(
        progress_granularity as i32,
        Some(move || {
            progress
                .fetch_add(progress_granularity, AtomicOrdering::Relaxed)
                .saturating_add(progress_granularity)
                > vm_budget
        }),
    )?;

    const TITLE_SQL: &str = "SELECT title
         FROM threads
         WHERE id = ?1
           AND typeof(title) = 'text'
           AND length(CAST(title AS BLOB)) BETWEEN 1 AND ?2
         LIMIT 1";
    let mut plan = connection.prepare(
        "EXPLAIN QUERY PLAN
         SELECT title
         FROM threads
         WHERE id = ?1
           AND typeof(title) = 'text'
           AND length(CAST(title AS BLOB)) BETWEEN 1 AND ?2
         LIMIT 1",
    )?;
    let mut rows = plan.query(rusqlite::params![
        target_thread_id,
        TITLE_LIMIT_BYTES as i64
    ])?;
    let mut plan_rows = 0usize;
    let mut indexed_lookup = false;
    while let Some(row) = rows.next()? {
        plan_rows += 1;
        if plan_rows > TITLE_SQLITE_PLAN_ROW_LIMIT {
            return Ok(None);
        }
        let detail = row.get::<_, String>(3)?;
        if detail.len() > 1_024 {
            return Ok(None);
        }
        let detail = detail.to_ascii_uppercase();
        if detail.contains("SCAN") || detail.contains("TEMP B-TREE") {
            return Ok(None);
        }
        if detail.contains("SEARCH THREADS") && detail.contains("ID=?") {
            indexed_lookup = true;
        }
    }
    drop(rows);
    drop(plan);
    if !indexed_lookup {
        return Ok(None);
    }

    let title = connection
        .query_row(
            TITLE_SQL,
            rusqlite::params![target_thread_id, TITLE_LIMIT_BYTES as i64],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(title
        .filter(|title| !title.is_empty() && title.len() <= TITLE_LIMIT_BYTES)
        .map(SessionTitle))
}

struct InspectedSource {
    discovered_index: usize,
    source_key: String,
    identity: String,
    metadata: Option<SourceMetadata>,
    inspection_error: Option<SourceInspectionError>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CodexScannerSlicePhase {
    Discovering,
    Processing,
}

struct CodexSliceCycle {
    transition_at: String,
    phase: CodexScannerSlicePhase,
    cursor: SessionDiscoveryCursor,
    persisted_sources: HashSet<String>,
    persisted_identities: HashMap<String, String>,
    reserved_keys: HashMap<String, String>,
    current_sources: HashSet<String>,
    identity_to_index: HashMap<String, usize>,
    inspected: Vec<InspectedSource>,
    thread_sources: HashMap<String, Vec<usize>>,
    replay_topology: ReplayTopology,
    replay_groups: HashMap<(usize, String), Vec<usize>>,
    referenced_parent_indices: HashSet<usize>,
    processing_depths: Vec<usize>,
    processing_depth_index: usize,
    processed_indices: HashSet<usize>,
    replay_cache: ReplayGroupCache,
    needs_rescan: bool,
}

impl CodexSliceCycle {
    fn new(scanner: &CodexScanner, transition_at: &str) -> Result<Self, CodexScannerError> {
        Ok(Self {
            transition_at: transition_at.to_owned(),
            phase: CodexScannerSlicePhase::Discovering,
            cursor: SessionDiscoveryCursor::with_limits(
                SessionDiscoveryKind::Codex,
                scanner.discovery_limits,
            )
            .map_err(CodexScannerError::Discovery)?,
            persisted_sources: scanner.persisted_codex_source_keys()?,
            persisted_identities: scanner.persisted_identity_sources()?,
            reserved_keys: HashMap::new(),
            current_sources: HashSet::new(),
            identity_to_index: HashMap::new(),
            inspected: Vec::new(),
            thread_sources: HashMap::new(),
            replay_topology: ReplayTopology {
                depths: Vec::new(),
                errors: Vec::new(),
            },
            replay_groups: HashMap::new(),
            referenced_parent_indices: HashSet::new(),
            processing_depths: Vec::new(),
            processing_depth_index: 0,
            processed_indices: HashSet::new(),
            replay_cache: ReplayGroupCache::default(),
            needs_rescan: false,
        })
    }

    fn observe(
        &mut self,
        scanner: &mut CodexScanner,
        source: &DiscoveredSession,
    ) -> Result<(), CodexScannerError> {
        let identity = opaque_file_identity(&scanner.domain_key, source.identity());
        if self.identity_to_index.contains_key(&identity) {
            return Ok(());
        }
        let source_key = if let Some(source_key) = self.persisted_identities.get(&identity) {
            source_key.clone()
        } else {
            let path_key = scanner.path_source_key(source);
            if !self.reserved_keys.contains_key(&path_key) {
                path_key
            } else {
                let mut counter = 0u64;
                loop {
                    let candidate = scanner.collision_source_key(source, &identity, counter);
                    if !self.reserved_keys.contains_key(&candidate)
                        && !self.persisted_sources.contains(&candidate)
                    {
                        break candidate;
                    }
                    counter = counter.checked_add(1).ok_or(CodexScannerError::Parse)?;
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
        let (metadata, inspection_error) = match scanner.inspect_source(source) {
            Ok(metadata) => (Some(metadata), None),
            Err(error) => (None, Some(error)),
        };
        let index = self.inspected.len();
        self.inspected.push(InspectedSource {
            discovered_index: 0,
            source_key: source_key.clone(),
            identity: identity.clone(),
            metadata,
            inspection_error,
        });
        self.current_sources.insert(source_key);
        self.identity_to_index.insert(identity, index);
        Ok(())
    }

    fn prepare_processing(&mut self) {
        self.thread_sources.clear();
        for (index, source) in self.inspected.iter().enumerate() {
            if let Some(metadata) = &source.metadata {
                self.thread_sources
                    .entry(metadata.root_thread_id.clone())
                    .or_default()
                    .push(index);
            }
        }
        self.replay_topology = ReplayTopology::build(&self.inspected, &self.thread_sources);
        self.referenced_parent_indices = self
            .inspected
            .iter()
            .filter_map(|source| source.metadata.as_ref()?.parent_thread_id.as_ref())
            .filter_map(|parent_id| self.thread_sources.get(parent_id))
            .flatten()
            .copied()
            .collect();
        self.processing_depths = self.replay_topology.depths.clone();
        self.processing_depths.sort_unstable();
        self.processing_depths.dedup();
        self.replay_groups.clear();
        for (index, source) in self.inspected.iter().enumerate() {
            let Some(parent_thread_id) = source
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.parent_thread_id.as_ref())
            else {
                continue;
            };
            self.replay_groups
                .entry((self.replay_topology.depths[index], parent_thread_id.clone()))
                .or_default()
                .push(index);
        }
        self.phase = CodexScannerSlicePhase::Processing;
        self.processing_depth_index = 0;
    }
}

fn map_codex_slice_error(error: SessionDiscoverySliceError) -> CodexScannerError {
    match error {
        SessionDiscoverySliceError::Discovery(error) => CodexScannerError::Discovery(error),
        SessionDiscoverySliceError::CursorKindMismatch => {
            CodexScannerError::Discovery(DiscoveryError::Unsafe)
        }
    }
}

#[derive(Clone)]
struct SourceMetadata {
    root_thread_id: String,
    created_at: String,
    parent_thread_id: Option<String>,
}

struct ReplayTopology {
    depths: Vec<usize>,
    errors: Vec<Option<SessionSourceErrorCode>>,
}

#[derive(Clone, Copy)]
enum ReplayEdge {
    Root,
    Parent(usize),
    Invalid(SessionSourceErrorCode),
}

impl ReplayTopology {
    fn build(inspected: &[InspectedSource], thread_sources: &HashMap<String, Vec<usize>>) -> Self {
        const UNRESOLVED: usize = usize::MAX;
        const INVALID_DEPTH: usize = usize::MAX - 1;

        let edge_for = |index: usize| {
            let Some(metadata) = inspected[index].metadata.as_ref() else {
                return ReplayEdge::Root;
            };
            let Some(parent_thread_id) = metadata.parent_thread_id.as_ref() else {
                return ReplayEdge::Root;
            };
            let Some(parent_indices) = thread_sources.get(parent_thread_id) else {
                return ReplayEdge::Invalid(SessionSourceErrorCode::SourceReplayParentMissing);
            };
            if parent_indices.len() != 1 || parent_indices[0] == index {
                return ReplayEdge::Invalid(SessionSourceErrorCode::SourceReplayParentAmbiguous);
            }
            ReplayEdge::Parent(parent_indices[0])
        };

        let mut depths = vec![UNRESOLVED; inspected.len()];
        let mut errors = vec![None; inspected.len()];
        let mut visiting = vec![false; inspected.len()];
        let mut path = Vec::new();
        for start in 0..inspected.len() {
            if depths[start] != UNRESOLVED {
                continue;
            }
            path.clear();
            let mut current = start;
            loop {
                if depths[current] != UNRESOLVED {
                    break;
                }
                if visiting[current] {
                    let cycle_start = path
                        .iter()
                        .position(|candidate| *candidate == current)
                        .expect("a visiting replay node belongs to the current path");
                    for &cycle_node in &path[cycle_start..] {
                        depths[cycle_node] = INVALID_DEPTH;
                        errors[cycle_node] = Some(SessionSourceErrorCode::SourceReplayInconsistent);
                        visiting[cycle_node] = false;
                    }
                    break;
                }
                visiting[current] = true;
                path.push(current);
                match edge_for(current) {
                    ReplayEdge::Root => {
                        depths[current] = 0;
                        visiting[current] = false;
                        break;
                    }
                    ReplayEdge::Invalid(code) => {
                        depths[current] = INVALID_DEPTH;
                        errors[current] = Some(code);
                        visiting[current] = false;
                        break;
                    }
                    ReplayEdge::Parent(parent) => current = parent,
                }
            }

            for &node in path.iter().rev() {
                if depths[node] != UNRESOLVED {
                    continue;
                }
                match edge_for(node) {
                    ReplayEdge::Root => depths[node] = 0,
                    ReplayEdge::Invalid(code) => {
                        depths[node] = INVALID_DEPTH;
                        errors[node] = Some(code);
                    }
                    ReplayEdge::Parent(parent) => {
                        if errors[parent].is_some() {
                            depths[node] = INVALID_DEPTH;
                            errors[node] = Some(SessionSourceErrorCode::SourceReplayInconsistent);
                        } else if let Some(depth) = depths[parent].checked_add(1) {
                            depths[node] = depth;
                        } else {
                            depths[node] = INVALID_DEPTH;
                            errors[node] = Some(SessionSourceErrorCode::SourceReplayInconsistent);
                        }
                    }
                }
                visiting[node] = false;
            }
        }
        Self { depths, errors }
    }
}

#[derive(Debug)]
enum SourceInspectionError {
    RecordTooLarge,
    Read,
    Parse,
}

impl From<JsonlError> for SourceInspectionError {
    fn from(error: JsonlError) -> Self {
        match error {
            JsonlError::RecordTooLarge { .. } => Self::RecordTooLarge,
            JsonlError::SourceChanged
            | JsonlError::SourceUnavailable
            | JsonlError::ReadFailed
            | JsonlError::CursorOverflow => Self::Read,
        }
    }
}

#[derive(Clone, Debug)]
struct ReplayState {
    resolution: ReplayResolution,
    parent_source_key: Option<String>,
    parent_generation: Option<u64>,
    boundary_fingerprint: Option<[u8; 32]>,
    initial_fingerprint: Option<[u8; 32]>,
    replayed_events: u64,
}

impl ReplayState {
    fn not_forked() -> Self {
        Self {
            resolution: ReplayResolution::NotForked,
            parent_source_key: None,
            parent_generation: None,
            boundary_fingerprint: None,
            initial_fingerprint: None,
            replayed_events: 0,
        }
    }

    fn deferred(code: SessionSourceErrorCode) -> Self {
        Self {
            resolution: ReplayResolution::Deferred(code),
            parent_source_key: None,
            parent_generation: None,
            boundary_fingerprint: None,
            initial_fingerprint: None,
            replayed_events: 0,
        }
    }

    fn resolved(
        parent_source_key: String,
        parent_generation: u64,
        replayed_events: u64,
        boundary_fingerprint: [u8; 32],
        initial_fingerprint: [u8; 32],
    ) -> Self {
        Self {
            resolution: ReplayResolution::Resolved { replayed_events },
            parent_source_key: Some(parent_source_key),
            parent_generation: Some(parent_generation),
            boundary_fingerprint: Some(boundary_fingerprint),
            initial_fingerprint: Some(initial_fingerprint),
            replayed_events,
        }
    }
}

fn maximum_replay_page_retained_bytes(limit: usize) -> usize {
    const OPAQUE_SOURCE_KEY_BYTES: usize = 64;
    const CANONICAL_TIMESTAMP_BYTES: usize = 20;

    std::mem::size_of::<CodexReplaySignaturePage>()
        .saturating_add(
            limit.saturating_add(1).saturating_mul(
                std::mem::size_of::<CodexReplaySignature>()
                    .saturating_add(OPAQUE_SOURCE_KEY_BYTES)
                    .saturating_add(CANONICAL_TIMESTAMP_BYTES),
            ),
        )
        .saturating_add(OPAQUE_SOURCE_KEY_BYTES)
}

fn replay_page_retained_bytes(page: &CodexReplaySignaturePage) -> usize {
    std::mem::size_of_val(page)
        .saturating_add(
            page.items
                .capacity()
                .saturating_mul(std::mem::size_of::<CodexReplaySignature>()),
        )
        .saturating_add(page.items.iter().fold(0usize, |total, signature| {
            total
                .saturating_add(signature.parent_source_key.capacity())
                .saturating_add(signature.occurred_at.capacity())
        }))
        .saturating_add(
            page.next_page_key
                .as_ref()
                .map_or(0, |key| key.parent_source_key().len()),
        )
}

struct CachedReplayBoundary {
    work_index: u32,
    replayed_events: u32,
    boundary_fingerprint: [u8; 32],
}

struct ReplayGroupCache {
    parent_index: Option<usize>,
    parent_source_key: Option<String>,
    parent_generation: Option<u64>,
    initial_fingerprint: Option<[u8; 32]>,
    group_error: Option<SessionSourceErrorCode>,
    boundaries: Vec<CachedReplayBoundary>,
}

impl Default for ReplayGroupCache {
    fn default() -> Self {
        Self {
            parent_index: None,
            parent_source_key: None,
            parent_generation: None,
            initial_fingerprint: None,
            group_error: Some(SessionSourceErrorCode::SourceReplayInconsistent),
            boundaries: Vec::new(),
        }
    }
}

impl ReplayGroupCache {
    fn reset(&mut self, parent_index: usize) {
        self.parent_index = Some(parent_index);
        self.parent_source_key = None;
        self.parent_generation = None;
        self.initial_fingerprint = None;
        self.group_error = Some(SessionSourceErrorCode::SourceReplayInconsistent);
        self.boundaries = Vec::new();
    }

    fn resolution_for(&self, work_index: usize) -> ReplayState {
        if let Some(code) = self.group_error {
            return ReplayState::deferred(code);
        }
        let Ok(work_index) = u32::try_from(work_index) else {
            return ReplayState::deferred(SessionSourceErrorCode::SourceReplayInconsistent);
        };
        let Ok(boundary_index) = self
            .boundaries
            .binary_search_by_key(&work_index, |boundary| boundary.work_index)
        else {
            return ReplayState::deferred(SessionSourceErrorCode::SourceReplayInconsistent);
        };
        let boundary = &self.boundaries[boundary_index];
        ReplayState::resolved(
            self.parent_source_key
                .as_ref()
                .expect("resolved replay cache has one parent key")
                .clone(),
            self.parent_generation
                .expect("resolved replay cache has one parent generation"),
            u64::from(boundary.replayed_events),
            boundary.boundary_fingerprint,
            self.initial_fingerprint
                .expect("resolved replay cache has one initial fingerprint"),
        )
    }

    fn retained_bytes(&self) -> usize {
        self.boundaries
            .capacity()
            .saturating_mul(std::mem::size_of::<CachedReplayBoundary>())
            .saturating_add(
                self.parent_source_key
                    .as_ref()
                    .map_or(0, |key| key.capacity()),
            )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceProcessOutcome {
    Advanced,
    Unchanged,
    Interrupted,
    CleanupPending,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SourceProcessResult {
    outcome: SourceProcessOutcome,
    status: SessionSourceStatus,
    complete_byte_offset: u64,
}

fn parser_checkpoint(
    totals: TokenTotals,
    model: Option<String>,
    event_ordinal: u64,
    verified_replay_events: u64,
    verified_replay_fingerprint: Option<[u8; 32]>,
    replay: &ReplayState,
) -> ParserCheckpoint {
    ParserCheckpoint {
        version: PARSER_CHECKPOINT_VERSION,
        previous_input_tokens: totals.input,
        previous_output_tokens: totals.output,
        previous_cache_read_tokens: totals.cache_read,
        previous_cache_write_tokens: totals.cache_write,
        previous_reasoning_tokens: totals.reasoning,
        current_model: model,
        event_ordinal,
        lineage_source_key: replay.parent_source_key.clone(),
        lineage_generation: replay.parent_generation,
        lineage_record_ordinal: verified_replay_events,
        structural_hash: verified_replay_fingerprint,
    }
}

fn maximum_timestamp(current: Option<String>, candidate: &str) -> Option<String> {
    Some(match current {
        Some(current) if current.as_str() >= candidate => current,
        _ => candidate.to_owned(),
    })
}

fn metadata_matches(meta: &CodexSessionMeta, expected: &SourceMetadata) -> bool {
    meta.root_thread_id == expected.root_thread_id
        && meta.created_at == expected.created_at
        && meta.parent_thread_id == expected.parent_thread_id
}

fn map_jsonl_scanner_error(error: JsonlError) -> CodexScannerError {
    match error {
        JsonlError::RecordTooLarge { .. } => CodexScannerError::RecordTooLarge,
        JsonlError::CursorOverflow => CodexScannerError::Parse,
        JsonlError::SourceChanged | JsonlError::SourceUnavailable | JsonlError::ReadFailed => {
            CodexScannerError::Read
        }
    }
}

fn session_index(
    key: &[u8; 32],
    source: &InspectedSource,
    metadata: &SourceMetadata,
    generation: u64,
    last_active_at: &str,
    message_count: u64,
    usage_event_count: u64,
) -> SessionIndexRecord {
    SessionIndexRecord {
        session_key: session_key(key, &metadata.root_thread_id),
        source_key: source.source_key.clone(),
        generation,
        source_kind: SessionSourceKind::Codex,
        created_at: metadata.created_at.clone(),
        last_active_at: last_active_at.to_owned(),
        message_count,
        usage_event_count,
        availability: SessionAvailability::Available,
    }
}

#[cfg(test)]
mod test_hooks {
    use std::{
        cell::{Cell, RefCell},
        fs,
        path::PathBuf,
    };

    thread_local! {
        static DELETE_AFTER_DISCOVERY: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
        static BEFORE_TITLE_SQLITE_OPEN: RefCell<Option<Box<dyn FnOnce()>>> =
            const { RefCell::new(None) };
        static BEFORE_TITLE_SQLITE_REVALIDATE: RefCell<Option<Box<dyn FnOnce()>>> =
            const { RefCell::new(None) };
        static REPLAY_GROUP_CHILD_VISITS: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn delete_after_discovery(path: PathBuf) {
        DELETE_AFTER_DISCOVERY.with(|pending| {
            assert!(pending.borrow_mut().replace(path).is_none());
        });
    }

    pub(super) fn run_after_discovery() {
        DELETE_AFTER_DISCOVERY.with(|pending| {
            if let Some(path) = pending.borrow_mut().take() {
                fs::remove_file(path).expect("test source must disappear after discovery");
            }
        });
    }

    pub(super) fn before_title_sqlite_open(action: impl FnOnce() + 'static) {
        BEFORE_TITLE_SQLITE_OPEN.with(|pending| {
            assert!(pending.borrow_mut().replace(Box::new(action)).is_none());
        });
    }

    pub(super) fn run_before_title_sqlite_open() {
        BEFORE_TITLE_SQLITE_OPEN.with(|pending| {
            if let Some(action) = pending.borrow_mut().take() {
                action();
            }
        });
    }

    pub(super) fn before_title_sqlite_revalidate(action: impl FnOnce() + 'static) {
        BEFORE_TITLE_SQLITE_REVALIDATE.with(|pending| {
            assert!(pending.borrow_mut().replace(Box::new(action)).is_none());
        });
    }

    pub(super) fn run_before_title_sqlite_revalidate() {
        BEFORE_TITLE_SQLITE_REVALIDATE.with(|pending| {
            if let Some(action) = pending.borrow_mut().take() {
                action();
            }
        });
    }

    pub(super) fn reset_replay_group_child_visits() {
        REPLAY_GROUP_CHILD_VISITS.set(0);
    }

    pub(super) fn note_replay_group_child_visit() {
        REPLAY_GROUP_CHILD_VISITS.set(REPLAY_GROUP_CHILD_VISITS.get().saturating_add(1));
    }

    pub(super) fn replay_group_child_visits() -> usize {
        REPLAY_GROUP_CHILD_VISITS.get()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        path::Path,
        sync::{Arc, Mutex},
    };

    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::{
        CodexScanSummary, CodexScanner, ScanControl, ScanOutcome, SessionSourceErrorCode,
        SessionSourceStatus, session_key, test_hooks,
    };

    const TEST_DOMAIN_KEY: [u8; 32] = [0x35; 32];
    const NOW: &str = "2026-07-26T12:00:00Z";

    fn scanner(root: &TempDir, state: &TempDir) -> CodexScanner {
        CodexScanner::open(
            root.path(),
            state.path().join("state.sqlite3"),
            TEST_DOMAIN_KEY,
        )
        .unwrap()
    }

    fn write_session(root: &Path, relative: &str, lines: &[serde_json::Value]) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut bytes = Vec::new();
        for line in lines {
            serde_json::to_writer(&mut bytes, line).unwrap();
            bytes.push(b'\n');
        }
        fs::write(path, bytes).unwrap();
    }

    fn meta(id: &str, timestamp: &str) -> serde_json::Value {
        serde_json::json!({
            "timestamp": timestamp,
            "type": "session_meta",
            "payload": {"id": id}
        })
    }

    fn token(timestamp: &str, input: u64, output: u64) -> serde_json::Value {
        serde_json::json!({
            "timestamp": timestamp,
            "type":"event_msg",
            "payload":{"type":"token_count","info":{"total_token_usage":{
                "input_tokens":input,
                "output_tokens":output
            }}}
        })
    }

    fn find_session<'a>(
        summary: &'a CodexScanSummary,
        thread_id: &str,
    ) -> &'a super::SourceScanSummary {
        let expected = session_key(&TEST_DOMAIN_KEY, thread_id);
        summary
            .sources
            .iter()
            .find(|source| source.session_key.as_deref() == Some(expected.as_str()))
            .unwrap()
    }

    fn write_title_database(path: &Path, thread_id: &str, title: &str) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    title TEXT NOT NULL
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads (id, title) VALUES (?1, ?2)",
                rusqlite::params![thread_id, title],
            )
            .unwrap();
    }

    fn visible_title(
        result: &Result<Option<super::SessionTitle>, super::CodexScannerError>,
    ) -> Option<&str> {
        result
            .as_ref()
            .ok()
            .and_then(|title| title.as_ref())
            .map(super::SessionTitle::as_str)
    }

    #[test]
    fn source_disappearing_after_discovery_is_io_failed_and_does_not_stop_its_sibling() {
        let root = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let sessions = root.path().join("sessions/2026/07/26");
        fs::create_dir_all(&sessions).unwrap();
        let vanished = sessions.join("a-vanished.jsonl");
        fs::write(
            &vanished,
            concat!(
                "{\"timestamp\":\"2026-07-26T12:00:00Z\",\"type\":\"session_meta\",",
                "\"payload\":{\"id\":\"vanished\"}}\n"
            ),
        )
        .unwrap();
        fs::write(
            sessions.join("z-sibling.jsonl"),
            concat!(
                "{\"timestamp\":\"2026-07-26T12:00:00Z\",\"type\":\"session_meta\",",
                "\"payload\":{\"id\":\"sibling\"}}\n"
            ),
        )
        .unwrap();
        let mut scanner = CodexScanner::open(
            root.path(),
            state.path().join("state.sqlite3"),
            TEST_DOMAIN_KEY,
        )
        .unwrap();
        test_hooks::delete_after_discovery(vanished);

        let summary = scanner
            .scan("2026-07-26T12:00:00Z", ScanControl::default())
            .unwrap();

        assert_eq!(summary.outcome, ScanOutcome::Complete);
        let failed = summary
            .sources
            .iter()
            .find(|source| source.error_code.is_some())
            .unwrap();
        assert_eq!(
            failed.error_code,
            Some(SessionSourceErrorCode::SourceIoFailed)
        );
        assert_eq!(failed.status, SessionSourceStatus::Unavailable);
        assert_eq!(failed.complete_byte_offset, 0);
        let sibling = summary
            .sources
            .iter()
            .find(|source| {
                source.session_key.as_deref()
                    == Some(session_key(&TEST_DOMAIN_KEY, "sibling").as_str())
            })
            .unwrap();
        assert_eq!(sibling.status, SessionSourceStatus::Available);
        assert_eq!(sibling.error_code, None);
    }

    #[test]
    fn replay_limit_is_a_stable_resource_outcome_without_a_public_test_api() {
        let root = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let mut lines = vec![meta("limited-rollout", NOW)];
        for ordinal in 1..=4 {
            lines.push(token("2026-07-26T12:00:01Z", ordinal, 1));
        }
        write_session(root.path(), "sessions/2026/07/26/limited.jsonl", &lines);
        let mut scanner = scanner(&root, &state);
        scanner.replay_limit = 3;

        let summary = scanner.scan(NOW, ScanControl::default()).unwrap();
        let source_summary = find_session(&summary, "limited-rollout");
        assert_eq!(source_summary.status, SessionSourceStatus::ResourceLimited);
        assert_eq!(
            source_summary.error_code,
            Some(SessionSourceErrorCode::SourceReplayLimit)
        );
        let persisted = scanner
            .state()
            .load_session_source(&source_summary.source_key)
            .unwrap()
            .unwrap();
        assert_eq!(persisted.status, SessionSourceStatus::ResourceLimited);
        assert_eq!(
            persisted.error_code,
            Some(SessionSourceErrorCode::SourceReplayLimit)
        );
        assert!(persisted.current_generation.is_none());
    }

    #[test]
    fn replay_limit_keeps_a_current_generation_stale_without_a_public_test_api() {
        let root = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let relative = "sessions/2026/07/26/current-replay-limit.jsonl";
        write_session(
            root.path(),
            relative,
            &[
                meta("current-replay-limit", NOW),
                token("2026-07-26T12:00:01Z", 1, 1),
                token("2026-07-26T12:00:02Z", 2, 2),
            ],
        );
        let mut scanner = scanner(&root, &state);
        scanner.replay_limit = 3;
        let first = scanner.scan(NOW, ScanControl::default()).unwrap();
        let key = find_session(&first, "current-replay-limit")
            .source_key
            .clone();
        let mut writer = OpenOptions::new()
            .append(true)
            .open(root.path().join(relative))
            .unwrap();
        for ordinal in 3..=4 {
            serde_json::to_writer(
                &mut writer,
                &token("2026-07-26T12:00:03Z", ordinal, ordinal),
            )
            .unwrap();
            writer.write_all(b"\n").unwrap();
        }
        writer.flush().unwrap();
        drop(writer);

        let limited = scanner
            .scan("2026-07-26T12:01:00Z", ScanControl::default())
            .unwrap();
        let source_summary = find_session(&limited, "current-replay-limit");
        assert_eq!(source_summary.status, SessionSourceStatus::Stale);
        assert_eq!(
            source_summary.error_code,
            Some(SessionSourceErrorCode::SourceReplayLimit)
        );
        let persisted = scanner.state().load_session_source(&key).unwrap().unwrap();
        assert_eq!(persisted.status, SessionSourceStatus::Stale);
        assert_eq!(
            persisted.error_code,
            Some(SessionSourceErrorCode::SourceReplayLimit)
        );
    }

    #[test]
    fn replay_resolution_resource_failure_is_contained_without_a_public_test_api() {
        let root = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let mut parent = vec![meta("contained-parent", NOW)];
        for ordinal in 1..=4 {
            parent.push(token(&format!("2026-07-26T12:00:0{ordinal}Z"), ordinal, 1));
        }
        write_session(root.path(), "sessions/2026/07/26/parent.jsonl", &parent);
        let mut initial = scanner(&root, &state);
        initial.scan(NOW, ScanControl::default()).unwrap();
        drop(initial);

        let mut child_meta = meta("contained-child", "2026-07-26T12:00:05Z");
        child_meta["payload"]["parent_thread_id"] = serde_json::json!("contained-parent");
        let mut child = vec![child_meta];
        child.extend(parent.iter().skip(1).cloned());
        child.push(token("2026-07-26T12:00:06Z", 9, 2));
        write_session(root.path(), "sessions/2026/07/26/child.jsonl", &child);
        write_session(
            root.path(),
            "sessions/2026/07/26/sibling.jsonl",
            &[
                meta("contained-sibling", NOW),
                token("2026-07-26T12:00:01Z", 3, 1),
            ],
        );

        let mut limited = scanner(&root, &state);
        limited.replay_limit = 3;
        let summary = limited.scan(NOW, ScanControl::default()).unwrap();
        let child = find_session(&summary, "contained-child");
        assert_eq!(child.status, SessionSourceStatus::ResourceLimited);
        assert_eq!(
            child.error_code,
            Some(SessionSourceErrorCode::SourceReplayLimit)
        );
        let sibling = find_session(&summary, "contained-sibling");
        assert_eq!(sibling.status, SessionSourceStatus::Available);
        assert!(
            limited
                .state()
                .load_current_generation(&sibling.source_key)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn sqlite_title_path_replacement_after_pin_never_exposes_replacement_content() {
        const THREAD_ID: &str = "title-path-race";
        const TRUSTED_TITLE: &str = "trusted-before-race";
        const REPLACEMENT_CANARY: &str = "REPLACEMENT-TITLE-CANARY";

        let root = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        write_session(
            root.path(),
            "sessions/2026/07/26/title-path-race.jsonl",
            &[meta(THREAD_ID, NOW)],
        );
        let target = root.path().join("state_5.sqlite");
        let replacement = root.path().join("replacement.sqlite");
        let displaced = root.path().join("displaced.sqlite");
        write_title_database(&target, THREAD_ID, TRUSTED_TITLE);
        write_title_database(&replacement, THREAD_ID, REPLACEMENT_CANARY);

        let mut scanner = scanner(&root, &state);
        let summary = scanner.scan(NOW, ScanControl::default()).unwrap();
        let source_key = find_session(&summary, THREAD_ID).source_key.clone();
        let outcome = Arc::new(Mutex::new("pending"));
        let hook_outcome = Arc::clone(&outcome);
        let hook_target = target.clone();
        let hook_replacement = replacement.clone();
        let hook_displaced = displaced.clone();
        test_hooks::before_title_sqlite_open(move || {
            if fs::rename(&hook_target, &hook_displaced).is_err() {
                *hook_outcome.lock().unwrap() = "refused";
                return;
            }
            if fs::rename(&hook_replacement, &hook_target).is_err() {
                fs::rename(&hook_displaced, &hook_target)
                    .expect("the trusted database must be restored");
                *hook_outcome.lock().unwrap() = "refused";
                return;
            }
            *hook_outcome.lock().unwrap() = "replaced";
        });

        let raced = scanner.title_for_source(&source_key);
        let outcome = *outcome.lock().unwrap();
        assert_ne!(outcome, "pending", "the path-race hook must run after pin");
        assert_ne!(visible_title(&raced), Some(REPLACEMENT_CANARY));
        assert!(!format!("{raced:?}").contains(REPLACEMENT_CANARY));

        if outcome == "refused" {
            assert_eq!(visible_title(&raced), Some(TRUSTED_TITLE));
            return;
        }

        let persistent = scanner.title_for_source(&source_key);
        assert_ne!(
            visible_title(&persistent),
            Some(REPLACEMENT_CANARY),
            "a persistent replacement must remain fail-closed for this scanner"
        );
        assert!(!format!("{persistent:?}").contains(REPLACEMENT_CANARY));

        fs::rename(&target, &replacement).unwrap();
        fs::rename(&displaced, &target).unwrap();
        let before_aba = Arc::new(Mutex::new(false));
        let before_aba_hit = Arc::clone(&before_aba);
        let before_target = target.clone();
        let before_replacement = replacement.clone();
        let before_displaced = displaced.clone();
        test_hooks::before_title_sqlite_open(move || {
            fs::rename(&before_target, &before_displaced).unwrap();
            fs::rename(&before_replacement, &before_target).unwrap();
            *before_aba_hit.lock().unwrap() = true;
        });
        let after_aba = Arc::new(Mutex::new(false));
        let after_aba_hit = Arc::clone(&after_aba);
        let after_target = target.clone();
        let after_replacement = replacement.clone();
        let after_displaced = displaced.clone();
        test_hooks::before_title_sqlite_revalidate(move || {
            fs::rename(&after_target, &after_replacement).unwrap();
            fs::rename(&after_displaced, &after_target).unwrap();
            *after_aba_hit.lock().unwrap() = true;
        });

        let aba = scanner.title_for_source(&source_key);
        assert!(*before_aba.lock().unwrap());
        assert!(
            *after_aba.lock().unwrap(),
            "the ABA hook must run after SQLite releases the path"
        );
        assert_ne!(visible_title(&aba), Some(REPLACEMENT_CANARY));
        assert!(!format!("{aba:?}").contains(REPLACEMENT_CANARY));
    }

    #[test]
    fn sqlite_title_vm_budget_interrupts_without_exposing_the_target_value() {
        const THREAD_ID: &str = "title-vm-budget";
        const TITLE_CANARY: &str = "VM-BUDGET-TITLE-CANARY";

        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    title TEXT NOT NULL
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads (id, title) VALUES (?1, ?2)",
                rusqlite::params![THREAD_ID, TITLE_CANARY],
            )
            .unwrap();

        let result = super::query_indexed_title(&connection, THREAD_ID, 0);
        assert!(result.is_err(), "a zero VM budget must fail closed");
        assert!(!format!("{result:?}").contains(TITLE_CANARY));
    }

    #[test]
    fn replay_child_group_hard_limit_is_contained_to_one_parent() {
        fn inspected(
            source_key: String,
            thread_id: String,
            parent_thread_id: Option<String>,
        ) -> super::InspectedSource {
            super::InspectedSource {
                discovered_index: 0,
                source_key,
                identity: "test-identity".to_owned(),
                metadata: Some(super::SourceMetadata {
                    root_thread_id: thread_id,
                    created_at: NOW.to_owned(),
                    parent_thread_id,
                }),
                inspection_error: None,
            }
        }

        let root = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let mut scanner = scanner(&root, &state);
        let limited_parent = "limited-parent".to_owned();
        let other_parent = "other-parent".to_owned();
        let mut sources = Vec::with_capacity(super::MAX_REPLAY_CHILDREN_PER_PARENT + 4);
        sources.push(inspected("a".repeat(64), limited_parent.clone(), None));
        for child in 0..=super::MAX_REPLAY_CHILDREN_PER_PARENT {
            sources.push(inspected(
                format!("limited-child-key-{child:04}"),
                format!("limited-child-{child:04}"),
                Some(limited_parent.clone()),
            ));
        }
        let other_parent_index = sources.len();
        sources.push(inspected("b".repeat(64), other_parent.clone(), None));
        let other_child_index = sources.len();
        sources.push(inspected(
            "other-child-key".to_owned(),
            "other-child".to_owned(),
            Some(other_parent),
        ));

        let mut cache = super::ReplayGroupCache {
            boundaries: Vec::with_capacity(super::MAX_REPLAY_CHILDREN_PER_PARENT),
            ..super::ReplayGroupCache::default()
        };
        let limited_group = (1..=super::MAX_REPLAY_CHILDREN_PER_PARENT + 1).collect::<Vec<_>>();
        scanner
            .load_replay_group(0, &sources, &limited_group, &mut cache)
            .unwrap();
        assert_eq!(
            cache.boundaries.capacity(),
            0,
            "switching groups must release the previous retained allocation"
        );
        assert_eq!(
            cache.resolution_for(1).resolution,
            super::ReplayResolution::Deferred(SessionSourceErrorCode::SourceReplayLimit)
        );
        assert_eq!(
            cache
                .resolution_for(super::MAX_REPLAY_CHILDREN_PER_PARENT)
                .resolution,
            super::ReplayResolution::Deferred(SessionSourceErrorCode::SourceReplayLimit)
        );
        assert!(
            scanner.metrics.maximum_replay_group_working_bytes
                <= super::MAX_REPLAY_GROUP_WORKING_BYTES
        );

        scanner
            .load_replay_group(
                other_parent_index,
                &sources,
                &[other_child_index],
                &mut cache,
            )
            .unwrap();
        assert_eq!(
            cache.resolution_for(other_child_index).resolution,
            super::ReplayResolution::Deferred(SessionSourceErrorCode::SourceReplayParentMissing),
            "a different parent group must not inherit the hard-limit result"
        );
    }

    #[test]
    fn replay_group_child_discovery_is_linear_across_many_parents() {
        let root = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        for group in 0..64 {
            let parent_id = format!("linear-parent-{group:02}");
            let child_id = format!("linear-child-{group:02}");
            let parent = vec![meta(&parent_id, NOW), token("2026-07-26T12:00:01Z", 1, 1)];
            write_session(
                root.path(),
                &format!("sessions/2026/07/26/linear-parent-{group:02}.jsonl"),
                &parent,
            );
            let mut child_meta = meta(&child_id, "2026-07-26T12:00:02Z");
            child_meta["payload"]["parent_thread_id"] = serde_json::json!(parent_id);
            write_session(
                root.path(),
                &format!("sessions/2026/07/26/linear-child-{group:02}.jsonl"),
                &[child_meta, parent[1].clone()],
            );
        }
        let mut scanner = scanner(&root, &state);
        test_hooks::reset_replay_group_child_visits();

        scanner.scan(NOW, ScanControl::default()).unwrap();

        assert!(
            test_hooks::replay_group_child_visits() <= 128,
            "64 one-child groups must visit O(N) children, observed {} visits",
            test_hooks::replay_group_child_visits()
        );
    }

    #[test]
    fn maximum_replay_group_page_and_compact_boundaries_fit_the_hard_cap() {
        assert_eq!(
            std::mem::size_of::<super::CachedReplayBoundary>(),
            40,
            "the boundary cache must remain compact on the supported 64-bit targets"
        );
        let parent_key = "a".repeat(64);
        let mut items = Vec::with_capacity(super::REPLAY_GROUP_PAGE_SIZE + 1);
        for ordinal in 1..=super::REPLAY_GROUP_PAGE_SIZE {
            items.push(super::CodexReplaySignature {
                parent_source_key: parent_key.clone(),
                parent_generation: 1,
                token_event_ordinal: ordinal as u64,
                occurred_at: NOW.to_owned(),
                signature_hash: [0x5a; 32],
            });
        }
        let page = super::CodexReplaySignaturePage {
            items,
            next_page_key: Some(
                super::ReplaySignaturePageKey::new(
                    parent_key,
                    1,
                    super::REPLAY_GROUP_PAGE_SIZE as u64,
                )
                .unwrap(),
            ),
        };
        let boundary_bytes = super::MAX_REPLAY_CHILDREN_PER_PARENT
            .saturating_mul(std::mem::size_of::<super::CachedReplayBoundary>());
        let working_bytes = boundary_bytes
            .saturating_add(64)
            .saturating_add(super::replay_page_retained_bytes(&page));

        assert!(
            working_bytes <= super::MAX_REPLAY_GROUP_WORKING_BYTES,
            "maximum replay group retains {working_bytes} bytes"
        );
        assert!(
            super::maximum_replay_page_retained_bytes(super::REPLAY_GROUP_PAGE_SIZE)
                >= super::replay_page_retained_bytes(&page)
        );
    }
}

fn token_signature_hash(key: &[u8; 32], token: &CodexTokenCount, model: &str) -> [u8; 32] {
    let total = token.total.unwrap_or_default();
    let last = token.last.unwrap_or_default();
    let fields = [
        token.occurred_at.as_bytes(),
        model.as_bytes(),
        &total.input.to_be_bytes(),
        &total.output.to_be_bytes(),
        &total.cache_read.to_be_bytes(),
        &total.cache_write.to_be_bytes(),
        &total.reasoning.to_be_bytes(),
        &last.input.to_be_bytes(),
        &last.output.to_be_bytes(),
        &last.cache_read.to_be_bytes(),
        &last.cache_write.to_be_bytes(),
        &last.reasoning.to_be_bytes(),
    ];
    opaque_hash(key, b"wokcore.codex.replay-signature.v1", &fields)
}

fn usage_id(key: &[u8; 32], root_thread_id: &str, generation: u64, ordinal: u64) -> String {
    opaque_hex(
        key,
        b"wokcore.codex.usage-id.v1",
        &[
            root_thread_id.as_bytes(),
            &generation.to_be_bytes(),
            &ordinal.to_be_bytes(),
        ],
    )
}

fn session_key(key: &[u8; 32], root_thread_id: &str) -> String {
    opaque_hex(
        key,
        b"wokcore.codex.session-key.v1",
        &[root_thread_id.as_bytes()],
    )
}

fn replay_chain_seed(key: &[u8; 32], generation: u64) -> [u8; 32] {
    opaque_hash(
        key,
        b"wokcore.codex.replay-chain-seed.v2",
        &[&generation.to_be_bytes()],
    )
}

fn replay_chain_step(
    key: &[u8; 32],
    generation: u64,
    ordinal: u64,
    previous: [u8; 32],
    signature: [u8; 32],
) -> [u8; 32] {
    opaque_hash(
        key,
        b"wokcore.codex.replay-chain-step.v2",
        &[
            &generation.to_be_bytes(),
            &ordinal.to_be_bytes(),
            &previous,
            &signature,
        ],
    )
}

fn opaque_file_identity(key: &[u8; 32], identity: PlatformFileIdentity) -> String {
    let bytes = platform_identity_bytes(identity);
    opaque_hex(key, b"wokcore.codex.file-identity.v1", &[&bytes])
}

pub(crate) fn open_source_for_paging(
    root: &SessionRootLease,
    domain_key: &[u8; 32],
    cursor: &SessionScanCursor,
    maximum_size: u64,
) -> Result<SessionFile, CodexScannerError> {
    let discovered = discover_codex_sessions(root, DiscoveryLimits::default())?;
    let mut target = None;
    for source in &discovered {
        let identity =
            SessionFileIdentity::new(opaque_file_identity(domain_key, source.identity()))
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
        .ok_or(CodexScannerError::Read)?
        .open(root, maximum_size)
        .map_err(|error| match error {
            SessionError::ReadLimitExceeded => CodexScannerError::RecordTooLarge,
            _ => CodexScannerError::Read,
        })?;
    let snapshot = file.snapshot();
    if snapshot.size != cursor.observed_size
        || system_time_utc(snapshot.modified) != cursor.modified_at
        || cursor.complete_byte_offset > snapshot.size
    {
        return Err(CodexScannerError::Parse);
    }
    let (head, boundary) = fingerprints_with_extent(
        &mut file,
        cursor.complete_byte_offset,
        cursor.observed_size,
        domain_key,
    )
    .map_err(|_| CodexScannerError::Read)?;
    if head != cursor.head_fingerprint || boundary != cursor.boundary_fingerprint {
        return Err(CodexScannerError::Parse);
    }
    Ok(file)
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

fn opaque_hex(key: &[u8; 32], domain: &[u8], fields: &[&[u8]]) -> String {
    hex(&opaque_hash(key, domain, fields))
}

fn opaque_hash(key: &[u8; 32], domain: &[u8], fields: &[&[u8]]) -> [u8; 32] {
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

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn fingerprints(
    file: &mut SessionFile,
    boundary_offset: u64,
    key: &[u8; 32],
) -> Result<([u8; 32], [u8; 32]), JsonlError> {
    fingerprints_with_extent(file, boundary_offset, file.snapshot().size, key)
}

fn fingerprints_with_extent(
    file: &mut SessionFile,
    boundary_offset: u64,
    observed_size: u64,
    key: &[u8; 32],
) -> Result<([u8; 32], [u8; 32]), JsonlError> {
    let mut head = file.read_range_bounded(0, HEAD_FINGERPRINT_BYTES)?;
    head.resize(HEAD_FINGERPRINT_BYTES, 0);
    let half_window = (FINGERPRINT_WINDOW_BYTES / 2) as u64;
    let start = boundary_offset.saturating_sub(half_window);
    let end = boundary_offset
        .saturating_add(half_window)
        .min(observed_size)
        .min(file.snapshot().size);
    let boundary_length =
        usize::try_from(end.saturating_sub(start)).unwrap_or(FINGERPRINT_WINDOW_BYTES);
    let boundary = file.read_range_bounded(start, boundary_length)?;
    Ok((
        opaque_hash(key, b"wokcore.codex.head-fingerprint.v1", &[&head]),
        opaque_hash(
            key,
            b"wokcore.codex.boundary-fingerprint.v1",
            &[&boundary_offset.to_be_bytes(), &boundary],
        ),
    ))
}

fn system_time_utc(value: Option<SystemTime>) -> String {
    let seconds = value
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_secs().min(i64::MAX as u64) as i64);
    format_epoch_seconds(seconds).unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

fn immutable_sqlite_uri(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    let mut encoded = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b':' | b'-' | b'_' | b'.') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push_str(&format!("{byte:02X}"));
        }
    }
    format!("file:{encoded}?mode=ro&immutable=1")
}
