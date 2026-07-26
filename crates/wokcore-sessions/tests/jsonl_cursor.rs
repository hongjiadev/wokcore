use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::SystemTime,
};

use tempfile::TempDir;
use wokcore_platform::sessions::SessionRootLease;
use wokcore_sessions::{
    cursor::{
        JsonlCursor, JsonlRecord, JsonlRecordStatus, MAX_JSONL_BATCH_INPUT_BYTES,
        MAX_JSONL_LINE_BYTES,
    },
    discovery::{DiscoveryLimits, SessionLocation, discover_codex_sessions},
};

fn create_live(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let directory = root.join("sessions/2026/07/26");
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

fn open(root: &Path, relative: &str) -> wokcore_platform::sessions::SessionFile {
    SessionRootLease::open(root)
        .unwrap()
        .open_file(relative, u64::MAX)
        .unwrap()
}

#[test]
fn discovery_is_deterministic_bounded_and_excludes_malformed_layouts() {
    let root = TempDir::new().unwrap();
    create_live(root.path(), "会话-b.jsonl", b"{}\n");
    create_live(root.path(), "a.jsonl", b"{}\n");
    fs::write(root.path().join("sessions/2026/07/26/no.txt"), b"{}\n").unwrap();
    fs::create_dir_all(root.path().join("sessions/not-a-year/07/26")).unwrap();
    fs::write(
        root.path().join("sessions/not-a-year/07/26/ignored.jsonl"),
        b"{}\n",
    )
    .unwrap();
    fs::create_dir_all(root.path().join("sessions/2026/07/26/deeper")).unwrap();
    fs::write(
        root.path().join("sessions/2026/07/26/deeper/ignored.jsonl"),
        b"{}\n",
    )
    .unwrap();
    fs::create_dir_all(root.path().join("archived_sessions")).unwrap();
    fs::write(root.path().join("archived_sessions/归档.jsonl"), b"{}\n").unwrap();

    let lease = SessionRootLease::open(root.path()).unwrap();
    let discovered = discover_codex_sessions(&lease, DiscoveryLimits::default()).expect("discover");
    let locations = discovered
        .iter()
        .map(|item| (item.location(), item.file_name().to_owned()))
        .collect::<Vec<_>>();
    assert_eq!(
        locations,
        vec![
            (SessionLocation::Live, "a.jsonl".to_owned()),
            (SessionLocation::Live, "会话-b.jsonl".to_owned()),
            (SessionLocation::Archive, "归档.jsonl".to_owned()),
        ]
    );

    let error = discover_codex_sessions(
        &lease,
        DiscoveryLimits {
            maximum_entries_per_directory: 1,
            maximum_total_sessions: 8,
            ..DiscoveryLimits::default()
        },
    )
    .unwrap_err();
    assert_eq!(error.stable_code(), "session_discovery_limit");
}

#[test]
fn live_archive_duplicate_identity_is_emitted_once() {
    let root = TempDir::new().unwrap();
    let live = create_live(root.path(), "same.jsonl", b"{}\n");
    fs::create_dir(root.path().join("archived_sessions")).unwrap();
    let archived = root.path().join("archived_sessions/same.jsonl");
    fs::hard_link(live, archived).unwrap();

    let lease = SessionRootLease::open(root.path()).unwrap();
    let discovered = discover_codex_sessions(&lease, DiscoveryLimits::default()).unwrap();
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].location(), SessionLocation::Live);
}

#[test]
fn cursor_commits_complete_bytes_and_replays_half_line() {
    let root = TempDir::new().unwrap();
    let path = create_live(root.path(), "cursor.jsonl", b"{\"n\":1}\n{\"n\":");
    let mut file = open(root.path(), "sessions/2026/07/26/cursor.jsonl");
    let cursor = JsonlCursor::new(0, 0);
    let first = cursor.scan(&mut file).unwrap();
    assert_eq!(first.complete_byte_offset, 8);
    assert_eq!(first.next_record_ordinal, 2);
    assert_eq!(first.records.len(), 1);

    let mut writer = fs::OpenOptions::new().append(true).open(path).unwrap();
    writer.write_all(b"2}\n").unwrap();
    writer.flush().unwrap();
    let mut file = open(root.path(), "sessions/2026/07/26/cursor.jsonl");
    let second = JsonlCursor::new(first.complete_byte_offset, first.next_record_ordinal)
        .scan(&mut file)
        .unwrap();
    assert_eq!(second.records[0].ordinal, 2);
    assert_eq!(second.records[0].value()["n"], 2);
}

#[test]
fn invalid_complete_records_are_stable_and_later_progress_is_retained() {
    let root = TempDir::new().unwrap();
    let mut bytes = b"{bad}\n".to_vec();
    bytes.extend_from_slice(&[0xff, b'\n']);
    bytes.extend_from_slice("{\"ok\":\"多语言\"}\n".as_bytes());
    create_live(root.path(), "invalid.jsonl", &bytes);
    let mut file = open(root.path(), "sessions/2026/07/26/invalid.jsonl");
    let scan = JsonlCursor::new(0, 1).scan(&mut file).unwrap();
    assert_eq!(scan.records.len(), 3);
    assert_eq!(scan.records[0].status, JsonlRecordStatus::InvalidJson);
    assert_eq!(scan.records[1].status, JsonlRecordStatus::InvalidUtf8);
    assert_eq!(scan.records[2].status, JsonlRecordStatus::Valid);
    assert_eq!(scan.complete_byte_offset, bytes.len() as u64);
    assert_eq!(scan.next_record_ordinal, 4);
    assert_eq!(
        format!("{:?}", scan.records),
        format!("{:?}", scan.records.clone())
    );
}

#[test]
fn line_limit_is_exact_and_buffer_is_bounded() {
    let root = TempDir::new().unwrap();
    let exact = vec![b' '; MAX_JSONL_LINE_BYTES - 1];
    let mut accepted = exact;
    accepted.push(b'\n');
    create_live(root.path(), "exact.jsonl", &accepted);
    let mut file = open(root.path(), "sessions/2026/07/26/exact.jsonl");
    let result = JsonlCursor::new(0, 1).scan(&mut file).unwrap();
    assert_eq!(result.peak_buffer_bytes, MAX_JSONL_LINE_BYTES);

    let mut oversized = vec![b' '; MAX_JSONL_LINE_BYTES];
    oversized.push(b'\n');
    create_live(root.path(), "large.jsonl", &oversized);
    let mut file = open(root.path(), "sessions/2026/07/26/large.jsonl");
    let error = JsonlCursor::new(0, 1).scan(&mut file).unwrap_err();
    assert_eq!(error.stable_code(), "session_record_too_large");
    assert!(error.peak_buffer_bytes() <= MAX_JSONL_LINE_BYTES + 1);
}

#[test]
fn reads_leave_source_bytes_metadata_and_entries_unchanged() {
    let root = TempDir::new().unwrap();
    let path = create_live(root.path(), "readonly.jsonl", b"{\"safe\":true}\n");
    let directory = path.parent().unwrap();
    let before_bytes = fs::read(&path).unwrap();
    let before_metadata = fs::metadata(&path).unwrap();
    let before_entries = names(directory);
    let before_modified = before_metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);

    let lease = SessionRootLease::open(root.path()).unwrap();
    let discovered = discover_codex_sessions(&lease, DiscoveryLimits::default()).unwrap();
    let mut file = discovered[0].open(&lease, u64::MAX).unwrap();
    let records = JsonlCursor::new(0, 1).scan(&mut file).unwrap();
    assert_eq!(records.records.len(), 1);

    assert_eq!(fs::read(&path).unwrap(), before_bytes);
    let after_metadata = fs::metadata(&path).unwrap();
    assert_eq!(after_metadata.len(), before_metadata.len());
    assert_eq!(
        after_metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        before_modified
    );
    assert_eq!(names(directory), before_entries);
}

fn names(path: &Path) -> Vec<String> {
    let mut names = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn jsonl_record_debug_never_contains_body() {
    let record = JsonlRecord::invalid(7, 99, JsonlRecordStatus::InvalidJson);
    let debug = format!("{record:?}");
    assert!(!debug.contains("prompt"));
    assert!(debug.contains("ordinal"));
}

#[test]
fn cursor_batch_has_a_total_input_bound_even_for_large_valid_records() {
    let root = TempDir::new().unwrap();
    let payload = "x".repeat(MAX_JSONL_BATCH_INPUT_BYTES / 2 + 32);
    let line = serde_json::to_string(&serde_json::json!({"payload": payload})).unwrap();
    let mut bytes = line.as_bytes().to_vec();
    bytes.push(b'\n');
    let first_end = bytes.len() as u64;
    bytes.extend_from_slice(line.as_bytes());
    bytes.push(b'\n');
    create_live(root.path(), "batch.jsonl", &bytes);

    let mut file = open(root.path(), "sessions/2026/07/26/batch.jsonl");
    let first = JsonlCursor::new(0, 1).scan(&mut file).unwrap();
    assert_eq!(first.records.len(), 1);
    assert_eq!(first.complete_byte_offset, first_end);
    assert!(!first.reached_end);
    let second = JsonlCursor::new(first.complete_byte_offset, first.next_record_ordinal)
        .scan(&mut file)
        .unwrap();
    assert_eq!(second.records.len(), 1);
    assert!(second.reached_end);
}

#[test]
fn ordinal_overflow_returns_a_stable_error_instead_of_reusing_an_id() {
    let root = TempDir::new().unwrap();
    create_live(root.path(), "overflow.jsonl", b"{}\n");
    let mut file = open(root.path(), "sessions/2026/07/26/overflow.jsonl");
    let error = JsonlCursor::new(0, u64::MAX).scan(&mut file).unwrap_err();
    assert_eq!(error.stable_code(), "session_cursor_overflow");
}

#[test]
fn discovery_enforces_global_entry_budget_and_real_calendar_days() {
    let root = TempDir::new().unwrap();
    create_live(root.path(), "valid.jsonl", b"{}\n");
    fs::create_dir_all(root.path().join("sessions/2026/02/30")).unwrap();
    fs::write(
        root.path().join("sessions/2026/02/30/invalid.jsonl"),
        b"{}\n",
    )
    .unwrap();
    fs::create_dir_all(root.path().join("sessions/2026/99/01")).unwrap();
    fs::write(
        root.path().join("sessions/2026/99/01/invalid.jsonl"),
        b"{}\n",
    )
    .unwrap();
    let lease = SessionRootLease::open(root.path()).unwrap();
    let discovered = discover_codex_sessions(&lease, DiscoveryLimits::default()).unwrap();
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].file_name(), "valid.jsonl");

    let error = discover_codex_sessions(
        &lease,
        DiscoveryLimits {
            maximum_total_entries: 2,
            ..DiscoveryLimits::default()
        },
    )
    .unwrap_err();
    assert_eq!(error.stable_code(), "session_discovery_limit");
}
