use std::{
    fs,
    sync::{Arc, OnceLock, mpsc as std_mpsc},
    time::Duration,
};

use tempfile::tempdir;
use wokcore_diagnostics::{
    event::{
        BuildIdentity, CapabilityVersion, DiagnosticBuildError, DiagnosticComponent,
        DiagnosticDropCounts, DiagnosticEvent, DiagnosticEventCode, DiagnosticEventDraft,
        DiagnosticLevel, EventId, FailoverDecision, GitCommit, ProviderProtocol, RetryDecision,
        StageCode, UtcTimestamp, WokcoreVersion,
    },
    recorder::{DiagnosticRecorder, RecordOutcome},
    redaction::{
        RedactedSummaries, SensitiveValues, StructuralObservation, StructuralObservations,
        StructuralSummaryInput, build_structural_summary,
    },
    retention::{RetentionManager, RetentionPolicy, RetentionTrigger},
    ring::{MAX_PAGE_BYTES, PageDirection, PageRequest},
    segment::{
        BatchLimit, BatchPushError, BoxedDurableWriterOwner, DURABLE_QUEUE_CAPACITY,
        DiagnosticDropCause, DropRecoveryTracker, DurableBatch, DurableEventKind, DurableFilter,
        DurableProcessError, DurableProcessOutcome, DurableProducer, DurableRecordOutcome,
        DurableWorkOutcome, FlushOutcome, MAX_BATCH_EVENT_BYTES, MAX_BATCH_EVENTS, SegmentWriter,
    },
    snapshot::{
        FailureSnapshot, MAX_FAILURE_SNAPSHOT_BYTES, MAX_FAILURE_SNAPSHOTS,
        SNAPSHOT_QUEUE_CAPACITY, SNAPSHOT_WRITE_BUDGET_DEFAULT, SNAPSHOT_WRITE_BUDGET_HARD_MAX,
        SnapshotCause, SnapshotConfigurationSummary, SnapshotCorrelation, SnapshotErrorCode,
        SnapshotLifecycleState, SnapshotPolicy, SnapshotRecorder, SnapshotRedactionSummary,
        SnapshotRequest, SnapshotRequestOutcome, SnapshotResourceState,
    },
};

fn draft(identity: u64) -> Result<DiagnosticEventDraft, DiagnosticBuildError> {
    let summary = build_structural_summary(
        StructuralSummaryInput::new(
            ProviderProtocol::OpenAiResponses,
            StageCode::Upstream,
            RetryDecision::NotRetried,
            FailoverDecision::NotSelected,
            false,
        ),
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
    .with_redacted_summaries(
        wokcore_diagnostics::redaction::RedactedSummaries::new().push(summary)?,
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

fn diagnostic_entries(root: &std::path::Path) -> Vec<fs::DirEntry> {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| !is_internal_diagnostic_entry(&entry.file_name()))
        .collect()
}

#[cfg(target_os = "macos")]
fn is_internal_diagnostic_entry(name: &std::ffi::OsStr) -> bool {
    name == std::ffi::OsStr::new(".wokcore-diagnostic-parent.lock")
}

#[cfg(not(target_os = "macos"))]
fn is_internal_diagnostic_entry(_name: &std::ffi::OsStr) -> bool {
    false
}

async fn prepared(identity: u64) -> wokcore_diagnostics::event::PreparedDiagnosticEvent {
    let (recorder, owner) = DiagnosticRecorder::new();
    let owner_task = tokio::spawn(owner.run());
    for sequence in 1..=identity {
        loop {
            match recorder.try_record(draft(sequence)) {
                RecordOutcome::Accepted => break,
                RecordOutcome::DroppedFull => tokio::task::yield_now().await,
                outcome => panic!("unexpected record outcome: {outcome:?}"),
            }
        }
    }
    let page = recorder
        .try_query(
            PageRequest::with_limits(PageDirection::Descending, None, 1, MAX_PAGE_BYTES).unwrap(),
        )
        .unwrap()
        .wait()
        .await
        .unwrap();
    let event = page.events()[0].clone();
    owner_task.abort();
    event
}

async fn prepared_many(count: usize) -> Vec<wokcore_diagnostics::event::PreparedDiagnosticEvent> {
    let (recorder, owner) = DiagnosticRecorder::new();
    let owner_task = tokio::spawn(owner.run());
    for identity in 1..=count {
        loop {
            match recorder.try_record(durable_draft(u64::try_from(identity).unwrap())) {
                RecordOutcome::Accepted => break,
                RecordOutcome::DroppedFull => tokio::task::yield_now().await,
                outcome => panic!("unexpected record outcome: {outcome:?}"),
            }
        }
    }
    let page = recorder
        .try_query(
            PageRequest::with_limits(PageDirection::Ascending, None, count, MAX_PAGE_BYTES)
                .unwrap(),
        )
        .unwrap()
        .wait()
        .await
        .unwrap();
    let events = page.events().to_vec();
    owner_task.abort();
    assert_eq!(events.len(), count);
    events
}

#[tokio::test]
async fn partial_flush_window_coalesces_spaced_durable_events() {
    let directory = tempdir().unwrap();
    let events = prepared_many(3).await;
    let (producer, mut owner) = DurableProducer::new(directory.path(), |_| Ok::<_, ()>(()));
    assert_eq!(
        producer.try_record(events[0].clone()),
        DurableRecordOutcome::Accepted
    );
    let delayed = producer.clone();
    let sender = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            delayed.try_record(events[1].clone()),
            DurableRecordOutcome::Accepted
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            delayed.try_record(events[2].clone()),
            DurableRecordOutcome::Accepted
        );
    });

    let outcome = owner
        .wait_process_next_batched(Duration::from_millis(100))
        .await
        .unwrap();
    sender.await.unwrap();

    assert_eq!(
        outcome,
        DurableWorkOutcome::Written {
            events: 3,
            rotations: 0
        }
    );
}

#[tokio::test]
async fn full_durable_batch_flushes_before_the_partial_deadline() {
    let directory = tempdir().unwrap();
    let events = prepared_many(MAX_BATCH_EVENTS).await;
    let (producer, mut owner) = DurableProducer::new(directory.path(), |_| Ok::<_, ()>(()));
    for event in events {
        assert_eq!(producer.try_record(event), DurableRecordOutcome::Accepted);
    }
    let started = tokio::time::Instant::now();

    let outcome = owner
        .wait_process_next_batched(Duration::from_secs(5))
        .await
        .unwrap();

    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(
        outcome,
        DurableWorkOutcome::Written {
            events: MAX_BATCH_EVENTS,
            rotations: 0
        }
    );
}

async fn prepared_debug(identity: u64) -> wokcore_diagnostics::event::PreparedDiagnosticEvent {
    let (recorder, owner) = DiagnosticRecorder::new();
    let owner_task = tokio::spawn(owner.run());
    for sequence in 1..identity {
        loop {
            match recorder.try_record(draft(sequence)) {
                RecordOutcome::Accepted => break,
                RecordOutcome::DroppedFull => tokio::task::yield_now().await,
                outcome => panic!("unexpected record outcome: {outcome:?}"),
            }
        }
    }
    let event = DiagnosticEventDraft::new(
        EventId::parse(&format!("018f47a2-4c1d-7a8f-9b2d-{identity:012x}")).unwrap(),
        UtcTimestamp::parse("2026-07-26T12:30:00Z").unwrap(),
        DiagnosticLevel::Debug,
        DiagnosticComponent::Diagnostics,
        DiagnosticEventCode::LifecycleTransition,
        BuildIdentity::new(
            WokcoreVersion::parse("0.1.0").unwrap(),
            GitCommit::parse("0123456789abcdef0123456789abcdef01234567").unwrap(),
            1,
            CapabilityVersion::new(3),
        ),
    );
    loop {
        match recorder.try_record(Ok(event.clone())) {
            RecordOutcome::Accepted => break,
            RecordOutcome::DroppedFull => tokio::task::yield_now().await,
            outcome => panic!("unexpected record outcome: {outcome:?}"),
        }
    }
    let prepared = recorder
        .try_query(
            PageRequest::with_limits(PageDirection::Descending, None, 1, MAX_PAGE_BYTES).unwrap(),
        )
        .unwrap()
        .wait()
        .await
        .unwrap()
        .events()[0]
        .clone();
    owner_task.abort();
    prepared
}

async fn truncated_prepared(identity: u64) -> wokcore_diagnostics::event::PreparedDiagnosticEvent {
    let mut observations = StructuralObservations::new();
    for _ in 0..256 {
        observations = observations.push(StructuralObservation::JsonShape).unwrap();
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
    )
    .unwrap();
    let draft = DiagnosticEventDraft::new(
        EventId::parse(&format!("018f47a2-4c1d-7a8f-9b2d-{identity:012x}")).unwrap(),
        UtcTimestamp::parse("2026-07-26T12:30:00Z").unwrap(),
        DiagnosticLevel::Error,
        DiagnosticComponent::Diagnostics,
        DiagnosticEventCode::RequestFailed,
        BuildIdentity::new(
            WokcoreVersion::parse("0.1.0").unwrap(),
            GitCommit::parse("0123456789abcdef0123456789abcdef01234567").unwrap(),
            1,
            CapabilityVersion::new(3),
        ),
    )
    .with_redacted_summaries(RedactedSummaries::new().push(summary).unwrap());
    let (recorder, owner) = DiagnosticRecorder::new();
    let owner_task = tokio::spawn(owner.run());
    assert_eq!(recorder.try_record(Ok(draft)), RecordOutcome::Accepted);
    let event = recorder
        .try_query(
            PageRequest::with_limits(PageDirection::Descending, None, 1, MAX_PAGE_BYTES).unwrap(),
        )
        .unwrap()
        .wait()
        .await
        .unwrap()
        .events()[0]
        .clone();
    owner_task.abort();
    assert!(
        wokcore_diagnostics::event::DiagnosticEvent::decode(event.encoded()).is_err(),
        "the public untrusted decoder must remain closed to truncated summaries"
    );
    event
}

async fn prepared_drop(
    identity: u64,
    counts: DiagnosticDropCounts,
) -> wokcore_diagnostics::event::PreparedDiagnosticEvent {
    let drop_draft = DiagnosticEventDraft::new(
        EventId::parse(&format!("018f47a2-4c1d-7a8f-9b2d-{identity:012x}")).unwrap(),
        UtcTimestamp::parse("2026-07-26T12:30:00Z").unwrap(),
        DiagnosticLevel::Warn,
        DiagnosticComponent::Diagnostics,
        DiagnosticEventCode::DiagnosticDrop,
        BuildIdentity::new(
            WokcoreVersion::parse("0.1.0").unwrap(),
            GitCommit::parse("0123456789abcdef0123456789abcdef01234567").unwrap(),
            1,
            CapabilityVersion::new(3),
        ),
    )
    .with_diagnostic_drop_counts(counts);
    let (recorder, owner) = DiagnosticRecorder::new();
    let owner_task = tokio::spawn(owner.run());
    for sequence in 1..identity {
        loop {
            match recorder.try_record(draft(sequence)) {
                RecordOutcome::Accepted => break,
                RecordOutcome::DroppedFull => tokio::task::yield_now().await,
                outcome => panic!("unexpected record outcome: {outcome:?}"),
            }
        }
    }
    loop {
        match recorder.try_record(Ok(drop_draft.clone())) {
            RecordOutcome::Accepted => break,
            RecordOutcome::DroppedFull => tokio::task::yield_now().await,
            outcome => panic!("unexpected record outcome: {outcome:?}"),
        }
    }
    let event = recorder
        .try_query(
            PageRequest::with_limits(PageDirection::Descending, None, 1, MAX_PAGE_BYTES).unwrap(),
        )
        .unwrap()
        .wait()
        .await
        .unwrap()
        .events()[0]
        .clone();
    owner_task.abort();
    event
}

#[tokio::test]
async fn batch_limits_are_128_events_and_256k_without_empty_tick_writes() {
    assert_eq!(MAX_BATCH_EVENTS, 128);
    assert_eq!(MAX_BATCH_EVENT_BYTES, 262_144);

    let event = prepared(1).await;
    let mut count_limited = DurableBatch::new();
    for _ in 0..MAX_BATCH_EVENTS {
        count_limited.try_push(event.clone()).unwrap();
    }
    assert_eq!(count_limited.event_count(), MAX_BATCH_EVENTS);
    assert_eq!(
        count_limited.try_push(event.clone()),
        Err(BatchPushError::new(BatchLimit::EventCount))
    );

    let mut byte_limited =
        DurableBatch::with_limits(MAX_BATCH_EVENTS, event.encoded_len()).unwrap();
    byte_limited.try_push(event.clone()).unwrap();
    assert_eq!(byte_limited.event_bytes(), event.encoded_len());
    assert_eq!(
        byte_limited.try_push(event),
        Err(BatchPushError::new(BatchLimit::EventBytes))
    );
    assert!(DurableBatch::with_limits(MAX_BATCH_EVENTS + 1, MAX_BATCH_EVENT_BYTES).is_err());
    assert!(DurableBatch::with_limits(MAX_BATCH_EVENTS, MAX_BATCH_EVENT_BYTES + 1).is_err());

    let directory = tempdir().unwrap();
    let root = directory.path().join("never-created");
    let mut writer = SegmentWriter::new(&root);
    assert_eq!(
        writer.flush(DurableBatch::new()).unwrap(),
        FlushOutcome::Noop
    );
    assert!(!root.exists());
}

#[tokio::test]
async fn segments_rotate_before_limit_and_preserve_canonical_jsonl() {
    let first = prepared(10).await;
    let second = prepared(11).await;
    let third = prepared(12).await;
    let line_bytes = first.encoded_len() + 1;
    assert_eq!(second.encoded_len() + 1, line_bytes);
    assert_eq!(third.encoded_len() + 1, line_bytes);

    let directory = tempdir().unwrap();
    let root = directory.path().join("diagnostics");
    fs::create_dir(&root).unwrap();
    let mut writer = SegmentWriter::with_segment_limit(&root, line_bytes * 2).unwrap();
    let mut first_batch = DurableBatch::new();
    first_batch.try_push(first.clone()).unwrap();
    first_batch.try_push(second.clone()).unwrap();
    assert_eq!(writer.flush(first_batch).unwrap(), FlushOutcome::Written);

    let mut second_batch = DurableBatch::new();
    second_batch.try_push(third.clone()).unwrap();
    assert_eq!(
        writer.flush(second_batch).unwrap(),
        FlushOutcome::Rotated {
            count: 1,
            active_segment: 2,
        }
    );

    let closed = root.join("segment-00000000000000000001.jsonl");
    let active = root.join("segment-00000000000000000002.jsonl");
    let mut expected_closed = Vec::new();
    expected_closed.extend_from_slice(first.encoded());
    expected_closed.push(b'\n');
    expected_closed.extend_from_slice(second.encoded());
    expected_closed.push(b'\n');
    let mut expected_active = third.encoded().to_vec();
    expected_active.push(b'\n');
    assert_eq!(fs::read(closed).unwrap(), expected_closed);
    assert_eq!(fs::read(active).unwrap(), expected_active);
}

#[tokio::test]
async fn retention_after_rotation_does_not_invalidate_the_active_segment() {
    let first = prepared(13).await;
    let second = prepared(14).await;
    let third = prepared(15).await;
    let fourth = prepared(16).await;
    let line_bytes = first.encoded_len() + 1;
    let directory = tempdir().unwrap();
    let root = directory.path().join("diagnostics");
    fs::create_dir(&root).unwrap();
    let mut writer = SegmentWriter::with_segment_limit(&root, line_bytes * 2).unwrap();

    let mut first_batch = DurableBatch::new();
    first_batch.try_push(first).unwrap();
    first_batch.try_push(second).unwrap();
    writer.flush(first_batch).unwrap();
    let mut rotation_batch = DurableBatch::new();
    rotation_batch.try_push(third.clone()).unwrap();
    let rotation = writer.flush(rotation_batch).unwrap();

    let retention = RetentionManager::with_policy(
        &root,
        RetentionPolicy::with_limits(Duration::ZERO, 0).unwrap(),
    );
    assert_eq!(
        retention
            .enforce_with_active(
                RetentionTrigger::Rotation,
                std::time::SystemTime::now(),
                &[],
                rotation.active_segment(),
            )
            .unwrap()
            .removed_files(),
        1
    );

    let mut post_retention_batch = DurableBatch::new();
    post_retention_batch.try_push(fourth.clone()).unwrap();
    assert_eq!(
        writer.flush(post_retention_batch).unwrap(),
        FlushOutcome::Written
    );
    let mut expected = third.encoded().to_vec();
    expected.push(b'\n');
    expected.extend_from_slice(fourth.encoded());
    expected.push(b'\n');
    assert_eq!(
        fs::read(root.join("segment-00000000000000000002.jsonl")).unwrap(),
        expected
    );
}

#[tokio::test]
async fn failed_rotation_is_recovered_before_the_next_batch() {
    let first = prepared(17).await;
    let dropped = prepared(18).await;
    let resumed = prepared(19).await;
    let line_bytes = first.encoded_len() + 1;
    let directory = tempdir().unwrap();
    let root = directory.path().join("diagnostics");
    fs::create_dir(&root).unwrap();
    let mut writer = SegmentWriter::with_segment_limit(&root, line_bytes).unwrap();

    let mut first_batch = DurableBatch::new();
    first_batch.try_push(first.clone()).unwrap();
    writer.flush(first_batch).unwrap();

    let raced_segment = root.join("segment-00000000000000000002.jsonl");
    fs::write(&raced_segment, b"").unwrap();
    let mut failed_batch = DurableBatch::new();
    failed_batch.try_push(dropped).unwrap();
    assert!(writer.flush(failed_batch).is_err());
    fs::remove_file(&raced_segment).unwrap();

    let mut resumed_batch = DurableBatch::new();
    resumed_batch.try_push(resumed.clone()).unwrap();
    assert_eq!(
        writer.flush(resumed_batch).unwrap(),
        FlushOutcome::Rotated {
            count: 1,
            active_segment: 2,
        }
    );
    let mut expected_first = first.encoded().to_vec();
    expected_first.push(b'\n');
    assert_eq!(
        fs::read(root.join("segment-00000000000000000001.jsonl")).unwrap(),
        expected_first
    );
    let mut expected_resumed = resumed.encoded().to_vec();
    expected_resumed.push(b'\n');
    assert_eq!(
        fs::read(root.join("segment-00000000000000000002.jsonl")).unwrap(),
        expected_resumed
    );
}

#[tokio::test]
async fn startup_recovers_a_partial_final_line_without_duplicate_bytes() {
    let first = prepared(20).await;
    let second = prepared(21).await;
    let directory = tempdir().unwrap();
    let root = directory.path().join("diagnostics");
    fs::create_dir_all(&root).unwrap();
    let active = root.join("segment-00000000000000000001.jsonl");
    let mut torn = first.encoded().to_vec();
    torn.push(b'\n');
    torn.extend_from_slice(br#"{"schema_version":"#);
    fs::write(&active, torn).unwrap();

    let mut writer = SegmentWriter::new(&root);
    let report = writer.recover().unwrap();
    assert_eq!(
        report.truncated_bytes(),
        u64::try_from(br#"{"schema_version":"#.len()).unwrap()
    );
    let mut expected = first.encoded().to_vec();
    expected.push(b'\n');
    assert_eq!(fs::read(&active).unwrap(), expected);

    let mut batch = DurableBatch::new();
    batch.try_push(second.clone()).unwrap();
    writer.flush(batch).unwrap();
    expected.extend_from_slice(second.encoded());
    expected.push(b'\n');
    assert_eq!(fs::read(active).unwrap(), expected);
}

#[tokio::test]
async fn startup_preserves_a_trusted_truncated_summary_before_a_torn_tail() {
    let event = truncated_prepared(22).await;
    let directory = tempdir().unwrap();
    let root = directory.path().join("diagnostics");
    fs::create_dir(&root).unwrap();
    let active = root.join("segment-00000000000000000001.jsonl");
    let mut bytes = event.encoded().to_vec();
    bytes.push(b'\n');
    bytes.extend_from_slice(b"{\"torn\"");
    fs::write(&active, bytes).unwrap();

    let mut writer = SegmentWriter::new(&root);
    assert_eq!(writer.recover().unwrap().truncated_bytes(), 7);
    let mut expected = event.encoded().to_vec();
    expected.push(b'\n');
    assert_eq!(fs::read(active).unwrap(), expected);
}

#[tokio::test]
async fn recovery_repairs_the_highest_trusted_prefix_and_ignores_fake_high_segments() {
    let events = prepared_many(5).await;
    let directory = tempdir().unwrap();
    let root = directory.path().join("strict-recovery");
    fs::create_dir(&root).unwrap();
    for index in 0..4_100 {
        fs::write(root.join(format!("foreign-{index:04}.bin")), b"foreign").unwrap();
    }
    let active = root.join("segment-00000000000000000001.jsonl");
    let unordered = root.join("segment-00000000000000000002.jsonl");
    let fake_high = root.join("segment-99999999999999999999.jsonl");
    let mut active_bytes = Vec::new();
    for event in &events[..2] {
        active_bytes.extend_from_slice(event.encoded());
        active_bytes.push(b'\n');
    }
    let mut unordered_bytes = Vec::new();
    unordered_bytes.extend_from_slice(events[3].encoded());
    unordered_bytes.push(b'\n');
    unordered_bytes.extend_from_slice(events[2].encoded());
    unordered_bytes.push(b'\n');
    fs::write(&active, &active_bytes).unwrap();
    fs::write(&unordered, &unordered_bytes).unwrap();
    fs::write(&fake_high, b"foreign").unwrap();

    let mut writer = SegmentWriter::new(&root);
    let report = writer.recover().unwrap();
    assert_eq!(report.active_segment(), 2);
    assert_eq!(report.last_sequence(), 4);
    let mut trusted_unordered_prefix = events[3].encoded().to_vec();
    trusted_unordered_prefix.push(b'\n');
    assert_eq!(fs::read(&unordered).unwrap(), trusted_unordered_prefix);
    assert_eq!(fs::read(&fake_high).unwrap(), b"foreign");

    let mut batch = DurableBatch::new();
    batch.try_push(events[4].clone()).unwrap();
    assert_eq!(writer.flush(batch).unwrap(), FlushOutcome::Written);
    trusted_unordered_prefix.extend_from_slice(events[4].encoded());
    trusted_unordered_prefix.push(b'\n');
    assert_eq!(fs::read(unordered).unwrap(), trusted_unordered_prefix);
    assert_eq!(fs::read(active).unwrap(), active_bytes);
}

#[tokio::test]
async fn recovery_handles_missing_newline_torn_utf8_oversized_tail_and_corrupt_older() {
    let events = prepared_many(3).await;
    for tail in [
        events[1].encoded().to_vec(),
        vec![0xf0, 0x9f],
        vec![b'x'; wokcore_diagnostics::event::MAX_PREPARED_EVENT_BYTES + 1],
    ] {
        let directory = tempdir().unwrap();
        let root = directory.path().join("tail");
        fs::create_dir(&root).unwrap();
        let active = root.join("segment-00000000000000000001.jsonl");
        let mut expected = events[0].encoded().to_vec();
        expected.push(b'\n');
        let mut torn = expected.clone();
        torn.extend_from_slice(&tail);
        fs::write(&active, torn).unwrap();

        let mut writer = SegmentWriter::new(&root);
        assert_eq!(
            writer.recover().unwrap().truncated_bytes(),
            u64::try_from(tail.len()).unwrap()
        );
        assert_eq!(fs::read(&active).unwrap(), expected);
        let mut batch = DurableBatch::new();
        batch.try_push(events[1].clone()).unwrap();
        assert_eq!(writer.flush(batch).unwrap(), FlushOutcome::Written);
        expected.extend_from_slice(events[1].encoded());
        expected.push(b'\n');
        assert_eq!(fs::read(active).unwrap(), expected);
    }

    let directory = tempdir().unwrap();
    let root = directory.path().join("corrupt-older");
    fs::create_dir(&root).unwrap();
    let older = root.join("segment-00000000000000000001.jsonl");
    let active = root.join("segment-00000000000000000002.jsonl");
    let mut corrupt = events[0].encoded().to_vec();
    corrupt.push(b'\n');
    corrupt.extend_from_slice(events[0].encoded());
    corrupt.push(b'\n');
    let mut active_bytes = events[1].encoded().to_vec();
    active_bytes.push(b'\n');
    fs::write(&older, &corrupt).unwrap();
    fs::write(&active, &active_bytes).unwrap();
    let mut writer = SegmentWriter::new(&root);
    assert!(writer.recover().is_err());
    assert_eq!(fs::read(&older).unwrap(), corrupt);
    assert_eq!(fs::read(active).unwrap(), active_bytes);
}

#[tokio::test]
async fn recovery_repairs_only_the_highest_active_suffix_and_never_mutates_corrupt_older_files() {
    let events = prepared_many(2).await;

    let directory = tempdir().unwrap();
    let root = directory.path().join("first-torn");
    fs::create_dir(&root).unwrap();
    let active = root.join("segment-00000000000000000001.jsonl");
    let first_torn = b"{\"schema_version\":".to_vec();
    fs::write(&active, &first_torn).unwrap();
    let mut writer = SegmentWriter::new(&root);
    let report = writer.recover().unwrap();
    assert_eq!(report.active_segment(), 1);
    assert_eq!(report.last_sequence(), 0);
    assert_eq!(
        report.truncated_bytes(),
        u64::try_from(first_torn.len()).unwrap()
    );
    assert!(fs::read(&active).unwrap().is_empty());

    let directory = tempdir().unwrap();
    let root = directory.path().join("rotated-first-torn");
    fs::create_dir(&root).unwrap();
    let closed = root.join("segment-00000000000000000001.jsonl");
    let active = root.join("segment-00000000000000000002.jsonl");
    let mut closed_bytes = events[0].encoded().to_vec();
    closed_bytes.push(b'\n');
    let rotated_torn = events[1].encoded()[..17].to_vec();
    fs::write(&closed, &closed_bytes).unwrap();
    fs::write(&active, &rotated_torn).unwrap();
    let mut writer = SegmentWriter::new(&root);
    let report = writer.recover().unwrap();
    assert_eq!(report.active_segment(), 2);
    assert_eq!(report.last_sequence(), 1);
    assert_eq!(
        report.truncated_bytes(),
        u64::try_from(rotated_torn.len()).unwrap()
    );
    assert!(fs::read(&active).unwrap().is_empty());
    assert_eq!(fs::read(&closed).unwrap(), closed_bytes);

    let directory = tempdir().unwrap();
    let root = directory.path().join("newline-corrupt-active");
    fs::create_dir(&root).unwrap();
    let active = root.join("segment-00000000000000000001.jsonl");
    let mut trusted = events[0].encoded().to_vec();
    trusted.push(b'\n');
    let mut corrupt_active = trusted.clone();
    corrupt_active.extend_from_slice(b"{\"invalid\":true}\n");
    fs::write(&active, &corrupt_active).unwrap();
    let mut writer = SegmentWriter::new(&root);
    let report = writer.recover().unwrap();
    assert_eq!(report.last_sequence(), 1);
    assert_eq!(
        report.truncated_bytes(),
        u64::try_from(corrupt_active.len() - trusted.len()).unwrap()
    );
    assert_eq!(fs::read(&active).unwrap(), trusted);

    let directory = tempdir().unwrap();
    let root = directory.path().join("corrupt-immutable");
    fs::create_dir(&root).unwrap();
    let older = root.join("segment-00000000000000000001.jsonl");
    let active = root.join("segment-00000000000000000002.jsonl");
    let mut corrupt_older = events[0].encoded().to_vec();
    corrupt_older.push(b'\n');
    corrupt_older.extend_from_slice(b"{\"invalid\":true}\n");
    let mut active_bytes = events[1].encoded().to_vec();
    active_bytes.push(b'\n');
    fs::write(&older, &corrupt_older).unwrap();
    fs::write(&active, &active_bytes).unwrap();
    let mut writer = SegmentWriter::new(&root);
    assert!(writer.recover().is_err());
    assert_eq!(fs::read(&older).unwrap(), corrupt_older);
    assert_eq!(fs::read(&active).unwrap(), active_bytes);

    let directory = tempdir().unwrap();
    let root = directory.path().join("retained-prefix-gap");
    fs::create_dir(&root).unwrap();
    let older = root.join("segment-00000000000000000005.jsonl");
    let active = root.join("segment-00000000000000000009.jsonl");
    let corrupt_older = b"{\"first_append_torn\":".to_vec();
    let mut active_bytes = events[0].encoded().to_vec();
    active_bytes.push(b'\n');
    fs::write(&older, &corrupt_older).unwrap();
    fs::write(&active, &active_bytes).unwrap();
    let mut writer = SegmentWriter::new(&root);
    assert!(writer.recover().is_err());
    assert_eq!(fs::read(&older).unwrap(), corrupt_older);
    assert_eq!(fs::read(&active).unwrap(), active_bytes);
}

#[tokio::test]
async fn recovery_allows_index_gaps_when_segment_sequences_remain_strict() {
    let events = prepared_many(2).await;
    let directory = tempdir().unwrap();
    let root = directory.path().join("valid-retained-gap");
    fs::create_dir(&root).unwrap();
    let older = root.join("segment-00000000000000000005.jsonl");
    let active = root.join("segment-00000000000000000009.jsonl");
    let mut older_bytes = events[0].encoded().to_vec();
    older_bytes.push(b'\n');
    let mut active_bytes = events[1].encoded().to_vec();
    active_bytes.push(b'\n');
    fs::write(&older, &older_bytes).unwrap();
    fs::write(&active, &active_bytes).unwrap();

    let mut writer = SegmentWriter::new(&root);
    let report = writer.recover().unwrap();
    assert_eq!(report.active_segment(), 9);
    assert_eq!(report.last_sequence(), 2);
    assert_eq!(fs::read(older).unwrap(), older_bytes);
    assert_eq!(fs::read(active).unwrap(), active_bytes);
}

#[tokio::test]
async fn recovery_rejects_an_empty_immutable_segment_without_mutating_later_files() {
    let event = prepared_many(1).await.remove(0);
    let directory = tempdir().unwrap();
    let root = directory.path().join("empty-retained-prefix");
    fs::create_dir(&root).unwrap();
    let older = root.join("segment-00000000000000000005.jsonl");
    let active = root.join("segment-00000000000000000009.jsonl");
    let mut active_bytes = event.encoded().to_vec();
    active_bytes.push(b'\n');
    fs::write(&older, b"").unwrap();
    fs::write(&active, &active_bytes).unwrap();

    let mut writer = SegmentWriter::new(&root);
    assert!(writer.recover().is_err());
    assert!(fs::read(older).unwrap().is_empty());
    assert_eq!(fs::read(active).unwrap(), active_bytes);
}

#[tokio::test]
async fn recovery_fails_closed_when_a_canonical_segment_cannot_be_opened() {
    let events = prepared_many(2).await;
    let directory = tempdir().unwrap();
    let root = directory.path().join("unopenable-canonical-segment");
    fs::create_dir(&root).unwrap();
    let older = root.join("segment-00000000000000000001.jsonl");
    let oversized = root.join("segment-00000000000000000002.jsonl");
    let active = root.join("segment-00000000000000000003.jsonl");
    let mut older_bytes = events[0].encoded().to_vec();
    older_bytes.push(b'\n');
    let oversized_bytes = vec![b'x'; wokcore_diagnostics::segment::MAX_SEGMENT_BYTES + 1];
    let mut active_bytes = events[1].encoded().to_vec();
    active_bytes.push(b'\n');
    fs::write(&older, &older_bytes).unwrap();
    fs::write(&oversized, &oversized_bytes).unwrap();
    fs::write(&active, &active_bytes).unwrap();

    let mut writer = SegmentWriter::new(&root);
    assert!(writer.recover().is_err());
    assert_eq!(fs::read(older).unwrap(), older_bytes);
    assert_eq!(fs::read(oversized).unwrap(), oversized_bytes);
    assert_eq!(fs::read(active).unwrap(), active_bytes);
}

#[tokio::test]
async fn canonical_segment_index_zero_is_never_owned_or_modified() {
    let event = prepared_many(1).await.remove(0);
    let directory = tempdir().unwrap();
    let root = directory.path().join("zero-segment");
    fs::create_dir(&root).unwrap();
    let zero = root.join("segment-00000000000000000000.jsonl");
    let mut zero_bytes = event.encoded().to_vec();
    zero_bytes.push(b'\n');
    fs::write(&zero, &zero_bytes).unwrap();

    let mut writer = SegmentWriter::new(&root);
    let report = writer.recover().unwrap();
    assert_eq!(report.active_segment(), 1);
    assert_eq!(report.last_sequence(), 0);
    assert_eq!(fs::read(&zero).unwrap(), zero_bytes);
    assert!(root.join("segment-00000000000000000001.jsonl").exists());
}

#[tokio::test]
async fn pinned_segment_and_snapshot_roots_fail_closed_after_parent_replacement() {
    let fixture = tempdir().unwrap();
    let segment_root = fixture.path().join("segments");
    let moved_segments = fixture.path().join("moved-segments");
    fs::create_dir(&segment_root).unwrap();
    let mut writer = SegmentWriter::new(&segment_root);
    writer.recover().unwrap();
    if fs::rename(&segment_root, &moved_segments).is_ok() {
        fs::create_dir(&segment_root).unwrap();
        fs::write(segment_root.join("foreign.bin"), b"foreign").unwrap();
        let mut batch = DurableBatch::new();
        batch.try_push(prepared(23).await).unwrap();
        assert!(writer.flush(batch).is_err());
        assert_eq!(
            fs::read(segment_root.join("foreign.bin")).unwrap(),
            b"foreign"
        );
        assert_eq!(
            diagnostic_entries(&segment_root).len(),
            1,
            "the replacement root must receive no diagnostic file"
        );
    }

    let snapshot_root = fixture.path().join("snapshots");
    let moved_snapshots = fixture.path().join("moved-snapshots");
    fs::create_dir(&snapshot_root).unwrap();
    let (recorder, mut owner) = SnapshotRecorder::new(&snapshot_root);
    recorder.try_request(SnapshotRequest::new(
        SnapshotCause::InternalFailure,
        SnapshotCorrelation::from_u128(24),
        prepared(24).await,
        30_000,
    ));
    assert!(owner.try_process_next().unwrap().written());
    if fs::rename(&snapshot_root, &moved_snapshots).is_ok() {
        fs::create_dir(&snapshot_root).unwrap();
        fs::write(snapshot_root.join("foreign.bin"), b"foreign").unwrap();
        recorder.try_request(SnapshotRequest::new(
            SnapshotCause::InternalFailure,
            SnapshotCorrelation::from_u128(25),
            prepared(25).await,
            30_060,
        ));
        assert!(owner.try_process_next().is_err());
        assert_eq!(
            fs::read(snapshot_root.join("foreign.bin")).unwrap(),
            b"foreign"
        );
        assert_eq!(diagnostic_entries(&snapshot_root).len(), 1);
    }
}

#[tokio::test]
async fn snapshot_queue_is_exactly_16_and_never_blocks() {
    assert_eq!(SNAPSHOT_QUEUE_CAPACITY, 16);
    assert_eq!(MAX_FAILURE_SNAPSHOTS, 10);
    assert_eq!(MAX_FAILURE_SNAPSHOT_BYTES, 2 * 1024 * 1024);
    assert_eq!(SNAPSHOT_WRITE_BUDGET_HARD_MAX, 8 * 1024 * 1024);

    let directory = tempdir().unwrap();
    let (recorder, _owner) = SnapshotRecorder::new(directory.path());
    let event = prepared(30).await;
    for _ in 0..SNAPSHOT_QUEUE_CAPACITY {
        assert_eq!(
            recorder.try_request(SnapshotRequest::new(
                SnapshotCause::UpstreamFailure,
                SnapshotCorrelation::from_u128(30),
                event.clone(),
                1_000,
            )),
            SnapshotRequestOutcome::Accepted
        );
    }
    assert_eq!(
        recorder.try_request(SnapshotRequest::new(
            SnapshotCause::UpstreamFailure,
            SnapshotCorrelation::from_u128(30),
            event,
            1_000,
        )),
        SnapshotRequestOutcome::DroppedFull
    );
    assert_eq!(recorder.metrics().queue_full(), 1);
}

#[tokio::test]
async fn snapshot_storms_coalesce_without_file_churn_and_honor_cooldown() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("snapshots");
    fs::create_dir(&root).unwrap();
    let (recorder, mut owner) = SnapshotRecorder::with_policy(
        &root,
        SnapshotPolicy::with_write_budget(SNAPSHOT_WRITE_BUDGET_HARD_MAX).unwrap(),
    );
    let event = prepared(31).await;
    assert_eq!(
        recorder.try_request(SnapshotRequest::new(
            SnapshotCause::UpstreamFailure,
            SnapshotCorrelation::from_u128(31),
            event.clone(),
            1_000,
        )),
        SnapshotRequestOutcome::Accepted
    );
    assert!(owner.try_process_next().unwrap().written());
    let first_listing = diagnostic_entries(&root).len();
    assert_eq!(first_listing, 1);

    for second in 1_001..1_060 {
        assert_eq!(
            recorder.try_request(SnapshotRequest::new(
                SnapshotCause::UpstreamFailure,
                SnapshotCorrelation::from_u128(31),
                event.clone(),
                second,
            )),
            SnapshotRequestOutcome::Accepted
        );
        assert!(owner.try_process_next().unwrap().suppressed());
    }
    assert_eq!(diagnostic_entries(&root).len(), first_listing);
    assert_eq!(recorder.metrics().cooldown_suppressed(), 59);

    assert_eq!(
        recorder.try_request(SnapshotRequest::new(
            SnapshotCause::UpstreamFailure,
            SnapshotCorrelation::from_u128(31),
            event,
            1_060,
        )),
        SnapshotRequestOutcome::Accepted
    );
    assert!(owner.try_process_next().unwrap().written());
    assert_eq!(diagnostic_entries(&root).len(), 2);
    assert_eq!(Duration::from_secs(60).as_secs(), 60);
}

#[tokio::test]
async fn canonical_snapshot_index_zero_is_never_owned_or_deleted() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("zero-snapshot");
    fs::create_dir(&root).unwrap();
    let (recorder, mut owner) = SnapshotRecorder::with_policy(
        &root,
        SnapshotPolicy::with_write_budget(SNAPSHOT_WRITE_BUDGET_HARD_MAX).unwrap(),
    );
    let event = prepared(32).await;
    assert_eq!(
        recorder.try_request(SnapshotRequest::new(
            SnapshotCause::UpstreamFailure,
            SnapshotCorrelation::from_u128(1),
            event.clone(),
            2_000,
        )),
        SnapshotRequestOutcome::Accepted
    );
    assert!(owner.try_process_next().unwrap().written());
    let first = root.join("snapshot-00000000000000000001.jsonl");
    let zero = root.join("snapshot-00000000000000000000.jsonl");
    let zero_bytes = fs::read(&first).unwrap();
    fs::write(&zero, &zero_bytes).unwrap();

    for correlation in 2..=11_u128 {
        assert_eq!(
            recorder.try_request(SnapshotRequest::new(
                SnapshotCause::UpstreamFailure,
                SnapshotCorrelation::from_u128(correlation),
                event.clone(),
                2_000,
            )),
            SnapshotRequestOutcome::Accepted
        );
        assert!(owner.try_process_next().unwrap().written());
    }

    assert_eq!(fs::read(&zero).unwrap(), zero_bytes);
}

#[tokio::test]
async fn snapshot_cooldown_uses_explicit_opaque_request_correlation() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("snapshot-correlation");
    fs::create_dir(&root).unwrap();
    let (recorder, mut owner) = SnapshotRecorder::with_policy(
        &root,
        SnapshotPolicy::with_write_budget(SNAPSHOT_WRITE_BUDGET_HARD_MAX).unwrap(),
    );
    let event = prepared(32).await;

    for correlation in [
        SnapshotCorrelation::from_u128(1),
        SnapshotCorrelation::from_u128(2),
    ] {
        assert_eq!(
            recorder.try_request(SnapshotRequest::with_correlation(
                SnapshotCause::UpstreamFailure,
                correlation,
                event.clone(),
                2_000,
            )),
            SnapshotRequestOutcome::Accepted
        );
        assert!(owner.try_process_next().unwrap().written());
    }

    assert_eq!(diagnostic_entries(&root).len(), 2);
    assert_eq!(
        recorder.try_request(SnapshotRequest::with_correlation(
            SnapshotCause::UpstreamFailure,
            SnapshotCorrelation::from_u128(2),
            event,
            2_001,
        )),
        SnapshotRequestOutcome::Accepted
    );
    assert!(owner.try_process_next().unwrap().suppressed());
    assert_eq!(diagnostic_entries(&root).len(), 2);
}

#[tokio::test]
async fn failure_snapshot_is_a_bounded_typed_causal_envelope_and_suppression_drains_once() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("typed-snapshot");
    fs::create_dir(&root).unwrap();
    for index in 0..4_100 {
        fs::write(root.join(format!("foreign-{index:04}.bin")), b"foreign").unwrap();
    }
    let events = prepared_many(2).await;
    let snapshot = FailureSnapshot::new(
        events,
        SnapshotLifecycleState::Degraded,
        SnapshotResourceState::new(true, false, 7),
        vec![
            SnapshotErrorCode::UpstreamTimeout,
            SnapshotErrorCode::StorageUnavailable,
        ],
        SnapshotRedactionSummary::new(11, 2),
        SnapshotConfigurationSummary::new(true, 7, 4, 3).unwrap(),
    )
    .unwrap();
    let correlation = SnapshotCorrelation::from_u128(0xfeed);
    let (recorder, mut owner) = SnapshotRecorder::with_policy(
        &root,
        SnapshotPolicy::with_write_budget(SNAPSHOT_WRITE_BUDGET_HARD_MAX).unwrap(),
    );
    assert_eq!(
        recorder.try_request(SnapshotRequest::with_correlation(
            SnapshotCause::InternalFailure,
            correlation,
            snapshot,
            4_000,
        )),
        SnapshotRequestOutcome::Accepted
    );
    assert!(owner.try_process_next().unwrap().written());
    let path = fs::read_dir(&root)
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| entry.file_name().to_string_lossy().starts_with("snapshot-"))
        .unwrap()
        .path();
    let bytes = fs::read(path).unwrap();
    let header: serde_json::Value =
        serde_json::from_slice(bytes.split(|byte| *byte == b'\n').next().unwrap()).unwrap();
    assert_eq!(header["kind"], "failure_snapshot");
    assert_eq!(header["cause"], "internal_failure");
    assert_eq!(header["correlation"], "0000000000000000000000000000feed");
    assert_eq!(header["lifecycle"], "degraded");
    assert_eq!(header["resources"]["memory_pressure"], true);
    assert_eq!(header["resources"]["durable_queue_depth"], 7);
    assert_eq!(header["error_chain"][0], "upstream_timeout");
    assert_eq!(header["redaction"]["removed_fields"], 11);
    assert_eq!(header["configuration"]["retention_days"], 7);
    assert_eq!(header["configuration"]["capability_version"], 3);

    assert_eq!(
        recorder.try_request(SnapshotRequest::with_correlation(
            SnapshotCause::InternalFailure,
            correlation,
            prepared(303).await,
            4_001,
        )),
        SnapshotRequestOutcome::Accepted
    );
    assert!(owner.try_process_next().unwrap().suppressed());
    let summary = owner.drain_suppression_summary().unwrap();
    assert_eq!(summary.cooldown_suppressed(), 1);
    assert_eq!(summary.total(), 1);
    assert!(owner.drain_suppression_summary().is_none());
    assert_eq!(recorder.metrics().cooldown_suppressed(), 1);
}

#[test]
fn durable_filter_and_drop_recovery_emit_only_one_typed_summary() {
    assert!(!DurableFilter::should_persist(DurableEventKind::Transient));
    for kind in [
        DurableEventKind::Warning,
        DurableEventKind::Error,
        DurableEventKind::Operational,
    ] {
        assert!(DurableFilter::should_persist(kind));
    }

    let mut tracker = DropRecoveryTracker::new();
    tracker.observe(DiagnosticDropCause::IngressFull, 3);
    tracker.observe(DiagnosticDropCause::IngressFull, 4);
    tracker.observe(DiagnosticDropCause::WriterUnavailable, 2);
    let summary = tracker.on_progress_resumed().unwrap();
    assert_eq!(summary.ingress_full(), 7);
    assert_eq!(summary.writer_unavailable(), 2);
    assert_eq!(summary.total(), 9);
    assert!(tracker.on_progress_resumed().is_none());

    tracker.observe(DiagnosticDropCause::InvalidEvent, u64::MAX);
    tracker.observe(DiagnosticDropCause::InvalidEvent, 1);
    assert_eq!(
        tracker.on_progress_resumed().unwrap().invalid_event(),
        u64::MAX
    );
}

#[tokio::test]
async fn durable_producer_closed_ingress_remains_observable_as_typed_drop_metrics() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("closed-durable-ingress");
    fs::create_dir(&root).unwrap();
    let (producer, owner) = DurableProducer::new(&root, |_| Ok::<_, ()>(()));
    drop(owner);
    let event = prepared_many(1).await.remove(0);

    assert_eq!(
        producer.try_record(event),
        DurableRecordOutcome::DroppedClosed
    );
    let metrics = producer.drop_metrics();
    assert_eq!(metrics.ingress_closed(), 1);
    assert_eq!(metrics.total(), 1);
}

#[tokio::test]
async fn durable_producer_filters_before_exact_nonblocking_queue_and_persists_one_drop_summary() {
    assert_eq!(DURABLE_QUEUE_CAPACITY, 256);
    let directory = tempdir().unwrap();
    let root = directory.path().join("durable");
    fs::create_dir(&root).unwrap();
    let regular = prepared_many(DURABLE_QUEUE_CAPACITY).await;
    let typed_counts = DiagnosticDropCounts::new(1, 0, 0, 0, 0);
    let prepared_summary = prepared_drop(301, typed_counts).await;
    let (summary_sender, summary_receiver) = std_mpsc::sync_channel(1);
    let (producer, mut owner) = DurableProducer::new(&root, move |summary| {
        summary_sender.try_send(summary).map_err(|_| ())
    });

    for event in regular.iter().cloned() {
        assert_eq!(producer.try_record(event), DurableRecordOutcome::Accepted);
    }
    assert_eq!(
        producer.try_record(prepared(302).await),
        DurableRecordOutcome::Filtered
    );
    assert_eq!(
        producer.try_record(regular[0].clone()),
        DurableRecordOutcome::DroppedFull
    );

    loop {
        match owner.try_process_next().unwrap() {
            DurableProcessOutcome::DropSummaryRequested => {
                let summary = summary_receiver.try_recv().unwrap();
                assert_eq!(summary.ingress_full(), 1);
                assert_eq!(
                    producer.try_record(prepared_summary.clone()),
                    DurableRecordOutcome::Accepted
                );
            }
            DurableProcessOutcome::Idle => break,
            DurableProcessOutcome::Written { .. } => {}
        }
    }
    let mut encoded = Vec::new();
    for entry in fs::read_dir(&root).unwrap().filter_map(Result::ok) {
        encoded.extend_from_slice(&fs::read(entry.path()).unwrap());
    }
    let decoded = encoded
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| DiagnosticEvent::decode(line).unwrap())
        .collect::<Vec<_>>();
    let drops = decoded
        .iter()
        .filter(|event| event.code() == DiagnosticEventCode::DiagnosticDrop)
        .collect::<Vec<_>>();
    assert_eq!(drops.len(), 1);
    assert_eq!(drops[0].diagnostic_drop_counts(), Some(typed_counts));
}

#[tokio::test]
async fn bounded_drop_requests_deliver_once_and_close_without_a_recorder_reference() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("bounded-drop-requests");
    fs::create_dir(&root).unwrap();
    let events = prepared_many(DURABLE_QUEUE_CAPACITY).await;
    let (producer, mut owner, mut requests): (_, BoxedDurableWriterOwner, _) =
        DurableProducer::with_drop_requests(&root);

    for event in events.iter().cloned() {
        assert_eq!(producer.try_record(event), DurableRecordOutcome::Accepted);
    }
    assert_eq!(
        producer.try_record(events[0].clone()),
        DurableRecordOutcome::DroppedFull
    );

    loop {
        match owner.try_process_next().unwrap() {
            DurableProcessOutcome::DropSummaryRequested => break,
            DurableProcessOutcome::Written { .. } => {}
            DurableProcessOutcome::Idle => panic!("drop summary was not requested"),
        }
    }
    let request = requests.recv().await.unwrap();
    let summary = request.summary();
    assert_eq!(summary.ingress_full(), 1);
    assert_eq!(summary.total(), 1);
    request.acknowledge();

    drop(owner);
    assert!(requests.recv().await.is_none());
}

#[tokio::test]
async fn unacknowledged_drop_request_restores_counts_for_retry() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("retry-unacknowledged-drop-request");
    fs::create_dir(&root).unwrap();
    let events = prepared_many(DURABLE_QUEUE_CAPACITY).await;
    let (producer, mut owner, mut requests) = DurableProducer::with_drop_requests(&root);

    for event in events.iter().cloned() {
        assert_eq!(producer.try_record(event), DurableRecordOutcome::Accepted);
    }
    assert_eq!(
        producer.try_record(events[0].clone()),
        DurableRecordOutcome::DroppedFull
    );
    loop {
        match owner.try_process_next().unwrap() {
            DurableProcessOutcome::DropSummaryRequested => break,
            DurableProcessOutcome::Written { .. } => {}
            DurableProcessOutcome::Idle => panic!("drop summary was not requested"),
        }
    }
    {
        let request = requests.recv().await.unwrap();
        assert_eq!(request.summary().ingress_full(), 1);
    }

    assert_eq!(
        owner.try_process_next().unwrap(),
        DurableProcessOutcome::DropSummaryRequested
    );
    let request = requests.recv().await.unwrap();
    assert_eq!(request.summary().ingress_full(), 1);
    request.acknowledge();
}

#[tokio::test]
async fn delayed_drop_summary_does_not_duplicate_requests_and_full_restores_all_counts() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("drop-inflight");
    fs::create_dir(&root).unwrap();
    let events = prepared_many(DURABLE_QUEUE_CAPACITY * 2).await;
    let first_summary = prepared_drop(513, DiagnosticDropCounts::new(1, 0, 0, 0, 0)).await;
    let second_summary = prepared_drop(514, DiagnosticDropCounts::new(3, 0, 0, 0, 0)).await;
    let (request_sender, request_receiver) = std_mpsc::sync_channel(2);
    let (producer, mut owner) = DurableProducer::new(&root, move |summary| {
        request_sender.try_send(summary).map_err(|_| ())
    });

    for event in events[..DURABLE_QUEUE_CAPACITY].iter().cloned() {
        assert_eq!(producer.try_record(event), DurableRecordOutcome::Accepted);
    }
    assert_eq!(
        producer.try_record(events[0].clone()),
        DurableRecordOutcome::DroppedFull
    );
    loop {
        match owner.try_process_next().unwrap() {
            DurableProcessOutcome::DropSummaryRequested => break,
            DurableProcessOutcome::Written { .. } => {}
            DurableProcessOutcome::Idle => panic!("drop summary was not requested"),
        }
    }
    assert_eq!(request_receiver.try_recv().unwrap().ingress_full(), 1);
    assert_eq!(
        owner.try_process_next().unwrap(),
        DurableProcessOutcome::Idle
    );
    assert_eq!(
        owner.try_process_next().unwrap(),
        DurableProcessOutcome::Idle
    );
    assert!(request_receiver.try_recv().is_err());

    for event in events[DURABLE_QUEUE_CAPACITY..].iter().cloned() {
        assert_eq!(producer.try_record(event), DurableRecordOutcome::Accepted);
    }
    assert_eq!(
        producer.try_record(events[DURABLE_QUEUE_CAPACITY].clone()),
        DurableRecordOutcome::DroppedFull
    );
    assert_eq!(
        producer.try_record(first_summary),
        DurableRecordOutcome::DroppedFull
    );
    loop {
        match owner.try_process_next().unwrap() {
            DurableProcessOutcome::DropSummaryRequested => break,
            DurableProcessOutcome::Written { .. } => {}
            DurableProcessOutcome::Idle => panic!("restored drop summary was not requested"),
        }
    }
    assert_eq!(request_receiver.try_recv().unwrap().ingress_full(), 3);
    assert!(request_receiver.try_recv().is_err());
    assert_eq!(
        producer.try_record(second_summary),
        DurableRecordOutcome::Accepted
    );
    assert!(matches!(
        owner.try_process_next().unwrap(),
        DurableProcessOutcome::Written { events: 1, .. }
    ));
    assert_eq!(
        owner.try_process_next().unwrap(),
        DurableProcessOutcome::Idle
    );

    let mut drop_counts = Vec::new();
    for entry in fs::read_dir(&root).unwrap().filter_map(Result::ok) {
        for line in fs::read(entry.path())
            .unwrap()
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let event = DiagnosticEvent::decode(line).unwrap();
            if let Some(counts) = event.diagnostic_drop_counts() {
                drop_counts.push(counts);
            }
        }
    }
    assert_eq!(drop_counts, [DiagnosticDropCounts::new(3, 0, 0, 0, 0)]);
}

#[tokio::test]
async fn reentrant_full_during_drop_request_cannot_leave_a_stale_inflight_flag() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("reentrant-drop-inflight");
    fs::create_dir(&root).unwrap();
    let events = prepared_many(DURABLE_QUEUE_CAPACITY * 2).await;
    let saturated_summary =
        prepared_drop(513, DiagnosticDropCounts::new(u64::MAX, 0, 0, 0, 0)).await;
    let producer_slot = Arc::new(OnceLock::<DurableProducer>::new());
    let callback_producer_slot = Arc::clone(&producer_slot);
    let callback_events = events[DURABLE_QUEUE_CAPACITY..].to_vec();
    let callback_summary = saturated_summary.clone();
    let (request_sender, request_receiver) = std_mpsc::sync_channel(2);
    let mut request_count = 0_usize;
    let (producer, mut owner) = DurableProducer::new(&root, move |summary| {
        request_sender.try_send(summary).unwrap();
        if request_count == 0 {
            let callback_producer = callback_producer_slot.get().unwrap();
            for event in callback_events.iter().cloned() {
                assert_eq!(
                    callback_producer.try_record(event),
                    DurableRecordOutcome::Accepted
                );
            }
            assert_eq!(
                callback_producer.try_record(callback_summary.clone()),
                DurableRecordOutcome::DroppedFull
            );
        }
        request_count += 1;
        Ok::<_, ()>(())
    });
    producer_slot.set(producer.clone()).unwrap();

    for event in events[..DURABLE_QUEUE_CAPACITY].iter().cloned() {
        assert_eq!(producer.try_record(event), DurableRecordOutcome::Accepted);
    }
    assert_eq!(
        producer.try_record(saturated_summary),
        DurableRecordOutcome::DroppedFull
    );

    loop {
        match owner.try_process_next().unwrap() {
            DurableProcessOutcome::DropSummaryRequested => break,
            DurableProcessOutcome::Written { .. } => {}
            DurableProcessOutcome::Idle => panic!("first drop summary was not requested"),
        }
    }
    assert_eq!(
        request_receiver.try_recv().unwrap().ingress_full(),
        u64::MAX
    );

    loop {
        match owner.try_process_next().unwrap() {
            DurableProcessOutcome::DropSummaryRequested => break,
            DurableProcessOutcome::Written { .. } => {}
            DurableProcessOutcome::Idle => {
                panic!("reentrant full left restored drop counts permanently idle")
            }
        }
    }
    assert_eq!(
        request_receiver.try_recv().unwrap().ingress_full(),
        u64::MAX
    );
}

#[tokio::test]
async fn failed_drop_request_restores_counts_and_clears_inflight_for_retry() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("failed-drop-request");
    fs::create_dir(&root).unwrap();
    let events = prepared_many(DURABLE_QUEUE_CAPACITY).await;
    let (request_sender, request_receiver) = std_mpsc::sync_channel(2);
    let (producer, mut owner) = DurableProducer::new(&root, move |summary| {
        request_sender.try_send(summary).unwrap();
        Err::<(), _>(())
    });

    for event in events.iter().cloned() {
        assert_eq!(producer.try_record(event), DurableRecordOutcome::Accepted);
    }
    assert_eq!(
        producer.try_record(events[0].clone()),
        DurableRecordOutcome::DroppedFull
    );

    loop {
        match owner.try_process_next() {
            Ok(DurableProcessOutcome::Written { .. }) => {}
            Err(DurableProcessError::DropPreparation) => break,
            outcome => panic!("unexpected first drop request outcome: {outcome:?}"),
        }
    }
    assert_eq!(request_receiver.try_recv().unwrap().ingress_full(), 1);
    assert!(matches!(
        owner.try_process_next(),
        Err(DurableProcessError::DropPreparation)
    ));
    assert_eq!(request_receiver.try_recv().unwrap().ingress_full(), 1);
    assert_eq!(producer.drop_metrics().ingress_full(), 1);
}

#[tokio::test]
async fn accepted_drop_request_defers_new_counts_until_its_event_is_written() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("accepted-drop-request");
    fs::create_dir(&root).unwrap();
    let events = prepared_many(DURABLE_QUEUE_CAPACITY * 2).await;
    let first_summary = prepared_drop(513, DiagnosticDropCounts::new(1, 0, 0, 0, 0)).await;
    let (request_sender, request_receiver) = std_mpsc::sync_channel(2);
    let (producer, mut owner) = DurableProducer::new(&root, move |summary| {
        request_sender.try_send(summary).map_err(|_| ())
    });

    for event in events[..DURABLE_QUEUE_CAPACITY].iter().cloned() {
        assert_eq!(producer.try_record(event), DurableRecordOutcome::Accepted);
    }
    assert_eq!(
        producer.try_record(events[0].clone()),
        DurableRecordOutcome::DroppedFull
    );
    loop {
        match owner.try_process_next().unwrap() {
            DurableProcessOutcome::DropSummaryRequested => break,
            DurableProcessOutcome::Written { .. } => {}
            DurableProcessOutcome::Idle => panic!("first drop summary was not requested"),
        }
    }
    assert_eq!(request_receiver.try_recv().unwrap().ingress_full(), 1);

    for event in events[DURABLE_QUEUE_CAPACITY..].iter().cloned() {
        assert_eq!(producer.try_record(event), DurableRecordOutcome::Accepted);
    }
    assert_eq!(
        producer.try_record(events[DURABLE_QUEUE_CAPACITY].clone()),
        DurableRecordOutcome::DroppedFull
    );
    loop {
        match owner.try_process_next().unwrap() {
            DurableProcessOutcome::Written { .. } => {}
            DurableProcessOutcome::Idle => break,
            DurableProcessOutcome::DropSummaryRequested => {
                panic!("new counts bypassed the accepted drop request")
            }
        }
    }
    assert!(request_receiver.try_recv().is_err());

    assert_eq!(
        producer.try_record(first_summary),
        DurableRecordOutcome::Accepted
    );
    assert!(matches!(
        owner.try_process_next().unwrap(),
        DurableProcessOutcome::Written { events: 1, .. }
    ));
    assert_eq!(
        owner.try_process_next().unwrap(),
        DurableProcessOutcome::DropSummaryRequested
    );
    assert_eq!(request_receiver.try_recv().unwrap().ingress_full(), 1);
}

#[tokio::test]
async fn durable_filter_cannot_be_spoofed_and_accepts_trusted_truncated_errors() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("typed-filter");
    fs::create_dir(&root).unwrap();
    let (producer, _owner) = DurableProducer::new(&root, move |_| Ok::<_, ()>(()));

    assert_eq!(
        producer.try_record(prepared_debug(304).await),
        DurableRecordOutcome::Filtered
    );
    assert_eq!(
        producer.try_record(prepared(305).await),
        DurableRecordOutcome::Filtered
    );
    assert_eq!(
        producer.try_record(truncated_prepared(306).await),
        DurableRecordOutcome::Accepted
    );
}

#[tokio::test]
async fn durable_startup_runs_without_events_and_failure_does_not_consume_queue() {
    let directory = tempdir().unwrap();
    let startup_root = directory.path().join("startup");
    fs::create_dir(&startup_root).unwrap();
    let (_producer, mut startup_owner) = DurableProducer::new(&startup_root, |_| Ok::<_, ()>(()));
    let first_recovery = startup_owner
        .recover_startup(std::time::SystemTime::now())
        .unwrap();
    assert_eq!(first_recovery.active_segment(), 1);
    assert_eq!(first_recovery.last_sequence(), 0);
    assert!(
        startup_root
            .join("segment-00000000000000000001.jsonl")
            .exists()
    );
    let cached_recovery = startup_owner
        .recover_startup(std::time::SystemTime::now())
        .unwrap();
    assert_eq!(cached_recovery, first_recovery);
    assert_eq!(diagnostic_entries(&startup_root).len(), 1);

    let missing_root = directory.path().join("missing");
    let (producer, mut owner) = DurableProducer::new(&missing_root, |_| Ok::<_, ()>(()));
    let mut events = prepared_many(1).await;
    let event = events.remove(0);
    assert_eq!(
        producer.try_record(event.clone()),
        DurableRecordOutcome::Accepted
    );
    assert!(owner.try_process_next().is_err());
    fs::create_dir(&missing_root).unwrap();
    assert!(matches!(
        owner.try_process_next().unwrap(),
        DurableProcessOutcome::Written { events: 1, .. }
    ));
    let bytes = fs::read(missing_root.join("segment-00000000000000000001.jsonl")).unwrap();
    assert!(bytes.starts_with(event.encoded()));
}

#[tokio::test]
async fn durable_wait_sleeps_without_writes_and_reports_closed_after_draining() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("awaitable-durable-work");
    fs::create_dir(&root).unwrap();
    let (producer, mut owner) = DurableProducer::new(&root, |_| Ok::<_, ()>(()));
    owner.recover_startup(std::time::SystemTime::now()).unwrap();
    let active = root.join("segment-00000000000000000001.jsonl");
    let before_bytes = fs::read(&active).unwrap();
    let before_modified = fs::metadata(&active).unwrap().modified().unwrap();

    assert!(
        tokio::time::timeout(Duration::from_millis(20), owner.wait_process_next())
            .await
            .is_err(),
        "an idle durable owner returned instead of waiting for work"
    );
    assert_eq!(fs::read(&active).unwrap(), before_bytes);
    assert_eq!(
        fs::metadata(&active).unwrap().modified().unwrap(),
        before_modified
    );

    let event = prepared_many(1).await.remove(0);
    assert_eq!(producer.try_record(event), DurableRecordOutcome::Accepted);
    assert!(matches!(
        owner.wait_process_next().await.unwrap(),
        DurableWorkOutcome::Written { events: 1, .. }
    ));

    drop(producer);
    assert_eq!(
        owner.wait_process_next().await.unwrap(),
        DurableWorkOutcome::Closed
    );
}

#[tokio::test]
async fn explicit_snapshot_shutdown_drains_requests_while_senders_remain_alive() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("graceful-snapshot-shutdown");
    fs::create_dir(&root).unwrap();
    let events = prepared_many(3).await;
    let (recorder, owner) = SnapshotRecorder::new(&root);
    let metrics = recorder.metrics();
    let shutdown = owner.shutdown_handle();

    for (identity, event) in events[..2].iter().cloned().enumerate() {
        assert_eq!(
            recorder.try_request(SnapshotRequest::new(
                SnapshotCause::InternalFailure,
                SnapshotCorrelation::from_u128(identity as u128 + 1),
                event,
                30_000 + u64::try_from(identity).unwrap() * 60,
            )),
            SnapshotRequestOutcome::Accepted
        );
    }
    let owner_task = tokio::spawn(owner.run());
    shutdown.request();

    tokio::time::timeout(Duration::from_secs(1), owner_task)
        .await
        .expect("snapshot owner did not stop while producer handles remained")
        .unwrap();
    assert_eq!(metrics.written(), 2);
    assert_eq!(
        recorder.try_request(SnapshotRequest::new(
            SnapshotCause::InternalFailure,
            SnapshotCorrelation::from_u128(3),
            events[2].clone(),
            30_120,
        )),
        SnapshotRequestOutcome::DroppedClosed
    );
}

#[tokio::test]
async fn explicit_snapshot_shutdown_rejects_new_work_before_owner_poll() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("snapshot-stop-before-poll");
    fs::create_dir(&root).unwrap();
    let event = prepared_many(1).await.remove(0);
    let (recorder, owner) = SnapshotRecorder::new(&root);
    let shutdown = owner.shutdown_handle();

    shutdown.request();

    assert_eq!(
        recorder.try_request(SnapshotRequest::new(
            SnapshotCause::InternalFailure,
            SnapshotCorrelation::from_u128(1),
            event,
            31_000,
        )),
        SnapshotRequestOutcome::DroppedClosed
    );
    tokio::time::timeout(Duration::from_secs(1), owner.run())
        .await
        .unwrap();
    assert_eq!(recorder.metrics().queue_closed(), 1);
    assert!(!root.join("snapshot-00000000000000000001.jsonl").exists());
}

#[tokio::test]
async fn snapshots_retain_ten_and_enforce_default_and_hard_write_budgets() {
    assert_eq!(SNAPSHOT_WRITE_BUDGET_DEFAULT, 4 * 1024 * 1024);
    assert!(SnapshotPolicy::with_write_budget(SNAPSHOT_WRITE_BUDGET_HARD_MAX + 1).is_err());

    let directory = tempdir().unwrap();
    let root = directory.path().join("snapshots");
    fs::create_dir(&root).unwrap();
    let (recorder, mut owner) = SnapshotRecorder::new(&root);
    for index in 0..12 {
        let event = prepared(100 + index).await;
        assert_eq!(
            recorder.try_request(SnapshotRequest::new(
                SnapshotCause::InternalFailure,
                SnapshotCorrelation::from_u128(u128::from(index)),
                event,
                10_000 + index * 60,
            )),
            SnapshotRequestOutcome::Accepted
        );
        assert!(owner.try_process_next().unwrap().written());
    }
    let files = diagnostic_entries(&root).len();
    assert_eq!(files, MAX_FAILURE_SNAPSHOTS);
    for entry in diagnostic_entries(&root) {
        assert!(
            usize::try_from(entry.metadata().unwrap().len()).unwrap() <= MAX_FAILURE_SNAPSHOT_BYTES
        );
    }

    let budget_root = directory.path().join("budget");
    fs::create_dir(&budget_root).unwrap();
    let event = prepared(200).await;
    let one_snapshot = FailureSnapshot::from(event.clone()).encoded_len();
    let (budget_recorder, mut budget_owner) = SnapshotRecorder::with_policy(
        &budget_root,
        SnapshotPolicy::with_write_budget(one_snapshot).unwrap(),
    );
    for (identity, event) in [(1_u64, event), (2, prepared(201).await)] {
        assert_eq!(
            budget_recorder.try_request(SnapshotRequest::new(
                SnapshotCause::StorageFailure,
                SnapshotCorrelation::from_u128(u128::from(identity)),
                event,
                20_000 + identity,
            )),
            SnapshotRequestOutcome::Accepted
        );
    }
    assert!(budget_owner.try_process_next().unwrap().written());
    assert!(budget_owner.try_process_next().unwrap().suppressed());
    assert_eq!(budget_recorder.metrics().budget_suppressed(), 1);
    assert_eq!(diagnostic_entries(&budget_root).len(), 1);
}

#[tokio::test]
async fn snapshot_budget_is_a_true_rolling_minute() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("rolling-budget");
    fs::create_dir(&root).unwrap();
    let events = [
        prepared(210).await,
        prepared(211).await,
        prepared(212).await,
        prepared(213).await,
    ];
    let snapshot_bytes = FailureSnapshot::from(events[0].clone()).encoded_len();
    assert!(
        events
            .iter()
            .all(|event| FailureSnapshot::from(event.clone()).encoded_len() == snapshot_bytes)
    );
    let (recorder, mut owner) = SnapshotRecorder::with_policy(
        &root,
        SnapshotPolicy::with_write_budget(snapshot_bytes * 2).unwrap(),
    );

    for (identity, (event, observed_at)) in events
        .into_iter()
        .zip([1_000, 1_059, 1_060, 1_060])
        .enumerate()
    {
        assert_eq!(
            recorder.try_request(SnapshotRequest::new(
                SnapshotCause::StorageFailure,
                SnapshotCorrelation::from_u128(identity as u128),
                event,
                observed_at,
            )),
            SnapshotRequestOutcome::Accepted
        );
    }

    assert!(owner.try_process_next().unwrap().written());
    assert!(owner.try_process_next().unwrap().written());
    assert!(owner.try_process_next().unwrap().written());
    assert!(owner.try_process_next().unwrap().suppressed());
    assert_eq!(recorder.metrics().budget_suppressed(), 1);
}
