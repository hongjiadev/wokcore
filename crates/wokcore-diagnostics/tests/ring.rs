use std::{collections::HashSet, path::Path, sync::Arc, time::Duration};

use wokcore_diagnostics::{
    event::{
        BuildIdentity, CapabilityVersion, DiagnosticBuildError, DiagnosticComponent,
        DiagnosticEventCode, DiagnosticEventDraft, DiagnosticLevel, EventId, FailoverDecision,
        GitCommit, MAX_PREPARED_EVENT_BYTES, ProviderProtocol, RetryDecision, StageCode,
        UtcTimestamp, WokcoreVersion,
    },
    recorder::{
        DiagnosticRecorder, INGRESS_QUEUE_CAPACITY, QUERY_QUEUE_CAPACITY, QueryAdmissionError,
        QueryReplyError, RecordOutcome,
    },
    redaction::{
        RedactedSummaries, SensitiveValues, StructuralObservation, StructuralObservations,
        StructuralSummaryInput, build_structural_summary,
    },
    ring::{
        MAX_PAGE_BYTES, MAX_PAGE_EVENTS, MAX_RING_BYTES, PageCursor, PageDirection, PageRequest,
    },
};

fn draft(
    identity: u64,
    cache_hits: usize,
    json_shapes: usize,
) -> Result<DiagnosticEventDraft, DiagnosticBuildError> {
    let mut observations = StructuralObservations::new();
    for _ in 0..cache_hits {
        observations = observations.push(StructuralObservation::CacheHit)?;
    }
    for _ in 0..json_shapes {
        observations = observations.push(StructuralObservation::JsonShape)?;
    }
    let summary = build_structural_summary(
        StructuralSummaryInput::new(
            ProviderProtocol::OpenAiResponses,
            StageCode::Upstream,
            RetryDecision::NotRetried,
            FailoverDecision::NotSelected,
            false,
        )
        .with_observations(observations),
        SensitiveValues::new(),
    )?;
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
    )
    .with_redacted_summaries(RedactedSummaries::new().push(summary)?))
}

async fn newest(
    recorder: &DiagnosticRecorder,
) -> wokcore_diagnostics::event::PreparedDiagnosticEvent {
    recorder
        .try_query(
            PageRequest::with_limits(PageDirection::Descending, None, 1, MAX_PAGE_BYTES).unwrap(),
        )
        .unwrap()
        .wait()
        .await
        .unwrap()
        .events()[0]
        .clone()
}

async fn measure(identity: u64, cache_hits: usize, json_shapes: usize) -> usize {
    let (recorder, owner) = DiagnosticRecorder::new();
    let owner_task = tokio::spawn(owner.run());
    assert_eq!(
        recorder.try_record(draft(identity, cache_hits, json_shapes)),
        RecordOutcome::Accepted
    );
    let length = newest(&recorder).await.encoded_len();
    owner_task.abort();
    length
}

async fn page(
    recorder: &DiagnosticRecorder,
    direction: PageDirection,
    cursor: Option<PageCursor>,
    max_events: usize,
    max_bytes: usize,
) -> wokcore_diagnostics::ring::RingPage {
    recorder
        .try_query(PageRequest::with_limits(direction, cursor, max_events, max_bytes).unwrap())
        .unwrap()
        .wait()
        .await
        .unwrap()
}

#[tokio::test]
async fn ring_uses_exact_prepared_lengths_and_whole_event_eviction() {
    let first_len = measure(1, 0, 80).await;
    let second_len = measure(2, 0, 70).await;
    let third_len = measure(3, 0, 90).await;
    let budget = first_len + second_len;
    let (recorder, owner) = DiagnosticRecorder::with_ring_byte_budget(budget).unwrap();
    let owner_task = tokio::spawn(owner.run());

    assert_eq!(
        recorder.try_record(draft(1, 0, 80)),
        RecordOutcome::Accepted
    );
    assert_eq!(
        recorder.try_record(draft(2, 0, 70)),
        RecordOutcome::Accepted
    );
    let before = page(
        &recorder,
        PageDirection::Ascending,
        None,
        10,
        MAX_PAGE_BYTES,
    )
    .await;
    assert_eq!(before.events().len(), 2);
    assert_eq!(before.ring_retained_bytes(), first_len + second_len);

    assert_eq!(
        recorder.try_record(draft(3, 0, 90)),
        RecordOutcome::Accepted
    );
    let after = page(
        &recorder,
        PageDirection::Ascending,
        None,
        10,
        MAX_PAGE_BYTES,
    )
    .await;
    assert_eq!(
        after
            .events()
            .iter()
            .map(|event| event.event_id())
            .collect::<Vec<_>>(),
        [EventId::parse("018f47a2-4c1d-7a8f-9b2d-000000000003").unwrap()]
    );
    assert_eq!(after.ring_retained_bytes(), third_len);
    owner_task.abort();

    let (_, max_owner) = DiagnosticRecorder::with_ring_byte_budget(MAX_RING_BYTES).unwrap();
    drop(max_owner);
    assert!(DiagnosticRecorder::with_ring_byte_budget(MAX_PREPARED_EVENT_BYTES - 1).is_err());
    assert!(DiagnosticRecorder::with_ring_byte_budget(MAX_RING_BYTES + 1).is_err());
}

#[tokio::test]
async fn ring_page_is_count_and_byte_bounded_without_full_ring_copy() {
    let (recorder, owner) = DiagnosticRecorder::new();
    let owner_task = tokio::spawn(owner.run());
    for identity in 1..=6 {
        assert_eq!(
            recorder.try_record(draft(identity, identity as usize, 0)),
            RecordOutcome::Accepted
        );
    }

    let ascending = page(&recorder, PageDirection::Ascending, None, 2, MAX_PAGE_BYTES).await;
    assert_eq!(
        ascending
            .events()
            .iter()
            .map(|event| event.sequence())
            .collect::<Vec<_>>(),
        [1, 2]
    );
    let repeated = page(&recorder, PageDirection::Ascending, None, 2, MAX_PAGE_BYTES).await;
    assert!(Arc::ptr_eq(
        &ascending.events()[0].encoded_handle(),
        &repeated.events()[0].encoded_handle()
    ));
    let next = page(
        &recorder,
        PageDirection::Ascending,
        ascending.next_cursor(),
        2,
        MAX_PAGE_BYTES,
    )
    .await;
    assert_eq!(
        next.events()
            .iter()
            .map(|event| event.sequence())
            .collect::<Vec<_>>(),
        [3, 4]
    );

    let descending = page(
        &recorder,
        PageDirection::Descending,
        None,
        2,
        MAX_PAGE_BYTES,
    )
    .await;
    assert_eq!(
        descending
            .events()
            .iter()
            .map(|event| event.sequence())
            .collect::<Vec<_>>(),
        [6, 5]
    );
    let descending_next = page(
        &recorder,
        PageDirection::Descending,
        descending.next_cursor(),
        2,
        MAX_PAGE_BYTES,
    )
    .await;
    assert_eq!(
        descending_next
            .events()
            .iter()
            .map(|event| event.sequence())
            .collect::<Vec<_>>(),
        [4, 3]
    );
    owner_task.abort();

    let (large_recorder, large_owner) = DiagnosticRecorder::new();
    let large_task = tokio::spawn(large_owner.run());
    assert_eq!(
        large_recorder.try_record(draft(100, 0, 130)),
        RecordOutcome::Accepted
    );
    assert_eq!(
        large_recorder.try_record(draft(101, 0, 130)),
        RecordOutcome::Accepted
    );
    let byte_bounded = page(
        &large_recorder,
        PageDirection::Ascending,
        None,
        MAX_PAGE_EVENTS,
        MAX_PREPARED_EVENT_BYTES,
    )
    .await;
    assert_eq!(byte_bounded.events().len(), 1);
    assert_eq!(
        byte_bounded.encoded_bytes(),
        byte_bounded.events()[0].encoded_len()
    );
    assert!(byte_bounded.encoded_bytes() <= MAX_PREPARED_EVENT_BYTES);
    large_task.abort();
}

#[tokio::test]
async fn page_budget_floor_guarantees_lossless_progress_in_both_directions() {
    assert!(
        PageRequest::with_limits(
            PageDirection::Ascending,
            None,
            1,
            MAX_PREPARED_EVENT_BYTES - 1,
        )
        .is_err()
    );
    let (recorder, owner) = DiagnosticRecorder::new();
    let owner_task = tokio::spawn(owner.run());
    for identity in 1..=40 {
        assert_eq!(
            recorder.try_record(draft(identity, identity as usize, 0)),
            RecordOutcome::Accepted
        );
    }

    for direction in [PageDirection::Ascending, PageDirection::Descending] {
        let mut cursor = None;
        let mut seen = Vec::new();
        loop {
            let current = page(&recorder, direction, cursor, 1, MAX_PREPARED_EVENT_BYTES).await;
            if current.events().is_empty() {
                assert!(current.next_cursor().is_none());
                break;
            }
            assert_eq!(current.events().len(), 1);
            assert!(current.encoded_bytes() <= MAX_PREPARED_EVENT_BYTES);
            let next = current.next_cursor();
            assert_ne!(next, cursor);
            cursor = next;
            seen.push(current.events()[0].sequence());
        }
        let expected = match direction {
            PageDirection::Ascending => (1..=40).collect::<Vec<_>>(),
            PageDirection::Descending => (1..=40).rev().collect::<Vec<_>>(),
        };
        assert_eq!(seen, expected);
        assert_eq!(seen.iter().copied().collect::<HashSet<_>>().len(), 40);
    }
    owner_task.abort();
}

#[test]
fn ingress_try_send_saturates_at_256_without_waiting_or_proxy_failure() {
    let (recorder, owner) = DiagnosticRecorder::new();
    for identity in 0..INGRESS_QUEUE_CAPACITY {
        assert_eq!(
            recorder.try_record(draft(identity as u64 + 1, 0, 0)),
            RecordOutcome::Accepted
        );
    }
    assert_eq!(
        recorder.try_record(draft(999, 0, 0)),
        RecordOutcome::DroppedFull
    );
    assert_eq!(recorder.metrics().ingress_full(), 1);
    assert_eq!(
        recorder.try_record(Err(DiagnosticBuildError::InvalidValue)),
        RecordOutcome::DroppedInvalid
    );
    assert_eq!(recorder.metrics().invalid(), 1);
    drop(owner);
    assert_eq!(
        recorder.try_record(draft(1000, 0, 0)),
        RecordOutcome::DroppedClosed
    );
    assert_eq!(recorder.metrics().ingress_closed(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_owner_and_query_saturation_do_not_block_recorder() {
    let (recorder, owner) = DiagnosticRecorder::new();
    let mut pending = Vec::new();
    for _ in 0..QUERY_QUEUE_CAPACITY {
        pending.push(
            recorder
                .try_query(PageRequest::default_for(PageDirection::Ascending))
                .unwrap(),
        );
    }
    assert!(matches!(
        recorder.try_query(PageRequest::default_for(PageDirection::Ascending)),
        Err(QueryAdmissionError::Busy)
    ));
    assert_eq!(
        pending
            .pop()
            .unwrap()
            .wait_with_deadline(Duration::from_millis(1))
            .await,
        Err(QueryReplyError::Deadline)
    );
    for identity in 0..INGRESS_QUEUE_CAPACITY {
        assert_eq!(
            recorder.try_record(draft(identity as u64 + 1, 0, 0)),
            RecordOutcome::Accepted
        );
    }
    assert_eq!(
        recorder.try_record(draft(999, 0, 0)),
        RecordOutcome::DroppedFull
    );
    assert_eq!(recorder.metrics().query_busy(), 1);
    assert_eq!(recorder.metrics().ingress_full(), 1);
    drop(owner);
    assert_eq!(
        pending.pop().unwrap().wait().await,
        Err(QueryReplyError::Closed)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forced_two_producer_interleave_preserves_every_accepted_sequence() {
    let (recorder, owner) = DiagnosticRecorder::new();
    let owner_task = tokio::spawn(owner.run());
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut tasks = Vec::new();
    for producer in 0..2u64 {
        let recorder = recorder.clone();
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            let mut accepted = 0usize;
            for index in 0..100u64 {
                barrier.wait().await;
                let identity = 1 + producer * 1_000 + index;
                if recorder.try_record(draft(identity, index as usize % 4, 0))
                    == RecordOutcome::Accepted
                {
                    accepted += 1;
                }
                barrier.wait().await;
            }
            accepted
        }));
    }
    let mut accepted = 0usize;
    for task in tasks {
        accepted += task.await.unwrap();
    }
    assert_eq!(accepted, 200);
    assert_eq!(
        recorder.try_record(Err(DiagnosticBuildError::InvalidValue)),
        RecordOutcome::DroppedInvalid
    );
    let retained = page(
        &recorder,
        PageDirection::Ascending,
        None,
        MAX_PAGE_EVENTS,
        MAX_PAGE_BYTES,
    )
    .await;
    owner_task.abort();
    assert_eq!(retained.events().len(), accepted);
    assert_eq!(
        retained
            .events()
            .iter()
            .map(|event| event.sequence())
            .collect::<Vec<_>>(),
        (1..=accepted as u64).collect::<Vec<_>>()
    );
    assert_eq!(
        retained
            .events()
            .iter()
            .map(|event| event.event_id())
            .collect::<HashSet<_>>()
            .len(),
        accepted
    );
    assert_eq!(recorder.metrics().invalid(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_drains_admitted_queries_after_all_senders_close() {
    let (recorder, owner) = DiagnosticRecorder::new();
    assert_eq!(recorder.try_record(draft(1, 0, 0)), RecordOutcome::Accepted);
    let pending = recorder
        .try_query(PageRequest::default_for(PageDirection::Ascending))
        .unwrap();
    drop(recorder);
    owner.run().await;
    let page = pending.wait().await.unwrap();
    assert_eq!(page.events().len(), 1);
    assert_eq!(page.events()[0].sequence(), 1);
}

#[test]
fn owner_is_single_writer_and_never_waits_with_ring_ownership() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let recorder = std::fs::read_to_string(root.join("recorder.rs")).unwrap();
    let event = std::fs::read_to_string(root.join("event.rs")).unwrap();
    let ring = std::fs::read_to_string(root.join("ring.rs")).unwrap();
    for forbidden in [
        "Mutex",
        "RwLock",
        "spin_loop",
        "thread::sleep",
        "tokio::time::sleep",
        "Semaphore",
    ] {
        assert!(!recorder.contains(forbidden), "{forbidden}");
        assert!(!ring.contains(forbidden), "{forbidden}");
    }
    assert!(recorder.contains("ring: DiagnosticRing"));
    assert!(recorder.contains("fn finalize_and_insert"));
    assert_eq!(recorder.matches("prepare_template()").count(), 1);
    assert_eq!(event.matches("serde_json::to_vec(&self.0)").count(), 1);
    assert!(event.contains("self.encoded[index] ="));
    assert!(!ring.contains("pub struct DiagnosticRing"));
    assert!(!ring.contains("pub fn insert(&mut self"));
}

#[test]
fn all_debug_metrics_and_drop_paths_are_canary_free() {
    let canary = "Authorization=Bearer_秘密🧪_C:\\Users\\Alice";
    let (recorder, owner) = DiagnosticRecorder::new();
    assert_eq!(
        recorder.try_record(Err(UtcTimestamp::parse(canary).unwrap_err())),
        RecordOutcome::DroppedInvalid
    );
    for _ in 0..QUERY_QUEUE_CAPACITY {
        recorder
            .try_query(PageRequest::default_for(PageDirection::Ascending))
            .unwrap();
    }
    assert!(matches!(
        recorder.try_query(PageRequest::default_for(PageDirection::Ascending)),
        Err(QueryAdmissionError::Busy)
    ));
    let metrics = recorder.metrics();
    let rendered = format!("{recorder:?} {owner:?} {metrics:?}");
    assert!(!rendered.contains(canary));
    assert_eq!(metrics.invalid(), 1);
    assert_eq!(metrics.query_busy(), 1);
    assert_eq!(metrics.total_dropped(), 2);
}
