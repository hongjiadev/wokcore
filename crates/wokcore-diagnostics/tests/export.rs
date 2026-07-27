use std::fs;

use sha2::{Digest, Sha256};
use tempfile::tempdir;
use wokcore_diagnostics::{
    event::{
        BuildIdentity, CapabilityVersion, DiagnosticComponent, DiagnosticEventCode,
        DiagnosticEventDraft, DiagnosticLevel, EventId, GitCommit, UtcTimestamp, WokcoreVersion,
    },
    export::{
        CapabilitySummary, ExportBuildIdentity, ExportCapability, ExportConfiguration,
        ExportCoordinator, ExportError, ExportPlatformCategory, ExportRedactionCounters,
        ExportSelection, LeakCanarySet, ResourceSummary, StableErrorSummary, StableExportErrorCode,
        StableExportErrorSource, SupportPackage, export_support_package, prepare_support_package,
        verify_support_package,
    },
    recorder::{DiagnosticRecorder, RecordOutcome},
    ring::{MAX_PAGE_BYTES, PageDirection, PageRequest},
    segment::{DurableBatch, SegmentWriter},
    snapshot::{
        FailureSnapshot, SnapshotCause, SnapshotConfigurationSummary, SnapshotCorrelation,
        SnapshotErrorCode, SnapshotLifecycleState, SnapshotRecorder, SnapshotRedactionSummary,
        SnapshotRequest, SnapshotRequestOutcome, SnapshotResourceState,
    },
};
use wokcore_platform::{
    diagnostics::{DiagnosticDirectory, DiagnosticReadLease},
    sessions::{PinnedExportDestination, SessionRootLease},
};

async fn prepared(identity: u64) -> wokcore_diagnostics::event::PreparedDiagnosticEvent {
    prepared_many(identity, 1).await.pop().unwrap()
}

fn draft(identity: u64) -> DiagnosticEventDraft {
    DiagnosticEventDraft::new(
        EventId::parse(&format!("018f47a2-4c1d-7a8f-9b2d-{identity:012x}")).unwrap(),
        UtcTimestamp::parse("2026-07-26T12:30:00Z").unwrap(),
        DiagnosticLevel::Info,
        DiagnosticComponent::Diagnostics,
        DiagnosticEventCode::RequestCompleted,
        BuildIdentity::new(
            WokcoreVersion::parse("0.1.0").unwrap(),
            GitCommit::parse("0123456789abcdef0123456789abcdef01234567").unwrap(),
            1,
            CapabilityVersion::new(3),
        ),
    )
}

async fn prepared_many(
    first_identity: u64,
    count: usize,
) -> Vec<wokcore_diagnostics::event::PreparedDiagnosticEvent> {
    let (recorder, owner) = DiagnosticRecorder::new();
    let owner_task = tokio::spawn(owner.run());
    for offset in 0..count {
        let identity = first_identity
            .checked_add(u64::try_from(offset).unwrap())
            .unwrap();
        loop {
            match recorder.try_record(Ok(draft(identity))) {
                RecordOutcome::Accepted => break,
                RecordOutcome::DroppedFull => tokio::task::yield_now().await,
                outcome => panic!("unexpected synthetic record outcome: {outcome:?}"),
            }
        }
    }
    let mut events = Vec::with_capacity(count);
    let mut cursor = None;
    while events.len() < count {
        let page = loop {
            match recorder.try_query(
                PageRequest::with_limits(
                    PageDirection::Ascending,
                    cursor,
                    count.saturating_sub(events.len()).min(1_000),
                    MAX_PAGE_BYTES,
                )
                .unwrap(),
            ) {
                Ok(pending) => break pending.wait().await.unwrap(),
                Err(_) => tokio::task::yield_now().await,
            }
        };
        assert!(!page.events().is_empty());
        cursor = page.next_cursor();
        events.extend_from_slice(page.events());
    }
    owner_task.abort();
    events
}

fn persisted_event_leases(root: &std::path::Path) -> Vec<DiagnosticReadLease> {
    let directory = DiagnosticDirectory::open(root).unwrap();
    let mut entries = directory.entries(4_096).unwrap();
    entries.sort_by(|left, right| left.name().cmp(right.name()));
    entries
        .iter()
        .map(|entry| directory.open_read(entry, 4 * 1024 * 1024).unwrap())
        .collect()
}

fn persist_events(
    root: &std::path::Path,
    events: &[wokcore_diagnostics::event::PreparedDiagnosticEvent],
) -> Vec<DiagnosticReadLease> {
    fs::create_dir(root).unwrap();
    let mut writer = SegmentWriter::with_segment_limit(root, 32 * 1024).unwrap();
    for events in events.chunks(128) {
        let mut batch = DurableBatch::new();
        for event in events {
            batch.try_push(event.clone()).unwrap();
        }
        writer.flush(batch).unwrap();
    }
    drop(writer);
    persisted_event_leases(root)
}

fn persist_snapshot(
    root: &std::path::Path,
    events: Vec<wokcore_diagnostics::event::PreparedDiagnosticEvent>,
) {
    let (recorder, mut owner) = SnapshotRecorder::new(root);
    let snapshot = FailureSnapshot::new(
        events,
        SnapshotLifecycleState::Degraded,
        SnapshotResourceState::new(false, false, 0),
        vec![SnapshotErrorCode::UpstreamUnavailable],
        SnapshotRedactionSummary::new(0, 0),
        SnapshotConfigurationSummary::new(true, 7, 4, 3).unwrap(),
    )
    .unwrap();
    assert_eq!(
        recorder.try_request(SnapshotRequest::new(
            SnapshotCause::UpstreamFailure,
            SnapshotCorrelation::from_u128(7),
            snapshot,
            1,
        )),
        SnapshotRequestOutcome::Accepted
    );
    assert!(owner.try_process_next().unwrap().written());
}

fn persist_raw_source(root: &std::path::Path, name: &str, bytes: &[u8]) -> DiagnosticReadLease {
    fs::create_dir_all(root).unwrap();
    let directory = DiagnosticDirectory::open(root).unwrap();
    drop(
        directory
            .create_new(name.as_ref(), bytes, 4 * 1024 * 1024)
            .unwrap(),
    );
    directory
        .open_name_read(name.as_ref(), 4 * 1024 * 1024)
        .unwrap()
}

fn encoded_document(events: &[wokcore_diagnostics::event::PreparedDiagnosticEvent]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for event in events {
        bytes.extend_from_slice(event.encoded());
        bytes.push(b'\n');
    }
    bytes
}

#[derive(Debug)]
struct ZipEntryOffsets {
    name: Vec<u8>,
    local_crc_offset: usize,
    central_crc_offset: usize,
    payload: std::ops::Range<usize>,
}

fn little_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn little_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn zip_entry_offsets(bytes: &[u8]) -> Vec<ZipEntryOffsets> {
    const LOCAL_SIGNATURE: &[u8; 4] = b"PK\x03\x04";
    const CENTRAL_SIGNATURE: &[u8; 4] = b"PK\x01\x02";
    let mut entries = Vec::new();
    let mut offset = 0_usize;
    while bytes.get(offset..offset + 4) == Some(LOCAL_SIGNATURE) {
        let name_len = usize::from(little_u16(&bytes[offset + 26..offset + 28]));
        let extra_len = usize::from(little_u16(&bytes[offset + 28..offset + 30]));
        let size = usize::try_from(little_u32(&bytes[offset + 18..offset + 22])).unwrap();
        let name_start = offset + 30;
        let payload_start = name_start + name_len + extra_len;
        entries.push(ZipEntryOffsets {
            name: bytes[name_start..name_start + name_len].to_vec(),
            local_crc_offset: offset + 14,
            central_crc_offset: usize::MAX,
            payload: payload_start..payload_start + size,
        });
        offset = payload_start + size;
    }
    for entry in &mut entries {
        assert_eq!(
            bytes.get(offset..offset + 4),
            Some(CENTRAL_SIGNATURE.as_slice()),
            "missing central record for {:?}",
            String::from_utf8_lossy(&entry.name)
        );
        let name_len = usize::from(little_u16(&bytes[offset + 28..offset + 30]));
        let extra_len = usize::from(little_u16(&bytes[offset + 30..offset + 32]));
        let comment_len = usize::from(little_u16(&bytes[offset + 32..offset + 34]));
        assert_eq!(
            &bytes[offset + 46..offset + 46 + name_len],
            entry.name.as_slice()
        );
        entry.central_crc_offset = offset + 16;
        offset += 46 + name_len + extra_len + comment_len;
    }
    entries
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn patch_entry_crc(bytes: &mut [u8], entry: &ZipEntryOffsets) {
    let crc = crc32(&bytes[entry.payload.clone()]).to_le_bytes();
    bytes[entry.local_crc_offset..entry.local_crc_offset + 4].copy_from_slice(&crc);
    bytes[entry.central_crc_offset..entry.central_crc_offset + 4].copy_from_slice(&crc);
}

fn configuration() -> ExportConfiguration {
    configuration_with_reverse_order(false)
}

fn configuration_with_reverse_order(reverse: bool) -> ExportConfiguration {
    let mut capabilities = vec![
        ExportCapability::DiagnosticsExport,
        ExportCapability::DiagnosticsRead,
        ExportCapability::SessionsRead,
    ];
    if reverse {
        capabilities.reverse();
    }
    ExportConfiguration::new(
        true,
        7,
        4,
        ExportBuildIdentity::new("0.1.0", "0123456789abcdef0123456789abcdef01234567", 1, 3)
            .unwrap(),
        CapabilitySummary::new(capabilities).unwrap(),
    )
    .unwrap()
}

fn resources() -> ResourceSummary {
    resources_with_reverse_order(false)
}

fn resources_with_reverse_order(reverse: bool) -> ResourceSummary {
    let mut stable_errors = vec![
        StableErrorSummary::new(
            StableExportErrorCode::ResourceLimit,
            vec![
                StableExportErrorSource::Storage,
                StableExportErrorSource::Platform,
            ],
            ExportPlatformCategory::Filesystem,
            2,
        )
        .unwrap(),
        StableErrorSummary::new(
            StableExportErrorCode::UpstreamUnavailable,
            vec![
                StableExportErrorSource::Provider,
                StableExportErrorSource::Protocol,
            ],
            ExportPlatformCategory::Network,
            1,
        )
        .unwrap(),
    ];
    if reverse {
        stable_errors.reverse();
    }
    ResourceSummary::new(
        16 * 1024 * 1024,
        64 * 1024 * 1024,
        3,
        ExportPlatformCategory::Filesystem,
        stable_errors,
        ExportRedactionCounters::new(1, 2, 3, 4, 5, 6),
    )
    .unwrap()
}

async fn assert_support_package_names_rejected_before_publish(names: [&str; 2]) {
    let fixture = tempdir().unwrap();
    let exports = fixture.path().join("exports");
    fs::create_dir(&exports).unwrap();
    let source = encoded_document(&[prepared(43).await]);

    for (case, name) in names.into_iter().enumerate() {
        let diagnostics = fixture.path().join(format!("invalid-index-{case}"));
        let destination = exports.join(format!("invalid-index-{case}.zip"));
        let error = match SupportPackage::new(
            vec![persist_raw_source(&diagnostics, name, &source)],
            configuration(),
            resources(),
            ExportSelection::complete(),
        ) {
            Err(error) => Some(error),
            Ok(mut package) => {
                let coordinator = ExportCoordinator::new();
                let operation = coordinator.try_begin().unwrap();
                let worker = operation.start_worker().unwrap();
                export_support_package(
                    worker,
                    PinnedExportDestination::create(&destination, &[]).unwrap(),
                    &mut package,
                    &LeakCanarySet::new(),
                )
                .unwrap();
                None
            }
        };

        assert!(!destination.exists(), "{name} reached publish");
        assert_eq!(error, Some(ExportError::InvalidInput), "{name}");
    }
}

#[tokio::test]
async fn support_package_rejects_zero_persistent_indexes_before_publish() {
    assert_support_package_names_rejected_before_publish([
        "segment-00000000000000000000.jsonl",
        "snapshot-00000000000000000000.jsonl",
    ])
    .await;
}

#[tokio::test]
async fn support_package_rejects_overflow_persistent_indexes_before_publish() {
    assert_support_package_names_rejected_before_publish([
        "segment-18446744073709551616.jsonl",
        "snapshot-18446744073709551616.jsonl",
    ])
    .await;
}

#[test]
fn support_package_accepts_max_u64_persistent_indexes() {
    let fixture = tempdir().unwrap();
    let diagnostics = fixture.path().join("diagnostics");
    let package = SupportPackage::new(
        vec![
            persist_raw_source(&diagnostics, "segment-18446744073709551615.jsonl", b"{}\n"),
            persist_raw_source(&diagnostics, "snapshot-18446744073709551615.jsonl", b"{}\n"),
        ],
        configuration(),
        resources(),
        ExportSelection::complete(),
    );

    assert!(package.is_ok());
}

#[tokio::test]
async fn export_memory_is_bounded_below_package_size() {
    let fixture = tempdir().unwrap();
    let segments = fixture.path().join("diagnostics");
    let exports = fixture.path().join("exports");
    fs::create_dir(&segments).unwrap();
    fs::create_dir(&exports).unwrap();
    let events = prepared_many(9, 512).await;
    let mut writer = SegmentWriter::new(&segments);
    for events in events.chunks(128) {
        let mut batch = DurableBatch::new();
        for event in events {
            batch.try_push(event.clone()).unwrap();
        }
        writer.flush(batch).unwrap();
    }
    drop(writer);
    let leases = persisted_event_leases(&segments);
    let mut package = SupportPackage::new(
        leases,
        configuration(),
        resources(),
        ExportSelection::complete(),
    )
    .unwrap();
    let coordinator = ExportCoordinator::new();
    let operation = coordinator.try_begin().unwrap();
    let worker = operation.start_worker().unwrap();
    let destination = exports.join("streamed.zip");
    let destination_lease = PinnedExportDestination::create(&destination, &[]).unwrap();

    let stats = export_support_package(
        worker,
        destination_lease,
        &mut package,
        &LeakCanarySet::new(),
    )
    .unwrap();

    verify_support_package(&destination).unwrap();
    assert!(stats.package_bytes() > 128 * 1024);
    assert!(stats.peak_buffer_bytes() <= 160 * 1024);
    assert!(stats.package_bytes() > 2 * stats.peak_buffer_bytes() as u64);
}

#[tokio::test]
async fn export_peak_memory_accounts_for_each_persistent_source() {
    async fn export_with_source_count(source_count: usize) -> (usize, usize) {
        let fixture = tempdir().unwrap();
        let mut diagnostics = fixture.path().join("diagnostics");
        for index in 0..6 {
            diagnostics = diagnostics.join(format!("resident-allocation-{index:02}"));
        }
        let exports = fixture.path().join("exports");
        fs::create_dir_all(&diagnostics).unwrap();
        fs::create_dir(&exports).unwrap();
        let source = encoded_document(&[prepared(9).await]);
        for index in 1..=source_count {
            fs::write(
                diagnostics.join(format!("segment-{index:020}.jsonl")),
                &source,
            )
            .unwrap();
        }
        let leases = persisted_event_leases(&diagnostics);
        let minimum_accounted_source_bytes = leases
            .iter()
            .try_fold(
                std::mem::size_of::<DiagnosticReadLease>() * leases.capacity(),
                |total, lease| total.checked_add(lease.resident_allocation_bytes().unwrap()),
            )
            .unwrap()
            .checked_add(source.len().saturating_sub(1).checked_mul(8).unwrap())
            .unwrap();
        let mut package = SupportPackage::new(
            leases,
            configuration(),
            resources(),
            ExportSelection::complete(),
        )
        .unwrap();
        let coordinator = ExportCoordinator::new();
        let operation = coordinator.try_begin().unwrap();
        let worker = operation.start_worker().unwrap();
        let destination = exports.join("package.zip");
        let stats = export_support_package(
            worker,
            PinnedExportDestination::create(&destination, &[]).unwrap(),
            &mut package,
            &LeakCanarySet::new(),
        )
        .unwrap();
        verify_support_package(&destination).unwrap();
        (stats.peak_buffer_bytes(), minimum_accounted_source_bytes)
    }

    let (one_source, one_source_minimum) = export_with_source_count(1).await;
    let (many_sources, many_source_minimum) = export_with_source_count(128).await;
    let minimum_lease_slot_growth = std::mem::size_of::<DiagnosticReadLease>() * (128_usize - 1);
    let source_growth = many_sources.saturating_sub(one_source);

    assert!(one_source >= one_source_minimum);
    assert!(many_sources >= many_source_minimum);
    assert!(
        source_growth > minimum_lease_slot_growth,
        "source-proportional cursors, heap entries, and leases must contribute to the peak"
    );
}

#[test]
fn export_peak_memory_accounts_for_deep_destination_and_many_session_roots() {
    fn export_peak(destination: PinnedExportDestination) -> usize {
        let mut package = SupportPackage::new(
            Vec::new(),
            configuration(),
            resources(),
            ExportSelection::complete(),
        )
        .unwrap();
        let coordinator = ExportCoordinator::new();
        let operation = coordinator.try_begin().unwrap();
        let worker = operation.start_worker().unwrap();
        export_support_package(worker, destination, &mut package, &LeakCanarySet::new())
            .unwrap()
            .peak_buffer_bytes()
    }

    let fixture = tempdir().unwrap();
    let shallow_parent = fixture.path().join("shallow");
    fs::create_dir(&shallow_parent).unwrap();
    let shallow_destination =
        PinnedExportDestination::create(shallow_parent.join("package.zip"), &[]).unwrap();
    let shallow_resident_bytes = shallow_destination.resident_allocation_bytes().unwrap();
    let shallow_peak = export_peak(shallow_destination);

    let mut deep_parent = fixture.path().join("deep");
    for index in 0..8 {
        deep_parent = deep_parent.join(format!("destination-resident-{index:02}"));
    }
    fs::create_dir_all(&deep_parent).unwrap();
    let mut session_roots = Vec::new();
    for index in 0..32 {
        let root = fixture
            .path()
            .join("session-roots")
            .join(format!("session-resident-{index:02}"))
            .join("nested");
        fs::create_dir_all(&root).unwrap();
        session_roots.push(SessionRootLease::open(root).unwrap());
    }
    let session_root_references = session_roots.iter().collect::<Vec<_>>();
    let deep_destination =
        PinnedExportDestination::create(deep_parent.join("package.zip"), &session_root_references)
            .unwrap();
    let deep_resident_bytes = deep_destination.resident_allocation_bytes().unwrap();
    drop(session_root_references);
    drop(session_roots);
    let deep_peak = export_peak(deep_destination);
    let resident_growth = deep_resident_bytes
        .checked_sub(shallow_resident_bytes)
        .unwrap();

    assert!(resident_growth > 0);
    assert_eq!(
        deep_peak.checked_sub(shallow_peak),
        Some(resident_growth),
        "reported peak growth must equal exact pinned destination resident growth"
    );
}

#[tokio::test]
async fn streamed_zip_has_deterministic_manifest_entries_and_checksums() {
    let fixture = tempdir().unwrap();
    let sessions = fixture.path().join("sessions");
    let mixed_diagnostics = fixture.path().join("mixed-diagnostics");
    let pure_diagnostics = fixture.path().join("pure-diagnostics");
    let exports = fixture.path().join("exports");
    fs::create_dir(&sessions).unwrap();
    fs::create_dir(&exports).unwrap();
    let session = SessionRootLease::open(&sessions).unwrap();
    let events = prepared_many(1, 200).await;
    let mixed_segment_events = events
        .iter()
        .enumerate()
        .filter(|(index, _)| !matches!(index, 1 | 3))
        .map(|(_, event)| event.clone())
        .collect::<Vec<_>>();
    let _mixed_segment_leases = persist_events(&mixed_diagnostics, &mixed_segment_events);
    persist_snapshot(
        &mixed_diagnostics,
        vec![events[1].clone(), events[2].clone(), events[3].clone()],
    );
    let mut package = SupportPackage::new(
        persisted_event_leases(&mixed_diagnostics),
        configuration(),
        resources(),
        ExportSelection::complete(),
    )
    .unwrap();
    let canaries = LeakCanarySet::new();
    let coordinator = ExportCoordinator::new();

    let first_operation = coordinator.try_begin().unwrap();
    let first_worker = first_operation.start_worker().unwrap();
    let first = exports.join("first.zip");
    let first_destination = PinnedExportDestination::create(&first, &[&session]).unwrap();
    let first_stats =
        export_support_package(first_worker, first_destination, &mut package, &canaries).unwrap();
    drop(first_operation);
    verify_support_package(&first).unwrap();

    let second_operation = coordinator.try_begin().unwrap();
    let second_worker = second_operation.start_worker().unwrap();
    let second = exports.join("second.zip");
    let mut reversed_leases = persist_events(&pure_diagnostics, &events);
    reversed_leases.reverse();
    let mut logically_identical_package = SupportPackage::new(
        reversed_leases,
        configuration_with_reverse_order(true),
        resources_with_reverse_order(true),
        ExportSelection::complete(),
    )
    .unwrap();
    let second_destination = PinnedExportDestination::create(&second, &[&session]).unwrap();
    let second_stats = export_support_package(
        second_worker,
        second_destination,
        &mut logically_identical_package,
        &canaries,
    )
    .unwrap();
    drop(second_operation);
    verify_support_package(&second).unwrap();

    assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
    assert!(
        first_stats.peak_buffer_bytes() <= 160 * 1024,
        "first peak was {} bytes",
        first_stats.peak_buffer_bytes()
    );
    assert!(
        second_stats.peak_buffer_bytes() <= 160 * 1024,
        "second peak was {} bytes",
        second_stats.peak_buffer_bytes()
    );
    assert_eq!(first_stats.package_bytes(), second_stats.package_bytes());
    assert!(first_stats.peak_buffer_bytes() > second_stats.peak_buffer_bytes());

    let original = fs::read(&first).unwrap();
    let event_name = original
        .windows(b"events.jsonl".len())
        .position(|window| window == b"events.jsonl")
        .unwrap();
    let mut payload_mutation = original.clone();
    payload_mutation[event_name + b"events.jsonl".len()] ^= 1;
    let payload_path = exports.join("payload-mutated.zip");
    fs::write(&payload_path, payload_mutation).unwrap();
    assert_eq!(
        verify_support_package(payload_path).unwrap_err(),
        ExportError::InvalidPackage
    );

    let central_signature = [0x50, 0x4b, 0x01, 0x02];
    let central = original
        .windows(central_signature.len())
        .position(|window| window == central_signature)
        .unwrap();
    let mut central_mutation = original.clone();
    central_mutation[central + 16] ^= 1;
    let central_path = exports.join("central-mutated.zip");
    fs::write(&central_path, central_mutation).unwrap();
    assert_eq!(
        verify_support_package(central_path).unwrap_err(),
        ExportError::InvalidPackage
    );

    let mut local_timestamp_mutation = original.clone();
    local_timestamp_mutation[12] ^= 1;
    let local_timestamp_path = exports.join("local-timestamp-mutated.zip");
    fs::write(&local_timestamp_path, local_timestamp_mutation).unwrap();
    assert_eq!(
        verify_support_package(local_timestamp_path).unwrap_err(),
        ExportError::InvalidPackage
    );

    let mut trailing_mutation = original;
    trailing_mutation.push(0);
    let trailing_path = exports.join("trailing-mutated.zip");
    fs::write(&trailing_path, trailing_mutation).unwrap();
    assert_eq!(
        verify_support_package(trailing_path).unwrap_err(),
        ExportError::InvalidPackage
    );

    let unordered_root = fixture.path().join("unordered-diagnostics");
    let unordered = encoded_document(&[events[4].clone(), events[3].clone()]);
    let mut unordered_package = SupportPackage::new(
        vec![persist_raw_source(
            &unordered_root,
            "segment-00000000000000000001.jsonl",
            &unordered,
        )],
        configuration(),
        resources(),
        ExportSelection::complete(),
    )
    .unwrap();
    let unordered_operation = coordinator.try_begin().unwrap();
    let unordered_worker = unordered_operation.start_worker().unwrap();
    let unordered_target = exports.join("unordered.zip");
    let unordered_destination =
        PinnedExportDestination::create(&unordered_target, &[&session]).unwrap();
    assert_eq!(
        export_support_package(
            unordered_worker,
            unordered_destination,
            &mut unordered_package,
            &LeakCanarySet::new(),
        )
        .unwrap_err(),
        ExportError::InvalidInput
    );
    assert!(!unordered_target.exists());
    drop(unordered_operation);

    let conflict_root = fixture.path().join("conflict-diagnostics");
    let first_conflict = encoded_document(&[events[0].clone()]);
    let mut second_conflict = events[1].encoded().to_vec();
    let marker = br#""sequence":""#;
    let marker_start = second_conflict
        .windows(marker.len())
        .position(|window| window == marker)
        .unwrap();
    let sequence_start = marker_start + marker.len();
    second_conflict[sequence_start..sequence_start + 20].copy_from_slice(b"00000000000000000001");
    second_conflict.push(b'\n');
    let mut conflict_package = SupportPackage::new(
        vec![
            persist_raw_source(
                &conflict_root,
                "segment-00000000000000000001.jsonl",
                &first_conflict,
            ),
            persist_raw_source(
                &conflict_root,
                "segment-00000000000000000002.jsonl",
                &second_conflict,
            ),
        ],
        configuration(),
        resources(),
        ExportSelection::complete(),
    )
    .unwrap();
    let conflict_operation = coordinator.try_begin().unwrap();
    let conflict_worker = conflict_operation.start_worker().unwrap();
    let conflict_target = exports.join("conflict.zip");
    let conflict_destination =
        PinnedExportDestination::create(&conflict_target, &[&session]).unwrap();
    assert_eq!(
        export_support_package(
            conflict_worker,
            conflict_destination,
            &mut conflict_package,
            &LeakCanarySet::new(),
        )
        .unwrap_err(),
        ExportError::InvalidInput
    );
    assert!(!conflict_target.exists());
    drop(conflict_operation);
}

#[tokio::test]
async fn export_leak_scan_rejects_every_sensitive_category_before_publish() {
    let fixture = tempdir().unwrap();
    let sessions = fixture.path().join("sessions");
    let diagnostics = fixture.path().join("diagnostics");
    let exports = fixture.path().join("exports");
    fs::create_dir(&sessions).unwrap();
    fs::create_dir(&exports).unwrap();
    let session = SessionRootLease::open(&sessions).unwrap();
    let events = vec![prepared(2).await];
    let mut package = SupportPackage::new(
        persist_events(&diagnostics, &events),
        configuration(),
        resources(),
        ExportSelection::complete(),
    )
    .unwrap();
    let mut leak = LeakCanarySet::new();
    leak.push(b"manifest_version").unwrap();
    let forbidden = [
        br#"{"AuThOrIzAtIoN":"Bearer synthetic"}"#.as_slice(),
        br#"{"safe":"C:\\Users\\synthetic"}"#,
        br#"{"safe":"\\\\server\\share"}"#,
        br#"{"safe":"\\\\?\\C:\\synthetic"}"#,
        br#"{"safe":"\\\\.\\pipe\\synthetic"}"#,
        br#"{"safe":"/var/tmp/synthetic"}"#,
        br#"{"safe":"file:///tmp/synthetic"}"#,
        br#"{"safe":"https://user:pass@example.invalid/path?q=value"}"#,
        br#"{"safe":"%2Ftmp%2Fsynthetic"}"#,
    ];
    for candidate in forbidden {
        assert_eq!(
            LeakCanarySet::new()
                .validate_candidate(candidate)
                .unwrap_err(),
            ExportError::LeakDetected
        );
    }
    for (canary, safe_neighbor) in [
        ("quote\"credential", "quote'credential"),
        ("backslash\\credential", "backslash/credential"),
        ("control\n\t\r\u{0001}credential", "control-x-credential"),
        ("percent%credential", "percent-credential"),
    ] {
        let mut canaries = LeakCanarySet::new();
        canaries.push(canary.as_bytes()).unwrap();
        let encoded = serde_json::to_vec(&serde_json::json!({ "safe": canary })).unwrap();
        assert_eq!(
            canaries.validate_candidate(&encoded).unwrap_err(),
            ExportError::LeakDetected,
            "missed JSON encoding for {canary:?}"
        );
        let safe = serde_json::to_vec(&serde_json::json!({ "safe": safe_neighbor })).unwrap();
        canaries.validate_candidate(&safe).unwrap();
    }
    let maximum_escaped_canary = "\u{0001}".repeat(256);
    let mut bounded_canaries = LeakCanarySet::new();
    for _ in 0..16 {
        bounded_canaries
            .push(maximum_escaped_canary.as_bytes())
            .unwrap();
    }
    assert_eq!(
        bounded_canaries
            .push(maximum_escaped_canary.as_bytes())
            .unwrap_err(),
        ExportError::InvalidInput
    );
    let mut multilingual = LeakCanarySet::new();
    multilingual.push("synthetic-机密-🔐".as_bytes()).unwrap();
    assert_eq!(
        multilingual
            .validate_candidate("prefix synthetic-机密-🔐 suffix".as_bytes())
            .unwrap_err(),
        ExportError::LeakDetected
    );
    let coordinator = ExportCoordinator::new();

    let operation = coordinator.try_begin().unwrap();
    assert_eq!(coordinator.try_begin().unwrap_err(), ExportError::Busy);
    let worker = operation.start_worker().unwrap();
    let leaked = exports.join("leaked.zip");
    let leaked_destination = PinnedExportDestination::create(&leaked, &[&session]).unwrap();
    assert_eq!(
        export_support_package(worker, leaked_destination, &mut package, &leak).unwrap_err(),
        ExportError::LeakDetected
    );
    assert!(!leaked.exists());
    drop(operation);

    let cancelled_operation = coordinator.try_begin().unwrap();
    let cancelled_worker = cancelled_operation.start_worker().unwrap();
    drop(cancelled_operation);
    let cancelled = exports.join("cancelled.zip");
    let cancelled_destination = PinnedExportDestination::create(&cancelled, &[&session]).unwrap();
    assert_eq!(
        export_support_package(
            cancelled_worker,
            cancelled_destination,
            &mut package,
            &LeakCanarySet::new(),
        )
        .unwrap_err(),
        ExportError::Cancelled
    );
    assert!(!cancelled.exists());
    assert!(coordinator.try_begin().is_ok());
    drop(coordinator.try_begin().unwrap());

    let partial_operation = coordinator.try_begin().unwrap();
    let (partial_owner, partial_worker) = partial_operation.split().unwrap();
    let partial = exports.join("partial.zip");
    let partial_destination = PinnedExportDestination::create(&partial, &[&session]).unwrap();
    let prepared = prepare_support_package(
        partial_worker,
        partial_destination,
        &mut package,
        &LeakCanarySet::new(),
    )
    .unwrap();
    let mut body = prepared.into_body(partial_owner).unwrap();
    assert!(!body.read_next(127).unwrap().unwrap().is_empty());
    assert!(!partial.exists());
    drop(body);
    assert!(!partial.exists());
    let success_operation = coordinator.try_begin().unwrap();
    let (success_owner, success_worker) = success_operation.split().unwrap();
    let streamed = exports.join("streamed-temporary.zip");
    let streamed_destination = PinnedExportDestination::create(&streamed, &[&session]).unwrap();
    let prepared = prepare_support_package(
        success_worker,
        streamed_destination,
        &mut package,
        &LeakCanarySet::new(),
    )
    .unwrap();
    let mut body = prepared.into_body(success_owner).unwrap();
    let mut streamed_bytes = Vec::new();
    while let Some(chunk) = body.read_next(4_096).unwrap() {
        streamed_bytes.extend_from_slice(&chunk);
    }
    body.finish().unwrap();
    assert!(!streamed.exists());
    let verifier_fixture = exports.join("streamed-verifier-fixture.zip");
    fs::write(&verifier_fixture, streamed_bytes).unwrap();
    verify_support_package(&verifier_fixture).unwrap();
    fs::remove_file(verifier_fixture).unwrap();
    assert!(coordinator.try_begin().is_ok());
    assert_eq!(
        fs::read_dir(&exports)
            .unwrap()
            .filter_map(Result::ok)
            .count(),
        0
    );
}

#[tokio::test]
async fn final_archive_canary_gate_scans_numbers_zip_headers_and_chunk_boundaries() {
    let fixture = tempdir().unwrap();
    let diagnostics = fixture.path().join("diagnostics");
    let exports = fixture.path().join("exports");
    fs::create_dir(&exports).unwrap();
    let events = prepared_many(1, 100).await;
    let mut package = SupportPackage::new(
        persist_events(&diagnostics, &events),
        configuration(),
        resources(),
        ExportSelection::complete(),
    )
    .unwrap();
    let coordinator = ExportCoordinator::new();

    let baseline_operation = coordinator.try_begin().unwrap();
    let baseline_worker = baseline_operation.start_worker().unwrap();
    let baseline = exports.join("baseline.zip");
    export_support_package(
        baseline_worker,
        PinnedExportDestination::create(&baseline, &[]).unwrap(),
        &mut package,
        &LeakCanarySet::new(),
    )
    .unwrap();
    drop(baseline_operation);
    let baseline_bytes = fs::read(&baseline).unwrap();
    let boundary = 16 * 1024;
    assert!(baseline_bytes.len() > boundary + 64);
    let cross_chunk = baseline_bytes[boundary - 64..boundary + 64].to_vec();

    for (index, canary) in [
        br#""event_count":100"#.to_vec(),
        b"PK\x03\x04".to_vec(),
        cross_chunk,
    ]
    .into_iter()
    .enumerate()
    {
        let mut canaries = LeakCanarySet::new();
        canaries.push(&canary).unwrap();
        let operation = coordinator.try_begin().unwrap();
        let worker = operation.start_worker().unwrap();
        let destination = exports.join(format!("leaked-{index}.zip"));
        assert_eq!(
            export_support_package(
                worker,
                PinnedExportDestination::create(&destination, &[]).unwrap(),
                &mut package,
                &canaries,
            )
            .unwrap_err(),
            ExportError::LeakDetected
        );
        assert!(!destination.exists());
        drop(operation);
    }
}

#[tokio::test]
async fn verifier_rejects_reordered_events_with_recomputed_crc_and_sha256() {
    let fixture = tempdir().unwrap();
    let diagnostics = fixture.path().join("diagnostics");
    let exports = fixture.path().join("exports");
    fs::create_dir(&exports).unwrap();
    let events = prepared_many(1, 2).await;
    let mut package = SupportPackage::new(
        persist_events(&diagnostics, &events),
        configuration(),
        resources(),
        ExportSelection::complete(),
    )
    .unwrap();
    let coordinator = ExportCoordinator::new();
    let operation = coordinator.try_begin().unwrap();
    let worker = operation.start_worker().unwrap();
    let original = exports.join("ordered.zip");
    export_support_package(
        worker,
        PinnedExportDestination::create(&original, &[]).unwrap(),
        &mut package,
        &LeakCanarySet::new(),
    )
    .unwrap();
    drop(operation);
    verify_support_package(&original).unwrap();

    let mut reordered = fs::read(&original).unwrap();
    let entries = zip_entry_offsets(&reordered);
    assert_eq!(entries.len(), 5);
    assert_eq!(entries[1].name, b"events.jsonl");
    assert_eq!(entries[4].name, b"checksums.sha256");
    let events_entry = &entries[1];
    let event_payload = &reordered[events_entry.payload.clone()];
    let first_end = event_payload
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap()
        + 1;
    let second_end = first_end
        + event_payload[first_end..]
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap()
        + 1;
    assert_eq!(second_end, event_payload.len());
    assert_eq!(first_end, second_end - first_end);
    let first = event_payload[..first_end].to_vec();
    let second = event_payload[first_end..second_end].to_vec();
    let payload_start = events_entry.payload.start;
    reordered[payload_start..payload_start + first_end].copy_from_slice(&second);
    reordered[payload_start + first_end..payload_start + second_end].copy_from_slice(&first);
    patch_entry_crc(&mut reordered, events_entry);

    let event_hash = Sha256::digest(&reordered[events_entry.payload.clone()]);
    let event_hash_hex = format!("{event_hash:x}");
    let checksums_entry = &entries[4];
    let checksums = &reordered[checksums_entry.payload.clone()];
    let event_line_marker = b"  events.jsonl\n";
    let marker = checksums
        .windows(event_line_marker.len())
        .position(|window| window == event_line_marker)
        .unwrap();
    let hash_start = checksums_entry.payload.start + marker - 64;
    reordered[hash_start..hash_start + 64].copy_from_slice(event_hash_hex.as_bytes());
    patch_entry_crc(&mut reordered, checksums_entry);

    assert_eq!(
        little_u32(&reordered[events_entry.local_crc_offset..events_entry.local_crc_offset + 4]),
        crc32(&reordered[events_entry.payload.clone()])
    );
    assert_eq!(
        &reordered[hash_start..hash_start + 64],
        event_hash_hex.as_bytes()
    );
    assert_eq!(
        little_u32(
            &reordered[checksums_entry.central_crc_offset..checksums_entry.central_crc_offset + 4]
        ),
        crc32(&reordered[checksums_entry.payload.clone()])
    );

    let reordered_path = exports.join("reordered-with-valid-integrity.zip");
    fs::write(&reordered_path, reordered).unwrap();
    assert_eq!(
        verify_support_package(reordered_path).unwrap_err(),
        ExportError::InvalidPackage
    );
}

#[test]
fn export_debug_errors_and_verifier_are_canary_free_and_bounded() {
    let canary = "synthetic-sensitive-value";
    for error in [
        ExportError::Busy,
        ExportError::Cancelled,
        ExportError::InvalidInput,
        ExportError::Boundary,
        ExportError::Io,
        ExportError::LeakDetected,
        ExportError::InvalidPackage,
        ExportError::Limit,
    ] {
        assert!(!format!("{error:?}").contains(canary));
        assert!(!error.to_string().contains(canary));
    }
    for (field, invalid) in [
        ("retention_days", 0),
        ("retention_days", 8),
        ("segment_mib", 0),
        ("segment_mib", 5),
    ] {
        let mut encoded = serde_json::to_value(configuration()).unwrap();
        encoded[field] = invalid.into();
        assert!(serde_json::from_value::<ExportConfiguration>(encoded).is_err());
    }
    let fixture = tempdir().unwrap();
    let malformed = fixture.path().join("malformed.zip");
    fs::write(&malformed, [0x50, 0x4b, 0x03, 0x04, 0xff]).unwrap();
    assert_eq!(
        verify_support_package(malformed).unwrap_err(),
        ExportError::InvalidPackage
    );

    let coordinator = ExportCoordinator::new();
    let old_operation = coordinator.try_begin().unwrap();
    let old_worker = old_operation.start_worker().unwrap();
    drop(old_worker);

    let current_operation = coordinator.try_begin().unwrap();
    let current_worker = current_operation.start_worker().unwrap();
    assert_eq!(old_operation.start_worker().unwrap_err(), ExportError::Busy);
    assert_eq!(coordinator.try_begin().unwrap_err(), ExportError::Busy);

    drop(current_operation);
    drop(current_worker);
    drop(old_operation);
    assert!(coordinator.try_begin().is_ok());
}

#[test]
fn leak_canaries_scan_decoded_json_strings_and_keys_exactly() {
    assert_eq!(
        LeakCanarySet::new()
            .validate_candidate(br#"{"au\u0074horization":"safe"}"#)
            .unwrap_err(),
        ExportError::LeakDetected
    );
    assert_eq!(
        LeakCanarySet::new()
            .validate_candidate(br#"{"safe":"C:\u005cUsers\u005csynthetic"}"#)
            .unwrap_err(),
        ExportError::LeakDetected
    );

    let mut alternate_escape = LeakCanarySet::new();
    alternate_escape.push(b"quote\"credential").unwrap();
    assert_eq!(
        alternate_escape
            .validate_candidate(br#"{"safe":"quote\u0022credential"}"#)
            .unwrap_err(),
        ExportError::LeakDetected
    );
    alternate_escape
        .validate_candidate(br#"{"safe":"quote\\u0022credential"}"#)
        .unwrap();

    let mut control_canary = LeakCanarySet::new();
    control_canary.push(b"line\nbreak\x01").unwrap();
    assert_eq!(
        control_canary
            .validate_candidate(br#"{"safe":"line\nbreak\u0001"}"#)
            .unwrap_err(),
        ExportError::LeakDetected
    );
    control_canary
        .validate_candidate(br#"{"safe":"line\\nbreak\\u0001"}"#)
        .unwrap();

    let mut key_canary = LeakCanarySet::new();
    key_canary.push(b"synthetic-key").unwrap();
    assert_eq!(
        key_canary
            .validate_candidate(br#"{"synthe\u0074ic-key":"safe"}"#)
            .unwrap_err(),
        ExportError::LeakDetected
    );
    key_canary
        .validate_candidate(br#"{"synthetic-keighbor":"safe"}"#)
        .unwrap();
}

#[tokio::test]
async fn export_rejects_noncanonical_persistent_event_encoding() {
    let fixture = tempdir().unwrap();
    let diagnostics = fixture.path().join("diagnostics");
    let exports = fixture.path().join("exports");
    fs::create_dir(&exports).unwrap();
    let event = prepared(41).await;
    let mut alternate_encoding = event.encoded().to_vec();
    let component = br#""component":"diagnostics""#;
    let component_start = alternate_encoding
        .windows(component.len())
        .position(|window| window == component)
        .unwrap();
    alternate_encoding.splice(
        component_start..component_start + component.len(),
        br#""component":"diagnosti\u0063s""#.iter().copied(),
    );
    alternate_encoding.push(b'\n');
    let mut package = SupportPackage::new(
        vec![persist_raw_source(
            &diagnostics,
            "segment-00000000000000000001.jsonl",
            &alternate_encoding,
        )],
        configuration(),
        resources(),
        ExportSelection::complete(),
    )
    .unwrap();
    let coordinator = ExportCoordinator::new();
    let operation = coordinator.try_begin().unwrap();
    let worker = operation.start_worker().unwrap();
    let destination = exports.join("noncanonical.zip");

    assert_eq!(
        export_support_package(
            worker,
            PinnedExportDestination::create(&destination, &[]).unwrap(),
            &mut package,
            &LeakCanarySet::new(),
        )
        .unwrap_err(),
        ExportError::InvalidInput
    );
    assert!(!destination.exists());
}

#[tokio::test]
async fn export_rejects_decoded_canary_in_persistent_event_before_publish() {
    let fixture = tempdir().unwrap();
    let diagnostics = fixture.path().join("diagnostics");
    let exports = fixture.path().join("exports");
    fs::create_dir(&exports).unwrap();
    let event = prepared(42).await;
    let document = encoded_document(std::slice::from_ref(&event));
    let mut package = SupportPackage::new(
        vec![persist_raw_source(
            &diagnostics,
            "segment-00000000000000000001.jsonl",
            &document,
        )],
        configuration(),
        resources(),
        ExportSelection::complete(),
    )
    .unwrap();
    let mut canaries = LeakCanarySet::new();
    canaries
        .push(b"018f47a2-4c1d-7a8f-9b2d-00000000002a")
        .unwrap();
    let coordinator = ExportCoordinator::new();
    let operation = coordinator.try_begin().unwrap();
    let worker = operation.start_worker().unwrap();
    let destination = exports.join("event-canary.zip");

    assert_eq!(
        export_support_package(
            worker,
            PinnedExportDestination::create(&destination, &[]).unwrap(),
            &mut package,
            &canaries,
        )
        .unwrap_err(),
        ExportError::LeakDetected
    );
    assert!(!destination.exists());
}

#[tokio::test]
async fn dropping_export_at_each_partial_stage_cancels_and_releases_admission() {
    let coordinator = ExportCoordinator::new();
    let active = coordinator.try_begin().unwrap();
    let mut waiting = Box::pin(coordinator.begin());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), &mut waiting)
            .await
            .is_err()
    );
    drop(waiting);
    assert_eq!(coordinator.try_begin().unwrap_err(), ExportError::Busy);
    drop(active);

    let next = tokio::time::timeout(std::time::Duration::from_secs(1), coordinator.begin())
        .await
        .unwrap()
        .unwrap();
    let (owner, worker) = next.split().unwrap();
    drop(owner);
    assert_eq!(coordinator.try_begin().unwrap_err(), ExportError::Busy);
    drop(worker);
    assert!(coordinator.try_begin().is_ok());
}

#[tokio::test]
async fn pinned_create_new_export_cleans_only_its_owned_temporary_on_races() {
    let fixture = tempdir().unwrap();
    let sessions = fixture.path().join("sessions");
    let diagnostics = fixture.path().join("diagnostics");
    let exports = fixture.path().join("exports");
    fs::create_dir(&sessions).unwrap();
    fs::create_dir(&exports).unwrap();
    let session = SessionRootLease::open(&sessions).unwrap();
    let events = vec![prepared(3).await];
    let mut package = SupportPackage::new(
        persist_events(&diagnostics, &events),
        configuration(),
        resources(),
        ExportSelection::complete(),
    )
    .unwrap();
    let coordinator = ExportCoordinator::new();
    let foreign = exports.join("foreign.zip");
    fs::write(&foreign, b"foreign").unwrap();
    assert!(PinnedExportDestination::create(&foreign, &[&session]).is_err());
    assert_eq!(fs::read(&foreign).unwrap(), b"foreign");
    assert!(PinnedExportDestination::create(sessions.join("rejected.zip"), &[&session]).is_err());

    let finished_operation = coordinator.try_begin().unwrap();
    let finished_worker = finished_operation.start_worker().unwrap();
    let finished_path = exports.join("finished.zip");
    let finished_destination =
        PinnedExportDestination::create(&finished_path, &[&session]).unwrap();
    export_support_package(
        finished_worker,
        finished_destination,
        &mut package,
        &LeakCanarySet::new(),
    )
    .unwrap();

    let current_operation = coordinator.try_begin().unwrap();
    let current_worker = current_operation.start_worker().unwrap();
    assert_eq!(coordinator.try_begin().unwrap_err(), ExportError::Busy);

    drop(current_operation);
    drop(current_worker);
    drop(finished_operation);
    assert!(coordinator.try_begin().is_ok());
}
