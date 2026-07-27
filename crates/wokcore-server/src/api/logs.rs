use std::{
    collections::{BTreeMap, HashSet},
    ffi::OsStr,
    path::PathBuf,
};

use axum::{
    body::Body,
    extract::{Extension, Query, State, rejection::QueryRejection},
    http::{StatusCode, header},
    response::Response,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use wokcore_diagnostics::{
    event::{DiagnosticEvent, MAX_PREPARED_EVENT_BYTES, UtcTimestamp},
    ring::{MAX_PAGE_BYTES, MAX_PAGE_EVENTS, PageDirection, PageRequest},
    segment::MAX_SEGMENT_BYTES,
};
use wokcore_platform::diagnostics::{DiagnosticDirectory, DiagnosticStoreError};

use crate::{
    ServerState,
    observability::DiagnosticWriterHandle,
    query::{QueryCancellation, QueryServiceError},
};

use super::{cursor, error::ApiError, request_id::RequestId, sessions::map_query_error};

const LOG_DEFAULT_LIMIT: usize = 100;
const LOG_MAX_LIMIT: usize = 1_000;
const LOG_MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const LOG_ROUTE: &str = "diagnostics.logs";
const MAX_DIAGNOSTIC_FILES: usize = 4_096;
const DIAGNOSTIC_READ_CHUNK: usize = 64 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LogsQuery {
    request_id: Option<String>,
    trace_id: Option<String>,
    session_key: Option<String>,
    level_min: Option<String>,
    component: Option<String>,
    since: Option<String>,
    until: Option<String>,
    order: Option<String>,
    after: Option<String>,
    limit: Option<usize>,
}

pub(super) async fn logs(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
    query: Result<Query<LogsQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid_query(request_id))?;
    let level_min = parse_level(query.level_min.as_deref())
        .ok_or_else(|| ApiError::invalid_query(request_id))?;
    let component = parse_component(query.component.as_deref())
        .ok_or_else(|| ApiError::invalid_query(request_id))?;
    let order =
        parse_order(query.order.as_deref()).ok_or_else(|| ApiError::invalid_query(request_id))?;
    if !valid_optional_correlation(query.request_id.as_deref())
        || !valid_optional_correlation(query.trace_id.as_deref())
        || !valid_optional_correlation(query.session_key.as_deref())
    {
        return Err(ApiError::invalid_query(request_id));
    }
    if query
        .since
        .as_deref()
        .is_some_and(|value| UtcTimestamp::parse(value).is_err())
        || query
            .until
            .as_deref()
            .is_some_and(|value| UtcTimestamp::parse(value).is_err())
        || matches!(
            (&query.since, &query.until),
            (Some(since), Some(until)) if since >= until
        )
    {
        return Err(ApiError::invalid_time_range(request_id));
    }
    let limit = query.limit.unwrap_or(LOG_DEFAULT_LIMIT);
    if !(1..=LOG_MAX_LIMIT).contains(&limit) {
        return Err(ApiError::limit_out_of_range(request_id));
    }
    let runtime = state
        .query
        .clone()
        .ok_or_else(|| ApiError::query_busy(request_id))?;
    let filters = LogFilters {
        request_id: query.request_id,
        trace_id: query.trace_id,
        session_key: query.session_key,
        level_min,
        component,
        since: query.since,
        until: query.until,
        order,
    };
    let binding = log_binding(&filters);
    let domain_key = *runtime.session_domain_key();
    let diagnostics_root = runtime.diagnostics_root().to_path_buf();
    let diagnostics = state.diagnostics.clone();
    let active_writer = diagnostics.is_some();
    let after = query.after;
    let pending = runtime
        .handle()
        .try_submit(move |cancellation| {
            build_logs(
                diagnostics_root,
                diagnostics,
                active_writer,
                domain_key,
                filters,
                binding,
                after,
                limit,
                cancellation,
            )
        })
        .map_err(|error| map_query_error(error, request_id))?;
    let bytes = pending
        .wait()
        .await
        .map_err(|error| map_query_error(error, request_id))?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .map_err(|_| ApiError::internal_failure(request_id))
}

#[allow(clippy::too_many_arguments)]
fn build_logs(
    diagnostics_root: PathBuf,
    diagnostics: Option<DiagnosticWriterHandle>,
    active_writer: bool,
    domain_key: [u8; 32],
    filters: LogFilters,
    binding: String,
    after: Option<String>,
    limit: usize,
    cancellation: &QueryCancellation,
) -> Result<Vec<u8>, QueryServiceError> {
    let after = after
        .as_deref()
        .map(|token| {
            cursor::decode::<LogCursor>(&domain_key, LOG_ROUTE, &binding, token)
                .map_err(|_| QueryServiceError::InvalidCursor)
        })
        .transpose()?;
    let dropped_events = diagnostics
        .as_ref()
        .map_or(0, |handle| handle.recorder().metrics().total_dropped());
    let mut selected = SelectedLogs::new(filters.order, limit);
    read_persistent_events(
        &diagnostics_root,
        active_writer,
        &filters,
        after.as_ref(),
        &mut selected,
        cancellation,
    )?;
    if let Some(diagnostics) = diagnostics {
        read_ring_events(
            &diagnostics,
            &filters,
            after.as_ref(),
            &mut selected,
            cancellation,
        )?;
    }
    let (mut items, mut has_more) = selected.into_items();
    loop {
        cancellation.check()?;
        let next_cursor = if has_more {
            items
                .last()
                .map(|item| {
                    cursor::encode(
                        &domain_key,
                        LOG_ROUTE,
                        &binding,
                        LogCursor {
                            occurred_at: item.occurred_at.clone(),
                            event_id: item.event_id.clone(),
                        },
                    )
                    .map_err(|_| QueryServiceError::Execution)
                })
                .transpose()?
        } else {
            None
        };
        let encoded = serde_json::to_vec(&LogsResponse {
            schema_version: 1,
            items: items.iter().map(|item| &item.value).collect(),
            next_cursor,
            dropped_events,
        })
        .map_err(|_| QueryServiceError::Execution)?;
        if encoded.len() <= LOG_MAX_RESPONSE_BYTES {
            return Ok(encoded);
        }
        if items.pop().is_none() {
            return Err(QueryServiceError::ResponseLimit);
        }
        has_more = true;
    }
}

fn read_ring_events(
    diagnostics: &DiagnosticWriterHandle,
    filters: &LogFilters,
    after: Option<&LogCursor>,
    selected: &mut SelectedLogs,
    cancellation: &QueryCancellation,
) -> Result<(), QueryServiceError> {
    let mut page_cursor = None;
    loop {
        cancellation.check()?;
        let request = PageRequest::with_limits(
            PageDirection::Ascending,
            page_cursor,
            MAX_PAGE_EVENTS,
            MAX_PAGE_BYTES,
        )
        .map_err(|_| QueryServiceError::Execution)?;
        let page = diagnostics
            .recorder()
            .try_query(request)
            .map_err(|_| QueryServiceError::Busy)?
            .blocking_wait()
            .map_err(|_| QueryServiceError::Execution)?;
        for event in page.events() {
            selected.consider(event.encoded(), filters, after)?;
        }
        let Some(next) = page.next_cursor() else {
            return Ok(());
        };
        page_cursor = Some(next);
    }
}

fn read_persistent_events(
    root: &PathBuf,
    active_writer: bool,
    filters: &LogFilters,
    after: Option<&LogCursor>,
    selected: &mut SelectedLogs,
    cancellation: &QueryCancellation,
) -> Result<(), QueryServiceError> {
    if !root.exists() {
        return Ok(());
    }
    let directory = DiagnosticDirectory::open(root).map_err(|_| QueryServiceError::Execution)?;
    let entries = directory
        .entries(MAX_DIAGNOSTIC_FILES)
        .map_err(|_| QueryServiceError::Execution)?;
    let active_segment = active_writer
        .then(|| {
            entries
                .iter()
                .filter(|entry| is_segment_name(entry.name()))
                .map(|entry| entry.name().to_os_string())
                .max()
        })
        .flatten();
    for entry in entries {
        cancellation.check()?;
        if !is_segment_name(entry.name()) {
            continue;
        }
        if entry.is_empty() {
            continue;
        }
        let mut lease = match directory.open_read(&entry, MAX_SEGMENT_BYTES as u64) {
            Ok(lease) => lease,
            Err(
                DiagnosticStoreError::Io
                | DiagnosticStoreError::Changed
                | DiagnosticStoreError::Unavailable,
            ) if active_segment.as_deref() == Some(entry.name()) => continue,
            Err(_) => return Err(QueryServiceError::Execution),
        };
        let mut bytes = Vec::with_capacity(
            usize::try_from(lease.len()).map_err(|_| QueryServiceError::Execution)?,
        );
        let mut offset = 0_u64;
        loop {
            cancellation.check()?;
            let chunk = lease
                .read_range(offset, DIAGNOSTIC_READ_CHUNK)
                .map_err(|_| QueryServiceError::Execution)?;
            if chunk.is_empty() {
                break;
            }
            offset = offset
                .checked_add(u64::try_from(chunk.len()).map_err(|_| QueryServiceError::Execution)?)
                .ok_or(QueryServiceError::Execution)?;
            bytes.extend_from_slice(&chunk);
            if bytes.len() > MAX_SEGMENT_BYTES {
                return Err(QueryServiceError::Execution);
            }
        }
        for line in bytes.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            selected.consider(line, filters, after)?;
        }
    }
    Ok(())
}

fn is_segment_name(name: &OsStr) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix("segment-"))
        .and_then(|suffix| suffix.strip_suffix(".jsonl"))
        .is_some_and(|index| {
            index.len() == 20
                && index.bytes().all(|byte| byte.is_ascii_digit())
                && index != "00000000000000000000"
        })
}

struct SelectedLogs {
    order: LogOrder,
    maximum: usize,
    items: BTreeMap<(String, String), LogItem>,
    event_ids: HashSet<String>,
}

impl SelectedLogs {
    fn new(order: LogOrder, limit: usize) -> Self {
        Self {
            order,
            maximum: limit.saturating_add(1),
            items: BTreeMap::new(),
            event_ids: HashSet::new(),
        }
    }

    fn consider(
        &mut self,
        encoded: &[u8],
        filters: &LogFilters,
        after: Option<&LogCursor>,
    ) -> Result<(), QueryServiceError> {
        if encoded.is_empty() || encoded.len() > MAX_PREPARED_EVENT_BYTES {
            return Err(QueryServiceError::Execution);
        }
        DiagnosticEvent::decode(encoded).map_err(|_| QueryServiceError::Execution)?;
        let value: Value =
            serde_json::from_slice(encoded).map_err(|_| QueryServiceError::Execution)?;
        let item = LogItem::from_value(value).ok_or(QueryServiceError::Execution)?;
        if !filters.matches(&item.value, &item.occurred_at, &item.event_id)
            || self.event_ids.contains(&item.event_id)
        {
            return Ok(());
        }
        if let Some(after) = after {
            let key = (&item.occurred_at, &item.event_id);
            let cursor = (&after.occurred_at, &after.event_id);
            if match self.order {
                LogOrder::Ascending => key <= cursor,
                LogOrder::Descending => key >= cursor,
            } {
                return Ok(());
            }
        }
        let key = (item.occurred_at.clone(), item.event_id.clone());
        self.event_ids.insert(item.event_id.clone());
        self.items.insert(key, item);
        if self.items.len() > self.maximum {
            let evicted_key = match self.order {
                LogOrder::Ascending => self.items.last_key_value().map(|(key, _)| key.clone()),
                LogOrder::Descending => self.items.first_key_value().map(|(key, _)| key.clone()),
            }
            .expect("an oversized log selection is not empty");
            let evicted = self
                .items
                .remove(&evicted_key)
                .expect("the selected log exists");
            self.event_ids.remove(&evicted.event_id);
        }
        Ok(())
    }

    fn into_items(self) -> (Vec<LogItem>, bool) {
        let has_more = self.items.len() > self.maximum.saturating_sub(1);
        let mut items = match self.order {
            LogOrder::Ascending => self.items.into_values().collect::<Vec<_>>(),
            LogOrder::Descending => self.items.into_values().rev().collect::<Vec<_>>(),
        };
        items.truncate(self.maximum.saturating_sub(1));
        (items, has_more)
    }
}

struct LogItem {
    occurred_at: String,
    event_id: String,
    value: Value,
}

impl LogItem {
    fn from_value(value: Value) -> Option<Self> {
        Some(Self {
            occurred_at: value.get("occurred_at")?.as_str()?.to_owned(),
            event_id: value.get("event_id")?.as_str()?.to_owned(),
            value,
        })
    }
}

#[derive(Clone)]
struct LogFilters {
    request_id: Option<String>,
    trace_id: Option<String>,
    session_key: Option<String>,
    level_min: Option<u8>,
    component: Option<&'static str>,
    since: Option<String>,
    until: Option<String>,
    order: LogOrder,
}

impl LogFilters {
    fn matches(&self, event: &Value, occurred_at: &str, _event_id: &str) -> bool {
        if self
            .since
            .as_deref()
            .is_some_and(|since| occurred_at < since)
            || self
                .until
                .as_deref()
                .is_some_and(|until| occurred_at >= until)
        {
            return false;
        }
        if self.component.is_some_and(|component| {
            event.get("component").and_then(Value::as_str) != Some(component)
        }) {
            return false;
        }
        if self.level_min.is_some_and(|minimum| {
            event
                .get("level")
                .and_then(Value::as_str)
                .and_then(level_rank)
                .is_none_or(|level| level < minimum)
        }) {
            return false;
        }
        let correlations = event.get("correlations");
        correlation_matches(correlations, "request_id", self.request_id.as_deref())
            && correlation_matches(correlations, "trace_id", self.trace_id.as_deref())
            && correlation_matches(
                correlations,
                "opaque_session_id",
                self.session_key.as_deref(),
            )
    }
}

fn correlation_matches(correlations: Option<&Value>, key: &str, expected: Option<&str>) -> bool {
    expected.is_none_or(|expected| {
        correlations
            .and_then(|value| value.get(key))
            .and_then(Value::as_str)
            == Some(expected)
    })
}

fn parse_level(value: Option<&str>) -> Option<Option<u8>> {
    match value {
        None => Some(None),
        Some(value @ ("debug" | "info" | "warn" | "error")) => level_rank(value).map(Some),
        Some(_) => None,
    }
}

fn level_rank(value: &str) -> Option<u8> {
    match value {
        "trace" => Some(0),
        "debug" => Some(1),
        "info" => Some(2),
        "warn" => Some(3),
        "error" => Some(4),
        _ => None,
    }
}

fn parse_component(value: Option<&str>) -> Option<Option<&'static str>> {
    match value {
        None => Some(None),
        Some("core") => Some(Some("core")),
        Some("router") => Some(Some("router")),
        Some("provider") => Some(Some("provider")),
        Some("storage") => Some(Some("storage")),
        Some("sessions") => Some(Some("sessions")),
        Some("diagnostics") => Some(Some("diagnostics")),
        Some("platform") => Some(Some("platform")),
        Some(_) => None,
    }
}

#[derive(Clone, Copy)]
enum LogOrder {
    Ascending,
    Descending,
}

fn parse_order(value: Option<&str>) -> Option<LogOrder> {
    match value {
        None | Some("desc") => Some(LogOrder::Descending),
        Some("asc") => Some(LogOrder::Ascending),
        Some(_) => None,
    }
}

pub(super) fn valid_optional_correlation(value: Option<&str>) -> bool {
    value.is_none_or(|value| {
        !value.is_empty()
            && value.len() <= 256
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    })
}

fn log_binding(filters: &LogFilters) -> String {
    format!(
        "request={};trace={};session={};level={};component={};since={};until={};order={}",
        filters.request_id.as_deref().unwrap_or_default(),
        filters.trace_id.as_deref().unwrap_or_default(),
        filters.session_key.as_deref().unwrap_or_default(),
        filters
            .level_min
            .map_or(String::new(), |level| level.to_string()),
        filters.component.unwrap_or_default(),
        filters.since.as_deref().unwrap_or_default(),
        filters.until.as_deref().unwrap_or_default(),
        match filters.order {
            LogOrder::Ascending => "asc",
            LogOrder::Descending => "desc",
        }
    )
}

#[derive(Deserialize, Serialize)]
struct LogCursor {
    occurred_at: String,
    event_id: String,
}

#[derive(Serialize)]
struct LogsResponse<'a> {
    schema_version: u8,
    items: Vec<&'a Value>,
    next_cursor: Option<String>,
    dropped_events: u64,
}
