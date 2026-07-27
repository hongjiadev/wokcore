use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::sync::{Notify, mpsc, oneshot};

use crate::{
    event::{DiagnosticBuildError, DiagnosticEventDraft, DiagnosticEventTemplate},
    ring::{DiagnosticRing, PageRequest, RingInsertOutcome, RingPage},
    segment::{DurableProducer, DurableRecordOutcome, RecoveryReport},
};

pub const INGRESS_QUEUE_CAPACITY: usize = 256;
const INTERNAL_INGRESS_QUEUE_CAPACITY: usize = 4;
pub const QUERY_QUEUE_CAPACITY: usize = 32;
pub const MAX_QUERY_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordOutcome {
    Accepted,
    DroppedFull,
    DroppedClosed,
    DroppedInvalid,
    DroppedOversized,
}

#[derive(Clone)]
pub struct DropMetrics {
    inner: Arc<DropMetricAtoms>,
}

struct DropMetricAtoms {
    ingress_full: AtomicU64,
    ingress_closed: AtomicU64,
    invalid: AtomicU64,
    oversized: AtomicU64,
    durable_full: AtomicU64,
    durable_closed: AtomicU64,
    query_busy: AtomicU64,
    query_closed: AtomicU64,
}

impl DropMetrics {
    fn new() -> Self {
        Self {
            inner: Arc::new(DropMetricAtoms {
                ingress_full: AtomicU64::new(0),
                ingress_closed: AtomicU64::new(0),
                invalid: AtomicU64::new(0),
                oversized: AtomicU64::new(0),
                durable_full: AtomicU64::new(0),
                durable_closed: AtomicU64::new(0),
                query_busy: AtomicU64::new(0),
                query_closed: AtomicU64::new(0),
            }),
        }
    }

    pub fn ingress_full(&self) -> u64 {
        self.inner.ingress_full.load(Ordering::Relaxed)
    }

    pub fn ingress_closed(&self) -> u64 {
        self.inner.ingress_closed.load(Ordering::Relaxed)
    }

    pub fn invalid(&self) -> u64 {
        self.inner.invalid.load(Ordering::Relaxed)
    }

    pub fn oversized(&self) -> u64 {
        self.inner.oversized.load(Ordering::Relaxed)
    }

    pub fn invalid_or_oversized(&self) -> u64 {
        self.invalid().saturating_add(self.oversized())
    }

    pub fn durable_full(&self) -> u64 {
        self.inner.durable_full.load(Ordering::Relaxed)
    }

    pub fn durable_closed(&self) -> u64 {
        self.inner.durable_closed.load(Ordering::Relaxed)
    }

    pub fn query_busy(&self) -> u64 {
        self.inner.query_busy.load(Ordering::Relaxed)
    }

    pub fn query_closed(&self) -> u64 {
        self.inner.query_closed.load(Ordering::Relaxed)
    }

    pub fn total_dropped(&self) -> u64 {
        [
            self.ingress_full(),
            self.ingress_closed(),
            self.invalid(),
            self.oversized(),
            self.durable_full(),
            self.durable_closed(),
            self.query_busy(),
            self.query_closed(),
        ]
        .into_iter()
        .fold(0u64, u64::saturating_add)
    }
}

impl fmt::Debug for DropMetrics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DropMetrics")
            .field("ingress_full", &self.ingress_full())
            .field("ingress_closed", &self.ingress_closed())
            .field("invalid", &self.invalid())
            .field("oversized", &self.oversized())
            .field("durable_full", &self.durable_full())
            .field("durable_closed", &self.durable_closed())
            .field("query_busy", &self.query_busy())
            .field("query_closed", &self.query_closed())
            .finish()
    }
}

fn increment(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(1))
    });
}

#[derive(Clone)]
pub struct DiagnosticRecorder {
    ingress: mpsc::Sender<DiagnosticEventTemplate>,
    internal_ingress: mpsc::Sender<DiagnosticEventTemplate>,
    queries: mpsc::Sender<RecorderCommand>,
    metrics: DropMetrics,
    remaining_sequences: Arc<AtomicU64>,
    shutdown: Arc<RecorderShutdownState>,
}

impl DiagnosticRecorder {
    pub fn new() -> (Self, RecorderOwner) {
        Self::with_ring(DiagnosticRing::new(), 1)
    }

    pub fn with_ring_byte_budget(
        byte_budget: usize,
    ) -> Result<(Self, RecorderOwner), DiagnosticBuildError> {
        Ok(Self::with_ring(
            DiagnosticRing::with_byte_budget(byte_budget)?,
            1,
        ))
    }

    fn with_ring(ring: DiagnosticRing, next_sequence: u64) -> (Self, RecorderOwner) {
        let (ingress, ingress_receiver) = mpsc::channel(INGRESS_QUEUE_CAPACITY);
        let (internal_ingress, internal_ingress_receiver) =
            mpsc::channel(INTERNAL_INGRESS_QUEUE_CAPACITY);
        let (queries, query_receiver) = mpsc::channel(QUERY_QUEUE_CAPACITY);
        let metrics = DropMetrics::new();
        let shutdown = Arc::new(RecorderShutdownState::default());
        let remaining = u64::MAX.saturating_sub(next_sequence).saturating_add(1);
        let remaining_sequences = Arc::new(AtomicU64::new(remaining));
        (
            Self {
                ingress,
                internal_ingress,
                queries,
                metrics: metrics.clone(),
                remaining_sequences: Arc::clone(&remaining_sequences),
                shutdown: Arc::clone(&shutdown),
            },
            RecorderOwner {
                ring,
                ingress: ingress_receiver,
                internal_ingress: internal_ingress_receiver,
                queries: query_receiver,
                metrics,
                next_sequence,
                remaining_sequences: Arc::clone(&remaining_sequences),
                durable: None,
                shutdown,
            },
        )
    }

    fn reserve_sequence(&self) -> Result<(), DiagnosticBuildError> {
        self.remaining_sequences
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| match value {
                0 => None,
                _ => Some(value - 1),
            })
            .map(|_| ())
            .map_err(|_| DiagnosticBuildError::CollectionLimit)
    }

    pub fn try_record(
        &self,
        draft: Result<DiagnosticEventDraft, DiagnosticBuildError>,
    ) -> RecordOutcome {
        self.try_record_with_admission(draft, false)
    }

    pub fn try_record_internal(
        &self,
        draft: Result<DiagnosticEventDraft, DiagnosticBuildError>,
    ) -> RecordOutcome {
        self.try_record_with_admission(draft, true)
    }

    fn try_record_with_admission(
        &self,
        draft: Result<DiagnosticEventDraft, DiagnosticBuildError>,
        internal: bool,
    ) -> RecordOutcome {
        if self.shutdown.requested.load(Ordering::Acquire)
            || (!internal && self.shutdown.ingress_closed.load(Ordering::Acquire))
        {
            increment(&self.metrics.inner.ingress_closed);
            return RecordOutcome::DroppedClosed;
        }
        let draft = match draft {
            Ok(draft) => draft,
            Err(_) => {
                increment(&self.metrics.inner.invalid);
                return RecordOutcome::DroppedInvalid;
            }
        };
        let ingress = if internal {
            &self.internal_ingress
        } else {
            &self.ingress
        };
        let permit = match ingress.try_reserve() {
            Ok(permit) => permit,
            Err(mpsc::error::TrySendError::Full(())) => {
                increment(&self.metrics.inner.ingress_full);
                return RecordOutcome::DroppedFull;
            }
            Err(mpsc::error::TrySendError::Closed(())) => {
                increment(&self.metrics.inner.ingress_closed);
                return RecordOutcome::DroppedClosed;
            }
        };
        let event = match draft.prepare_template() {
            Ok(event) => event,
            Err(DiagnosticBuildError::EventTooLarge) => {
                increment(&self.metrics.inner.oversized);
                return RecordOutcome::DroppedOversized;
            }
            Err(_) => {
                increment(&self.metrics.inner.invalid);
                return RecordOutcome::DroppedInvalid;
            }
        };
        if self.reserve_sequence().is_err() {
            increment(&self.metrics.inner.invalid);
            return RecordOutcome::DroppedInvalid;
        }
        permit.send(event);
        RecordOutcome::Accepted
    }

    pub fn try_query(&self, request: PageRequest) -> Result<PendingQuery, QueryAdmissionError> {
        if self.shutdown.requested.load(Ordering::Acquire) {
            increment(&self.metrics.inner.query_closed);
            return Err(QueryAdmissionError::Closed);
        }
        let (reply, receiver) = oneshot::channel();
        match self
            .queries
            .try_send(RecorderCommand::Query(QueryCommand { request, reply }))
        {
            Ok(()) => Ok(PendingQuery { receiver }),
            Err(mpsc::error::TrySendError::Full(_)) => {
                increment(&self.metrics.inner.query_busy);
                Err(QueryAdmissionError::Busy)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                increment(&self.metrics.inner.query_closed);
                Err(QueryAdmissionError::Closed)
            }
        }
    }

    pub fn try_barrier(&self) -> Result<PendingBarrier, BarrierAdmissionError> {
        if self.shutdown.requested.load(Ordering::Acquire) {
            return Err(BarrierAdmissionError::Closed);
        }
        let (reply, receiver) = oneshot::channel();
        match self.queries.try_send(RecorderCommand::Barrier { reply }) {
            Ok(()) => Ok(PendingBarrier { receiver }),
            Err(mpsc::error::TrySendError::Full(_)) => Err(BarrierAdmissionError::Busy),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(BarrierAdmissionError::Closed),
        }
    }

    pub fn try_close_ingress_barrier(&self) -> Result<PendingBarrier, BarrierAdmissionError> {
        if self.shutdown.requested.load(Ordering::Acquire) {
            return Err(BarrierAdmissionError::Closed);
        }
        self.shutdown.ingress_closed.store(true, Ordering::Release);
        let (reply, receiver) = oneshot::channel();
        match self
            .queries
            .try_send(RecorderCommand::CloseIngress { reply })
        {
            Ok(()) => Ok(PendingBarrier { receiver }),
            Err(mpsc::error::TrySendError::Full(_)) => Err(BarrierAdmissionError::Busy),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(BarrierAdmissionError::Closed),
        }
    }

    pub fn metrics(&self) -> DropMetrics {
        self.metrics.clone()
    }
}

impl fmt::Debug for DiagnosticRecorder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DiagnosticRecorder([redacted])")
    }
}

struct QueryCommand {
    request: PageRequest,
    reply: oneshot::Sender<RingPage>,
}

enum RecorderCommand {
    Query(QueryCommand),
    Barrier { reply: oneshot::Sender<()> },
    CloseIngress { reply: oneshot::Sender<()> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum QueryAdmissionError {
    #[error("diagnostic query is busy")]
    Busy,
    #[error("diagnostic query owner is unavailable")]
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum QueryReplyError {
    #[error("diagnostic query deadline elapsed")]
    Deadline,
    #[error("diagnostic query owner is unavailable")]
    Closed,
}

pub struct PendingQuery {
    receiver: oneshot::Receiver<RingPage>,
}

impl PendingQuery {
    pub async fn wait(self) -> Result<RingPage, QueryReplyError> {
        self.wait_with_deadline(MAX_QUERY_DEADLINE).await
    }

    pub async fn wait_with_deadline(self, deadline: Duration) -> Result<RingPage, QueryReplyError> {
        let bounded = deadline.min(MAX_QUERY_DEADLINE);
        match tokio::time::timeout(bounded, self.receiver).await {
            Ok(Ok(page)) => Ok(page),
            Ok(Err(_)) => Err(QueryReplyError::Closed),
            Err(_) => Err(QueryReplyError::Deadline),
        }
    }
}

impl fmt::Debug for PendingQuery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PendingQuery([redacted])")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum BarrierAdmissionError {
    #[error("diagnostic recorder barrier is busy")]
    Busy,
    #[error("diagnostic recorder owner is unavailable")]
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum BarrierReplyError {
    #[error("diagnostic recorder barrier deadline elapsed")]
    Deadline,
    #[error("diagnostic recorder owner is unavailable")]
    Closed,
}

pub struct PendingBarrier {
    receiver: oneshot::Receiver<()>,
}

impl PendingBarrier {
    pub async fn wait(self) -> Result<(), BarrierReplyError> {
        self.wait_with_deadline(MAX_QUERY_DEADLINE).await
    }

    pub async fn wait_with_deadline(self, deadline: Duration) -> Result<(), BarrierReplyError> {
        let bounded = deadline.min(MAX_QUERY_DEADLINE);
        match tokio::time::timeout(bounded, self.receiver).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(BarrierReplyError::Closed),
            Err(_) => Err(BarrierReplyError::Deadline),
        }
    }
}

impl fmt::Debug for PendingBarrier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PendingBarrier([redacted])")
    }
}

pub struct RecorderOwner {
    ring: DiagnosticRing,
    ingress: mpsc::Receiver<DiagnosticEventTemplate>,
    internal_ingress: mpsc::Receiver<DiagnosticEventTemplate>,
    queries: mpsc::Receiver<RecorderCommand>,
    metrics: DropMetrics,
    next_sequence: u64,
    remaining_sequences: Arc<AtomicU64>,
    durable: Option<DurableProducer>,
    shutdown: Arc<RecorderShutdownState>,
}

#[derive(Default)]
struct RecorderShutdownState {
    ingress_closed: AtomicBool,
    requested: AtomicBool,
    wake: Notify,
}

#[derive(Clone)]
pub struct RecorderShutdown {
    state: Arc<RecorderShutdownState>,
}

impl RecorderShutdown {
    pub fn close_ingress(&self) {
        self.state.ingress_closed.store(true, Ordering::Release);
    }

    pub fn request(&self) {
        self.close_ingress();
        self.state.requested.store(true, Ordering::Release);
        self.state.wake.notify_one();
    }
}

impl fmt::Debug for RecorderShutdown {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RecorderShutdown([redacted])")
    }
}

impl RecorderOwner {
    pub fn shutdown_handle(&self) -> RecorderShutdown {
        RecorderShutdown {
            state: Arc::clone(&self.shutdown),
        }
    }

    pub fn with_durable_producer(mut self, producer: DurableProducer) -> Self {
        self.durable = Some(producer);
        self
    }

    pub fn with_recovered_durable_producer(
        mut self,
        producer: DurableProducer,
        recovery: RecoveryReport,
    ) -> Result<Self, DiagnosticBuildError> {
        self.elevate_next_sequence(recovery.last_sequence())?;
        self.durable = Some(producer);
        Ok(self)
    }

    fn elevate_next_sequence(
        &mut self,
        recovered_last_sequence: u64,
    ) -> Result<(), DiagnosticBuildError> {
        let recovered_next = recovered_last_sequence.checked_add(1).unwrap_or(0);
        let current = self.next_sequence;
        if current == 0
            || recovered_next == current
            || (recovered_next != 0 && recovered_next < current)
        {
            return Ok(());
        }
        let skipped = if recovered_next == 0 {
            u64::MAX.saturating_sub(current).saturating_add(1)
        } else {
            recovered_next.saturating_sub(current)
        };
        self.remaining_sequences
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(skipped)
            })
            .map_err(|_| DiagnosticBuildError::CollectionLimit)?;
        self.next_sequence = recovered_next;
        Ok(())
    }

    pub async fn run(mut self) {
        let mut ingress_open = true;
        let mut internal_ingress_open = true;
        let mut queries_open = true;
        let mut stopping = false;
        let mut close_ingress_replies: Vec<oneshot::Sender<()>> = Vec::new();
        while ingress_open || internal_ingress_open || queries_open {
            if !stopping && self.shutdown.requested.load(Ordering::Acquire) {
                self.ingress.close();
                self.internal_ingress.close();
                self.queries.close();
                stopping = true;
            }
            tokio::select! {
                biased;
                event = self.ingress.recv(), if ingress_open => {
                    match event {
                        Some(event) => self.finalize_and_insert(event),
                        None => {
                            ingress_open = false;
                            for reply in close_ingress_replies.drain(..) {
                                let _ = reply.send(());
                            }
                        }
                    }
                }
                event = self.internal_ingress.recv(), if internal_ingress_open => {
                    match event {
                        Some(event) => self.finalize_and_insert(event),
                        None => internal_ingress_open = false,
                    }
                }
                query = self.queries.recv(), if queries_open => {
                    match query {
                        Some(RecorderCommand::Query(query)) => {
                            let page = self.ring.page(query.request);
                            let _ = query.reply.send(page);
                        }
                        Some(RecorderCommand::Barrier { reply }) => {
                            let _ = reply.send(());
                        }
                        Some(RecorderCommand::CloseIngress { reply }) => {
                            self.ingress.close();
                            if ingress_open {
                                close_ingress_replies.push(reply);
                            } else {
                                let _ = reply.send(());
                            }
                        }
                        None => queries_open = false,
                    }
                }
                () = self.shutdown.wake.notified(), if !stopping => {}
            }
        }
    }

    fn finalize_and_insert(&mut self, template: DiagnosticEventTemplate) {
        let sequence = self.next_sequence;
        self.next_sequence = match sequence {
            0 | u64::MAX => 0,
            _ => sequence + 1,
        };
        let event = match template.finalize(sequence) {
            Ok(event) => event,
            Err(_) => {
                increment(&self.metrics.inner.invalid);
                return;
            }
        };
        let durable_event = event.clone();
        match self.ring.insert(event) {
            RingInsertOutcome::Inserted => {
                if let Some(durable) = &self.durable {
                    match durable.try_record(durable_event) {
                        DurableRecordOutcome::DroppedFull => {
                            increment(&self.metrics.inner.durable_full);
                        }
                        DurableRecordOutcome::DroppedClosed => {
                            increment(&self.metrics.inner.durable_closed);
                        }
                        DurableRecordOutcome::Accepted | DurableRecordOutcome::Filtered => {}
                    }
                }
            }
            RingInsertOutcome::Oversized => increment(&self.metrics.inner.oversized),
            RingInsertOutcome::OutOfOrder => increment(&self.metrics.inner.invalid),
        }
    }
}

impl fmt::Debug for RecorderOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RecorderOwner([redacted])")
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{Arc, Barrier},
    };

    use super::*;
    use crate::{
        event::{
            BuildIdentity, CapabilityVersion, DiagnosticComponent, DiagnosticDropCounts,
            DiagnosticEventCode, DiagnosticLevel, EventId, GitCommit, UtcTimestamp, WokcoreVersion,
        },
        ring::{MAX_PAGE_BYTES, PageDirection},
        segment::{DurableProcessOutcome, DurableProducer},
    };
    use tempfile::tempdir;

    fn draft(identity: u64) -> Result<DiagnosticEventDraft, DiagnosticBuildError> {
        Ok(DiagnosticEventDraft::new(
            EventId::parse(&format!("018f47a2-4c1d-7a8f-9b2d-{identity:012x}"))?,
            UtcTimestamp::parse("2026-07-26T12:30:00Z")?,
            DiagnosticLevel::Info,
            DiagnosticComponent::Diagnostics,
            DiagnosticEventCode::RequestCompleted,
            BuildIdentity::new(
                WokcoreVersion::parse("0.1.0")?,
                GitCommit::parse("0123456789abcdef0123456789abcdef01234567")?,
                1,
                CapabilityVersion::new(3),
            ),
        ))
    }

    fn durable_draft(identity: u64) -> Result<DiagnosticEventDraft, DiagnosticBuildError> {
        Ok(DiagnosticEventDraft::new(
            EventId::parse(&format!("018f47a2-4c1d-7a8f-9b2d-{identity:012x}"))?,
            UtcTimestamp::parse("2026-07-26T12:30:00Z")?,
            DiagnosticLevel::Warn,
            DiagnosticComponent::Diagnostics,
            DiagnosticEventCode::RequestFailed,
            BuildIdentity::new(
                WokcoreVersion::parse("0.1.0")?,
                GitCommit::parse("0123456789abcdef0123456789abcdef01234567")?,
                1,
                CapabilityVersion::new(3),
            ),
        ))
    }

    fn drop_draft(
        identity: u64,
        summary: crate::segment::DiagnosticDropSummary,
    ) -> Result<DiagnosticEventDraft, DiagnosticBuildError> {
        Ok(DiagnosticEventDraft::new(
            EventId::parse(&format!("018f47a2-4c1d-7a8f-9b2d-{identity:012x}"))?,
            UtcTimestamp::parse("2026-07-26T12:30:00Z")?,
            DiagnosticLevel::Warn,
            DiagnosticComponent::Diagnostics,
            DiagnosticEventCode::DiagnosticDrop,
            BuildIdentity::new(
                WokcoreVersion::parse("0.1.0")?,
                GitCommit::parse("0123456789abcdef0123456789abcdef01234567")?,
                1,
                CapabilityVersion::new(3),
            ),
        )
        .with_diagnostic_drop_counts(DiagnosticDropCounts::new(
            summary.ingress_full(),
            summary.ingress_closed(),
            summary.writer_unavailable(),
            summary.invalid_event(),
            summary.oversized_event(),
        )))
    }

    fn seed_recovered_sequence(root: &std::path::Path, sequence: u64) {
        let event = durable_draft(1)
            .unwrap()
            .prepare_template()
            .unwrap()
            .finalize(sequence)
            .unwrap();
        let mut bytes = event.encoded().to_vec();
        bytes.push(b'\n');
        fs::write(root.join("segment-00000000000000000001.jsonl"), bytes).unwrap();
    }

    #[test]
    fn atomic_drop_metrics_saturate_and_concurrent_reads_never_clear() {
        let metrics = DropMetrics::new();
        metrics
            .inner
            .ingress_full
            .store(u64::MAX - 1, Ordering::Relaxed);
        let barrier = Arc::new(Barrier::new(5));
        let mut readers = Vec::new();
        for _ in 0..4 {
            let metrics = metrics.clone();
            let barrier = Arc::clone(&barrier);
            readers.push(std::thread::spawn(move || {
                barrier.wait();
                let mut maximum = 0;
                for _ in 0..1_000 {
                    maximum = maximum.max(metrics.ingress_full());
                }
                maximum
            }));
        }
        barrier.wait();
        increment(&metrics.inner.ingress_full);
        increment(&metrics.inner.ingress_full);
        increment(&metrics.inner.ingress_full);
        for reader in readers {
            assert!(reader.join().unwrap() >= u64::MAX - 1);
        }
        assert_eq!(metrics.ingress_full(), u64::MAX);
        assert_eq!(metrics.ingress_full(), u64::MAX);

        for counter in [
            &metrics.inner.ingress_closed,
            &metrics.inner.invalid,
            &metrics.inner.oversized,
            &metrics.inner.query_busy,
            &metrics.inner.query_closed,
        ] {
            counter.store(u64::MAX, Ordering::Relaxed);
        }
        assert_eq!(metrics.total_dropped(), u64::MAX);
    }

    #[tokio::test]
    async fn owner_sequence_max_minus_one_and_max_saturate_without_wrap() {
        let (recorder, owner) = DiagnosticRecorder::with_ring(DiagnosticRing::new(), u64::MAX - 1);
        assert_eq!(recorder.try_record(draft(1)), RecordOutcome::Accepted);
        assert_eq!(recorder.try_record(draft(2)), RecordOutcome::Accepted);
        assert_eq!(recorder.try_record(draft(3)), RecordOutcome::DroppedInvalid);
        let pending = recorder
            .try_query(
                PageRequest::with_limits(PageDirection::Ascending, None, 10, MAX_PAGE_BYTES)
                    .unwrap(),
            )
            .unwrap();
        let owner_task = tokio::spawn(owner.run());
        let page = pending.wait().await.unwrap();
        owner_task.abort();
        assert_eq!(
            page.events()
                .iter()
                .map(|event| event.sequence())
                .collect::<Vec<_>>(),
            [u64::MAX - 1, u64::MAX]
        );
        assert!(page.events().iter().all(|event| event.sequence() != 0));
        assert_eq!(recorder.metrics().invalid(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn recorder_owner_forwards_in_global_sequence_without_blocking_ring() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("durable");
        fs::create_dir(&root).unwrap();
        let (producer, mut durable_owner) = DurableProducer::new(&root, |_| Ok::<_, ()>(()));
        let (recorder, owner) = DiagnosticRecorder::new();
        let owner_task = tokio::spawn(owner.with_durable_producer(producer).run());

        assert_eq!(recorder.try_record(draft(1)), RecordOutcome::Accepted);
        let mut workers = Vec::new();
        for worker in 0..4_u64 {
            let recorder = recorder.clone();
            workers.push(std::thread::spawn(move || {
                for offset in 0..50_u64 {
                    let identity = 10 + worker * 50 + offset;
                    loop {
                        match recorder.try_record(durable_draft(identity)) {
                            RecordOutcome::Accepted => break,
                            RecordOutcome::DroppedFull => std::thread::yield_now(),
                            outcome => panic!("unexpected record outcome: {outcome:?}"),
                        }
                    }
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        let page = recorder
            .try_query(
                PageRequest::with_limits(PageDirection::Ascending, None, 1_000, MAX_PAGE_BYTES)
                    .unwrap(),
            )
            .unwrap()
            .wait()
            .await
            .unwrap();
        assert_eq!(page.events().len(), 201);

        loop {
            match durable_owner.try_process_next().unwrap() {
                DurableProcessOutcome::Idle | DurableProcessOutcome::DropSummaryRequested => break,
                DurableProcessOutcome::Written { .. } => {}
            }
        }
        let mut files = fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        files.sort();
        let mut sequences = Vec::new();
        for file in files {
            for line in fs::read(file)
                .unwrap()
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
            {
                let event = crate::event::DiagnosticEvent::decode(line).unwrap();
                assert_ne!(event.code(), DiagnosticEventCode::RequestCompleted);
                sequences.push(event.sequence());
            }
        }
        assert_eq!(sequences.len(), 200);
        assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
        owner_task.abort();
    }

    #[tokio::test]
    async fn explicit_shutdown_drains_accepted_events_while_senders_remain_alive() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("graceful-recorder-shutdown");
        fs::create_dir(&root).unwrap();
        let (producer, mut durable_owner) = DurableProducer::new(&root, |_| Ok::<_, ()>(()));
        let (recorder, owner) = DiagnosticRecorder::new();
        let shutdown = owner.shutdown_handle();

        for identity in 1..=16 {
            assert_eq!(
                recorder.try_record(durable_draft(identity)),
                RecordOutcome::Accepted
            );
        }
        let owner_task = tokio::spawn(owner.with_durable_producer(producer).run());
        shutdown.request();

        tokio::time::timeout(Duration::from_secs(1), owner_task)
            .await
            .expect("recorder owner did not stop while producer handles remained")
            .unwrap();
        assert_eq!(
            recorder.try_record(durable_draft(17)),
            RecordOutcome::DroppedClosed
        );

        let mut written = 0_usize;
        loop {
            match durable_owner.try_process_next().unwrap() {
                DurableProcessOutcome::Idle => break,
                DurableProcessOutcome::Written { events, .. } => {
                    written = written.saturating_add(events);
                }
                DurableProcessOutcome::DropSummaryRequested => {
                    panic!("graceful shutdown unexpectedly requested a drop summary")
                }
            }
        }
        assert_eq!(written, 16);
    }

    #[tokio::test]
    async fn explicit_shutdown_rejects_new_work_before_the_owner_is_polled() {
        let (recorder, owner) = DiagnosticRecorder::new();
        let shutdown = owner.shutdown_handle();

        shutdown.request();

        assert_eq!(
            recorder.try_record(durable_draft(1)),
            RecordOutcome::DroppedClosed
        );
        assert!(matches!(
            recorder.try_query(
                PageRequest::with_limits(PageDirection::Ascending, None, 1, MAX_PAGE_BYTES,)
                    .unwrap(),
            ),
            Err(QueryAdmissionError::Closed)
        ));
        assert!(matches!(
            recorder.try_barrier(),
            Err(BarrierAdmissionError::Closed)
        ));
        tokio::time::timeout(Duration::from_secs(1), owner.run())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn ingress_can_close_while_internal_drop_events_and_barriers_still_drain() {
        let (recorder, owner) = DiagnosticRecorder::new();
        let shutdown = owner.shutdown_handle();
        let owner_task = tokio::spawn(owner.run());

        shutdown.close_ingress();
        assert_eq!(
            recorder.try_record(durable_draft(1)),
            RecordOutcome::DroppedClosed
        );
        assert_eq!(
            recorder.try_record_internal(durable_draft(2)),
            RecordOutcome::Accepted
        );
        recorder.try_barrier().unwrap().wait().await.unwrap();

        shutdown.request();
        tokio::time::timeout(Duration::from_secs(1), owner_task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn barrier_waits_for_prior_ingress_and_is_closed_after_shutdown() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("recorder-barrier");
        fs::create_dir(&root).unwrap();
        let (producer, mut durable_owner) = DurableProducer::new(&root, |_| Ok::<_, ()>(()));
        let (recorder, owner) = DiagnosticRecorder::new();
        let shutdown = owner.shutdown_handle();
        let owner_task = tokio::spawn(owner.with_durable_producer(producer).run());

        for identity in 1..=16 {
            assert_eq!(
                recorder.try_record(durable_draft(identity)),
                RecordOutcome::Accepted
            );
        }
        recorder.try_barrier().unwrap().wait().await.unwrap();
        assert!(matches!(
            durable_owner.try_process_next().unwrap(),
            DurableProcessOutcome::Written { events: 16, .. }
        ));

        shutdown.request();
        assert!(matches!(
            recorder.try_barrier(),
            Err(BarrierAdmissionError::Closed)
        ));
        tokio::time::timeout(Duration::from_secs(1), owner_task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn recovered_durable_owner_seeds_the_first_restarted_sequence() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("restart-sequence");
        fs::create_dir(&root).unwrap();

        let (recorder, owner) = DiagnosticRecorder::new();
        let drop_recorder = recorder.clone();
        let (producer, mut durable_owner) = DurableProducer::new(&root, move |summary| {
            match drop_recorder.try_record(drop_draft(9_000, summary)) {
                RecordOutcome::Accepted => Ok(()),
                _ => Err(()),
            }
        });
        let recovery = durable_owner
            .recover_startup(std::time::SystemTime::now())
            .unwrap();
        let owner_task = tokio::spawn(
            owner
                .with_recovered_durable_producer(producer, recovery)
                .unwrap()
                .run(),
        );
        for identity in 1..=3 {
            assert_eq!(
                recorder.try_record(durable_draft(identity)),
                RecordOutcome::Accepted
            );
        }
        recorder
            .try_query(
                PageRequest::with_limits(PageDirection::Descending, None, 1, MAX_PAGE_BYTES)
                    .unwrap(),
            )
            .unwrap()
            .wait()
            .await
            .unwrap();
        while matches!(
            durable_owner.try_process_next().unwrap(),
            DurableProcessOutcome::Written { .. }
        ) {}
        owner_task.abort();
        drop(recorder);
        drop(durable_owner);

        let (restarted, restarted_owner) = DiagnosticRecorder::new();
        let drop_recorder = restarted.clone();
        let (producer, mut durable_owner) = DurableProducer::new(&root, move |summary| {
            match drop_recorder.try_record(drop_draft(9_001, summary)) {
                RecordOutcome::Accepted => Ok(()),
                _ => Err(()),
            }
        });
        let recovery = durable_owner
            .recover_startup(std::time::SystemTime::now())
            .unwrap();
        assert_eq!(recovery.last_sequence(), 3);
        let owner_task = tokio::spawn(
            restarted_owner
                .with_recovered_durable_producer(producer, recovery)
                .unwrap()
                .run(),
        );
        assert_eq!(
            restarted.try_record(durable_draft(4)),
            RecordOutcome::Accepted
        );
        restarted
            .try_query(
                PageRequest::with_limits(PageDirection::Descending, None, 1, MAX_PAGE_BYTES)
                    .unwrap(),
            )
            .unwrap()
            .wait()
            .await
            .unwrap();
        while matches!(
            durable_owner.try_process_next().unwrap(),
            DurableProcessOutcome::Written { .. }
        ) {}
        owner_task.abort();

        let mut files = fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        files.sort();
        let sequences = files
            .into_iter()
            .flat_map(|file| fs::read(file).unwrap())
            .collect::<Vec<_>>()
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                crate::event::DiagnosticEvent::decode(line)
                    .unwrap()
                    .sequence()
            })
            .collect::<Vec<_>>();
        assert_eq!(sequences, [1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn recovered_owner_elevates_prequeued_events_without_reusing_a_sequence() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("prequeued-recovery");
        fs::create_dir(&root).unwrap();
        seed_recovered_sequence(&root, 40);

        let (recorder, owner) = DiagnosticRecorder::new();
        assert_eq!(
            recorder.try_record(durable_draft(2)),
            RecordOutcome::Accepted
        );
        assert_eq!(
            recorder.try_record(durable_draft(3)),
            RecordOutcome::Accepted
        );
        let (producer, mut durable_owner) = DurableProducer::new(&root, |_| Ok::<_, ()>(()));
        let recovery = durable_owner
            .recover_startup(std::time::SystemTime::now())
            .unwrap();
        assert_eq!(recovery.last_sequence(), 40);
        let owner_task = tokio::spawn(
            owner
                .with_recovered_durable_producer(producer, recovery)
                .unwrap()
                .run(),
        );

        let page = recorder
            .try_query(
                PageRequest::with_limits(PageDirection::Ascending, None, 8, MAX_PAGE_BYTES)
                    .unwrap(),
            )
            .unwrap()
            .wait()
            .await
            .unwrap();
        assert_eq!(
            page.events()
                .iter()
                .map(|event| event.sequence())
                .collect::<Vec<_>>(),
            [41, 42]
        );
        loop {
            match durable_owner.try_process_next().unwrap() {
                DurableProcessOutcome::Idle | DurableProcessOutcome::DropSummaryRequested => break,
                DurableProcessOutcome::Written { .. } => {}
            }
        }
        owner_task.abort();

        let bytes = fs::read(root.join("segment-00000000000000000001.jsonl")).unwrap();
        let sequences = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                crate::event::DiagnosticEvent::decode(line)
                    .unwrap()
                    .sequence()
            })
            .collect::<Vec<_>>();
        assert_eq!(sequences, [40, 41, 42]);
    }

    #[tokio::test]
    async fn recovered_owner_reserves_the_last_sequence_and_fails_closed_past_u64_max() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("maximum-recovery");
        fs::create_dir(&root).unwrap();
        seed_recovered_sequence(&root, u64::MAX - 1);

        let (recorder, owner) = DiagnosticRecorder::new();
        assert_eq!(
            recorder.try_record(durable_draft(2)),
            RecordOutcome::Accepted
        );
        let (producer, mut durable_owner) = DurableProducer::new(&root, |_| Ok::<_, ()>(()));
        let recovery = durable_owner
            .recover_startup(std::time::SystemTime::now())
            .unwrap();
        let owner_task = tokio::spawn(
            owner
                .with_recovered_durable_producer(producer, recovery)
                .unwrap()
                .run(),
        );
        assert_eq!(
            recorder.try_record(durable_draft(3)),
            RecordOutcome::DroppedInvalid
        );
        let page = recorder
            .try_query(
                PageRequest::with_limits(PageDirection::Ascending, None, 8, MAX_PAGE_BYTES)
                    .unwrap(),
            )
            .unwrap()
            .wait()
            .await
            .unwrap();
        assert_eq!(page.events()[0].sequence(), u64::MAX);
        assert!(matches!(
            durable_owner.try_process_next().unwrap(),
            DurableProcessOutcome::Written { events: 1, .. }
        ));
        owner_task.abort();

        let overflow_root = directory.path().join("maximum-recovery-overflow");
        fs::create_dir(&overflow_root).unwrap();
        seed_recovered_sequence(&overflow_root, u64::MAX - 1);
        let (overflow_recorder, overflow_owner) = DiagnosticRecorder::new();
        assert_eq!(
            overflow_recorder.try_record(durable_draft(4)),
            RecordOutcome::Accepted
        );
        assert_eq!(
            overflow_recorder.try_record(durable_draft(5)),
            RecordOutcome::Accepted
        );
        let (producer, mut durable_owner) =
            DurableProducer::new(&overflow_root, |_| Ok::<_, ()>(()));
        let recovery = durable_owner
            .recover_startup(std::time::SystemTime::now())
            .unwrap();
        assert!(
            overflow_owner
                .with_recovered_durable_producer(producer, recovery)
                .is_err()
        );
        assert_eq!(
            overflow_recorder.try_record(durable_draft(6)),
            RecordOutcome::DroppedClosed
        );
    }

    #[tokio::test]
    async fn recorder_owner_records_a_closed_durable_forward_in_typed_metrics() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("closed-durable-forward");
        fs::create_dir(&root).unwrap();
        let (producer, durable_owner) = DurableProducer::new(&root, |_| Ok::<_, ()>(()));
        let durable_metrics = producer.clone();
        drop(durable_owner);
        let (recorder, owner) = DiagnosticRecorder::new();
        let owner_task = tokio::spawn(owner.with_durable_producer(producer).run());

        assert_eq!(
            recorder.try_record(durable_draft(1)),
            RecordOutcome::Accepted
        );
        recorder
            .try_query(
                PageRequest::with_limits(PageDirection::Descending, None, 1, MAX_PAGE_BYTES)
                    .unwrap(),
            )
            .unwrap()
            .wait()
            .await
            .unwrap();

        assert_eq!(durable_metrics.drop_metrics().ingress_closed(), 1);
        assert_eq!(recorder.metrics().durable_closed(), 1);
        owner_task.abort();
    }

    #[tokio::test]
    async fn drop_summary_uses_recorder_ingress_and_stays_ordered_with_following_events() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("drop-order");
        fs::create_dir(&root).unwrap();
        let (recorder, owner) = DiagnosticRecorder::new();
        let drop_recorder = recorder.clone();
        let (producer, mut durable_owner) = DurableProducer::new(&root, move |summary| {
            match drop_recorder.try_record(drop_draft(9_999, summary)) {
                RecordOutcome::Accepted => Ok(()),
                _ => Err(()),
            }
        });
        let owner_task = tokio::spawn(owner.with_durable_producer(producer).run());

        for identity in 1..=300 {
            loop {
                match recorder.try_record(durable_draft(identity)) {
                    RecordOutcome::Accepted => break,
                    RecordOutcome::DroppedFull => tokio::task::yield_now().await,
                    outcome => panic!("unexpected record outcome: {outcome:?}"),
                }
            }
        }
        let barrier = recorder
            .try_query(
                PageRequest::with_limits(PageDirection::Descending, None, 1, MAX_PAGE_BYTES)
                    .unwrap(),
            )
            .unwrap();
        barrier.wait().await.unwrap();

        loop {
            match durable_owner.try_process_next().unwrap() {
                DurableProcessOutcome::DropSummaryRequested => break,
                DurableProcessOutcome::Written { .. } => {}
                DurableProcessOutcome::Idle => tokio::task::yield_now().await,
            }
        }
        assert_eq!(
            recorder.try_record(durable_draft(10_000)),
            RecordOutcome::Accepted
        );
        recorder
            .try_query(
                PageRequest::with_limits(PageDirection::Descending, None, 1, MAX_PAGE_BYTES)
                    .unwrap(),
            )
            .unwrap()
            .wait()
            .await
            .unwrap();
        loop {
            match durable_owner.try_process_next().unwrap() {
                DurableProcessOutcome::Idle => break,
                DurableProcessOutcome::DropSummaryRequested
                | DurableProcessOutcome::Written { .. } => {}
            }
        }

        let mut files = fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        files.sort();
        let mut events = Vec::new();
        for file in files {
            for line in fs::read(file)
                .unwrap()
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
            {
                events.push(crate::event::DiagnosticEvent::decode(line).unwrap());
            }
        }
        assert!(
            events
                .windows(2)
                .all(|pair| pair[0].sequence() < pair[1].sequence())
        );
        let drop_index = events
            .iter()
            .position(|event| event.code() == DiagnosticEventCode::DiagnosticDrop)
            .unwrap();
        assert_eq!(
            events[drop_index]
                .diagnostic_drop_counts()
                .unwrap()
                .ingress_full(),
            44
        );
        assert_eq!(
            events.last().unwrap().event_id(),
            EventId::parse("018f47a2-4c1d-7a8f-9b2d-000000002710").unwrap()
        );
        assert!(drop_index < events.len() - 1);
        owner_task.abort();
    }
}
