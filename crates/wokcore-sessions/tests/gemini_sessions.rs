use std::{
    fs::{self, FileTimes, OpenOptions},
    io::Write,
    path::Path,
    time::SystemTime,
};

use tempfile::TempDir;
use wokcore_sessions::{
    gemini::{
        GeminiScanner, MAX_GEMINI_LOGICAL_WORKING_BYTES, MAX_LEGACY_JSON_PARSER_BYTES,
        MAX_LEGACY_JSON_SOURCE_WORK_BYTES,
    },
    model::{SessionScanControl, SessionScanOutcome},
};
use wokcore_storage::SessionSourceStatus;

const NOW: &str = "2026-07-26T12:30:00Z";
const TEST_DOMAIN_KEY: [u8; 32] = [0x47; 32];

fn write_bytes(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn write_legacy_with_exact_size(root: &Path, relative: &str, size: u64) {
    let base = br#"{"sessionId":"work-bound","startTime":"2026-07-26T12:00:00Z","messages":[]}"#;
    assert!(size >= base.len() as u64);
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .unwrap();
    file.write_all(base).unwrap();
    let spaces = [b' '; 64 * 1024];
    let mut remaining = size - base.len() as u64;
    while remaining != 0 {
        let length = usize::try_from(remaining.min(spaces.len() as u64)).unwrap();
        file.write_all(&spaces[..length]).unwrap();
        remaining -= length as u64;
    }
    file.flush().unwrap();
}

fn extend_with_ignored_jsonl_until(path: &Path, target_size: u64) {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    let chunk = b"{}\n".repeat(21_845);
    let mut length = file.metadata().unwrap().len();
    while length < target_size {
        let remaining = usize::try_from((target_size - length).min(chunk.len() as u64)).unwrap();
        file.write_all(&chunk[..remaining]).unwrap();
        length += remaining as u64;
    }
    file.flush().unwrap();
}

fn rewrite_preserving_modified_time(path: &Path, bytes: &[u8]) {
    let modified = fs::metadata(path).unwrap().modified().unwrap();
    fs::write(path, bytes).unwrap();
    OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(modified))
        .unwrap();
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

fn scanner(root: &TempDir, state: &TempDir) -> GeminiScanner {
    GeminiScanner::open(
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap()
}

#[test]
fn mixed_case_migration_and_hardlinks_are_deduplicated_with_current_preferred() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_bytes(
        root.path(),
        "tmp/project-a/chats/session-mixed.JSON",
        include_bytes!("fixtures/gemini/legacy.json"),
    );
    write_bytes(
        root.path(),
        "tmp/project-a/chats/session-mixed.JSONL",
        include_bytes!("fixtures/gemini/current.jsonl"),
    );
    fs::hard_link(
        root.path().join("tmp/project-a/chats/session-mixed.JSONL"),
        root.path()
            .join("tmp/project-a/chats/session-hardlink.jsonl"),
    )
    .unwrap();

    let mut scanner = scanner(&root, &state);
    let summary = scanner.scan(NOW, SessionScanControl::default()).unwrap();

    assert_eq!(summary.sources.len(), 1);
    let index = scanner
        .state()
        .load_current_session_index_page(&summary.sources[0].source_key, None, 1)
        .unwrap()
        .items
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(index.usage_event_count, 2);
}

#[test]
fn rename_preserves_source_generation_and_usage_identity() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let original = "tmp/project-a/chats/session-original.jsonl";
    let renamed = "tmp/project-a/chats/session-renamed.jsonl";
    write_bytes(
        root.path(),
        original,
        include_bytes!("fixtures/gemini/current.jsonl"),
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
fn identity_replacement_and_real_migration_reuse_the_source_atomically() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "tmp/project-a/chats/session-replaced.jsonl";
    write_bytes(
        root.path(),
        relative,
        br#"{"sessionId":"old-current","projectHash":"synthetic","startTime":"2026-07-26T12:00:00Z"}
{"id":"old-answer","timestamp":"2026-07-26T12:00:01Z","type":"gemini","content":"old","model":"gemini-test","tokens":{"input":100,"output":100}}
"#,
    );
    let mut replacement_scanner = scanner(&root, &state);
    let first = replacement_scanner
        .scan(NOW, SessionScanControl::default())
        .unwrap();
    let source_key = first.sources[0].source_key.clone();
    let generation = replacement_scanner
        .state()
        .load_current_generation(&source_key)
        .unwrap()
        .unwrap();
    write_bytes(
        root.path(),
        "replacement.tmp",
        br#"{"sessionId":"new-current","projectHash":"synthetic","startTime":"2026-07-26T12:01:00Z"}
{"id":"new-answer","timestamp":"2026-07-26T12:01:01Z","type":"gemini","content":"new","model":"gemini-test","tokens":{"input":2,"output":3}}
"#,
    );
    fs::remove_file(root.path().join(relative)).unwrap();
    fs::rename(
        root.path().join("replacement.tmp"),
        root.path().join(relative),
    )
    .unwrap();
    let second = replacement_scanner
        .scan("2026-07-26T12:31:00Z", SessionScanControl::default())
        .unwrap();
    assert_eq!(second.sources.len(), 1);
    assert_eq!(second.sources[0].source_key, source_key);
    assert_eq!(second.deleted_sources, 0);
    assert_eq!(
        replacement_scanner
            .state()
            .load_current_generation(&source_key)
            .unwrap(),
        Some(generation + 1)
    );
    let usage = replacement_scanner
        .state()
        .load_current_session_usage_page(&source_key, None, 10)
        .unwrap()
        .items;
    assert_eq!(usage.len(), 1);
    assert_eq!((usage[0].input_tokens, usage[0].output_tokens), (2, 3));

    let migration_root = TempDir::new().unwrap();
    let migration_state = TempDir::new().unwrap();
    let legacy = "tmp/project-a/chats/session-migrate.json";
    let current = "tmp/project-a/chats/session-migrate.jsonl";
    write_bytes(
        migration_root.path(),
        legacy,
        include_bytes!("fixtures/gemini/legacy.json"),
    );
    let mut migration_scanner = scanner(&migration_root, &migration_state);
    let before = migration_scanner
        .scan(NOW, SessionScanControl::default())
        .unwrap();
    let migration_key = before.sources[0].source_key.clone();
    let migration_generation = migration_scanner
        .state()
        .load_current_generation(&migration_key)
        .unwrap()
        .unwrap();
    fs::remove_file(migration_root.path().join(legacy)).unwrap();
    write_bytes(
        migration_root.path(),
        current,
        include_bytes!("fixtures/gemini/current.jsonl"),
    );
    let after = migration_scanner
        .scan("2026-07-26T12:31:00Z", SessionScanControl::default())
        .unwrap();
    assert_eq!(after.sources.len(), 1);
    assert_eq!(after.sources[0].source_key, migration_key);
    assert_eq!(after.deleted_sources, 0);
    assert_eq!(
        migration_scanner
            .state()
            .load_current_generation(&migration_key)
            .unwrap(),
        Some(migration_generation + 1)
    );
    let migrated_usage = migration_scanner
        .state()
        .load_current_session_usage_page(&migration_key, None, 10)
        .unwrap()
        .items;
    assert_eq!(migrated_usage.len(), 2);
    assert!(migrated_usage.iter().all(|usage| usage.input_tokens < 100));
    let global = migration_scanner
        .state()
        .load_global_current_session_usage_page(None, 500)
        .unwrap()
        .items;
    assert_eq!(global.len(), 2);
    assert!(global.iter().all(|usage| usage.input_tokens < 100));
}

#[test]
fn whole_document_fingerprint_detects_same_length_current_and_legacy_rewrites() {
    let prefix = "p".repeat(5 * 1024);
    let suffix = "s".repeat(5 * 1024);
    let cases = vec![
        (
            "tmp/project-a/chats/session-fingerprint.jsonl",
            format!(
                "{{\"sessionId\":\"current-fingerprint\",\"projectHash\":\"synthetic\",\"startTime\":\"2026-07-26T12:00:00Z\",\"prefix\":\"{prefix}\"}}\n\
                 {{\"id\":\"answer\",\"timestamp\":\"2026-07-26T12:00:02Z\",\"type\":\"gemini\",\"content\":\"answer\",\"model\":\"gemini-test\",\"tokens\":{{\"input\":1,\"output\":2}}}}\n\
                 {{\"ignored\":\"{suffix}\"}}\n"
            )
            .into_bytes(),
            b"\"input\":1".as_slice(),
            b"\"input\":9".as_slice(),
        ),
        (
            "tmp/project-a/chats/session-fingerprint.json",
            format!(
                "{{\"sessionId\":\"legacy-fingerprint\",\"projectHash\":\"synthetic\",\"startTime\":\"2026-07-26T12:00:00Z\",\"prefix\":\"{prefix}\",\
                 \"messages\":[{{\"id\":\"answer\",\"timestamp\":\"2026-07-26T12:00:02Z\",\"type\":\"gemini\",\"content\":\"answer\",\"model\":\"gemini-test\",\"tokens\":{{\"input\":2,\"output\":3}}}}],\
                 \"suffix\":\"{suffix}\"}}"
            )
            .into_bytes(),
            b"\"input\":2".as_slice(),
            b"\"input\":8".as_slice(),
        ),
    ];
    for (relative, initial, old_input, new_input) in cases {
        let root = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        assert!(initial.len() > 8 * 1024);
        write_bytes(root.path(), relative, &initial);
        let mut scanner = scanner(&root, &state);
        let first = scanner.scan(NOW, SessionScanControl::default()).unwrap();
        let source_key = first.sources[0].source_key.clone();
        let generation = scanner
            .state()
            .load_current_generation(&source_key)
            .unwrap()
            .unwrap();
        let cursor = scanner
            .state()
            .load_current_session_scan_cursor(&source_key)
            .unwrap()
            .unwrap();
        assert!(cursor.parser_checkpoint.structural_hash.is_some());
        let mut rewritten = initial.clone();
        let offset = rewritten
            .windows(old_input.len())
            .position(|window| window == old_input)
            .unwrap();
        rewritten[offset..offset + old_input.len()].copy_from_slice(new_input);
        assert_eq!(rewritten.len(), initial.len());
        assert!(offset >= 64);
        assert!(offset + old_input.len() <= initial.len() - 4 * 1024);
        assert_eq!(&rewritten[..64], &initial[..64]);
        assert_eq!(
            &rewritten[rewritten.len() - 4 * 1024..],
            &initial[initial.len() - 4 * 1024..]
        );
        assert_ne!(rewritten, initial);
        rewrite_preserving_modified_time(&root.path().join(relative), &rewritten);

        let second = scanner
            .scan("2026-07-26T12:31:00Z", SessionScanControl::default())
            .unwrap();
        assert_eq!(second.advanced_sources, 1);
        assert_eq!(
            scanner
                .state()
                .load_current_generation(&source_key)
                .unwrap(),
            Some(generation + 1)
        );
        let usage = scanner
            .state()
            .load_global_current_session_usage_page(None, 10)
            .unwrap()
            .items;
        assert_eq!(usage.len(), 1);
        assert_eq!(
            usage[0].input_tokens,
            if relative.ends_with("jsonl") { 9 } else { 8 }
        );
    }
}

#[test]
fn oversized_tokens_and_changed_current_work_are_isolated_from_siblings() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_bytes(
        root.path(),
        "tmp/project-a/chats/session-malicious.jsonl",
        br#"{"sessionId":"malicious","projectHash":"synthetic","startTime":"2026-07-26T12:00:00Z"}
{"id":"bad-answer","timestamp":"2026-07-26T12:00:01Z","type":"gemini","content":"bad","model":"gemini-test","tokens":{"input":9223372036854775808,"output":1}}
"#,
    );
    write_bytes(
        root.path(),
        "tmp/project-a/chats/session-sibling.jsonl",
        br#"{"sessionId":"sibling","projectHash":"synthetic","startTime":"2026-07-26T12:00:00Z"}
{"id":"sibling-answer","timestamp":"2026-07-26T12:00:01Z","type":"gemini","content":"sibling","model":"gemini-test","tokens":{"input":1,"output":1}}
"#,
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
        let document = format!(
            "{{\"sessionId\":\"{name}\",\"projectHash\":\"synthetic\",\"startTime\":\"2026-07-26T12:00:00Z\"}}\n\
             {{\"id\":\"{name}-answer\",\"timestamp\":\"2026-07-26T12:00:01Z\",\"type\":\"gemini\",\
             \"content\":\"answer\",\"model\":\"gemini-test\",\"tokens\":{{\"input\":1,\"output\":1}}}}\n"
        );
        write_bytes(
            bounded_root.path(),
            &format!("tmp/project-a/chats/session-{name}.jsonl"),
            document.as_bytes(),
        );
    }
    let mut bounded_scanner = scanner(&bounded_root, &bounded_state);
    bounded_scanner
        .scan(NOW, SessionScanControl::default())
        .unwrap();
    extend_with_ignored_jsonl_until(
        &bounded_root
            .path()
            .join("tmp/project-a/chats/session-heavy.jsonl"),
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
fn partial_later_token_snapshot_preserves_last_valid_usage_across_batches_and_restart() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "tmp/project-a/chats/session-partial.jsonl";
    write_bytes(
        root.path(),
        relative,
        br#"{"sessionId":"gemini-partial","projectHash":"synthetic","startTime":"2026-07-26T12:00:00Z"}
{"id":"answer","timestamp":"2026-07-26T12:00:01Z","type":"gemini","content":"valid","model":"gemini-test","tokens":{"input":7,"output":3,"cached":2,"total":12}}
"#,
    );
    let mut initial = scanner(&root, &state);
    let source_key = initial
        .scan(NOW, SessionScanControl::default())
        .unwrap()
        .sources[0]
        .source_key
        .clone();
    let mut writer = OpenOptions::new()
        .append(true)
        .open(root.path().join(relative))
        .unwrap();
    for _ in 0..520 {
        writer.write_all(b"{}\n").unwrap();
    }
    writer
        .write_all(
            br#"{"id":"answer","timestamp":"2026-07-26T12:01:00Z","type":"gemini","content":"partial","model":"gemini-test","tokens":{"input":"invalid","output":9}}
"#,
        )
        .unwrap();
    writer.flush().unwrap();
    drop(writer);
    initial
        .scan("2026-07-26T12:31:00Z", SessionScanControl::default())
        .unwrap();
    let usage = initial
        .state()
        .load_current_session_usage_page(&source_key, None, 500)
        .unwrap()
        .items;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].input_tokens, 7);
    assert_eq!(usage[0].output_tokens, 3);
    drop(initial);

    let mut restarted = scanner(&root, &state);
    let unchanged = restarted
        .scan("2026-07-26T12:32:00Z", SessionScanControl::default())
        .unwrap();
    assert_eq!(unchanged.unchanged_sources, 1);
    assert_eq!(
        restarted
            .state()
            .load_current_session_usage_page(&source_key, None, 500)
            .unwrap()
            .items[0]
            .input_tokens,
        7
    );
}

#[test]
fn completed_cursor_covers_trailing_non_usage_records() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "tmp/project-a/chats/session-trailing.jsonl";
    write_bytes(
        root.path(),
        relative,
        br#"{"sessionId":"gemini-trailing","projectHash":"synthetic","startTime":"2026-07-26T12:00:00Z"}
{"id":"answer-1","timestamp":"2026-07-26T12:00:01Z","type":"gemini","content":"answer","model":"gemini-test","tokens":{"input":1,"output":1,"total":2}}
{"timestamp":"2026-07-26T12:00:02Z","type":"metadata","note":"trailing"}
"#,
    );

    let mut scanner = scanner(&root, &state);
    let summary = scanner.scan(NOW, SessionScanControl::default()).unwrap();
    let cursor = scanner
        .state()
        .load_current_session_scan_cursor(&summary.sources[0].source_key)
        .unwrap()
        .unwrap();

    assert_eq!(cursor.stable_record_ordinal, 3 << 16);
    assert_eq!(
        cursor.complete_byte_offset,
        fs::metadata(root.path().join(relative)).unwrap().len()
    );
}

#[test]
fn current_incomplete_tail_is_excluded_until_the_record_is_completed() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "tmp/project-a/chats/session-incomplete-tail.jsonl";
    write_bytes(
        root.path(),
        relative,
        br#"{"sessionId":"gemini-incomplete","projectHash":"synthetic","startTime":"2026-07-26T12:00:00Z"}
{"id":"answer-1","timestamp":"2026-07-26T12:00:01Z","type":"gemini","content":"first","model":"gemini-test","tokens":{"input":1,"output":2}}
{"id":"answer-2","timestamp":"2026-07-26T12:00:02Z","type":"gemini","content":"second","model":"gemini-test","tokens":{"input":7"#,
    );
    let mut scanner = scanner(&root, &state);
    let first = scanner.scan(NOW, SessionScanControl::default()).unwrap();
    let source_key = first.sources[0].source_key.clone();
    let generation = scanner
        .state()
        .load_current_generation(&source_key)
        .unwrap()
        .unwrap();
    let first_cursor = scanner
        .state()
        .load_current_session_scan_cursor(&source_key)
        .unwrap()
        .unwrap();
    assert!(first_cursor.complete_byte_offset < first_cursor.observed_size);
    assert!(first_cursor.parser_checkpoint.structural_hash.is_some());
    assert_eq!(
        scanner
            .state()
            .load_current_session_usage_page(&source_key, None, 10)
            .unwrap()
            .items
            .len(),
        1
    );

    let mut writer = OpenOptions::new()
        .append(true)
        .open(root.path().join(relative))
        .unwrap();
    writer.write_all(b",\"output\":3}}\n").unwrap();
    writer.flush().unwrap();
    drop(writer);

    let completed = scanner
        .scan("2026-07-26T12:31:00Z", SessionScanControl::default())
        .unwrap();
    assert_eq!(completed.advanced_sources, 1);
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
    assert_eq!(usage.len(), 2);
    assert!(usage.iter().any(|record| record.input_tokens == 7));
    let cursor = scanner
        .state()
        .load_current_session_scan_cursor(&source_key)
        .unwrap()
        .unwrap();
    assert_eq!(cursor.complete_byte_offset, cursor.observed_size);
}

#[test]
fn legacy_current_migration_pair_is_deduplicated_and_jsonl_semantics_are_rebuilt() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_bytes(
        root.path(),
        "tmp/project-a/chats/session-migrated.json",
        include_bytes!("fixtures/gemini/legacy.json"),
    );
    write_bytes(
        root.path(),
        "tmp/project-a/chats/session-migrated.jsonl",
        include_bytes!("fixtures/gemini/current.jsonl"),
    );
    write_bytes(
        root.path(),
        "tmp/project-a/chats/session-legacy-only.json",
        include_bytes!("fixtures/gemini/legacy.json"),
    );
    write_bytes(
        root.path(),
        "tmp/project-a/chats/parent-session/subagent.jsonl",
        br#"{"sessionId":"gemini-subagent","projectHash":"synthetic","startTime":"2026-07-26T12:00:00Z","kind":"subagent"}
{"id":"subagent-user","timestamp":"2026-07-26T12:00:01Z","type":"user","content":"question"}
{"id":"subagent-answer","timestamp":"2026-07-26T12:00:02Z","type":"gemini","content":"answer","model":"gemini-test","tokens":{"input":1,"output":1,"cached":0,"total":2}}
"#,
    );

    let before = snapshot_tree(root.path());
    let mut scanner = scanner(&root, &state);
    let summary = scanner.scan(NOW, SessionScanControl::default()).unwrap();
    assert_eq!(summary.outcome, SessionScanOutcome::Complete);
    assert_eq!(summary.sources.len(), 3);
    assert_eq!(summary.advanced_sources, 3);
    assert_eq!(snapshot_tree(root.path()), before);

    let current = summary
        .sources
        .iter()
        .find(|source| {
            scanner
                .state()
                .load_current_session_usage_page(&source.source_key, None, 10)
                .unwrap()
                .items
                .len()
                == 2
        })
        .unwrap();
    let index = scanner
        .state()
        .load_current_session_index_page(&current.source_key, None, 1)
        .unwrap()
        .items
        .pop()
        .unwrap();
    assert_eq!(index.message_count, 3);
    assert_eq!(index.usage_event_count, 2);
    let usage = scanner
        .state()
        .load_current_session_usage_page(&current.source_key, None, 10)
        .unwrap()
        .items;
    assert_eq!(usage[0].output_tokens, 9);
    assert_eq!(usage[0].cache_read_tokens, 48_719);
    assert_eq!(usage[0].reasoning_tokens, 3);
    assert_eq!(usage[1].input_tokens, 0);
    assert_eq!(usage[1].cache_read_tokens, 7);
    assert_eq!(usage[1].cache_write_tokens, 0);
    assert_eq!(
        usage[1].reasoning_tokens, 0,
        "Gemini tool tokens are already represented by upstream totals and are not added to input"
    );
    assert!(
        summary.metrics.peak_parser_buffer_bytes <= MAX_LEGACY_JSON_PARSER_BYTES,
        "legacy JSON parsing must expose its measured fixed peak"
    );
}

#[test]
fn changed_jsonl_revision_promotes_a_new_generation_and_removes_rewound_usage() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "tmp/project-a/chats/session-current.jsonl";
    write_bytes(
        root.path(),
        relative,
        include_bytes!("fixtures/gemini/current.jsonl"),
    );
    let mut scanner = scanner(&root, &state);
    let first = scanner.scan(NOW, SessionScanControl::default()).unwrap();
    let source_key = first.sources[0].source_key.clone();
    let old_generation = scanner
        .state()
        .load_current_generation(&source_key)
        .unwrap()
        .unwrap();

    let mut changed = include_bytes!("fixtures/gemini/current.jsonl").to_vec();
    changed.extend_from_slice(b"{\"$rewindTo\":\"missing-message\"}\n");
    fs::write(root.path().join(relative), changed).unwrap();
    let second = scanner
        .scan("2026-07-26T12:31:00Z", SessionScanControl::default())
        .unwrap();
    assert_eq!(second.sources[0].status, SessionSourceStatus::Available);
    assert_eq!(
        scanner
            .state()
            .load_current_generation(&source_key)
            .unwrap(),
        Some(old_generation + 1)
    );
    assert!(
        scanner
            .state()
            .load_current_session_usage_page(&source_key, None, 10)
            .unwrap()
            .items
            .is_empty()
    );
}

#[test]
fn unchanged_restart_is_write_free() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_bytes(
        root.path(),
        "tmp/project-a/chats/session-current.jsonl",
        include_bytes!("fixtures/gemini/current.jsonl"),
    );
    let mut initial = scanner(&root, &state);
    initial.scan(NOW, SessionScanControl::default()).unwrap();
    drop(initial);

    let mut restarted = scanner(&root, &state);
    let before = snapshot_tree(state.path());
    let unchanged = restarted
        .scan("2026-07-26T12:31:00Z", SessionScanControl::default())
        .unwrap();
    assert_eq!(unchanged.unchanged_sources, 1);
    assert_eq!(unchanged.metrics.full_source_scans, 0);
    assert_eq!(snapshot_tree(state.path()), before);
}

#[test]
fn current_title_uses_the_final_checkpoint_and_rewind_state() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_bytes(
        root.path(),
        "tmp/project-a/chats/session-title-state.jsonl",
        br#"{"sessionId":"title-state","projectHash":"synthetic","startTime":"2026-07-26T12:00:00Z","messages":[{"id":"header-old","timestamp":"2026-07-26T12:00:00Z","type":"user","content":"HEADER-OLD-MUST-NOT-WIN"}]}
{"$set":{"messages":[{"id":"checkpoint-old","timestamp":"2026-07-26T12:00:01Z","type":"user","content":"CHECKPOINT-OLD-MUST-NOT-WIN"}]}}
{"$rewindTo":"checkpoint-old"}
{"id":"survivor","timestamp":"2026-07-26T12:00:02Z","type":"user","content":"surviving prompt"}
"#,
    );
    let mut state_scanner = scanner(&root, &state);
    let summary = state_scanner
        .scan(NOW, SessionScanControl::default())
        .unwrap();
    let title = state_scanner
        .title_for_source(&summary.sources[0].source_key)
        .unwrap()
        .unwrap();
    assert_eq!(title.as_str(), "surviving prompt");

    let summary_root = TempDir::new().unwrap();
    let summary_state = TempDir::new().unwrap();
    write_bytes(
        summary_root.path(),
        "tmp/project-a/chats/session-title-summary.jsonl",
        br#"{"sessionId":"title-summary","projectHash":"synthetic","startTime":"2026-07-26T12:00:00Z","messages":[{"id":"header","timestamp":"2026-07-26T12:00:00Z","type":"user","content":"header prompt"}]}
{"$set":{"messages":[{"id":"checkpoint","timestamp":"2026-07-26T12:00:01Z","type":"user","content":"checkpoint prompt"}],"summary":"final checkpoint summary"}}
"#,
    );
    let mut summary_scanner = scanner(&summary_root, &summary_state);
    let scanned = summary_scanner
        .scan(NOW, SessionScanControl::default())
        .unwrap();
    let title = summary_scanner
        .title_for_source(&scanned.sources[0].source_key)
        .unwrap()
        .unwrap();
    assert_eq!(title.as_str(), "final checkpoint summary");
}

#[test]
fn current_main_name_and_metadata_gate_are_source_local() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let valid =
        br#"{"sessionId":"valid-main","projectHash":"synthetic","startTime":"2026-07-26T12:00:00Z"}
{"id":"valid-user","timestamp":"2026-07-26T12:00:01Z","type":"user","content":"valid"}
"#;
    write_bytes(
        root.path(),
        "tmp/project-a/chats/not-a-main-session.jsonl",
        valid,
    );
    write_bytes(
        root.path(),
        "tmp/project-a/chats/session-valid.jsonl",
        valid,
    );
    write_bytes(
        root.path(),
        "tmp/project-a/chats/parent/subagent-id.jsonl",
        br#"{"sessionId":"valid-subagent","projectHash":"synthetic","startTime":"2026-07-26T12:00:00Z"}
{"id":"subagent-user","timestamp":"2026-07-26T12:00:01Z","type":"user","content":"valid subagent"}
"#,
    );
    write_bytes(
        root.path(),
        "tmp/project-a/chats/session-missing-project.jsonl",
        br#"{"sessionId":"missing-project","startTime":"2026-07-26T12:00:00Z"}
{"id":"invalid-user","timestamp":"2026-07-26T12:00:01Z","type":"user","content":"invalid"}
"#,
    );
    let mut scanner = scanner(&root, &state);
    let summary = scanner.scan(NOW, SessionScanControl::default()).unwrap();
    assert_eq!(
        summary.sources.len(),
        3,
        "non-session main JSONL must not be discovered"
    );
    assert_eq!(
        summary
            .sources
            .iter()
            .filter(|source| source.status == SessionSourceStatus::Available)
            .count(),
        2,
        "a valid main and arbitrary-named subagent remain available"
    );
    assert_eq!(
        summary
            .sources
            .iter()
            .filter(|source| source.error_code
                == Some(wokcore_storage::SessionSourceErrorCode::SourceParseInvalid))
            .count(),
        1,
        "a malformed metadata header is isolated to its source"
    );
}

#[test]
fn current_checkpoint_non_array_last_duplicate_is_source_local_and_has_no_title_fallback() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_bytes(
        root.path(),
        "tmp/project-a/chats/session-valid-control.jsonl",
        br#"{"sessionId":"valid-control","projectHash":"synthetic","startTime":"2026-07-26T12:00:00Z"}
{"$set":{"messages":null,"m\u0065ssages":[{"id":"valid","timestamp":"2026-07-26T12:00:01Z","type":"user","content":"final valid"}]}}
"#,
    );
    write_bytes(
        root.path(),
        "tmp/project-a/chats/session-invalid-control.jsonl",
        br#"{"sessionId":"invalid-control","projectHash":"synthetic","startTime":"2026-07-26T12:00:00Z","messages":[{"id":"old","timestamp":"2026-07-26T12:00:00Z","type":"user","content":"OLD TITLE MUST NOT RETURN"}]}
{"$set":{"messages":[{"id":"valid","timestamp":"2026-07-26T12:00:01Z","type":"user","content":"must not escape"}],"m\u0065ssages":null}}
"#,
    );
    let mut state_scanner = scanner(&root, &state);
    let summary = state_scanner
        .scan(NOW, SessionScanControl::default())
        .unwrap();
    let valid = summary
        .sources
        .iter()
        .find(|source| source.status == SessionSourceStatus::Available)
        .unwrap();
    let invalid = summary
        .sources
        .iter()
        .find(|source| {
            source.error_code == Some(wokcore_storage::SessionSourceErrorCode::SourceParseInvalid)
        })
        .unwrap();

    assert_eq!(
        state_scanner
            .state()
            .load_current_generation(&invalid.source_key)
            .unwrap(),
        None
    );
    assert!(
        state_scanner
            .title_for_source(&invalid.source_key)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        state_scanner
            .state()
            .load_current_session_usage_page(&valid.source_key, None, 10)
            .unwrap()
            .items
            .len(),
        0
    );

    let rewrite_root = TempDir::new().unwrap();
    let rewrite_state = TempDir::new().unwrap();
    let relative = "tmp/project-a/chats/session-invalid-title-rewrite.jsonl";
    let initial_control = r#"{"$set":{"messages":[{"id":"checkpoint","timestamp":"2026-07-26T12:00:01Z","type":"user","content":"initial title"}]}}"#;
    let invalid_control = r#"{"$set":{"messages":[{"id":"checkpoint","timestamp":"2026-07-26T12:00:01Z","type":"user","content":"OLD TITLE MUST NOT RETURN"}],"m\u0065ssages":null}}"#;
    let control_width = initial_control.len().max(invalid_control.len());
    let prefix_padding = "p".repeat(10 * 1024);
    let suffix_padding = "s".repeat(10 * 1024);
    let build_document = |control: &str| {
        format!(
            "{{\"sessionId\":\"invalid-title-rewrite\",\"projectHash\":\"synthetic\",\
             \"startTime\":\"2026-07-26T12:00:00Z\"}}\n\
             {{\"padding\":\"{prefix_padding}\"}}\n\
             {control:control_width$}\n\
             {{\"padding\":\"{suffix_padding}\"}}\n"
        )
    };
    let initial_document = build_document(initial_control);
    let invalid_document = build_document(invalid_control);
    assert_eq!(initial_document.len(), invalid_document.len());
    write_bytes(rewrite_root.path(), relative, initial_document.as_bytes());
    let mut rewrite_scanner = scanner(&rewrite_root, &rewrite_state);
    let initial = rewrite_scanner
        .scan(NOW, SessionScanControl::default())
        .unwrap();
    let source_key = initial.sources[0].source_key.clone();
    let generation = rewrite_scanner
        .state()
        .load_current_generation(&source_key)
        .unwrap();
    rewrite_preserving_modified_time(
        &rewrite_root.path().join(relative),
        invalid_document.as_bytes(),
    );
    let failed = rewrite_scanner
        .scan("2026-07-26T12:31:00Z", SessionScanControl::default())
        .unwrap();
    assert_eq!(
        failed.sources[0].error_code,
        Some(wokcore_storage::SessionSourceErrorCode::SourceParseInvalid)
    );
    assert_eq!(
        rewrite_scanner
            .state()
            .load_current_generation(&source_key)
            .unwrap(),
        generation,
        "a source-local failure must preserve the old committed aggregate"
    );
    assert!(
        rewrite_scanner
            .title_for_source(&source_key)
            .unwrap()
            .is_none(),
        "title lookup must fail closed instead of returning an old checkpoint title"
    );
}

#[test]
fn legacy_messages_duplicate_uses_last_value_and_non_array_last_is_source_local() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_bytes(
        root.path(),
        "tmp/project-a/chats/session-legacy-valid.json",
        br#"{"sessionId":"legacy-valid","startTime":"2026-07-26T12:00:00Z","messages":null,"m\u0065ssages":[{"id":"valid","timestamp":"2026-07-26T12:00:01Z","type":"gemini","content":"valid","model":"gemini-test","tokens":{"input":1,"output":2}}]}"#,
    );
    write_bytes(
        root.path(),
        "tmp/project-a/chats/session-legacy-invalid.json",
        br#"{"sessionId":"legacy-invalid","startTime":"2026-07-26T12:00:00Z","messages":[{"id":"old","timestamp":"2026-07-26T12:00:01Z","type":"gemini","content":"old","model":"gemini-test","tokens":{"input":1,"output":2}}],"m\u0065ssages":null}"#,
    );
    let mut scanner = scanner(&root, &state);
    let summary = scanner.scan(NOW, SessionScanControl::default()).unwrap();
    let valid = summary
        .sources
        .iter()
        .find(|source| source.status == SessionSourceStatus::Available)
        .unwrap();
    let invalid = summary
        .sources
        .iter()
        .find(|source| {
            source.error_code == Some(wokcore_storage::SessionSourceErrorCode::SourceParseInvalid)
        })
        .unwrap();

    let usage = scanner
        .state()
        .load_current_session_usage_page(&valid.source_key, None, 10)
        .unwrap()
        .items;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].output_tokens, 2);
    assert_eq!(
        scanner
            .state()
            .load_current_generation(&invalid.source_key)
            .unwrap(),
        None
    );
}

#[test]
fn legacy_title_streams_beyond_the_parser_buffer_without_persisting_body_text() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let padding = " ".repeat(MAX_LEGACY_JSON_PARSER_BYTES + 1);
    let document = format!(
        "{{\"sessionId\":\"large-title\",\"startTime\":\"2026-07-26T12:00:00Z\",\
         \"messages\":[{{\"id\":\"user\",\"timestamp\":\"2026-07-26T12:00:01Z\",\
         \"type\":\"user\",\"content\":\"first user fallback\"}}],\
         {padding}\"summary\":\"显式摘要 🌏\"}}"
    );
    assert!(document.len() > MAX_LEGACY_JSON_PARSER_BYTES);
    write_bytes(
        root.path(),
        "tmp/project-a/chats/session-large-title.json",
        document.as_bytes(),
    );
    let mut scanner = scanner(&root, &state);
    let summary = scanner.scan(NOW, SessionScanControl::default()).unwrap();
    let source_key = &summary.sources[0].source_key;
    let title = scanner
        .title_for_source(source_key)
        .unwrap()
        .expect("explicit title");
    assert_eq!(title.as_str(), "显式摘要 🌏");
    assert!(
        snapshot_tree(state.path())
            .iter()
            .all(|(_, _, _, bytes)| !String::from_utf8_lossy(bytes).contains("first user fallback"))
    );
}

#[test]
fn legacy_untrusted_strings_and_logical_working_set_are_source_local_bounded() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let long_session_id = "s".repeat(513);
    let invalid = format!(
        "{{\"sessionId\":\"{long_session_id}\",\"startTime\":\"2026-07-26T12:00:00Z\",\
         \"messages\":[]}}"
    );
    write_bytes(
        root.path(),
        "tmp/project-a/chats/session-a-invalid.json",
        invalid.as_bytes(),
    );

    let mut messages = String::new();
    for index in 0..1_200 {
        let id = format!("{index:04}-{}", "x".repeat(500));
        messages.push_str(&format!(
            "{{\"id\":\"{id}\",\"timestamp\":\"2026-07-26T12:00:01Z\",\
             \"type\":\"user\",\"content\":\"ignored\"}},"
        ));
    }
    let over_budget = format!(
        "{{\"sessionId\":\"over-budget\",\"startTime\":\"2026-07-26T12:00:00Z\",\
         \"messages\":[{messages}\
         {{\"id\":\"last\",\"timestamp\":\"2026-07-26T12:00:02Z\",\
         \"type\":\"user\",\"content\":\"ignored\"}}]}}"
    );
    assert!(over_budget.len() > MAX_GEMINI_LOGICAL_WORKING_BYTES);
    write_bytes(
        root.path(),
        "tmp/project-a/chats/session-b-over-budget.json",
        over_budget.as_bytes(),
    );
    write_bytes(
        root.path(),
        "tmp/project-a/chats/session-z-valid.json",
        include_bytes!("fixtures/gemini/legacy.json"),
    );

    let mut scanner = scanner(&root, &state);
    let summary = scanner.scan(NOW, SessionScanControl::default()).unwrap();
    assert_eq!(summary.sources.len(), 3);
    assert_eq!(
        summary
            .sources
            .iter()
            .filter(|source| source.status == SessionSourceStatus::ResourceLimited)
            .count(),
        2
    );
    assert_eq!(
        summary
            .sources
            .iter()
            .filter(|source| source.status == SessionSourceStatus::Available)
            .count(),
        1,
        "a resource-limited legacy source must not block its sibling"
    );
    assert!(
        summary.metrics.peak_parser_buffer_bytes <= MAX_GEMINI_LOGICAL_WORKING_BYTES,
        "successful siblings expose a fixed low-hundreds-KiB working-set ceiling"
    );
}

#[test]
fn legacy_source_work_limit_accepts_exact_boundary_and_rejects_one_over() {
    let exact_root = TempDir::new().unwrap();
    let exact_state = TempDir::new().unwrap();
    write_legacy_with_exact_size(
        exact_root.path(),
        "tmp/project-a/chats/session-exact.json",
        MAX_LEGACY_JSON_SOURCE_WORK_BYTES,
    );
    let mut exact = scanner(&exact_root, &exact_state);
    let exact_summary = exact.scan(NOW, SessionScanControl::default()).unwrap();
    assert_eq!(
        exact_summary.sources[0].status,
        SessionSourceStatus::Available
    );
    assert_eq!(
        exact_summary.metrics.parser_read_bytes,
        MAX_LEGACY_JSON_SOURCE_WORK_BYTES
    );

    let over_root = TempDir::new().unwrap();
    let over_state = TempDir::new().unwrap();
    write_legacy_with_exact_size(
        over_root.path(),
        "tmp/project-a/chats/session-over.json",
        MAX_LEGACY_JSON_SOURCE_WORK_BYTES + 1,
    );
    let mut over = scanner(&over_root, &over_state);
    let over_summary = over.scan(NOW, SessionScanControl::default()).unwrap();
    assert_eq!(
        over_summary.sources[0].status,
        SessionSourceStatus::ResourceLimited
    );
}

#[test]
fn legacy_global_string_bound_accepts_exact_boundary_and_rejects_one_over() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    for (name, length) in [("exact", 64 * 1024), ("over", 64 * 1024 + 1)] {
        let padding = "x".repeat(length);
        let document = format!(
            "{{\"sessionId\":\"string-{name}\",\"startTime\":\"2026-07-26T12:00:00Z\",\
             \"messages\":[],\"ignored\":\"{padding}\"}}"
        );
        write_bytes(
            root.path(),
            &format!("tmp/project-a/chats/session-{name}.json"),
            document.as_bytes(),
        );
    }
    let mut scanner = scanner(&root, &state);
    let summary = scanner.scan(NOW, SessionScanControl::default()).unwrap();
    assert_eq!(
        summary
            .sources
            .iter()
            .filter(|source| source.status == SessionSourceStatus::Available)
            .count(),
        1
    );
    assert_eq!(
        summary
            .sources
            .iter()
            .filter(|source| source.status == SessionSourceStatus::ResourceLimited)
            .count(),
        1
    );
}

#[test]
fn interrupted_multibatch_candidate_is_invisible_and_resumes_after_restart() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let mut lines = String::from(
        "{\"sessionId\":\"gemini-batched\",\"projectHash\":\"synthetic\",\
         \"startTime\":\"2026-07-26T12:00:00Z\"}\n",
    );
    for index in 0..500 {
        lines.push_str(&format!(
            "{{\"id\":\"answer-{index}\",\"timestamp\":\"2026-07-26T12:00:01Z\",\
             \"type\":\"gemini\",\"content\":\"answer\",\"model\":\"gemini-test\",\
             \"tokens\":{{\"input\":1,\"output\":1,\"cached\":0,\"thoughts\":0,\"tool\":0}}}}\n"
        ));
    }
    lines.push_str(
        "{\"id\":\"answer-0\",\"timestamp\":\"2026-07-26T12:00:02Z\",\
         \"type\":\"gemini\",\"content\":\"replacement\",\"model\":\"gemini-test\",\
         \"tokens\":{\"input\":1,\"output\":2,\"cached\":0,\"thoughts\":0,\"tool\":0}}\n",
    );
    write_bytes(
        root.path(),
        "tmp/project-a/chats/session-batched.jsonl",
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

    let initial_bytes = lines.as_bytes();
    let mut rewritten = initial_bytes.to_vec();
    let old = b"\"input\":1";
    let offset = rewritten
        .windows(old.len())
        .enumerate()
        .filter_map(|(index, window)| (window == old).then_some(index))
        .nth(1)
        .unwrap();
    rewritten[offset..offset + old.len()].copy_from_slice(b"\"input\":9");
    assert!(rewritten.len() > 8 * 1024);
    assert!(offset >= 64);
    assert!(offset + old.len() <= rewritten.len() - 4 * 1024);
    assert_eq!(&rewritten[..64], &initial_bytes[..64]);
    assert_eq!(
        &rewritten[rewritten.len() - 4 * 1024..],
        &initial_bytes[initial_bytes.len() - 4 * 1024..]
    );
    rewrite_preserving_modified_time(
        &root
            .path()
            .join("tmp/project-a/chats/session-batched.jsonl"),
        &rewritten,
    );

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
            .filter(|record| record.input_tokens == 9)
            .count(),
        1
    );
    assert_eq!(
        usage
            .iter()
            .filter(|record| record.output_tokens == 2)
            .count(),
        1
    );
}

#[test]
fn global_batch_stop_does_not_start_later_discovered_sources() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    for project in ["project-a", "project-b"] {
        let mut document = format!(
            "{{\"sessionId\":\"{project}\",\"projectHash\":\"synthetic\",\"startTime\":\"2026-07-26T12:00:00Z\"}}\n"
        );
        for index in 0..500 {
            document.push_str(&format!(
                "{{\"id\":\"{project}-{index}\",\"timestamp\":\"2026-07-26T12:00:01Z\",\"type\":\"gemini\",\
                 \"content\":\"answer\",\"model\":\"gemini-test\",\"tokens\":{{\"input\":1,\"output\":1}}}}\n"
            ));
        }
        write_bytes(
            root.path(),
            &format!("tmp/{project}/chats/session-heavy.jsonl"),
            document.as_bytes(),
        );
    }

    let mut scanner = scanner(&root, &state);
    let interrupted = scanner
        .scan(
            NOW,
            SessionScanControl {
                stop_after_committed_batches: Some(1),
            },
        )
        .unwrap();
    assert_eq!(interrupted.outcome, SessionScanOutcome::Interrupted);
    assert_eq!(interrupted.sources.len(), 1);
    assert_eq!(interrupted.metrics.committed_batches, 1);

    let completed = scanner
        .scan("2026-07-26T12:31:00Z", SessionScanControl::default())
        .unwrap();
    assert_eq!(completed.outcome, SessionScanOutcome::Complete);
    assert_eq!(completed.sources.len(), 2);
    assert_eq!(completed.advanced_sources, 2);
}

#[test]
fn cleanup_pending_keeps_new_generation_visible_and_does_not_block_siblings() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let heavy_relative = "tmp/project-a/chats/session-heavy.jsonl";
    let mut heavy = String::from(
        "{\"sessionId\":\"gemini-heavy\",\"projectHash\":\"synthetic\",\
         \"startTime\":\"2026-07-26T12:00:00Z\"}\n",
    );
    for index in 0..600 {
        heavy.push_str(&format!(
            "{{\"id\":\"answer-{index}\",\"timestamp\":\"2026-07-26T12:00:01Z\",\
             \"type\":\"gemini\",\"content\":\"answer\",\"model\":\"gemini-test\",\
             \"tokens\":{{\"input\":1,\"output\":1,\"cached\":0,\"thoughts\":0,\"tool\":0}}}}\n"
        ));
    }
    write_bytes(root.path(), heavy_relative, heavy.as_bytes());
    write_bytes(
        root.path(),
        "tmp/project-b/chats/session-sibling.jsonl",
        br#"{"sessionId":"gemini-sibling","projectHash":"synthetic","startTime":"2026-07-26T12:00:00Z"}
{"id":"sibling","timestamp":"2026-07-26T12:00:01Z","type":"gemini","content":"answer","model":"gemini-test","tokens":{"input":1,"output":1,"cached":0,"thoughts":0,"tool":0}}
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

    let mut rewritten = heavy.replacen("\"output\":1", "\"output\":2", 1);
    rewritten.push_str(
        "{\"id\":\"rewrite-marker\",\"timestamp\":\"2026-07-26T12:00:02Z\",\
         \"type\":\"user\",\"content\":\"forces a changed extent\"}\n",
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
        600
    );

    let recovered = scanner
        .scan("2026-07-26T12:32:00Z", SessionScanControl::default())
        .unwrap();
    assert_eq!(recovered.outcome, SessionScanOutcome::Complete);
    assert_eq!(recovered.unchanged_sources, 2);
    assert_eq!(
        scanner
            .state()
            .load_session_source(&heavy_source)
            .unwrap()
            .unwrap()
            .retired_generation,
        None
    );
}
