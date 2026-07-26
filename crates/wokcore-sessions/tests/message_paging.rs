use std::{
    fs::{self, FileTimes, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use tempfile::TempDir;
use wokcore_sessions::{
    claude::ClaudeScanner,
    codex::{CodexScanner, ScanControl},
    gemini::GeminiScanner,
    messages::{
        MAX_JSONL_PAGE_SOURCE_WORK_BYTES, MAX_MESSAGE_PAGE_MESSAGES, MAX_MESSAGE_PAGE_UTF8_BYTES,
        MessagePageCursor, MessagePageRequest, MessagePager, MessagePagerError, MessageRole,
        MessageToolType,
    },
    model::SessionScanControl,
};
use wokcore_storage::{SessionSourceKind, SessionSourceStatus};

const NOW: &str = "2026-07-26T12:30:00Z";
const TEST_DOMAIN_KEY: [u8; 32] = [0x50; 32];

fn write_bytes(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn write_source_with_exact_size(root: &Path, relative: &str, prefix: &[u8], size: u64) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .unwrap();
    assert!(size >= prefix.len() as u64);
    file.write_all(prefix).unwrap();
    let filler = vec![b'x'; 512 * 1024 - 1];
    let mut remaining = size - prefix.len() as u64;
    while remaining != 0 {
        let line_bytes = remaining.min(filler.len() as u64 + 1);
        if line_bytes == 1 {
            file.write_all(b"\n").unwrap();
        } else {
            file.write_all(&filler[..line_bytes as usize - 1]).unwrap();
            file.write_all(b"\n").unwrap();
        }
        remaining -= line_bytes;
    }
    file.flush().unwrap();
}

fn write_legacy_source_with_exact_size(root: &Path, relative: &str, size: u64) {
    let document = br#"{"sessionId":"legacy-source-work","startTime":"2026-07-26T12:00:00Z","messages":[{"id":"visible","timestamp":"2026-07-26T12:00:01Z","type":"user","content":"visible"}]}"#;
    assert!(size >= document.len() as u64);
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .unwrap();
    file.write_all(document).unwrap();
    let spaces = [b' '; 64 * 1024];
    let mut remaining = size - document.len() as u64;
    while remaining != 0 {
        let length = usize::try_from(remaining.min(spaces.len() as u64)).unwrap();
        file.write_all(&spaces[..length]).unwrap();
        remaining -= length as u64;
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

fn setup_claude() -> (TempDir, TempDir, String) {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_bytes(
        root.path(),
        "projects/project-a/main.jsonl",
        include_bytes!("fixtures/claude/snapshots.jsonl"),
    );
    let mut scanner = ClaudeScanner::open(
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let source_key = scanner
        .scan(NOW, SessionScanControl::default())
        .unwrap()
        .sources[0]
        .source_key
        .clone();
    (root, state, source_key)
}

fn setup_codex() -> (TempDir, TempDir, String) {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_bytes(
        root.path(),
        "sessions/2026/07/26/basic.jsonl",
        include_bytes!("fixtures/codex/basic.jsonl"),
    );
    let mut scanner = CodexScanner::open(
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let source_key = scanner.scan(NOW, ScanControl::default()).unwrap().sources[0]
        .source_key
        .clone();
    (root, state, source_key)
}

fn setup_gemini(relative: &str, bytes: &[u8]) -> (TempDir, TempDir, String) {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_bytes(root.path(), relative, bytes);
    let mut scanner = GeminiScanner::open(
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let source_key = scanner
        .scan(NOW, SessionScanControl::default())
        .unwrap()
        .sources[0]
        .source_key
        .clone();
    (root, state, source_key)
}

fn snapshot_files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn visit(root: &Path, current: &Path, output: &mut Vec<(PathBuf, Vec<u8>)>) {
        let mut entries = fs::read_dir(current)
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, output);
            } else {
                output.push((
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                ));
            }
        }
    }
    let mut output = Vec::new();
    visit(root, root, &mut output);
    output
}

fn collect_pages(
    pager: &mut MessagePager,
    source_key: &str,
    maximum_messages: usize,
) -> Vec<wokcore_sessions::messages::Message> {
    let mut cursor = None;
    let mut output = Vec::new();
    loop {
        let page = pager
            .page(
                source_key,
                MessagePageRequest {
                    maximum_messages,
                    maximum_utf8_bytes: MAX_MESSAGE_PAGE_UTF8_BYTES,
                    cursor,
                },
            )
            .unwrap();
        output.extend(page.messages);
        cursor = page.next_cursor;
        if cursor.is_none() {
            return output;
        }
    }
}

fn decode_cursor_hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = char::from(pair[0]).to_digit(16).unwrap() as u8;
            let low = char::from(pair[1]).to_digit(16).unwrap() as u8;
            (high << 4) | low
        })
        .collect()
}

#[test]
fn jsonl_cold_source_work_accepts_exact_limit_and_rejects_one_over() {
    for (size, expected_limit_error) in [
        (MAX_JSONL_PAGE_SOURCE_WORK_BYTES, false),
        (MAX_JSONL_PAGE_SOURCE_WORK_BYTES + 1, true),
    ] {
        let root = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        write_source_with_exact_size(
            root.path(),
            "tmp/project-a/chats/session-source-work.jsonl",
            br#"{"sessionId":"gemini-source-work","projectHash":"synthetic","startTime":"2026-07-26T12:00:00Z"}
{"id":"visible","timestamp":"2026-07-26T12:00:01Z","type":"user","content":"visible"}
"#,
            size,
        );
        let mut scanner = GeminiScanner::open(
            root.path(),
            state.path().join("state.sqlite3"),
            TEST_DOMAIN_KEY,
        )
        .unwrap();
        let scan = scanner.scan(NOW, SessionScanControl::default()).unwrap();
        let source_key = scan.sources[0].source_key.clone();
        let mut pager = MessagePager::open(
            SessionSourceKind::Gemini,
            root.path(),
            state.path().join("state.sqlite3"),
            TEST_DOMAIN_KEY,
        )
        .unwrap();
        let result = pager.page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: 64,
                cursor: None,
            },
        );
        if expected_limit_error {
            assert_eq!(scan.sources[0].status, SessionSourceStatus::ResourceLimited);
            assert!(matches!(result, Err(MessagePagerError::SourceUnavailable)));
            assert_eq!(pager.metrics().parser_read_bytes, 0);
            assert_eq!(pager.metrics().jsonl_index_rebuilds, 0);
        } else {
            assert_eq!(result.unwrap().messages[0].content, "visible");
            assert_eq!(
                pager.metrics().parser_read_bytes,
                MAX_JSONL_PAGE_SOURCE_WORK_BYTES
            );
        }
    }
}

#[test]
fn legacy_cold_source_work_accepts_exact_limit_with_a_visible_message() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_legacy_source_with_exact_size(
        root.path(),
        "tmp/project-a/chats/session-source-work.json",
        MAX_JSONL_PAGE_SOURCE_WORK_BYTES,
    );
    let mut scanner = GeminiScanner::open(
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let source_key = scanner
        .scan(NOW, SessionScanControl::default())
        .unwrap()
        .sources[0]
        .source_key
        .clone();
    let mut pager = MessagePager::open(
        SessionSourceKind::Gemini,
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let page = pager
        .page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: 64,
                cursor: None,
            },
        )
        .unwrap();
    assert_eq!(page.messages[0].content, "visible");
    assert_eq!(
        pager.metrics().parser_read_bytes,
        MAX_JSONL_PAGE_SOURCE_WORK_BYTES
    );
    assert!(pager.metrics().message_seek_read_bytes > 0);
}

#[test]
fn codex_large_source_pages_nearby_messages_before_hitting_per_page_work_limit() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let prefix = br#"{"timestamp":"2026-07-26T00:00:00Z","type":"session_meta","payload":{"id":"codex-large"}}
{"timestamp":"2026-07-26T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"first"}]}}
{"timestamp":"2026-07-26T00:00:02Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"second"}]}}
"#;
    write_source_with_exact_size(
        root.path(),
        "sessions/2026/07/26/large.jsonl",
        prefix,
        MAX_JSONL_PAGE_SOURCE_WORK_BYTES + prefix.len() as u64 + 1,
    );
    let mut scanner = CodexScanner::open(
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let source_key = scanner.scan(NOW, ScanControl::default()).unwrap().sources[0]
        .source_key
        .clone();
    let mut pager = MessagePager::open(
        SessionSourceKind::Codex,
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();

    let first = pager
        .page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: 64,
                cursor: None,
            },
        )
        .unwrap();
    assert_eq!(first.messages[0].content, "first");
    let second = pager
        .page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: 64,
                cursor: first.next_cursor,
            },
        )
        .unwrap();
    assert_eq!(second.messages[0].content, "second");
    assert!(matches!(
        pager.page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: 64,
                cursor: second.next_cursor,
            }
        ),
        Err(MessagePagerError::ResourceLimit)
    ));
}

#[test]
fn live_source_changes_do_not_leak_through_an_old_generation() {
    let (root, state, source_key) = setup_claude();
    let mut pager = MessagePager::open(
        SessionSourceKind::Claude,
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let _ = pager
        .page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: 64,
                cursor: None,
            },
        )
        .unwrap();
    let path = root.path().join("projects/project-a/main.jsonl");
    let mut bytes = fs::read(&path).unwrap();
    let old = b"cache only";
    let new = b"secret new";
    let offset = bytes
        .windows(old.len())
        .position(|window| window == old)
        .unwrap();
    bytes[offset..offset + old.len()].copy_from_slice(new);
    fs::write(&path, &bytes).unwrap();
    assert!(matches!(
        pager.page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: 64,
                cursor: None,
            }
        ),
        Err(MessagePagerError::StaleCursor)
    ));
    assert!(!format!("{:?}", pager.metrics()).contains("secret new"));

    let (gemini_root, gemini_state, gemini_key) = setup_gemini(
        "tmp/project-a/chats/session-current.jsonl",
        include_bytes!("fixtures/gemini/current.jsonl"),
    );
    let mut gemini_pager = MessagePager::open(
        SessionSourceKind::Gemini,
        gemini_root.path(),
        gemini_state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let mut writer = OpenOptions::new()
        .append(true)
        .open(
            gemini_root
                .path()
                .join("tmp/project-a/chats/session-current.jsonl"),
        )
        .unwrap();
    writer
        .write_all(
            br#"{"id":"unscanned","timestamp":"2026-07-26T12:30:00Z","type":"user","content":"UNSCANNED-MUST-NOT-ESCAPE"}
"#,
        )
        .unwrap();
    writer.flush().unwrap();
    assert!(matches!(
        gemini_pager.page(
            &gemini_key,
            MessagePageRequest {
                maximum_messages: 128,
                maximum_utf8_bytes: MAX_MESSAGE_PAGE_UTF8_BYTES,
                cursor: None,
            }
        ),
        Err(MessagePagerError::StaleCursor)
    ));
}

#[test]
fn codex_partial_tail_is_excluded_by_the_promoted_extent() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_bytes(
        root.path(),
        "sessions/2026/07/26/partial.jsonl",
        br#"{"timestamp":"2026-07-26T00:00:00Z","type":"session_meta","payload":{"id":"codex-partial"}}
{"timestamp":"2026-07-26T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"stable"}]}}
{"timestamp":"2026-07-26T00:00:02Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"UNSCANNED-PARTIAL"
"#,
    );
    let mut scanner = CodexScanner::open(
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let source_key = scanner.scan(NOW, ScanControl::default()).unwrap().sources[0]
        .source_key
        .clone();
    let mut pager = MessagePager::open(
        SessionSourceKind::Codex,
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();

    let page = pager
        .page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 8,
                maximum_utf8_bytes: 1024,
                cursor: None,
            },
        )
        .unwrap();

    assert_eq!(page.messages.len(), 1);
    assert_eq!(page.messages[0].content, "stable");
    assert!(page.next_cursor.is_none());
    assert!(!format!("{page:?}").contains("UNSCANNED-PARTIAL"));
}

#[test]
fn codex_title_lookup_rejects_an_unscanned_source_revision() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "sessions/2026/07/26/title.jsonl";
    write_bytes(
        root.path(),
        relative,
        br#"{"timestamp":"2026-07-26T00:00:00Z","type":"session_meta","payload":{"id":"codex-title"}}
"#,
    );
    write_bytes(
        root.path(),
        "session_index.jsonl",
        br#"{"id":"codex-title","thread_name":"stable title"}
"#,
    );
    let mut scanner = CodexScanner::open(
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let source_key = scanner.scan(NOW, ScanControl::default()).unwrap().sources[0]
        .source_key
        .clone();
    assert_eq!(
        scanner
            .title_for_source(&source_key)
            .unwrap()
            .unwrap()
            .as_str(),
        "stable title"
    );
    let mut writer = OpenOptions::new()
        .append(true)
        .open(root.path().join(relative))
        .unwrap();
    writer
        .write_all(b"{\"type\":\"unscanned-title-source-change\"}\n")
        .unwrap();
    writer.flush().unwrap();
    assert!(scanner.title_for_source(&source_key).unwrap().is_none());
}

#[test]
fn codex_cursor_resumes_from_the_next_byte_boundary() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let mut bytes = vec![b'x'; 384 * 1024];
    bytes.push(b'\n');
    bytes.extend_from_slice(
        br#"{"timestamp":"2026-07-26T00:00:00Z","type":"session_meta","payload":{"id":"codex-seek"}}
{"timestamp":"2026-07-26T00:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"first"}]}}
{"timestamp":"2026-07-26T00:00:02Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"second"}]}}
"#,
    );
    write_bytes(root.path(), "sessions/2026/07/26/seek.jsonl", &bytes);
    let mut scanner = CodexScanner::open(
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let source_key = scanner.scan(NOW, ScanControl::default()).unwrap().sources[0]
        .source_key
        .clone();
    let mut pager = MessagePager::open(
        SessionSourceKind::Codex,
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let first = pager
        .page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: 64,
                cursor: None,
            },
        )
        .unwrap();
    assert_eq!(first.messages[0].content, "first");
    let first_read = pager.metrics().parser_read_bytes;
    assert!(first_read > 384 * 1024);

    let second = pager
        .page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: 64,
                cursor: first.next_cursor,
            },
        )
        .unwrap();
    assert_eq!(second.messages[0].content, "second");
    let second_read = pager.metrics().parser_read_bytes - first_read;
    assert!(
        second_read < 64 * 1024,
        "the second page must seek past the large prefix, read {second_read} bytes"
    );
}

#[test]
fn indexed_jsonl_cache_is_reused_and_evicted_by_source_or_snapshot() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    for (name, session) in [("a", "claude-a"), ("b", "claude-b")] {
        write_bytes(
            root.path(),
            &format!("projects/project-a/{name}.jsonl"),
            format!(
                "{{\"type\":\"user\",\"uuid\":\"{name}-1\",\"sessionId\":\"{session}\",\
                 \"timestamp\":\"2026-07-26T12:00:00Z\",\"message\":{{\"role\":\"user\",\
                 \"content\":\"{name}-first\"}}}}\n\
                 {{\"type\":\"user\",\"uuid\":\"{name}-2\",\"sessionId\":\"{session}\",\
                 \"timestamp\":\"2026-07-26T12:00:01Z\",\"message\":{{\"role\":\"user\",\
                 \"content\":\"{name}-second\"}}}}\n"
            )
            .as_bytes(),
        );
    }
    let mut scanner = ClaudeScanner::open(
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let summary = scanner.scan(NOW, SessionScanControl::default()).unwrap();
    let key_a = &summary.sources[0].source_key;
    let key_b = &summary.sources[1].source_key;
    let mut pager = MessagePager::open(
        SessionSourceKind::Claude,
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();

    let first = pager
        .page(
            key_a,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: 64,
                cursor: None,
            },
        )
        .unwrap();
    let after_rebuild = pager.metrics();
    assert_eq!(after_rebuild.jsonl_index_rebuilds, 1);
    let _ = pager
        .page(
            key_a,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: 64,
                cursor: first.next_cursor,
            },
        )
        .unwrap();
    let after_hit = pager.metrics();
    assert_eq!(after_hit.jsonl_index_cache_hits, 1);
    assert_eq!(
        after_hit.parser_read_bytes, after_rebuild.parser_read_bytes,
        "a cache hit must seek message records without reparsing the source"
    );

    let _ = pager
        .page(
            key_b,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: 64,
                cursor: None,
            },
        )
        .unwrap();
    let _ = pager
        .page(
            key_a,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: 64,
                cursor: None,
            },
        )
        .unwrap();
    assert_eq!(pager.metrics().jsonl_index_rebuilds, 3);

    fs::OpenOptions::new()
        .append(true)
        .open(root.path().join("projects/project-a/a.jsonl"))
        .unwrap()
        .write_all(b"{\"type\":\"progress\"}\n")
        .unwrap();
    assert!(matches!(
        pager.page(
            key_a,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: 64,
                cursor: None,
            },
        ),
        Err(MessagePagerError::StaleCursor)
    ));
    assert_eq!(
        pager.metrics().jsonl_index_rebuilds,
        3,
        "a changed pinned snapshot must neither reuse nor rebuild an old-generation index"
    );
}

#[test]
fn request_accepts_exact_hard_limits_and_rejects_one_over() {
    let (root, state, source_key) = setup_claude();
    let mut pager = MessagePager::open(
        SessionSourceKind::Claude,
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    pager
        .page(
            &source_key,
            MessagePageRequest {
                maximum_messages: MAX_MESSAGE_PAGE_MESSAGES,
                maximum_utf8_bytes: MAX_MESSAGE_PAGE_UTF8_BYTES,
                cursor: None,
            },
        )
        .unwrap();
    assert!(matches!(
        pager.page(
            &source_key,
            MessagePageRequest {
                maximum_messages: MAX_MESSAGE_PAGE_MESSAGES + 1,
                maximum_utf8_bytes: MAX_MESSAGE_PAGE_UTF8_BYTES,
                cursor: None,
            }
        ),
        Err(MessagePagerError::InvalidPageLimit)
    ));
    assert!(matches!(
        pager.page(
            &source_key,
            MessagePageRequest {
                maximum_messages: MAX_MESSAGE_PAGE_MESSAGES,
                maximum_utf8_bytes: MAX_MESSAGE_PAGE_UTF8_BYTES + 1,
                cursor: None,
            }
        ),
        Err(MessagePagerError::InvalidPageLimit)
    ));
}

#[test]
fn opaque_cursor_pages_normalized_content_and_tool_metadata_without_payloads() {
    let (root, state, source_key) = setup_claude();
    let mut pager = MessagePager::open(
        SessionSourceKind::Claude,
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let first = pager
        .page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: 64,
                cursor: None,
            },
        )
        .unwrap();
    assert_eq!(first.messages.len(), 1);
    assert_eq!(first.messages[0].role, MessageRole::User);
    assert_eq!(first.messages[0].content, "任意语言：你好 🌏");
    assert_eq!(first.messages[0].timestamp, "2026-07-26T12:00:01Z");

    let cursor = first.next_cursor.unwrap();
    let encoded = cursor.as_str();
    for forbidden in [
        &source_key,
        "claude-main",
        "main.jsonl",
        "projects",
        "byte_offset",
        "sessionId",
    ] {
        assert!(!encoded.contains(forbidden));
    }
    assert_eq!(format!("{cursor:?}"), "MessagePageCursor(<redacted>)");

    let second = pager
        .page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 2,
                maximum_utf8_bytes: 128,
                cursor: Some(cursor),
            },
        )
        .unwrap();
    assert_eq!(second.messages.len(), 2);
    let assistant = second
        .messages
        .iter()
        .find(|message| message.content.contains("最终回答"))
        .unwrap();
    assert_eq!(assistant.role, MessageRole::Assistant);
    assert_eq!(assistant.tools.len(), 1);
    assert_eq!(assistant.tools[0].tool_type, MessageToolType::Call);
    assert_eq!(assistant.tools[0].name.as_deref(), Some("Read"));
    let debug = format!("{second:?}");
    assert!(!debug.contains("TOOL-INPUT-MUST-NOT-ESCAPE"));
}

#[test]
fn cursor_authentication_is_tamper_evident_and_old_generation_is_stale() {
    let (root, state, source_key) = setup_claude();
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
                maximum_messages: 1,
                maximum_utf8_bytes: 64,
                cursor: None,
            },
        )
        .unwrap();
    let cursor = page.next_cursor.unwrap();
    let next_page = pager
        .page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: 64,
                cursor: Some(cursor.clone()),
            },
        )
        .unwrap();
    let next_cursor = next_page.next_cursor.unwrap();
    let first_bytes = decode_cursor_hex(cursor.as_str());
    let second_bytes = decode_cursor_hex(next_cursor.as_str());
    const NONCE_BYTES: usize = 16;
    const PAYLOAD_BYTES: usize = 34;
    const TAG_BYTES: usize = 32;
    assert_eq!(first_bytes.len(), NONCE_BYTES + PAYLOAD_BYTES + TAG_BYTES);
    assert_eq!(second_bytes.len(), first_bytes.len());
    assert_ne!(
        &first_bytes[..NONCE_BYTES],
        &second_bytes[..NONCE_BYTES],
        "different cursor plaintexts must use different nonce-derived masks"
    );
    let cipher_delta = first_bytes[NONCE_BYTES..NONCE_BYTES + PAYLOAD_BYTES]
        .iter()
        .zip(&second_bytes[NONCE_BYTES..NONCE_BYTES + PAYLOAD_BYTES])
        .map(|(left, right)| left ^ right)
        .collect::<Vec<_>>();
    let mut known_plain_delta = vec![0u8; PAYLOAD_BYTES];
    known_plain_delta[17] = 3;
    assert_ne!(
        cipher_delta, known_plain_delta,
        "a known cursor plaintext must not reveal the mask for another cursor"
    );
    let mut tampered = cursor.as_str().as_bytes().to_vec();
    let last = tampered.last_mut().unwrap();
    *last = if *last == b'a' { b'b' } else { b'a' };
    let tampered = MessagePageCursor::parse(std::str::from_utf8(&tampered).unwrap()).unwrap();
    assert!(matches!(
        pager.page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: 64,
                cursor: Some(tampered),
            }
        ),
        Err(MessagePagerError::InvalidCursor)
    ));

    fs::write(
        root.path().join("projects/project-a/main.jsonl"),
        br#"{"type":"user","uuid":"replacement","sessionId":"claude-main","timestamp":"2026-07-26T12:01:00Z","message":{"role":"user","content":"replacement generation"}}
{"type":"assistant","uuid":"replacement-answer","sessionId":"claude-main","timestamp":"2026-07-26T12:01:01Z","message":{"id":"replacement-answer","role":"assistant","model":"claude-test","content":"replacement answer","usage":{"input_tokens":1,"output_tokens":1}}}
"#,
    )
    .unwrap();
    let mut scanner = ClaudeScanner::open(
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    scanner
        .scan("2026-07-26T12:31:00Z", SessionScanControl::default())
        .unwrap();
    assert!(matches!(
        pager.page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: 64,
                cursor: Some(cursor),
            }
        ),
        Err(MessagePagerError::StaleCursor)
    ));
}

#[test]
fn cold_pager_rejects_same_snapshot_middle_rewrites_for_current_and_legacy_gemini() {
    let prefix = "p".repeat(5 * 1024);
    let suffix = "s".repeat(5 * 1024);
    let cases = vec![
        (
            "tmp/project-a/chats/session-cold-fingerprint.jsonl",
            format!(
                "{{\"sessionId\":\"cold-current\",\"projectHash\":\"synthetic\",\"startTime\":\"2026-07-26T12:00:00Z\",\"prefix\":\"{prefix}\"}}\n\
                 {{\"id\":\"user\",\"timestamp\":\"2026-07-26T12:00:01Z\",\"type\":\"user\",\"content\":\"question\"}}\n\
                 {{\"id\":\"answer\",\"timestamp\":\"2026-07-26T12:00:02Z\",\"type\":\"gemini\",\"content\":\"answer-A\",\"model\":\"gemini-test\",\"tokens\":{{\"input\":1,\"output\":1}}}}\n\
                 {{\"ignored\":\"{suffix}\"}}\n"
            )
            .into_bytes(),
            b"answer-A".as_slice(),
            b"answer-B".as_slice(),
        ),
        (
            "tmp/project-a/chats/session-cold-fingerprint.json",
            format!(
                "{{\"sessionId\":\"cold-legacy\",\"projectHash\":\"synthetic\",\"startTime\":\"2026-07-26T12:00:00Z\",\"prefix\":\"{prefix}\",\
                 \"messages\":[{{\"id\":\"user\",\"timestamp\":\"2026-07-26T12:00:01Z\",\"type\":\"user\",\"content\":\"question\"}},\
                 {{\"id\":\"answer\",\"timestamp\":\"2026-07-26T12:00:02Z\",\"type\":\"gemini\",\"content\":\"answer-A\",\"model\":\"gemini-test\",\"tokens\":{{\"input\":1,\"output\":1}}}}],\
                 \"suffix\":\"{suffix}\"}}"
            )
            .into_bytes(),
            b"answer-A".as_slice(),
            b"answer-B".as_slice(),
        ),
    ];
    for (relative, initial, old, new) in cases {
        assert!(initial.len() > 8 * 1024);
        let (root, state, source_key) = setup_gemini(relative, &initial);
        let mut first_pager = MessagePager::open(
            SessionSourceKind::Gemini,
            root.path(),
            state.path().join("state.sqlite3"),
            TEST_DOMAIN_KEY,
        )
        .unwrap();
        let first = first_pager
            .page(
                &source_key,
                MessagePageRequest {
                    maximum_messages: 1,
                    maximum_utf8_bytes: MAX_MESSAGE_PAGE_UTF8_BYTES,
                    cursor: None,
                },
            )
            .unwrap_or_else(|error| panic!("{relative}: {error:?}"));
        let cursor = first.next_cursor.unwrap();

        let mut rewritten = initial.clone();
        let offset = rewritten
            .windows(old.len())
            .position(|window| window == old)
            .unwrap();
        rewritten[offset..offset + old.len()].copy_from_slice(new);
        assert!(offset >= 64);
        assert!(offset + old.len() <= initial.len() - 4 * 1024);
        assert_eq!(&rewritten[..64], &initial[..64]);
        assert_eq!(
            &rewritten[rewritten.len() - 4 * 1024..],
            &initial[initial.len() - 4 * 1024..]
        );
        assert_ne!(rewritten, initial);
        rewrite_preserving_modified_time(&root.path().join(relative), &rewritten);

        assert!(matches!(
            first_pager.page(
                &source_key,
                MessagePageRequest {
                    maximum_messages: 1,
                    maximum_utf8_bytes: MAX_MESSAGE_PAGE_UTF8_BYTES,
                    cursor: Some(cursor.clone()),
                }
            ),
            Err(MessagePagerError::StaleCursor)
        ));
        drop(first_pager);

        let mut cold = MessagePager::open(
            SessionSourceKind::Gemini,
            root.path(),
            state.path().join("state.sqlite3"),
            TEST_DOMAIN_KEY,
        )
        .unwrap();
        assert!(matches!(
            cold.page(
                &source_key,
                MessagePageRequest {
                    maximum_messages: 1,
                    maximum_utf8_bytes: MAX_MESSAGE_PAGE_UTF8_BYTES,
                    cursor: Some(cursor),
                }
            ),
            Err(MessagePagerError::StaleCursor)
        ));
    }
}

#[test]
fn hot_claude_cache_rejects_same_snapshot_middle_rewrite() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let relative = "projects/project-a/main.jsonl";
    let prefix = "p".repeat(5 * 1024);
    let suffix = "s".repeat(5 * 1024);
    let initial = format!(
        "{{\"type\":\"metadata\",\"padding\":\"{prefix}\"}}\n\
         {{\"type\":\"user\",\"uuid\":\"user\",\"sessionId\":\"claude-hot\",\"timestamp\":\"2026-07-26T12:00:00Z\",\"message\":{{\"role\":\"user\",\"content\":\"question\"}}}}\n\
         {{\"type\":\"assistant\",\"uuid\":\"answer\",\"sessionId\":\"claude-hot\",\"timestamp\":\"2026-07-26T12:00:01Z\",\"message\":{{\"id\":\"answer\",\"role\":\"assistant\",\"model\":\"claude-test\",\"content\":\"answer-A\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}}}\n\
         {{\"type\":\"metadata\",\"padding\":\"{suffix}\"}}\n"
    )
    .into_bytes();
    write_bytes(root.path(), relative, &initial);
    let mut scanner = ClaudeScanner::open(
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let source_key = scanner
        .scan(NOW, SessionScanControl::default())
        .unwrap()
        .sources[0]
        .source_key
        .clone();
    let mut pager = MessagePager::open(
        SessionSourceKind::Claude,
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let first = pager
        .page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: MAX_MESSAGE_PAGE_UTF8_BYTES,
                cursor: None,
            },
        )
        .unwrap();
    let cursor = first.next_cursor.unwrap();
    let mut rewritten = initial.clone();
    let old = b"answer-A";
    let offset = rewritten
        .windows(old.len())
        .position(|window| window == old)
        .unwrap();
    rewritten[offset..offset + old.len()].copy_from_slice(b"answer-B");
    assert!(offset >= 64);
    assert!(offset + old.len() <= rewritten.len() - 4 * 1024);
    assert_eq!(&rewritten[..64], &initial[..64]);
    assert_eq!(
        &rewritten[rewritten.len() - 4 * 1024..],
        &initial[initial.len() - 4 * 1024..]
    );
    rewrite_preserving_modified_time(&root.path().join(relative), &rewritten);

    assert!(matches!(
        pager.page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: MAX_MESSAGE_PAGE_UTF8_BYTES,
                cursor: Some(cursor),
            }
        ),
        Err(MessagePagerError::StaleCursor)
    ));
}

#[test]
fn oversized_jsonl_control_records_fail_closed_with_bounded_peak_memory() {
    let padding = " ".repeat(600 * 1024);
    for control in [
        format!(
            "{{\"$set\":{{\"messages\":[{{\"id\":\"user-1\",\"timestamp\":\"2026-07-26T12:00:01Z\",\"type\":\"user\",\"content\":\"replacement\"}}]}}{padding}}}\n"
        ),
        format!("{{\"$rewindTo\":\"user-1\"{padding}}}\n"),
        format!(
            "{{\"id\":\"user-1\",\"timestamp\":\"2026-07-26T12:00:01Z\",\"type\":\"user\",\"content\":\"replacement\"{padding}}}\n"
        ),
    ] {
        let document = format!(
            "{{\"sessionId\":\"oversized-control\",\"projectHash\":\"synthetic\",\"startTime\":\"2026-07-26T12:00:00Z\"}}\n\
             {{\"id\":\"user-1\",\"timestamp\":\"2026-07-26T12:00:01Z\",\"type\":\"user\",\"content\":\"original\"}}\n\
             {control}"
        );
        let (root, state, source_key) = setup_gemini(
            "tmp/project-a/chats/session-oversized-control.jsonl",
            document.as_bytes(),
        );
        let mut pager = MessagePager::open(
            SessionSourceKind::Gemini,
            root.path(),
            state.path().join("state.sqlite3"),
            TEST_DOMAIN_KEY,
        )
        .unwrap();
        assert!(matches!(
            pager.page(
                &source_key,
                MessagePageRequest {
                    maximum_messages: 8,
                    maximum_utf8_bytes: MAX_MESSAGE_PAGE_UTF8_BYTES,
                    cursor: None,
                }
            ),
            Err(MessagePagerError::ResourceLimit)
        ));
        assert!(
            pager.metrics().peak_parser_buffer_bytes <= 512 * 1024 + 64 * 1024,
            "oversized records must fail before their body accumulates"
        );
    }
}

#[test]
fn tiny_jsonl_records_have_linear_scan_and_compaction_work() {
    let mut document = String::from(
        "{\"sessionId\":\"tiny-lines\",\"projectHash\":\"synthetic\",\"startTime\":\"2026-07-26T12:00:00Z\"}\n",
    );
    for _ in 0..16_000 {
        document.push_str("{}\n");
    }
    document.push_str(
        "{\"id\":\"user\",\"timestamp\":\"2026-07-26T12:00:01Z\",\"type\":\"user\",\"content\":\"visible\"}\n",
    );
    let (root, state, source_key) = setup_gemini(
        "tmp/project-a/chats/session-tiny-lines.jsonl",
        document.as_bytes(),
    );
    let mut pager = MessagePager::open(
        SessionSourceKind::Gemini,
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let page = pager
        .page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: MAX_MESSAGE_PAGE_UTF8_BYTES,
                cursor: None,
            },
        )
        .unwrap();
    assert_eq!(page.messages.len(), 1);
    let metrics = pager.metrics();
    assert!(metrics.parser_read_bytes > 0);
    assert!(metrics.parser_examined_bytes <= metrics.parser_read_bytes.saturating_mul(3));
    assert!(metrics.parser_compacted_bytes <= metrics.parser_read_bytes.saturating_mul(2));
}

#[test]
fn official_parts_and_tool_only_messages_keep_only_safe_fields() {
    let claude_root = TempDir::new().unwrap();
    let claude_state = TempDir::new().unwrap();
    write_bytes(
        claude_root.path(),
        "projects/project-a/tool-only.jsonl",
        br#"{"type":"user","uuid":"tool-result","sessionId":"claude-tool-only","timestamp":"2026-07-26T12:00:00Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":"CLAUDE-RESULT-MUST-NOT-ESCAPE"}]}}
{"type":"assistant","uuid":"tool-call","sessionId":"claude-tool-only","timestamp":"2026-07-26T12:00:01Z","message":{"id":"tool-call","role":"assistant","model":"claude-test","content":[{"type":"tool_use","name":"Read","input":{"path":"CLAUDE-INPUT-MUST-NOT-ESCAPE"}}],"usage":{"input_tokens":1,"output_tokens":1}}}
"#,
    );
    let mut claude_scanner = ClaudeScanner::open(
        claude_root.path(),
        claude_state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let claude_key = claude_scanner
        .scan(NOW, SessionScanControl::default())
        .unwrap()
        .sources[0]
        .source_key
        .clone();
    let mut claude = MessagePager::open(
        SessionSourceKind::Claude,
        claude_root.path(),
        claude_state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let claude_messages = collect_pages(&mut claude, &claude_key, 8);
    assert_eq!(claude_messages.len(), 2);
    assert!(
        claude_messages
            .iter()
            .all(|message| message.content.is_empty())
    );
    assert_eq!(
        claude_messages[0].tools[0].tool_type,
        MessageToolType::Result
    );
    assert_eq!(claude_messages[1].tools[0].tool_type, MessageToolType::Call);
    assert_eq!(claude_messages[1].tools[0].name.as_deref(), Some("Read"));
    let claude_debug = format!("{claude_messages:?}");
    assert!(!claude_debug.contains("CLAUDE-RESULT-MUST-NOT-ESCAPE"));
    assert!(!claude_debug.contains("CLAUDE-INPUT-MUST-NOT-ESCAPE"));

    let gemini = br#"{"sessionId":"gemini-parts","projectHash":"synthetic","startTime":"2026-07-26T12:00:00Z"}
{"id":"user-1","timestamp":"2026-07-26T12:00:01Z","type":"user","content":[{"text":"visible part"},{"thought":"GEMINI-THOUGHT-MUST-NOT-ESCAPE"}]}
{"id":"tool-1","timestamp":"2026-07-26T12:00:02Z","type":"gemini","content":[{"functionCall":{"name":"shell","args":{"secret":"GEMINI-ARGS-MUST-NOT-ESCAPE"}}}],"model":"gemini-test","tokens":{"input":1,"output":1}}
{"id":"tool-2","timestamp":"2026-07-26T12:00:03Z","type":"gemini","content":[{"functionResponse":{"name":"shell","response":{"output":"GEMINI-RESULT-MUST-NOT-ESCAPE"}}}],"model":"gemini-test","tokens":{"input":1,"output":1}}
"#;
    let (gemini_root, gemini_state, gemini_key) =
        setup_gemini("tmp/project-a/chats/session-parts.jsonl", gemini);
    let mut gemini_pager = MessagePager::open(
        SessionSourceKind::Gemini,
        gemini_root.path(),
        gemini_state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let gemini_messages = collect_pages(&mut gemini_pager, &gemini_key, 8);
    assert_eq!(gemini_messages.len(), 3);
    assert_eq!(gemini_messages[0].content, "visible part");
    assert!(gemini_messages[1].content.is_empty());
    assert!(gemini_messages[2].content.is_empty());
    assert_eq!(gemini_messages[1].tools[0].tool_type, MessageToolType::Call);
    assert_eq!(
        gemini_messages[2].tools[0].tool_type,
        MessageToolType::Result
    );
    assert_eq!(gemini_messages[1].tools[0].name.as_deref(), Some("shell"));
    let gemini_debug = format!("{gemini_messages:?}");
    for secret in [
        "GEMINI-THOUGHT-MUST-NOT-ESCAPE",
        "GEMINI-ARGS-MUST-NOT-ESCAPE",
        "GEMINI-RESULT-MUST-NOT-ESCAPE",
    ] {
        assert!(!gemini_debug.contains(secret));
    }
}

#[test]
fn message_and_page_debug_are_redacted() {
    let (root, state, source_key) = setup_claude();
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
                maximum_messages: 1,
                maximum_utf8_bytes: MAX_MESSAGE_PAGE_UTF8_BYTES,
                cursor: None,
            },
        )
        .unwrap();
    let secret = &page.messages[0].content;
    assert!(!format!("{:?}", page.messages[0]).contains(secret));
    assert!(!format!("{page:?}").contains(secret));
}

#[test]
fn legacy_small_pages_use_one_linear_index_and_exact_eof_has_no_tail_cursor() {
    let mut messages = Vec::new();
    for index in 0..256 {
        messages.push(serde_json::json!({
            "id": format!("message-{index}"),
            "timestamp": format!("2026-07-26T12:{:02}:{:02}Z", index / 60, index % 60),
            "type": if index % 2 == 0 { "user" } else { "gemini" },
            "content": format!("message {index}"),
            "model": "gemini-test",
            "tokens": {"input": 1, "output": 1}
        }));
    }
    let document = serde_json::to_vec(&serde_json::json!({
        "sessionId": "legacy-linear",
        "startTime": "2026-07-26T12:00:00Z",
        "messages": messages
    }))
    .unwrap();
    let (root, state, source_key) =
        setup_gemini("tmp/project-a/chats/session-legacy-linear.json", &document);
    let mut pager = MessagePager::open(
        SessionSourceKind::Gemini,
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let all = collect_pages(&mut pager, &source_key, 1);
    assert_eq!(all.len(), 256);
    let metrics = pager.metrics();
    assert_eq!(metrics.legacy_index_rebuilds, 1);
    assert!(metrics.legacy_index_cache_hits >= 255);
    assert!(metrics.parser_read_bytes >= document.len() as u64);
    assert!(
        metrics.parser_read_bytes + metrics.message_seek_read_bytes < document.len() as u64 * 4,
        "small-page pagination must remain linear in source size"
    );

    let one = serde_json::to_vec(&serde_json::json!({
        "sessionId": "legacy-exact",
        "startTime": "2026-07-26T12:00:00Z",
        "messages": [{
            "id": "message-1",
            "timestamp": "2026-07-26T12:00:01Z",
            "type": "user",
            "content": "only message"
        }]
    }))
    .unwrap();
    let (one_root, one_state, one_key) =
        setup_gemini("tmp/project-a/chats/session-legacy-exact.json", &one);
    let mut one_pager = MessagePager::open(
        SessionSourceKind::Gemini,
        one_root.path(),
        one_state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let page = one_pager
        .page(
            &one_key,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: MAX_MESSAGE_PAGE_UTF8_BYTES,
                cursor: None,
            },
        )
        .unwrap();
    assert_eq!(page.messages.len(), 1);
    assert!(page.next_cursor.is_none());
}

#[test]
fn current_checkpoint_small_pages_seek_only_exact_message_elements() {
    let messages = (0..256)
        .map(|index| {
            serde_json::json!({
                "id": format!("message-{index}"),
                "timestamp": format!("2026-07-26T12:{:02}:{:02}Z", index / 60, index % 60),
                "type": if index % 2 == 0 { "user" } else { "gemini" },
                "content": format!("checkpoint message {index}"),
                "model": "gemini-test",
                "tokens": {"input": 1, "output": 1}
            })
        })
        .collect::<Vec<_>>();
    let expected_seek_bytes = messages
        .iter()
        .map(|message| serde_json::to_vec(message).unwrap().len() as u64)
        .sum::<u64>();
    let metadata = serde_json::json!({
        "sessionId": "current-checkpoint-linear",
        "projectHash": "synthetic",
        "startTime": "2026-07-26T12:00:00Z"
    });
    let checkpoint = serde_json::json!({"$set": {"messages": messages}});
    let document = format!(
        "{}\n{}\n",
        serde_json::to_string(&metadata).unwrap(),
        serde_json::to_string(&checkpoint).unwrap()
    );
    let (root, state, source_key) = setup_gemini(
        "tmp/project-a/chats/session-current-checkpoint-linear.jsonl",
        document.as_bytes(),
    );
    let mut pager = MessagePager::open(
        SessionSourceKind::Gemini,
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();

    let all = collect_pages(&mut pager, &source_key, 1);
    assert_eq!(all.len(), 256);
    assert_eq!(all[0].content, "checkpoint message 0");
    assert_eq!(all[255].content, "checkpoint message 255");
    let metrics = pager.metrics();
    assert_eq!(metrics.jsonl_index_rebuilds, 1);
    assert!(metrics.jsonl_index_cache_hits >= 255);
    assert_eq!(
        metrics.message_seek_read_bytes, expected_seek_bytes,
        "each checkpoint page must seek only the selected message element"
    );
    assert_eq!(metrics.parser_read_bytes, document.len() as u64);
}

#[test]
fn current_checkpoint_span_index_decodes_keys_uses_last_duplicates_and_crosses_chunks() {
    let padding = "x".repeat(64 * 1024);
    let document = format!(
        "{{\"sessionId\":\"escaped-checkpoint\",\"projectHash\":\"synthetic\",\
         \"startTime\":\"2026-07-26T12:00:00Z\"}}\n\
         {{\"padding\":\"{padding}\",\
         \"$set\":{{\"messages\":[{{\"id\":\"decoy\",\"timestamp\":\"2026-07-26T12:00:00Z\",\
         \"type\":\"user\",\"content\":\"decoy\"}}]}},\
         \"$s\\u0065t\":{{\"nested\":{{\"messages\":[{{\"id\":\"nested\",\
         \"timestamp\":\"2026-07-26T12:00:00Z\",\"type\":\"user\",\
         \"content\":\"nested decoy\"}}]}},\
         \"m\\u0065ssages\":[\
         {{\"id\":\"actual-1\",\"timestamp\":\"2026-07-26T12:00:01Z\",\
         \"type\":\"user\",\"content\":\"actual one\"}},\
         {{\"id\":\"actual-2\",\"timestamp\":\"2026-07-26T12:00:02Z\",\
         \"type\":\"gemini\",\"content\":\"actual two\",\"model\":\"gemini-test\"}}]}}}}\n"
    );
    let (root, state, source_key) = setup_gemini(
        "tmp/project-a/chats/session-current-escaped-checkpoint.jsonl",
        document.as_bytes(),
    );
    let mut pager = MessagePager::open(
        SessionSourceKind::Gemini,
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();

    let messages = collect_pages(&mut pager, &source_key, 1);
    assert_eq!(
        messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        vec!["actual one", "actual two"]
    );
    assert!(pager.metrics().message_seek_read_bytes < 512);
}

#[test]
fn codex_and_both_gemini_formats_page_only_normalized_message_fields() {
    let (codex_root, codex_state, codex_key) = setup_codex();
    let mut codex = MessagePager::open(
        SessionSourceKind::Codex,
        codex_root.path(),
        codex_state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let codex_messages = collect_pages(&mut codex, &codex_key, 1);
    assert_eq!(codex_messages.len(), 1);
    assert_eq!(codex_messages[0].role, MessageRole::Assistant);
    assert!(codex_messages[0].content.contains("مرحبا"));

    let (current_root, current_state, current_key) = setup_gemini(
        "tmp/project-a/chats/session-current.jsonl",
        include_bytes!("fixtures/gemini/current.jsonl"),
    );
    let mut current = MessagePager::open(
        SessionSourceKind::Gemini,
        current_root.path(),
        current_state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let current_messages = collect_pages(&mut current, &current_key, 1);
    assert_eq!(
        current_messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        vec![
            "checkpoint question",
            "checkpoint answer",
            "tool call final"
        ]
    );
    assert_eq!(current_messages[0].timestamp, "2026-07-26T12:00:01Z");
    assert_eq!(current_messages[2].tools.len(), 1);
    assert_eq!(current_messages[2].tools[0].name.as_deref(), Some("shell"));
    let current_debug = format!("{current_messages:?}");
    for secret in [
        "must disappear",
        "old snapshot",
        "CURRENT-TOOL-INPUT-MUST-NOT-ESCAPE",
        "CURRENT-TOOL-OUTPUT-MUST-NOT-ESCAPE",
    ] {
        assert!(!current_debug.contains(secret));
    }

    let (legacy_root, legacy_state, legacy_key) = setup_gemini(
        "tmp/project-a/chats/session-legacy.json",
        include_bytes!("fixtures/gemini/legacy.json"),
    );
    let mut legacy = MessagePager::open(
        SessionSourceKind::Gemini,
        legacy_root.path(),
        legacy_state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let legacy_messages = collect_pages(&mut legacy, &legacy_key, 1);
    assert_eq!(legacy_messages.len(), 2);
    assert_eq!(legacy_messages[0].content, "legacy question");
    assert_eq!(legacy_messages[1].content, "legacy answer");
    assert_eq!(
        legacy_messages[1].tools[0].name.as_deref(),
        Some("read_file")
    );
    let legacy_debug = format!("{legacy_messages:?}");
    assert!(!legacy_debug.contains("LEGACY-TOOL-INPUT-MUST-NOT-ESCAPE"));
    assert!(!legacy_debug.contains("LEGACY-TOOL-OUTPUT-MUST-NOT-ESCAPE"));
}

#[test]
fn actual_utf8_item_boundary_is_exact_and_oversized_item_advances() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let timestamp = "2026-07-26T12:00:01Z";
    let base = "界".repeat((64 * 1024 - timestamp.len()) / "界".len());
    let filler = "a".repeat(64 * 1024 - timestamp.len() - base.len());
    let exact_text = base + &filler;
    let exact_item_bytes = exact_text.len() + timestamp.len();
    let lines = format!(
        "{{\"type\":\"user\",\"uuid\":\"exact\",\"sessionId\":\"claude-boundary\",\"timestamp\":\"{timestamp}\",\"message\":{{\"role\":\"user\",\"content\":{}}}}}\n\
         {{\"type\":\"user\",\"uuid\":\"over\",\"sessionId\":\"claude-boundary\",\"timestamp\":\"{timestamp}\",\"message\":{{\"role\":\"user\",\"content\":{}}}}}\n\
         {{\"type\":\"user\",\"uuid\":\"after\",\"sessionId\":\"claude-boundary\",\"timestamp\":\"{timestamp}\",\"message\":{{\"role\":\"user\",\"content\":\"after\"}}}}\n",
        serde_json::to_string(&exact_text).unwrap(),
        serde_json::to_string(&(exact_text.clone() + "a")).unwrap(),
    );
    write_bytes(
        root.path(),
        "projects/project-a/boundary.jsonl",
        lines.as_bytes(),
    );
    let mut scanner = ClaudeScanner::open(
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let source_key = scanner
        .scan(NOW, SessionScanControl::default())
        .unwrap()
        .sources[0]
        .source_key
        .clone();
    let mut pager = MessagePager::open(
        SessionSourceKind::Claude,
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let first = pager
        .page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: exact_item_bytes,
                cursor: None,
            },
        )
        .unwrap();
    assert_eq!(first.messages[0].content.len(), exact_text.len());
    let second = pager
        .page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: exact_item_bytes,
                cursor: first.next_cursor.clone(),
            },
        )
        .unwrap();
    assert_eq!(second.messages[0].content, "after");
}

#[test]
fn cursor_is_bound_to_key_kind_source_and_paging_is_read_only() {
    let (claude_root, claude_state, claude_key) = setup_claude();
    write_bytes(
        claude_root.path(),
        "projects/project-b/other.jsonl",
        br#"{"type":"user","uuid":"other","sessionId":"claude-other","timestamp":"2026-07-26T12:00:01Z","message":{"role":"user","content":"other"}}
"#,
    );
    let mut scanner = ClaudeScanner::open(
        claude_root.path(),
        claude_state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let summary = scanner
        .scan("2026-07-26T12:31:00Z", SessionScanControl::default())
        .unwrap();
    let other_key = summary
        .sources
        .iter()
        .find(|source| source.source_key != claude_key)
        .unwrap()
        .source_key
        .clone();
    let source_before = snapshot_files(claude_root.path());
    let state_before = snapshot_files(claude_state.path());
    let mut pager = MessagePager::open(
        SessionSourceKind::Claude,
        claude_root.path(),
        claude_state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let page = pager
        .page(
            &claude_key,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: 64,
                cursor: None,
            },
        )
        .unwrap();
    let cursor = page.next_cursor.unwrap();
    assert!(matches!(
        pager.page(
            &other_key,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: 64,
                cursor: Some(cursor.clone()),
            }
        ),
        Err(MessagePagerError::InvalidCursor)
    ));
    let mut wrong_key = MessagePager::open(
        SessionSourceKind::Claude,
        claude_root.path(),
        claude_state.path().join("state.sqlite3"),
        [0x51; 32],
    )
    .unwrap();
    assert!(matches!(
        wrong_key.page(
            &claude_key,
            MessagePageRequest {
                maximum_messages: 1,
                maximum_utf8_bytes: 64,
                cursor: Some(cursor),
            }
        ),
        Err(MessagePagerError::InvalidCursor)
    ));
    assert_eq!(snapshot_files(claude_root.path()), source_before);
    assert_eq!(snapshot_files(claude_state.path()), state_before);
}

#[test]
fn legacy_deep_cursor_skips_large_prior_payload_without_exposing_it() {
    let mut messages = String::new();
    let large_payload = "0,".repeat(80_000);
    for index in 0..50 {
        let extras = if index == 0 {
            format!(
                ",\"displayContent\":\"DISPLAY-MUST-NOT-ESCAPE\",\
                 \"thoughts\":[\"THINKING-MUST-NOT-ESCAPE\"],\
                 \"toolCalls\":[{{\"name\":\"shell\",\"args\":{{\"large\":[{large_payload}0]}},\
                 \"result\":{{\"secret\":\"RESULT-MUST-NOT-ESCAPE\"}}}}]"
            )
        } else {
            String::new()
        };
        messages.push_str(&format!(
            "{{\"id\":\"message-{index}\",\"timestamp\":\"2026-07-26T12:00:01Z\",\
             \"type\":\"user\",\"content\":\"message {index}\"{extras}}},"
        ));
    }
    let document = format!(
        "{{\"sessionId\":\"deep-cursor\",\"startTime\":\"2026-07-26T12:00:00Z\",\
         \"messages\":[{messages}\
         {{\"id\":\"final\",\"timestamp\":\"2026-07-26T12:00:02+05:45\",\
         \"type\":\"gemini\",\"content\":\"अन्तिम उत्तर\"}}]}}"
    );
    let (root, state, source_key) =
        setup_gemini("tmp/project-a/chats/session-deep.json", document.as_bytes());
    let mut pager = MessagePager::open(
        SessionSourceKind::Gemini,
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let first = pager
        .page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 40,
                maximum_utf8_bytes: MAX_MESSAGE_PAGE_UTF8_BYTES,
                cursor: None,
            },
        )
        .unwrap();
    assert_eq!(first.messages.len(), 40);
    let later = pager
        .page(
            &source_key,
            MessagePageRequest {
                maximum_messages: 20,
                maximum_utf8_bytes: MAX_MESSAGE_PAGE_UTF8_BYTES,
                cursor: first.next_cursor.clone(),
            },
        )
        .unwrap();
    assert_eq!(later.messages.last().unwrap().content, "अन्तिम उत्तर");
    assert_eq!(
        later.messages.last().unwrap().timestamp,
        "2026-07-26T06:15:02Z"
    );
    let debug = format!("{first:?}{later:?}");
    for secret in [
        "DISPLAY-MUST-NOT-ESCAPE",
        "THINKING-MUST-NOT-ESCAPE",
        "RESULT-MUST-NOT-ESCAPE",
    ] {
        assert!(!debug.contains(secret));
    }
}

#[test]
fn codex_tool_events_expose_only_role_type_and_name() {
    let root = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write_bytes(
        root.path(),
        "sessions/2026/07/26/tools.jsonl",
        r#"{"timestamp":"2026-07-26T00:00:00Z","type":"session_meta","payload":{"id":"codex-tools"}}
{"timestamp":"2026-07-26T12:45:01+12:45","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"任意语言 प्रश्न"}]}}
{"timestamp":"2026-07-26T12:45:02+12:45","type":"response_item","payload":{"type":"function_call","name":"read_file","arguments":"CODEX-TOOL-INPUT-MUST-NOT-ESCAPE"}}
{"timestamp":"2026-07-26T12:45:03+12:45","type":"response_item","payload":{"type":"function_call_output","name":"read_file","output":"CODEX-TOOL-OUTPUT-MUST-NOT-ESCAPE"}}
"#
        .as_bytes(),
    );
    let mut scanner = CodexScanner::open(
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let source_key = scanner.scan(NOW, ScanControl::default()).unwrap().sources[0]
        .source_key
        .clone();
    let mut pager = MessagePager::open(
        SessionSourceKind::Codex,
        root.path(),
        state.path().join("state.sqlite3"),
        TEST_DOMAIN_KEY,
    )
    .unwrap();
    let messages = collect_pages(&mut pager, &source_key, 1);
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].content, "任意语言 प्रश्न");
    assert_eq!(messages[1].role, MessageRole::Tool);
    assert_eq!(messages[1].timestamp, "2026-07-26T00:00:02Z");
    assert_eq!(messages[1].tools[0].tool_type, MessageToolType::Call);
    assert_eq!(messages[2].tools[0].tool_type, MessageToolType::Result);
    assert_eq!(messages[2].tools[0].name.as_deref(), Some("read_file"));
    let debug = format!("{messages:?}");
    assert!(!debug.contains("CODEX-TOOL-INPUT-MUST-NOT-ESCAPE"));
    assert!(!debug.contains("CODEX-TOOL-OUTPUT-MUST-NOT-ESCAPE"));
}
