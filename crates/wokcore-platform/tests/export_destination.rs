use std::{
    ffi::OsString,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use wokcore_platform::sessions::{
    MAX_PINNED_EXPORT_READ_BYTES, PinnedExportDestination, SessionError, SessionRootLease,
};

#[test]
fn destination_parent_must_exist_and_be_a_verifiable_directory() {
    let fixture = ExportFixture::new();
    let missing = fixture.root.path().join("missing/export.zip");
    assert!(PinnedExportDestination::create(&missing, &[&fixture.session]).is_err());
    assert!(!missing.exists());

    let parent_file = fixture.root.path().join("parent-file");
    fs::write(&parent_file, b"not a directory").unwrap();
    let destination = parent_file.join("export.zip");
    assert!(PinnedExportDestination::create(&destination, &[&fixture.session]).is_err());
    assert_eq!(fs::read(parent_file).unwrap(), b"not a directory");
}

#[test]
fn destination_parent_cannot_be_inside_or_alias_a_session_root() {
    let fixture = ExportFixture::new();
    let nested_parent = fixture.session_path.join("nested");
    fs::create_dir(&nested_parent).unwrap();
    let before = directory_entries(&nested_parent);

    assert!(matches!(
        PinnedExportDestination::create(nested_parent.join("export.zip"), &[&fixture.session]),
        Err(SessionError::UnsafePath)
    ));
    assert_eq!(directory_entries(&nested_parent), before);

    let alias = fixture.root.path().join("session-alias");
    create_directory_link(&fixture.session_path, &alias);
    assert!(matches!(
        PinnedExportDestination::create(alias.join("export.zip"), &[&fixture.session]),
        Err(SessionError::UnsafePath)
    ));
    assert_eq!(
        directory_entries(&fixture.session_path),
        vec![OsString::from("nested")]
    );
}

#[test]
fn target_symlink_reparse_existing_file_and_hardlink_fail_before_temporary_creation() {
    let fixture = ExportFixture::new();
    let external = fixture.root.path().join("external.zip");
    fs::write(&external, b"external").unwrap();

    let symlink_target = fixture.export_parent.join("symlink.zip");
    create_file_link(&external, &symlink_target);
    assert_rejected_without_directory_change(&fixture, &symlink_target);
    assert_eq!(fs::read(&external).unwrap(), b"external");

    let existing_target = fixture.export_parent.join("existing.zip");
    fs::write(&existing_target, b"existing").unwrap();
    assert_rejected_without_directory_change(&fixture, &existing_target);
    assert_eq!(fs::read(&existing_target).unwrap(), b"existing");

    let hardlink_target = fixture.export_parent.join("hardlink.zip");
    fs::hard_link(&external, &hardlink_target).unwrap();
    assert_rejected_without_directory_change(&fixture, &hardlink_target);
    assert_eq!(fs::read(&external).unwrap(), b"external");

    #[cfg(windows)]
    {
        let stream_target = fixture.export_parent.join("external.zip:diagnostics");
        assert_rejected_without_directory_change(&fixture, &stream_target);
        assert_eq!(fs::read(&external).unwrap(), b"external");
    }
}

#[cfg(any(windows, target_vendor = "apple"))]
#[test]
fn named_temporary_is_exclusive_and_drop_removes_only_the_owned_entry() {
    let fixture = ExportFixture::new();
    let decoy = fixture.export_parent.join(".wokcore-export-decoy.tmp");
    fs::write(&decoy, b"decoy").unwrap();
    let target = fixture.export_parent.join("drop.zip");
    let before = directory_entries(&fixture.export_parent);

    {
        let mut destination =
            PinnedExportDestination::create(&target, &[&fixture.session]).unwrap();
        destination.write_all(b"partial export").unwrap();
        assert!(!target.exists());
        let during = directory_entries(&fixture.export_parent);
        assert_eq!(during.len(), before.len() + 1);
        assert!(during.iter().any(|name| {
            name.to_string_lossy().starts_with(".wokcore-export-")
                && name != ".wokcore-export-decoy.tmp"
        }));
    }

    assert_eq!(directory_entries(&fixture.export_parent), before);
    assert_eq!(fs::read(decoy).unwrap(), b"decoy");
    assert!(!target.exists());
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn anonymous_temporary_drop_preserves_foreign_decoy_without_publishing() {
    let fixture = ExportFixture::new();
    let decoy = fixture.export_parent.join(".wokcore-export-decoy.tmp");
    fs::write(&decoy, b"decoy").unwrap();
    let target = fixture.export_parent.join("drop.zip");
    let mut destination = PinnedExportDestination::create(&target, &[&fixture.session]).unwrap();
    destination.write_all(b"partial export").unwrap();
    assert!(!target.exists());

    drop(destination);

    assert_eq!(fs::read(decoy).unwrap(), b"decoy");
    assert!(!target.exists());
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn anonymous_commit_links_owned_contents_without_touching_a_foreign_decoy() {
    let fixture = ExportFixture::new();
    let target = fixture.export_parent.join("anonymous.zip");
    let mut destination = PinnedExportDestination::create(&target, &[&fixture.session]).unwrap();
    destination.write_all(b"owned anonymous contents").unwrap();
    let decoy = fixture.export_parent.join(".wokcore-export-forged.tmp");
    fs::write(&decoy, b"forged replacement").unwrap();

    assert!(!target.exists());
    destination.commit().unwrap();

    assert_eq!(fs::read(&target).unwrap(), b"owned anonymous contents");
    assert_eq!(fs::read(&decoy).unwrap(), b"forged replacement");
}

#[test]
fn commit_is_same_directory_create_new_and_leaves_no_temporary_entry() {
    let fixture = ExportFixture::new();
    let target = fixture.export_parent.join("diagnostics.zip");
    let mut destination = PinnedExportDestination::create(&target, &[&fixture.session]).unwrap();
    destination.write_all(b"complete export").unwrap();

    destination.commit().unwrap();

    assert_eq!(fs::read(&target).unwrap(), b"complete export");
    assert_eq!(
        directory_entries(&fixture.export_parent),
        vec![OsString::from("diagnostics.zip")]
    );
    assert!(directory_entries(&fixture.session_path).is_empty());
}

#[test]
fn owned_temporary_can_be_verified_with_bounded_reads_before_commit() {
    let fixture = ExportFixture::new();
    let target = fixture.export_parent.join("verified.zip");
    let mut destination = PinnedExportDestination::create(&target, &[&fixture.session]).unwrap();
    destination.write_all(b"0123456789").unwrap();

    destination.sync_data().unwrap();
    assert_eq!(destination.len().unwrap(), 10);
    assert_eq!(destination.read_owned_range(3, 4).unwrap(), b"3456");
    assert_eq!(destination.read_owned_range(10, 8).unwrap(), b"");
    assert!(!target.exists());

    destination.commit().unwrap();
    assert_eq!(fs::read(target).unwrap(), b"0123456789");
}

#[cfg(windows)]
#[test]
fn verified_owned_temporary_blocks_external_length_changes() {
    let fixture = ExportFixture::new();
    let target = fixture.export_parent.join("changed-after-verify.zip");
    let mut destination = PinnedExportDestination::create(&target, &[&fixture.session]).unwrap();
    destination.write_all(b"verified").unwrap();
    destination.sync_data().unwrap();
    assert_eq!(destination.read_owned_range(0, 64).unwrap(), b"verified");
    let temporary = fs::read_dir(&fixture.export_parent)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".wokcore-export-"))
        })
        .unwrap();
    assert!(fs::OpenOptions::new().append(true).open(temporary).is_err());
    destination.commit().unwrap();
    assert_eq!(fs::read(target).unwrap(), b"verified");
}

#[test]
fn owned_temporary_reads_enforce_the_hard_chunk_limit_and_freeze_writes() {
    let fixture = ExportFixture::new();
    let target = fixture.export_parent.join("bounded-read.zip");
    let mut destination = PinnedExportDestination::create(&target, &[&fixture.session]).unwrap();
    destination
        .write_all(&vec![b'x'; MAX_PINNED_EXPORT_READ_BYTES + 1])
        .unwrap();

    assert_eq!(
        destination
            .read_owned_range(0, MAX_PINNED_EXPORT_READ_BYTES)
            .unwrap()
            .len(),
        MAX_PINNED_EXPORT_READ_BYTES
    );
    assert!(matches!(
        destination.read_owned_range(0, MAX_PINNED_EXPORT_READ_BYTES + 1),
        Err(SessionError::ReadLimitExceeded)
    ));
    assert!(destination.write_all(b"late").is_err());
}

#[test]
fn raced_target_creation_fails_without_replacement_and_cleans_owned_temporary() {
    let fixture = ExportFixture::new();
    let target = fixture.export_parent.join("raced.zip");
    let mut destination = PinnedExportDestination::create(&target, &[&fixture.session]).unwrap();
    destination.write_all(b"owned temporary").unwrap();
    fs::write(&target, b"raced target").unwrap();

    assert!(matches!(
        destination.commit(),
        Err(SessionError::UnsafePath)
    ));
    assert_eq!(fs::read(&target).unwrap(), b"raced target");
    assert_eq!(
        directory_entries(&fixture.export_parent),
        vec![OsString::from("raced.zip")]
    );
    assert!(directory_entries(&fixture.session_path).is_empty());
}

#[cfg(unix)]
#[test]
fn parent_swap_fails_identity_recheck_and_cleans_only_the_owned_temporary() {
    let fixture = ExportFixture::new();
    let target = fixture.export_parent.join("swapped.zip");
    let mut destination = PinnedExportDestination::create(&target, &[&fixture.session]).unwrap();
    destination.write_all(b"owned temporary").unwrap();
    let moved_parent = fixture.root.path().join("moved-export");
    fs::rename(&fixture.export_parent, &moved_parent).unwrap();
    fs::create_dir(&fixture.export_parent).unwrap();
    fs::write(&target, b"replacement parent target").unwrap();

    assert!(matches!(
        destination.commit(),
        Err(SessionError::UnsafePath)
    ));
    assert_eq!(fs::read(&target).unwrap(), b"replacement parent target");
    assert!(directory_entries(&moved_parent).is_empty());
    assert!(directory_entries(&fixture.session_path).is_empty());
}

fn assert_rejected_without_directory_change(fixture: &ExportFixture, target: &Path) {
    let before = directory_entries(&fixture.export_parent);
    assert!(matches!(
        PinnedExportDestination::create(target, &[&fixture.session]),
        Err(SessionError::UnsafePath)
    ));
    assert_eq!(directory_entries(&fixture.export_parent), before);
}

struct ExportFixture {
    root: tempfile::TempDir,
    session_path: PathBuf,
    session: SessionRootLease,
    export_parent: PathBuf,
}

impl ExportFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let session_path = root.path().join("sessions");
        fs::create_dir(&session_path).unwrap();
        let session = SessionRootLease::open(&session_path).unwrap();
        let export_parent = root.path().join("exports");
        fs::create_dir(&export_parent).unwrap();
        Self {
            root,
            session_path,
            session,
            export_parent,
        }
    }
}

fn directory_entries(path: &Path) -> Vec<OsString> {
    let mut entries = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

#[cfg(unix)]
fn create_directory_link(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(windows)]
fn create_directory_link(target: &Path, link: &Path) {
    std::os::windows::fs::symlink_dir(target, link).unwrap();
}

#[cfg(unix)]
fn create_file_link(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(windows)]
fn create_file_link(target: &Path, link: &Path) {
    std::os::windows::fs::symlink_file(target, link).unwrap();
}
