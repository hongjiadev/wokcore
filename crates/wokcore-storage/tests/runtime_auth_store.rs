use std::{
    env, fs,
    path::Path,
    process::Command,
    sync::{Arc, Barrier},
    thread,
    time::SystemTime,
};

use rusqlite::{Connection, params};
use wokcore_core::{id::ClientId, secret::SecretRef};
use wokcore_storage::{ClientTokenMetadata, ReadOnlyStateStore, StateStore, StorageError};

const ISSUED_AT: &str = "2026-07-26T00:00:00Z";
const REVOKED_AT: &str = "2026-07-26T01:00:00Z";

fn create_schema_one(path: &Path) {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(include_str!("../migrations/0001_initial.sql"))
        .unwrap();
}

fn token(token_id: &str, client_id: &str, digest_byte: u8) -> ClientTokenMetadata {
    ClientTokenMetadata {
        token_id: token_id.to_owned(),
        client_id: ClientId::new(client_id).unwrap(),
        digest: [digest_byte; 32],
        issued_at: ISSUED_AT.to_owned(),
    }
}

#[test]
fn existing_schema_one_upgrades_to_schema_two_without_losing_data() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    create_schema_one(&path);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "INSERT INTO accounts(id, provider_id, display_name, auth_state) VALUES (?1, ?2, ?3, ?4)",
            params!["account-1", "provider-1", "Primary", "ready"],
        )
        .unwrap();
    drop(connection);

    let store = StateStore::open(&path).unwrap();

    assert_eq!(store.health().unwrap().schema_version, 2);
    let connection = Connection::open(path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT display_name FROM accounts WHERE id = 'account-1'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "Primary"
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version IN (1, 2)",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        2
    );
}

#[test]
fn concurrent_schema_one_opens_apply_schema_two_once() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    create_schema_one(&path);
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
        assert_eq!(handle.join().unwrap().schema_version, 2);
    }

    let connection = Connection::open(path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version = 2",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
}

#[test]
fn failed_schema_two_migration_preserves_schema_one_data_and_version() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    create_schema_one(&path);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "INSERT INTO accounts(id, provider_id, display_name, auth_state)
             VALUES ('account-1', 'provider-1', 'Primary', 'ready');
             CREATE VIEW runtime_secret_bindings AS SELECT id FROM accounts;",
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        StateStore::open(&path).unwrap_err(),
        StorageError::StateDatabase { .. }
    ));

    let connection = Connection::open(path).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        1
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT display_name FROM accounts WHERE id = 'account-1'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "Primary"
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'client_tokens'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
}

#[test]
fn runtime_secret_binding_is_insert_only_revisioned_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = StateStore::open(directory.path().join("state.db")).unwrap();
    let first_ref = SecretRef::parse("secret:00000000-0000-0000-0000-000000000001").unwrap();
    let replacement_ref = SecretRef::parse("secret:00000000-0000-0000-0000-000000000002").unwrap();

    let binding = store
        .bind_runtime_secret_if_absent("management", &first_ref, ISSUED_AT)
        .unwrap();

    assert_eq!(binding.name, "management");
    assert_eq!(binding.secret_ref, first_ref);
    assert_eq!(binding.revision, 1);
    assert_eq!(binding.created_at, ISSUED_AT);
    assert_eq!(
        store.runtime_secret_binding("management").unwrap(),
        Some(binding)
    );
    assert!(matches!(
        store
            .bind_runtime_secret_if_absent("management", &replacement_ref, REVOKED_AT)
            .unwrap_err(),
        StorageError::RuntimeSecretBindingConflict { actual: 1 }
    ));
    assert_eq!(
        store
            .runtime_secret_binding("management")
            .unwrap()
            .unwrap()
            .secret_ref,
        first_ref
    );
}

#[test]
fn client_issue_persists_only_digest_and_metadata_and_rejects_duplicates_atomically() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let mut store = StateStore::open(&path).unwrap();
    let first = token("token-1", "wokrouter", 0x11);

    store.issue_client_token(&first).unwrap();

    let connection = Connection::open(&path).unwrap();
    let stored = connection
        .query_row(
            "SELECT token_id, client_id, token_digest, issued_at, revoked_at FROM client_tokens",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        stored,
        (
            "token-1".to_owned(),
            "wokrouter".to_owned(),
            vec![0x11; 32],
            ISSUED_AT.to_owned(),
            None,
        )
    );
    drop(connection);

    assert!(matches!(
        store
            .issue_client_token(&token("token-1", "other-client", 0x22))
            .unwrap_err(),
        StorageError::StateDatabase { .. }
    ));
    assert!(matches!(
        store
            .issue_client_token(&token("token-2", "other-client", 0x11))
            .unwrap_err(),
        StorageError::StateDatabase { .. }
    ));
    assert_eq!(store.load_active_client_tokens().unwrap(), vec![first]);
}

#[test]
fn revocation_is_idempotent_and_active_load_excludes_revoked_rows() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = StateStore::open(directory.path().join("state.db")).unwrap();
    let first = token("token-1", "wokrouter", 0x11);
    let second = token("token-2", "other-client", 0x22);
    store.issue_client_token(&first).unwrap();
    store.issue_client_token(&second).unwrap();

    assert!(
        !store
            .revoke_client_token(
                &ClientId::new("other-client").unwrap(),
                "token-1",
                REVOKED_AT,
            )
            .unwrap()
    );
    assert_eq!(store.load_active_client_tokens().unwrap().len(), 2);
    assert!(
        store
            .revoke_client_token(&first.client_id, "token-1", REVOKED_AT)
            .unwrap()
    );
    assert!(
        !store
            .revoke_client_token(&first.client_id, "token-1", REVOKED_AT)
            .unwrap()
    );
    assert!(
        !store
            .revoke_client_token(&first.client_id, "missing", REVOKED_AT)
            .unwrap()
    );

    assert_eq!(store.load_active_client_tokens().unwrap(), vec![second]);
}

#[test]
fn runtime_auth_schema_contains_only_secret_refs_digests_and_non_secret_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let _store = StateStore::open(&path).unwrap();
    let connection = Connection::open(path).unwrap();

    let binding_columns = table_columns(&connection, "runtime_secret_bindings");
    let token_columns = table_columns(&connection, "client_tokens");
    assert_eq!(
        binding_columns,
        ["binding_name", "secret_ref", "revision", "created_at"]
    );
    assert_eq!(
        token_columns,
        [
            "token_id",
            "client_id",
            "token_digest",
            "issued_at",
            "revoked_at",
        ]
    );

    let migration = include_str!("../migrations/0002_runtime_auth.sql").to_ascii_lowercase();
    for forbidden in [
        "authorization",
        "cookie",
        "prompt",
        "response",
        "tool",
        "session",
        "admin_token",
        "proxy_token",
    ] {
        assert!(
            !migration.contains(forbidden),
            "runtime auth migration contains forbidden field {forbidden}"
        );
    }
}

#[test]
fn auth_metadata_reads_do_not_change_database_or_wal() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let mut store = StateStore::open(&path).unwrap();
    let secret_ref = SecretRef::parse("secret:00000000-0000-0000-0000-000000000001").unwrap();
    store
        .bind_runtime_secret_if_absent("management", &secret_ref, ISSUED_AT)
        .unwrap();
    store
        .issue_client_token(&token("token-1", "wokrouter", 0x11))
        .unwrap();
    let database_modified = modified_time(&path);
    let wal_size = store.wal_size_bytes().unwrap();

    for _ in 0..100 {
        assert!(
            store
                .runtime_secret_binding("management")
                .unwrap()
                .is_some()
        );
        assert_eq!(store.load_active_client_tokens().unwrap().len(), 1);
    }

    assert_eq!(modified_time(&path), database_modified);
    assert_eq!(store.wal_size_bytes().unwrap(), wal_size);
}

#[test]
fn read_only_inspection_reads_health_and_management_binding_without_writes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let mut store = StateStore::open(&path).unwrap();
    let secret_ref = SecretRef::parse("secret:00000000-0000-0000-0000-000000000001").unwrap();
    store
        .bind_runtime_secret_if_absent("management", &secret_ref, ISSUED_AT)
        .unwrap();
    store
        .issue_client_token(&token("token-1", "wokrouter", 0x11))
        .unwrap();
    drop(store);
    let before = directory_snapshot(directory.path());

    let read_only = ReadOnlyStateStore::open(&path).unwrap();
    assert_eq!(read_only.health().unwrap().schema_version, 2);
    assert_eq!(
        read_only
            .runtime_secret_binding("management")
            .unwrap()
            .unwrap()
            .secret_ref,
        secret_ref
    );
    drop(read_only);

    assert_snapshot_unchanged(directory.path(), &before);
}

#[test]
fn read_only_inspection_sees_a_valid_committed_crash_wal_without_writes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    create_schema_one_main_with_wal(&path);
    run_crash_wal_helper(&path, "migrate-two");
    assert!(fs::metadata(path.with_extension("db-wal")).unwrap().len() > 0);
    let before = directory_snapshot(directory.path());

    let read_only = ReadOnlyStateStore::open(&path).unwrap();

    assert_eq!(read_only.health().unwrap().schema_version, 2);
    drop(read_only);
    assert_snapshot_unchanged(directory.path(), &before);
}

#[test]
fn read_only_inspection_rebuilds_a_missing_crash_wal_index_in_memory() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    create_schema_one_main_with_wal(&path);
    run_crash_wal_helper(&path, "migrate-two");
    fs::remove_file(path.with_extension("db-shm")).unwrap();
    let before = directory_snapshot(directory.path());

    let read_only = ReadOnlyStateStore::open(&path).unwrap();

    assert_eq!(read_only.health().unwrap().schema_version, 2);
    drop(read_only);
    assert_snapshot_unchanged(directory.path(), &before);
}

#[test]
fn read_only_inspection_rebuilds_a_corrupt_crash_wal_index_in_memory() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    create_schema_one_main_with_wal(&path);
    run_crash_wal_helper(&path, "migrate-two");
    let shm_path = path.with_extension("db-shm");
    let mut shm = fs::read(&shm_path).unwrap();
    shm[0] ^= 0x01;
    fs::write(&shm_path, shm).unwrap();
    let before = directory_snapshot(directory.path());

    let read_only = ReadOnlyStateStore::open(&path).unwrap();

    assert_eq!(read_only.health().unwrap().schema_version, 2);
    drop(read_only);
    assert_snapshot_unchanged(directory.path(), &before);
}

#[test]
fn read_only_inspection_reports_a_corrupt_crash_wal_without_writes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    create_schema_one_main_with_wal(&path);
    run_crash_wal_helper(&path, "migrate-two");
    let wal_path = path.with_extension("db-wal");
    let mut wal = fs::read(&wal_path).unwrap();
    *wal.last_mut().unwrap() ^= 0x01;
    fs::write(&wal_path, wal).unwrap();
    let before = directory_snapshot(directory.path());

    let result = ReadOnlyStateStore::open(&path);

    assert!(matches!(
        result,
        Err(StorageError::StateDatabaseCorrupt { .. }) | Err(StorageError::StateDatabase { .. })
    ));
    assert_snapshot_unchanged(directory.path(), &before);
}

#[test]
fn read_only_inspection_reports_a_truncated_main_database_as_corrupt_without_writes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    fs::write(&path, vec![0_u8; 99]).unwrap();
    fs::write(path.with_extension("db-wal"), vec![0_u8; 32]).unwrap();
    fs::write(path.with_extension("db-shm"), b"unchanged shm").unwrap();
    let before = directory_snapshot(directory.path());

    let result = ReadOnlyStateStore::open(&path);

    assert!(matches!(
        result,
        Err(StorageError::StateDatabaseCorrupt { .. })
    ));
    assert_snapshot_unchanged(directory.path(), &before);
}

#[test]
fn read_only_inspection_reports_a_truncated_nonempty_wal_as_corrupt_without_writes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    drop(StateStore::open(&path).unwrap());
    fs::write(path.with_extension("db-wal"), vec![0_u8; 31]).unwrap();
    fs::write(path.with_extension("db-shm"), b"unchanged shm").unwrap();
    let before = directory_snapshot(directory.path());

    let result = ReadOnlyStateStore::open(&path);

    assert!(matches!(
        result,
        Err(StorageError::StateDatabaseCorrupt { .. })
    ));
    assert_snapshot_unchanged(directory.path(), &before);
}

#[test]
#[ignore = "spawned only by crash-WAL inspection tests"]
fn crash_wal_writer_helper() {
    let Some(path) = env::var_os("WOKCORE_CRASH_WAL_PATH") else {
        return;
    };
    let mode = env::var("WOKCORE_CRASH_WAL_MODE").unwrap();
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch("PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;")
        .unwrap();
    match mode.as_str() {
        "migrate-two" => connection
            .execute_batch(include_str!("../migrations/0002_runtime_auth.sql"))
            .unwrap(),
        _ => panic!("unexpected crash WAL helper mode"),
    }
    std::process::exit(0);
}

fn create_schema_one_main_with_wal(path: &Path) {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch("PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;")
        .unwrap();
    connection
        .execute_batch(include_str!("../migrations/0001_initial.sql"))
        .unwrap();
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
}

fn run_crash_wal_helper(path: &Path, mode: &str) {
    let status = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "crash_wal_writer_helper",
            "--ignored",
            "--nocapture",
        ])
        .env("WOKCORE_CRASH_WAL_PATH", path)
        .env("WOKCORE_CRASH_WAL_MODE", mode)
        .status()
        .unwrap();
    assert!(status.success());
}

fn table_columns(connection: &Connection, table: &str) -> Vec<String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    statement
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn modified_time(path: &Path) -> SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

fn directory_snapshot(path: &Path) -> Vec<(String, Vec<u8>, SystemTime)> {
    let mut snapshot = fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                fs::read(entry.path()).unwrap(),
                entry.metadata().unwrap().modified().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    snapshot.sort_by(|left, right| left.0.cmp(&right.0));
    snapshot
}

fn assert_snapshot_unchanged(path: &Path, before: &[(String, Vec<u8>, SystemTime)]) {
    let after = directory_snapshot(path);
    let before_names = before.iter().map(|(name, _, _)| name).collect::<Vec<_>>();
    let after_names = after.iter().map(|(name, _, _)| name).collect::<Vec<_>>();
    assert_eq!(after_names, before_names, "directory entries changed");
    for ((name, before_bytes, before_modified), (_, after_bytes, after_modified)) in
        before.iter().zip(&after)
    {
        assert_eq!(
            after_bytes.len(),
            before_bytes.len(),
            "{name} length changed"
        );
        let first_difference = before_bytes
            .iter()
            .zip(after_bytes)
            .position(|(before, after)| before != after);
        assert!(
            after_bytes == before_bytes,
            "{name} bytes changed at offset {first_difference:?}"
        );
        assert_eq!(
            after_modified, before_modified,
            "{name} modified time changed"
        );
    }
}
