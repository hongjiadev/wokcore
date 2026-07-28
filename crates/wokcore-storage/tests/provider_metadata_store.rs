use std::{fs, path::Path};

use wokcore_core::id::{AccountId, ProviderId};
use wokcore_storage::{
    AccountRuntimeHealth, AccountRuntimeMetadata, ProviderMetadataBatch,
    ProviderMetadataBatchOutcome, ProviderRuntimeMetadata, StateStore, StorageError,
    state_store_writer,
};

#[test]
fn schema_four_replaces_raw_affinity_tables_and_preserves_unrelated_state() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("state.db");
    {
        let connection = rusqlite::Connection::open(&path).expect("connection");
        connection
            .execute_batch(include_str!("../migrations/0001_initial.sql"))
            .expect("schema one");
        connection
            .execute_batch(include_str!("../migrations/0002_runtime_auth.sql"))
            .expect("schema two");
        connection
            .execute_batch(include_str!("../migrations/0003_session_diagnostics.sql"))
            .expect("schema three");
        connection
            .execute(
                "INSERT INTO accounts(id, provider_id, display_name, auth_state)
                 VALUES ('legacy-account', 'legacy-provider', 'Legacy', 'ready')",
                [],
            )
            .expect("legacy account");
        connection
            .execute(
                "INSERT INTO thread_affinities(thread_key, account_id, updated_at)
                 VALUES ('raw-thread-key-must-disappear', 'legacy-account', '2026-07-27T00:00:00Z')",
                [],
            )
            .expect("legacy affinity");
    }

    let store = StateStore::open(&path).expect("migrated store");
    assert_eq!(store.health().expect("health").schema_version, 5);
    store.checkpoint_truncate().expect("checkpoint");
    drop(store);

    let connection = rusqlite::Connection::open(&path).expect("inspection");
    assert_eq!(
        connection
            .query_row(
                "SELECT display_name FROM accounts WHERE id = 'legacy-account'",
                [],
                |row| row.get::<_, String>(0)
            )
            .expect("preserved account"),
        "Legacy"
    );
    for removed in ["thread_affinities", "quota_windows"] {
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [removed],
                    |row| row.get::<_, i64>(0)
                )
                .expect("table count"),
            0
        );
    }
    drop(connection);
    assert!(
        !fs::read(&path)
            .expect("database bytes")
            .windows(b"raw-thread-key-must-disappear".len())
            .any(|window| window == b"raw-thread-key-must-disappear")
    );
}

#[test]
fn metadata_batch_round_trips_and_normalizes_expired_runtime_windows() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("state.db");
    let mut store = StateStore::open(&path).expect("store");
    let batch = batch();

    assert_eq!(
        store
            .record_provider_metadata_batch(&batch)
            .expect("record"),
        ProviderMetadataBatchOutcome {
            provider_rows_written: 1,
            account_rows_written: 2,
            provider_rows_deleted: 0,
            account_rows_deleted: 0,
        }
    );

    let loaded = store.load_provider_metadata(1_500).expect("load metadata");
    assert_eq!(loaded.providers, batch.providers);
    assert_eq!(loaded.accounts[0], batch.accounts[0]);
    assert_eq!(loaded.accounts[1].health, AccountRuntimeHealth::Healthy);
    assert_eq!(loaded.accounts[1].cooldown_until_ms, None);
    assert_eq!(loaded.accounts[1].quota_remaining, None);
    assert_eq!(loaded.accounts[1].quota_resets_at_ms, None);
}

#[test]
fn unchanged_replay_writes_no_wal_bytes_and_changed_rows_are_batched() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("state.db");
    let mut store = StateStore::open(&path).expect("store");
    let batch = batch();
    store
        .record_provider_metadata_batch(&batch)
        .expect("initial batch");
    store.checkpoint_truncate().expect("checkpoint");
    let before = store.wal_size_bytes().expect("WAL bytes");

    assert_eq!(
        store
            .record_provider_metadata_batch(&batch)
            .expect("unchanged replay"),
        ProviderMetadataBatchOutcome::default()
    );
    assert_eq!(store.wal_size_bytes().expect("WAL bytes"), before);

    let mut changed = batch.clone();
    changed.accounts[0].health = AccountRuntimeHealth::Quarantined;
    changed.accounts[0].consecutive_failures = 2;
    changed.accounts[0].updated_at_ms = 1_100;
    assert_eq!(
        store
            .record_provider_metadata_batch(&changed)
            .expect("changed batch"),
        ProviderMetadataBatchOutcome {
            provider_rows_written: 0,
            account_rows_written: 1,
            provider_rows_deleted: 0,
            account_rows_deleted: 0,
        }
    );
}

#[test]
fn invalid_batch_is_rejected_before_any_partial_write() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("state.db");
    let mut store = StateStore::open(&path).expect("store");
    let mut invalid = batch();
    invalid.accounts.push(invalid.accounts[0].clone());

    assert!(matches!(
        store.record_provider_metadata_batch(&invalid),
        Err(StorageError::InvalidStateRecord { .. })
    ));
    assert_eq!(
        store.load_provider_metadata(1_000).expect("empty metadata"),
        ProviderMetadataBatch::default()
    );
}

#[test]
fn provider_metadata_schema_and_values_are_content_free() {
    let migration_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations/0004_provider_metadata.sql");
    let migration = fs::read_to_string(migration_path)
        .expect("migration")
        .to_ascii_lowercase();
    for forbidden in [
        "prompt",
        "response",
        "tool_payload",
        "thread_key",
        "authorization",
        "cookie",
        "credential",
    ] {
        assert!(
            !migration.contains(forbidden),
            "Provider metadata migration contains forbidden field {forbidden}"
        );
    }

    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("state.db");
    let mut store = StateStore::open(&path).expect("store");
    store
        .record_provider_metadata_batch(&batch())
        .expect("record");
    store.checkpoint_truncate().expect("checkpoint");
    drop(store);
    let bytes = fs::read(path).expect("database");
    for forbidden in [
        b"synthetic-prompt-body".as_slice(),
        b"raw-thread-key".as_slice(),
        b"Bearer synthetic-secret".as_slice(),
    ] {
        assert!(
            !bytes
                .windows(forbidden.len())
                .any(|window| window == forbidden)
        );
    }
}

#[test]
fn replacement_batch_removes_stale_rows_without_unbounded_history() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("state.db");
    let mut store = StateStore::open(&path).expect("store");
    store
        .record_provider_metadata_batch(&batch())
        .expect("initial batch");
    let replacement = ProviderMetadataBatch {
        providers: vec![ProviderRuntimeMetadata {
            provider_id: provider("next-provider"),
            updated_at_ms: 2_000,
        }],
        accounts: vec![AccountRuntimeMetadata {
            provider_id: provider("next-provider"),
            account_id: account("next-account"),
            health: AccountRuntimeHealth::Healthy,
            consecutive_failures: 0,
            cooldown_until_ms: None,
            quota_remaining: None,
            quota_resets_at_ms: None,
            selection_count: 0,
            last_selected_sequence: 0,
            updated_at_ms: 2_000,
        }],
    };

    assert_eq!(
        store
            .record_provider_metadata_batch(&replacement)
            .expect("replacement"),
        ProviderMetadataBatchOutcome {
            provider_rows_written: 1,
            account_rows_written: 1,
            provider_rows_deleted: 1,
            account_rows_deleted: 2,
        }
    );
    assert_eq!(
        store
            .load_provider_metadata(2_000)
            .expect("current metadata"),
        replacement
    );
}

#[tokio::test]
async fn metadata_batch_uses_the_existing_single_writer_queue() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("state.db");
    let store = StateStore::open(&path).expect("store");
    let (client, shutdown, writer) = state_store_writer(store);
    let writer_task = tokio::spawn(writer.run());

    assert_eq!(
        client
            .try_record_provider_metadata_batch(batch())
            .expect("submit")
            .wait()
            .await
            .expect("write"),
        ProviderMetadataBatchOutcome {
            provider_rows_written: 1,
            account_rows_written: 2,
            provider_rows_deleted: 0,
            account_rows_deleted: 0,
        }
    );
    shutdown
        .shutdown()
        .await
        .expect("shutdown")
        .wait()
        .await
        .expect("shutdown acknowledgement");
    writer_task.await.expect("writer");

    assert_eq!(
        StateStore::open(path)
            .expect("reopen")
            .load_provider_metadata(1_000)
            .expect("metadata"),
        batch()
    );
}

fn batch() -> ProviderMetadataBatch {
    ProviderMetadataBatch {
        providers: vec![ProviderRuntimeMetadata {
            provider_id: provider("provider"),
            updated_at_ms: 1_000,
        }],
        accounts: vec![
            AccountRuntimeMetadata {
                provider_id: provider("provider"),
                account_id: account("ready"),
                health: AccountRuntimeHealth::Healthy,
                consecutive_failures: 0,
                cooldown_until_ms: None,
                quota_remaining: Some(10),
                quota_resets_at_ms: Some(2_000),
                selection_count: 7,
                last_selected_sequence: 11,
                updated_at_ms: 1_000,
            },
            AccountRuntimeMetadata {
                provider_id: provider("provider"),
                account_id: account("stale"),
                health: AccountRuntimeHealth::CoolingDown,
                consecutive_failures: 1,
                cooldown_until_ms: Some(1_400),
                quota_remaining: Some(0),
                quota_resets_at_ms: Some(1_400),
                selection_count: 2,
                last_selected_sequence: 3,
                updated_at_ms: 1_000,
            },
        ],
    }
}

fn provider(value: &str) -> ProviderId {
    ProviderId::new(value).expect("provider")
}

fn account(value: &str) -> AccountId {
    AccountId::new(value).expect("account")
}
