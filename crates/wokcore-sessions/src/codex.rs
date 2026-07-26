use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use wokcore_platform::sessions::{
    SessionFile, SessionFileIdentity as PlatformFileIdentity, SessionRootLease,
};
use wokcore_storage::{
    CandidateBeginOutcome, CodexReplaySignature, MAX_CODEX_REPLAY_SIGNATURES,
    MAX_SESSION_BATCH_BYTES, MAX_SESSION_BATCH_ROWS, ParserCheckpoint, ReplaySignaturePageKey,
    SessionAvailability, SessionBatch, SessionFileIdentity, SessionGenerationState,
    SessionIndexRecord, SessionScanCursor, SessionScanResultCode, SessionSourceErrorCode,
    SessionSourceKind, SessionSourcePageKey, SessionSourceStatus, SessionUsageRecord, StateStore,
    StorageError,
};

use crate::{
    cursor::{JsonlCursor, JsonlError, JsonlRecordStatus, MAX_JSONL_LINE_BYTES},
    discovery::{
        DiscoveredSession, DiscoveryError, DiscoveryLimits, SessionLocation,
        discover_codex_sessions,
    },
    model::{ReplayResolution, TokenTotals},
};

pub const REPLAY_PAGE_SIZE: usize = 512;
const FINGERPRINT_WINDOW_BYTES: usize = 4 * 1024;
const HEAD_FINGERPRINT_BYTES: usize = 64;
const TITLE_INDEX_LIMIT_BYTES: u64 = 4 * 1024 * 1024;
const TITLE_LIMIT_BYTES: usize = 512;
const TITLE_ROW_LIMIT: usize = 4_096;
const SESSION_BATCH_ROW_TARGET: usize = 384;
const THREAD_ID_LIMIT_BYTES: usize = 512;
const MODEL_LIMIT_BYTES: usize = 256;
const METADATA_PROBE_RECORDS: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexSessionMeta {
    pub root_thread_id: String,
    pub created_at: String,
    pub parent_thread_id: Option<String>,
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
    if let Some(legacy) = payload
        .get("source")
        .and_then(Value::as_object)
        .and_then(|source| source.get("subagent"))
        .and_then(Value::as_object)
        .and_then(|subagent| subagent.get("thread_spawn"))
        .and_then(Value::as_object)
        .and_then(|spawn| spawn.get("parent_thread_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
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
    if normalized.is_empty() || normalized.len() > MODEL_LIMIT_BYTES {
        return "unknown".to_owned();
    }
    normalized
        .rsplit_once('/')
        .map_or(normalized.clone(), |(_, model)| model.to_owned())
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceScanSummary {
    pub source_key: String,
    pub root_thread_id: Option<String>,
    pub title: Option<String>,
    pub status: SessionSourceStatus,
    pub error_code: Option<SessionSourceErrorCode>,
    pub replay_resolution: ReplayResolution,
    pub complete_byte_offset: u64,
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
}

pub struct CodexScanner {
    root: SessionRootLease,
    root_path: PathBuf,
    state: StateStore,
    domain_key: [u8; 32],
    discovery_limits: DiscoveryLimits,
    replay_limit: u64,
    metrics: ScannerMetrics,
}

impl CodexScanner {
    pub fn open(
        root_path: impl AsRef<Path>,
        state_path: impl AsRef<Path>,
        domain_key: [u8; 32],
    ) -> Result<Self, CodexScannerError> {
        let root_path = root_path.as_ref().to_path_buf();
        let root = SessionRootLease::open(&root_path).map_err(|_| CodexScannerError::Root)?;
        let state = StateStore::open(state_path)?;
        Ok(Self {
            root,
            root_path,
            state,
            domain_key,
            discovery_limits: DiscoveryLimits::default(),
            replay_limit: MAX_CODEX_REPLAY_SIGNATURES,
            metrics: ScannerMetrics::default(),
        })
    }

    #[doc(hidden)]
    pub fn set_test_replay_limit(&mut self, limit: u64) {
        self.replay_limit = limit.min(MAX_CODEX_REPLAY_SIGNATURES);
    }

    pub fn state(&self) -> &StateStore {
        &self.state
    }

    pub fn scan(
        &mut self,
        transition_at: &str,
        control: ScanControl,
    ) -> Result<CodexScanSummary, CodexScannerError> {
        self.metrics = ScannerMetrics::default();
        let discovered = discover_codex_sessions(&self.root, self.discovery_limits)?;
        #[cfg(test)]
        test_hooks::run_after_discovery();
        let persisted_identities = self.persisted_identity_sources()?;
        let titles = self.read_titles();
        let mut inspected = Vec::with_capacity(discovered.len());
        for (index, source) in discovered.iter().enumerate() {
            let identity = opaque_file_identity(&self.domain_key, source.identity());
            let source_key = persisted_identities
                .get(&identity)
                .cloned()
                .unwrap_or_else(|| self.path_source_key(source));
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

        let mut order = (0..inspected.len()).collect::<Vec<_>>();
        order.sort_by_key(|index| {
            inspected[*index]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.parent_thread_id.as_ref())
                .is_some()
        });
        let referenced_parent_indices = inspected
            .iter()
            .filter_map(|source| source.metadata.as_ref()?.parent_thread_id.as_ref())
            .filter_map(|parent_id| thread_sources.get(parent_id))
            .flatten()
            .copied()
            .collect::<HashSet<_>>();

        let persisted_sources = self.persisted_codex_source_keys()?;
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

        for work_index in order {
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
                    root_thread_id: None,
                    title: None,
                    status,
                    error_code: Some(code),
                    replay_resolution: ReplayResolution::NotForked,
                    complete_byte_offset: 0,
                });
                continue;
            };
            let replay =
                match self.resolve_replay(work_index, metadata, &inspected, &thread_sources) {
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
                            root_thread_id: Some(metadata.root_thread_id.clone()),
                            title: title_for(&titles, &metadata.root_thread_id),
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
                summaries.push(SourceScanSummary {
                    source_key: source.source_key.clone(),
                    root_thread_id: Some(metadata.root_thread_id.clone()),
                    title: title_for(&titles, &metadata.root_thread_id),
                    status: if code == SessionSourceErrorCode::SourceReplayLimit {
                        SessionSourceStatus::ResourceLimited
                    } else {
                        SessionSourceStatus::Unavailable
                    },
                    error_code: Some(code),
                    replay_resolution: ReplayResolution::Deferred(code),
                    complete_byte_offset: 0,
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
                    let code = (process.status == SessionSourceStatus::ResourceLimited)
                        .then_some(SessionSourceErrorCode::SourceReplayLimit);
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
                    if let Some(generation) = self
                        .state
                        .load_session_source(&source.source_key)?
                        .and_then(|source| source.staging_generation)
                    {
                        self.cleanup_generation(&source.source_key, generation)?;
                    }
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
                Err(error) => return Err(error),
            };
            match process.outcome {
                SourceProcessOutcome::Advanced => advanced_sources += 1,
                SourceProcessOutcome::Unchanged => unchanged_sources += 1,
                SourceProcessOutcome::Failed => {}
                SourceProcessOutcome::Interrupted => {
                    overall_outcome = ScanOutcome::Interrupted;
                }
            }
            summaries.push(SourceScanSummary {
                source_key: source.source_key.clone(),
                root_thread_id: Some(metadata.root_thread_id.clone()),
                title: title_for(&titles, &metadata.root_thread_id),
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
            if overall_outcome == ScanOutcome::Interrupted {
                break;
            }
        }

        summaries.sort_by(|left, right| left.source_key.cmp(&right.source_key));
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

    fn inspect_source(
        &mut self,
        source: &DiscoveredSession,
    ) -> Result<SourceMetadata, SourceInspectionError> {
        self.metrics.source_opens = self.metrics.source_opens.saturating_add(1);
        let mut file = source
            .open(&self.root, u64::MAX)
            .map_err(|_| SourceInspectionError::Read)?;
        let scan = JsonlCursor::new(0, 1)
            .scan_bounded(&mut file, METADATA_PROBE_RECORDS, MAX_JSONL_LINE_BYTES)
            .map_err(SourceInspectionError::from)?;
        self.metrics.metadata_probe_bytes = self
            .metrics
            .metadata_probe_bytes
            .saturating_add(scan.read_bytes);
        let mut meta: Option<CodexSessionMeta> = None;
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
                if meta.as_ref().is_some_and(|existing| existing != &found) {
                    return Err(SourceInspectionError::Parse);
                }
                meta = Some(found);
                break;
            }
        }
        let meta = meta.ok_or(SourceInspectionError::Parse)?;
        Ok(SourceMetadata {
            root_thread_id: meta.root_thread_id,
            created_at: meta.created_at,
            parent_thread_id: meta.parent_thread_id,
        })
    }

    fn resolve_replay(
        &mut self,
        work_index: usize,
        metadata: &SourceMetadata,
        inspected: &[InspectedSource],
        thread_sources: &HashMap<String, Vec<usize>>,
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
        let parent = &inspected[parent_indices[0]];
        let Some(parent_metadata) = parent.metadata.as_ref() else {
            return Ok(ReplayState::deferred(
                SessionSourceErrorCode::SourceReplayInconsistent,
            ));
        };
        if parent_metadata.created_at > metadata.created_at {
            return Ok(ReplayState::deferred(
                SessionSourceErrorCode::SourceReplayInconsistent,
            ));
        }
        let Some(parent_generation) = self.state.load_current_generation(&parent.source_key)?
        else {
            return Ok(ReplayState::deferred(
                SessionSourceErrorCode::SourceReplayParentMissing,
            ));
        };
        let parent_cursor = self
            .state
            .load_current_session_scan_cursor(&parent.source_key)?;
        let (replayed, fingerprint, indexed_events) =
            self.parent_boundary_at(&parent.source_key, parent_generation, &metadata.created_at)?;
        if parent_cursor
            .as_ref()
            .is_some_and(|cursor| cursor.parser_checkpoint.event_ordinal != indexed_events)
        {
            return Ok(ReplayState::deferred(
                SessionSourceErrorCode::SourceReplayInconsistent,
            ));
        }

        Ok(ReplayState::resolved(
            parent.source_key.clone(),
            parent_generation,
            replayed,
            fingerprint,
        ))
    }

    fn parent_boundary_at(
        &mut self,
        parent_source_key: &str,
        parent_generation: u64,
        child_created_at: &str,
    ) -> Result<(u64, [u8; 32], u64), CodexScannerError> {
        let mut page_key: Option<ReplaySignaturePageKey> = None;
        let mut replayed = 0u64;
        let mut indexed_events = 0u64;
        let mut boundary_material = Vec::new();
        let mut beyond_cutoff = false;
        loop {
            let page = self.state.load_codex_replay_signature_page(
                parent_source_key,
                parent_generation,
                page_key.as_ref(),
                REPLAY_PAGE_SIZE,
            )?;
            self.metrics.replay_pages_loaded = self.metrics.replay_pages_loaded.saturating_add(1);
            self.metrics.maximum_replay_page_rows =
                self.metrics.maximum_replay_page_rows.max(page.items.len());
            for signature in &page.items {
                indexed_events = indexed_events
                    .checked_add(1)
                    .ok_or(CodexScannerError::Parse)?;
                if indexed_events > self.replay_limit {
                    return Err(CodexScannerError::ReplayLimit);
                }
                if beyond_cutoff || signature.occurred_at.as_str() > child_created_at {
                    beyond_cutoff = true;
                    continue;
                }
                replayed = replayed.checked_add(1).ok_or(CodexScannerError::Parse)?;
                boundary_material.clear();
                boundary_material.extend_from_slice(&signature.signature_hash);
            }
            page_key = page.next_page_key;
            if page_key.is_none() {
                break;
            }
        }
        Ok((
            replayed,
            replay_boundary_hash(
                &self.domain_key,
                parent_generation,
                replayed,
                &boundary_material,
            ),
            indexed_events,
        ))
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
        let force_replay_rebuild = if builds_parent_index {
            match &current {
                Some(cursor) if cursor.parser_checkpoint.event_ordinal > 0 => {
                    let expected = cursor.parser_checkpoint.event_ordinal;
                    let after = if expected == 1 {
                        None
                    } else {
                        Some(ReplaySignaturePageKey::new(
                            &source.source_key,
                            cursor.generation,
                            expected.checked_sub(1).ok_or(CodexScannerError::Parse)?,
                        )?)
                    };
                    let tail = self.state.load_codex_replay_signature_page(
                        &source.source_key,
                        cursor.generation,
                        after.as_ref(),
                        1,
                    )?;
                    tail.items
                        .first()
                        .is_none_or(|signature| signature.token_event_ordinal != expected)
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
                && persisted_source.as_ref().is_none_or(|state| {
                    state.status == SessionSourceStatus::Available
                        || state.error_code == Some(SessionSourceErrorCode::SourceSessionsAbsent)
                })
            {
                let _ = self.state.record_source_success(
                    &source.source_key,
                    cursor.generation,
                    transition_at,
                )?;
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
        let mut replay_page_key = if replay_verified == 0 {
            None
        } else {
            Some(ReplaySignaturePageKey::new(
                replay
                    .parent_source_key
                    .as_deref()
                    .ok_or(CodexScannerError::ReplayInconsistent)?,
                replay
                    .parent_generation
                    .ok_or(CodexScannerError::ReplayInconsistent)?,
                replay_verified,
            )?)
        };
        let mut expected_replay = VecDeque::new();
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

        loop {
            let scan = JsonlCursor::new(offset, record_ordinal)
                .scan(&mut file)
                .map_err(map_jsonl_scanner_error)?;
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
                            let _ = self.state.fail_candidate(
                                &source.source_key,
                                generation,
                                SessionSourceErrorCode::SourceReplayLimit,
                                transition_at,
                            );
                            return Ok(SourceProcessResult {
                                outcome: SourceProcessOutcome::Failed,
                                status: SessionSourceStatus::ResourceLimited,
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
                            if expected_replay.is_empty() {
                                let parent_source_key = replay
                                    .parent_source_key
                                    .as_deref()
                                    .ok_or(CodexScannerError::ReplayInconsistent)?;
                                let parent_generation = replay
                                    .parent_generation
                                    .ok_or(CodexScannerError::ReplayInconsistent)?;
                                let page = self.state.load_codex_replay_signature_page(
                                    parent_source_key,
                                    parent_generation,
                                    replay_page_key.as_ref(),
                                    REPLAY_PAGE_SIZE,
                                )?;
                                self.metrics.replay_pages_loaded =
                                    self.metrics.replay_pages_loaded.saturating_add(1);
                                self.metrics.maximum_replay_page_rows =
                                    self.metrics.maximum_replay_page_rows.max(page.items.len());
                                replay_page_key = page.next_page_key;
                                expected_replay.extend(page.items);
                            }
                            let expected = expected_replay
                                .pop_front()
                                .ok_or(CodexScannerError::ReplayInconsistent)?;
                            if signature_hash != expected.signature_hash {
                                return Err(CodexScannerError::ReplayInconsistent);
                            }
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
                        replay,
                        transition_at,
                        is_candidate,
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
                        return Ok(SourceProcessResult {
                            outcome: SourceProcessOutcome::Interrupted,
                            status: SessionSourceStatus::Stale,
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
            replay,
            transition_at,
            is_candidate,
            pending_usage,
            pending_replay,
            Some(index),
        )?;
        *committed_batches += 1;
        if control
            .stop_after_committed_batches
            .is_some_and(|maximum| *committed_batches >= maximum)
        {
            return Ok(SourceProcessResult {
                outcome: SourceProcessOutcome::Interrupted,
                status: SessionSourceStatus::Stale,
                complete_byte_offset: cursor.complete_byte_offset,
            });
        }

        if is_candidate {
            self.state
                .promote_candidate(&source.source_key, generation, transition_at)?;
            self.cleanup_retired(&source.source_key)?;
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
    ) -> Result<SessionScanCursor, CodexScannerError> {
        loop {
            if let Some(staging) = self
                .state
                .load_staging_session_scan_cursor(&source.source_key)?
            {
                if staging.generation == generation
                    && staging.file_identity.as_str() == source.identity
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
                                self.cleanup_generation(&source.source_key, generation)?;
                                continue;
                            }
                            CandidateBeginOutcome::Started => return Ok(staging),
                        }
                    }
                }
                self.cleanup_generation(&source.source_key, staging.generation)?;
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
                parser_checkpoint: parser_checkpoint(TokenTotals::default(), None, 0, 0, replay),
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
                    self.cleanup_generation(&source.source_key, generation)?;
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
        replay: &ReplayState,
        transition_at: &str,
        candidate: bool,
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
            replay,
        );
        cursor.head_fingerprint = head_fingerprint;
        cursor.boundary_fingerprint = boundary_fingerprint;
        cursor.result_code = Some(SessionScanResultCode::Advanced);
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

    fn cleanup_retired(&mut self, source_key: &str) -> Result<(), CodexScannerError> {
        if let Some(generation) = self
            .state
            .load_session_source(source_key)?
            .and_then(|source| source.retired_generation)
        {
            self.cleanup_generation(source_key, generation)?;
        }
        Ok(())
    }

    fn cleanup_generation(
        &mut self,
        source_key: &str,
        generation: u64,
    ) -> Result<(), CodexScannerError> {
        loop {
            let outcome = self.state.cleanup_generation_batch(
                source_key,
                generation,
                MAX_SESSION_BATCH_ROWS,
                MAX_SESSION_BATCH_BYTES,
            )?;
            if outcome.complete {
                return Ok(());
            }
        }
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
            parser_checkpoint: parser_checkpoint(TokenTotals::default(), None, 0, 0, &replay),
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
                self.cleanup_generation(&source.source_key, generation)?;
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

    fn read_titles(&mut self) -> HashMap<String, String> {
        let mut titles = self.read_session_index_titles().unwrap_or_default();
        if let Ok(database_titles) = self.read_immutable_database_titles() {
            for (id, title) in database_titles {
                titles.entry(id).or_insert(title);
            }
        }
        titles
    }

    fn read_session_index_titles(&mut self) -> Result<HashMap<String, String>, CodexScannerError> {
        let mut file = match self
            .root
            .open_file("session_index.jsonl", TITLE_INDEX_LIMIT_BYTES)
        {
            Ok(file) => file,
            Err(_) => return Ok(HashMap::new()),
        };
        self.metrics.source_opens = self.metrics.source_opens.saturating_add(1);
        let mut output: HashMap<String, String> = HashMap::new();
        let mut offset = 0;
        let mut ordinal = 1;
        let mut rows = 0usize;
        loop {
            let scan = JsonlCursor::new(offset, ordinal)
                .scan(&mut file)
                .map_err(|_| CodexScannerError::Read)?;
            let empty = scan.records.is_empty();
            for record in scan.records {
                if rows >= TITLE_ROW_LIMIT {
                    return Ok(HashMap::new());
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
                if id.is_empty()
                    || id.len() > THREAD_ID_LIMIT_BYTES
                    || title.is_empty()
                    || title.len() > TITLE_LIMIT_BYTES
                {
                    continue;
                }
                output.insert(id.to_owned(), title.to_owned());
            }
            offset = scan.complete_byte_offset;
            ordinal = scan.next_record_ordinal;
            if scan.reached_end || empty {
                break;
            }
        }
        Ok(output)
    }

    fn read_immutable_database_titles(&self) -> Result<HashMap<String, String>, CodexScannerError> {
        let mut output = HashMap::new();
        for name in ["state_5.sqlite", "state.sqlite"] {
            let path = self.root_path.join(name);
            if !path.is_file() {
                continue;
            }
            let uri = immutable_sqlite_uri(&path);
            let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX;
            let connection =
                Connection::open_with_flags(uri, flags).map_err(|_| CodexScannerError::Read)?;
            connection
                .pragma_update(None, "query_only", true)
                .map_err(|_| CodexScannerError::Read)?;
            connection
                .pragma_update(None, "cache_size", -256i64)
                .map_err(|_| CodexScannerError::Read)?;
            let exists = connection
                .query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM sqlite_schema
                         WHERE type='table' AND name='threads'
                     )",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap_or(false);
            if !exists {
                continue;
            }
            let mut statement = match connection.prepare(
                "SELECT id, title
                     FROM (
                         SELECT id, title, updated_at
                         FROM threads
                         ORDER BY updated_at DESC, id
                         LIMIT ?3
                     )
                     WHERE typeof(id) = 'text'
                       AND length(CAST(id AS BLOB)) BETWEEN 1 AND ?1
                       AND typeof(title) = 'text'
                       AND length(CAST(title AS BLOB)) BETWEEN 1 AND ?2
                     ORDER BY updated_at DESC, id",
            ) {
                Ok(statement) => statement,
                Err(_) => continue,
            };
            let rows = statement
                .query_map(
                    rusqlite::params![
                        THREAD_ID_LIMIT_BYTES as i64,
                        TITLE_LIMIT_BYTES as i64,
                        TITLE_ROW_LIMIT as i64
                    ],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .map_err(|_| CodexScannerError::Read)?;
            for row in rows {
                let (id, title) = row.map_err(|_| CodexScannerError::Read)?;
                if !id.is_empty()
                    && id.len() <= THREAD_ID_LIMIT_BYTES
                    && !title.is_empty()
                    && title.len() <= TITLE_LIMIT_BYTES
                {
                    output.entry(id).or_insert(title);
                }
            }
        }
        Ok(output)
    }
}

#[derive(Debug)]
struct InspectedSource {
    discovered_index: usize,
    source_key: String,
    identity: String,
    metadata: Option<SourceMetadata>,
    inspection_error: Option<SourceInspectionError>,
}

#[derive(Clone, Debug)]
struct SourceMetadata {
    root_thread_id: String,
    created_at: String,
    parent_thread_id: Option<String>,
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
    replayed_events: u64,
}

impl ReplayState {
    fn not_forked() -> Self {
        Self {
            resolution: ReplayResolution::NotForked,
            parent_source_key: None,
            parent_generation: None,
            boundary_fingerprint: None,
            replayed_events: 0,
        }
    }

    fn deferred(code: SessionSourceErrorCode) -> Self {
        Self {
            resolution: ReplayResolution::Deferred(code),
            parent_source_key: None,
            parent_generation: None,
            boundary_fingerprint: None,
            replayed_events: 0,
        }
    }

    fn resolved(
        parent_source_key: String,
        parent_generation: u64,
        replayed_events: u64,
        boundary_fingerprint: [u8; 32],
    ) -> Self {
        Self {
            resolution: ReplayResolution::Resolved { replayed_events },
            parent_source_key: Some(parent_source_key),
            parent_generation: Some(parent_generation),
            boundary_fingerprint: Some(boundary_fingerprint),
            replayed_events,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceProcessOutcome {
    Advanced,
    Unchanged,
    Interrupted,
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
    replay: &ReplayState,
) -> ParserCheckpoint {
    ParserCheckpoint {
        version: 1,
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
        structural_hash: replay.boundary_fingerprint,
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

fn title_for(titles: &HashMap<String, String>, thread_id: &str) -> Option<String> {
    titles.get(thread_id).cloned()
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
    use std::{cell::RefCell, fs, path::PathBuf};

    thread_local! {
        static DELETE_AFTER_DISCOVERY: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
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
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::{
        CodexScanner, ScanControl, ScanOutcome, SessionSourceErrorCode, SessionSourceStatus,
        test_hooks,
    };

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
        let mut scanner =
            CodexScanner::open(root.path(), state.path().join("state.sqlite3"), [0x35; 32])
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
            .find(|source| source.root_thread_id.as_deref() == Some("sibling"))
            .unwrap();
        assert_eq!(sibling.status, SessionSourceStatus::Available);
        assert_eq!(sibling.error_code, None);
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

fn replay_boundary_hash(
    key: &[u8; 32],
    generation: u64,
    replayed: u64,
    signature: &[u8],
) -> [u8; 32] {
    opaque_hash(
        key,
        b"wokcore.codex.replay-boundary.v1",
        &[
            &generation.to_be_bytes(),
            &replayed.to_be_bytes(),
            signature,
        ],
    )
}

fn opaque_file_identity(key: &[u8; 32], identity: PlatformFileIdentity) -> String {
    let bytes = platform_identity_bytes(identity);
    opaque_hex(key, b"wokcore.codex.file-identity.v1", &[&bytes])
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
