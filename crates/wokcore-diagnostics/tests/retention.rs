use std::{
    fs::{self, File, FileTimes},
    io::Write,
    time::{Duration, SystemTime},
};

use tempfile::tempdir;
use wokcore_diagnostics::event::DiagnosticEvent;
use wokcore_diagnostics::retention::{
    ClosedSegmentLease, MAX_CLOSED_SEGMENT_BYTES, MAX_RETENTION_AGE, RETENTION_PAGE_ENTRIES,
    RetentionError, RetentionManager, RetentionPolicy, RetentionTrigger,
};
use wokcore_platform::diagnostics::DiagnosticDirectory;

fn canonical_event(sequence: u64) -> Vec<u8> {
    let encoded = format!(
        concat!(
            "{{\"schema_version\":1,\"sequence\":\"{0:020}\",",
            "\"event_id\":\"018f47a2-4c1d-7a8f-9b2d-{0:012x}\",",
            "\"occurred_at\":\"2026-07-26T12:30:00Z\",\"level\":\"info\",",
            "\"component\":\"diagnostics\",\"code\":\"request_completed\",",
            "\"correlations\":null,\"build\":{{\"wokcore_version\":\"0.1.0\",",
            "\"git_commit\":\"0123456789abcdef0123456789abcdef01234567\",",
            "\"api_major\":1,\"capability_version\":3}},\"provider\":null,",
            "\"decision\":null,\"measurements\":null,\"error\":null,",
            "\"diagnostic_drop\":null,\"summaries\":[],\"redaction_counts\":{{",
            "\"authorization_values_removed\":0,\"cookie_values_removed\":0,",
            "\"body_values_removed\":0,\"path_values_removed\":0,",
            "\"token_values_removed\":0,\"credential_values_removed\":0}}}}\n"
        ),
        sequence
    )
    .into_bytes();
    DiagnosticEvent::decode(&encoded[..encoded.len() - 1]).unwrap();
    encoded
}

fn create_segment(root: &std::path::Path, index: u64, modified: SystemTime) -> usize {
    let path = root.join(format!("segment-{index:020}.jsonl"));
    let encoded = canonical_event(index);
    let mut file = File::create(path).unwrap();
    file.write_all(&encoded).unwrap();
    file.set_times(FileTimes::new().set_modified(modified))
        .unwrap();
    encoded.len()
}

#[test]
fn retention_runs_only_after_startup_and_rotation() {
    let directory = tempdir().unwrap();
    let missing = directory.path().join("missing");
    let manager = RetentionManager::new(&missing);
    let now = SystemTime::now();

    for trigger in [
        RetentionTrigger::IdleTick,
        RetentionTrigger::Query,
        RetentionTrigger::BatchFlush,
    ] {
        let report = manager.enforce(trigger, now, &[]).unwrap();
        assert!(report.noop());
        assert!(!missing.exists());
    }
}

#[test]
fn retention_enforces_seven_days_then_64mib_oldest_first() {
    assert_eq!(MAX_RETENTION_AGE, Duration::from_secs(7 * 24 * 60 * 60));
    assert_eq!(MAX_CLOSED_SEGMENT_BYTES, 64 * 1024 * 1024);

    let directory = tempdir().unwrap();
    let root = directory.path().join("diagnostics");
    fs::create_dir(&root).unwrap();
    let now = SystemTime::now();
    let segment_bytes = create_segment(&root, 1, now - MAX_RETENTION_AGE - Duration::from_secs(1));
    create_segment(&root, 2, now - MAX_RETENTION_AGE);
    create_segment(&root, 3, now - Duration::from_secs(1));
    fs::write(root.join("active.jsonl"), b"active").unwrap();
    fs::write(root.join("foreign.txt"), b"foreign").unwrap();

    let manager = RetentionManager::with_policy(
        &root,
        RetentionPolicy::with_limits(MAX_RETENTION_AGE, u64::try_from(segment_bytes * 2).unwrap())
            .unwrap(),
    );
    let report = manager
        .enforce(RetentionTrigger::Startup, now, &[])
        .unwrap();
    assert_eq!(report.removed_files(), 1);
    assert!(!root.join("segment-00000000000000000001.jsonl").exists());
    assert!(root.join("segment-00000000000000000002.jsonl").exists());
    assert!(root.join("segment-00000000000000000003.jsonl").exists());
    assert!(root.join("active.jsonl").exists());
    assert!(root.join("foreign.txt").exists());
}

#[test]
fn retention_preserves_active_and_leased_segments_while_trimming_size() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("diagnostics");
    fs::create_dir(&root).unwrap();
    let now = SystemTime::now();
    let segment_bytes = create_segment(&root, 1, now - Duration::from_secs(3));
    create_segment(&root, 2, now - Duration::from_secs(2));
    create_segment(&root, 3, now - Duration::from_secs(1));
    fs::write(root.join("active.jsonl"), vec![b'a'; 32]).unwrap();

    let directory_capability = DiagnosticDirectory::open(&root).unwrap();
    let leased_entry = directory_capability
        .entries(8)
        .unwrap()
        .into_iter()
        .find(|entry| entry.name() == "segment-00000000000000000001.jsonl")
        .unwrap();
    let lease = ClosedSegmentLease::open(&directory_capability, &leased_entry).unwrap();
    let manager = RetentionManager::with_policy(
        &root,
        RetentionPolicy::with_limits(MAX_RETENTION_AGE, u64::try_from(segment_bytes).unwrap())
            .unwrap(),
    );
    let report = manager
        .enforce(RetentionTrigger::Rotation, now, &[&lease])
        .unwrap();
    assert_eq!(report.removed_files(), 1);
    assert!(root.join("segment-00000000000000000001.jsonl").exists());
    assert!(!root.join("segment-00000000000000000002.jsonl").exists());
    assert!(root.join("segment-00000000000000000003.jsonl").exists());
    assert!(root.join("active.jsonl").exists());
}

#[test]
fn retention_never_owns_or_deletes_canonical_segment_index_zero() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("zero-segment");
    fs::create_dir(&root).unwrap();
    let zero = root.join("segment-00000000000000000000.jsonl");
    let zero_bytes = canonical_event(1);
    fs::write(&zero, &zero_bytes).unwrap();
    fs::write(
        root.join("segment-00000000000000000001.jsonl"),
        canonical_event(2),
    )
    .unwrap();
    let manager = RetentionManager::with_policy(
        &root,
        RetentionPolicy::with_limits(Duration::ZERO, 0).unwrap(),
    );

    manager
        .enforce(RetentionTrigger::Startup, SystemTime::now(), &[])
        .unwrap();
    assert_eq!(fs::read(&zero).unwrap(), zero_bytes);
}

#[test]
fn retention_rejects_hardlinked_owned_names_without_deleting_either_link() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("diagnostics");
    fs::create_dir(&root).unwrap();
    let segment = root.join("segment-00000000000000000001.jsonl");
    let foreign = root.join("foreign-hardlink.bin");
    fs::write(&segment, b"safe").unwrap();
    fs::hard_link(&segment, &foreign).unwrap();
    fs::write(root.join("segment-00000000000000000002.jsonl"), b"active").unwrap();
    let manager = RetentionManager::with_policy(
        &root,
        RetentionPolicy::with_limits(Duration::ZERO, 0).unwrap(),
    );

    assert!(
        manager
            .enforce(RetentionTrigger::Startup, SystemTime::now(), &[])
            .is_err()
    );
    assert!(segment.exists());
    assert!(foreign.exists());
}

#[test]
fn retention_keeps_its_original_directory_lease_after_parent_replacement() {
    let fixture = tempdir().unwrap();
    let root = fixture.path().join("diagnostics");
    let moved = fixture.path().join("moved");
    fs::create_dir(&root).unwrap();
    create_segment(&root, 1, SystemTime::UNIX_EPOCH);
    create_segment(&root, 2, SystemTime::now());
    let manager = RetentionManager::new(&root);
    manager
        .enforce(RetentionTrigger::Startup, SystemTime::now(), &[])
        .unwrap();

    if fs::rename(&root, &moved).is_ok() {
        fs::create_dir(&root).unwrap();
        create_segment(&root, 1, SystemTime::UNIX_EPOCH);
        create_segment(&root, 2, SystemTime::now());
        assert!(
            manager
                .enforce(RetentionTrigger::Rotation, SystemTime::now(), &[])
                .is_err()
        );
        assert!(root.join("segment-00000000000000000001.jsonl").exists());
        assert!(root.join("segment-00000000000000000002.jsonl").exists());
    }
}

#[test]
fn retention_pages_through_more_than_4096_owned_segments_with_bounded_state() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("paged");
    fs::create_dir(&root).unwrap();
    for index in 1..=4_101 {
        fs::write(
            root.join(format!("segment-{index:020}.jsonl")),
            canonical_event(index),
        )
        .unwrap();
    }
    let manager = RetentionManager::with_policy(
        &root,
        RetentionPolicy::with_limits(MAX_RETENTION_AGE, 0).unwrap(),
    );

    let report = manager
        .enforce(RetentionTrigger::Startup, SystemTime::now(), &[])
        .unwrap();
    assert_eq!(
        report.pages_scanned(),
        3 * 4_101_usize.div_ceil(RETENTION_PAGE_ENTRIES)
    );
    assert_eq!(report.removed_files(), 4_100);
    assert!(!root.join("segment-00000000000000000001.jsonl").exists());
    assert_eq!(
        fs::read_dir(&root).unwrap().filter_map(Result::ok).count(),
        1
    );
    assert!(root.join("segment-00000000000000004101.jsonl").exists());
}

#[test]
fn size_retention_scans_512_candidates_in_three_linear_passes() {
    const SEGMENT_COUNT: u64 = 512;

    let directory = tempdir().unwrap();
    let root = directory.path().join("linear-size");
    fs::create_dir(&root).unwrap();
    let now = SystemTime::now();
    for index in 1..=SEGMENT_COUNT {
        create_segment(&root, index, now);
    }
    let manager = RetentionManager::with_policy(
        &root,
        RetentionPolicy::with_limits(MAX_RETENTION_AGE, 0).unwrap(),
    );

    let report = manager
        .enforce(RetentionTrigger::Startup, now, &[])
        .unwrap();

    let pages_per_pass = usize::try_from(SEGMENT_COUNT)
        .unwrap()
        .div_ceil(RETENTION_PAGE_ENTRIES);
    assert_eq!(report.pages_scanned(), pages_per_pass * 3);
    assert_eq!(report.removed_files(), 511);
    assert_eq!(
        fs::read_dir(&root).unwrap().filter_map(Result::ok).count(),
        1
    );
    assert!(root.join("segment-00000000000000000512.jsonl").exists());
}

#[test]
fn corrupt_canonical_segment_aborts_zero_retention_without_deleting_valid_segments() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("corrupt-canonical");
    fs::create_dir(&root).unwrap();
    let first = root.join("segment-00000000000000000001.jsonl");
    let corrupt = root.join("segment-00000000000000000002.jsonl");
    let active = root.join("segment-00000000000000000003.jsonl");
    let first_bytes = canonical_event(1);
    let corrupt_bytes = b"{\"invalid\":true}\n".to_vec();
    let active_bytes = canonical_event(3);
    fs::write(&first, &first_bytes).unwrap();
    fs::write(&corrupt, &corrupt_bytes).unwrap();
    fs::write(&active, &active_bytes).unwrap();
    let manager = RetentionManager::with_policy(
        &root,
        RetentionPolicy::with_limits(Duration::ZERO, 0).unwrap(),
    );

    assert_eq!(
        manager.enforce(RetentionTrigger::Startup, SystemTime::now(), &[]),
        Err(RetentionError::UnsafeBoundary)
    );
    assert_eq!(fs::read(first).unwrap(), first_bytes);
    assert_eq!(fs::read(corrupt).unwrap(), corrupt_bytes);
    assert_eq!(fs::read(active).unwrap(), active_bytes);
}

#[test]
fn nonmonotonic_canonical_sequence_aborts_zero_retention_without_deleting_anything() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("nonmonotonic-canonical");
    fs::create_dir(&root).unwrap();
    let first = root.join("segment-00000000000000000001.jsonl");
    let regressed = root.join("segment-00000000000000000002.jsonl");
    let active = root.join("segment-00000000000000000003.jsonl");
    let first_bytes = canonical_event(2);
    let regressed_bytes = canonical_event(1);
    let active_bytes = canonical_event(3);
    fs::write(&first, &first_bytes).unwrap();
    fs::write(&regressed, &regressed_bytes).unwrap();
    fs::write(&active, &active_bytes).unwrap();
    let manager = RetentionManager::with_policy(
        &root,
        RetentionPolicy::with_limits(Duration::ZERO, 0).unwrap(),
    );

    assert_eq!(
        manager.enforce(RetentionTrigger::Startup, SystemTime::now(), &[]),
        Err(RetentionError::UnsafeBoundary)
    );
    assert_eq!(fs::read(first).unwrap(), first_bytes);
    assert_eq!(fs::read(regressed).unwrap(), regressed_bytes);
    assert_eq!(fs::read(active).unwrap(), active_bytes);
}

#[test]
fn explicit_active_index_must_be_the_highest_canonical_segment() {
    let directory = tempdir().unwrap();
    let root = directory.path().join("stale-active-index");
    fs::create_dir(&root).unwrap();
    let mut originals = Vec::new();
    for index in 1..=3 {
        let path = root.join(format!("segment-{index:020}.jsonl"));
        let bytes = canonical_event(index);
        fs::write(&path, &bytes).unwrap();
        originals.push((path, bytes));
    }
    let manager = RetentionManager::with_policy(
        &root,
        RetentionPolicy::with_limits(Duration::ZERO, 0).unwrap(),
    );

    assert_eq!(
        manager.enforce_with_active(RetentionTrigger::Rotation, SystemTime::now(), &[], Some(1),),
        Err(RetentionError::UnsafeBoundary)
    );
    for (path, bytes) in originals {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}
