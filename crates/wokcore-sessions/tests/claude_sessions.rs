use std::{fs, io::Write, path::Path, time::SystemTime};

use tempfile::TempDir;
use wokcore_sessions::{
    claude::{ClaudeScanner, MAX_CLAUDE_LOGICAL_WORKING_BYTES},
    discovery::SessionDiscoverySliceBudget,
    messages::{MAX_MESSAGE_PAGE_UTF8_BYTES, MessagePageRequest, MessagePager, MessageRole},
    model::{SessionScanControl, SessionScanOutcome},
};
use wokcore_storage::{SessionSourceKind, SessionSourceStatus};

const NOW: &str = "2026-07-26T12:30:00Z";
const TEST_DOMAIN_KEY: [u8; 32] = [0x43; 32];

fn write_bytes(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn write_jsonl(root: &Path, relative: &str, records: &[serde_json::Value]) {
    let mut bytes = Vec::new();
    for record in records {
        serde_json::to_writer(&mut bytes, record).unwrap();
        bytes.push(b'\n');
    }
    write_bytes(root, relative, &bytes);
}

fn extend_with_ignored_jsonl_until(path: &Path, target_size: u64) {
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    let chunk = b"{}\n".repeat(21_845);
    let mut length = file.metadata().unwrap().len();
    while length < target_size {
        let remaining = usize::try_from((target_size - length).min(chunk.len() as u64)).unwrap();
        file.write_all(&chunk[..remaining]).unwrap();
        length += remaining as u64;
    }
    file.flush().unwrap();
}

fn snapshot_tree(root: &Path) -> Vec<(String, u64, Option<SystemTime>, Vec<u8>)> {
    fn visit(
        base: &Path,
        current: &Path,
        output: &mut Vec<(String, u64, Option<SystemTime>, Vec<u8>)>,
    ) {
        let mut entries = fs::read_dir(current)
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            let relative = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if metadata.is_dir() {
                output.push((
                    relative,
                    metadata.len(),
                    metadata.modified().ok(),
                    Vec::new(),
                ));
                visit(base, &path, output);
            } else {
                output.push((
                    relative,
                    metadata.len(),
                    metadata.modified().ok(),
                    fs::read(path).unwrap(),
                ));
            }
        }
    }
    let mut output = Vec::new();
    visit(root, root, &mut output);
    output
}

fn scanner(root: &TempDir, state: &TempDir) -> ClaudeScanner {
    ClaudeScanner::open(
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap()
}

#[test]
fn slice_scanner_reuses_one_cycle_and_preserves_missing_source_transitions() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "projects/project-a/session.jsonl";
    write_bytes(
        root.path(),
        relative,
        include_bytes!("fixtures/claude/snapshots.jsonl"),
    );
    let mut scanner = scanner(&root, &state);
    let mut sources = Vec::new();

    for _ in 0..16 {
        let summary = scanner
            .scan_slice(
                NOW,
                SessionScanControl::default(),
                SessionDiscoverySliceBudget::default(),
            )
            .unwrap();
        sources.extend(summary.sources);
        if summary.outcome == SessionScanOutcome::Complete {
            break;
        }
    }

    assert_eq!(sources.len(), 1);
    let source_key = sources[0].source_key.clone();
    fs::remove_file(root.path().join(relative)).unwrap();
    let mut deleted = 0;
    for _ in 0..16 {
        let summary = scanner
            .scan_slice(
                "2026-07-26T12:31:00Z",
                SessionScanControl::default(),
                SessionDiscoverySliceBudget::default(),
            )
            .unwrap();
        deleted += summary.deleted_sources;
        if summary.outcome == SessionScanOutcome::Complete {
            break;
        }
    }

    assert_eq!(deleted, 1);
    assert_eq!(
        scanner
            .state()
            .load_session_source(&source_key)
            .unwrap()
            .unwrap()
            .status,
        SessionSourceStatus::Stale
    );
}

#[test]
fn rename_preserves_source_generation_and_usage_identity() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let original = "projects/project-a/original.jsonl";
    let renamed = "projects/project-a/renamed.jsonl";
    write_bytes(
        root.path(),
        original,
        include_bytes!("fixtures/claude/snapshots.jsonl"),
    );
    let mut scanner = scanner(&root, &state);
    let first = scanner.scan(NOW, SessionScanControl::default()).unwrap();
    let source_key = first.sources[0].source_key.clone();
    let cursor = scanner
        .state()
        .load_current_session_scan_cursor(&source_key)
        .unwrap()
        .unwrap();
    let usage_ids = scanner
        .state()
        .load_current_session_usage_page(&source_key, None, 500)
        .unwrap()
        .items
        .into_iter()
        .map(|usage| usage.usage_id)
        .collect::<Vec<_>>();

    fs::rename(root.path().join(original), root.path().join(renamed)).unwrap();
    let moved = scanner
        .scan("2026-07-26T12:31:00Z", SessionScanControl::default())
        .unwrap();

    assert_eq!(moved.sources.len(), 1);
    assert_eq!(moved.sources[0].source_key, source_key);
    assert_eq!(moved.deleted_sources, 0);
    assert_eq!(
        scanner
            .state()
            .load_current_session_scan_cursor(&source_key)
            .unwrap()
            .unwrap()
            .generation,
        cursor.generation
    );
    assert_eq!(
        scanner
            .state()
            .load_current_session_usage_page(&source_key, None, 500)
            .unwrap()
            .items
            .into_iter()
            .map(|usage| usage.usage_id)
            .collect::<Vec<_>>(),
        usage_ids
    );
}

#[test]
fn same_path_identity_replacement_atomically_supersedes_the_old_generation() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "projects/project-a/replaced.jsonl";
    write_jsonl(
        root.path(),
        relative,
        &[
            serde_json::json!({
                "type":"user","uuid":"old-user","sessionId":"old-session",
                "timestamp":"2026-07-26T12:00:00Z",
                "message":{"role":"user","content":"old"}
            }),
            serde_json::json!({
                "type":"assistant","uuid":"old-answer","sessionId":"old-session",
                "timestamp":"2026-07-26T12:00:01Z",
                "message":{"id":"old-answer","role":"assistant","model":"claude-test",
                    "content":"old","usage":{"input_tokens":100,"output_tokens":100}}
            }),
        ],
    );
    let mut scanner = scanner(&root, &state);
    let first = scanner.scan(NOW, SessionScanControl::default()).unwrap();
    let source_key = first.sources[0].source_key.clone();
    let generation = scanner
        .state()
        .load_current_generation(&source_key)
        .unwrap()
        .unwrap();

    let replacement = root.path().join("replacement.tmp");
    write_jsonl(
        root.path(),
        "replacement.tmp",
        &[
            serde_json::json!({
                "type":"user","uuid":"new-user","sessionId":"new-session",
                "timestamp":"2026-07-26T12:01:00Z",
                "message":{"role":"user","content":"new"}
            }),
            serde_json::json!({
                "type":"assistant","uuid":"new-answer","sessionId":"new-session",
                "timestamp":"2026-07-26T12:01:01Z",
                "message":{"id":"new-answer","role":"assistant","model":"claude-test",
                    "content":"new","usage":{"input_tokens":2,"output_tokens":3}}
            }),
        ],
    );
    fs::remove_file(root.path().join(relative)).unwrap();
    fs::rename(replacement, root.path().join(relative)).unwrap();

    let second = scanner
        .scan("2026-07-26T12:31:00Z", SessionScanControl::default())
        .unwrap();
    assert_eq!(second.sources.len(), 1);
    assert_eq!(second.sources[0].source_key, source_key);
    assert_eq!(second.deleted_sources, 0);
    assert_eq!(
        scanner
            .state()
            .load_current_generation(&source_key)
            .unwrap(),
        Some(generation + 1)
    );
    let usage = scanner
        .state()
        .load_current_session_usage_page(&source_key, None, 10)
        .unwrap()
        .items;
    assert_eq!(usage.len(), 1);
    assert_eq!((usage[0].input_tokens, usage[0].output_tokens), (2, 3));
    let global = scanner
        .state()
        .load_global_current_session_usage_page(None, 500)
        .unwrap()
        .items;
    assert_eq!(global.len(), 1);
    assert_eq!((global[0].input_tokens, global[0].output_tokens), (2, 3));
}

#[test]
fn oversized_tokens_and_changed_source_work_are_isolated_from_siblings() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_jsonl(
        root.path(),
        "projects/project-a/malicious.jsonl",
        &[
            serde_json::json!({
                "type":"user","uuid":"bad-user","sessionId":"bad-session",
                "timestamp":"2026-07-26T12:00:00Z",
                "message":{"role":"user","content":"bad"}
            }),
            serde_json::json!({
                "type":"assistant","uuid":"bad-answer","sessionId":"bad-session",
                "timestamp":"2026-07-26T12:00:01Z",
                "message":{"id":"bad-answer","role":"assistant","model":"claude-test",
                    "content":"bad",
                    "usage":{"input_tokens":9_223_372_036_854_775_808u64,"output_tokens":1}}
            }),
        ],
    );
    write_jsonl(
        root.path(),
        "projects/project-a/sibling.jsonl",
        &[
            serde_json::json!({
                "type":"user","uuid":"sibling-user","sessionId":"sibling-session",
                "timestamp":"2026-07-26T12:00:00Z",
                "message":{"role":"user","content":"sibling"}
            }),
            serde_json::json!({
                "type":"assistant","uuid":"sibling-answer","sessionId":"sibling-session",
                "timestamp":"2026-07-26T12:00:01Z",
                "message":{"id":"sibling-answer","role":"assistant","model":"claude-test",
                    "content":"sibling","usage":{"input_tokens":1,"output_tokens":1}}
            }),
        ],
    );
    let mut malicious_scanner = scanner(&root, &state);
    let first = malicious_scanner
        .scan(NOW, SessionScanControl::default())
        .unwrap();
    assert_eq!(
        first
            .sources
            .iter()
            .filter(|source| source.status == SessionSourceStatus::Available)
            .count(),
        1
    );
    assert_eq!(
        first
            .sources
            .iter()
            .filter(|source| source.error_code
                == Some(wokcore_storage::SessionSourceErrorCode::SourceRecordTooLarge))
            .count(),
        1
    );

    let bounded_root = TempDir::new().unwrap();
    let bounded_state = TempDir::new().unwrap();
    for name in ["heavy", "sibling"] {
        write_jsonl(
            bounded_root.path(),
            &format!("projects/project-a/{name}.jsonl"),
            &[
                serde_json::json!({
                    "type":"user","uuid":format!("{name}-user"),"sessionId":format!("{name}-session"),
                    "timestamp":"2026-07-26T12:00:00Z",
                    "message":{"role":"user","content":"prompt"}
                }),
                serde_json::json!({
                    "type":"assistant","uuid":format!("{name}-answer"),"sessionId":format!("{name}-session"),
                    "timestamp":"2026-07-26T12:00:01Z",
                    "message":{"id":format!("{name}-answer"),"role":"assistant","model":"claude-test",
                        "content":"answer","usage":{"input_tokens":1,"output_tokens":1}}
                }),
            ],
        );
    }
    let mut bounded_scanner = scanner(&bounded_root, &bounded_state);
    bounded_scanner
        .scan(NOW, SessionScanControl::default())
        .unwrap();
    extend_with_ignored_jsonl_until(
        &bounded_root.path().join("projects/project-a/heavy.jsonl"),
        64 * 1024 * 1024 + 1,
    );
    let changed = bounded_scanner
        .scan("2026-07-26T12:31:00Z", SessionScanControl::default())
        .unwrap();
    assert_eq!(
        changed
            .sources
            .iter()
            .filter(|source| source.status == SessionSourceStatus::Available)
            .count(),
        1
    );
    assert_eq!(
        changed
            .sources
            .iter()
            .filter(|source| source.error_code
                == Some(wokcore_storage::SessionSourceErrorCode::SourceRecordTooLarge))
            .count(),
        1
    );
    assert!(changed.metrics.parser_read_bytes <= 64 * 1024 * 1024);
}

#[test]
fn explicit_title_priority_uses_last_valid_value_and_stale_source_is_not_read() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "projects/project-a/title.jsonl";
    let oversized = "x".repeat(513);
    write_jsonl(
        root.path(),
        relative,
        &[
            serde_json::json!({
                "type":"user","uuid":"user-1","sessionId":"claude-title",
                "timestamp":"2026-07-26T12:00:00Z",
                "message":{"role":"user","content":oversized}
            }),
            serde_json::json!({"type":"metadata","summary":"summary-old"}),
            serde_json::json!({"type":"metadata","summary":"summary-new"}),
            serde_json::json!({"type":"metadata","lastPrompt":"last prompt"}),
            serde_json::json!({"type":"metadata","aiTitle":"AI title"}),
            serde_json::json!({"type":"custom-title","customTitle":"custom-old"}),
            serde_json::json!({"type":"custom-title","customTitle":"custom-new"}),
            serde_json::json!({"type":"custom-title","customTitle":"y".repeat(513)}),
        ],
    );
    let mut scanner = scanner(&root, &state);
    let source_key = scanner
        .scan(NOW, SessionScanControl::default())
        .unwrap()
        .sources[0]
        .source_key
        .clone();
    assert_eq!(
        scanner
            .title_for_source(&source_key)
            .unwrap()
            .unwrap()
            .as_str(),
        "custom-new"
    );

    let mut writer = fs::OpenOptions::new()
        .append(true)
        .open(root.path().join(relative))
        .unwrap();
    writeln!(
        writer,
        "{}",
        serde_json::json!({"type":"custom-title","customTitle":"unscanned title"})
    )
    .unwrap();
    writer.flush().unwrap();
    assert!(scanner.title_for_source(&source_key).unwrap().is_none());
}

#[test]
fn completed_cursor_covers_trailing_non_usage_records() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_jsonl(
        root.path(),
        "projects/project-a/trailing.jsonl",
        &[
            serde_json::json!({
                "type":"user",
                "uuid":"user-1",
                "sessionId":"claude-trailing",
                "timestamp":"2026-07-26T12:00:00Z",
                "message":{"role":"user","content":"question"}
            }),
            serde_json::json!({
                "type":"assistant",
                "uuid":"assistant-1",
                "sessionId":"claude-trailing",
                "timestamp":"2026-07-26T12:00:01Z",
                "message":{"id":"message-1","role":"assistant","model":"claude-test",
                    "content":"answer","usage":{"input_tokens":1,"output_tokens":1}}
            }),
            serde_json::json!({
                "type":"progress",
                "sessionId":"claude-trailing",
                "timestamp":"2026-07-26T12:00:02Z"
            }),
        ],
    );

    let mut deep_scanner = scanner(&root, &state);
    let summary = deep_scanner
        .scan(NOW, SessionScanControl::default())
        .unwrap();
    let cursor = deep_scanner
        .state()
        .load_current_session_scan_cursor(&summary.sources[0].source_key)
        .unwrap()
        .unwrap();

    assert_eq!(cursor.stable_record_ordinal, 3);
    assert_eq!(
        cursor.complete_byte_offset,
        fs::metadata(root.path().join("projects/project-a/trailing.jsonl"))
            .unwrap()
            .len()
    );
}

#[test]
fn discovers_main_subagent_and_nested_workflow_jsonl_with_fixed_budgets() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_bytes(
        root.path(),
        "projects/project-a/main.jsonl",
        include_bytes!("fixtures/claude/snapshots.jsonl"),
    );
    for (relative, session_id) in [
        (
            "projects/project-a/session-a/subagents/agent-a.jsonl",
            "claude-subagent",
        ),
        (
            "projects/project-a/session-a/subagents/workflows/run-7/agent-b.jsonl",
            "claude-workflow",
        ),
        (
            "projects/project-a/noninteractive.jsonl",
            "claude-noninteractive",
        ),
    ] {
        write_jsonl(
            root.path(),
            relative,
            &[
                serde_json::json!({
                    "type":"user",
                    "uuid":format!("{session_id}-user"),
                    "sessionId":session_id,
                    "timestamp":"2026-07-26T12:00:00Z",
                    "message":{"role":"user","content":"synthetic"}
                }),
                serde_json::json!({
                    "type":"assistant",
                    "uuid":format!("{session_id}-assistant"),
                    "sessionId":session_id,
                    "timestamp":"2026-07-26T12:00:01Z",
                    "message":{"id":format!("{session_id}-message"),"role":"assistant",
                        "model":"claude-test","content":"synthetic",
                        "usage":{"input_tokens":1,"output_tokens":1}}
                }),
            ],
        );
    }
    write_bytes(
        root.path(),
        "projects/project-a/session-a/subagents/not-a-session.txt",
        b"must be ignored",
    );

    let before = snapshot_tree(root.path());
    let mut recursive_scanner = scanner(&root, &state);
    let summary = recursive_scanner
        .scan(NOW, SessionScanControl::default())
        .unwrap();

    assert_eq!(summary.outcome, SessionScanOutcome::Complete);
    assert_eq!(summary.advanced_sources, 4);
    assert_eq!(summary.sources.len(), 4);
    assert!(
        summary
            .sources
            .iter()
            .all(|source| source.status == SessionSourceStatus::Available)
    );
    assert_eq!(snapshot_tree(root.path()), before);
}

#[test]
fn subagents_are_discovered_recursively_with_an_explicit_depth_limit() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let deep = "projects/project-a/session-a/subagents/one/two/three/four/five/agent-deep.jsonl";
    write_jsonl(
        root.path(),
        deep,
        &[
            serde_json::json!({
                "type":"user",
                "uuid":"deep-user",
                "sessionId":"claude-deep",
                "timestamp":"2026-07-26T12:00:00Z",
                "message":{"role":"user","content":"deep prompt"}
            }),
            serde_json::json!({
                "type":"assistant",
                "uuid":"deep-assistant",
                "sessionId":"claude-deep",
                "timestamp":"2026-07-26T12:00:01Z",
                "message":{"id":"deep-assistant","role":"assistant","model":"claude-test",
                    "content":"deep answer","usage":{"input_tokens":1,"output_tokens":1}}
            }),
        ],
    );
    let mut recursive_scanner = scanner(&root, &state);
    let summary = recursive_scanner
        .scan(NOW, SessionScanControl::default())
        .unwrap();
    assert_eq!(summary.sources.len(), 1);
    assert_eq!(summary.sources[0].status, SessionSourceStatus::Available);

    let too_deep_root = TempDir::new().unwrap();
    let too_deep_state = TempDir::new().unwrap();
    let mut relative = String::from("projects/project-a/session-a/subagents");
    for depth in 0..40 {
        relative.push_str(&format!("/depth-{depth}"));
    }
    relative.push_str("/agent-too-deep.jsonl");
    write_jsonl(
        too_deep_root.path(),
        &relative,
        &[serde_json::json!({
            "type":"user",
            "uuid":"too-deep",
            "sessionId":"claude-too-deep",
            "timestamp":"2026-07-26T12:00:00Z",
            "message":{"role":"user","content":"must not be reached"}
        })],
    );
    let mut too_deep_scanner = scanner(&too_deep_root, &too_deep_state);
    assert!(matches!(
        too_deep_scanner.scan(NOW, SessionScanControl::default()),
        Err(wokcore_sessions::claude::ClaudeScannerError::ResourceLimit)
    ));
}

#[test]
fn invisible_claude_records_do_not_count_or_page() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_jsonl(
        root.path(),
        "projects/project-a/visibility.jsonl",
        &[
            serde_json::json!({
                "type":"user",
                "uuid":"visible-user",
                "sessionId":"claude-visibility",
                "timestamp":"2026-07-26T12:00:00Z",
                "message":{"role":"user","content":"visible prompt"}
            }),
            serde_json::json!({
                "type":"assistant",
                "uuid":"meta",
                "sessionId":"claude-visibility",
                "timestamp":"2026-07-26T12:00:01Z",
                "isMeta":true,
                "message":{"id":"meta","role":"assistant","model":"claude-test",
                    "content":"META-MUST-NOT-ESCAPE",
                    "usage":{"input_tokens":100,"output_tokens":100}}
            }),
            serde_json::json!({
                "type":"assistant",
                "uuid":"sidechain",
                "sessionId":"claude-visibility",
                "timestamp":"2026-07-26T12:00:02Z",
                "isSidechain":true,
                "message":{"id":"sidechain","role":"assistant","model":"claude-test",
                    "content":"SIDECHAIN-MUST-NOT-ESCAPE",
                    "usage":{"input_tokens":200,"output_tokens":200}}
            }),
            serde_json::json!({
                "type":"assistant",
                "uuid":"team",
                "sessionId":"claude-visibility",
                "timestamp":"2026-07-26T12:00:03Z",
                "teamName":"private-team",
                "message":{"id":"team","role":"assistant","model":"claude-test",
                    "content":"TEAM-MUST-NOT-ESCAPE",
                    "usage":{"input_tokens":300,"output_tokens":300}}
            }),
            serde_json::json!({
                "type":"assistant",
                "uuid":"visible-answer",
                "sessionId":"claude-visibility",
                "timestamp":"2026-07-26T12:00:04Z",
                "message":{"id":"visible-answer","role":"assistant","model":"claude-test",
                    "content":"visible answer","usage":{"input_tokens":2,"output_tokens":3}}
            }),
        ],
    );
    let mut scanner = scanner(&root, &state);
    let summary = scanner.scan(NOW, SessionScanControl::default()).unwrap();
    let source_key = summary.sources[0].source_key.clone();
    let index = scanner
        .state()
        .load_current_session_index_page(&source_key, None, 1)
        .unwrap()
        .items
        .pop()
        .unwrap();
    assert_eq!(index.message_count, 2);
    assert_eq!(index.usage_event_count, 1);
    let usage = scanner
        .state()
        .load_current_session_usage_page(&source_key, None, 10)
        .unwrap()
        .items;
    assert_eq!(usage.len(), 1);
    assert_eq!((usage[0].input_tokens, usage[0].output_tokens), (2, 3));

    let mut pager = MessagePager::open(
        SessionSourceKind::Claude,
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let page = pager
        .page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 16,
                maximum_utf8_bytes: MAX_MESSAGE_PAGE_UTF8_BYTES,
                cursor: None,
            },
        )
        .unwrap();
    assert_eq!(page.messages.len(), 2);
    assert_eq!(page.messages[0].role, MessageRole::User);
    let debug = format!("{:?}", page.messages);
    for hidden in [
        "META-MUST-NOT-ESCAPE",
        "SIDECHAIN-MUST-NOT-ESCAPE",
        "TEAM-MUST-NOT-ESCAPE",
    ] {
        assert!(!debug.contains(hidden));
    }
}

#[test]
fn last_valid_message_snapshot_replaces_usage_without_clamping_cache_dimensions() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let title = "PRIVATE TITLE MUST NOT PERSIST";
    let tool_payload = "TOOL-INPUT-MUST-NOT-ESCAPE";
    let raw_session_id = "claude-main";
    write_bytes(
        root.path(),
        "projects/project-a/main.jsonl",
        include_bytes!("fixtures/claude/snapshots.jsonl"),
    );

    let mut initial_scanner = scanner(&root, &state);
    let first = initial_scanner
        .scan(NOW, SessionScanControl::default())
        .unwrap();
    let source = first.sources.first().unwrap();
    assert!(!source.source_key.contains(raw_session_id));
    assert!(
        !source
            .session_key
            .as_deref()
            .unwrap()
            .contains(raw_session_id)
    );
    let index = initial_scanner
        .state()
        .load_current_session_index_page(&source.source_key, None, 1)
        .unwrap()
        .items
        .pop()
        .unwrap();
    assert_eq!(index.message_count, 3);
    assert_eq!(index.usage_event_count, 2);
    let usage = initial_scanner
        .state()
        .load_current_session_usage_page(&source.source_key, None, 10)
        .unwrap()
        .items;
    assert_eq!(usage.len(), 2);
    assert_eq!(
        (
            usage[0].input_tokens,
            usage[0].output_tokens,
            usage[0].cache_read_tokens,
            usage[0].cache_write_tokens,
        ),
        (2, 9, 48_719, 2_061)
    );
    assert_eq!(
        (
            usage[1].input_tokens,
            usage[1].output_tokens,
            usage[1].cache_read_tokens,
            usage[1].cache_write_tokens,
        ),
        (0, 0, 7, 11)
    );
    assert_eq!(usage[0].model, "claude-sonnet-4-5");
    assert_ne!(usage[0].usage_id, usage[1].usage_id);

    let database = snapshot_tree(state.path());
    for (_, _, _, bytes) in database {
        for secret in [title, tool_payload, raw_session_id, "最终回答"] {
            assert!(
                !bytes
                    .windows(secret.len())
                    .any(|window| window == secret.as_bytes()),
                "{secret} leaked into state"
            );
        }
    }

    drop(initial_scanner);
    let mut restarted = scanner(&root, &state);
    let before_restart = snapshot_tree(state.path());
    let unchanged = restarted
        .scan("2026-07-26T12:31:00Z", SessionScanControl::default())
        .unwrap();
    assert_eq!(unchanged.unchanged_sources, 1);
    assert_eq!(unchanged.metrics.full_source_scans, 0);
    assert_eq!(snapshot_tree(state.path()), before_restart);
}

#[test]
fn malformed_replacement_preserves_last_promoted_generation() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "projects/project-a/main.jsonl";
    write_bytes(
        root.path(),
        relative,
        include_bytes!("fixtures/claude/snapshots.jsonl"),
    );
    let mut scanner = scanner(&root, &state);
    let first = scanner.scan(NOW, SessionScanControl::default()).unwrap();
    let source_key = first.sources[0].source_key.clone();
    let generation = scanner
        .state()
        .load_current_generation(&source_key)
        .unwrap()
        .unwrap();

    fs::write(root.path().join(relative), b"{\"type\":\"assistant\"}\n{").unwrap();
    let failed = scanner
        .scan("2026-07-26T12:32:00Z", SessionScanControl::default())
        .unwrap();
    assert_eq!(failed.sources[0].status, SessionSourceStatus::Stale);
    assert_eq!(
        scanner
            .state()
            .load_current_generation(&source_key)
            .unwrap(),
        Some(generation)
    );
    assert_eq!(
        scanner
            .state()
            .load_current_session_usage_page(&source_key, None, 10)
            .unwrap()
            .items
            .len(),
        2
    );
}

#[test]
fn partial_later_snapshot_does_not_erase_the_last_valid_usage() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let mut bytes = include_bytes!("fixtures/claude/snapshots.jsonl").to_vec();
    bytes.extend_from_slice(
        br#"{"type":"assistant","sessionId":"claude-main","timestamp":"2026-07-26T12:00:05Z","message":{"id":"assistant-1","role":"assistant","model":"claude-sonnet-4-5","content":"partial","usage":{"input_tokens":"bad"}}}
"#,
    );
    write_bytes(root.path(), "projects/project-a/main.jsonl", &bytes);

    let mut scanner = scanner(&root, &state);
    let summary = scanner.scan(NOW, SessionScanControl::default()).unwrap();
    let usage = scanner
        .state()
        .load_current_session_usage_page(&summary.sources[0].source_key, None, 10)
        .unwrap()
        .items;
    assert_eq!(usage.len(), 2);
    assert_eq!(usage[0].output_tokens, 9);
    assert_eq!(usage[0].cache_read_tokens, 48_719);
}

#[test]
fn unique_append_keeps_generation_while_middle_rewrite_plus_append_rebuilds() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "projects/project-a/main.jsonl";
    write_bytes(
        root.path(),
        relative,
        include_bytes!("fixtures/claude/snapshots.jsonl"),
    );
    let mut scanner = scanner(&root, &state);
    let initial = scanner.scan(NOW, SessionScanControl::default()).unwrap();
    let source_key = initial.sources[0].source_key.clone();
    let initial_generation = scanner
        .state()
        .load_current_generation(&source_key)
        .unwrap()
        .unwrap();

    let mut appended = include_bytes!("fixtures/claude/snapshots.jsonl").to_vec();
    appended.extend_from_slice(
        br#"{"type":"user","uuid":"new-user","sessionId":"claude-main","timestamp":"2026-07-26T12:01:00Z","message":{"role":"user","content":"new"}}
"#,
    );
    fs::write(root.path().join(relative), &appended).unwrap();
    let append = scanner
        .scan("2026-07-26T12:31:00Z", SessionScanControl::default())
        .unwrap();
    assert_eq!(
        scanner
            .state()
            .load_current_generation(&source_key)
            .unwrap(),
        Some(initial_generation)
    );
    assert!(
        append.metrics.parser_read_bytes <= appended.len() as u64 + 64 * 1024,
        "active append has an explicit measured source-read ceiling"
    );

    let rewritten = String::from_utf8(appended).unwrap().replacen(
        "\"output_tokens\":9",
        "\"output_tokens\":8",
        1,
    );
    let mut rewritten = rewritten.into_bytes();
    rewritten.extend_from_slice(
        br#"{"type":"user","uuid":"another-user","sessionId":"claude-main","timestamp":"2026-07-26T12:02:00Z","message":{"role":"user","content":"another"}}
"#,
    );
    fs::write(root.path().join(relative), rewritten).unwrap();
    scanner
        .scan("2026-07-26T12:32:00Z", SessionScanControl::default())
        .unwrap();
    assert_eq!(
        scanner
            .state()
            .load_current_generation(&source_key)
            .unwrap(),
        Some(initial_generation + 1)
    );
    let usage = scanner
        .state()
        .load_current_session_usage_page(&source_key, None, 10)
        .unwrap()
        .items;
    assert_eq!(usage[0].output_tokens, 8);
}

#[test]
fn interrupted_multibatch_candidate_is_invisible_and_resumes_after_restart() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let mut lines = String::new();
    for index in 0..500 {
        lines.push_str(&format!(
            "{{\"type\":\"assistant\",\"sessionId\":\"claude-batched\",\
             \"timestamp\":\"2026-07-26T12:00:01Z\",\"message\":{{\
             \"id\":\"answer-{index}\",\"role\":\"assistant\",\"model\":\"claude-test\",\
             \"content\":\"answer\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}}}\n"
        ));
    }
    lines.push_str(
        "{\"type\":\"assistant\",\"sessionId\":\"claude-batched\",\
         \"timestamp\":\"2026-07-26T12:00:02Z\",\"message\":{\
         \"id\":\"answer-0\",\"role\":\"assistant\",\"model\":\"claude-test\",\
         \"content\":\"replacement\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n",
    );
    write_bytes(
        root.path(),
        "projects/project-a/batched.jsonl",
        lines.as_bytes(),
    );
    let mut initial = scanner(&root, &state);
    let interrupted = initial
        .scan(
            NOW,
            SessionScanControl {
                stop_after_committed_batches: Some(1),
            },
        )
        .unwrap();
    assert_eq!(interrupted.outcome, SessionScanOutcome::Interrupted);
    assert_eq!(interrupted.metrics.aggregate_message_inspections, 500);
    let source_key = interrupted.sources[0].source_key.clone();
    assert_eq!(
        initial
            .state()
            .load_current_generation(&source_key)
            .unwrap(),
        None
    );
    assert!(
        initial
            .state()
            .load_current_session_usage_page(&source_key, None, 10)
            .unwrap()
            .items
            .is_empty()
    );
    drop(initial);

    let mut restarted = scanner(&root, &state);
    let resumed = restarted
        .scan("2026-07-26T12:31:00Z", SessionScanControl::default())
        .unwrap();
    assert_eq!(resumed.outcome, SessionScanOutcome::Complete);
    let usage = restarted
        .state()
        .load_current_session_usage_page(&source_key, None, 500)
        .unwrap()
        .items;
    assert_eq!(usage.len(), 500);
    assert_eq!(
        usage
            .iter()
            .filter(|record| record.output_tokens == 2)
            .count(),
        1,
        "late replacement revision must resume without duplicate or omission"
    );
}

#[test]
fn cleanup_pending_keeps_new_generation_visible_and_does_not_block_siblings() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let heavy_relative = "projects/project-a/heavy.jsonl";
    let mut heavy = String::new();
    for index in 0..600 {
        heavy.push_str(&format!(
            "{{\"type\":\"assistant\",\"sessionId\":\"claude-heavy\",\
             \"timestamp\":\"2026-07-26T12:00:01Z\",\"message\":{{\
             \"id\":\"answer-{index}\",\"role\":\"assistant\",\"model\":\"claude-test\",\
             \"content\":\"answer\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}}}\n"
        ));
    }
    write_bytes(root.path(), heavy_relative, heavy.as_bytes());
    write_bytes(
        root.path(),
        "projects/project-b/sibling.jsonl",
        br#"{"type":"assistant","sessionId":"claude-sibling","timestamp":"2026-07-26T12:00:01Z","message":{"id":"sibling","role":"assistant","model":"claude-test","content":"answer","usage":{"input_tokens":1,"output_tokens":1}}}
"#,
    );
    let mut scanner = scanner(&root, &state);
    let initial = scanner.scan(NOW, SessionScanControl::default()).unwrap();
    let heavy_source = initial
        .sources
        .iter()
        .find(|source| {
            scanner
                .state()
                .load_current_session_index_page(&source.source_key, None, 1)
                .unwrap()
                .items[0]
                .usage_event_count
                == 600
        })
        .unwrap()
        .source_key
        .clone();
    let generation = scanner
        .state()
        .load_current_generation(&heavy_source)
        .unwrap()
        .unwrap();

    let mut rewritten = heavy.replacen("\"output_tokens\":1", "\"output_tokens\":2", 1);
    rewritten.push_str(
        "{\"type\":\"user\",\"uuid\":\"rewrite-marker\",\"sessionId\":\"claude-heavy\",\
         \"timestamp\":\"2026-07-26T12:00:02Z\",\"message\":{\"role\":\"user\",\
         \"content\":\"forces a changed extent\"}}\n",
    );
    fs::write(root.path().join(heavy_relative), rewritten).unwrap();
    let pending = scanner
        .scan("2026-07-26T12:31:00Z", SessionScanControl::default())
        .unwrap();
    assert_eq!(pending.outcome, SessionScanOutcome::Interrupted);
    assert_eq!(pending.sources.len(), 2);
    assert_eq!(pending.unchanged_sources, 1);
    let source_state = scanner
        .state()
        .load_session_source(&heavy_source)
        .unwrap()
        .unwrap();
    assert_eq!(source_state.current_generation, Some(generation + 1));
    assert_eq!(source_state.retired_generation, Some(generation));
    assert_eq!(
        scanner
            .state()
            .load_current_session_index_page(&heavy_source, None, 1)
            .unwrap()
            .items[0]
            .usage_event_count,
        600,
        "the promoted generation remains atomically visible while old rows are cleaned"
    );

    let recovered = scanner
        .scan("2026-07-26T12:32:00Z", SessionScanControl::default())
        .unwrap();
    assert_eq!(recovered.outcome, SessionScanOutcome::Complete);
    assert_eq!(recovered.advanced_sources + recovered.unchanged_sources, 2);
    assert_eq!(
        scanner
            .state()
            .load_session_source(&heavy_source)
            .unwrap()
            .unwrap()
            .retired_generation,
        None
    );
    let idempotent = scanner
        .scan("2026-07-26T12:33:00Z", SessionScanControl::default())
        .unwrap();
    assert_eq!(idempotent.outcome, SessionScanOutcome::Complete);
    assert_eq!(idempotent.unchanged_sources, 2);
}

#[test]
fn logical_working_set_limit_is_source_local_and_keeps_sibling_scanning() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let mut over_budget = String::new();
    for index in 0..1_200 {
        let id = format!("{index:04}-{}", "x".repeat(500));
        over_budget.push_str(&format!(
            "{{\"type\":\"assistant\",\"sessionId\":\"claude-over-budget\",\
             \"timestamp\":\"2026-07-26T12:00:01Z\",\"message\":{{\"id\":\"{id}\",\
             \"role\":\"assistant\",\"model\":\"claude-test\",\"content\":\"ignored\",\
             \"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}}}\n"
        ));
    }
    write_bytes(
        root.path(),
        "projects/project-a/over-budget.jsonl",
        over_budget.as_bytes(),
    );
    write_bytes(
        root.path(),
        "projects/project-b/valid.jsonl",
        include_bytes!("fixtures/claude/snapshots.jsonl"),
    );
    let mut scanner = scanner(&root, &state);
    let summary = scanner.scan(NOW, SessionScanControl::default()).unwrap();
    assert_eq!(
        summary
            .sources
            .iter()
            .filter(|source| source.status == SessionSourceStatus::ResourceLimited)
            .count(),
        1
    );
    assert_eq!(
        summary
            .sources
            .iter()
            .filter(|source| source.status == SessionSourceStatus::Available)
            .count(),
        1
    );
    assert!(summary.metrics.peak_parser_buffer_bytes <= MAX_CLAUDE_LOGICAL_WORKING_BYTES);
}
