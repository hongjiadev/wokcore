use rusqlite::Connection;
use tokio::time::{Duration, timeout};
use wokcore_storage::{
    AttemptId, OpaqueFingerprint, RequestId, RequestMetric, RequestSupplementalMetadata,
    STATE_STORE_WRITER_QUEUE_CAPACITY, SessionBatch, StateStore, StateStoreWriterSubmitError,
    SupplementalFailoverDecision, SupplementalRetryDecision, TraceId, state_store_writer,
};

fn metric(request_id: &str) -> RequestMetric {
    RequestMetric {
        request_id: request_id.to_owned(),
        provider_id: "provider-1".to_owned(),
        model: "model-1".to_owned(),
        started_at: "2026-07-27T00:00:00Z".to_owned(),
        latency_ms: 125,
        input_tokens: Some(10),
        output_tokens: Some(20),
        status_code: 200,
        error_code: None,
    }
}

fn opaque(byte: u8) -> String {
    format!("{byte:02x}").repeat(32)
}

fn supplemental(request_id: &str, occurred_at: &str) -> RequestSupplementalMetadata {
    RequestSupplementalMetadata {
        request_id: RequestId::new(request_id).unwrap(),
        attempt_id: AttemptId::new(format!("attempt-{request_id}")).unwrap(),
        trace_id: TraceId::new(format!("trace-{request_id}")).unwrap(),
        occurred_at: occurred_at.to_owned(),
        route_fingerprint: OpaqueFingerprint::new(opaque(1)).unwrap(),
        provider_fingerprint: OpaqueFingerprint::new(opaque(2)).unwrap(),
        account_fingerprint: None,
        retry_decision: SupplementalRetryDecision::None,
        failover_decision: SupplementalFailoverDecision::None,
        queue_ms: 0,
        connect_ms: 0,
        first_byte_ms: 0,
        total_ms: 0,
        request_bytes: 0,
        response_bytes: 0,
        status_code: Some(200),
        error_code: None,
    }
}

fn request_metric_count(path: &std::path::Path) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM request_metrics", [], |row| row.get(0))
        .unwrap()
}

fn wal_size(path: &std::path::Path) -> u64 {
    std::fs::metadata(format!("{}-wal", path.display()))
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

#[test]
fn blocking_workers_can_wait_for_the_single_writer_without_an_async_runtime() {
    let directory = tempfile::tempdir().unwrap();
    let store = StateStore::open(directory.path().join("state.db")).unwrap();
    let (client, shutdown, writer) = state_store_writer(store);
    let writer_thread = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(writer.run());
    });
    let receipt = client.try_execute(|_| Ok(42_u8)).unwrap();

    assert_eq!(receipt.blocking_wait().unwrap(), 42);

    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            shutdown.shutdown().await.unwrap().wait().await.unwrap();
        });
    writer_thread.join().unwrap();
}

#[tokio::test]
async fn session_batch_command_uses_the_existing_bounded_commit_path() {
    let directory = tempfile::tempdir().unwrap();
    let store = StateStore::open(directory.path().join("state.db")).unwrap();
    let (client, _shutdown, mut writer) = state_store_writer(store);
    let receipt = client
        .try_commit_session_batch(SessionBatch::default())
        .unwrap();

    assert!(writer.run_one().await);
    let outcome = receipt.wait().await.unwrap();

    assert_eq!(outcome.inserted_rows, 0);
    assert_eq!(outcome.dropped_rows, 0);
}

#[tokio::test]
async fn writer_queue_holds_exactly_four_commands() {
    let directory = tempfile::tempdir().unwrap();
    let store = StateStore::open(directory.path().join("state.db")).unwrap();
    let (client, _shutdown, mut writer) = state_store_writer(store);
    let mut receipts = Vec::new();

    for value in 0..STATE_STORE_WRITER_QUEUE_CAPACITY {
        receipts.push(
            client
                .try_execute(move |_| Ok(value))
                .expect("the documented queue capacity accepts the command"),
        );
    }
    let rejected = client.try_execute(|_| Ok(()));

    assert_eq!(STATE_STORE_WRITER_QUEUE_CAPACITY, 4);
    assert!(matches!(
        rejected,
        Err(StateStoreWriterSubmitError::QueueFull)
    ));

    for (expected, receipt) in receipts.into_iter().enumerate() {
        assert!(writer.run_one().await);
        assert_eq!(receipt.wait().await.unwrap(), expected);
    }
}

#[tokio::test]
async fn store_is_not_written_until_the_unique_writer_runs_the_command() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let store = StateStore::open(&path).unwrap();
    let (client, _shutdown, mut writer) = state_store_writer(store);
    let receipt = client
        .try_execute(|store| store.record_request_metrics(&[metric("actor-owned")]))
        .unwrap();

    assert_eq!(request_metric_count(&path), 0);

    assert!(writer.run_one().await);
    receipt.wait().await.unwrap();

    assert_eq!(request_metric_count(&path), 1);
}

#[tokio::test]
async fn activity_revision_covers_command_admission_and_completion() {
    let directory = tempfile::tempdir().unwrap();
    let store = StateStore::open(directory.path().join("state.db")).unwrap();
    let (client, _shutdown, mut writer) = state_store_writer(store);
    assert_eq!(client.activity_revision(), 0);

    let receipt = client.try_execute(|_| Ok(())).unwrap();
    assert_eq!(client.activity_revision(), 1);

    assert!(writer.run_one().await);
    receipt.wait().await.unwrap();
    assert_eq!(client.activity_revision(), 2);
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(client.has_been_idle_for(Duration::from_millis(1)));

    let receipt = client.try_execute(|_| Ok(())).unwrap();
    assert!(!client.has_been_idle_for(Duration::from_secs(1)));
    assert!(writer.run_one().await);
    receipt.wait().await.unwrap();
}

#[tokio::test]
async fn flush_barrier_does_not_reset_writer_idle_activity() {
    let directory = tempfile::tempdir().unwrap();
    let store = StateStore::open(directory.path().join("state.db")).unwrap();
    let (client, _shutdown, mut writer) = state_store_writer(store);
    let write = client.try_execute(|_| Ok(())).unwrap();
    assert!(writer.run_one().await);
    write.wait().await.unwrap();
    let revision = client.activity_revision();
    tokio::time::sleep(Duration::from_millis(5)).await;

    let flushed = client.try_flush().unwrap();
    assert!(writer.run_one().await);
    flushed.wait().await.unwrap();

    assert_eq!(client.activity_revision(), revision);
    assert!(client.has_been_idle_for(Duration::from_millis(1)));
}

#[tokio::test]
async fn truncate_rechecks_idle_revision_inside_the_writer() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let store = StateStore::open(&path).unwrap();
    let (client, _shutdown, mut writer) = state_store_writer(store);
    let seed = client
        .try_execute(|store| store.record_request_metrics(&[metric("seed")]))
        .unwrap();
    assert!(writer.run_one().await);
    seed.wait().await.unwrap();
    assert!(wal_size(&path) > 0);
    tokio::time::sleep(Duration::from_millis(5)).await;

    let stale_checkpoint = client
        .try_checkpoint(Some(Duration::from_millis(1)))
        .unwrap();
    let later_activity = client.try_execute(|_| Ok(())).unwrap();
    assert!(writer.run_one().await);
    stale_checkpoint.wait().await.unwrap();
    assert!(wal_size(&path) > 0);
    assert!(writer.run_one().await);
    later_activity.wait().await.unwrap();

    tokio::time::sleep(Duration::from_millis(5)).await;
    let eligible_checkpoint = client
        .try_checkpoint(Some(Duration::from_millis(1)))
        .unwrap();
    assert!(writer.run_one().await);
    eligible_checkpoint.wait().await.unwrap();
    assert_eq!(wal_size(&path), 0);
}

#[tokio::test]
async fn supplemental_cleanup_is_piggybacked_once_without_an_idle_timer() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = StateStore::open(directory.path().join("state.db")).unwrap();
    store
        .record_request_supplemental_batch(&[supplemental(
            "old-before-first",
            "2026-07-25T00:00:00Z",
        )])
        .unwrap();
    let (client, shutdown, writer) = state_store_writer(store);
    let writer_task = tokio::spawn(writer.run());

    client
        .try_record_request_supplemental_batch(vec![supplemental(
            "new-first",
            "2026-07-27T00:00:00Z",
        )])
        .unwrap()
        .wait()
        .await
        .unwrap();
    client
        .try_execute(|store| {
            store
                .record_request_supplemental_batch(&[supplemental(
                    "old-after-first",
                    "2026-07-25T00:00:00Z",
                )])
                .map(|_| ())
        })
        .unwrap()
        .wait()
        .await
        .unwrap();
    client
        .try_record_request_supplemental_batch(vec![supplemental(
            "new-second",
            "2026-07-27T00:00:01Z",
        )])
        .unwrap()
        .wait()
        .await
        .unwrap();
    let stats = client
        .try_execute(|store| store.inspect_request_supplemental())
        .unwrap()
        .wait()
        .await
        .unwrap();

    assert_eq!(stats.rows, 3);
    shutdown.shutdown().await.unwrap().wait().await.unwrap();
    writer_task.await.unwrap();
}

#[tokio::test]
async fn shutdown_closes_live_clients_drains_accepted_commands_and_acknowledges_flush() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let store = StateStore::open(&path).unwrap();
    let (client, shutdown, writer) = state_store_writer(store);
    let live_clone = client.clone();
    let write = client
        .try_execute(|store| store.record_request_metrics(&[metric("before-shutdown")]))
        .unwrap();
    let flushed = client.try_flush().unwrap();
    let writer_task = tokio::spawn(writer.run());

    let stopped = shutdown.shutdown().await.unwrap();

    write.wait().await.unwrap();
    flushed.wait().await.unwrap();
    stopped.wait().await.unwrap();
    timeout(Duration::from_secs(1), writer_task)
        .await
        .expect("writer exits after draining despite a live client clone")
        .unwrap();

    assert_eq!(request_metric_count(&path), 1);
    assert!(matches!(
        live_clone.try_execute(|_| Ok(())),
        Err(StateStoreWriterSubmitError::WriterClosed)
    ));
}
