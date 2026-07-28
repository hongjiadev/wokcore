#[cfg(any(windows, target_vendor = "apple"))]
use std::fs::OpenOptions;
#[cfg(target_vendor = "apple")]
use std::io::Write;
use std::{ffi::OsStr, fs};

use tempfile::tempdir;
use wokcore_platform::diagnostics::{
    DIAGNOSTIC_EXPORT_TEMPORARY_PREFIX, DiagnosticDirectory, DiagnosticStoreError,
    MAX_DIAGNOSTIC_DELETE_TOMBSTONES, MAX_DIAGNOSTIC_WRITE_CHUNK_BYTES,
};

#[cfg(unix)]
fn assert_no_diagnostic_residue(root: &std::path::Path) {
    let residue = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| {
            #[cfg(target_os = "macos")]
            {
                return name != OsStr::new(".wokcore-diagnostic-parent.lock");
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = name;
                true
            }
        })
        .collect::<Vec<_>>();
    assert!(
        residue.is_empty(),
        "unexpected diagnostic residue: {residue:?}"
    );
}

#[test]
fn pinned_diagnostic_file_updates_only_the_opened_identity() {
    let fixture = tempdir().unwrap();
    let root = fixture.path().join("diagnostics");
    fs::create_dir(&root).unwrap();
    let directory = DiagnosticDirectory::open(&root).unwrap();
    let mut file = directory
        .create_new(OsStr::new("active.jsonl"), b"abc", 64)
        .unwrap();
    file.append(b"def").unwrap();
    assert_eq!(file.read_range(0, 64).unwrap(), b"abcdef");
    file.truncate(3).unwrap();
    assert_eq!(file.read_range(0, 64).unwrap(), b"abc");
    drop(file);

    assert!(
        directory
            .create_new(OsStr::new("active.jsonl"), b"replacement", 64)
            .is_err()
    );
    assert_eq!(fs::read(root.join("active.jsonl")).unwrap(), b"abc");
}

#[test]
fn hardlinks_and_links_are_rejected_without_modifying_their_targets() {
    let fixture = tempdir().unwrap();
    let root = fixture.path().join("diagnostics");
    fs::create_dir(&root).unwrap();
    let owned = root.join("segment-00000000000000000001.jsonl");
    let hardlink = fixture.path().join("foreign-hardlink");
    fs::write(&owned, b"owned").unwrap();
    fs::hard_link(&owned, &hardlink).unwrap();
    let directory = DiagnosticDirectory::open(&root).unwrap();
    assert!(directory.entries(8).is_err());
    assert_eq!(fs::read(&owned).unwrap(), b"owned");
    assert_eq!(fs::read(&hardlink).unwrap(), b"owned");

    let foreign = fixture.path().join("foreign");
    fs::write(&foreign, b"foreign").unwrap();
    let link = root.join("active.jsonl");
    create_file_link(&foreign, &link);
    assert!(
        directory
            .open_name_update(OsStr::new("active.jsonl"), 64)
            .is_err()
    );
    assert_eq!(fs::read(foreign).unwrap(), b"foreign");
}

#[test]
fn replacing_the_parent_invalidates_the_lease_before_mutation() {
    let fixture = tempdir().unwrap();
    let root = fixture.path().join("diagnostics");
    let moved = fixture.path().join("moved");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("active.jsonl"), b"owned").unwrap();
    let directory = DiagnosticDirectory::open(&root).unwrap();
    fs::rename(&root, &moved).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join("active.jsonl"), b"foreign").unwrap();

    assert!(directory.revalidate().is_err());
    assert!(
        directory
            .create_new(OsStr::new("new.jsonl"), b"safe", 64)
            .is_err()
    );
    assert_eq!(fs::read(root.join("active.jsonl")).unwrap(), b"foreign");
    assert!(!root.join("new.jsonl").exists());
    assert_eq!(fs::read(moved.join("active.jsonl")).unwrap(), b"owned");
}

#[test]
fn reopening_an_already_secure_directory_does_not_invalidate_open_files() {
    let fixture = tempdir().unwrap();
    let root = fixture.path().join("diagnostics");
    fs::create_dir(&root).unwrap();
    let first_directory = DiagnosticDirectory::open(&root).unwrap();
    let mut file = first_directory
        .create_new(OsStr::new("active.jsonl"), b"first", 64)
        .unwrap();

    let second_directory = DiagnosticDirectory::open(&root).unwrap();
    file.append(b"-second").unwrap();
    assert_eq!(file.read_range(0, 64).unwrap(), b"first-second");
    assert!(
        second_directory
            .entries(8)
            .unwrap()
            .iter()
            .any(|entry| entry.name() == "active.jsonl")
    );
}

#[test]
fn read_and_update_leases_expose_only_diagnostic_capabilities() {
    let fixture = tempdir().unwrap();
    let root = fixture.path().join("diagnostics");
    fs::create_dir(&root).unwrap();
    let directory = DiagnosticDirectory::open(&root).unwrap();
    let created = directory
        .create_new(OsStr::new("segment.jsonl"), b"safe", 64)
        .unwrap();
    let created_identity = created.identity();
    drop(created);

    let entry = directory.entries(4).unwrap().pop().unwrap();
    assert_eq!(entry.name(), OsStr::new("segment.jsonl"));
    assert_eq!(entry.len(), 4);
    assert_eq!(entry.identity(), created_identity);

    let mut reader = directory.open_read(&entry, 64).unwrap();
    assert_eq!(reader.name(), entry.name());
    assert_eq!(reader.len(), entry.len());
    assert_eq!(reader.identity(), entry.identity());
    assert_eq!(reader.read_range(1, 2).unwrap(), b"af");

    let mut updater = directory.open_update(&entry, 64).unwrap();
    updater.append(b"r").unwrap();
    assert_eq!(updater.read_range(0, 64).unwrap(), b"safer");
}

#[test]
fn diagnostics_errors_are_static_and_value_free() {
    let variants = [
        DiagnosticStoreError::UnsafePath,
        DiagnosticStoreError::EnumerationLimitExceeded,
        DiagnosticStoreError::SizeLimitExceeded,
        DiagnosticStoreError::CleanupLimitExceeded,
        DiagnosticStoreError::Changed,
        DiagnosticStoreError::Unavailable,
        DiagnosticStoreError::Io,
    ];
    for error in variants {
        assert!(!format!("{error:?}").is_empty());
        assert!(!error.to_string().is_empty());
        assert!(std::error::Error::source(&error).is_none());
    }
}

#[test]
fn reserved_delete_tombstones_are_hidden_and_cannot_be_created_through_the_public_api() {
    const {
        assert!(MAX_DIAGNOSTIC_DELETE_TOMBSTONES > 4_096);
    }
    let fixture = tempdir().unwrap();
    let root = fixture.path().join("diagnostics");
    fs::create_dir(&root).unwrap();
    let tombstone = ".wokcore-diagnostic-delete-00000";
    fs::write(root.join(tombstone), []).unwrap();
    let directory = DiagnosticDirectory::open(&root).unwrap();

    assert!(directory.entries(1).unwrap().is_empty());
    assert!(
        directory
            .entries_page(None, 1)
            .unwrap()
            .entries()
            .is_empty()
    );
    assert_eq!(
        directory
            .create_new(OsStr::new(tombstone), b"foreign", 64)
            .unwrap_err(),
        DiagnosticStoreError::UnsafePath
    );
    assert_eq!(fs::read(root.join(tombstone)).unwrap(), b"");
}

#[test]
fn internal_export_directories_are_hidden_but_unrelated_directories_fail_closed() {
    let fixture = tempdir().unwrap();
    let root = fixture.path().join("diagnostics");
    fs::create_dir(&root).unwrap();
    let temporary = format!("{DIAGNOSTIC_EXPORT_TEMPORARY_PREFIX}owned");
    fs::create_dir(root.join(&temporary)).unwrap();
    fs::write(root.join("segment.jsonl"), b"safe").unwrap();
    let directory = DiagnosticDirectory::open(&root).unwrap();

    let entries = directory.entries(2).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name(), OsStr::new("segment.jsonl"));
    assert_eq!(
        directory
            .create_new(OsStr::new(&temporary), b"foreign", 64)
            .unwrap_err(),
        DiagnosticStoreError::UnsafePath
    );

    fs::create_dir(root.join("unexpected-directory")).unwrap();
    assert_eq!(
        directory.entries(4).unwrap_err(),
        DiagnosticStoreError::UnsafePath
    );
}

#[test]
fn entry_pages_are_bounded_sorted_and_eventually_cover_large_directories() {
    let fixture = tempdir().unwrap();
    let root = fixture.path().join("diagnostics");
    fs::create_dir(&root).unwrap();
    for name in ["09", "01", "07", "03", "05", "02", "08", "04", "06", "00"] {
        fs::write(root.join(name), name.as_bytes()).unwrap();
    }
    let directory = DiagnosticDirectory::open(&root).unwrap();
    assert_eq!(
        directory.entries_page(None, 0).unwrap_err(),
        DiagnosticStoreError::EnumerationLimitExceeded
    );

    let first = directory.entries_page(None, 3).unwrap();
    assert_eq!(
        first
            .entries()
            .iter()
            .map(|entry| entry.name().to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        ["00", "01", "02"]
    );
    assert_eq!(first.next_after(), Some(OsStr::new("02")));

    let second = directory.entries_page(first.next_after(), 3).unwrap();
    assert_eq!(
        second
            .entries()
            .iter()
            .map(|entry| entry.name().to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        ["03", "04", "05"]
    );
    assert_eq!(second.next_after(), Some(OsStr::new("05")));
}

#[test]
fn read_lease_blocks_removal_until_the_lease_is_dropped() {
    let fixture = tempdir().unwrap();
    let root = fixture.path().join("diagnostics");
    fs::create_dir(&root).unwrap();
    let directory = DiagnosticDirectory::open(&root).unwrap();
    drop(
        directory
            .create_new(OsStr::new("leased.jsonl"), b"safe", 64)
            .unwrap(),
    );
    let entry = directory.entries(4).unwrap().pop().unwrap();
    let lease = directory.open_read(&entry, 64).unwrap();

    assert!(directory.remove(&entry).is_err());
    assert_eq!(fs::read(root.join("leased.jsonl")).unwrap(), b"safe");

    drop(lease);
    directory.remove(&entry).unwrap();
    assert!(!root.join("leased.jsonl").exists());
    assert!(directory.entries(1).unwrap().is_empty());
    #[cfg(unix)]
    assert_no_diagnostic_residue(&root);
}

#[test]
fn staged_file_streams_bounded_chunks_and_commits_the_exact_published_handle() {
    let fixture = tempdir().unwrap();
    let root = fixture.path().join("diagnostics");
    fs::create_dir(&root).unwrap();
    let directory = DiagnosticDirectory::open(&root).unwrap();
    let mut staged = directory
        .create_staged(
            OsStr::new("snapshot.bin"),
            2 * MAX_DIAGNOSTIC_WRITE_CHUNK_BYTES as u64,
        )
        .unwrap();
    staged
        .write_chunk(&vec![b'a'; MAX_DIAGNOSTIC_WRITE_CHUNK_BYTES])
        .unwrap();
    staged.write_chunk(b"tail").unwrap();
    assert_eq!(staged.len(), MAX_DIAGNOSTIC_WRITE_CHUNK_BYTES as u64 + 4);
    assert!(!root.join("snapshot.bin").exists());

    let mut published = staged.commit().unwrap();
    assert_eq!(
        published
            .read_range(0, MAX_DIAGNOSTIC_WRITE_CHUNK_BYTES + 8)
            .unwrap(),
        [
            vec![b'a'; MAX_DIAGNOSTIC_WRITE_CHUNK_BYTES],
            b"tail".to_vec()
        ]
        .concat()
    );
    published.append(b"!").unwrap();
    assert_eq!(
        fs::metadata(root.join("snapshot.bin")).unwrap().len(),
        published.len()
    );
}

#[test]
fn staged_file_limits_freeze_failures_and_drop_only_its_owned_temporary() {
    let fixture = tempdir().unwrap();
    let root = fixture.path().join("diagnostics");
    fs::create_dir(&root).unwrap();
    let directory = DiagnosticDirectory::open(&root).unwrap();

    let mut oversized_chunk = directory
        .create_staged(OsStr::new("oversized.bin"), u64::MAX)
        .unwrap();
    assert_eq!(
        oversized_chunk
            .write_chunk(&vec![0; MAX_DIAGNOSTIC_WRITE_CHUNK_BYTES + 1])
            .unwrap_err(),
        DiagnosticStoreError::SizeLimitExceeded
    );
    assert_eq!(
        oversized_chunk.write_chunk(b"later").unwrap_err(),
        DiagnosticStoreError::Io
    );
    assert_eq!(
        oversized_chunk.commit().unwrap_err(),
        DiagnosticStoreError::Io
    );
    assert!(!root.join("oversized.bin").exists());

    let mut cumulative = directory
        .create_staged(OsStr::new("cumulative.bin"), 3)
        .unwrap();
    cumulative.write_chunk(b"abc").unwrap();
    assert_eq!(
        cumulative.write_chunk(b"d").unwrap_err(),
        DiagnosticStoreError::SizeLimitExceeded
    );
    drop(cumulative);
    assert!(!root.join("cumulative.bin").exists());
    assert!(directory.entries(8).unwrap().is_empty());
}

#[cfg(any(windows, target_vendor = "apple"))]
#[test]
fn staged_commit_rejects_an_owned_temporary_changed_outside_the_writer() {
    let fixture = tempdir().unwrap();
    let root = fixture.path().join("diagnostics");
    fs::create_dir(&root).unwrap();
    let directory = DiagnosticDirectory::open(&root).unwrap();
    let mut staged = directory
        .create_staged(OsStr::new("snapshot.bin"), 64)
        .unwrap();
    staged.write_chunk(b"owned").unwrap();
    let temporary = fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name().is_some_and(|name| {
                name != "snapshot.bin" && name != ".wokcore-diagnostic-parent.lock"
            })
        })
        .unwrap();
    let external = OpenOptions::new().append(true).open(temporary);
    #[cfg(windows)]
    {
        assert!(external.is_err());
        staged.commit().unwrap();
        assert_eq!(fs::read(root.join("snapshot.bin")).unwrap(), b"owned");
    }
    #[cfg(target_vendor = "apple")]
    {
        let mut external = external.unwrap();
        external.write_all(b"!").unwrap();
        external.sync_data().unwrap();
        drop(external);
        assert_eq!(staged.commit().unwrap_err(), DiagnosticStoreError::Changed);
        assert!(!root.join("snapshot.bin").exists());
        assert_no_diagnostic_residue(&root);
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn anonymous_staged_file_is_invisible_until_complete_commit() {
    let fixture = tempdir().unwrap();
    let root = fixture.path().join("diagnostics");
    fs::create_dir(&root).unwrap();
    let directory = DiagnosticDirectory::open(&root).unwrap();
    let mut staged = directory
        .create_staged(OsStr::new("snapshot.bin"), 64)
        .unwrap();

    staged.write_chunk(b"complete snapshot").unwrap();
    assert!(fs::read_dir(&root).unwrap().next().is_none());
    assert!(!root.join("snapshot.bin").exists());

    staged.commit().unwrap();

    assert_eq!(
        fs::read(root.join("snapshot.bin")).unwrap(),
        b"complete snapshot"
    );
    let entries = directory.entries(2).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name(), OsStr::new("snapshot.bin"));
}

#[test]
fn staged_commit_fails_closed_after_parent_replacement() {
    let fixture = tempdir().unwrap();
    let root = fixture.path().join("diagnostics");
    let moved = fixture.path().join("moved");
    fs::create_dir(&root).unwrap();
    let directory = DiagnosticDirectory::open(&root).unwrap();
    let mut staged = directory
        .create_staged(OsStr::new("snapshot.bin"), 64)
        .unwrap();
    staged.write_chunk(b"owned").unwrap();

    #[cfg(windows)]
    {
        assert!(fs::rename(&root, &moved).is_err());
        drop(staged);
        assert!(!root.join("snapshot.bin").exists());
        assert!(directory.entries(8).unwrap().is_empty());
    }

    #[cfg(unix)]
    {
        fs::rename(&root, &moved).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join("snapshot.bin"), b"foreign").unwrap();

        assert!(staged.commit().is_err());
        assert_eq!(fs::read(root.join("snapshot.bin")).unwrap(), b"foreign");
        assert!(!moved.join("snapshot.bin").exists());
    }
}

#[cfg(unix)]
fn create_file_link(target: &std::path::Path, link: &std::path::Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(windows)]
fn create_file_link(target: &std::path::Path, link: &std::path::Path) {
    std::os::windows::fs::symlink_file(target, link).unwrap();
}
