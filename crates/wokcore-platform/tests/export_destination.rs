use std::{
    ffi::OsString,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use wokcore_platform::sessions::{PinnedExportDestination, SessionError, SessionRootLease};

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

#[test]
fn temporary_is_exclusive_relative_to_the_pinned_parent_and_drop_removes_only_owned_file() {
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
        #[cfg(windows)]
        {
            assert_eq!(during.len(), before.len() + 1);
            assert!(during.iter().any(|name| {
                name.to_string_lossy().starts_with(".wokcore-export-")
                    && name != ".wokcore-export-decoy.tmp"
            }));
        }
        #[cfg(unix)]
        assert_eq!(during, before);
    }

    assert_eq!(directory_entries(&fixture.export_parent), before);
    assert_eq!(fs::read(decoy).unwrap(), b"decoy");
    assert!(!target.exists());
}

#[cfg(unix)]
#[test]
fn unix_commit_publishes_owned_anonymous_contents_without_a_source_name() {
    let fixture = ExportFixture::new();
    let target = fixture.export_parent.join("anonymous.zip");
    let mut destination = PinnedExportDestination::create(&target, &[&fixture.session]).unwrap();
    destination.write_all(b"owned anonymous contents").unwrap();
    let decoy = fixture.export_parent.join(".wokcore-export-forged.tmp");
    fs::write(&decoy, b"forged replacement").unwrap();

    assert_eq!(
        directory_entries(&fixture.export_parent),
        vec![OsString::from(".wokcore-export-forged.tmp")]
    );
    destination.commit().unwrap();

    assert_eq!(fs::read(&target).unwrap(), b"owned anonymous contents");
    assert_eq!(fs::read(&decoy).unwrap(), b"forged replacement");
    assert_eq!(
        directory_entries(&fixture.export_parent),
        vec![
            OsString::from(".wokcore-export-forged.tmp"),
            OsString::from("anonymous.zip"),
        ]
    );
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
