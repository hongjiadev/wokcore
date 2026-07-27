use std::{
    fs,
    path::Path,
    sync::{Arc, Barrier},
    thread,
};

use wokcore_core::secret::SecretRef;
use wokcore_storage::{RequestMetric, StateStore, StorageError, WAL_CHECKPOINT_THRESHOLD_BYTES};

fn metric(request_id: &str) -> RequestMetric {
    RequestMetric {
        request_id: request_id.to_owned(),
        provider_id: "provider-1".to_owned(),
        model: "model-1".to_owned(),
        started_at: "2026-07-25T00:00:00Z".to_owned(),
        latency_ms: 125,
        input_tokens: Some(10),
        output_tokens: Some(20),
        status_code: 200,
        error_code: None,
    }
}

fn row_count(path: &Path, table: &str) -> i64 {
    let connection = rusqlite::Connection::open(path).unwrap();
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[test]
fn first_open_applies_initial_schema_and_disables_automatic_checkpointing() {
    let directory = tempfile::tempdir().unwrap();
    let store = StateStore::open(directory.path().join("state.db")).unwrap();

    assert_eq!(store.health().unwrap().schema_version, 4);
    assert_eq!(store.pragma_foreign_keys().unwrap(), 1);
    assert_eq!(store.pragma_journal_mode().unwrap(), "wal");
    assert_eq!(store.pragma_busy_timeout().unwrap(), 5_000);
    assert_eq!(store.pragma_wal_autocheckpoint().unwrap(), 0);
    assert_eq!(WAL_CHECKPOINT_THRESHOLD_BYTES, 16 * 1024 * 1024);
}

#[test]
fn concurrent_first_opens_commit_exactly_one_valid_initial_migration() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let barrier = Arc::new(Barrier::new(2));

    let handles = (0..2)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            let path = path.clone();
            thread::spawn(move || {
                barrier.wait();
                StateStore::open(path).unwrap().health().unwrap()
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        assert_eq!(handle.join().unwrap().schema_version, 4);
    }

    let connection = rusqlite::Connection::open(path).unwrap();
    let versions = connection
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    let tables = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('accounts', 'request_metrics', 'orphan_secrets', 'provider_runtime_metadata', 'account_runtime_metadata')",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();

    assert_eq!(versions, 4);
    assert_eq!(tables, 5);
}

#[test]
fn corrupt_database_is_reported_without_changing_original_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let original = b"not a sqlite database";
    fs::write(&path, original).unwrap();

    let error = StateStore::open(&path).unwrap_err();

    assert!(matches!(error, StorageError::StateDatabaseCorrupt { .. }));
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn empty_metric_batch_creates_no_row_and_does_not_grow_the_wal() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let mut store = StateStore::open(&path).unwrap();
    let wal_bytes_before = store.wal_size_bytes().unwrap();

    store.record_request_metrics(&[]).unwrap();

    assert_eq!(row_count(&path, "request_metrics"), 0);
    assert_eq!(store.wal_size_bytes().unwrap(), wal_bytes_before);
}

#[test]
fn one_batch_persists_all_request_metadata_and_token_totals() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let mut store = StateStore::open(&path).unwrap();
    let mut second = metric("request-2");
    second.provider_id = "provider-2".to_owned();
    second.model = "model-2".to_owned();
    second.latency_ms = 250;
    second.input_tokens = None;
    second.output_tokens = Some(30);
    second.status_code = 429;
    second.error_code = Some("rate_limit".to_owned());

    store
        .record_request_metrics(&[metric("request-1"), second])
        .unwrap();

    let connection = rusqlite::Connection::open(path).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT request_id, provider_id, model, started_at, latency_ms, input_tokens, output_tokens, status_code, error_code FROM request_metrics ORDER BY request_id",
        )
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, Option<String>>(8)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(
        rows,
        vec![
            (
                "request-1".to_owned(),
                "provider-1".to_owned(),
                "model-1".to_owned(),
                "2026-07-25T00:00:00Z".to_owned(),
                125,
                Some(10),
                Some(20),
                200,
                None,
            ),
            (
                "request-2".to_owned(),
                "provider-2".to_owned(),
                "model-2".to_owned(),
                "2026-07-25T00:00:00Z".to_owned(),
                250,
                None,
                Some(30),
                429,
                Some("rate_limit".to_owned()),
            ),
        ]
    );
}

#[test]
fn duplicate_row_rolls_back_the_whole_metric_batch() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let mut store = StateStore::open(&path).unwrap();

    let error = store
        .record_request_metrics(&[metric("duplicate"), metric("duplicate")])
        .unwrap_err();

    assert!(matches!(error, StorageError::StateDatabase { .. }));
    assert_eq!(row_count(&path, "request_metrics"), 0);
}

#[test]
fn request_schema_contains_only_metadata_and_token_total_columns() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let _store = StateStore::open(&path).unwrap();
    let connection = rusqlite::Connection::open(path).unwrap();
    let mut statement = connection
        .prepare("PRAGMA table_info(request_metrics)")
        .unwrap();
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(
        columns,
        [
            "request_id",
            "provider_id",
            "model",
            "started_at",
            "latency_ms",
            "input_tokens",
            "output_tokens",
            "status_code",
            "error_code",
        ]
    );

    let migration = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations/0001_initial.sql"),
    )
    .unwrap()
    .to_ascii_lowercase();
    for forbidden in [
        "prompt",
        "response_body",
        "tool_payload",
        "sse_chunk",
        "authorization",
        "cookie",
        "session_body",
    ] {
        assert!(
            !migration.contains(forbidden),
            "migration must not contain forbidden state field {forbidden}"
        );
    }
}

#[test]
fn orphan_secret_metadata_remains_available_for_recovery_writes() {
    let directory = tempfile::tempdir().unwrap();
    let store = StateStore::open(directory.path().join("state.db")).unwrap();
    let secret_ref = SecretRef::parse("secret:00000000-0000-0000-0000-000000000001").unwrap();

    store
        .record_orphan_secret(&secret_ref, "2026-07-25T00:00:00Z")
        .unwrap();

    assert_eq!(store.orphan_secret_refs().unwrap(), vec![secret_ref]);
}

#[test]
fn passive_checkpoint_runs_only_at_the_injected_wal_threshold() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = StateStore::open(directory.path().join("state.db")).unwrap();
    store
        .record_request_metrics(&[metric("checkpoint-passive")])
        .unwrap();
    let wal_bytes = store.wal_size_bytes().unwrap();
    assert!(wal_bytes > 0);

    let skipped = store.checkpoint_passive_if_at_least(wal_bytes + 1).unwrap();
    assert_eq!(skipped, None);
    assert_eq!(store.wal_size_bytes().unwrap(), wal_bytes);

    let completed = store
        .checkpoint_passive_if_at_least(wal_bytes)
        .unwrap()
        .unwrap();
    assert!(!completed.busy);
    assert!(completed.log_frames > 0);
    assert_eq!(completed.checkpointed_frames, completed.log_frames);
}

#[test]
fn truncate_checkpoint_is_explicit_and_reduces_an_idle_wal() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = StateStore::open(directory.path().join("state.db")).unwrap();
    store
        .record_request_metrics(&[metric("checkpoint-truncate")])
        .unwrap();
    let wal_bytes_before = store.wal_size_bytes().unwrap();
    assert!(wal_bytes_before > 0);

    let completed = store.checkpoint_truncate().unwrap();

    assert!(!completed.busy);
    assert!(store.wal_size_bytes().unwrap() < wal_bytes_before);
}

#[test]
fn state_store_has_no_singular_write_or_background_checkpoint_loop() {
    let state_directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/state");
    let source = fs::read_to_string(state_directory.join("store.rs")).unwrap();
    let lower = source.to_ascii_lowercase();

    assert!(!source.contains("record_request_metric("));
    for forbidden in [
        "heartbeat",
        "watcher",
        "tokio::time",
        "std::thread",
        "spawn(",
        "async fn",
    ] {
        assert!(
            !lower.contains(forbidden),
            "state store must not contain background mechanism {forbidden}"
        );
    }

    let batch_start = source
        .find("pub fn record_request_metrics")
        .expect("batch write API");
    let after_batch = &source[batch_start + 1..];
    let batch_end = after_batch
        .find("\n    pub fn ")
        .map_or(source.len(), |offset| batch_start + 1 + offset);
    assert!(!source[batch_start..batch_end].contains("checkpoint"));
}
