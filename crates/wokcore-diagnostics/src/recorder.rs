use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::sync::{mpsc, oneshot};

use crate::{
    event::{DiagnosticBuildError, DiagnosticEventDraft, DiagnosticEventTemplate},
    ring::{DiagnosticRing, PageRequest, RingInsertOutcome, RingPage},
};

pub const INGRESS_QUEUE_CAPACITY: usize = 256;
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
    queries: mpsc::Sender<QueryCommand>,
    metrics: DropMetrics,
    remaining_sequences: Arc<AtomicU64>,
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
        let (queries, query_receiver) = mpsc::channel(QUERY_QUEUE_CAPACITY);
        let metrics = DropMetrics::new();
        let remaining = u64::MAX.saturating_sub(next_sequence).saturating_add(1);
        (
            Self {
                ingress,
                queries,
                metrics: metrics.clone(),
                remaining_sequences: Arc::new(AtomicU64::new(remaining)),
            },
            RecorderOwner {
                ring,
                ingress: ingress_receiver,
                queries: query_receiver,
                metrics,
                next_sequence,
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
        let draft = match draft {
            Ok(draft) => draft,
            Err(_) => {
                increment(&self.metrics.inner.invalid);
                return RecordOutcome::DroppedInvalid;
            }
        };
        let permit = match self.ingress.try_reserve() {
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
        let (reply, receiver) = oneshot::channel();
        match self.queries.try_send(QueryCommand { request, reply }) {
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

pub struct RecorderOwner {
    ring: DiagnosticRing,
    ingress: mpsc::Receiver<DiagnosticEventTemplate>,
    queries: mpsc::Receiver<QueryCommand>,
    metrics: DropMetrics,
    next_sequence: u64,
}

impl RecorderOwner {
    pub async fn run(mut self) {
        let mut ingress_open = true;
        let mut queries_open = true;
        while ingress_open || queries_open {
            tokio::select! {
                biased;
                event = self.ingress.recv(), if ingress_open => {
                    match event {
                        Some(event) => self.finalize_and_insert(event),
                        None => ingress_open = false,
                    }
                }
                query = self.queries.recv(), if queries_open => {
                    match query {
                        Some(query) => {
                            let page = self.ring.page(query.request);
                            let _ = query.reply.send(page);
                        }
                        None => queries_open = false,
                    }
                }
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
        match self.ring.insert(event) {
            RingInsertOutcome::Inserted => {}
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
    use std::sync::{Arc, Barrier};

    use super::*;
    use crate::{
        event::{
            BuildIdentity, CapabilityVersion, DiagnosticComponent, DiagnosticEventCode,
            DiagnosticLevel, EventId, GitCommit, UtcTimestamp, WokcoreVersion,
        },
        ring::{MAX_PAGE_BYTES, PageDirection},
    };

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
}
