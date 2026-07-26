use std::{
    fs,
    path::Path,
    str::FromStr,
    sync::{Arc, Barrier},
    thread,
};

use rusqlite::{Connection, params};
use wokcore_core::id::ClientId;
use wokcore_storage::{
    AttemptId, CandidateBeginOutcome, ClientTokenMetadata, ClientTokenScope, CodexReplaySignature,
    GlobalSessionIndexPageKey, GlobalSessionUsagePageKey, MAX_CODEX_REPLAY_SIGNATURES,
    MAX_SESSION_BATCH_BYTES, MAX_SESSION_BATCH_ROWS, MAX_SUPPLEMENTAL_ROWS, OpaqueFingerprint,
    ParserCheckpoint, ReadOnlyStateStore, ReplaySignaturePageKey, RequestId,
    RequestSupplementalMetadata, SessionAvailability, SessionBatch, SessionFileIdentity,
    SessionGenerationState, SessionIndexPageKey, SessionIndexRecord, SessionScanCursor,
    SessionScanResultCode, SessionSourceErrorCode, SessionSourceKind, SessionSourcePageKey,
    SessionSourceStatus, SessionUsagePageKey, SessionUsageRecord, StateStore, StorageError,
    SupplementalErrorCode, SupplementalFailoverDecision, SupplementalRetryDecision, TraceId,
};

const NOW: &str = "2026-07-26T00:00:00Z";

fn create_schema_two(path: &Path) {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(include_str!("../migrations/0001_initial.sql"))
        .unwrap();
    connection
        .execute_batch(include_str!("../migrations/0002_runtime_auth.sql"))
        .unwrap();
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

fn opaque(value: u64) -> String {
    format!("{value:064x}")
}

fn checkpoint() -> ParserCheckpoint {
    ParserCheckpoint {
        version: 1,
        previous_input_tokens: 100,
        previous_output_tokens: 50,
        previous_cache_read_tokens: 25,
        previous_cache_write_tokens: 10,
        previous_reasoning_tokens: 5,
        current_model: Some("gpt-5.6".to_owned()),
        event_ordinal: 7,
        lineage_source_key: Some(opaque(99)),
        lineage_generation: Some(4),
        lineage_record_ordinal: 6,
        structural_hash: Some([0x41; 32]),
    }
}

fn cursor(source_key: &str, generation: u64, offset: u64) -> SessionScanCursor {
    SessionScanCursor {
        source_key: source_key.to_owned(),
        source_kind: SessionSourceKind::Codex,
        generation,
        generation_state: SessionGenerationState::Staging,
        file_identity: SessionFileIdentity::new(opaque(400)).unwrap(),
        observed_size: offset + 128,
        modified_at: NOW.to_owned(),
        complete_byte_offset: offset,
        stable_record_ordinal: offset / 10,
        parser_checkpoint: checkpoint(),
        head_fingerprint: [0x11; 32],
        boundary_fingerprint: [0x22; 32],
        parent_source_key: Some(opaque(99)),
        parent_generation: Some(4),
        replay_boundary_fingerprint: Some([0x33; 32]),
        result_code: Some(SessionScanResultCode::Advanced),
        result_changed_at: Some(NOW.to_owned()),
    }
}

fn index_record(source_key: &str, generation: u64, session_value: u64) -> SessionIndexRecord {
    SessionIndexRecord {
        session_key: opaque(session_value),
        source_key: source_key.to_owned(),
        generation,
        source_kind: SessionSourceKind::Codex,
        created_at: "2026-07-25T00:00:00Z".to_owned(),
        last_active_at: NOW.to_owned(),
        message_count: 3,
        usage_event_count: 1,
        availability: SessionAvailability::Available,
    }
}

fn usage_record(
    source_key: &str,
    generation: u64,
    session_value: u64,
    usage_value: u64,
    revision: u64,
) -> SessionUsageRecord {
    SessionUsageRecord {
        usage_id: opaque(usage_value),
        session_key: opaque(session_value),
        source_key: source_key.to_owned(),
        generation,
        source_kind: SessionSourceKind::Codex,
        model: "gpt-5.6".to_owned(),
        occurred_at: NOW.to_owned(),
        input_tokens: 10,
        output_tokens: 20,
        cache_read_tokens: 3,
        cache_write_tokens: 4,
        reasoning_tokens: 5,
        record_revision: revision,
    }
}

fn replay_signature(source_key: &str, generation: u64, ordinal: u64) -> CodexReplaySignature {
    CodexReplaySignature {
        parent_source_key: source_key.to_owned(),
        parent_generation: generation,
        token_event_ordinal: ordinal,
        occurred_at: NOW.to_owned(),
        signature_hash: [ordinal as u8; 32],
    }
}

fn supplemental(request_id: &str) -> RequestSupplementalMetadata {
    RequestSupplementalMetadata {
        request_id: RequestId::new(request_id).unwrap(),
        attempt_id: AttemptId::new("attempt-1").unwrap(),
        trace_id: TraceId::new("trace-1").unwrap(),
        occurred_at: NOW.to_owned(),
        route_fingerprint: OpaqueFingerprint::new(opaque(201)).unwrap(),
        provider_fingerprint: OpaqueFingerprint::new(opaque(202)).unwrap(),
        account_fingerprint: Some(OpaqueFingerprint::new(opaque(203)).unwrap()),
        retry_decision: SupplementalRetryDecision::None,
        failover_decision: SupplementalFailoverDecision::None,
        queue_ms: 1,
        connect_ms: 2,
        first_byte_ms: 3,
        total_ms: 4,
        request_bytes: 5,
        response_bytes: 6,
        status_code: Some(200),
        error_code: None,
    }
}

fn dense_supplemental(index: usize) -> RequestSupplementalMetadata {
    let mut metadata = supplemental(&format!("{index:04}-{}", "r".repeat(250)));
    metadata.attempt_id = AttemptId::new("a".repeat(256)).unwrap();
    metadata.trace_id = TraceId::new("t".repeat(256)).unwrap();
    metadata.error_code = Some(SupplementalErrorCode::new("e".repeat(128)).unwrap());
    metadata
}

fn promote_one_record(
    store: &mut StateStore,
    source_key: &str,
    generation: u64,
    session_value: u64,
) {
    let mut generation_cursor = cursor(source_key, generation, 100);
    store.begin_or_resume_candidate(&generation_cursor).unwrap();
    store
        .commit_candidate_batch(&SessionBatch {
            cursor: Some(generation_cursor.clone()),
            index_records: vec![index_record(source_key, generation, session_value)],
            ..SessionBatch::default()
        })
        .unwrap();
    store
        .promote_candidate(source_key, generation, NOW)
        .unwrap();
    generation_cursor.generation_state = SessionGenerationState::Current;
}

#[test]
fn external_crate_can_rebuild_every_page_key_from_validated_components() {
    let source_key = opaque(1);
    let session_key = opaque(2);
    let usage_id = opaque(3);

    let index = SessionIndexPageKey::new(&source_key, 4, NOW, &session_key).unwrap();
    assert_eq!(index.source_key(), source_key);
    assert_eq!(index.generation(), 4);
    assert_eq!(index.last_active_at(), NOW);
    assert_eq!(index.session_key(), session_key);

    let source = SessionSourcePageKey::new(&source_key).unwrap();
    assert_eq!(source.source_key(), source_key);

    let global_index = GlobalSessionIndexPageKey::new(NOW, &session_key, &source_key).unwrap();
    assert_eq!(global_index.last_active_at(), NOW);
    assert_eq!(global_index.session_key(), session_key);
    assert_eq!(global_index.source_key(), source_key);

    let usage = SessionUsagePageKey::new(&source_key, 4, NOW, &usage_id).unwrap();
    assert_eq!(usage.source_key(), source_key);
    assert_eq!(usage.generation(), 4);
    assert_eq!(usage.occurred_at(), NOW);
    assert_eq!(usage.usage_id(), usage_id);

    let global_usage = GlobalSessionUsagePageKey::new(NOW, &usage_id, &source_key).unwrap();
    assert_eq!(global_usage.occurred_at(), NOW);
    assert_eq!(global_usage.usage_id(), usage_id);
    assert_eq!(global_usage.source_key(), source_key);

    let replay = ReplaySignaturePageKey::new(&source_key, 4, 5).unwrap();
    assert_eq!(replay.parent_source_key(), source_key);
    assert_eq!(replay.parent_generation(), 4);
    assert_eq!(replay.token_event_ordinal(), 5);

    assert!(SessionSourcePageKey::new("not-an-opaque-key").is_err());
    assert!(SessionIndexPageKey::new(&source_key, 0, NOW, &session_key).is_err());
    assert!(
        GlobalSessionIndexPageKey::new("2026-07-26T00:00:00+00:00", &session_key, &source_key)
            .is_err()
    );
    assert!(SessionUsagePageKey::new(&source_key, 4, NOW, "not-an-opaque-key").is_err());
    assert!(
        GlobalSessionUsagePageKey::new("2026-07-26T00:00:00+00:00", &usage_id, &source_key,)
            .is_err()
    );
    assert!(ReplaySignaturePageKey::new(&source_key, 4, 0).is_err());
}

#[test]
fn ordered_migration_history_reaches_schema_three() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");

    let store = StateStore::open(&path).unwrap();

    assert_eq!(store.health().unwrap().schema_version, 3);
    let connection = Connection::open(path).unwrap();
    let versions = connection
        .prepare("SELECT version FROM schema_migrations ORDER BY version")
        .unwrap()
        .query_map([], |row| row.get::<_, i64>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(versions, [1, 2, 3]);
}

#[test]
fn schema_three_has_only_compact_session_and_supplemental_columns() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let _store = StateStore::open(&path).unwrap();
    let connection = Connection::open(path).unwrap();

    assert_eq!(
        table_columns(&connection, "session_sources"),
        [
            "source_key",
            "source_kind",
            "current_generation",
            "staging_generation",
            "retired_generation",
            "status",
            "error_code",
            "last_transition_at",
        ]
    );
    assert_eq!(
        table_columns(&connection, "session_scan_cursors"),
        [
            "source_key",
            "generation",
            "source_kind",
            "generation_state",
            "file_identity",
            "observed_size",
            "modified_at",
            "complete_byte_offset",
            "stable_record_ordinal",
            "parser_checkpoint",
            "head_fingerprint",
            "boundary_fingerprint",
            "parent_source_key",
            "parent_generation",
            "replay_boundary_fingerprint",
            "result_code",
            "result_changed_at",
        ]
    );
    assert_eq!(
        table_columns(&connection, "session_index"),
        [
            "session_key",
            "source_key",
            "generation",
            "source_kind",
            "created_at",
            "last_active_at",
            "message_count",
            "usage_event_count",
            "availability",
        ]
    );
    assert_eq!(
        table_columns(&connection, "session_usage_records"),
        [
            "usage_id",
            "session_key",
            "source_key",
            "generation",
            "source_kind",
            "model",
            "occurred_at",
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
            "reasoning_tokens",
            "record_revision",
        ]
    );
    assert_eq!(
        table_columns(&connection, "codex_replay_signatures"),
        [
            "parent_source_key",
            "parent_generation",
            "token_event_ordinal",
            "occurred_at",
            "signature_hash",
        ]
    );
    assert_eq!(
        table_columns(&connection, "request_supplemental_metadata"),
        [
            "request_id",
            "attempt_id",
            "trace_id",
            "occurred_at",
            "route_fingerprint",
            "provider_fingerprint",
            "account_fingerprint",
            "retry_decision",
            "failover_decision",
            "queue_ms",
            "connect_ms",
            "first_byte_ms",
            "total_ms",
            "request_bytes",
            "response_bytes",
            "status_code",
            "error_code",
            "logical_bytes",
        ]
    );
    assert_eq!(
        table_columns(&connection, "client_token_scopes"),
        ["token_id", "scope"]
    );

    let migration = include_str!("../migrations/0003_session_diagnostics.sql").to_ascii_lowercase();
    for forbidden in [
        "prompt",
        "response_body",
        "tool_payload",
        "sse_chunk",
        "authorization",
        "cookie",
        "session_body",
        "credential",
        "absolute_path",
        "relative_path",
        "confirmation_state",
    ] {
        assert!(
            !migration.contains(forbidden),
            "schema migration contains forbidden field {forbidden}"
        );
    }
}

#[test]
fn schema_three_has_current_generation_keyset_indexes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let _store = StateStore::open(&path).unwrap();
    let connection = Connection::open(path).unwrap();
    let indexes = connection
        .prepare(
            "SELECT name FROM sqlite_schema
             WHERE type = 'index' AND name NOT LIKE 'sqlite_autoindex_%'
             ORDER BY name",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    for expected in [
        "codex_replay_current_order",
        "session_index_current_order",
        "session_index_global_current_order",
        "session_usage_current_order",
        "session_usage_global_current_order",
    ] {
        assert!(indexes.iter().any(|index| index == expected), "{expected}");
    }
}

#[test]
fn schema_two_tokens_migrate_with_proxy_scope_and_scope_allowlist_is_exact() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    create_schema_two(&path);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "INSERT INTO client_tokens(token_id, client_id, token_digest, issued_at)
             VALUES (?1, ?2, ?3, ?4)",
            params!["existing", "wokrouter", [0x11_u8; 32], NOW],
        )
        .unwrap();
    drop(connection);

    let mut store = StateStore::open(&path).unwrap();

    let migrated = store.load_active_scoped_client_tokens().unwrap();
    assert_eq!(migrated.len(), 1);
    assert_eq!(migrated[0].token.token_id, "existing");
    assert_eq!(migrated[0].scopes, [ClientTokenScope::ProxyUse]);

    let allowed = [
        ("proxy.use", ClientTokenScope::ProxyUse),
        ("sessions.read", ClientTokenScope::SessionsRead),
        ("usage.read", ClientTokenScope::UsageRead),
        ("diagnostics.read", ClientTokenScope::DiagnosticsRead),
        ("diagnostics.export", ClientTokenScope::DiagnosticsExport),
    ];
    for (text, expected) in allowed {
        assert_eq!(ClientTokenScope::from_str(text).unwrap(), expected);
        assert_eq!(expected.as_str(), text);
    }
    for rejected in ["*", "sessions.*", "proxy", "admin", "Proxy.Use", ""] {
        assert!(ClientTokenScope::from_str(rejected).is_err(), "{rejected}");
    }

    let explicit = ClientTokenMetadata {
        token_id: "explicit".to_owned(),
        client_id: ClientId::new("wokrouter").unwrap(),
        digest: [0x22; 32],
        issued_at: NOW.to_owned(),
    };
    store
        .issue_client_token_with_scopes(
            &explicit,
            &[
                ClientTokenScope::SessionsRead,
                ClientTokenScope::DiagnosticsExport,
            ],
        )
        .unwrap();
    let scoped = store.load_active_scoped_client_tokens().unwrap();
    assert_eq!(scoped.len(), 2);
    assert_eq!(
        scoped[1].scopes,
        [
            ClientTokenScope::SessionsRead,
            ClientTokenScope::DiagnosticsExport,
        ]
    );
}

#[test]
fn schema_two_migration_is_transactional_concurrent_and_preserves_data() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    create_schema_two(&path);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "INSERT INTO accounts(id, provider_id, display_name, auth_state)
             VALUES ('account-1', 'provider-1', 'Primary', 'ready')",
            [],
        )
        .unwrap();
    drop(connection);
    let barrier = Arc::new(Barrier::new(2));
    let handles = (0..2)
        .map(|_| {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                StateStore::open(path).unwrap().health().unwrap()
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        assert_eq!(handle.join().unwrap().schema_version, 3);
    }
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
                "SELECT COUNT(*) FROM schema_migrations WHERE version = 3",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
}

#[test]
fn failed_schema_three_migration_preserves_schema_two_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    create_schema_two(&path);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "INSERT INTO accounts(id, provider_id, display_name, auth_state)
             VALUES ('account-1', 'provider-1', 'Primary', 'ready');
             CREATE VIEW session_sources AS SELECT id AS source_key FROM accounts;",
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
        2
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
}

#[test]
fn session_state_types_are_public_storage_contracts() {
    fn assert_contracts(
        _: SessionSourceKind,
        _: ParserCheckpoint,
        _: SessionScanCursor,
        _: SessionIndexRecord,
        _: SessionUsageRecord,
    ) {
    }

    let _ = assert_contracts;
}

#[test]
fn candidate_batch_is_atomic_resumable_and_hidden_until_promotion() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let source_key = opaque(1);
    let initial = cursor(&source_key, 1, 0);
    let advanced = cursor(&source_key, 1, 100);
    let mut store = StateStore::open(&path).unwrap();

    assert_eq!(
        store.begin_or_resume_candidate(&initial).unwrap(),
        CandidateBeginOutcome::Started
    );
    store
        .commit_candidate_batch(&SessionBatch {
            cursor: Some(advanced.clone()),
            index_records: vec![index_record(&source_key, 1, 10)],
            usage_records: vec![usage_record(&source_key, 1, 10, 20, 1)],
            replay_signatures: vec![replay_signature(&source_key, 1, 1)],
            supplemental_metadata: vec![supplemental("request-1")],
        })
        .unwrap();

    assert_eq!(store.load_current_generation(&source_key).unwrap(), None);
    assert!(
        store
            .load_current_session_index_page(&source_key, None, 200)
            .unwrap()
            .items
            .is_empty()
    );
    assert!(
        store
            .load_current_session_usage_page(&source_key, None, 500)
            .unwrap()
            .items
            .is_empty()
    );
    drop(store);

    let mut store = StateStore::open(&path).unwrap();
    assert_eq!(
        store.begin_or_resume_candidate(&initial).unwrap(),
        CandidateBeginOutcome::Resumed(Box::new(advanced.clone()))
    );
    store.promote_candidate(&source_key, 1, NOW).unwrap();

    assert_eq!(store.load_current_generation(&source_key).unwrap(), Some(1));
    assert_eq!(
        store
            .load_current_session_index_page(&source_key, None, 200)
            .unwrap()
            .items,
        [index_record(&source_key, 1, 10)]
    );
    assert_eq!(
        store
            .load_current_session_usage_page(&source_key, None, 500)
            .unwrap()
            .items,
        [usage_record(&source_key, 1, 10, 20, 1)]
    );
    let replay = store
        .load_codex_replay_signature_page(&source_key, 1, None, 512)
        .unwrap();
    assert_eq!(replay.items, [replay_signature(&source_key, 1, 1)]);
    assert_eq!(replay.next_page_key, None);

    let connection = Connection::open(path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM request_supplemental_metadata",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
}

#[test]
fn same_generation_append_advances_index_monotonically_and_rolls_back_regressions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let source_key = opaque(101);
    let mut store = StateStore::open(&path).unwrap();
    let initial_cursor = cursor(&source_key, 1, 100);
    let initial_index = index_record(&source_key, 1, 101);
    store.begin_or_resume_candidate(&initial_cursor).unwrap();
    store
        .commit_candidate_batch(&SessionBatch {
            cursor: Some(initial_cursor),
            index_records: vec![initial_index.clone()],
            ..SessionBatch::default()
        })
        .unwrap();
    store.promote_candidate(&source_key, 1, NOW).unwrap();

    let mut appended_cursor = cursor(&source_key, 1, 200);
    appended_cursor.generation_state = SessionGenerationState::Current;
    appended_cursor.modified_at = "2026-07-26T00:01:00Z".to_owned();
    appended_cursor.result_changed_at = Some("2026-07-26T00:01:00Z".to_owned());
    let mut appended_index = initial_index.clone();
    appended_index.last_active_at = "2026-07-26T00:01:00Z".to_owned();
    appended_index.message_count = 5;
    appended_index.usage_event_count = 2;
    store
        .commit_session_batch(&SessionBatch {
            cursor: Some(appended_cursor.clone()),
            index_records: vec![appended_index.clone()],
            ..SessionBatch::default()
        })
        .unwrap();
    assert_eq!(
        store
            .load_current_session_index_page(&source_key, None, 200)
            .unwrap()
            .items,
        [appended_index]
    );

    let mut later_cursor = cursor(&source_key, 1, 300);
    later_cursor.generation_state = SessionGenerationState::Current;
    later_cursor.modified_at = "2026-07-26T00:02:00Z".to_owned();
    later_cursor.result_changed_at = Some("2026-07-26T00:02:00Z".to_owned());
    let mut regressive_index = initial_index;
    regressive_index.last_active_at = "2026-07-26T00:02:00Z".to_owned();
    regressive_index.message_count = 4;
    regressive_index.usage_event_count = 3;
    assert!(matches!(
        store
            .commit_session_batch(&SessionBatch {
                cursor: Some(later_cursor),
                index_records: vec![regressive_index],
                ..SessionBatch::default()
            })
            .unwrap_err(),
        StorageError::StableRecordConflict { .. } | StorageError::InvalidStateRecord { .. }
    ));
    drop(store);

    let connection = Connection::open(path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT complete_byte_offset FROM session_scan_cursors
                 WHERE source_key = ?1 AND generation = 1",
                [&source_key],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        200
    );
}

#[test]
fn current_cursor_rejects_invariant_changes_and_regressive_file_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let source_key = opaque(109);
    let mut store = StateStore::open(&path).unwrap();
    let staging = cursor(&source_key, 1, 100);
    store.begin_or_resume_candidate(&staging).unwrap();
    store
        .commit_candidate_batch(&SessionBatch {
            cursor: Some(staging.clone()),
            ..SessionBatch::default()
        })
        .unwrap();
    store.promote_candidate(&source_key, 1, NOW).unwrap();
    let mut expected = staging;
    expected.generation_state = SessionGenerationState::Current;

    let wal_before_replay = store.wal_size_bytes().unwrap();
    let replay_outcome = store
        .commit_session_batch(&SessionBatch {
            cursor: Some(expected.clone()),
            ..SessionBatch::default()
        })
        .unwrap();
    assert_eq!(replay_outcome.inserted_rows, 0);
    assert_eq!(replay_outcome.dropped_rows, 0);
    assert_eq!(store.wal_size_bytes().unwrap(), wal_before_replay);

    let mut changed_checkpoint = expected.clone();
    changed_checkpoint.parser_checkpoint.previous_input_tokens += 1;
    assert!(matches!(
        store
            .commit_session_batch(&SessionBatch {
                cursor: Some(changed_checkpoint),
                ..SessionBatch::default()
            })
            .unwrap_err(),
        StorageError::StableRecordConflict { .. }
    ));

    let mut changed_lineage = expected.clone();
    changed_lineage.parent_generation = Some(5);
    assert!(matches!(
        store
            .commit_session_batch(&SessionBatch {
                cursor: Some(changed_lineage),
                ..SessionBatch::default()
            })
            .unwrap_err(),
        StorageError::StableRecordConflict { .. } | StorageError::InvalidStateRecord { .. }
    ));

    let mut regressive_size = expected.clone();
    regressive_size.observed_size -= 1;
    assert!(matches!(
        store
            .commit_session_batch(&SessionBatch {
                cursor: Some(regressive_size),
                ..SessionBatch::default()
            })
            .unwrap_err(),
        StorageError::StableRecordConflict { .. }
    ));

    let mut cleared_result = expected.clone();
    cleared_result.result_code = None;
    cleared_result.result_changed_at = None;
    assert!(matches!(
        store
            .commit_session_batch(&SessionBatch {
                cursor: Some(cleared_result),
                ..SessionBatch::default()
            })
            .unwrap_err(),
        StorageError::StableRecordConflict { .. }
    ));

    let mut result_transition = expected.clone();
    result_transition.result_code = Some(SessionScanResultCode::Unchanged);
    result_transition.result_changed_at = Some("2026-07-26T00:01:00Z".to_owned());
    store
        .commit_session_batch(&SessionBatch {
            cursor: Some(result_transition.clone()),
            ..SessionBatch::default()
        })
        .unwrap();
    expected = result_transition;
    assert_eq!(
        store.load_current_session_scan_cursor(&source_key).unwrap(),
        Some(expected)
    );
}

#[test]
fn staging_resume_rejects_changed_parent_lineage_and_replay_anchor() {
    let directory = tempfile::tempdir().unwrap();
    let source_key = opaque(102);
    let mut store = StateStore::open(directory.path().join("state.db")).unwrap();
    let initial = cursor(&source_key, 1, 100);
    store.begin_or_resume_candidate(&initial).unwrap();

    let mut changed_parent = initial.clone();
    changed_parent.parent_source_key = Some(opaque(103));
    changed_parent.parser_checkpoint.lineage_source_key = Some(opaque(103));
    assert_eq!(
        store.begin_or_resume_candidate(&changed_parent).unwrap(),
        CandidateBeginOutcome::CleanupRequired { generation: 1 }
    );

    let mut changed_generation = initial.clone();
    changed_generation.parent_generation = Some(5);
    changed_generation.parser_checkpoint.lineage_generation = Some(5);
    assert_eq!(
        store
            .begin_or_resume_candidate(&changed_generation)
            .unwrap(),
        CandidateBeginOutcome::CleanupRequired { generation: 1 }
    );

    let mut changed_anchor = initial;
    changed_anchor.replay_boundary_fingerprint = Some([0x44; 32]);
    assert_eq!(
        store.begin_or_resume_candidate(&changed_anchor).unwrap(),
        CandidateBeginOutcome::CleanupRequired { generation: 1 }
    );
}

#[test]
fn promoted_cursor_reloads_completely_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let source_key = opaque(104);
    let expected = cursor(&source_key, 1, 321);
    let mut store = StateStore::open(&path).unwrap();
    store.begin_or_resume_candidate(&expected).unwrap();
    store
        .commit_candidate_batch(&SessionBatch {
            cursor: Some(expected.clone()),
            ..SessionBatch::default()
        })
        .unwrap();
    store.promote_candidate(&source_key, 1, NOW).unwrap();
    drop(store);

    let store = StateStore::open(path).unwrap();
    let mut expected = expected;
    expected.generation_state = SessionGenerationState::Current;
    assert_eq!(
        store.load_current_session_scan_cursor(&source_key).unwrap(),
        Some(expected)
    );
}

#[test]
fn invalid_candidate_row_rolls_back_cursor_and_every_record() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let source_key = opaque(2);
    let initial = cursor(&source_key, 1, 0);
    let mut store = StateStore::open(&path).unwrap();
    store.begin_or_resume_candidate(&initial).unwrap();
    let mut invalid_usage = usage_record(&source_key, 1, 11, 21, 1);
    invalid_usage.model = "m".repeat(257);

    assert!(matches!(
        store
            .commit_candidate_batch(&SessionBatch {
                cursor: Some(cursor(&source_key, 1, 100)),
                index_records: vec![index_record(&source_key, 1, 11)],
                usage_records: vec![invalid_usage],
                replay_signatures: vec![replay_signature(&source_key, 1, 1)],
                supplemental_metadata: vec![supplemental("request-invalid")],
            })
            .unwrap_err(),
        StorageError::InvalidStateRecord { .. }
    ));
    assert_eq!(
        store.begin_or_resume_candidate(&initial).unwrap(),
        CandidateBeginOutcome::Resumed(Box::new(initial))
    );

    let connection = Connection::open(path).unwrap();
    for table in [
        "session_index",
        "session_usage_records",
        "codex_replay_signatures",
        "request_supplemental_metadata",
    ] {
        assert_eq!(
            connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0,
            "{table}"
        );
    }
}

#[test]
fn usage_revision_replay_is_idempotent_higher_replaces_and_conflict_rolls_back() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let source_key = opaque(3);
    let mut store = StateStore::open(&path).unwrap();
    let mut current_cursor = cursor(&source_key, 1, 100);
    store.begin_or_resume_candidate(&current_cursor).unwrap();
    let original = usage_record(&source_key, 1, 12, 22, 1);
    store
        .commit_candidate_batch(&SessionBatch {
            cursor: Some(current_cursor.clone()),
            index_records: vec![index_record(&source_key, 1, 12)],
            usage_records: vec![original.clone()],
            ..SessionBatch::default()
        })
        .unwrap();
    store.promote_candidate(&source_key, 1, NOW).unwrap();
    current_cursor.generation_state = SessionGenerationState::Current;
    let wal_before = store.wal_size_bytes().unwrap();

    store
        .commit_session_batch(&SessionBatch {
            cursor: Some(current_cursor.clone()),
            usage_records: vec![original.clone()],
            ..SessionBatch::default()
        })
        .unwrap();
    assert_eq!(store.wal_size_bytes().unwrap(), wal_before);

    let mut replacement = original.clone();
    replacement.record_revision = 2;
    replacement.output_tokens = 30;
    store
        .commit_session_batch(&SessionBatch {
            cursor: Some(current_cursor.clone()),
            usage_records: vec![replacement.clone()],
            ..SessionBatch::default()
        })
        .unwrap();
    assert_eq!(
        store
            .load_current_session_usage_page(&source_key, None, 500)
            .unwrap()
            .items,
        [replacement]
    );

    let mut conflicting = original;
    conflicting.output_tokens = 99;
    let error = store
        .commit_session_batch(&SessionBatch {
            cursor: Some(current_cursor),
            index_records: vec![index_record(&source_key, 1, 13)],
            usage_records: vec![conflicting],
            ..SessionBatch::default()
        })
        .unwrap_err();
    assert!(matches!(error, StorageError::StableRecordConflict { .. }));
    assert_eq!(
        store
            .load_current_session_index_page(&source_key, None, 200)
            .unwrap()
            .items,
        [index_record(&source_key, 1, 12)]
    );
}

#[test]
fn failed_multibatch_candidate_stays_hidden_and_bounded_cleanup_preserves_current() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let source_key = opaque(4);
    let mut store = StateStore::open(&path).unwrap();
    promote_one_record(&mut store, &source_key, 1, 1000);
    let first_page = store
        .load_current_session_index_page(&source_key, None, 1)
        .unwrap();
    assert_eq!(first_page.items, [index_record(&source_key, 1, 1000)]);

    let second_cursor = cursor(&source_key, 2, 100);
    store.begin_or_resume_candidate(&second_cursor).unwrap();
    let first_batch = (0..MAX_SESSION_BATCH_ROWS)
        .map(|offset| index_record(&source_key, 2, 2000 + offset as u64))
        .collect::<Vec<_>>();
    store
        .commit_candidate_batch(&SessionBatch {
            cursor: Some(second_cursor.clone()),
            index_records: first_batch,
            ..SessionBatch::default()
        })
        .unwrap();
    let mut final_cursor = cursor(&source_key, 2, 200);
    let mut invalid_final = usage_record(&source_key, 2, 2512, 3000, 1);
    invalid_final.model = "m".repeat(257);
    assert!(matches!(
        store
            .commit_candidate_batch(&SessionBatch {
                cursor: Some(final_cursor.clone()),
                index_records: vec![index_record(&source_key, 2, 2512)],
                usage_records: vec![invalid_final],
                ..SessionBatch::default()
            })
            .unwrap_err(),
        StorageError::InvalidStateRecord { .. }
    ));
    assert_eq!(
        store
            .load_current_session_index_page(&source_key, None, 200)
            .unwrap()
            .items,
        [index_record(&source_key, 1, 1000)]
    );

    assert!(
        store
            .fail_candidate(
                &source_key,
                2,
                SessionSourceErrorCode::SourceParseInvalid,
                "2026-07-26T00:01:00Z",
            )
            .unwrap()
    );
    let wal_after_failure = store.wal_size_bytes().unwrap();
    assert!(
        !store
            .fail_candidate(
                &source_key,
                2,
                SessionSourceErrorCode::SourceParseInvalid,
                "2026-07-26T00:02:00Z",
            )
            .unwrap()
    );
    assert_eq!(store.wal_size_bytes().unwrap(), wal_after_failure);
    let source = store.load_session_source(&source_key).unwrap().unwrap();
    assert_eq!(source.current_generation, Some(1));
    assert_eq!(source.staging_generation, Some(2));
    assert_eq!(source.status, SessionSourceStatus::Stale);
    assert_eq!(
        source.error_code,
        Some(SessionSourceErrorCode::SourceParseInvalid)
    );

    loop {
        let cleanup = store
            .cleanup_generation_batch(
                &source_key,
                2,
                MAX_SESSION_BATCH_ROWS,
                MAX_SESSION_BATCH_BYTES,
            )
            .unwrap();
        assert!(cleanup.deleted_rows <= MAX_SESSION_BATCH_ROWS);
        assert!(cleanup.deleted_bytes <= MAX_SESSION_BATCH_BYTES);
        if cleanup.complete {
            break;
        }
    }
    assert_eq!(
        store
            .load_session_source(&source_key)
            .unwrap()
            .unwrap()
            .staging_generation,
        None
    );

    final_cursor.generation = 3;
    store.begin_or_resume_candidate(&final_cursor).unwrap();
    store
        .commit_candidate_batch(&SessionBatch {
            cursor: Some(final_cursor),
            index_records: vec![index_record(&source_key, 3, 4000)],
            ..SessionBatch::default()
        })
        .unwrap();
    store
        .promote_candidate(&source_key, 3, "2026-07-26T00:03:00Z")
        .unwrap();
    let source = store.load_session_source(&source_key).unwrap().unwrap();
    assert_eq!(source.current_generation, Some(3));
    assert_eq!(source.retired_generation, Some(1));
    assert_eq!(
        store
            .load_current_session_index_page(&source_key, None, 200)
            .unwrap()
            .items,
        [index_record(&source_key, 3, 4000)]
    );
}

#[test]
fn faults_after_each_of_three_candidate_batches_never_change_current_visibility() {
    for failure_after_batch in 1..=3_u64 {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let source_key = opaque(600 + failure_after_batch);
        let mut store = StateStore::open(&path).unwrap();
        promote_one_record(&mut store, &source_key, 1, 6000);
        let initial = cursor(&source_key, 2, 0);
        store.begin_or_resume_candidate(&initial).unwrap();

        let candidate_batches = (1..=3_u64)
            .map(|batch_number| {
                (
                    cursor(&source_key, 2, batch_number * 100),
                    index_record(&source_key, 2, 6100 + batch_number),
                )
            })
            .collect::<Vec<_>>();
        for (batch_cursor, batch_index) in candidate_batches
            .into_iter()
            .take(failure_after_batch as usize)
        {
            store
                .commit_candidate_batch(&SessionBatch {
                    cursor: Some(batch_cursor),
                    index_records: vec![batch_index],
                    ..SessionBatch::default()
                })
                .unwrap();
        }
        drop(store);

        let mut store = StateStore::open(&path).unwrap();
        assert_eq!(
            store
                .load_current_session_index_page(&source_key, None, 200)
                .unwrap()
                .items,
            [index_record(&source_key, 1, 6000)]
        );
        let persisted = cursor(&source_key, 2, failure_after_batch * 100);
        assert_eq!(
            store.begin_or_resume_candidate(&persisted).unwrap(),
            CandidateBeginOutcome::Resumed(Box::new(persisted.clone()))
        );

        if failure_after_batch == 3 {
            let mut invalid_final = index_record(&source_key, 2, 6200);
            invalid_final.message_count = 2;
            let mut prior = invalid_final.clone();
            prior.message_count = 3;
            store
                .commit_candidate_batch(&SessionBatch {
                    cursor: Some(persisted.clone()),
                    index_records: vec![prior],
                    ..SessionBatch::default()
                })
                .unwrap();
            assert!(matches!(
                store
                    .commit_candidate_batch(&SessionBatch {
                        cursor: Some(cursor(&source_key, 2, 400)),
                        index_records: vec![invalid_final],
                        ..SessionBatch::default()
                    })
                    .unwrap_err(),
                StorageError::StableRecordConflict { .. }
            ));
            assert_eq!(
                store
                    .load_current_session_index_page(&source_key, None, 200)
                    .unwrap()
                    .items,
                [index_record(&source_key, 1, 6000)]
            );
        }
    }
}

#[test]
fn successful_multibatch_candidate_promotes_with_one_pointer_flip() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let source_key = opaque(5);
    let mut store = StateStore::open(&path).unwrap();
    promote_one_record(&mut store, &source_key, 1, 5000);
    let candidate_cursor = cursor(&source_key, 2, 100);
    store.begin_or_resume_candidate(&candidate_cursor).unwrap();
    let first_batch = (0..MAX_SESSION_BATCH_ROWS)
        .map(|offset| index_record(&source_key, 2, 6000 + offset as u64))
        .collect::<Vec<_>>();
    store
        .commit_candidate_batch(&SessionBatch {
            cursor: Some(candidate_cursor),
            index_records: first_batch,
            ..SessionBatch::default()
        })
        .unwrap();
    store
        .commit_candidate_batch(&SessionBatch {
            cursor: Some(cursor(&source_key, 2, 200)),
            index_records: vec![index_record(&source_key, 2, 7000)],
            ..SessionBatch::default()
        })
        .unwrap();

    assert_eq!(
        store
            .load_current_session_index_page(&source_key, None, 200)
            .unwrap()
            .items,
        [index_record(&source_key, 1, 5000)]
    );
    store
        .promote_candidate(&source_key, 2, "2026-07-26T00:04:00Z")
        .unwrap();

    let connection = Connection::open(path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT current_generation FROM session_sources WHERE source_key = ?1",
                [&source_key],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        2
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM session_index i
                 JOIN session_sources s
                   ON s.source_key = i.source_key
                  AND s.current_generation = i.generation
                 WHERE i.source_key = ?1",
                [&source_key],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        (MAX_SESSION_BATCH_ROWS + 1) as i64
    );
}

#[test]
fn bounded_source_and_global_current_pages_restore_only_visible_generations() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = StateStore::open(directory.path().join("state.db")).unwrap();
    let first_source = opaque(106);
    let second_source = opaque(107);
    for (source_key, session_value, usage_value) in [
        (&first_source, 1060_u64, 2060_u64),
        (&second_source, 1070, 2070),
    ] {
        let generation_cursor = cursor(source_key, 1, 100);
        store.begin_or_resume_candidate(&generation_cursor).unwrap();
        store
            .commit_candidate_batch(&SessionBatch {
                cursor: Some(generation_cursor),
                index_records: vec![index_record(source_key, 1, session_value)],
                usage_records: vec![usage_record(source_key, 1, session_value, usage_value, 1)],
                ..SessionBatch::default()
            })
            .unwrap();
        store.promote_candidate(source_key, 1, NOW).unwrap();
    }

    let first_source_page = store.load_session_sources_page(None, 1).unwrap();
    assert_eq!(first_source_page.items.len(), 1);
    let second_source_page = store
        .load_session_sources_page(first_source_page.next_page_key.as_ref(), 1)
        .unwrap();
    assert_eq!(second_source_page.items.len(), 1);
    assert_eq!(second_source_page.next_page_key, None);
    assert!(matches!(
        store.load_session_sources_page(None, 513).unwrap_err(),
        StorageError::InvalidStateRecord { .. }
    ));

    let global_index = store
        .load_global_current_session_index_page(None, 200)
        .unwrap();
    assert_eq!(global_index.items.len(), 2);
    assert!(
        global_index
            .items
            .iter()
            .all(|record| record.generation == 1)
    );
    let global_usage = store
        .load_global_current_session_usage_page(None, 500)
        .unwrap();
    assert_eq!(global_usage.items.len(), 2);
    assert!(
        global_usage
            .items
            .iter()
            .all(|record| record.generation == 1)
    );

    let replacement_cursor = cursor(&first_source, 2, 200);
    store
        .begin_or_resume_candidate(&replacement_cursor)
        .unwrap();
    store
        .commit_candidate_batch(&SessionBatch {
            cursor: Some(replacement_cursor),
            index_records: vec![index_record(&first_source, 2, 1080)],
            usage_records: vec![usage_record(&first_source, 2, 1080, 2080, 1)],
            ..SessionBatch::default()
        })
        .unwrap();
    store
        .promote_candidate(&first_source, 2, "2026-07-26T00:03:00Z")
        .unwrap();

    let global_index = store
        .load_global_current_session_index_page(None, 200)
        .unwrap();
    assert_eq!(global_index.items.len(), 2);
    assert!(global_index.items.iter().any(|record| {
        record.source_key == first_source
            && record.generation == 2
            && record.session_key == opaque(1080)
    }));
    assert!(
        !global_index
            .items
            .iter()
            .any(|record| record.source_key == first_source && record.generation == 1)
    );
}

#[test]
fn replay_pages_are_ordered_capped_generation_bound_and_stale_after_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let source_key = opaque(6);
    let mut store = StateStore::open(&path).unwrap();
    let first_cursor = cursor(&source_key, 1, 100);
    store.begin_or_resume_candidate(&first_cursor).unwrap();
    store
        .commit_candidate_batch(&SessionBatch {
            cursor: Some(first_cursor),
            replay_signatures: vec![
                replay_signature(&source_key, 1, 2),
                replay_signature(&source_key, 1, 1),
            ],
            ..SessionBatch::default()
        })
        .unwrap();
    store.promote_candidate(&source_key, 1, NOW).unwrap();

    let first = store
        .load_codex_replay_signature_page(&source_key, 1, None, 1)
        .unwrap();
    assert_eq!(first.items, [replay_signature(&source_key, 1, 1)]);
    let key = first.next_page_key.unwrap();
    let second = store
        .load_codex_replay_signature_page(&source_key, 1, Some(&key), 1)
        .unwrap();
    assert_eq!(second.items, [replay_signature(&source_key, 1, 2)]);
    assert_eq!(second.next_page_key, None);
    assert!(matches!(
        store
            .load_codex_replay_signature_page(&source_key, 1, None, MAX_SESSION_BATCH_ROWS + 1,)
            .unwrap_err(),
        StorageError::InvalidStateRecord { .. }
    ));

    let second_cursor = cursor(&source_key, 2, 100);
    store.begin_or_resume_candidate(&second_cursor).unwrap();
    store
        .commit_candidate_batch(&SessionBatch {
            cursor: Some(second_cursor),
            replay_signatures: vec![replay_signature(&source_key, 2, 1)],
            ..SessionBatch::default()
        })
        .unwrap();
    store
        .promote_candidate(&source_key, 2, "2026-07-26T00:05:00Z")
        .unwrap();
    assert!(matches!(
        store
            .load_codex_replay_signature_page(&source_key, 1, Some(&key), 1)
            .unwrap_err(),
        StorageError::StalePageKey
    ));
}

#[test]
fn empty_and_oversized_batches_are_rejected_without_writes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let source_key = opaque(7);
    let mut store = StateStore::open(&path).unwrap();
    let wal_before = store.wal_size_bytes().unwrap();

    store
        .commit_session_batch(&SessionBatch::default())
        .unwrap();
    assert_eq!(store.wal_size_bytes().unwrap(), wal_before);

    let generation_cursor = cursor(&source_key, 1, 0);
    store.begin_or_resume_candidate(&generation_cursor).unwrap();
    let too_many = (0..=MAX_SESSION_BATCH_ROWS)
        .map(|offset| index_record(&source_key, 1, 8000 + offset as u64))
        .collect::<Vec<_>>();
    assert!(matches!(
        store
            .commit_candidate_batch(&SessionBatch {
                cursor: Some(generation_cursor),
                index_records: too_many,
                ..SessionBatch::default()
            })
            .unwrap_err(),
        StorageError::SessionBatchLimitExceeded
    ));
    assert_eq!(
        Connection::open(path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM session_index", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        0
    );
}

#[test]
fn repeated_unchanged_success_and_identical_failure_are_zero_write() {
    let directory = tempfile::tempdir().unwrap();
    let source_key = opaque(8);
    let mut store = StateStore::open(directory.path().join("state.db")).unwrap();
    promote_one_record(&mut store, &source_key, 1, 9000);
    let wal_after_promotion = store.wal_size_bytes().unwrap();

    assert!(
        !store
            .record_source_success(&source_key, 1, "2026-07-26T00:10:00Z")
            .unwrap()
    );
    assert_eq!(store.wal_size_bytes().unwrap(), wal_after_promotion);
    assert!(
        store
            .fail_candidate(
                &source_key,
                1,
                SessionSourceErrorCode::SourceRootMissing,
                "2026-07-26T00:11:00Z",
            )
            .unwrap()
    );
    let wal_after_failure = store.wal_size_bytes().unwrap();
    assert!(
        !store
            .fail_candidate(
                &source_key,
                1,
                SessionSourceErrorCode::SourceRootMissing,
                "2026-07-26T00:12:00Z",
            )
            .unwrap()
    );
    assert_eq!(store.wal_size_bytes().unwrap(), wal_after_failure);
    let mut retained = index_record(&source_key, 1, 9000);
    retained.availability = SessionAvailability::Unavailable;
    assert_eq!(
        store
            .load_current_session_index_page(&source_key, None, 200)
            .unwrap()
            .items,
        [retained]
    );
}

#[test]
fn stale_source_state_update_cannot_overwrite_a_new_current_generation() {
    let directory = tempfile::tempdir().unwrap();
    let source_key = opaque(105);
    let mut store = StateStore::open(directory.path().join("state.db")).unwrap();
    promote_one_record(&mut store, &source_key, 1, 1050);
    promote_one_record(&mut store, &source_key, 2, 1051);
    store
        .fail_candidate(
            &source_key,
            2,
            SessionSourceErrorCode::SourceIoFailed,
            "2026-07-26T00:01:00Z",
        )
        .unwrap();

    assert!(matches!(
        store
            .record_source_success(&source_key, 1, "2026-07-26T00:02:00Z")
            .unwrap_err(),
        StorageError::CandidateStateConflict
    ));
    let source = store.load_session_source(&source_key).unwrap().unwrap();
    assert_eq!(source.current_generation, Some(2));
    assert_eq!(source.status, SessionSourceStatus::Stale);
    assert_eq!(
        source.error_code,
        Some(SessionSourceErrorCode::SourceIoFailed)
    );
}

#[test]
fn source_without_success_becomes_unavailable_without_aggregate_deletion() {
    let directory = tempfile::tempdir().unwrap();
    let source_key = opaque(9);
    let mut store = StateStore::open(directory.path().join("state.db")).unwrap();
    let generation_cursor = cursor(&source_key, 1, 0);
    store.begin_or_resume_candidate(&generation_cursor).unwrap();

    store
        .fail_candidate(
            &source_key,
            1,
            SessionSourceErrorCode::SourceSessionsAbsent,
            NOW,
        )
        .unwrap();

    let state = store.load_session_source(&source_key).unwrap().unwrap();
    assert_eq!(state.current_generation, None);
    assert_eq!(state.status, SessionSourceStatus::Unavailable);
    assert_eq!(
        state.error_code,
        Some(SessionSourceErrorCode::SourceSessionsAbsent)
    );
}

#[test]
fn current_index_derives_effective_availability_from_source_status() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let source_key = opaque(145);
    let mut store = StateStore::open(&path).unwrap();
    promote_one_record(&mut store, &source_key, 1, 1450);

    store
        .fail_candidate(
            &source_key,
            1,
            SessionSourceErrorCode::SourceIoFailed,
            "2026-07-26T00:01:00Z",
        )
        .unwrap();

    let current = store
        .load_current_session_index_page(&source_key, None, 200)
        .unwrap();
    assert_eq!(
        current.items[0].availability,
        SessionAvailability::Unavailable
    );
    let global = store
        .load_global_current_session_index_page(None, 200)
        .unwrap();
    assert_eq!(
        global.items[0].availability,
        SessionAvailability::Unavailable
    );

    let connection = Connection::open(&path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT availability FROM session_index
                 WHERE source_key = ?1 AND generation = 1",
                [&source_key],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "available"
    );
    drop(connection);

    store
        .record_source_success(&source_key, 1, "2026-07-26T00:02:00Z")
        .unwrap();
    assert_eq!(
        store
            .load_current_session_index_page(&source_key, None, 200)
            .unwrap()
            .items[0]
            .availability,
        SessionAvailability::Available
    );
    assert_eq!(
        store
            .load_global_current_session_index_page(None, 200)
            .unwrap()
            .items[0]
            .availability,
        SessionAvailability::Available
    );
}

#[test]
fn supplemental_batch_is_bounded_atomic_retained_and_drops_under_pressure() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let mut store = StateStore::open(&path).unwrap();
    let wal_before = store.wal_size_bytes().unwrap();

    let empty = store.record_request_supplemental_batch(&[]).unwrap();
    assert_eq!(empty.inserted_rows, 0);
    assert_eq!(empty.dropped_rows, 0);
    assert_eq!(store.wal_size_bytes().unwrap(), wal_before);

    let inserted = store
        .record_request_supplemental_batch(&[supplemental("request-1")])
        .unwrap();
    assert_eq!(inserted.inserted_rows, 1);
    assert_eq!(inserted.dropped_rows, 0);
    let stats = store.inspect_request_supplemental().unwrap();
    assert_eq!(stats.rows, 1);
    assert!(stats.logical_bytes <= 2 * 1024);

    let replay_wal = store.wal_size_bytes().unwrap();
    let replay = store
        .record_request_supplemental_batch(&[supplemental("request-1")])
        .unwrap();
    assert_eq!(replay.inserted_rows, 0);
    assert_eq!(replay.dropped_rows, 0);
    assert_eq!(store.wal_size_bytes().unwrap(), replay_wal);

    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE request_supplemental_metadata
             SET occurred_at = '2026-07-24T00:00:00Z'
             WHERE request_id = 'request-1'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "WITH RECURSIVE ids(value) AS (
                VALUES(2)
                UNION ALL
                SELECT value + 1 FROM ids WHERE value < ?1
             )
             INSERT INTO request_supplemental_metadata(
                request_id, attempt_id, trace_id, occurred_at, route_fingerprint,
                provider_fingerprint, account_fingerprint, retry_decision, failover_decision,
                queue_ms, connect_ms, first_byte_ms, total_ms, request_bytes, response_bytes,
                status_code, error_code, logical_bytes
             )
             SELECT printf('request-%d', value), 'attempt-1', 'trace-1',
                    '2026-07-24T00:00:00Z', ?2, ?3, NULL, 'none', 'none',
                    0, 0, 0, 0, 0, 0, NULL, NULL, 256
             FROM ids",
            params![MAX_SUPPLEMENTAL_ROWS as i64, opaque(301), opaque(302)],
        )
        .unwrap();
    drop(connection);
    assert_eq!(
        store.inspect_request_supplemental().unwrap().rows,
        MAX_SUPPLEMENTAL_ROWS
    );

    let dropped = store
        .record_request_supplemental_batch(&[supplemental("pressure")])
        .unwrap();
    assert_eq!(dropped.inserted_rows, 0);
    assert_eq!(dropped.dropped_rows, 1);
    let cleanup = store
        .cleanup_request_supplemental("2026-07-26T00:00:01Z", MAX_SESSION_BATCH_ROWS)
        .unwrap();
    assert_eq!(cleanup.deleted_rows, MAX_SESSION_BATCH_ROWS);
    assert!(cleanup.deleted_bytes <= MAX_SESSION_BATCH_BYTES);
    let accepted = store
        .record_request_supplemental_batch(&[supplemental("after-cleanup")])
        .unwrap();
    assert_eq!(accepted.inserted_rows, 1);
    assert_eq!(accepted.dropped_rows, 0);
}

#[test]
fn every_session_batch_path_reports_exact_supplemental_pressure_drops() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let mut store = StateStore::open(&path).unwrap();
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "WITH RECURSIVE ids(value) AS (
                VALUES(1)
                UNION ALL
                SELECT value + 1 FROM ids WHERE value < ?1
             )
             INSERT INTO request_supplemental_metadata(
                request_id, attempt_id, trace_id, occurred_at, route_fingerprint,
                provider_fingerprint, account_fingerprint, retry_decision, failover_decision,
                queue_ms, connect_ms, first_byte_ms, total_ms, request_bytes, response_bytes,
                status_code, error_code, logical_bytes
             )
             SELECT printf('capacity-%d', value), 'attempt-1', 'trace-1',
                    ?2, ?3, ?4, NULL, 'none', 'none',
                    0, 0, 0, 0, 0, 0, NULL, NULL, 256
             FROM ids",
            params![MAX_SUPPLEMENTAL_ROWS as i64, NOW, opaque(401), opaque(402)],
        )
        .unwrap();
    drop(connection);

    let outcome = store
        .commit_session_batch(&SessionBatch {
            supplemental_metadata: vec![
                supplemental("capacity-bypass-1"),
                supplemental("capacity-bypass-2"),
            ],
            ..SessionBatch::default()
        })
        .unwrap();
    assert_eq!(outcome.inserted_rows, 0);
    assert_eq!(outcome.dropped_rows, 2);

    assert_eq!(
        store.inspect_request_supplemental().unwrap().rows,
        MAX_SUPPLEMENTAL_ROWS
    );

    let source_key = opaque(403);
    let candidate_cursor = cursor(&source_key, 1, 100);
    store.begin_or_resume_candidate(&candidate_cursor).unwrap();
    let outcome = store
        .commit_candidate_batch(&SessionBatch {
            cursor: Some(candidate_cursor),
            index_records: vec![index_record(&source_key, 1, 4030)],
            supplemental_metadata: vec![supplemental("candidate-pressure")],
            ..SessionBatch::default()
        })
        .unwrap();
    assert_eq!(outcome.inserted_rows, 0);
    assert_eq!(outcome.dropped_rows, 1);
    store.promote_candidate(&source_key, 1, NOW).unwrap();
    assert_eq!(
        store
            .load_current_session_index_page(&source_key, None, 200)
            .unwrap()
            .items,
        [index_record(&source_key, 1, 4030)]
    );
}

#[test]
fn replay_rollout_hard_limit_rejects_one_more_signature() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let source_key = opaque(10);
    let mut store = StateStore::open(&path).unwrap();
    promote_one_record(&mut store, &source_key, 1, 10_000);
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "WITH RECURSIVE ordinals(value) AS (
                VALUES(1)
                UNION ALL
                SELECT value + 1 FROM ordinals WHERE value < ?1
             )
             INSERT INTO codex_replay_signatures(
                parent_source_key, parent_generation, token_event_ordinal,
                occurred_at, signature_hash
             )
             SELECT ?2, 1, value, ?3, zeroblob(32) FROM ordinals",
            params![MAX_CODEX_REPLAY_SIGNATURES as i64, source_key, NOW],
        )
        .unwrap();
    drop(connection);
    let mut current_cursor = cursor(&source_key, 1, 100);
    current_cursor.generation_state = SessionGenerationState::Current;

    assert!(matches!(
        store
            .commit_session_batch(&SessionBatch {
                cursor: Some(current_cursor),
                replay_signatures: vec![replay_signature(
                    &source_key,
                    1,
                    MAX_CODEX_REPLAY_SIGNATURES + 1,
                )],
                ..SessionBatch::default()
            })
            .unwrap_err(),
        StorageError::ReplaySignatureLimitExceeded
    ));
}

#[test]
fn schema_three_offline_and_live_read_only_inspection_do_not_write() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let store = StateStore::open(&path).unwrap();
    let before_live = directory_snapshot(directory.path());

    let live = ReadOnlyStateStore::open_live(&path).unwrap();
    assert_eq!(live.health().unwrap().schema_version, 3);
    drop(live);
    assert_eq!(directory_snapshot(directory.path()), before_live);
    drop(store);
    let before_offline = directory_snapshot(directory.path());

    let offline = ReadOnlyStateStore::open(&path).unwrap();
    assert_eq!(offline.health().unwrap().schema_version, 3);
    drop(offline);
    assert_eq!(directory_snapshot(directory.path()), before_offline);
}

#[test]
fn supplemental_row_and_batch_byte_limits_roll_back_atomically() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let mut store = StateStore::open(&path).unwrap();
    let dense = dense_supplemental(1);

    let outcome = store
        .record_request_supplemental_batch(std::slice::from_ref(&dense))
        .unwrap();
    assert_eq!(outcome.inserted_rows, 1);
    assert!(
        store.inspect_request_supplemental().unwrap().logical_bytes
            <= wokcore_storage::MAX_SUPPLEMENTAL_ROW_BYTES
    );

    let mut invalid = supplemental("invalid");
    invalid.occurred_at = "not-a-timestamp".to_owned();
    assert!(matches!(
        store
            .record_request_supplemental_batch(&[supplemental("atomic"), invalid])
            .unwrap_err(),
        StorageError::InvalidStateRecord { .. }
    ));
    assert_eq!(store.inspect_request_supplemental().unwrap().rows, 1);

    let oversized = (2..452).map(dense_supplemental).collect::<Vec<_>>();
    assert!(matches!(
        store
            .record_request_supplemental_batch(&oversized)
            .unwrap_err(),
        StorageError::SessionBatchLimitExceeded
    ));
    assert_eq!(store.inspect_request_supplemental().unwrap().rows, 1);
}

#[test]
fn timestamps_require_one_canonical_utc_second_representation() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = StateStore::open(directory.path().join("state.db")).unwrap();
    for (offset, invalid_timestamp) in [
        "2026-7-26T00:00:00Z",
        "2026-07-26T00:00:00+00:00",
        "2026-02-30T00:00:00Z",
        "2026-07-26T24:00:00Z",
        "2026-07-26T00:00:00.000Z",
        "C:\\sessionsT00:00:00Z",
    ]
    .into_iter()
    .enumerate()
    {
        let source_key = opaque(500 + offset as u64);
        let mut invalid = cursor(&source_key, 1, 0);
        invalid.modified_at = invalid_timestamp.to_owned();
        assert!(
            matches!(
                store.begin_or_resume_candidate(&invalid).unwrap_err(),
                StorageError::InvalidStateRecord { .. }
            ),
            "{invalid_timestamp}"
        );
    }
}

#[test]
fn persistent_identity_decision_code_and_http_status_fields_are_constrained() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = StateStore::open(directory.path().join("state.db")).unwrap();
    assert!(SessionFileIdentity::new("C:relative-session").is_err());
    assert!(RequestId::new("C:\\private\\request").is_err());
    assert!(SupplementalErrorCode::new("arbitrary error text").is_err());

    for (offset, mutate) in [(3_u64, "low-status"), (4, "high-status")] {
        let mut invalid = supplemental(&format!("typed-{offset}"));
        match mutate {
            "low-status" => invalid.status_code = Some(99),
            "high-status" => invalid.status_code = Some(600),
            _ => unreachable!(),
        }
        assert!(
            matches!(
                store
                    .record_request_supplemental_batch(&[invalid])
                    .unwrap_err(),
                StorageError::InvalidStateRecord { .. }
            ),
            "{mutate}"
        );
    }
}

#[test]
fn restart_identity_mismatch_and_single_retired_slot_require_bounded_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let source_key = opaque(11);
    let mut store = StateStore::open(directory.path().join("state.db")).unwrap();
    let first_cursor = cursor(&source_key, 1, 100);
    store.begin_or_resume_candidate(&first_cursor).unwrap();
    store
        .commit_candidate_batch(&SessionBatch {
            cursor: Some(first_cursor.clone()),
            index_records: vec![index_record(&source_key, 1, 11_000)],
            ..SessionBatch::default()
        })
        .unwrap();
    let mut changed_identity = cursor(&source_key, 1, 0);
    changed_identity.file_identity = SessionFileIdentity::new(opaque(401)).unwrap();
    assert_eq!(
        store.begin_or_resume_candidate(&changed_identity).unwrap(),
        CandidateBeginOutcome::CleanupRequired { generation: 1 }
    );
    loop {
        if store
            .cleanup_generation_batch(
                &source_key,
                1,
                MAX_SESSION_BATCH_ROWS,
                MAX_SESSION_BATCH_BYTES,
            )
            .unwrap()
            .complete
        {
            break;
        }
    }
    assert_eq!(
        store.begin_or_resume_candidate(&changed_identity).unwrap(),
        CandidateBeginOutcome::Started
    );
    store.promote_candidate(&source_key, 1, NOW).unwrap();

    promote_one_record(&mut store, &source_key, 2, 12_000);
    assert_eq!(
        store
            .begin_or_resume_candidate(&cursor(&source_key, 3, 0))
            .unwrap(),
        CandidateBeginOutcome::CleanupRequired { generation: 1 }
    );
    loop {
        if store
            .cleanup_generation_batch(
                &source_key,
                1,
                MAX_SESSION_BATCH_ROWS,
                MAX_SESSION_BATCH_BYTES,
            )
            .unwrap()
            .complete
        {
            break;
        }
    }
    assert_eq!(
        store
            .begin_or_resume_candidate(&cursor(&source_key, 3, 0))
            .unwrap(),
        CandidateBeginOutcome::Started
    );
}

#[test]
fn generation_cleanup_accounts_for_unicode_in_utf8_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let source_key = opaque(108);
    let mut store = StateStore::open(&path).unwrap();
    let first_cursor = cursor(&source_key, 1, 100);
    let mut unicode_usage = usage_record(&source_key, 1, 1080, 2080, 1);
    unicode_usage.model = "界".repeat(10);
    store.begin_or_resume_candidate(&first_cursor).unwrap();
    store
        .commit_candidate_batch(&SessionBatch {
            cursor: Some(first_cursor),
            usage_records: vec![unicode_usage],
            ..SessionBatch::default()
        })
        .unwrap();
    store.promote_candidate(&source_key, 1, NOW).unwrap();
    promote_one_record(&mut store, &source_key, 2, 1081);

    let cleanup = store
        .cleanup_generation_batch(&source_key, 1, MAX_SESSION_BATCH_ROWS, 290)
        .unwrap();
    assert_eq!(cleanup.deleted_rows, 0);
    assert_eq!(cleanup.deleted_bytes, 0);
    assert!(!cleanup.complete);
    drop(store);

    assert_eq!(
        Connection::open(path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM session_usage_records
                 WHERE source_key = ?1 AND generation = 1",
                [&source_key],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
}

#[test]
fn generation_cleanup_cursor_bytes_exactly_match_batch_accounting() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let source_key = opaque(146);
    let mut store = StateStore::open(&path).unwrap();
    let first_cursor = cursor(&source_key, 1, 100);
    store.begin_or_resume_candidate(&first_cursor).unwrap();
    store.promote_candidate(&source_key, 1, NOW).unwrap();
    let second_cursor = cursor(&source_key, 2, 100);
    store.begin_or_resume_candidate(&second_cursor).unwrap();
    store
        .promote_candidate(&source_key, 2, "2026-07-26T00:01:00Z")
        .unwrap();

    let connection = Connection::open(&path).unwrap();
    let checkpoint_bytes = usize::try_from(
        connection
            .query_row(
                "SELECT length(parser_checkpoint) FROM session_scan_cursors
                 WHERE source_key = ?1 AND generation = 1",
                [&source_key],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
    )
    .unwrap();
    drop(connection);
    let expected_cursor_bytes = source_key.len()
        + first_cursor.file_identity.as_str().len()
        + first_cursor.modified_at.len()
        + "advanced".len()
        + NOW.len()
        + first_cursor.parent_source_key.as_ref().unwrap().len()
        + checkpoint_bytes
        + 128;

    let too_small = store
        .cleanup_generation_batch(
            &source_key,
            1,
            MAX_SESSION_BATCH_ROWS,
            expected_cursor_bytes - 1,
        )
        .unwrap();
    assert_eq!(too_small.deleted_rows, 0);
    assert_eq!(too_small.deleted_bytes, 0);
    assert!(!too_small.complete);

    let exact = store
        .cleanup_generation_batch(
            &source_key,
            1,
            MAX_SESSION_BATCH_ROWS,
            expected_cursor_bytes,
        )
        .unwrap();
    assert_eq!(exact.deleted_rows, 1);
    assert_eq!(exact.deleted_bytes, expected_cursor_bytes);
    assert!(exact.complete);
}

#[test]
fn every_source_kind_replacement_exposes_only_latest_usage_generation() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = StateStore::open(directory.path().join("state.db")).unwrap();
    for (offset, source_kind) in [
        SessionSourceKind::Codex,
        SessionSourceKind::Claude,
        SessionSourceKind::Gemini,
    ]
    .into_iter()
    .enumerate()
    {
        let source_key = opaque(20 + offset as u64);
        let mut first_cursor = cursor(&source_key, 1, 100);
        first_cursor.source_kind = source_kind;
        let mut first_index = index_record(&source_key, 1, 20_000 + offset as u64);
        first_index.source_kind = source_kind;
        let mut first_usage = usage_record(
            &source_key,
            1,
            20_000 + offset as u64,
            21_000 + offset as u64,
            1,
        );
        first_usage.source_kind = source_kind;
        store.begin_or_resume_candidate(&first_cursor).unwrap();
        store
            .commit_candidate_batch(&SessionBatch {
                cursor: Some(first_cursor),
                index_records: vec![first_index],
                usage_records: vec![first_usage],
                ..SessionBatch::default()
            })
            .unwrap();
        store.promote_candidate(&source_key, 1, NOW).unwrap();

        let mut second_cursor = cursor(&source_key, 2, 200);
        second_cursor.source_kind = source_kind;
        let mut second_index = index_record(&source_key, 2, 22_000 + offset as u64);
        second_index.source_kind = source_kind;
        let mut second_usage = usage_record(
            &source_key,
            2,
            22_000 + offset as u64,
            23_000 + offset as u64,
            1,
        );
        second_usage.source_kind = source_kind;
        store.begin_or_resume_candidate(&second_cursor).unwrap();
        store
            .commit_candidate_batch(&SessionBatch {
                cursor: Some(second_cursor),
                index_records: vec![second_index],
                usage_records: vec![second_usage.clone()],
                ..SessionBatch::default()
            })
            .unwrap();
        store
            .promote_candidate(&source_key, 2, "2026-07-26T01:00:00Z")
            .unwrap();

        assert_eq!(
            store
                .load_current_session_usage_page(&source_key, None, 500)
                .unwrap()
                .items,
            [second_usage]
        );
    }
}

fn directory_snapshot(path: &Path) -> Vec<(String, Vec<u8>)> {
    let mut snapshot = fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    snapshot.sort_by(|left, right| left.0.cmp(&right.0));
    snapshot
}
