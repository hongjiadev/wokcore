use std::{
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::SystemTime,
};

use thiserror::Error;
use tokio::sync::mpsc;
use wokcore_platform::diagnostics::{DiagnosticDirectory, DiagnosticFile, DiagnosticStoreError};

use crate::event::{
    DiagnosticDropCounts, DiagnosticEventCode, DiagnosticLevel, EventId, PreparedDiagnosticEvent,
};
use crate::retention::{RetentionError, RetentionManager, RetentionTrigger};

pub const DURABLE_QUEUE_CAPACITY: usize = 256;
pub const MAX_BATCH_EVENTS: usize = 128;
pub const MAX_BATCH_EVENT_BYTES: usize = 256 * 1024;
pub const MAX_SEGMENT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableEventKind {
    Transient,
    Warning,
    Error,
    Operational,
}

pub struct DurableFilter;

impl DurableFilter {
    pub const fn should_persist(kind: DurableEventKind) -> bool {
        !matches!(kind, DurableEventKind::Transient)
    }

    fn prepared_should_persist(event: &PreparedDiagnosticEvent) -> Result<bool, ()> {
        let event =
            crate::event::decode_trusted_prepared_encoding(event.encoded()).map_err(|_| ())?;
        Ok(!matches!(
            event.level(),
            DiagnosticLevel::Trace | DiagnosticLevel::Debug
        ) && (matches!(
            event.level(),
            DiagnosticLevel::Warn | DiagnosticLevel::Error
        ) || matches!(
            event.code(),
            DiagnosticEventCode::LifecycleTransition
                | DiagnosticEventCode::RetryDecision
                | DiagnosticEventCode::FailoverDecision
                | DiagnosticEventCode::DiagnosticDrop
        )))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticDropCause {
    IngressFull,
    IngressClosed,
    WriterUnavailable,
    InvalidEvent,
    OversizedEvent,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DiagnosticDropSummary {
    ingress_full: u64,
    ingress_closed: u64,
    writer_unavailable: u64,
    invalid_event: u64,
    oversized_event: u64,
}

impl DiagnosticDropSummary {
    pub const fn ingress_full(self) -> u64 {
        self.ingress_full
    }

    pub const fn writer_unavailable(self) -> u64 {
        self.writer_unavailable
    }

    pub const fn ingress_closed(self) -> u64 {
        self.ingress_closed
    }

    pub const fn invalid_event(self) -> u64 {
        self.invalid_event
    }

    pub const fn oversized_event(self) -> u64 {
        self.oversized_event
    }

    pub const fn total(self) -> u64 {
        self.ingress_full
            .saturating_add(self.ingress_closed)
            .saturating_add(self.writer_unavailable)
            .saturating_add(self.invalid_event)
            .saturating_add(self.oversized_event)
    }
}

#[derive(Debug, Default)]
pub struct DropRecoveryTracker {
    pending: DiagnosticDropSummary,
}

impl DropRecoveryTracker {
    pub const fn new() -> Self {
        Self {
            pending: DiagnosticDropSummary {
                ingress_full: 0,
                ingress_closed: 0,
                writer_unavailable: 0,
                invalid_event: 0,
                oversized_event: 0,
            },
        }
    }

    pub fn observe(&mut self, cause: DiagnosticDropCause, count: u64) {
        let value = match cause {
            DiagnosticDropCause::IngressFull => &mut self.pending.ingress_full,
            DiagnosticDropCause::IngressClosed => &mut self.pending.ingress_closed,
            DiagnosticDropCause::WriterUnavailable => &mut self.pending.writer_unavailable,
            DiagnosticDropCause::InvalidEvent => &mut self.pending.invalid_event,
            DiagnosticDropCause::OversizedEvent => &mut self.pending.oversized_event,
        };
        *value = value.saturating_add(count);
    }

    pub fn on_progress_resumed(&mut self) -> Option<DiagnosticDropSummary> {
        if self.pending.total() == 0 {
            return None;
        }
        Some(std::mem::take(&mut self.pending))
    }
}

impl From<DiagnosticDropSummary> for DiagnosticDropCounts {
    fn from(summary: DiagnosticDropSummary) -> Self {
        Self::new(
            summary.ingress_full,
            summary.ingress_closed,
            summary.writer_unavailable,
            summary.invalid_event,
            summary.oversized_event,
        )
    }
}

#[derive(Default)]
struct DropAtoms {
    ingress_full: AtomicU64,
    ingress_closed: AtomicU64,
    writer_unavailable: AtomicU64,
    invalid_event: AtomicU64,
    oversized_event: AtomicU64,
}

impl DropAtoms {
    fn observe(&self, cause: DiagnosticDropCause, count: u64) {
        let counter = match cause {
            DiagnosticDropCause::IngressFull => &self.ingress_full,
            DiagnosticDropCause::IngressClosed => &self.ingress_closed,
            DiagnosticDropCause::WriterUnavailable => &self.writer_unavailable,
            DiagnosticDropCause::InvalidEvent => &self.invalid_event,
            DiagnosticDropCause::OversizedEvent => &self.oversized_event,
        };
        let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            Some(value.saturating_add(count))
        });
    }

    fn take(&self) -> DiagnosticDropSummary {
        DiagnosticDropSummary {
            ingress_full: self.ingress_full.swap(0, Ordering::AcqRel),
            ingress_closed: self.ingress_closed.swap(0, Ordering::AcqRel),
            writer_unavailable: self.writer_unavailable.swap(0, Ordering::AcqRel),
            invalid_event: self.invalid_event.swap(0, Ordering::AcqRel),
            oversized_event: self.oversized_event.swap(0, Ordering::AcqRel),
        }
    }

    fn snapshot(&self) -> DiagnosticDropSummary {
        DiagnosticDropSummary {
            ingress_full: self.ingress_full.load(Ordering::Acquire),
            ingress_closed: self.ingress_closed.load(Ordering::Acquire),
            writer_unavailable: self.writer_unavailable.load(Ordering::Acquire),
            invalid_event: self.invalid_event.load(Ordering::Acquire),
            oversized_event: self.oversized_event.load(Ordering::Acquire),
        }
    }

    fn restore(&self, summary: DiagnosticDropSummary) {
        self.observe(DiagnosticDropCause::IngressFull, summary.ingress_full);
        self.observe(DiagnosticDropCause::IngressClosed, summary.ingress_closed);
        self.observe(
            DiagnosticDropCause::WriterUnavailable,
            summary.writer_unavailable,
        );
        self.observe(DiagnosticDropCause::InvalidEvent, summary.invalid_event);
        self.observe(DiagnosticDropCause::OversizedEvent, summary.oversized_event);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableRecordOutcome {
    Accepted,
    Filtered,
    DroppedFull,
    DroppedClosed,
}

#[derive(Clone)]
pub struct DurableProducer {
    sender: mpsc::Sender<PreparedDiagnosticEvent>,
    drops: Arc<DropAtoms>,
    drop_summary_inflight: Arc<AtomicBool>,
}

impl DurableProducer {
    pub fn new<F, E>(root: impl AsRef<Path>, request_drop: F) -> (Self, DurableWriterOwner<F>)
    where
        F: FnMut(DiagnosticDropSummary) -> Result<(), E>,
    {
        let root = root.as_ref().to_path_buf();
        let (sender, receiver) = mpsc::channel(DURABLE_QUEUE_CAPACITY);
        let drops = Arc::new(DropAtoms::default());
        let drop_summary_inflight = Arc::new(AtomicBool::new(false));
        (
            Self {
                sender,
                drops: Arc::clone(&drops),
                drop_summary_inflight: Arc::clone(&drop_summary_inflight),
            },
            DurableWriterOwner {
                receiver,
                writer: SegmentWriter::new(&root),
                retention: RetentionManager::new(root),
                startup_recovery: None,
                drops,
                drop_summary_inflight,
                request_drop,
                indeterminate_drop: None,
                pending_ingress: None,
                progress_observed: false,
            },
        )
    }

    pub fn try_record(&self, event: PreparedDiagnosticEvent) -> DurableRecordOutcome {
        match DurableFilter::prepared_should_persist(&event) {
            Ok(true) => {}
            Ok(false) => return DurableRecordOutcome::Filtered,
            Err(()) => {
                self.drops.observe(DiagnosticDropCause::InvalidEvent, 1);
                return DurableRecordOutcome::Filtered;
            }
        }
        match self.sender.try_send(event) {
            Ok(()) => DurableRecordOutcome::Accepted,
            Err(mpsc::error::TrySendError::Full(event)) => {
                if let Some(summary) = drop_summary_from_prepared(&event) {
                    self.drops.restore(summary);
                    self.drop_summary_inflight.store(false, Ordering::Release);
                }
                self.drops.observe(DiagnosticDropCause::IngressFull, 1);
                DurableRecordOutcome::DroppedFull
            }
            Err(mpsc::error::TrySendError::Closed(event)) => {
                if let Some(summary) = drop_summary_from_prepared(&event) {
                    self.drops.restore(summary);
                    self.drop_summary_inflight.store(false, Ordering::Release);
                }
                self.drops.observe(DiagnosticDropCause::IngressClosed, 1);
                DurableRecordOutcome::DroppedClosed
            }
        }
    }

    pub fn drop_metrics(&self) -> DiagnosticDropSummary {
        self.drops.snapshot()
    }
}

impl fmt::Debug for DurableProducer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DurableProducer([redacted])")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableProcessOutcome {
    Idle,
    DropSummaryRequested,
    Written { events: usize, rotations: usize },
}

#[derive(Debug, Error)]
pub enum DurableProcessError {
    #[error("durable diagnostic segment operation failed")]
    Segment(#[from] SegmentError),
    #[error("durable diagnostic drop summary preparation failed")]
    DropPreparation,
    #[error("durable diagnostic retention operation failed")]
    Retention(#[from] RetentionError),
}

pub struct DurableWriterOwner<F> {
    receiver: mpsc::Receiver<PreparedDiagnosticEvent>,
    writer: SegmentWriter,
    retention: RetentionManager,
    startup_recovery: Option<RecoveryReport>,
    drops: Arc<DropAtoms>,
    drop_summary_inflight: Arc<AtomicBool>,
    request_drop: F,
    indeterminate_drop: Option<(PreparedDiagnosticEvent, DiagnosticDropSummary)>,
    pending_ingress: Option<PreparedDiagnosticEvent>,
    progress_observed: bool,
}

impl<F, E> DurableWriterOwner<F>
where
    F: FnMut(DiagnosticDropSummary) -> Result<(), E>,
{
    pub fn try_process_next(&mut self) -> Result<DurableProcessOutcome, DurableProcessError> {
        self.try_process_next_at(SystemTime::now())
    }

    pub fn recover_startup(
        &mut self,
        now: SystemTime,
    ) -> Result<RecoveryReport, DurableProcessError> {
        if let Some(recovery) = self.startup_recovery {
            return Ok(recovery);
        }
        let recovery = self.writer.recover()?;
        self.retention.enforce_with_active(
            RetentionTrigger::Startup,
            now,
            &[],
            Some(recovery.active_segment()),
        )?;
        self.startup_recovery = Some(recovery);
        Ok(recovery)
    }

    pub fn try_process_next_at(
        &mut self,
        now: SystemTime,
    ) -> Result<DurableProcessOutcome, DurableProcessError> {
        let _ = self.recover_startup(now)?;
        if let Some((prepared, summary)) = self.indeterminate_drop.take() {
            let recovery = match self.writer.recover() {
                Ok(recovery) => recovery,
                Err(error) => {
                    self.indeterminate_drop = Some((prepared, summary));
                    return Err(error.into());
                }
            };
            if recovery.last_sequence() == prepared.sequence()
                && recovery.last_event_id() == Some(prepared.event_id())
            {
                // The append reached durable storage before the reported failure.
                self.drop_summary_inflight.store(false, Ordering::Release);
            } else if recovery.last_sequence() < prepared.sequence() {
                self.drops.restore(summary);
                self.drop_summary_inflight.store(false, Ordering::Release);
            } else {
                self.indeterminate_drop = Some((prepared, summary));
                return Err(SegmentError::InvalidData.into());
            }
        }
        let first = if let Some(pending) = self.pending_ingress.take() {
            pending
        } else {
            match self.receiver.try_recv() {
                Ok(event) => event,
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                    if self.progress_observed {
                        if self.drop_summary_inflight.load(Ordering::Acquire) {
                            return Ok(DurableProcessOutcome::Idle);
                        }
                        let summary = self.drops.take();
                        if summary.total() == 0 {
                            return Ok(DurableProcessOutcome::Idle);
                        }
                        self.drop_summary_inflight.store(true, Ordering::Release);
                        match (self.request_drop)(summary) {
                            Ok(()) => {
                                return Ok(DurableProcessOutcome::DropSummaryRequested);
                            }
                            Err(_) => {
                                self.drops.restore(summary);
                                self.drop_summary_inflight.store(false, Ordering::Release);
                                return Err(DurableProcessError::DropPreparation);
                            }
                        }
                    } else {
                        return Ok(DurableProcessOutcome::Idle);
                    }
                }
            }
        };
        let attempted_drop =
            drop_summary_from_prepared(&first).map(|summary| (first.clone(), summary));
        let is_drop_summary = attempted_drop.is_some();
        let mut batch = DurableBatch::new();
        batch
            .try_push(first)
            .map_err(|_| SegmentError::InvalidData)?;
        while !is_drop_summary && batch.event_count() < MAX_BATCH_EVENTS {
            let event = match self.receiver.try_recv() {
                Ok(event) => event,
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                    break;
                }
            };
            if drop_summary_from_prepared(&event).is_some() {
                self.pending_ingress = Some(event);
                break;
            }
            if batch
                .event_bytes()
                .checked_add(event.encoded_len())
                .is_none_or(|bytes| bytes > MAX_BATCH_EVENT_BYTES)
            {
                self.pending_ingress = Some(event);
                break;
            }
            batch.try_push(event).map_err(|_| {
                self.drops.observe(DiagnosticDropCause::InvalidEvent, 1);
                DurableProcessError::Segment(SegmentError::InvalidData)
            })?;
        }
        let event_count = batch.event_count();
        match self.writer.flush(batch) {
            Ok(outcome) => {
                if attempted_drop.is_some() {
                    self.drop_summary_inflight.store(false, Ordering::Release);
                }
                if let Some(active_segment) = outcome.active_segment() {
                    self.retention.enforce_with_active(
                        RetentionTrigger::Rotation,
                        now,
                        &[],
                        Some(active_segment),
                    )?;
                }
                self.progress_observed = true;
                Ok(DurableProcessOutcome::Written {
                    events: event_count,
                    rotations: outcome.rotation_count(),
                })
            }
            Err(error) => {
                if let Some(indeterminate) = attempted_drop {
                    self.indeterminate_drop = Some(indeterminate);
                }
                self.drops.observe(
                    DiagnosticDropCause::WriterUnavailable,
                    u64::try_from(event_count).unwrap_or(u64::MAX),
                );
                Err(error.into())
            }
        }
    }
}

impl<F> fmt::Debug for DurableWriterOwner<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DurableWriterOwner([redacted])")
    }
}

fn drop_summary_from_prepared(prepared: &PreparedDiagnosticEvent) -> Option<DiagnosticDropSummary> {
    let event = crate::event::decode_trusted_prepared_encoding(prepared.encoded()).ok()?;
    if event.code() != DiagnosticEventCode::DiagnosticDrop {
        return None;
    }
    let counts = event.diagnostic_drop_counts()?;
    Some(DiagnosticDropSummary {
        ingress_full: counts.ingress_full(),
        ingress_closed: counts.ingress_closed(),
        writer_unavailable: counts.writer_failures(),
        invalid_event: counts.invalid_events(),
        oversized_event: counts.oversized_events(),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BatchLimit {
    EventCount,
    EventBytes,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("diagnostic batch limit reached")]
pub struct BatchPushError {
    limit: BatchLimit,
}

impl BatchPushError {
    pub const fn new(limit: BatchLimit) -> Self {
        Self { limit }
    }

    pub const fn limit(self) -> BatchLimit {
        self.limit
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum BatchConfigurationError {
    #[error("invalid diagnostic batch limits")]
    InvalidLimits,
}

pub struct DurableBatch {
    events: Vec<PreparedDiagnosticEvent>,
    event_bytes: usize,
    max_events: usize,
    max_event_bytes: usize,
}

impl DurableBatch {
    pub fn new() -> Self {
        Self {
            events: Vec::with_capacity(MAX_BATCH_EVENTS),
            event_bytes: 0,
            max_events: MAX_BATCH_EVENTS,
            max_event_bytes: MAX_BATCH_EVENT_BYTES,
        }
    }

    pub fn with_limits(
        max_events: usize,
        max_event_bytes: usize,
    ) -> Result<Self, BatchConfigurationError> {
        if max_events == 0
            || max_events > MAX_BATCH_EVENTS
            || max_event_bytes == 0
            || max_event_bytes > MAX_BATCH_EVENT_BYTES
        {
            return Err(BatchConfigurationError::InvalidLimits);
        }
        Ok(Self {
            events: Vec::with_capacity(max_events),
            event_bytes: 0,
            max_events,
            max_event_bytes,
        })
    }

    pub fn try_push(&mut self, event: PreparedDiagnosticEvent) -> Result<(), BatchPushError> {
        if self.events.len() >= self.max_events {
            return Err(BatchPushError::new(BatchLimit::EventCount));
        }
        let next_bytes = self
            .event_bytes
            .checked_add(event.encoded_len())
            .ok_or_else(|| BatchPushError::new(BatchLimit::EventBytes))?;
        if next_bytes > self.max_event_bytes {
            return Err(BatchPushError::new(BatchLimit::EventBytes));
        }
        self.event_bytes = next_bytes;
        self.events.push(event);
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    pub const fn event_bytes(&self) -> usize {
        self.event_bytes
    }

    pub fn events(&self) -> &[PreparedDiagnosticEvent] {
        &self.events
    }
}

impl Default for DurableBatch {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for DurableBatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableBatch")
            .field("event_count", &self.event_count())
            .field("event_bytes", &self.event_bytes)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlushOutcome {
    Noop,
    Written,
    Rotated { count: usize, active_segment: u64 },
}

impl FlushOutcome {
    pub const fn rotation_count(self) -> usize {
        match self {
            Self::Rotated { count, .. } => count,
            Self::Noop | Self::Written => 0,
        }
    }

    pub const fn active_segment(self) -> Option<u64> {
        match self {
            Self::Rotated { active_segment, .. } => Some(active_segment),
            Self::Noop | Self::Written => None,
        }
    }
}

#[derive(Debug, Error)]
pub enum SegmentError {
    #[error("invalid diagnostic segment limit")]
    InvalidLimit,
    #[error("diagnostic event exceeds segment limit")]
    EventExceedsSegment,
    #[error("diagnostic segment contains invalid data")]
    InvalidData,
    #[error("diagnostic segment operation failed")]
    Io,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RecoveryReport {
    truncated_bytes: u64,
    active_segment: u64,
    last_sequence: u64,
    last_event_id: Option<EventId>,
}

impl RecoveryReport {
    pub const fn truncated_bytes(self) -> u64 {
        self.truncated_bytes
    }

    pub const fn active_segment(self) -> u64 {
        self.active_segment
    }

    pub const fn last_sequence(self) -> u64 {
        self.last_sequence
    }

    pub const fn last_event_id(self) -> Option<EventId> {
        self.last_event_id
    }
}

pub struct SegmentWriter {
    root: PathBuf,
    segment_limit: usize,
    directory: Option<DiagnosticDirectory>,
    active: Option<DiagnosticFile>,
    active_len: usize,
    last_sequence: u64,
    last_event_id: Option<EventId>,
    next_segment: u64,
    initialized: bool,
}

impl SegmentWriter {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            segment_limit: MAX_SEGMENT_BYTES,
            directory: None,
            active: None,
            active_len: 0,
            last_sequence: 0,
            last_event_id: None,
            next_segment: 1,
            initialized: false,
        }
    }

    pub fn with_segment_limit(
        root: impl AsRef<Path>,
        segment_limit: usize,
    ) -> Result<Self, SegmentError> {
        if segment_limit == 0 || segment_limit > MAX_SEGMENT_BYTES {
            return Err(SegmentError::InvalidLimit);
        }
        let mut writer = Self::new(root);
        writer.segment_limit = segment_limit;
        Ok(writer)
    }

    pub fn recover(&mut self) -> Result<RecoveryReport, SegmentError> {
        let directory = DiagnosticDirectory::open(&self.root).map_err(map_platform_error)?;
        let mut after = None::<OsString>;
        let maximum_size =
            u64::try_from(self.segment_limit).map_err(|_| SegmentError::InvalidData)?;
        let mut pending = None::<(u64, DiagnosticFile, SegmentScan)>;
        let mut immutable_last_sequence = 0_u64;
        let mut immutable_last_event_id = None;
        loop {
            let page = directory
                .entries_page(after.as_deref(), 128)
                .map_err(map_platform_error)?;
            let next = page.next_after().map(OsString::from);
            for entry in page.into_entries() {
                let Some(name) = entry.name().to_str() else {
                    continue;
                };
                let Some(index) = parse_segment_index(name) else {
                    continue;
                };
                let mut file = directory
                    .open_update(&entry, maximum_size)
                    .map_err(map_platform_error)?;
                let scan = scan_segment(&mut file)?;
                if let Some((_, previous, previous_scan)) = pending.take() {
                    if previous_scan.status != SegmentScanStatus::Complete {
                        return Err(SegmentError::InvalidData);
                    }
                    let previous_last = previous_scan
                        .last_sequence
                        .ok_or(SegmentError::InvalidData)?;
                    if scan
                        .first_sequence
                        .is_some_and(|first| first <= previous_last)
                    {
                        return Err(SegmentError::InvalidData);
                    }
                    immutable_last_sequence = previous_last;
                    immutable_last_event_id = previous_scan.last_event_id;
                    drop(previous);
                }
                pending = Some((index, file, scan));
            }
            let Some(next) = next else {
                break;
            };
            after = Some(next);
        }
        let (active_index, mut active, scan) = if let Some((index, active, scan)) = pending {
            (index, active, scan)
        } else {
            let active_index = 1;
            let active = directory
                .create_new(&segment_name(active_index), b"", maximum_size)
                .map_err(map_platform_error)?;
            (
                active_index,
                active,
                SegmentScan {
                    valid_len: 0,
                    first_sequence: None,
                    last_sequence: None,
                    last_event_id: None,
                    status: SegmentScanStatus::Complete,
                },
            )
        };
        let original_len = active.len();
        let valid_len = scan.valid_len;
        let truncated_bytes = if scan.status != SegmentScanStatus::Complete {
            original_len.saturating_sub(valid_len)
        } else {
            0
        };
        if truncated_bytes != 0 {
            active.truncate(valid_len).map_err(map_platform_error)?;
        }
        self.next_segment = active_index
            .checked_add(1)
            .ok_or(SegmentError::InvalidData)?;
        self.active_len = usize::try_from(active.len()).map_err(|_| SegmentError::InvalidData)?;
        self.last_sequence = scan.last_sequence.unwrap_or(immutable_last_sequence);
        self.last_event_id = scan.last_event_id.or(immutable_last_event_id);
        self.directory = Some(directory);
        self.active = Some(active);
        self.initialized = true;
        Ok(RecoveryReport {
            truncated_bytes,
            active_segment: active_index,
            last_sequence: self.last_sequence,
            last_event_id: self.last_event_id,
        })
    }

    pub fn flush(&mut self, batch: DurableBatch) -> Result<FlushOutcome, SegmentError> {
        if batch.is_empty() {
            return Ok(FlushOutcome::Noop);
        }
        if !self.initialized {
            self.recover()?;
        }
        let mut last_sequence = self.last_sequence;
        let mut last_event_id = self.last_event_id;
        for event in &batch.events {
            if event.sequence() <= last_sequence {
                return Err(SegmentError::InvalidData);
            }
            last_sequence = event.sequence();
            last_event_id = Some(event.event_id());
        }
        let mut pending = Vec::with_capacity(
            batch
                .event_bytes()
                .checked_add(batch.event_count())
                .ok_or(SegmentError::InvalidData)?,
        );
        let mut rotations = 0_usize;
        for event in batch.events {
            let line_len = event
                .encoded_len()
                .checked_add(1)
                .ok_or(SegmentError::InvalidData)?;
            if line_len > self.segment_limit {
                return Err(SegmentError::EventExceedsSegment);
            }
            let next_len = self
                .active_len
                .checked_add(pending.len())
                .and_then(|length| length.checked_add(line_len))
                .ok_or(SegmentError::InvalidData)?;
            if next_len > self.segment_limit {
                self.append(&pending)?;
                pending.clear();
                self.rotate()?;
                rotations = rotations.saturating_add(1);
            }
            pending.extend_from_slice(event.encoded());
            pending.push(b'\n');
        }
        self.append(&pending)?;
        self.last_sequence = last_sequence;
        self.last_event_id = last_event_id;
        if rotations == 0 {
            Ok(FlushOutcome::Written)
        } else {
            Ok(FlushOutcome::Rotated {
                count: rotations,
                active_segment: self.next_segment.saturating_sub(1),
            })
        }
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), SegmentError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let result = (|| {
            self.active
                .as_mut()
                .ok_or(SegmentError::InvalidData)?
                .append(bytes)
                .map_err(map_platform_error)?;
            self.active_len = self
                .active_len
                .checked_add(bytes.len())
                .ok_or(SegmentError::InvalidData)?;
            Ok(())
        })();
        if result.is_err() {
            self.reset_after_failure();
        }
        result
    }

    fn rotate(&mut self) -> Result<(), SegmentError> {
        if self.active_len == 0 {
            return Ok(());
        }
        let result = (|| {
            drop(self.active.take().ok_or(SegmentError::InvalidData)?);
            let maximum_size =
                u64::try_from(self.segment_limit).map_err(|_| SegmentError::InvalidData)?;
            let active = self
                .directory
                .as_ref()
                .ok_or(SegmentError::InvalidData)?
                .create_new(&segment_name(self.next_segment), b"", maximum_size)
                .map_err(map_platform_error)?;
            self.next_segment = self
                .next_segment
                .checked_add(1)
                .ok_or(SegmentError::InvalidData)?;
            self.active = Some(active);
            self.active_len = 0;
            Ok(())
        })();
        if result.is_err() {
            self.reset_after_failure();
        }
        result
    }

    fn reset_after_failure(&mut self) {
        self.active = None;
        self.directory = None;
        self.active_len = 0;
        self.last_sequence = 0;
        self.last_event_id = None;
        self.initialized = false;
    }
}

impl fmt::Debug for SegmentWriter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SegmentWriter([redacted])")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SegmentScanStatus {
    Complete,
    TornTail,
    Corrupt,
}

#[derive(Clone, Copy, Debug)]
struct SegmentScan {
    valid_len: u64,
    first_sequence: Option<u64>,
    last_sequence: Option<u64>,
    last_event_id: Option<EventId>,
    status: SegmentScanStatus,
}

fn scan_segment(file: &mut DiagnosticFile) -> Result<SegmentScan, SegmentError> {
    scan_segment_with_reader(
        file.len(),
        crate::event::MAX_PREPARED_EVENT_BYTES,
        crate::event::MAX_PREPARED_EVENT_BYTES,
        |offset, maximum_bytes| {
            file.read_range(offset, maximum_bytes)
                .map_err(map_platform_error)
        },
    )
}

fn scan_segment_with_reader<F>(
    file_len: u64,
    read_chunk_bytes: usize,
    maximum_line_bytes: usize,
    mut read_range: F,
) -> Result<SegmentScan, SegmentError>
where
    F: FnMut(u64, usize) -> Result<Vec<u8>, SegmentError>,
{
    if read_chunk_bytes == 0 || maximum_line_bytes == 0 {
        return Err(SegmentError::InvalidData);
    }
    let mut valid_len = 0_u64;
    let mut first_sequence = None;
    let mut last_sequence = None;
    let mut last_event_id = None;
    let mut read_offset = 0_u64;
    let mut line = Vec::with_capacity(maximum_line_bytes);
    let mut line_oversized = false;
    while read_offset < file_len {
        let remaining = usize::try_from(file_len.saturating_sub(read_offset)).unwrap_or(usize::MAX);
        let requested = read_chunk_bytes.min(remaining);
        let bytes = read_range(read_offset, requested)?;
        if bytes.is_empty() {
            return Err(SegmentError::InvalidData);
        }
        if bytes.len() > requested {
            return Err(SegmentError::InvalidData);
        }
        let chunk_start = read_offset;
        read_offset = read_offset
            .checked_add(u64::try_from(bytes.len()).map_err(|_| SegmentError::InvalidData)?)
            .ok_or(SegmentError::InvalidData)?;
        for (index, byte) in bytes.into_iter().enumerate() {
            if byte != b'\n' {
                if !line_oversized {
                    if line.len() == maximum_line_bytes {
                        line_oversized = true;
                    } else {
                        line.push(byte);
                    }
                }
                continue;
            }
            if line_oversized {
                return Ok(SegmentScan {
                    valid_len,
                    first_sequence,
                    last_sequence,
                    last_event_id,
                    status: SegmentScanStatus::Corrupt,
                });
            }
            let event = match crate::event::decode_trusted_prepared_encoding(&line) {
                Ok(event) => event,
                Err(_) => {
                    return Ok(SegmentScan {
                        valid_len,
                        first_sequence,
                        last_sequence,
                        last_event_id,
                        status: SegmentScanStatus::Corrupt,
                    });
                }
            };
            if last_sequence.is_some_and(|last| event.sequence() <= last) {
                return Ok(SegmentScan {
                    valid_len,
                    first_sequence,
                    last_sequence,
                    last_event_id,
                    status: SegmentScanStatus::Corrupt,
                });
            }
            first_sequence.get_or_insert(event.sequence());
            last_sequence = Some(event.sequence());
            last_event_id = Some(event.event_id());
            valid_len = chunk_start
                .checked_add(
                    u64::try_from(index.saturating_add(1))
                        .map_err(|_| SegmentError::InvalidData)?,
                )
                .ok_or(SegmentError::InvalidData)?;
            line.clear();
        }
    }
    if !line.is_empty() || line_oversized {
        return Ok(SegmentScan {
            valid_len,
            first_sequence,
            last_sequence,
            last_event_id,
            status: if valid_len == 0 {
                SegmentScanStatus::Corrupt
            } else {
                SegmentScanStatus::TornTail
            },
        });
    }
    Ok(SegmentScan {
        valid_len,
        first_sequence,
        last_sequence,
        last_event_id,
        status: SegmentScanStatus::Complete,
    })
}

pub(crate) fn validate_complete_segment_bytes(bytes: &[u8]) -> Option<(u64, u64)> {
    if bytes.is_empty() || bytes.len() > MAX_SEGMENT_BYTES || !bytes.ends_with(b"\n") {
        return None;
    }
    let mut first = None;
    let mut last = None;
    for line in bytes[..bytes.len() - 1].split(|byte| *byte == b'\n') {
        if line.is_empty() {
            return None;
        }
        let event = crate::event::decode_trusted_prepared_encoding(line).ok()?;
        if last.is_some_and(|previous| event.sequence() <= previous) {
            return None;
        }
        first.get_or_insert(event.sequence());
        last = Some(event.sequence());
    }
    Some((first?, last?))
}

fn segment_name(index: u64) -> OsString {
    OsString::from(format!("segment-{index:020}.jsonl"))
}

fn parse_segment_index(name: &str) -> Option<u64> {
    let value = name.strip_prefix("segment-")?.strip_suffix(".jsonl")?;
    if value.len() != 20 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok().filter(|index| *index != 0)
}

fn map_platform_error(error: DiagnosticStoreError) -> SegmentError {
    match error {
        DiagnosticStoreError::UnsafePath
        | DiagnosticStoreError::Changed
        | DiagnosticStoreError::Unavailable
        | DiagnosticStoreError::SizeLimitExceeded => SegmentError::InvalidData,
        _ => SegmentError::Io,
    }
}

#[cfg(test)]
mod scan_tests {
    use super::{SegmentError, SegmentScanStatus, scan_segment_with_reader};

    fn canonical_line(sequence: u64) -> Vec<u8> {
        let encoded = format!(
            concat!(
                "{{\"schema_version\":1,\"sequence\":\"{0:020}\",",
                "\"event_id\":\"018f47a2-4c1d-7a8f-9b2d-{0:012x}\",",
                "\"occurred_at\":\"2026-07-26T12:30:00Z\",\"level\":\"info\",",
                "\"component\":\"diagnostics\",\"code\":\"request_completed\",",
                "\"correlations\":null,\"build\":{{\"wokcore_version\":\"0.1.0\",",
                "\"git_commit\":\"0123456789abcdef0123456789abcdef01234567\",",
                "\"api_major\":1,\"capability_version\":3}},\"provider\":null,",
                "\"decision\":null,\"measurements\":null,\"error\":null,",
                "\"diagnostic_drop\":null,\"summaries\":[],\"redaction_counts\":{{",
                "\"authorization_values_removed\":0,\"cookie_values_removed\":0,",
                "\"body_values_removed\":0,\"path_values_removed\":0,",
                "\"token_values_removed\":0,\"credential_values_removed\":0}}}}"
            ),
            sequence
        )
        .into_bytes();
        crate::event::decode_trusted_prepared_encoding(&encoded).unwrap();
        encoded
    }

    #[test]
    fn four_mib_typical_segment_is_read_once_in_forward_chunks() {
        const READ_CHUNK_BYTES: usize = 16 * 1024;

        let mut source = Vec::with_capacity(super::MAX_SEGMENT_BYTES);
        let mut sequence = 1_u64;
        loop {
            let line = canonical_line(sequence);
            if source
                .len()
                .checked_add(line.len() + 1)
                .is_none_or(|next| next > super::MAX_SEGMENT_BYTES)
            {
                break;
            }
            source.extend_from_slice(&line);
            source.push(b'\n');
            sequence += 1;
        }
        assert!(source.len() > super::MAX_SEGMENT_BYTES - crate::event::MAX_PREPARED_EVENT_BYTES);

        let mut calls = 0_usize;
        let mut returned_bytes = 0_usize;
        let scan = scan_segment_with_reader(
            u64::try_from(source.len()).unwrap(),
            READ_CHUNK_BYTES,
            crate::event::MAX_PREPARED_EVENT_BYTES,
            |offset, maximum_bytes| {
                calls += 1;
                let start = usize::try_from(offset).map_err(|_| SegmentError::InvalidData)?;
                let end = start.saturating_add(maximum_bytes).min(source.len());
                returned_bytes += end.saturating_sub(start);
                Ok(source[start..end].to_vec())
            },
        )
        .unwrap();

        assert_eq!(scan.status, SegmentScanStatus::Complete);
        assert_eq!(scan.last_sequence, Some(sequence - 1));
        assert_eq!(returned_bytes, source.len());
        assert_eq!(calls, source.len().div_ceil(READ_CHUNK_BYTES));
    }

    #[test]
    fn exact_maximum_line_remains_valid_when_it_crosses_a_chunk() {
        let line = canonical_line(1);
        let mut source = line.clone();
        source.push(b'\n');
        let chunk_bytes = line.len() - 1;

        let scan = scan_segment_with_reader(
            u64::try_from(source.len()).unwrap(),
            chunk_bytes,
            line.len(),
            |offset, maximum_bytes| {
                let start = usize::try_from(offset).map_err(|_| SegmentError::InvalidData)?;
                let end = start.saturating_add(maximum_bytes).min(source.len());
                Ok(source[start..end].to_vec())
            },
        )
        .unwrap();

        assert_eq!(scan.status, SegmentScanStatus::Complete);
        assert_eq!(scan.valid_len, u64::try_from(source.len()).unwrap());
        assert_eq!(scan.first_sequence, Some(1));
        assert_eq!(scan.last_sequence, Some(1));
    }
}
