use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use tempfile::tempdir;
use tokio::time::{sleep, timeout};
use wokcore_diagnostics::{
    event::{
        BuildIdentity, CapabilityVersion, DiagnosticBuildError, DiagnosticComponent,
        DiagnosticEvent, DiagnosticEventCode, DiagnosticEventDraft, DiagnosticLevel, EventId,
        GitCommit, UtcTimestamp, WokcoreVersion,
    },
    recorder::RecordOutcome,
};
use wokcore_server::{
    auth::{EntropySource, TokenError},
    observability::{
        PreparedDiagnosticWriter, PreparedStateWriter, REQUEST_METRIC_BATCH_ROWS,
        REQUEST_METRIC_FLUSH_INTERVAL, ScanTimestampSource,
    },
};
use wokcore_storage::{ProviderMetadataBatch, RequestMetric, StateStoreWriterSubmitError};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

struct FixedEntropy;

impl EntropySource for FixedEntropy {
    fn fill(&self, output: &mut [u8; 32]) -> Result<(), TokenError> {
        *output = [0x5a; 32];
        Ok(())
    }
}

struct FixedTimestamp;

impl ScanTimestampSource for FixedTimestamp {
    fn now(&self) -> Option<String> {
        Some("2026-07-27T00:00:00Z".to_owned())
    }
}

fn event(identity: u64) -> Result<DiagnosticEventDraft, DiagnosticBuildError> {
    Ok(DiagnosticEventDraft::new(
        EventId::parse(&format!("018f47a2-4c1d-7a8f-9b2d-{identity:012x}"))?,
        UtcTimestamp::parse("2026-07-27T00:00:00Z")?,
        DiagnosticLevel::Warn,
        DiagnosticComponent::Diagnostics,
        DiagnosticEventCode::RequestFailed,
        BuildIdentity::new(
            WokcoreVersion::parse("0.1.0")?,
            GitCommit::parse("0123456789abcdef0123456789abcdef01234567")?,
            1,
            CapabilityVersion::new(1),
        ),
    ))
}

#[derive(Debug, Eq, PartialEq)]
struct FileFingerprint {
    path: PathBuf,
    bytes: u64,
    modified: Option<SystemTime>,
}

fn fingerprints(root: &Path) -> Vec<FileFingerprint> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let mut entries = fs::read_dir(directory)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let metadata = entry.metadata().unwrap();
            if metadata.is_dir() {
                pending.push(entry.path());
            } else {
                files.push(FileFingerprint {
                    path: entry.path().strip_prefix(root).unwrap().to_path_buf(),
                    bytes: metadata.len(),
                    modified: metadata.modified().ok(),
                });
            }
        }
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    files
}

fn open_writer(root: &Path) -> wokcore_server::observability::RunningDiagnosticWriter {
    let (_, prepared) =
        PreparedDiagnosticWriter::open(root, Arc::new(FixedEntropy), Arc::new(FixedTimestamp))
            .unwrap();
    prepared.start().unwrap()
}

fn durable_drop_events(root: &Path) -> usize {
    let mut drop_events = 0_usize;
    for entry in fs::read_dir(root).unwrap().filter_map(Result::ok) {
        if !entry.file_name().to_string_lossy().starts_with("segment-") {
            continue;
        }
        let Ok(bytes) = fs::read(entry.path()) else {
            continue;
        };
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            if DiagnosticEvent::decode(line)
                .is_ok_and(|event| event.code() == DiagnosticEventCode::DiagnosticDrop)
            {
                drop_events += 1;
            }
        }
    }
    drop_events
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_diagnostic_writer_performs_zero_file_writes() {
    let fixture = tempdir().unwrap();
    let root = fixture.path().join("diagnostics");
    let running = open_writer(&root);
    let before = fingerprints(&root);

    sleep(Duration::from_millis(350)).await;

    assert_eq!(fingerprints(&root), before);
    timeout(TEST_TIMEOUT, running.shutdown())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_flushes_accepted_event_even_when_handle_clones_remain() {
    let fixture = tempdir().unwrap();
    let root = fixture.path().join("diagnostics");
    let (_, prepared) =
        PreparedDiagnosticWriter::open(&root, Arc::new(FixedEntropy), Arc::new(FixedTimestamp))
            .unwrap();
    let running = prepared.start().unwrap();
    let retained = running.handle();

    assert_eq!(
        retained.recorder().try_record(event(1)),
        RecordOutcome::Accepted
    );
    timeout(TEST_TIMEOUT, running.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        retained.recorder().try_record(event(2)),
        RecordOutcome::DroppedClosed
    );

    let persisted = fs::read_dir(&root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("segment-"))
        .flat_map(|entry| fs::read(entry.path()).unwrap())
        .collect::<Vec<_>>();
    assert!(!persisted.is_empty());
    assert!(
        String::from_utf8(persisted)
            .unwrap()
            .contains("018f47a2-4c1d-7a8f-9b2d-000000000001")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn high_pressure_shutdown_persists_the_durable_drop_summary_fixed_point() {
    let fixture = tempdir().unwrap();
    let root = fixture.path().join("diagnostics");
    let (_, prepared) =
        PreparedDiagnosticWriter::open(&root, Arc::new(FixedEntropy), Arc::new(FixedTimestamp))
            .unwrap();
    let running = prepared.start().unwrap();
    let retained = running.handle();
    let mut accepted = 0_u64;
    while accepted < 4_096 {
        match retained.recorder().try_record(event(accepted + 1)) {
            RecordOutcome::Accepted => accepted += 1,
            RecordOutcome::DroppedFull => tokio::task::yield_now().await,
            outcome => panic!("unexpected diagnostic admission outcome: {outcome:?}"),
        }
    }

    timeout(Duration::from_secs(15), running.shutdown())
        .await
        .unwrap()
        .unwrap();

    assert!(retained.recorder().metrics().durable_full() > 0);
    assert!(durable_drop_events(&root) > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_diagnostic_shutdown_finishes_drop_summary_cleanup_in_the_background() {
    let fixture = tempdir().unwrap();
    let root = fixture.path().join("diagnostics");
    let (_, prepared) =
        PreparedDiagnosticWriter::open(&root, Arc::new(FixedEntropy), Arc::new(FixedTimestamp))
            .unwrap();
    let running = prepared.start().unwrap();
    let retained = running.handle();
    let mut accepted = 0_u64;
    while accepted < 4_096 {
        match retained.recorder().try_record(event(accepted + 1)) {
            RecordOutcome::Accepted => accepted += 1,
            RecordOutcome::DroppedFull => tokio::task::yield_now().await,
            outcome => panic!("unexpected diagnostic admission outcome: {outcome:?}"),
        }
    }
    let shutdown = tokio::spawn(running.shutdown());
    shutdown.abort();
    let _ = shutdown.await;

    timeout(Duration::from_secs(15), async {
        loop {
            if durable_drop_events(&root) > 0 {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(retained.recorder().metrics().durable_full() > 0);
    assert_eq!(
        retained.recorder().try_record(event(5_000)),
        RecordOutcome::DroppedClosed
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_state_writer_performs_zero_file_writes_and_shutdown_ignores_live_clients() {
    let fixture = tempdir().unwrap();
    let state_path = fixture.path().join("state.sqlite3");
    let (handle, prepared) = PreparedStateWriter::open(&state_path).unwrap();
    let running = prepared.start().unwrap();
    let retained = handle.clone();
    let before = fingerprints(fixture.path());

    sleep(Duration::from_millis(350)).await;

    assert_eq!(fingerprints(fixture.path()), before);
    timeout(TEST_TIMEOUT, running.checkpoint_and_shutdown(false))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        retained.client().try_execute(|_| Ok(())),
        Err(StateStoreWriterSubmitError::WriterClosed)
    ));
}

fn request_metric(identity: usize) -> RequestMetric {
    RequestMetric {
        request_id: format!("019844f0-4de0-7000-8000-{identity:012}"),
        provider_id: "synthetic-provider".to_owned(),
        model: "synthetic-model".to_owned(),
        started_at: "2026-07-27T00:00:00Z".to_owned(),
        latency_ms: 5,
        input_tokens: Some(2),
        output_tokens: Some(3),
        status_code: 200,
        error_code: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_metrics_flush_by_count_or_elapsed_time_and_never_write_per_request() {
    let fixture = tempdir().unwrap();
    let state_path = fixture.path().join("state.sqlite3");
    let (handle, prepared) = PreparedStateWriter::open(&state_path).unwrap();
    let running = prepared.start().unwrap();
    let initial_revision = handle.client().activity_revision();

    for identity in 0..REQUEST_METRIC_BATCH_ROWS - 1 {
        handle.observe_request_metric(request_metric(identity), || {
            panic!("metadata snapshot must be deferred until the batch is due")
        });
    }
    assert_eq!(handle.client().activity_revision(), initial_revision);

    handle.observe_request_metric(request_metric(REQUEST_METRIC_BATCH_ROWS), || {
        Some(ProviderMetadataBatch::default())
    });
    assert!(handle.client().activity_revision() > initial_revision);
    running.flush().await.unwrap();

    let after_count_flush = handle.client().activity_revision();
    handle.observe_request_metric(request_metric(10_000), || {
        panic!("the first partial row must remain memory-only")
    });
    assert_eq!(handle.client().activity_revision(), after_count_flush);
    sleep(REQUEST_METRIC_FLUSH_INTERVAL + Duration::from_millis(25)).await;
    handle.observe_request_metric(request_metric(10_001), || {
        Some(ProviderMetadataBatch::default())
    });
    assert!(handle.client().activity_revision() > after_count_flush);

    running.checkpoint_and_shutdown(false).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_state_shutdown_still_closes_the_single_writer() {
    let fixture = tempdir().unwrap();
    let state_path = fixture.path().join("state.sqlite3");
    let (handle, prepared) = PreparedStateWriter::open(&state_path).unwrap();
    let running = prepared.start().unwrap();
    let retained = handle.clone();
    let shutdown = tokio::spawn(running.checkpoint_and_shutdown(false));
    shutdown.abort();
    let _ = shutdown.await;

    timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                retained.client().try_execute(|_| Ok(())),
                Err(StateStoreWriterSubmitError::WriterClosed)
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
