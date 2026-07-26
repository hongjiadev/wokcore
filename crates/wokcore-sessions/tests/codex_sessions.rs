use std::{
    fs::{self, FileTimes, OpenOptions},
    io::Write,
    path::Path,
    time::SystemTime,
};

use tempfile::TempDir;
use wokcore_sessions::{
    codex::{
        CodexScanner, CodexStructuralRecord, ScanControl, ScanOutcome, normalize_model,
        parse_codex_record, parse_timestamp_utc,
    },
    model::{ReplayResolution, TokenTotals},
};
use wokcore_storage::{
    MAX_CODEX_REPLAY_SIGNATURES, MAX_SESSION_BATCH_ROWS, SessionScanResultCode,
    SessionSourceErrorCode, SessionSourceStatus, StateStore,
};

const TEST_DOMAIN_KEY: [u8; 32] = [0x5a; 32];
const NOW: &str = "2026-07-26T12:00:00Z";

fn write_session(root: &Path, relative: &str, lines: &[serde_json::Value]) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut bytes = Vec::new();
    for line in lines {
        serde_json::to_writer(&mut bytes, line).unwrap();
        bytes.push(b'\n');
    }
    fs::write(path, bytes).unwrap();
}

fn meta(id: &str, timestamp: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "timestamp": timestamp,
        "type": "session_meta",
        "payload": {"id": id}
    })
}

fn turn(model: &str) -> serde_json::Value {
    serde_json::json!({"type":"turn_context","payload":{"model":model}})
}

fn token(timestamp: &str, input: u64, output: u64, cached: u64) -> serde_json::Value {
    serde_json::json!({
        "timestamp": timestamp,
        "type":"event_msg",
        "payload":{"type":"token_count","info":{"total_token_usage":{
            "input_tokens":input,
            "output_tokens":output,
            "cached_input_tokens":cached
        }}}
    })
}

fn token_with_total_and_last(
    timestamp: &str,
    total_input: u64,
    total_output: u64,
    last_input: u64,
    last_output: u64,
) -> serde_json::Value {
    serde_json::json!({
        "timestamp": timestamp,
        "type":"event_msg",
        "payload":{"type":"token_count","info":{
            "total_token_usage":{"input_tokens":total_input,"output_tokens":total_output},
            "last_token_usage":{"input_tokens":last_input,"output_tokens":last_output}
        }}
    })
}

fn last_token(timestamp: &str, input: u64, output: u64) -> serde_json::Value {
    serde_json::json!({
        "timestamp": timestamp,
        "type":"event_msg",
        "payload":{"type":"token_count","info":{
            "last_token_usage":{"input_tokens":input,"output_tokens":output}
        }}
    })
}

fn scanner(root: &TempDir, state: &TempDir) -> CodexScanner {
    CodexScanner::open(
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap()
}

#[test]
fn synthetic_fixture_files_cover_basic_and_forked_rollouts() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    for (relative, bytes) in [
        (
            "sessions/2026/07/26/basic.jsonl",
            include_bytes!("fixtures/codex/basic.jsonl").as_slice(),
        ),
        (
            "sessions/2026/07/26/fork-parent.jsonl",
            include_bytes!("fixtures/codex/fork-parent.jsonl").as_slice(),
        ),
        (
            "sessions/2026/07/26/fork-child.jsonl",
            include_bytes!("fixtures/codex/fork-child.jsonl").as_slice(),
        ),
    ] {
        let path = root.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    let mut codex_scanner = scanner(&root, &state);
    let summary = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    let basic = summary
        .sources
        .iter()
        .find(|source| source.root_thread_id.as_deref() == Some("fixture-basic"))
        .unwrap();
    let basic_index = codex_scanner
        .state()
        .load_current_session_index_page(&basic.source_key, None, 1)
        .unwrap()
        .items
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(basic_index.message_count, 1);
    let child = summary
        .sources
        .iter()
        .find(|source| source.root_thread_id.as_deref() == Some("fixture-child"))
        .unwrap();
    assert_eq!(
        child.replay_resolution,
        ReplayResolution::Resolved { replayed_events: 2 }
    );
    let child_usage = codex_scanner
        .state()
        .load_current_session_usage_page(&child.source_key, None, 500)
        .unwrap();
    assert_eq!(child_usage.items.len(), 1);
    assert_eq!(child_usage.items[0].input_tokens, 5);
}

#[test]
fn response_item_index_counts_messages_only_and_tracks_timestamp_without_tokens() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_session(
        root.path(),
        "sessions/2026/07/26/no-token-messages.jsonl",
        &[
            meta(
                "no-token-messages",
                serde_json::json!("2026-07-26T12:00:00Z"),
            ),
            serde_json::json!({
                "timestamp":"2026-07-26T12:00:02+00:00",
                "type":"response_item",
                "payload":{"type":"message","role":"assistant","content":[]}
            }),
            serde_json::json!({
                "timestamp":"2026-07-26T17:30:03+05:30",
                "type":"response_item",
                "payload":{"type":"function_call","name":"synthetic","arguments":"{}"}
            }),
        ],
    );

    let mut codex_scanner = scanner(&root, &state);
    let summary = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    let index = codex_scanner
        .state()
        .load_current_session_index_page(&summary.sources[0].source_key, None, 1)
        .unwrap()
        .items
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(index.message_count, 1);
    assert_eq!(index.usage_event_count, 0);
    assert_eq!(index.last_active_at, "2026-07-26T12:00:03Z");
}

#[test]
fn structural_parser_is_language_independent_and_forward_compatible() {
    assert!(matches!(
        parse_codex_record(&meta("thread-a", serde_json::json!("2026-07-26T00:00:00Z"))).unwrap(),
        CodexStructuralRecord::SessionMeta(_)
    ));
    assert!(matches!(
        parse_codex_record(&turn("openai/gpt-5.6-codex")).unwrap(),
        CodexStructuralRecord::TurnContext(_)
    ));
    assert!(matches!(
        parse_codex_record(&token("2026-07-26T00:00:01Z", 10, 2, 20)).unwrap(),
        CodexStructuralRecord::TokenCount(_)
    ));
    assert!(matches!(
        parse_codex_record(&serde_json::json!({
            "type":"response_item","payload":{"type":"message","content":"任何语言"}
        }))
        .unwrap(),
        CodexStructuralRecord::ResponseItem(_)
    ));
    assert!(matches!(
        parse_codex_record(&serde_json::json!({"type":"future_record","payload":{"secret":"x"}}))
            .unwrap(),
        CodexStructuralRecord::Unknown
    ));
}

#[test]
fn current_and_legacy_parent_fields_must_be_consistent() {
    let record = serde_json::json!({
        "type":"session_meta",
        "timestamp":"2026-07-26T00:00:00Z",
        "payload":{
            "id":"child",
            "forked_from_id":"parent-a",
            "parent_thread_id":"parent-a",
            "source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-a"}}}
        }
    });
    let CodexStructuralRecord::SessionMeta(meta) = parse_codex_record(&record).unwrap() else {
        panic!("meta");
    };
    assert_eq!(meta.parent_thread_id.as_deref(), Some("parent-a"));

    let conflict = serde_json::json!({
        "type":"session_meta",
        "timestamp":"2026-07-26T00:00:00Z",
        "payload":{"id":"child","forked_from_id":"a","parent_thread_id":"b"}
    });
    assert_eq!(
        parse_codex_record(&conflict).unwrap_err().stable_code(),
        "codex_parent_inconsistent"
    );
}

#[test]
fn malformed_present_legacy_parent_fields_fail_closed_while_normal_sources_remain_compatible() {
    for malformed_source in [
        serde_json::json!({"subagent":{"thread_spawn":{"parent_thread_id":7}}}),
        serde_json::json!({"subagent":{"thread_spawn":{"parent_thread_id":""}}}),
        serde_json::json!({"subagent":{"thread_spawn":"not-an-object"}}),
        serde_json::json!({"subagent":"not-an-object"}),
    ] {
        let record = serde_json::json!({
            "type":"session_meta",
            "timestamp":"2026-07-26T00:00:00Z",
            "payload":{"id":"child","source":malformed_source}
        });
        assert_eq!(
            parse_codex_record(&record).unwrap_err().stable_code(),
            "codex_parent_inconsistent"
        );
    }

    for normal_source in [
        serde_json::json!("cli"),
        serde_json::json!({"type":"cli"}),
        serde_json::json!({"subagent":{"kind":"review"}}),
    ] {
        let record = serde_json::json!({
            "type":"session_meta",
            "timestamp":"2026-07-26T00:00:00Z",
            "payload":{"id":"normal","source":normal_source}
        });
        let CodexStructuralRecord::SessionMeta(meta) = parse_codex_record(&record).unwrap() else {
            panic!("meta");
        };
        assert_eq!(meta.parent_thread_id, None);
    }
}

#[test]
fn recognized_field_types_bounds_and_missing_token_timestamps_fail_closed() {
    let invalid = [
        serde_json::json!({
            "type":"session_meta","timestamp":"2026-07-26T00:00:00Z",
            "payload":{"id":7}
        }),
        serde_json::json!({
            "type":"session_meta","timestamp":"2026-07-26T00:00:00Z",
            "payload":{"id":"child","parent_thread_id":7}
        }),
        serde_json::json!({
            "type":"session_meta","timestamp":"2026-07-26T00:00:00Z",
            "payload":{"id":"x".repeat(513)}
        }),
        serde_json::json!({
            "type":"turn_context","payload":{"model":"x".repeat(257)}
        }),
        serde_json::json!({
            "type":"event_msg","timestamp":"2026-07-26T00:00:00Z",
            "payload":{"type":"token_count","info":{"total_token_usage":[]}}
        }),
        serde_json::json!({
            "type":"event_msg","timestamp":"2026-07-26T00:00:00Z",
            "payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":-1}}}
        }),
        serde_json::json!({
            "type":"event_msg","timestamp":"2026-07-26T00:00:00Z",
            "payload":{"type":"token_count","info":{"last_token_usage":{"output_tokens":1.5}}}
        }),
        serde_json::json!({
            "type":"event_msg",
            "payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1}}}
        }),
    ];
    for record in invalid {
        let error = parse_codex_record(&record).unwrap_err();
        assert!(matches!(
            error.stable_code(),
            "codex_record_invalid" | "codex_timestamp_invalid"
        ));
        assert!(!format!("{error:?}").contains("0001-01-01"));
    }
}

#[test]
fn timestamps_cover_all_offsets_and_integer_epoch_units() {
    let cases = [
        (
            serde_json::json!("2026-07-26T12:00:00Z"),
            "2026-07-26T12:00:00Z",
        ),
        (
            serde_json::json!("2026-07-26T20:00:00+08:00"),
            "2026-07-26T12:00:00Z",
        ),
        (
            serde_json::json!("2026-07-26T06:30:00-05:30"),
            "2026-07-26T12:00:00Z",
        ),
        (
            serde_json::json!("2026-07-26T17:45:00+05:45"),
            "2026-07-26T12:00:00Z",
        ),
        (serde_json::json!(1785067200_i64), "2026-07-26T12:00:00Z"),
        (serde_json::json!(1785067200000_i64), "2026-07-26T12:00:00Z"),
        (
            serde_json::json!(1785067200000000_i64),
            "2026-07-26T12:00:00Z",
        ),
        (
            serde_json::json!(1785067200000000000_i64),
            "2026-07-26T12:00:00Z",
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(parse_timestamp_utc(&input).unwrap(), expected);
    }
}

#[test]
fn cumulative_deltas_saturate_clamp_cache_and_ignore_duplicates() {
    let mut totals = TokenTotals::default();
    assert_eq!(
        totals.apply_cumulative(TokenTotals {
            input: 10,
            output: 5,
            cache_read: 50,
            cache_write: 2,
            reasoning: 1,
        }),
        Some(TokenTotals {
            input: 10,
            output: 5,
            cache_read: 10,
            cache_write: 2,
            reasoning: 1,
        })
    );
    assert_eq!(
        totals.apply_cumulative(TokenTotals {
            input: 10,
            output: 5,
            cache_read: 10,
            cache_write: 2,
            reasoning: 1,
        }),
        None
    );
    assert_eq!(
        totals.apply_cumulative(TokenTotals {
            input: 3,
            output: 9,
            cache_read: 2,
            cache_write: 1,
            reasoning: 0,
        }),
        Some(TokenTotals {
            input: 0,
            output: 4,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
        })
    );
}

#[test]
fn model_normalization_does_not_assume_provider() {
    assert_eq!(normalize_model(" openai/GPT-5.6-CODEX "), "gpt-5.6-codex");
    assert_eq!(normalize_model("custom::模型-A"), "custom::模型-a");
    assert_eq!(normalize_model(""), "unknown");
}

#[test]
fn scanner_reconstructs_usage_and_restart_is_write_free_when_unchanged() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_session(
        root.path(),
        "sessions/2026/07/26/usage.jsonl",
        &[
            meta("root-thread", serde_json::json!("2026-07-26T12:00:00Z")),
            turn("GPT-5.6-CODEX"),
            token("2026-07-26T12:00:01Z", 10, 2, 4),
            token("2026-07-26T12:00:02Z", 10, 2, 4),
            token("2026-07-26T12:00:03Z", 15, 7, 7),
            token_with_total_and_last("2026-07-26T12:00:04Z", 15, 7, 99, 99),
            last_token("2026-07-26T12:00:05Z", 2, 1),
        ],
    );

    let mut first = scanner(&root, &state);
    let summary = first.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(summary.advanced_sources, 1);
    let source_key = summary.sources[0].source_key.clone();
    let usage = first
        .state()
        .load_current_session_usage_page(&source_key, None, 500)
        .unwrap();
    assert_eq!(usage.items.len(), 3);
    assert_eq!(usage.items[0].input_tokens, 10);
    assert_eq!(usage.items[1].input_tokens, 5);
    assert_eq!(usage.items[2].input_tokens, 2);
    assert_ne!(usage.items[0].usage_id, usage.items[1].usage_id);
    assert_eq!(usage.items[0].model, "gpt-5.6-codex");
    let cursor = first
        .state()
        .load_current_session_scan_cursor(&source_key)
        .unwrap()
        .unwrap();
    assert_eq!(cursor.parser_checkpoint.event_ordinal, 5);
    assert_eq!(cursor.parser_checkpoint.previous_input_tokens, 17);
    drop(first);

    let mut restarted = scanner(&root, &state);
    let before = snapshot_tree(state.path());
    let second = restarted.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(second.advanced_sources, 0);
    assert_eq!(second.unchanged_sources, 1);
    assert_eq!(second.metrics.full_source_scans, 0);
    assert_eq!(snapshot_tree(state.path()), before);
}

#[test]
fn truncation_regrow_and_replacement_create_atomic_new_generations() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "sessions/2026/07/26/generation.jsonl";
    write_session(
        root.path(),
        relative,
        &[
            meta(
                "generation-thread",
                serde_json::json!("2026-07-26T12:00:00Z"),
            ),
            token("2026-07-26T12:00:01Z", 100, 10, 0),
        ],
    );
    let mut codex_scanner = scanner(&root, &state);
    let first = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    let source_key = first.sources[0].source_key.clone();
    assert_eq!(
        codex_scanner
            .state()
            .load_current_generation(&source_key)
            .unwrap(),
        Some(1)
    );

    write_session(
        root.path(),
        relative,
        &[
            meta(
                "generation-thread",
                serde_json::json!("2026-07-26T12:00:00Z"),
            ),
            token("2026-07-26T12:00:02Z", 5, 1, 0),
            serde_json::json!({"padding":"same identity regrows beyond old cursor boundary"}),
        ],
    );
    let second = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(second.advanced_sources, 1);
    assert_eq!(
        codex_scanner
            .state()
            .load_current_generation(&source_key)
            .unwrap(),
        Some(2)
    );
    let usage = codex_scanner
        .state()
        .load_current_session_usage_page(&source_key, None, 500)
        .unwrap();
    assert_eq!(usage.items.len(), 1);
    assert_eq!(usage.items[0].input_tokens, 5);
}

#[test]
fn interrupted_multibatch_candidate_stays_hidden_and_resumes() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let mut lines = vec![meta(
        "batch-thread",
        serde_json::json!("2026-07-26T12:00:00Z"),
    )];
    for ordinal in 1..=700 {
        lines.push(token("2026-07-26T12:00:01Z", ordinal, ordinal, 0));
    }
    write_session(root.path(), "sessions/2026/07/26/batches.jsonl", &lines);
    let mut codex_scanner = scanner(&root, &state);
    let interrupted = codex_scanner
        .scan(
            NOW,
            ScanControl {
                stop_after_committed_batches: Some(1),
            },
        )
        .unwrap();
    assert_eq!(interrupted.outcome, ScanOutcome::Interrupted);
    let source_key = interrupted.sources[0].source_key.clone();
    assert!(
        codex_scanner
            .state()
            .load_current_session_usage_page(&source_key, None, 500)
            .unwrap()
            .items
            .is_empty()
    );
    drop(codex_scanner);

    let mut resumed = scanner(&root, &state);
    let done = resumed.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(done.outcome, ScanOutcome::Complete);
    assert_eq!(
        resumed
            .state()
            .load_current_generation(&source_key)
            .unwrap(),
        Some(1)
    );
}

#[test]
fn interrupted_current_append_is_deferred_until_eof_and_resumes_without_losing_tail() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "sessions/2026/07/26/current-append-crash.jsonl";
    write_session(
        root.path(),
        relative,
        &[
            meta(
                "current-append-crash",
                serde_json::json!("2026-07-26T12:00:00Z"),
            ),
            token("2026-07-26T12:00:01Z", 1, 1, 0),
        ],
    );
    let mut initial = scanner(&root, &state);
    let first = initial.scan(NOW, ScanControl::default()).unwrap();
    let key = first.sources[0].source_key.clone();
    let mut writer = OpenOptions::new()
        .append(true)
        .open(root.path().join(relative))
        .unwrap();
    for ordinal in 2..=701 {
        serde_json::to_writer(
            &mut writer,
            &token("2026-07-26T12:00:02Z", ordinal, ordinal, 0),
        )
        .unwrap();
        writer.write_all(b"\n").unwrap();
    }
    writer.flush().unwrap();
    drop(writer);

    let interrupted = initial
        .scan(
            "2026-07-26T12:01:00Z",
            ScanControl {
                stop_after_committed_batches: Some(1),
            },
        )
        .unwrap();
    assert_eq!(interrupted.outcome, ScanOutcome::Interrupted);
    let interrupted_cursor = initial
        .state()
        .load_current_session_scan_cursor(&key)
        .unwrap()
        .unwrap();
    assert_eq!(
        interrupted_cursor.result_code,
        Some(SessionScanResultCode::Deferred)
    );
    let interrupted_source = initial.state().load_session_source(&key).unwrap().unwrap();
    assert_eq!(interrupted_source.status, SessionSourceStatus::Stale);
    assert_eq!(
        interrupted_source.error_code,
        Some(SessionSourceErrorCode::SourceCandidateInterrupted)
    );
    drop(initial);

    let mut restarted = scanner(&root, &state);
    let completed = restarted
        .scan("2026-07-26T12:02:00Z", ScanControl::default())
        .unwrap();
    assert_eq!(completed.outcome, ScanOutcome::Complete);
    assert_eq!(completed.advanced_sources, 1);
    let cursor = restarted
        .state()
        .load_current_session_scan_cursor(&key)
        .unwrap()
        .unwrap();
    assert_eq!(cursor.result_code, Some(SessionScanResultCode::Advanced));
    assert_eq!(
        cursor.complete_byte_offset,
        fs::metadata(root.path().join(relative)).unwrap().len()
    );
    let index = restarted
        .state()
        .load_current_session_index_page(&key, None, 1)
        .unwrap()
        .items
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(index.usage_event_count, 701);
}

#[test]
fn fork_replay_resolves_and_late_missing_ambiguous_inconsistent_are_stable() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let parent_records = vec![
        meta("parent", serde_json::json!("2026-07-26T12:00:00Z")),
        token("2026-07-26T12:00:01Z", 10, 1, 0),
        token("2026-07-26T12:00:02Z", 20, 2, 0),
    ];
    write_session(
        root.path(),
        "sessions/2026/07/26/parent.jsonl",
        &parent_records,
    );
    let mut child_meta = meta("child", serde_json::json!("2026-07-26T12:00:03Z"));
    child_meta["payload"]["forked_from_id"] = serde_json::json!("parent");
    write_session(
        root.path(),
        "sessions/2026/07/26/child.jsonl",
        &[
            child_meta,
            parent_records[1].clone(),
            parent_records[2].clone(),
            token("2026-07-26T12:00:04Z", 25, 4, 0),
        ],
    );
    let mut codex_scanner = scanner(&root, &state);
    let scan = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    let child = scan
        .sources
        .iter()
        .find(|item| item.root_thread_id.as_deref() == Some("child"))
        .unwrap();
    assert!(matches!(
        child.replay_resolution,
        ReplayResolution::Resolved { replayed_events: 2 }
    ));
    let usage = codex_scanner
        .state()
        .load_current_session_usage_page(&child.source_key, None, 500)
        .unwrap();
    assert_eq!(usage.items.len(), 1);
    assert_eq!(usage.items[0].input_tokens, 5);

    let missing_root = TempDir::new().unwrap();
    let missing_state = TempDir::new().unwrap();
    let mut missing_meta = meta("missing-child", serde_json::json!("2026-07-26T12:00:03Z"));
    missing_meta["payload"]["parent_thread_id"] = serde_json::json!("absent");
    write_session(
        missing_root.path(),
        "sessions/2026/07/26/child.jsonl",
        &[missing_meta],
    );
    let mut missing_scanner = scanner(&missing_root, &missing_state);
    let missing = missing_scanner.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(
        missing.sources[0].replay_resolution,
        ReplayResolution::Deferred(SessionSourceErrorCode::SourceReplayParentMissing)
    );
    assert_eq!(missing.sources[0].complete_byte_offset, 0);
}

#[test]
fn late_ambiguous_zero_prefix_and_incomplete_children_have_stable_replay_outcomes() {
    let late_root = TempDir::new().unwrap();
    let late_state = TempDir::new().unwrap();
    let mut child_meta = meta("late-child", serde_json::json!("2026-07-26T12:00:03Z"));
    child_meta["payload"]["parent_thread_id"] = serde_json::json!("late-parent");
    write_session(
        late_root.path(),
        "sessions/2026/07/26/child.jsonl",
        &[child_meta.clone(), token("2026-07-26T12:00:04Z", 15, 2, 0)],
    );
    let mut late_scanner = scanner(&late_root, &late_state);
    let deferred = late_scanner.scan(NOW, ScanControl::default()).unwrap();
    let child_key = deferred.sources[0].source_key.clone();
    assert_eq!(
        deferred.sources[0].replay_resolution,
        ReplayResolution::Deferred(SessionSourceErrorCode::SourceReplayParentMissing)
    );
    write_session(
        late_root.path(),
        "sessions/2026/07/26/parent.jsonl",
        &[
            meta("late-parent", serde_json::json!("2026-07-26T12:00:00Z")),
            token("2026-07-26T12:00:01Z", 10, 1, 0),
        ],
    );
    write_session(
        late_root.path(),
        "sessions/2026/07/26/child.jsonl",
        &[
            child_meta,
            token("2026-07-26T12:00:01Z", 10, 1, 0),
            token("2026-07-26T12:00:04Z", 15, 2, 0),
        ],
    );
    let resolved = late_scanner.scan(NOW, ScanControl::default()).unwrap();
    let child = resolved
        .sources
        .iter()
        .find(|source| source.source_key == child_key)
        .unwrap();
    assert_eq!(
        child.replay_resolution,
        ReplayResolution::Resolved { replayed_events: 1 }
    );

    let ambiguous_root = TempDir::new().unwrap();
    let ambiguous_state = TempDir::new().unwrap();
    for name in ["parent-a.jsonl", "parent-b.jsonl"] {
        write_session(
            ambiguous_root.path(),
            &format!("sessions/2026/07/26/{name}"),
            &[meta(
                "duplicate-parent",
                serde_json::json!("2026-07-26T12:00:00Z"),
            )],
        );
    }
    let mut ambiguous_meta = meta("ambiguous-child", serde_json::json!("2026-07-26T12:00:03Z"));
    ambiguous_meta["payload"]["forked_from_id"] = serde_json::json!("duplicate-parent");
    write_session(
        ambiguous_root.path(),
        "sessions/2026/07/26/child.jsonl",
        &[ambiguous_meta],
    );
    let mut ambiguous_scanner = scanner(&ambiguous_root, &ambiguous_state);
    let ambiguous = ambiguous_scanner.scan(NOW, ScanControl::default()).unwrap();
    assert!(ambiguous.sources.iter().any(|source| {
        source.replay_resolution
            == ReplayResolution::Deferred(SessionSourceErrorCode::SourceReplayParentAmbiguous)
    }));

    let incomplete_root = TempDir::new().unwrap();
    let incomplete_state = TempDir::new().unwrap();
    write_session(
        incomplete_root.path(),
        "sessions/2026/07/26/parent.jsonl",
        &[
            meta("future-parent", serde_json::json!("2026-07-26T12:00:00Z")),
            token("2026-07-26T12:05:00Z", 10, 1, 0),
        ],
    );
    let mut incomplete_meta = meta(
        "incomplete-child",
        serde_json::json!("2026-07-26T12:03:00Z"),
    );
    incomplete_meta["payload"]["parent_thread_id"] = serde_json::json!("future-parent");
    write_session(
        incomplete_root.path(),
        "sessions/2026/07/26/child.jsonl",
        &[incomplete_meta],
    );
    let mut missing_prefix_meta = meta(
        "missing-prefix-child",
        serde_json::json!("2026-07-26T12:06:00Z"),
    );
    missing_prefix_meta["payload"]["parent_thread_id"] = serde_json::json!("future-parent");
    write_session(
        incomplete_root.path(),
        "sessions/2026/07/26/missing-prefix.jsonl",
        &[missing_prefix_meta],
    );
    let mut incomplete_scanner = scanner(&incomplete_root, &incomplete_state);
    let incomplete = incomplete_scanner
        .scan(NOW, ScanControl::default())
        .unwrap();
    assert!(incomplete.sources.iter().any(|source| {
        source.root_thread_id.as_deref() == Some("incomplete-child")
            && source.replay_resolution == ReplayResolution::Resolved { replayed_events: 0 }
    }));
    assert!(incomplete.sources.iter().any(|source| {
        source.root_thread_id.as_deref() == Some("missing-prefix-child")
            && source.replay_resolution
                == ReplayResolution::Deferred(SessionSourceErrorCode::SourceReplayInconsistent)
    }));
}

#[test]
fn parent_generation_change_invalidates_and_rebuilds_child_lineage() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let parent_path = "sessions/2026/07/26/parent.jsonl";
    let child_path = "sessions/2026/07/26/child.jsonl";
    let make_parent = |input| {
        vec![
            meta("changing-parent", serde_json::json!("2026-07-26T12:00:00Z")),
            token("2026-07-26T12:00:01Z", input, 1, 0),
        ]
    };
    let make_child = |input| {
        let mut child = meta("changing-child", serde_json::json!("2026-07-26T12:00:03Z"));
        child["payload"]["parent_thread_id"] = serde_json::json!("changing-parent");
        vec![
            child,
            token("2026-07-26T12:00:01Z", input, 1, 0),
            token("2026-07-26T12:00:04Z", input + 5, 2, 0),
        ]
    };
    write_session(root.path(), parent_path, &make_parent(10));
    write_session(root.path(), child_path, &make_child(10));
    let mut codex_scanner = scanner(&root, &state);
    let first = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    let parent_key = first
        .sources
        .iter()
        .find(|source| source.root_thread_id.as_deref() == Some("changing-parent"))
        .unwrap()
        .source_key
        .clone();
    let child_key = first
        .sources
        .iter()
        .find(|source| source.root_thread_id.as_deref() == Some("changing-child"))
        .unwrap()
        .source_key
        .clone();

    write_session(root.path(), parent_path, &make_parent(20));
    write_session(root.path(), child_path, &make_child(20));
    codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(
        codex_scanner
            .state()
            .load_current_generation(&parent_key)
            .unwrap(),
        Some(2)
    );
    let child_cursor = codex_scanner
        .state()
        .load_current_session_scan_cursor(&child_key)
        .unwrap()
        .unwrap();
    assert_eq!(child_cursor.generation, 2);
    assert_eq!(child_cursor.parent_generation, Some(2));
}

#[test]
fn rewritten_child_replay_prefix_is_revalidated_before_generation_rebuild() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let parent = vec![
        meta(
            "rewritten-prefix-parent",
            serde_json::json!("2026-07-26T12:00:00Z"),
        ),
        token("2026-07-26T12:00:01Z", 10, 1, 0),
        token("2026-07-26T12:00:02Z", 20, 2, 0),
    ];
    write_session(
        root.path(),
        "sessions/2026/07/26/parent-prefix.jsonl",
        &parent,
    );
    let mut child_meta = meta(
        "rewritten-prefix-child",
        serde_json::json!("2026-07-26T12:00:03Z"),
    );
    child_meta["payload"]["parent_thread_id"] = serde_json::json!("rewritten-prefix-parent");
    write_session(
        root.path(),
        "sessions/2026/07/26/child-prefix.jsonl",
        &[
            child_meta,
            parent[1].clone(),
            parent[2].clone(),
            token("2026-07-26T12:00:04Z", 25, 4, 0),
        ],
    );
    let child_path = root.path().join("sessions/2026/07/26/child-prefix.jsonl");
    let mut codex_scanner = scanner(&root, &state);
    let initial = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    let child = initial
        .sources
        .iter()
        .find(|source| source.root_thread_id.as_deref() == Some("rewritten-prefix-child"))
        .unwrap();
    let child_key = child.source_key.clone();
    let old_modified = fs::metadata(&child_path).unwrap().modified().unwrap();
    let mut bytes = fs::read(&child_path).unwrap();
    let needle = b"\"input_tokens\":10";
    let position = bytes
        .windows(needle.len())
        .position(|window| window == needle)
        .unwrap()
        + b"\"input_tokens\":".len();
    bytes[position + 1] = b'1';
    fs::write(&child_path, bytes).unwrap();
    OpenOptions::new()
        .write(true)
        .open(&child_path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(old_modified))
        .unwrap();

    let rewritten = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    let child = rewritten
        .sources
        .iter()
        .find(|source| source.root_thread_id.as_deref() == Some("rewritten-prefix-child"))
        .unwrap();
    assert_eq!(
        child.replay_resolution,
        ReplayResolution::Deferred(SessionSourceErrorCode::SourceReplayInconsistent)
    );
    assert_eq!(child.complete_byte_offset, 0);
    assert_eq!(
        codex_scanner
            .state()
            .load_current_generation(&child_key)
            .unwrap(),
        Some(1)
    );
    let usage = codex_scanner
        .state()
        .load_current_session_usage_page(&child_key, None, 500)
        .unwrap();
    assert_eq!(usage.items.len(), 1);
    assert_eq!(usage.items[0].input_tokens, 5);
}

#[test]
fn long_parent_pages_survive_restart_and_multiple_late_children_reuse_them() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let mut parent = vec![meta(
        "long-parent",
        serde_json::json!("2026-07-26T12:00:00Z"),
    )];
    for ordinal in 1..=1_200 {
        parent.push(token("2026-07-26T12:00:01Z", ordinal, ordinal, 0));
    }
    write_session(root.path(), "sessions/2026/07/26/parent.jsonl", &parent);
    let child_records = |id: &str| {
        let mut child_meta = meta(id, serde_json::json!("2026-07-26T12:10:00Z"));
        child_meta["payload"]["parent_thread_id"] = serde_json::json!("long-parent");
        let mut records = vec![child_meta];
        records.extend(parent.iter().skip(1).cloned());
        records.push(token("2026-07-26T12:11:00Z", 1_205, 1_205, 0));
        records
    };
    write_session(
        root.path(),
        "sessions/2026/07/26/child-1.jsonl",
        &child_records("long-child-1"),
    );
    let mut first = scanner(&root, &state);
    let initial = first.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(initial.metrics.parent_index_builds, 1);
    assert_eq!(initial.metrics.maximum_replay_page_rows, 512);
    assert_eq!(initial.metrics.full_source_scans, 2);
    drop(first);

    write_session(
        root.path(),
        "sessions/2026/07/26/child-2.jsonl",
        &child_records("long-child-2"),
    );
    write_session(
        root.path(),
        "sessions/2026/07/26/child-3.jsonl",
        &child_records("long-child-3"),
    );
    let mut restarted = scanner(&root, &state);
    let late = restarted.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(late.metrics.parent_index_builds, 0);
    assert_eq!(late.metrics.full_source_scans, 2);
    assert_eq!(late.metrics.replay_child_scans, 2);
    assert_eq!(late.metrics.maximum_replay_page_rows, 512);
}

#[test]
fn parent_index_with_a_middle_ordinal_gap_is_rebuilt_once_then_all_children_use_committed_pages() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let parent = vec![
        meta("lease-parent", serde_json::json!("2026-07-26T12:00:00Z")),
        token("2026-07-26T12:00:01Z", 10, 1, 0),
        token("2026-07-26T12:00:02Z", 20, 2, 0),
        token("2026-07-26T12:00:03Z", 30, 3, 0),
    ];
    write_session(root.path(), "sessions/2026/07/26/parent.jsonl", &parent);
    let mut initial = scanner(&root, &state);
    initial.scan(NOW, ScanControl::default()).unwrap();
    drop(initial);
    let state_path = state.path().join("state.sqlite3");
    let connection = rusqlite::Connection::open(&state_path).unwrap();
    connection
        .execute(
            "DELETE FROM codex_replay_signatures WHERE token_event_ordinal = 2",
            [],
        )
        .unwrap();
    drop(connection);
    for index in 1..=2 {
        let mut child_meta = meta(
            &format!("lease-child-{index}"),
            serde_json::json!("2026-07-26T12:00:04Z"),
        );
        child_meta["payload"]["parent_thread_id"] = serde_json::json!("lease-parent");
        write_session(
            root.path(),
            &format!("sessions/2026/07/26/child-{index}.jsonl"),
            &[
                child_meta,
                parent[1].clone(),
                parent[2].clone(),
                parent[3].clone(),
                token("2026-07-26T12:00:05Z", 35, 4, 0),
            ],
        );
    }

    let mut rebuild = scanner(&root, &state);
    let rebuilt = rebuild.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(rebuilt.metrics.parent_index_builds, 1);
    assert_eq!(rebuilt.metrics.full_source_scans, 3);
    assert_eq!(rebuilt.metrics.maximum_replay_page_rows, 3);
    for child in rebuilt.sources.iter().filter(|source| {
        source
            .root_thread_id
            .as_deref()
            .is_some_and(|thread| thread.starts_with("lease-child-"))
    }) {
        let usage = rebuild
            .state()
            .load_current_session_usage_page(&child.source_key, None, 500)
            .unwrap();
        assert_eq!(usage.items.len(), 1);
        assert_eq!(usage.items[0].input_tokens, 5);
    }
    drop(rebuild);

    let mut restarted = scanner(&root, &state);
    let reused = restarted.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(reused.metrics.parent_index_builds, 0);
    assert_eq!(reused.metrics.full_source_scans, 0);
    assert_eq!(reused.metrics.replay_child_scans, 0);
}

#[test]
fn replay_pages_are_bounded_and_rollout_limit_is_stable() {
    assert_eq!(wokcore_sessions::codex::REPLAY_PAGE_SIZE, 512);
    assert_eq!(MAX_CODEX_REPLAY_SIGNATURES, 262_144);
}

#[test]
fn immutable_title_sources_are_bounded_and_zero_write_with_active_wal() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_session(
        root.path(),
        "sessions/2026/07/26/title.jsonl",
        &[meta(
            "title-thread",
            serde_json::json!("2026-07-26T12:00:00Z"),
        )],
    );
    let index = root.path().join("session_index.jsonl");
    fs::write(
        &index,
        b"{\"id\":\"title-thread\",\"thread_name\":\"Old explicit\",\"updated_at\":\"2026-07-26T10:00:00Z\"}\n\
          {\"id\":\"title-thread\",\"thread_name\":\"Explicit title\",\"updated_at\":\"2026-07-26T09:00:00Z\"}\n",
    )
    .unwrap();
    let db = root.path().join("state_5.sqlite");
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA wal_autocheckpoint=0;
             CREATE TABLE threads(id TEXT PRIMARY KEY, title TEXT NOT NULL, updated_at INTEGER);
             INSERT INTO threads VALUES('title-thread', 'checkpointed', 1);
             PRAGMA wal_checkpoint(TRUNCATE);",
        )
        .unwrap();
    connection
        .execute(
            "UPDATE threads SET title='uncheckpointed', updated_at=3 WHERE id='title-thread'",
            [],
        )
        .unwrap();
    let before = snapshot_tree(root.path());
    let mut scanner = scanner(&root, &state);
    let scan = scanner.scan(NOW, ScanControl::default()).unwrap();
    let title = scan.sources[0].title.as_deref();
    assert!(matches!(
        title,
        Some("Explicit title") | Some("checkpointed") | None
    ));
    assert_eq!(snapshot_tree(root.path()), before);
    drop(connection);
}

#[test]
fn immutable_title_query_filters_oversized_fields_within_its_candidate_window() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_session(
        root.path(),
        "sessions/2026/07/26/bounded-db-title.jsonl",
        &[meta(
            "bounded-db-title",
            serde_json::json!("2026-07-26T12:00:00Z"),
        )],
    );
    let db = root.path().join("state_5.sqlite");
    let mut connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE threads(id TEXT PRIMARY KEY, title TEXT NOT NULL, updated_at INTEGER);",
        )
        .unwrap();
    let transaction = connection.transaction().unwrap();
    transaction
        .execute(
            "INSERT INTO threads VALUES(?1, ?2, 10000)",
            rusqlite::params!["x".repeat(1024 * 1024), "y".repeat(1024 * 1024)],
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO threads VALUES('bounded-db-title', 'Bounded title', 9999)",
            [],
        )
        .unwrap();
    transaction.commit().unwrap();
    drop(connection);

    let mut codex_scanner = scanner(&root, &state);
    let summary = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(summary.sources[0].title.as_deref(), Some("Bounded title"));
    assert!(!format!("{summary:?}").contains(&"x".repeat(1024)));
    assert!(!format!("{summary:?}").contains(&"y".repeat(1024)));
}

#[test]
fn immutable_title_query_does_not_search_beyond_its_candidate_window() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_session(
        root.path(),
        "sessions/2026/07/26/db-title-window.jsonl",
        &[meta(
            "db-title-window",
            serde_json::json!("2026-07-26T12:00:00Z"),
        )],
    );
    let db = root.path().join("state_5.sqlite");
    let mut connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE threads(id TEXT PRIMARY KEY, title TEXT NOT NULL, updated_at INTEGER);",
        )
        .unwrap();
    let transaction = connection.transaction().unwrap();
    for ordinal in 0..4_096 {
        transaction
            .execute(
                "INSERT INTO threads VALUES(?1, ?2, ?3)",
                rusqlite::params![
                    format!("{ordinal:04}-{}", "i".repeat(512)),
                    "t".repeat(513),
                    10_000 - ordinal
                ],
            )
            .unwrap();
    }
    transaction
        .execute(
            "INSERT INTO threads VALUES('db-title-window', 'Outside budget', 1)",
            [],
        )
        .unwrap();
    transaction.commit().unwrap();
    drop(connection);

    let mut codex_scanner = scanner(&root, &state);
    let summary = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(summary.sources[0].title, None);
}

#[test]
fn oversized_title_index_falls_back_instead_of_returning_an_old_entry() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_session(
        root.path(),
        "sessions/2026/07/26/title-limit.jsonl",
        &[meta(
            "title-limit",
            serde_json::json!("2026-07-26T12:00:00Z"),
        )],
    );
    let mut bytes = Vec::new();
    for index in 0..=4_096 {
        serde_json::to_writer(
            &mut bytes,
            &serde_json::json!({
                "id": if index == 0 {"title-limit"} else {"other"},
                "thread_name": if index == 0 {"stale"} else {"ignored"},
                "updated_at":"2026-07-26T12:00:00Z"
            }),
        )
        .unwrap();
        bytes.push(b'\n');
    }
    fs::write(root.path().join("session_index.jsonl"), bytes).unwrap();
    let mut codex_scanner = scanner(&root, &state);
    let summary = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(summary.sources[0].title, None);
}

#[test]
fn outputs_debug_and_database_never_contain_session_body_or_raw_path() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let secret = "PROMPT_SECRET_XYZ";
    write_session(
        root.path(),
        "sessions/2026/07/26/private-name.jsonl",
        &[
            meta("privacy-thread", serde_json::json!("2026-07-26T12:00:00Z")),
            serde_json::json!({"type":"response_item","payload":{"content":secret,"tool_output":secret}}),
            token("2026-07-26T12:00:01Z", 1, 1, 0),
        ],
    );
    let mut scanner = scanner(&root, &state);
    let summary = scanner.scan(NOW, ScanControl::default()).unwrap();
    assert!(!format!("{summary:?}").contains(secret));
    assert!(!format!("{summary:?}").contains("private-name.jsonl"));
    assert!(!format!("{summary:?}").contains(&root.path().display().to_string()));
    let parse_error = parse_codex_record(&serde_json::json!({
        "type":"session_meta","timestamp":"2026-07-26T12:00:00Z",
        "payload":{"id":7,"secret":secret}
    }))
    .unwrap_err();
    for formatted in [format!("{parse_error:?}"), parse_error.to_string()] {
        assert!(!formatted.contains(secret));
        assert!(!formatted.contains("private-name.jsonl"));
        assert!(!formatted.contains(&root.path().display().to_string()));
    }
    drop(scanner);
    let raw_path = root.path().display().to_string();
    for (name, _, _, bytes) in snapshot_tree(state.path()) {
        assert!(
            !bytes
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "secret appeared in {name}"
        );
        assert!(
            !bytes
                .windows(b"private-name.jsonl".len())
                .any(|window| window == b"private-name.jsonl"),
            "basename appeared in {name}"
        );
        assert!(
            !bytes
                .windows(raw_path.len())
                .any(|window| window == raw_path.as_bytes()),
            "absolute path appeared in {name}"
        );
    }
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
            let metadata = fs::metadata(&path).unwrap();
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

#[test]
fn source_failure_retains_last_promoted_aggregate() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "sessions/2026/07/26/failure.jsonl";
    write_session(
        root.path(),
        relative,
        &[
            meta("failure-thread", serde_json::json!("2026-07-26T12:00:00Z")),
            token("2026-07-26T12:00:01Z", 9, 2, 0),
        ],
    );
    let mut scanner = scanner(&root, &state);
    let first = scanner.scan(NOW, ScanControl::default()).unwrap();
    let key = first.sources[0].source_key.clone();
    fs::write(root.path().join(relative), vec![b'x'; 16 * 1024 * 1024 + 2]).unwrap();
    let failed = scanner.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(failed.sources[0].status, SessionSourceStatus::Stale);
    let usage = scanner
        .state()
        .load_current_session_usage_page(&key, None, 500)
        .unwrap();
    assert_eq!(usage.items.len(), 1);
    fs::remove_file(root.path().join(relative)).unwrap();
    let deleted = scanner.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(deleted.deleted_sources, 1);
    let source = scanner.state().load_session_source(&key).unwrap().unwrap();
    assert_eq!(source.status, SessionSourceStatus::Stale);
    assert_eq!(
        source.error_code,
        Some(SessionSourceErrorCode::SourceSessionsAbsent)
    );
    let usage = scanner
        .state()
        .load_current_session_usage_page(&key, None, 500)
        .unwrap();
    assert_eq!(usage.items.len(), 1);
    let before_repeat = snapshot_tree(state.path());
    let repeated = scanner
        .scan("2026-07-26T12:01:00Z", ScanControl::default())
        .unwrap();
    assert_eq!(repeated.deleted_sources, 1);
    assert_eq!(snapshot_tree(state.path()), before_repeat);
}

#[test]
fn malformed_sources_are_contained_idempotently_while_a_sibling_advances() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let bad_relative = "sessions/2026/07/26/a-bad-current.jsonl";
    let sibling_relative = "sessions/2026/07/26/z-good-sibling.jsonl";
    write_session(
        root.path(),
        bad_relative,
        &[
            meta("bad-current", serde_json::json!("2026-07-26T12:00:00Z")),
            token("2026-07-26T12:00:01Z", 9, 2, 0),
        ],
    );
    write_session(
        root.path(),
        sibling_relative,
        &[meta(
            "good-sibling",
            serde_json::json!("2026-07-26T12:00:00Z"),
        )],
    );
    let mut codex_scanner = scanner(&root, &state);
    let initial = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    let bad_key = initial
        .sources
        .iter()
        .find(|source| source.root_thread_id.as_deref() == Some("bad-current"))
        .unwrap()
        .source_key
        .clone();

    let mut bad_writer = OpenOptions::new()
        .append(true)
        .open(root.path().join(bad_relative))
        .unwrap();
    serde_json::to_writer(
        &mut bad_writer,
        &serde_json::json!({
            "timestamp":"2026-07-26T12:00:02Z",
            "type":"event_msg",
            "payload":{"type":"token_count","info":{
                "total_token_usage":{"input_tokens":-1}
            }}
        }),
    )
    .unwrap();
    bad_writer.write_all(b"\n").unwrap();
    bad_writer.flush().unwrap();
    drop(bad_writer);
    let mut sibling_writer = OpenOptions::new()
        .append(true)
        .open(root.path().join(sibling_relative))
        .unwrap();
    serde_json::to_writer(&mut sibling_writer, &token("2026-07-26T12:00:03Z", 4, 1, 0)).unwrap();
    sibling_writer.write_all(b"\n").unwrap();
    sibling_writer.flush().unwrap();
    drop(sibling_writer);
    write_session(
        root.path(),
        "sessions/2026/07/26/b-initial-malformed.jsonl",
        &[
            meta(
                "initial-malformed",
                serde_json::json!("2026-07-26T12:00:00Z"),
            ),
            serde_json::json!({
                "timestamp":"2026-07-26T12:00:01Z",
                "type":"event_msg",
                "payload":{"type":"token_count","info":{
                    "last_token_usage":{"output_tokens":-1}
                }}
            }),
        ],
    );

    let failed = codex_scanner
        .scan("2026-07-26T12:01:00Z", ScanControl::default())
        .unwrap();
    let bad = failed
        .sources
        .iter()
        .find(|source| source.root_thread_id.as_deref() == Some("bad-current"))
        .unwrap();
    assert_eq!(bad.status, SessionSourceStatus::Stale);
    assert_eq!(
        bad.error_code,
        Some(SessionSourceErrorCode::SourceParseInvalid)
    );
    let initial_bad = failed
        .sources
        .iter()
        .find(|source| source.root_thread_id.as_deref() == Some("initial-malformed"))
        .unwrap();
    assert_eq!(initial_bad.status, SessionSourceStatus::Unavailable);
    assert_eq!(
        initial_bad.error_code,
        Some(SessionSourceErrorCode::SourceParseInvalid)
    );
    let sibling = failed
        .sources
        .iter()
        .find(|source| source.root_thread_id.as_deref() == Some("good-sibling"))
        .unwrap();
    assert_eq!(sibling.status, SessionSourceStatus::Available);
    assert_eq!(
        codex_scanner
            .state()
            .load_current_session_usage_page(&bad_key, None, 500)
            .unwrap()
            .items
            .len(),
        1
    );

    let before_repeat = snapshot_tree(state.path());
    let repeated = codex_scanner
        .scan("2026-07-26T12:01:00Z", ScanControl::default())
        .unwrap();
    assert!(repeated.sources.iter().any(|source| {
        source.root_thread_id.as_deref() == Some("good-sibling")
            && source.status == SessionSourceStatus::Available
    }));
    assert_eq!(snapshot_tree(state.path()), before_repeat);
}

#[test]
fn deleted_source_reappearance_recovers_once_then_becomes_write_free() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "sessions/2026/07/26/reappearing.jsonl";
    write_session(
        root.path(),
        relative,
        &[
            meta("reappearing", serde_json::json!("2026-07-26T12:00:00Z")),
            token("2026-07-26T12:00:01Z", 5, 1, 0),
        ],
    );
    let path = root.path().join(relative);
    let held = root.path().join("held-reappearing.jsonl");
    let mut codex_scanner = scanner(&root, &state);
    let first = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    let key = first.sources[0].source_key.clone();
    fs::rename(&path, &held).unwrap();
    codex_scanner
        .scan("2026-07-26T12:01:00Z", ScanControl::default())
        .unwrap();
    assert_eq!(
        codex_scanner
            .state()
            .load_session_source(&key)
            .unwrap()
            .unwrap()
            .status,
        SessionSourceStatus::Stale
    );

    fs::rename(&held, &path).unwrap();
    let restored = codex_scanner
        .scan("2026-07-26T12:02:00Z", ScanControl::default())
        .unwrap();
    assert_eq!(restored.sources[0].source_key, key);
    assert_eq!(restored.sources[0].status, SessionSourceStatus::Available);
    drop(codex_scanner);
    let mut restarted = scanner(&root, &state);
    let before_unchanged = snapshot_tree(state.path());
    let unchanged = restarted
        .scan("2026-07-26T12:03:00Z", ScanControl::default())
        .unwrap();
    assert_eq!(unchanged.unchanged_sources, 1);
    assert_eq!(snapshot_tree(state.path()), before_unchanged);
}

#[test]
fn persisted_live_to_archive_move_inherits_key_generation_cursor_and_usage_ids() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "sessions/2026/07/26/move.jsonl";
    write_session(
        root.path(),
        relative,
        &[
            meta("move-thread", serde_json::json!("2026-07-26T12:00:00Z")),
            token("2026-07-26T12:00:01Z", 8, 2, 0),
        ],
    );
    let mut codex_scanner = scanner(&root, &state);
    let first = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    let key = first.sources[0].source_key.clone();
    let cursor = codex_scanner
        .state()
        .load_current_session_scan_cursor(&key)
        .unwrap()
        .unwrap();
    let usage_ids = codex_scanner
        .state()
        .load_current_session_usage_page(&key, None, 500)
        .unwrap()
        .items
        .into_iter()
        .map(|usage| usage.usage_id)
        .collect::<Vec<_>>();

    fs::create_dir(root.path().join("archived_sessions")).unwrap();
    fs::rename(
        root.path().join(relative),
        root.path().join("archived_sessions/move.jsonl"),
    )
    .unwrap();
    let moved = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(moved.deleted_sources, 0);
    assert_eq!(moved.sources[0].source_key, key);
    assert_eq!(
        codex_scanner
            .state()
            .load_current_session_scan_cursor(&key)
            .unwrap()
            .unwrap(),
        cursor
    );
    assert_eq!(
        codex_scanner
            .state()
            .load_current_session_usage_page(&key, None, 500)
            .unwrap()
            .items
            .into_iter()
            .map(|usage| usage.usage_id)
            .collect::<Vec<_>>(),
        usage_ids
    );
}

#[test]
fn archived_old_inode_and_new_live_inode_at_the_same_path_receive_distinct_stable_keys() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let live = "sessions/2026/07/26/reused-path.jsonl";
    write_session(
        root.path(),
        live,
        &[
            meta("archived-a", serde_json::json!("2026-07-26T12:00:00Z")),
            token("2026-07-26T12:00:01Z", 10, 1, 0),
        ],
    );
    let mut codex_scanner = scanner(&root, &state);
    let first = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    let old_key = first.sources[0].source_key.clone();

    fs::create_dir(root.path().join("archived_sessions")).unwrap();
    fs::rename(
        root.path().join(live),
        root.path().join("archived_sessions/archived-a.jsonl"),
    )
    .unwrap();
    write_session(
        root.path(),
        live,
        &[
            meta("new-live-b", serde_json::json!("2026-07-26T12:01:00Z")),
            token("2026-07-26T12:01:01Z", 20, 2, 0),
        ],
    );

    let split = codex_scanner
        .scan("2026-07-26T12:02:00Z", ScanControl::default())
        .unwrap();
    assert_eq!(split.sources.len(), 2);
    let archived = split
        .sources
        .iter()
        .find(|source| source.root_thread_id.as_deref() == Some("archived-a"))
        .unwrap();
    let live = split
        .sources
        .iter()
        .find(|source| source.root_thread_id.as_deref() == Some("new-live-b"))
        .unwrap();
    assert_eq!(archived.source_key, old_key);
    assert_ne!(live.source_key, old_key);
    assert_ne!(archived.source_key, live.source_key);

    let before = snapshot_tree(state.path());
    let repeated = codex_scanner
        .scan("2026-07-26T12:03:00Z", ScanControl::default())
        .unwrap();
    assert_eq!(repeated.unchanged_sources, 2);
    assert_eq!(repeated.metrics.full_source_scans, 0);
    assert_eq!(snapshot_tree(state.path()), before);
}

#[test]
fn interrupted_candidate_move_to_archive_resumes_the_same_staging_key() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "sessions/2026/07/26/staging-move.jsonl";
    let mut lines = vec![meta(
        "staging-move",
        serde_json::json!("2026-07-26T12:00:00Z"),
    )];
    for ordinal in 1..=700 {
        lines.push(token("2026-07-26T12:00:01Z", ordinal, ordinal, 0));
    }
    write_session(root.path(), relative, &lines);
    let mut codex_scanner = scanner(&root, &state);
    let interrupted = codex_scanner
        .scan(
            NOW,
            ScanControl {
                stop_after_committed_batches: Some(1),
            },
        )
        .unwrap();
    let key = interrupted.sources[0].source_key.clone();
    assert!(
        codex_scanner
            .state()
            .load_staging_session_scan_cursor(&key)
            .unwrap()
            .is_some()
    );
    fs::create_dir(root.path().join("archived_sessions")).unwrap();
    fs::rename(
        root.path().join(relative),
        root.path().join("archived_sessions/staging-move.jsonl"),
    )
    .unwrap();
    drop(codex_scanner);

    let mut restarted = scanner(&root, &state);
    let completed = restarted.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(completed.sources[0].source_key, key);
    assert_eq!(
        restarted.state().load_current_generation(&key).unwrap(),
        Some(1)
    );
    assert!(
        restarted
            .state()
            .load_staging_session_scan_cursor(&key)
            .unwrap()
            .is_none()
    );
}

#[test]
fn interrupted_candidate_resumes_after_source_append_without_duplicate_staging_rows() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "sessions/2026/07/26/staging-append.jsonl";
    let mut lines = vec![meta(
        "staging-append",
        serde_json::json!("2026-07-26T12:00:00Z"),
    )];
    for ordinal in 1..=700 {
        lines.push(token(
            &format!("2026-07-26T12:{:02}:00Z", ordinal / 60),
            ordinal,
            ordinal,
            0,
        ));
    }
    write_session(root.path(), relative, &lines);
    let mut codex_scanner = scanner(&root, &state);
    let interrupted = codex_scanner
        .scan(
            NOW,
            ScanControl {
                stop_after_committed_batches: Some(1),
            },
        )
        .unwrap();
    let key = interrupted.sources[0].source_key.clone();
    let staged_offset = codex_scanner
        .state()
        .load_staging_session_scan_cursor(&key)
        .unwrap()
        .unwrap()
        .complete_byte_offset;
    let mut writer = OpenOptions::new()
        .append(true)
        .open(root.path().join(relative))
        .unwrap();
    for ordinal in 701..=750 {
        serde_json::to_writer(
            &mut writer,
            &token("2026-07-26T12:59:00Z", ordinal, ordinal, 0),
        )
        .unwrap();
        writer.write_all(b"\n").unwrap();
    }
    writer.flush().unwrap();
    drop(writer);
    drop(codex_scanner);

    let mut restarted = scanner(&root, &state);
    let completed = restarted.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(completed.outcome, ScanOutcome::Complete);
    assert_eq!(
        restarted.state().load_current_generation(&key).unwrap(),
        Some(1)
    );
    let cursor = restarted
        .state()
        .load_current_session_scan_cursor(&key)
        .unwrap()
        .unwrap();
    assert!(cursor.complete_byte_offset > staged_offset);
    let index = restarted
        .state()
        .load_current_session_index_page(&key, None, 1)
        .unwrap()
        .items
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(index.usage_event_count, 750);
}

#[test]
fn interrupted_replacement_move_to_archive_promotes_the_second_generation() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "sessions/2026/07/26/replacement-move.jsonl";
    write_session(
        root.path(),
        relative,
        &[
            meta(
                "replacement-move",
                serde_json::json!("2026-07-26T12:00:00Z"),
            ),
            token("2026-07-26T12:00:01Z", 1, 1, 0),
        ],
    );
    let mut initial = scanner(&root, &state);
    let first = initial.scan(NOW, ScanControl::default()).unwrap();
    let key = first.sources[0].source_key.clone();
    let live = root.path().join(relative);
    fs::rename(&live, live.with_extension("old")).unwrap();
    let mut replacement = vec![meta(
        "replacement-move",
        serde_json::json!("2026-07-26T12:00:00Z"),
    )];
    for ordinal in 1..=700 {
        replacement.push(token("2026-07-26T12:00:01Z", ordinal, ordinal, 0));
    }
    write_session(root.path(), relative, &replacement);
    let interrupted = initial
        .scan(
            NOW,
            ScanControl {
                stop_after_committed_batches: Some(1),
            },
        )
        .unwrap();
    assert_eq!(interrupted.outcome, ScanOutcome::Interrupted);
    assert_eq!(interrupted.sources[0].source_key, key);
    assert_eq!(
        initial
            .state()
            .load_staging_session_scan_cursor(&key)
            .unwrap()
            .unwrap()
            .generation,
        2
    );
    fs::create_dir(root.path().join("archived_sessions")).unwrap();
    fs::rename(
        root.path().join(relative),
        root.path().join("archived_sessions/replacement-move.jsonl"),
    )
    .unwrap();
    drop(initial);

    let mut restarted = scanner(&root, &state);
    let completed = restarted.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(completed.sources[0].source_key, key);
    assert_eq!(
        restarted.state().load_current_generation(&key).unwrap(),
        Some(2)
    );
    let index = restarted
        .state()
        .load_current_session_index_page(&key, None, 1)
        .unwrap()
        .items
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(index.usage_event_count, 700);
}

#[test]
fn retired_replay_history_cleanup_advances_one_bounded_batch_per_scan() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "sessions/2026/07/26/bounded-retired.jsonl";
    let mut original = vec![meta(
        "bounded-retired",
        serde_json::json!("2026-07-26T12:00:00Z"),
    )];
    for ordinal in 1..=900 {
        original.push(token("2026-07-26T12:00:01Z", ordinal, ordinal, 0));
    }
    write_session(root.path(), relative, &original);
    let mut codex_scanner = scanner(&root, &state);
    let first = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    let key = first.sources[0].source_key.clone();
    let state_path = state.path().join("state.sqlite3");
    let rows_for_generation = |generation: u64| {
        let connection = rusqlite::Connection::open(&state_path).unwrap();
        usize::try_from(
            connection
                .query_row(
                    "SELECT
                    (SELECT COUNT(*) FROM session_index
                     WHERE source_key = ?1 AND generation = ?2)
                  + (SELECT COUNT(*) FROM session_usage_records
                     WHERE source_key = ?1 AND generation = ?2)
                  + (SELECT COUNT(*) FROM codex_replay_signatures
                     WHERE parent_source_key = ?1 AND parent_generation = ?2)
                  + (SELECT COUNT(*) FROM session_scan_cursors
                     WHERE source_key = ?1 AND generation = ?2)",
                    rusqlite::params![&key, i64::try_from(generation).unwrap()],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
        )
        .unwrap()
    };
    let original_rows = rows_for_generation(1);
    assert!(original_rows > MAX_SESSION_BATCH_ROWS * 3);

    write_session(
        root.path(),
        relative,
        &[
            meta("bounded-retired", serde_json::json!("2026-07-26T12:00:00Z")),
            token("2026-07-26T12:02:00Z", 2, 1, 0),
        ],
    );
    write_session(
        root.path(),
        "sessions/2026/07/26/z-cleanup-sibling.jsonl",
        &[
            meta(
                "z-cleanup-sibling",
                serde_json::json!("2026-07-26T12:02:00Z"),
            ),
            token("2026-07-26T12:02:01Z", 3, 1, 0),
        ],
    );
    let replacement = codex_scanner
        .scan("2026-07-26T12:02:01Z", ScanControl::default())
        .unwrap();
    assert_eq!(replacement.outcome, ScanOutcome::Interrupted);
    assert_eq!(
        codex_scanner.state().load_current_generation(&key).unwrap(),
        Some(2)
    );
    let after_first_cleanup = rows_for_generation(1);
    assert!(original_rows - after_first_cleanup <= MAX_SESSION_BATCH_ROWS);
    assert!(after_first_cleanup > 0);
    assert!(replacement.sources.iter().any(|source| {
        source.root_thread_id.as_deref() == Some("z-cleanup-sibling")
            && source.status == SessionSourceStatus::Available
    }));

    let mut previous = after_first_cleanup;
    for attempt in 0..10 {
        let summary = codex_scanner
            .scan(
                &format!("2026-07-26T12:03:{attempt:02}Z"),
                ScanControl::default(),
            )
            .unwrap();
        assert_eq!(
            codex_scanner.state().load_current_generation(&key).unwrap(),
            Some(2)
        );
        let remaining = rows_for_generation(1);
        assert!(previous - remaining <= MAX_SESSION_BATCH_ROWS);
        previous = remaining;
        if remaining == 0 {
            assert_eq!(summary.outcome, ScanOutcome::Complete);
            break;
        }
        assert_eq!(summary.outcome, ScanOutcome::Interrupted);
    }
    assert_eq!(previous, 0);
}

#[test]
fn one_scan_never_cleans_both_an_old_retired_generation_and_a_newly_retired_generation() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "sessions/2026/07/26/one-cleanup-budget.jsonl";
    let mut generation_one = vec![meta(
        "one-cleanup-budget",
        serde_json::json!("2026-07-26T12:00:00Z"),
    )];
    for ordinal in 1..=600 {
        generation_one.push(token("2026-07-26T12:00:01Z", ordinal, ordinal, 0));
    }
    write_session(root.path(), relative, &generation_one);
    let mut codex_scanner = scanner(&root, &state);
    let first = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    let key = first.sources[0].source_key.clone();

    let generation_two = [
        meta(
            "one-cleanup-budget",
            serde_json::json!("2026-07-26T12:00:00Z"),
        ),
        token("2026-07-26T12:01:00Z", 2, 1, 0),
    ];
    write_session(root.path(), relative, &generation_two);
    let promoted_two = codex_scanner
        .scan("2026-07-26T12:01:01Z", ScanControl::default())
        .unwrap();
    assert_eq!(promoted_two.outcome, ScanOutcome::Interrupted);
    let second_cleanup = codex_scanner
        .scan("2026-07-26T12:01:02Z", ScanControl::default())
        .unwrap();
    assert_eq!(second_cleanup.outcome, ScanOutcome::Interrupted);
    assert_eq!(
        codex_scanner
            .state()
            .load_session_source(&key)
            .unwrap()
            .unwrap()
            .retired_generation,
        Some(1)
    );

    let generation_three = [
        meta(
            "one-cleanup-budget",
            serde_json::json!("2026-07-26T12:00:00Z"),
        ),
        token("2026-07-26T12:02:00Z", 3, 1, 0),
    ];
    write_session(root.path(), relative, &generation_three);
    let promoted_three = codex_scanner
        .scan("2026-07-26T12:02:01Z", ScanControl::default())
        .unwrap();
    let source = codex_scanner
        .state()
        .load_session_source(&key)
        .unwrap()
        .unwrap();
    assert_eq!(source.current_generation, Some(3));
    assert_eq!(source.retired_generation, Some(2));
    assert_eq!(promoted_three.outcome, ScanOutcome::Interrupted);
}

#[test]
fn append_is_incremental_while_identity_replacement_and_truncate_regrow_are_generations() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "sessions/2026/07/26/changes.jsonl";
    write_session(
        root.path(),
        relative,
        &[
            meta("changes-thread", serde_json::json!("2026-07-26T12:00:00Z")),
            token("2026-07-26T12:00:01Z", 10, 1, 0),
        ],
    );
    let mut codex_scanner = scanner(&root, &state);
    let first = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    let key = first.sources[0].source_key.clone();
    let first_cursor = codex_scanner
        .state()
        .load_current_session_scan_cursor(&key)
        .unwrap()
        .unwrap();

    let mut writer = OpenOptions::new()
        .append(true)
        .open(root.path().join(relative))
        .unwrap();
    serde_json::to_writer(&mut writer, &token("2026-07-26T12:00:02Z", 14, 3, 0)).unwrap();
    writer.write_all(b"\n").unwrap();
    writer.flush().unwrap();
    let appended = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(appended.advanced_sources, 1);
    let append_cursor = codex_scanner
        .state()
        .load_current_session_scan_cursor(&key)
        .unwrap()
        .unwrap();
    assert_eq!(append_cursor.generation, 1);
    assert!(append_cursor.complete_byte_offset > first_cursor.complete_byte_offset);

    let path = root.path().join(relative);
    let moved = path.with_extension("old");
    fs::rename(&path, &moved).unwrap();
    write_session(
        root.path(),
        relative,
        &[
            meta("changes-thread", serde_json::json!("2026-07-26T12:00:00Z")),
            token("2026-07-26T12:00:03Z", 3, 1, 0),
        ],
    );
    codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(
        codex_scanner.state().load_current_generation(&key).unwrap(),
        Some(2)
    );

    write_session(
        root.path(),
        relative,
        &[
            meta("changes-thread", serde_json::json!("2026-07-26T12:00:00Z")),
            token("2026-07-26T12:00:04Z", 6, 2, 0),
            serde_json::json!({"padding":"regrown beyond the old cursor"}),
        ],
    );
    codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(
        codex_scanner.state().load_current_generation(&key).unwrap(),
        Some(3)
    );
}

#[test]
fn large_append_tail_is_streamed_once_during_incremental_processing() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "sessions/2026/07/26/single-read-append.jsonl";
    write_session(
        root.path(),
        relative,
        &[meta(
            "single-read-append",
            serde_json::json!("2026-07-26T12:00:00Z"),
        )],
    );
    let mut codex_scanner = scanner(&root, &state);
    codex_scanner.scan(NOW, ScanControl::default()).unwrap();

    let mut writer = OpenOptions::new()
        .append(true)
        .open(root.path().join(relative))
        .unwrap();
    let line = serde_json::to_vec(&serde_json::json!({
        "future":"x".repeat(8 * 1024)
    }))
    .unwrap();
    let mut appended_bytes = 0u64;
    for _ in 0..256 {
        writer.write_all(&line).unwrap();
        writer.write_all(b"\n").unwrap();
        appended_bytes += (line.len() + 1) as u64;
    }
    writer.flush().unwrap();
    drop(writer);

    let scan = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(scan.advanced_sources, 1);
    assert!(scan.metrics.parser_read_bytes >= appended_bytes);
    assert!(
        scan.metrics.parser_read_bytes <= appended_bytes + 256 * 1024,
        "append bytes={appended_bytes}, parser bytes={}",
        scan.metrics.parser_read_bytes
    );
}

#[test]
fn restored_mtime_same_identity_rewrite_is_detected_by_boundary_fingerprint() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "sessions/2026/07/26/restored-mtime.jsonl";
    let mut lines = vec![meta(
        "mtime-thread",
        serde_json::json!("2026-07-26T12:00:00Z"),
    )];
    for ordinal in 0..24 {
        lines.push(serde_json::json!({
            "future":ordinal,
            "padding":"a".repeat(512)
        }));
    }
    lines.push(token("2026-07-26T12:00:01Z", 10, 1, 0));
    write_session(root.path(), relative, &lines);
    let path = root.path().join(relative);
    let old_modified = fs::metadata(&path).unwrap().modified().unwrap();
    let mut codex_scanner = scanner(&root, &state);
    let first = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    let key = first.sources[0].source_key.clone();
    let mut bytes = fs::read(&path).unwrap();
    assert!(bytes.len() > 8 * 1024);
    let marker = b"\"padding\":\"";
    let position = bytes
        .windows(marker.len())
        .rposition(|window| window == marker)
        .unwrap()
        + marker.len();
    assert!(position > 64);
    assert!(position >= bytes.len() - 2 * 1024);
    bytes[position] = b'b';
    fs::write(&path, bytes).unwrap();
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(old_modified))
        .unwrap();

    codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(
        codex_scanner.state().load_current_generation(&key).unwrap(),
        Some(2)
    );
}

#[test]
fn restored_mtime_incomplete_tail_rewrite_is_detected_across_the_cursor_boundary() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "sessions/2026/07/26/incomplete-tail-rewrite.jsonl";
    write_session(
        root.path(),
        relative,
        &[
            meta(
                "incomplete-tail-rewrite",
                serde_json::json!("2026-07-26T12:00:00Z"),
            ),
            token("2026-07-26T12:00:01Z", 7, 1, 0),
        ],
    );
    let path = root.path().join(relative);
    let incomplete = b"{\"future\":\"aaaaaaaa\"";
    let mut writer = OpenOptions::new().append(true).open(&path).unwrap();
    writer.write_all(incomplete).unwrap();
    writer.flush().unwrap();
    drop(writer);
    let old_modified = fs::metadata(&path).unwrap().modified().unwrap();
    let mut codex_scanner = scanner(&root, &state);
    let first = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    let key = first.sources[0].source_key.clone();
    let first_cursor = codex_scanner
        .state()
        .load_current_session_scan_cursor(&key)
        .unwrap()
        .unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().len() - first_cursor.complete_byte_offset,
        incomplete.len() as u64
    );

    let mut bytes = fs::read(&path).unwrap();
    let tail_start = first_cursor.complete_byte_offset as usize;
    bytes[tail_start..].copy_from_slice(b"{\"future\":\"bbbbbbbb\"");
    fs::write(&path, bytes).unwrap();
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(old_modified))
        .unwrap();

    let rescanned = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(rescanned.unchanged_sources, 0);
    assert_eq!(rescanned.advanced_sources, 1);
    assert_eq!(
        codex_scanner.state().load_current_generation(&key).unwrap(),
        Some(2)
    );
}

#[test]
fn staging_resume_revalidates_fingerprints_after_same_identity_regrow() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "sessions/2026/07/26/staging-aba.jsonl";
    let build = |input| {
        let mut lines = vec![meta(
            "staging-aba",
            serde_json::json!("2026-07-26T12:00:00Z"),
        )];
        for _ in 0..700 {
            lines.push(token("2026-07-26T12:00:01Z", input, 1, 0));
        }
        lines
    };
    write_session(root.path(), relative, &build(10));
    let mut codex_scanner = scanner(&root, &state);
    let interrupted = codex_scanner
        .scan(
            NOW,
            ScanControl {
                stop_after_committed_batches: Some(1),
            },
        )
        .unwrap();
    let key = interrupted.sources[0].source_key.clone();
    let old_size = fs::metadata(root.path().join(relative)).unwrap().len();
    write_session(root.path(), relative, &build(20));
    assert_eq!(
        fs::metadata(root.path().join(relative)).unwrap().len(),
        old_size
    );
    drop(codex_scanner);

    let mut restarted = scanner(&root, &state);
    restarted.scan(NOW, ScanControl::default()).unwrap();
    let usage = restarted
        .state()
        .load_current_session_usage_page(&key, None, 500)
        .unwrap();
    assert_eq!(usage.items.len(), 1);
    assert_eq!(usage.items[0].input_tokens, 20);
    assert_eq!(
        restarted.state().load_current_generation(&key).unwrap(),
        Some(1)
    );
}

#[test]
fn scanner_skips_invalid_complete_records_before_meta_and_bounds_meta_probe() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let path = root.path().join("sessions/2026/07/26/invalid-prefix.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut bytes = b"{bad}\n".to_vec();
    bytes.extend_from_slice(&[0xff, b'\n']);
    for value in [
        meta("invalid-prefix", serde_json::json!("2026-07-26T12:00:00Z")),
        token("2026-07-26T12:00:01Z", 4, 1, 0),
    ] {
        serde_json::to_writer(&mut bytes, &value).unwrap();
        bytes.push(b'\n');
    }
    fs::write(&path, bytes).unwrap();
    let mut codex_scanner = scanner(&root, &state);
    let promoted = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(promoted.advanced_sources, 1);
    assert!(promoted.sources[0].complete_byte_offset > 0);

    let limited_root = TempDir::new().unwrap();
    let limited_state = TempDir::new().unwrap();
    let limited = limited_root
        .path()
        .join("sessions/2026/07/26/meta-too-late.jsonl");
    fs::create_dir_all(limited.parent().unwrap()).unwrap();
    let mut late = b"{}\n".repeat(64);
    serde_json::to_writer(
        &mut late,
        &meta("too-late", serde_json::json!("2026-07-26T12:00:00Z")),
    )
    .unwrap();
    late.push(b'\n');
    fs::write(limited, late).unwrap();
    let mut limited_scanner = scanner(&limited_root, &limited_state);
    let failed = limited_scanner.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(failed.advanced_sources, 0);
    assert_eq!(failed.sources[0].status, SessionSourceStatus::Unavailable);
    assert_eq!(
        limited_scanner
            .state()
            .load_session_source(&failed.sources[0].source_key)
            .unwrap()
            .unwrap()
            .status,
        failed.sources[0].status
    );
}

#[test]
fn legal_large_meta_line_and_large_total_session_remain_bounded() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let path = root.path().join("sessions/2026/07/26/large-meta.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let padded_meta = serde_json::json!({
        "type":"session_meta",
        "timestamp":"2026-07-26T12:00:00Z",
        "payload":{"id":"large-meta","padding":"x".repeat(300 * 1024)}
    });
    let mut bytes = serde_json::to_vec(&padded_meta).unwrap();
    bytes.push(b'\n');
    let filler = serde_json::to_vec(&serde_json::json!({"future":"y".repeat(1024)})).unwrap();
    while bytes.len() <= 17 * 1024 * 1024 {
        bytes.extend_from_slice(&filler);
        bytes.push(b'\n');
    }
    fs::write(path, bytes).unwrap();
    let mut codex_scanner = scanner(&root, &state);
    let result = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(result.advanced_sources, 1);
    assert_eq!(result.metrics.full_source_scans, 1);
    assert!(result.metrics.metadata_probe_bytes <= 512 * 1024);
}

#[test]
fn metadata_probe_reads_a_large_meta_after_short_invalid_complete_records() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let path = root
        .path()
        .join("sessions/2026/07/26/invalid-before-large-meta.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut bytes = b"{bad}\n{\"future\":true}\n".to_vec();
    serde_json::to_writer(
        &mut bytes,
        &serde_json::json!({
            "type":"session_meta",
            "timestamp":"2026-07-26T12:00:00Z",
            "payload":{
                "id":"large-meta-after-invalid",
                "padding":"x".repeat(300 * 1024)
            }
        }),
    )
    .unwrap();
    bytes.push(b'\n');
    serde_json::to_writer(&mut bytes, &token("2026-07-26T12:00:01Z", 7, 2, 0)).unwrap();
    bytes.push(b'\n');
    fs::write(path, bytes).unwrap();

    let mut codex_scanner = scanner(&root, &state);
    let result = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(result.advanced_sources, 1);
    assert_eq!(result.sources[0].status, SessionSourceStatus::Available);
    let usage = codex_scanner
        .state()
        .load_current_session_usage_page(&result.sources[0].source_key, None, 500)
        .unwrap();
    assert_eq!(usage.items.len(), 1);
    assert_eq!(usage.items[0].input_tokens, 7);
    assert!(result.metrics.metadata_probe_bytes < 512 * 1024);
}

#[test]
fn metadata_probe_reports_actual_bounded_read_bytes() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let path = root
        .path()
        .join("sessions/2026/07/26/actual-probe-bytes.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut bytes = serde_json::to_vec(&meta(
        "actual-probe-bytes",
        serde_json::json!("2026-07-26T12:00:00Z"),
    ))
    .unwrap();
    bytes.push(b'\n');
    bytes.extend(std::iter::repeat_n(b'x', 512 * 1024));
    let expected_read_bytes = bytes.len() as u64;
    fs::write(path, bytes).unwrap();

    let mut codex_scanner = scanner(&root, &state);
    let result = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(result.advanced_sources, 1);
    assert_eq!(
        result.metrics.metadata_probe_bytes, expected_read_bytes,
        "the metric must include read-ahead beyond the complete metadata record"
    );
}

#[test]
fn replay_limit_is_an_end_to_end_stable_resource_outcome() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let mut lines = vec![meta(
        "limited-rollout",
        serde_json::json!("2026-07-26T12:00:00Z"),
    )];
    for ordinal in 1..=4 {
        lines.push(token("2026-07-26T12:00:01Z", ordinal, 1, 0));
    }
    write_session(root.path(), "sessions/2026/07/26/limited.jsonl", &lines);
    let mut codex_scanner = scanner(&root, &state);
    codex_scanner.set_test_replay_limit(3);
    let result = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    assert_eq!(
        result.sources[0].status,
        SessionSourceStatus::ResourceLimited
    );
    let source = codex_scanner
        .state()
        .load_session_source(&result.sources[0].source_key)
        .unwrap()
        .unwrap();
    assert_eq!(source.status, SessionSourceStatus::ResourceLimited);
    assert_eq!(
        source.error_code,
        Some(SessionSourceErrorCode::SourceReplayLimit)
    );
    assert!(source.current_generation.is_none());
}

#[test]
fn replay_limit_on_a_current_generation_reports_the_persisted_error_while_stale() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "sessions/2026/07/26/current-replay-limit.jsonl";
    write_session(
        root.path(),
        relative,
        &[
            meta(
                "current-replay-limit",
                serde_json::json!("2026-07-26T12:00:00Z"),
            ),
            token("2026-07-26T12:00:01Z", 1, 1, 0),
            token("2026-07-26T12:00:02Z", 2, 2, 0),
        ],
    );
    let mut codex_scanner = scanner(&root, &state);
    codex_scanner.set_test_replay_limit(3);
    let first = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    let key = first.sources[0].source_key.clone();
    let mut writer = OpenOptions::new()
        .append(true)
        .open(root.path().join(relative))
        .unwrap();
    for ordinal in 3..=4 {
        serde_json::to_writer(
            &mut writer,
            &token("2026-07-26T12:00:03Z", ordinal, ordinal, 0),
        )
        .unwrap();
        writer.write_all(b"\n").unwrap();
    }
    writer.flush().unwrap();
    drop(writer);

    let limited = codex_scanner
        .scan("2026-07-26T12:01:00Z", ScanControl::default())
        .unwrap();
    assert_eq!(limited.sources[0].status, SessionSourceStatus::Stale);
    assert_eq!(
        limited.sources[0].error_code,
        Some(SessionSourceErrorCode::SourceReplayLimit)
    );
    let source = codex_scanner
        .state()
        .load_session_source(&key)
        .unwrap()
        .unwrap();
    assert_eq!(source.status, SessionSourceStatus::Stale);
    assert_eq!(
        source.error_code,
        Some(SessionSourceErrorCode::SourceReplayLimit)
    );
}

#[test]
fn replay_resolution_resource_failure_is_contained_to_one_child_source() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let mut parent = vec![meta(
        "contained-parent",
        serde_json::json!("2026-07-26T12:00:00Z"),
    )];
    for ordinal in 1..=4 {
        parent.push(token(
            &format!("2026-07-26T12:00:0{ordinal}Z"),
            ordinal,
            1,
            0,
        ));
    }
    write_session(root.path(), "sessions/2026/07/26/parent.jsonl", &parent);
    let mut initial = scanner(&root, &state);
    initial.scan(NOW, ScanControl::default()).unwrap();
    drop(initial);

    let mut child_meta = meta("contained-child", serde_json::json!("2026-07-26T12:00:05Z"));
    child_meta["payload"]["parent_thread_id"] = serde_json::json!("contained-parent");
    let mut child = vec![child_meta];
    child.extend(parent.iter().skip(1).cloned());
    child.push(token("2026-07-26T12:00:06Z", 9, 2, 0));
    write_session(root.path(), "sessions/2026/07/26/child.jsonl", &child);
    write_session(
        root.path(),
        "sessions/2026/07/26/sibling.jsonl",
        &[
            meta(
                "contained-sibling",
                serde_json::json!("2026-07-26T12:00:00Z"),
            ),
            token("2026-07-26T12:00:01Z", 3, 1, 0),
        ],
    );

    let mut limited = scanner(&root, &state);
    limited.set_test_replay_limit(3);
    let summary = limited.scan(NOW, ScanControl::default()).unwrap();
    let child = summary
        .sources
        .iter()
        .find(|source| source.root_thread_id.as_deref() == Some("contained-child"))
        .unwrap();
    assert_eq!(child.status, SessionSourceStatus::ResourceLimited);
    assert_eq!(
        child.error_code,
        Some(SessionSourceErrorCode::SourceReplayLimit)
    );
    let sibling = summary
        .sources
        .iter()
        .find(|source| source.root_thread_id.as_deref() == Some("contained-sibling"))
        .unwrap();
    assert_eq!(sibling.status, SessionSourceStatus::Available);
    assert!(
        limited
            .state()
            .load_current_generation(&sibling.source_key)
            .unwrap()
            .is_some()
    );
}

#[test]
fn child_defers_and_keeps_its_current_generation_stale_when_parent_is_nonterminal() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let parent_path = "sessions/2026/07/26/nonterminal-parent.jsonl";
    let parent = vec![
        meta(
            "nonterminal-parent",
            serde_json::json!("2026-07-26T12:00:00Z"),
        ),
        token("2026-07-26T12:00:01Z", 10, 1, 0),
    ];
    write_session(root.path(), parent_path, &parent);
    let mut child_meta = meta(
        "nonterminal-child",
        serde_json::json!("2026-07-26T12:00:02Z"),
    );
    child_meta["payload"]["parent_thread_id"] = serde_json::json!("nonterminal-parent");
    write_session(
        root.path(),
        "sessions/2026/07/26/nonterminal-child.jsonl",
        &[
            child_meta,
            parent[1].clone(),
            token("2026-07-26T12:00:03Z", 15, 2, 0),
        ],
    );
    let mut codex_scanner = scanner(&root, &state);
    let first = codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    let child_key = first
        .sources
        .iter()
        .find(|source| source.root_thread_id.as_deref() == Some("nonterminal-child"))
        .unwrap()
        .source_key
        .clone();

    let mut writer = OpenOptions::new()
        .append(true)
        .open(root.path().join(parent_path))
        .unwrap();
    serde_json::to_writer(
        &mut writer,
        &serde_json::json!({
            "type":"event_msg",
            "timestamp":"2026-07-26T12:00:04Z",
            "payload":{"type":"token_count","info":{"total_token_usage":[]}}
        }),
    )
    .unwrap();
    writer.write_all(b"\n").unwrap();
    writer.flush().unwrap();
    drop(writer);

    let failed = codex_scanner
        .scan("2026-07-26T12:01:00Z", ScanControl::default())
        .unwrap();
    let parent_summary = failed
        .sources
        .iter()
        .find(|source| source.root_thread_id.as_deref() == Some("nonterminal-parent"))
        .unwrap();
    assert_eq!(parent_summary.status, SessionSourceStatus::Stale);
    let child_summary = failed
        .sources
        .iter()
        .find(|source| source.root_thread_id.as_deref() == Some("nonterminal-child"))
        .unwrap();
    assert_eq!(child_summary.status, SessionSourceStatus::Stale);
    assert_eq!(
        child_summary.replay_resolution,
        ReplayResolution::Deferred(SessionSourceErrorCode::SourceReplayInconsistent)
    );
    assert_eq!(
        codex_scanner
            .state()
            .load_current_generation(&child_key)
            .unwrap(),
        Some(1)
    );
    let child_usage = codex_scanner
        .state()
        .load_current_session_usage_page(&child_key, None, 500)
        .unwrap();
    assert_eq!(child_usage.items.len(), 1);
    assert_eq!(
        codex_scanner
            .state()
            .load_session_source(&child_key)
            .unwrap()
            .unwrap()
            .status,
        SessionSourceStatus::Stale
    );
}

#[test]
fn child_with_a_current_generation_becomes_stale_when_parent_disappears() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let parent_path = "sessions/2026/07/26/missing-parent-after-success.jsonl";
    let parent = vec![
        meta(
            "missing-parent-after-success",
            serde_json::json!("2026-07-26T12:00:00Z"),
        ),
        token("2026-07-26T12:00:01Z", 10, 1, 0),
    ];
    write_session(root.path(), parent_path, &parent);
    let mut child_meta = meta(
        "missing-parent-child",
        serde_json::json!("2026-07-26T12:00:02Z"),
    );
    child_meta["payload"]["parent_thread_id"] = serde_json::json!("missing-parent-after-success");
    write_session(
        root.path(),
        "sessions/2026/07/26/missing-parent-child.jsonl",
        &[
            child_meta,
            parent[1].clone(),
            token("2026-07-26T12:00:03Z", 15, 2, 0),
        ],
    );
    let mut codex_scanner = scanner(&root, &state);
    codex_scanner.scan(NOW, ScanControl::default()).unwrap();
    fs::remove_file(root.path().join(parent_path)).unwrap();

    let missing = codex_scanner
        .scan("2026-07-26T12:01:00Z", ScanControl::default())
        .unwrap();
    let child = missing
        .sources
        .iter()
        .find(|source| source.root_thread_id.as_deref() == Some("missing-parent-child"))
        .unwrap();
    assert_eq!(child.status, SessionSourceStatus::Stale);
    assert_eq!(
        child.replay_resolution,
        ReplayResolution::Deferred(SessionSourceErrorCode::SourceReplayParentMissing)
    );
}

#[test]
fn stale_source_state_forces_validation_before_returning_to_available() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_session(
        root.path(),
        "sessions/2026/07/26/retry-stale.jsonl",
        &[
            meta("retry-stale", serde_json::json!("2026-07-26T12:00:00Z")),
            token("2026-07-26T12:00:01Z", 3, 1, 0),
        ],
    );
    let mut initial = scanner(&root, &state);
    let first = initial.scan(NOW, ScanControl::default()).unwrap();
    let source_key = first.sources[0].source_key.clone();
    drop(initial);
    let state_path = state.path().join("state.sqlite3");
    let mut direct = StateStore::open(&state_path).unwrap();
    direct
        .fail_candidate(
            &source_key,
            1,
            SessionSourceErrorCode::SourceParseInvalid,
            "2026-07-26T12:01:00Z",
        )
        .unwrap();
    drop(direct);

    let mut restarted = scanner(&root, &state);
    let retried = restarted
        .scan("2026-07-26T12:02:00Z", ScanControl::default())
        .unwrap();
    assert_eq!(retried.metrics.full_source_scans, 1);
    assert_eq!(retried.sources[0].status, SessionSourceStatus::Available);
    let source = restarted
        .state()
        .load_session_source(&source_key)
        .unwrap()
        .unwrap();
    assert_eq!(source.status, SessionSourceStatus::Available);
    assert_eq!(source.error_code, None);
}
