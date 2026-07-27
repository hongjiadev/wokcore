use std::{
    ffi::OsStr,
    io,
    path::PathBuf,
    pin::Pin,
    task::{Context, Poll},
};

use axum::{
    body::Body,
    extract::{Extension, Query, State, rejection::QueryRejection},
    http::{StatusCode, header},
    response::Response,
};
use bytes::Bytes;
use futures_core::Stream;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;
use wokcore_diagnostics::{
    event::{DiagnosticEvent, MAX_PREPARED_EVENT_BYTES, UtcTimestamp},
    export::{
        CapabilitySummary, ExportBuildIdentity, ExportCapability, ExportConfiguration, ExportError,
        ExportPlatformCategory, ExportRedactionCounters, ExportSelection, ExportWorkerLease,
        LeakCanarySet, PreparedSupportPackage, ResourceSummary, SupportPackage, SupportPackageBody,
        prepare_support_package,
    },
    ring::{MAX_PAGE_BYTES, MAX_PAGE_EVENTS, PageDirection, PageRequest},
    segment::MAX_SEGMENT_BYTES,
    snapshot::MAX_FAILURE_SNAPSHOT_BYTES,
};
use wokcore_platform::{
    diagnostics::{
        DIAGNOSTIC_EXPORT_TEMPORARY_PREFIX, DiagnosticDirectory, DiagnosticReadLease,
        DiagnosticStagedFile, DiagnosticStoreError,
    },
    sessions::PinnedExportDestination,
};

use crate::{
    ServerState,
    observability::DiagnosticWriterHandle,
    runtime::{utc_epoch_seconds, utc_timestamp_from_epoch_seconds},
};

use super::{
    error::ApiError,
    logs::valid_optional_correlation,
    request_id::{RequestId, record_export_stream_failure},
};

const EXPORT_DEFAULT_WINDOW_SECONDS: u64 = 15 * 60;
const EXPORT_MAX_WINDOW_SECONDS: u64 = 24 * 60 * 60;
const EXPORT_DEFAULT_MAX_BYTES: usize = 16 * 1024 * 1024;
const EXPORT_MIN_MAX_BYTES: usize = 64 * 1024;
const EXPORT_MAX_MAX_BYTES: usize = 64 * 1024 * 1024;
const EXPORT_PACKAGE_RESERVE_BYTES: usize = 16 * 1024;
const EXPORT_STREAM_CHUNK_BYTES: usize = 16 * 1024;
const EXPORT_STREAM_QUEUE_DEPTH: usize = 2;
const MAX_DIAGNOSTIC_FILES: usize = 4_096;
const MAX_EXPORT_SOURCE_FILES: usize = 4_096;
const DIAGNOSTIC_READ_CHUNK: usize = 64 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ExportQuery {
    request_id: Option<String>,
    trace_id: Option<String>,
    session_key: Option<String>,
    since: Option<String>,
    until: Option<String>,
    include_snapshots: Option<bool>,
    max_bytes: Option<usize>,
}

pub(super) async fn diagnostics_export(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
    query: Result<Query<ExportQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid_query(request_id))?;
    if !valid_optional_correlation(query.request_id.as_deref())
        || !valid_optional_correlation(query.trace_id.as_deref())
        || !valid_optional_correlation(query.session_key.as_deref())
    {
        return Err(ApiError::invalid_query(request_id));
    }
    let max_bytes = query.max_bytes.unwrap_or(EXPORT_DEFAULT_MAX_BYTES);
    if !(EXPORT_MIN_MAX_BYTES..=EXPORT_MAX_MAX_BYTES).contains(&max_bytes) {
        return Err(ApiError::export_limit_out_of_range(request_id));
    }
    let now = state
        .token_metadata
        .now()
        .map_err(|_| ApiError::internal_failure(request_id))?;
    let content_disposition =
        export_content_disposition(&now).ok_or_else(|| ApiError::internal_failure(request_id))?;
    let until = query.until.unwrap_or_else(|| now.clone());
    let until_epoch =
        utc_epoch_seconds(&until).ok_or_else(|| ApiError::invalid_time_range(request_id))?;
    let since = match query.since {
        Some(since) => since,
        None => utc_timestamp_from_epoch_seconds(
            until_epoch.saturating_sub(EXPORT_DEFAULT_WINDOW_SECONDS),
        )
        .ok_or_else(|| ApiError::invalid_time_range(request_id))?,
    };
    let since_epoch =
        utc_epoch_seconds(&since).ok_or_else(|| ApiError::invalid_time_range(request_id))?;
    if UtcTimestamp::parse(&since).is_err()
        || UtcTimestamp::parse(&until).is_err()
        || since_epoch >= until_epoch
        || until_epoch.saturating_sub(since_epoch) > EXPORT_MAX_WINDOW_SECONDS
    {
        return Err(ApiError::invalid_time_range(request_id));
    }
    let runtime = state
        .query
        .clone()
        .ok_or_else(|| ApiError::query_busy(request_id))?;
    let operation = runtime
        .export_coordinator()
        .try_begin()
        .map_err(|error| map_export_error(error, request_id))?;
    let (owner, worker) = operation
        .split()
        .map_err(|error| map_export_error(error, request_id))?;
    let filters = ExportFilters {
        request_id: query.request_id,
        trace_id: query.trace_id,
        session_key: query.session_key,
        since,
        until,
        include_snapshots: query.include_snapshots.unwrap_or(false),
    };
    let diagnostics_root = runtime.diagnostics_root().to_path_buf();
    let diagnostics = state.diagnostics.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        prepare_filtered_export(diagnostics_root, diagnostics, filters, max_bytes, worker)
    })
    .await
    .map_err(|_| ApiError::internal_failure(request_id))?
    .map_err(|error| map_export_error(error, request_id))?;
    if prepared.package.stats().package_bytes() > max_bytes as u64 {
        return Err(ApiError::export_limit_out_of_range(request_id));
    }
    let package_bytes = prepared.package.stats().package_bytes();
    let body = prepared
        .package
        .into_body(owner)
        .map_err(|error| map_export_error(error, request_id))?;
    let (sender, receiver) = mpsc::channel(EXPORT_STREAM_QUEUE_DEPTH);
    tokio::task::spawn_blocking(move || {
        stream_package(body, sender, prepared.temporary, state, request_id)
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/zip")
        .header(header::CONTENT_DISPOSITION, content_disposition)
        .header(header::CONTENT_LENGTH, package_bytes)
        .body(Body::from_stream(ExportBodyStream { receiver }))
        .map_err(|_| ApiError::internal_failure(request_id))
}

struct PreparedExport {
    package: PreparedSupportPackage,
    temporary: tempfile::TempDir,
}

fn prepare_filtered_export(
    diagnostics_root: PathBuf,
    diagnostics: Option<DiagnosticWriterHandle>,
    filters: ExportFilters,
    max_bytes: usize,
    worker: ExportWorkerLease,
) -> Result<PreparedExport, ExportError> {
    std::fs::create_dir_all(&diagnostics_root).map_err(|_| ExportError::Io)?;
    let source_directory =
        DiagnosticDirectory::open(&diagnostics_root).map_err(|_| ExportError::Boundary)?;
    let source_entries = source_directory
        .entries(MAX_DIAGNOSTIC_FILES)
        .map_err(|_| ExportError::Boundary)?;
    let active_segment = diagnostics
        .is_some()
        .then(|| {
            source_entries
                .iter()
                .filter(|entry| is_segment_name(entry.name()))
                .map(|entry| entry.name().to_os_string())
                .max()
        })
        .flatten();
    let temporary = tempfile::Builder::new()
        .prefix(DIAGNOSTIC_EXPORT_TEMPORARY_PREFIX)
        .tempdir_in(&diagnostics_root)
        .map_err(|_| ExportError::Io)?;
    let staged_directory =
        DiagnosticDirectory::open(temporary.path()).map_err(|_| ExportError::Boundary)?;
    let mut selection = SelectionBudget::new(max_bytes);
    let mut next_segment = 1_u64;
    for entry in source_entries {
        let snapshot = is_snapshot_name(entry.name());
        let maximum = if is_segment_name(entry.name()) {
            MAX_SEGMENT_BYTES
        } else if snapshot && filters.include_snapshots {
            MAX_FAILURE_SNAPSHOT_BYTES
        } else {
            continue;
        };
        if entry.is_empty() {
            continue;
        }
        let lease = match source_directory.open_read(&entry, maximum as u64) {
            Ok(lease) => lease,
            Err(
                DiagnosticStoreError::Io
                | DiagnosticStoreError::Changed
                | DiagnosticStoreError::Unavailable,
            ) if active_segment.as_deref() == Some(entry.name()) => continue,
            Err(_) => return Err(ExportError::Boundary),
        };
        stage_filtered_lease(
            lease,
            snapshot,
            &staged_directory,
            &mut next_segment,
            &filters,
            &mut selection,
        )?;
    }
    let ring_bytes = if let Some(diagnostics) = diagnostics.as_ref() {
        stage_ring_events(
            diagnostics,
            &staged_directory,
            &mut next_segment,
            &filters,
            &mut selection,
        )?
    } else {
        0
    };
    let mut leases = Vec::new();
    for entry in staged_directory
        .entries(MAX_EXPORT_SOURCE_FILES)
        .map_err(|_| ExportError::Boundary)?
    {
        if !is_segment_name(entry.name()) {
            continue;
        }
        leases.push(
            staged_directory
                .open_read(&entry, MAX_SEGMENT_BYTES as u64)
                .map_err(|_| ExportError::Boundary)?,
        );
    }
    let build = ExportBuildIdentity::new(
        env!("CARGO_PKG_VERSION"),
        option_env!("WOKCORE_GIT_COMMIT").unwrap_or("0000000000000000000000000000000000000000"),
        1,
        1,
    )?;
    let capabilities = CapabilitySummary::new(vec![
        ExportCapability::ClientTokenIssue,
        ExportCapability::ClientTokenRevoke,
        ExportCapability::DiagnosticsEventsV1,
        ExportCapability::DiagnosticsExportV1,
        ExportCapability::DiscoveryV1,
        ExportCapability::ServiceDrain,
        ExportCapability::ServiceStatus,
        ExportCapability::SessionsIndexV1,
        ExportCapability::SessionsMessagesV1,
        ExportCapability::UsageSessionV1,
    ])?;
    let configuration = ExportConfiguration::new(true, 7, 4, build, capabilities)?;
    let suppressed_snapshots = diagnostics.as_ref().map_or(0, |diagnostics| {
        let metrics = diagnostics.snapshots().metrics();
        metrics
            .queue_full()
            .saturating_add(metrics.queue_closed())
            .saturating_add(metrics.cooldown_suppressed())
            .saturating_add(metrics.budget_suppressed())
            .saturating_add(metrics.io_errors())
    });
    let resources = ResourceSummary::new(
        ring_bytes as u64,
        selection.selected_bytes as u64,
        suppressed_snapshots,
        ExportPlatformCategory::None,
        Vec::new(),
        ExportRedactionCounters::default(),
    )?;
    let export_selection = if selection.omitted_events == 0 {
        ExportSelection::complete()
    } else {
        ExportSelection::truncated(selection.omitted_events)?
    };
    let mut package = SupportPackage::new(leases, configuration, resources, export_selection)?;
    let destination = PinnedExportDestination::create(temporary.path().join("package.zip"), &[])
        .map_err(|_| ExportError::Boundary)?;
    let package =
        prepare_support_package(worker, destination, &mut package, &LeakCanarySet::new())?;
    Ok(PreparedExport { package, temporary })
}

fn stage_filtered_lease(
    mut lease: DiagnosticReadLease,
    skip_first_line: bool,
    destination: &DiagnosticDirectory,
    next_segment: &mut u64,
    filters: &ExportFilters,
    selection: &mut SelectionBudget,
) -> Result<(), ExportError> {
    let name = next_segment_name(next_segment)?;
    let mut staged = destination
        .create_staged(OsStr::new(&name), MAX_SEGMENT_BYTES as u64)
        .map_err(|_| ExportError::Boundary)?;
    let mut pending = Vec::with_capacity(DIAGNOSTIC_READ_CHUNK + MAX_PREPARED_EVENT_BYTES);
    let mut offset = 0_u64;
    let mut first_line = true;
    loop {
        let chunk = lease
            .read_range(offset, DIAGNOSTIC_READ_CHUNK)
            .map_err(|_| ExportError::Boundary)?;
        if chunk.is_empty() {
            break;
        }
        offset = offset
            .checked_add(u64::try_from(chunk.len()).map_err(|_| ExportError::Limit)?)
            .ok_or(ExportError::Limit)?;
        pending.extend_from_slice(&chunk);
        let mut consumed = 0_usize;
        while let Some(newline) = pending[consumed..].iter().position(|byte| *byte == b'\n') {
            let end = consumed.checked_add(newline).ok_or(ExportError::Limit)?;
            let line = &pending[consumed..end];
            if skip_first_line && first_line {
                first_line = false;
            } else if !line.is_empty() {
                selection.consider(line, filters, &mut staged)?;
            }
            consumed = end.checked_add(1).ok_or(ExportError::Limit)?;
        }
        if consumed != 0 {
            pending.drain(..consumed);
        }
        if pending.len() > MAX_PREPARED_EVENT_BYTES {
            return Err(ExportError::InvalidInput);
        }
    }
    if !pending.is_empty() {
        return Err(ExportError::InvalidInput);
    }
    if staged.is_empty() {
        return Ok(());
    }
    staged.commit().map_err(|_| ExportError::Boundary)?;
    *next_segment = next_segment.checked_add(1).ok_or(ExportError::Limit)?;
    Ok(())
}

fn stage_ring_events(
    diagnostics: &DiagnosticWriterHandle,
    destination: &DiagnosticDirectory,
    next_segment: &mut u64,
    filters: &ExportFilters,
    selection: &mut SelectionBudget,
) -> Result<usize, ExportError> {
    let mut cursor = None;
    loop {
        let request = PageRequest::with_limits(
            PageDirection::Ascending,
            cursor,
            MAX_PAGE_EVENTS,
            MAX_PAGE_BYTES,
        )
        .map_err(|_| ExportError::InvalidInput)?;
        let page = diagnostics
            .recorder()
            .try_query(request)
            .map_err(|_| ExportError::Busy)?
            .blocking_wait()
            .map_err(|_| ExportError::Io)?;
        let retained_bytes = page.ring_retained_bytes();
        let name = next_segment_name(next_segment)?;
        let mut staged = destination
            .create_staged(OsStr::new(&name), MAX_SEGMENT_BYTES as u64)
            .map_err(|_| ExportError::Boundary)?;
        for event in page.events() {
            selection.consider(event.encoded(), filters, &mut staged)?;
        }
        if !staged.is_empty() {
            staged.commit().map_err(|_| ExportError::Boundary)?;
            *next_segment = next_segment.checked_add(1).ok_or(ExportError::Limit)?;
        }
        let Some(next) = page.next_cursor() else {
            return Ok(retained_bytes);
        };
        cursor = Some(next);
    }
}

struct SelectionBudget {
    remaining_bytes: usize,
    selected_bytes: usize,
    omitted_events: u64,
    exhausted: bool,
}

impl SelectionBudget {
    fn new(max_bytes: usize) -> Self {
        Self {
            remaining_bytes: max_bytes.saturating_sub(EXPORT_PACKAGE_RESERVE_BYTES),
            selected_bytes: 0,
            omitted_events: 0,
            exhausted: false,
        }
    }

    fn consider(
        &mut self,
        encoded: &[u8],
        filters: &ExportFilters,
        destination: &mut DiagnosticStagedFile,
    ) -> Result<(), ExportError> {
        if encoded.is_empty() || encoded.len() > MAX_PREPARED_EVENT_BYTES {
            return Err(ExportError::InvalidInput);
        }
        let event = DiagnosticEvent::decode(encoded).map_err(|_| ExportError::InvalidInput)?;
        let value: Value =
            serde_json::from_slice(encoded).map_err(|_| ExportError::InvalidInput)?;
        if !filters.matches(&value) {
            return Ok(());
        }
        let required = encoded.len().checked_add(1).ok_or(ExportError::Limit)?;
        if self.exhausted || required > self.remaining_bytes {
            self.exhausted = true;
            self.omitted_events = self.omitted_events.saturating_add(1);
            return Ok(());
        }
        let _ = event;
        destination
            .write_chunk(encoded)
            .and_then(|()| destination.write_chunk(b"\n"))
            .map_err(|_| ExportError::Boundary)?;
        self.remaining_bytes -= required;
        self.selected_bytes = self
            .selected_bytes
            .checked_add(required)
            .ok_or(ExportError::Limit)?;
        Ok(())
    }
}

#[derive(Clone)]
struct ExportFilters {
    request_id: Option<String>,
    trace_id: Option<String>,
    session_key: Option<String>,
    since: String,
    until: String,
    include_snapshots: bool,
}

impl ExportFilters {
    fn matches(&self, event: &Value) -> bool {
        let Some(occurred_at) = event.get("occurred_at").and_then(Value::as_str) else {
            return false;
        };
        if occurred_at < self.since.as_str() || occurred_at >= self.until.as_str() {
            return false;
        }
        let correlations = event.get("correlations");
        export_correlation_matches(correlations, "request_id", self.request_id.as_deref())
            && export_correlation_matches(correlations, "trace_id", self.trace_id.as_deref())
            && export_correlation_matches(
                correlations,
                "opaque_session_id",
                self.session_key.as_deref(),
            )
    }
}

fn export_correlation_matches(
    correlations: Option<&Value>,
    key: &str,
    expected: Option<&str>,
) -> bool {
    expected.is_none_or(|expected| {
        correlations
            .and_then(|value| value.get(key))
            .and_then(Value::as_str)
            == Some(expected)
    })
}

fn next_segment_name(next_segment: &u64) -> Result<String, ExportError> {
    if *next_segment == 0 || *next_segment > MAX_EXPORT_SOURCE_FILES as u64 {
        return Err(ExportError::Limit);
    }
    Ok(format!("segment-{next_segment:020}.jsonl"))
}

fn is_segment_name(name: &OsStr) -> bool {
    has_numbered_jsonl_name(name, "segment-")
}

fn is_snapshot_name(name: &OsStr) -> bool {
    has_numbered_jsonl_name(name, "snapshot-")
}

fn has_numbered_jsonl_name(name: &OsStr, prefix: &str) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix(prefix))
        .and_then(|suffix| suffix.strip_suffix(".jsonl"))
        .is_some_and(|index| {
            index.len() == 20
                && index.bytes().all(|byte| byte.is_ascii_digit())
                && index != "00000000000000000000"
        })
}

fn stream_package(
    mut body: SupportPackageBody,
    sender: mpsc::Sender<Result<Bytes, io::Error>>,
    temporary: tempfile::TempDir,
    state: ServerState,
    request_id: RequestId,
) {
    let _temporary = temporary;
    loop {
        match body.read_next(EXPORT_STREAM_CHUNK_BYTES) {
            Ok(Some(bytes)) => {
                if sender.blocking_send(Ok(Bytes::from(bytes))).is_err() {
                    return;
                }
            }
            Ok(None) => {
                if body.finish().is_err() {
                    record_export_stream_failure(&state, request_id);
                    let _ = sender.blocking_send(Err(io::Error::other("diagnostic export failed")));
                }
                return;
            }
            Err(_) => {
                record_export_stream_failure(&state, request_id);
                let _ = sender.blocking_send(Err(io::Error::other("diagnostic export failed")));
                return;
            }
        }
    }
}

struct ExportBodyStream {
    receiver: mpsc::Receiver<Result<Bytes, io::Error>>,
}

impl Stream for ExportBodyStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(context)
    }
}

fn map_export_error(error: ExportError, request_id: RequestId) -> ApiError {
    match error {
        ExportError::Busy => ApiError::diagnostics_export_busy(request_id),
        ExportError::Limit => ApiError::export_limit_out_of_range(request_id),
        ExportError::InvalidInput => ApiError::invalid_query(request_id),
        ExportError::Cancelled
        | ExportError::Boundary
        | ExportError::Io
        | ExportError::LeakDetected
        | ExportError::InvalidPackage => ApiError::internal_failure(request_id),
    }
}

fn export_content_disposition(timestamp: &str) -> Option<String> {
    UtcTimestamp::parse(timestamp).ok()?;
    let compact = timestamp
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>();
    Some(format!(
        "attachment; filename=\"wokcore-diagnostics-{compact}.zip\""
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::ExportFilters;

    #[test]
    fn export_filters_match_half_open_time_and_correlations() {
        let filters = ExportFilters {
            request_id: Some("request-1".to_owned()),
            trace_id: None,
            session_key: None,
            since: "2026-07-26T01:00:00Z".to_owned(),
            until: "2026-07-26T02:00:00Z".to_owned(),
            include_snapshots: false,
        };
        assert!(filters.matches(&json!({
            "occurred_at": "2026-07-26T01:00:00Z",
            "correlations": {"request_id": "request-1"}
        })));
        assert!(!filters.matches(&json!({
            "occurred_at": "2026-07-26T02:00:00Z",
            "correlations": {"request_id": "request-1"}
        })));
        assert!(!filters.matches(&json!({
            "occurred_at": "2026-07-26T01:30:00Z",
            "correlations": {"request_id": "other"}
        })));
    }
}
