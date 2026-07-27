use std::{fs, sync::Arc};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use secrecy::ExposeSecret;
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;
use wokcore_core::{
    id::ProviderId,
    secret::{SecretPurpose, SecretScope},
};
use wokcore_server::{
    ServerState,
    api::build_router,
    auth::{AuthRegistry, EntropySource, StateAuthMetadataStore, TokenError, TokenMaterial},
    lifecycle::ServiceLifecycle,
    observability::SessionRootPaths,
    query::{DEFAULT_QUERY_WORKERS, QueryRuntime, QueryService},
};
use wokcore_sessions::codex::{CodexScanner, ScanControl};
use wokcore_storage::{
    MemorySecretStore, ParserCheckpoint, SessionAvailability, SessionBatch, SessionFileIdentity,
    SessionGenerationState, SessionIndexRecord, SessionScanCursor, SessionScanResultCode,
    SessionSourceKind, SessionUsageRecord, StateStore,
};

const AUTHORITY: &str = "127.0.0.1:43129";
const CREATED_AT: &str = "2026-07-26T00:00:00Z";

#[derive(Debug)]
struct FixedEntropy;

impl EntropySource for FixedEntropy {
    fn fill(&self, output: &mut [u8; 32]) -> Result<(), TokenError> {
        output.fill(0x55);
        Ok(())
    }
}

struct Fixture {
    app: axum::Router,
    management: String,
    query: QueryService,
    diagnostics_root: std::path::PathBuf,
    _directory: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let state_path = directory.path().join("state.db");
    let mut store = StateStore::open(&state_path).unwrap();
    seed_source(
        &mut store,
        opaque(1),
        SessionSourceKind::Codex,
        &[
            (opaque(101), "2026-07-26T03:00:00Z"),
            (opaque(102), "2026-07-26T02:00:00Z"),
        ],
    );
    seed_source(
        &mut store,
        opaque(2),
        SessionSourceKind::Claude,
        &[(opaque(201), "2026-07-26T01:00:00Z")],
    );
    let metadata = Arc::new(StateAuthMetadataStore::new(store));
    let secrets = Arc::new(MemorySecretStore::default());
    let scope = SecretScope {
        provider_id: ProviderId::new("wokcore-runtime").unwrap(),
        account_id: None,
        purpose: SecretPurpose::Auxiliary,
    };
    let auth = AuthRegistry::bootstrap(
        secrets,
        metadata,
        Arc::new(FixedEntropy),
        scope,
        CREATED_AT.to_owned(),
    )
    .await
    .unwrap();
    let management = TokenMaterial::generate_admin(&FixedEntropy)
        .unwrap()
        .into_response_value()
        .expose_secret()
        .to_owned();
    let domain_key = auth.session_domain_key();
    let query = QueryService::start(DEFAULT_QUERY_WORKERS).unwrap();
    let diagnostics_root = directory.path().join("diagnostics");
    fs::create_dir_all(&diagnostics_root).unwrap();
    fs::write(
        diagnostics_root.join("segment-00000000000000000001.jsonl"),
        [
            canonical_event(1, "2026-07-26T01:00:00Z", "info", "sessions", "request-1"),
            canonical_event(2, "2026-07-26T02:00:00Z", "warn", "storage", "request-2"),
            canonical_event(3, "2026-07-26T03:00:00Z", "error", "sessions", "request-3"),
        ]
        .concat(),
    )
    .unwrap();
    let runtime = QueryRuntime::new(
        query.handle(),
        &state_path,
        None,
        domain_key,
        diagnostics_root,
    );
    let lifecycle = ServiceLifecycle::new();
    lifecycle.mark_running().unwrap();
    let state = ServerState::new(
        AUTHORITY,
        Uuid::parse_str("019844f0-4de0-7000-8000-000000000011").unwrap(),
        Arc::new(auth),
        lifecycle,
    )
    .with_query_runtime(runtime);
    Fixture {
        app: build_router(state),
        management,
        query,
        diagnostics_root: directory.path().join("diagnostics"),
        _directory: directory,
    }
}

async fn message_fixture(content: &str) -> (Fixture, String) {
    let directory = tempfile::tempdir().unwrap();
    let state_path = directory.path().join("state.db");
    let store = StateStore::open(&state_path).unwrap();
    let metadata = Arc::new(StateAuthMetadataStore::new(store));
    let secrets = Arc::new(MemorySecretStore::default());
    let scope = SecretScope {
        provider_id: ProviderId::new("wokcore-runtime").unwrap(),
        account_id: None,
        purpose: SecretPurpose::Auxiliary,
    };
    let auth = AuthRegistry::bootstrap(
        secrets,
        metadata,
        Arc::new(FixedEntropy),
        scope,
        CREATED_AT.to_owned(),
    )
    .await
    .unwrap();
    let management = TokenMaterial::generate_admin(&FixedEntropy)
        .unwrap()
        .into_response_value()
        .expose_secret()
        .to_owned();
    let domain_key = auth.session_domain_key();
    let roots = SessionRootPaths {
        codex: directory.path().join("codex"),
        claude: directory.path().join("claude"),
        gemini: directory.path().join("gemini"),
    };
    fs::create_dir_all(roots.codex.join("sessions/2026/07/26")).unwrap();
    fs::create_dir_all(&roots.claude).unwrap();
    fs::create_dir_all(&roots.gemini).unwrap();
    let source = format!(
        "{{\"timestamp\":\"2026-07-26T12:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"api-messages\"}}}}\n\
         {{\"timestamp\":\"2026-07-26T12:00:01Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":{}}}]}}}}\n",
        serde_json::to_string(content).unwrap()
    );
    fs::write(
        roots.codex.join("sessions/2026/07/26/api-messages.jsonl"),
        source,
    )
    .unwrap();
    let mut scanner = CodexScanner::open(&roots.codex, &state_path, domain_key).unwrap();
    let scanned = scanner
        .scan("2026-07-26T12:30:00Z", ScanControl::default())
        .unwrap();
    let session_key = scanned.sources[0].session_key.clone().unwrap();
    let query = QueryService::start(DEFAULT_QUERY_WORKERS).unwrap();
    let runtime = QueryRuntime::new(
        query.handle(),
        &state_path,
        Some(roots),
        domain_key,
        directory.path().join("diagnostics"),
    );
    let lifecycle = ServiceLifecycle::new();
    lifecycle.mark_running().unwrap();
    let state = ServerState::new(
        AUTHORITY,
        Uuid::parse_str("019844f0-4de0-7000-8000-000000000012").unwrap(),
        Arc::new(auth),
        lifecycle,
    )
    .with_query_runtime(runtime);
    (
        Fixture {
            app: build_router(state),
            management,
            query,
            diagnostics_root: directory.path().join("diagnostics"),
            _directory: directory,
        },
        session_key,
    )
}

#[tokio::test]
async fn session_list_orders_filters_and_paginates_with_bound_cursors() {
    let fixture = fixture().await;
    let first = send(
        &fixture.app,
        "/wokcore/v1/sessions?limit=2",
        &fixture.management,
    )
    .await;
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(first.1["schema_version"], 1);
    assert_eq!(first.1["items"].as_array().unwrap().len(), 2);
    assert_eq!(first.1["items"][0]["session_key"], opaque(101));
    assert_eq!(first.1["items"][1]["session_key"], opaque(102));
    assert_eq!(first.1["index_status"]["phase"], "starting");
    assert_eq!(
        first.1["index_status"]["sources"].as_array().unwrap().len(),
        3
    );
    let cursor = first.1["next_cursor"].as_str().unwrap();

    let second = send(
        &fixture.app,
        &format!("/wokcore/v1/sessions?limit=2&before={cursor}"),
        &fixture.management,
    )
    .await;
    assert_eq!(second.0, StatusCode::OK);
    assert_eq!(second.1["items"].as_array().unwrap().len(), 1);
    assert_eq!(second.1["items"][0]["source"], "claude");
    assert!(second.1["next_cursor"].is_null());

    let cross_filter = send(
        &fixture.app,
        &format!("/wokcore/v1/sessions?source=codex&before={cursor}"),
        &fixture.management,
    )
    .await;
    assert_eq!(cross_filter.0, StatusCode::BAD_REQUEST);
    assert_eq!(cross_filter.1["error"]["code"], "invalid_cursor");

    fixture.query.shutdown().await.unwrap();
}

#[tokio::test]
async fn session_list_rejects_invalid_filters_limits_and_cursor_tampering() {
    let fixture = fixture().await;
    for (query, code) in [
        ("source=other", "invalid_query"),
        ("availability=stale", "invalid_query"),
        ("unknown=value", "invalid_query"),
        ("limit=0", "limit_out_of_range"),
        ("limit=201", "limit_out_of_range"),
        ("before=not-a-cursor", "invalid_cursor"),
    ] {
        let response = send(
            &fixture.app,
            &format!("/wokcore/v1/sessions?{query}"),
            &fixture.management,
        )
        .await;
        assert_eq!(response.0, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(response.1["error"]["code"], code, "{query}");
    }
    fixture.query.shutdown().await.unwrap();
}

#[tokio::test]
async fn session_messages_fragment_utf8_with_complete_json_byte_budgets() {
    let expected = "界🌐".repeat(2_000);
    let (fixture, session_key) = message_fixture(&expected).await;
    let mut cursor = None;
    let mut reconstructed = String::new();
    let mut message_key = None;
    let mut expected_offset = 0;

    loop {
        let path = match cursor.as_deref() {
            Some(cursor) => format!(
                "/wokcore/v1/sessions/{session_key}/messages?limit=1&max_bytes=4096&after={cursor}"
            ),
            None => format!("/wokcore/v1/sessions/{session_key}/messages?limit=1&max_bytes=4096"),
        };
        let (status, body, encoded_bytes) =
            send_with_size(&fixture.app, &path, &fixture.management).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(encoded_bytes <= 4096);
        assert_eq!(body["source_generation"], 1);
        let item = &body["items"][0];
        assert_eq!(item["fragment_offset_bytes"], expected_offset);
        let fragment = item["content"].as_str().unwrap();
        expected_offset += fragment.len();
        reconstructed.push_str(fragment);
        match &message_key {
            Some(key) => assert_eq!(item["message_key"], *key),
            None => message_key = Some(item["message_key"].clone()),
        }
        cursor = body["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            assert_eq!(item["fragment_final"], true);
            break;
        }
    }
    assert_eq!(reconstructed, expected);
    fixture.query.shutdown().await.unwrap();
}

#[tokio::test]
async fn session_messages_use_exact_limit_errors_and_unknown_session_status() {
    let (fixture, session_key) = message_fixture("visible").await;
    for (suffix, code) in [
        ("limit=0", "limit_out_of_range"),
        ("limit=501", "limit_out_of_range"),
        ("max_bytes=4095", "response_limit_out_of_range"),
        ("max_bytes=1048577", "response_limit_out_of_range"),
    ] {
        let response = send(
            &fixture.app,
            &format!("/wokcore/v1/sessions/{session_key}/messages?{suffix}"),
            &fixture.management,
        )
        .await;
        assert_eq!(response.0, StatusCode::BAD_REQUEST);
        assert_eq!(response.1["error"]["code"], code);
    }
    let missing = send(
        &fixture.app,
        &format!("/wokcore/v1/sessions/{}/messages", opaque(9999)),
        &fixture.management,
    )
    .await;
    assert_eq!(missing.0, StatusCode::NOT_FOUND);
    assert_eq!(missing.1["error"]["code"], "session_not_found");
    fixture.query.shutdown().await.unwrap();
}

#[tokio::test]
async fn usage_aggregates_filter_order_and_paginate_in_sql() {
    let fixture = fixture().await;
    let first = send(
        &fixture.app,
        "/wokcore/v1/usage?group_by=source&limit=1",
        &fixture.management,
    )
    .await;
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(first.1["group_by"], "source");
    assert_eq!(first.1["totals"]["input_tokens"], 30);
    assert_eq!(first.1["totals"]["session_count"], 3);
    assert_eq!(first.1["buckets"][0]["key"], "codex");
    assert_eq!(first.1["buckets"][0]["input_tokens"], 20);
    let cursor = first.1["next_cursor"].as_str().unwrap();

    let second = send(
        &fixture.app,
        &format!("/wokcore/v1/usage?group_by=source&limit=1&after={cursor}"),
        &fixture.management,
    )
    .await;
    assert_eq!(second.0, StatusCode::OK);
    assert_eq!(second.1["buckets"][0]["key"], "claude");
    assert!(second.1["next_cursor"].is_null());

    let ranged = send(
        &fixture.app,
        "/wokcore/v1/usage?since=2026-07-26T02:00:00Z&until=2026-07-26T03:00:00Z",
        &fixture.management,
    )
    .await;
    assert_eq!(ranged.0, StatusCode::OK);
    assert_eq!(ranged.1["totals"]["input_tokens"], 10);
    assert_eq!(ranged.1["buckets"][0]["key"], "2026-07-26");

    let reused = send(
        &fixture.app,
        &format!("/wokcore/v1/usage?group_by=model&after={cursor}"),
        &fixture.management,
    )
    .await;
    assert_eq!(reused.0, StatusCode::BAD_REQUEST);
    assert_eq!(reused.1["error"]["code"], "invalid_cursor");
    fixture.query.shutdown().await.unwrap();
}

#[tokio::test]
async fn usage_rejects_invalid_ranges_filters_and_limits() {
    let fixture = fixture().await;
    for (query, code) in [
        ("source=other", "invalid_query"),
        ("group_by=hour", "invalid_query"),
        ("limit=0", "limit_out_of_range"),
        ("limit=501", "limit_out_of_range"),
        ("since=not-time", "invalid_time_range"),
        (
            "since=2026-07-26T03:00:00Z&until=2026-07-26T03:00:00Z",
            "invalid_time_range",
        ),
    ] {
        let response = send(
            &fixture.app,
            &format!("/wokcore/v1/usage?{query}"),
            &fixture.management,
        )
        .await;
        assert_eq!(response.0, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(response.1["error"]["code"], code, "{query}");
    }
    fixture.query.shutdown().await.unwrap();
}

#[tokio::test]
async fn diagnostic_logs_read_persistent_segments_filter_and_paginate() {
    let fixture = fixture().await;
    let first = send(
        &fixture.app,
        "/wokcore/v1/logs?limit=2",
        &fixture.management,
    )
    .await;
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(first.1["items"].as_array().unwrap().len(), 2);
    assert_eq!(first.1["items"][0]["level"], "error");
    assert_eq!(first.1["items"][1]["level"], "warn");
    assert_eq!(first.1["dropped_events"], 0);
    let cursor = first.1["next_cursor"].as_str().unwrap();

    let second = send(
        &fixture.app,
        &format!("/wokcore/v1/logs?limit=2&after={cursor}"),
        &fixture.management,
    )
    .await;
    assert_eq!(second.0, StatusCode::OK);
    assert_eq!(second.1["items"].as_array().unwrap().len(), 1);
    assert_eq!(second.1["items"][0]["level"], "info");

    let filtered = send(
        &fixture.app,
        "/wokcore/v1/logs?level_min=warn&component=sessions",
        &fixture.management,
    )
    .await;
    assert_eq!(filtered.0, StatusCode::OK);
    assert_eq!(filtered.1["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        filtered.1["items"][0]["correlations"]["request_id"],
        "request-3"
    );
    fixture.query.shutdown().await.unwrap();
}

#[tokio::test]
async fn diagnostic_logs_reject_invalid_filters_ranges_limits_and_cursor_reuse() {
    let fixture = fixture().await;
    for (query, code) in [
        ("level_min=trace", "invalid_query"),
        ("component=unknown", "invalid_query"),
        ("order=newest", "invalid_query"),
        ("request_id=bad%20id", "invalid_query"),
        ("limit=0", "limit_out_of_range"),
        ("limit=1001", "limit_out_of_range"),
        ("since=not-time", "invalid_time_range"),
    ] {
        let response = send(
            &fixture.app,
            &format!("/wokcore/v1/logs?{query}"),
            &fixture.management,
        )
        .await;
        assert_eq!(response.0, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(response.1["error"]["code"], code, "{query}");
    }
    let first = send(
        &fixture.app,
        "/wokcore/v1/logs?order=asc&limit=1",
        &fixture.management,
    )
    .await;
    let cursor = first.1["next_cursor"].as_str().unwrap();
    let reused = send(
        &fixture.app,
        &format!("/wokcore/v1/logs?order=desc&after={cursor}"),
        &fixture.management,
    )
    .await;
    assert_eq!(reused.0, StatusCode::BAD_REQUEST);
    assert_eq!(reused.1["error"]["code"], "invalid_cursor");
    fixture.query.shutdown().await.unwrap();
}

#[tokio::test]
async fn diagnostic_logs_treat_an_empty_active_segment_as_an_empty_source() {
    let fixture = fixture().await;
    fs::write(
        fixture
            .diagnostics_root
            .join("segment-00000000000000000001.jsonl"),
        b"",
    )
    .unwrap();
    let response = send(&fixture.app, "/wokcore/v1/logs", &fixture.management).await;
    assert_eq!(response.0, StatusCode::OK);
    assert_eq!(response.1["items"], serde_json::json!([]));
    fixture.query.shutdown().await.unwrap();
}

#[tokio::test]
async fn diagnostic_export_streams_a_filtered_bounded_support_package() {
    let fixture = fixture().await;
    let response = send_raw(
        &fixture.app,
        concat!(
            "/wokcore/v1/diagnostics/export?",
            "request_id=request-2&",
            "since=2026-07-26T00:00:00Z&",
            "until=2026-07-26T04:00:00Z&",
            "max_bytes=65536"
        ),
        &fixture.management,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/zip");
    let disposition = response.headers()[header::CONTENT_DISPOSITION]
        .to_str()
        .unwrap();
    assert!(
        disposition.starts_with("attachment; filename=\"wokcore-diagnostics-")
            && disposition.ends_with("Z.zip\"")
            && disposition.len()
                == "attachment; filename=\"wokcore-diagnostics-20260726T000000Z.zip\"".len(),
        "{disposition}"
    );
    let declared = response.headers()[header::CONTENT_LENGTH]
        .to_str()
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    assert_eq!(bytes.len(), declared);
    assert!(bytes.starts_with(b"PK"));
    assert!(contains_bytes(&bytes, b"request-2"));
    assert!(!contains_bytes(&bytes, b"request-1"));
    assert!(!contains_bytes(&bytes, b"request-3"));
    fixture.query.shutdown().await.unwrap();
}

#[tokio::test]
async fn diagnostic_export_rejects_invalid_window_limits_and_unknown_fields() {
    let fixture = fixture().await;
    for (query, code) in [
        ("max_bytes=65535", "export_limit_out_of_range"),
        ("max_bytes=67108865", "export_limit_out_of_range"),
        ("since=not-time", "invalid_time_range"),
        (
            "since=2026-07-25T00:00:00Z&until=2026-07-26T00:00:01Z",
            "invalid_time_range",
        ),
        ("unknown=value", "invalid_query"),
    ] {
        let response = send(
            &fixture.app,
            &format!("/wokcore/v1/diagnostics/export?{query}"),
            &fixture.management,
        )
        .await;
        assert_eq!(response.0, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(response.1["error"]["code"], code, "{query}");
    }
    fixture.query.shutdown().await.unwrap();
}

async fn send(app: &axum::Router, path: &str, management: &str) -> (StatusCode, Value) {
    let (status, body, _) = send_with_size(app, path, management).await;
    (status, body)
}

async fn send_with_size(
    app: &axum::Router,
    path: &str,
    management: &str,
) -> (StatusCode, Value, usize) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path)
                .header(header::HOST, AUTHORITY)
                .header(header::AUTHORIZATION, format!("Bearer {management}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let encoded_bytes = bytes.len();
    (
        status,
        serde_json::from_slice(&bytes).unwrap(),
        encoded_bytes,
    )
}

async fn send_raw(app: &axum::Router, path: &str, management: &str) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path)
                .header(header::HOST, AUTHORITY)
                .header(header::AUTHORIZATION, format!("Bearer {management}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|candidate| candidate == needle)
}

fn seed_source(
    store: &mut StateStore,
    source_key: String,
    source_kind: SessionSourceKind,
    sessions: &[(String, &str)],
) {
    let cursor = scan_cursor(&source_key, source_kind);
    store.begin_or_resume_candidate(&cursor).unwrap();
    let index_records = sessions
        .iter()
        .map(|(session_key, last_active_at)| SessionIndexRecord {
            session_key: session_key.clone(),
            source_key: source_key.clone(),
            generation: 1,
            source_kind,
            created_at: CREATED_AT.to_owned(),
            last_active_at: (*last_active_at).to_owned(),
            message_count: 3,
            usage_event_count: 1,
            availability: SessionAvailability::Available,
        })
        .collect::<Vec<_>>();
    let usage_records = sessions
        .iter()
        .map(|(session_key, occurred_at)| SessionUsageRecord {
            usage_id: session_key.clone(),
            session_key: session_key.clone(),
            source_key: source_key.clone(),
            generation: 1,
            source_kind,
            model: match source_kind {
                SessionSourceKind::Codex => "gpt-5.6",
                SessionSourceKind::Claude => "claude-opus",
                SessionSourceKind::Gemini => "gemini-pro",
            }
            .to_owned(),
            occurred_at: (*occurred_at).to_owned(),
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 2,
            cache_write_tokens: 1,
            reasoning_tokens: 3,
            record_revision: 1,
        })
        .collect();
    store
        .commit_candidate_batch(&SessionBatch {
            cursor: Some(cursor),
            index_records,
            usage_records,
            ..SessionBatch::default()
        })
        .unwrap();
    store.promote_candidate(&source_key, 1, CREATED_AT).unwrap();
}

fn scan_cursor(source_key: &str, source_kind: SessionSourceKind) -> SessionScanCursor {
    SessionScanCursor {
        source_key: source_key.to_owned(),
        source_kind,
        generation: 1,
        generation_state: SessionGenerationState::Staging,
        file_identity: SessionFileIdentity::new(opaque(900)).unwrap(),
        observed_size: 128,
        modified_at: CREATED_AT.to_owned(),
        complete_byte_offset: 64,
        stable_record_ordinal: 1,
        parser_checkpoint: ParserCheckpoint {
            version: 1,
            previous_input_tokens: 0,
            previous_output_tokens: 0,
            previous_cache_read_tokens: 0,
            previous_cache_write_tokens: 0,
            previous_reasoning_tokens: 0,
            current_model: None,
            event_ordinal: 0,
            lineage_source_key: None,
            lineage_generation: None,
            lineage_record_ordinal: 0,
            structural_hash: None,
        },
        head_fingerprint: [1; 32],
        boundary_fingerprint: [2; 32],
        parent_source_key: None,
        parent_generation: None,
        replay_boundary_fingerprint: None,
        result_code: Some(SessionScanResultCode::Advanced),
        result_changed_at: Some(CREATED_AT.to_owned()),
    }
}

fn opaque(value: u64) -> String {
    format!("{value:064x}")
}

fn canonical_event(
    sequence: u64,
    occurred_at: &str,
    level: &str,
    component: &str,
    request_id: &str,
) -> Vec<u8> {
    format!(
        concat!(
            "{{\"schema_version\":1,\"sequence\":\"{sequence:020}\",",
            "\"event_id\":\"018f47a2-4c1d-7a8f-9b2d-{sequence:012x}\",",
            "\"occurred_at\":\"{occurred_at}\",\"level\":\"{level}\",",
            "\"component\":\"{component}\",\"code\":\"request_completed\",",
            "\"correlations\":{{\"request_id\":\"{request_id}\",\"trace_id\":null,",
            "\"attempt_id\":null,\"client_id\":null,\"parent_event_id\":null,",
            "\"opaque_session_id\":null}},",
            "\"build\":{{\"wokcore_version\":\"0.1.0\",",
            "\"git_commit\":\"0123456789abcdef0123456789abcdef01234567\",",
            "\"api_major\":1,\"capability_version\":3}},\"provider\":null,",
            "\"decision\":null,\"measurements\":null,\"error\":null,",
            "\"diagnostic_drop\":null,\"summaries\":[],\"redaction_counts\":{{",
            "\"authorization_values_removed\":0,\"cookie_values_removed\":0,",
            "\"body_values_removed\":0,\"path_values_removed\":0,",
            "\"token_values_removed\":0,\"credential_values_removed\":0}}}}\n"
        ),
        sequence = sequence,
        occurred_at = occurred_at,
        level = level,
        component = component,
        request_id = request_id,
    )
    .into_bytes()
}
