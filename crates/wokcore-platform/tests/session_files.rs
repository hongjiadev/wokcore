use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use wokcore_platform::sessions::{
    SessionError, SessionFileIdentity, SessionFileKind, SessionRootLease,
};

#[test]
fn pinned_root_and_file_identities_are_stable_and_native() {
    let fixture = SessionFixture::new(b"synthetic session");
    let first_root = SessionRootLease::open(fixture.root.path()).unwrap();
    let second_root = SessionRootLease::open(fixture.root.path()).unwrap();
    let first_file = first_root.open_file(&fixture.relative_file, 1024).unwrap();
    let second_file = second_root.open_file(&fixture.relative_file, 1024).unwrap();

    assert_eq!(first_root.identity(), second_root.identity());
    assert_eq!(
        first_file.snapshot().identity,
        second_file.snapshot().identity
    );
    assert_eq!(first_file.snapshot().kind, SessionFileKind::RegularFile);
    assert_eq!(first_file.snapshot().size, 17);
    assert_native_identity(first_root.identity());
    assert_native_identity(first_file.snapshot().identity);
}

#[test]
fn component_safe_enumeration_and_reads_preserve_source_state() {
    let fixture = SessionFixture::new(b"synthetic session");
    let before = SourceObservation::capture(&fixture.file);
    let root = SessionRootLease::open(fixture.root.path()).unwrap();
    let directory = root.open_directory("sessions/2026/07/26").unwrap();

    let entries = directory.entries(8).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name(), "session.jsonl");
    assert_eq!(entries[0].snapshot().kind, SessionFileKind::RegularFile);
    let mut opened = directory.open_file(&entries[0], 1024).unwrap();
    assert_eq!(
        opened.read_bounded(1024).unwrap(),
        b"synthetic session".to_vec()
    );
    let reopened = root.open_file(&fixture.relative_file, 1024).unwrap();
    assert_eq!(opened.snapshot().identity, reopened.snapshot().identity);

    let after = SourceObservation::capture(&fixture.file);
    assert_eq!(before, after);
}

#[test]
fn enumeration_is_bounded_before_collecting_untrusted_directory_entries() {
    let fixture = SessionFixture::new(b"first");
    fs::write(
        fixture.file.parent().unwrap().join("second.jsonl"),
        b"second",
    )
    .unwrap();
    let root = SessionRootLease::open(fixture.root.path()).unwrap();
    let directory = root.open_directory("sessions/2026/07/26").unwrap();

    assert!(matches!(
        directory.entries(1),
        Err(SessionError::EnumerationLimitExceeded)
    ));
}

#[test]
fn relative_paths_cannot_escape_the_pinned_root() {
    let fixture = SessionFixture::new(b"inside");
    let outside = fixture.root.path().parent().unwrap().join("outside.jsonl");
    fs::write(&outside, b"outside").unwrap();
    let root = SessionRootLease::open(fixture.root.path()).unwrap();

    for path in [
        PathBuf::from("../outside.jsonl"),
        outside.clone(),
        PathBuf::from("sessions/../../outside.jsonl"),
    ] {
        assert!(matches!(
            root.open_file(path, 1024),
            Err(SessionError::UnsafePath)
        ));
    }
    assert_eq!(fs::read(outside).unwrap(), b"outside");
}

#[test]
fn root_and_intermediate_directory_swaps_fail_closed() {
    let fixture = SessionFixture::new(b"original");
    let root_path = fixture.root.path().to_path_buf();
    let root = SessionRootLease::open(&root_path).unwrap();
    let moved_root = root_path.with_extension("moved");
    fs::rename(&root_path, &moved_root).unwrap();
    fs::create_dir(&root_path).unwrap();
    fs::write(root_path.join("replacement.jsonl"), b"replacement").unwrap();

    assert!(matches!(
        root.open_file("replacement.jsonl", 1024),
        Err(SessionError::UnsafePath)
    ));

    fs::remove_dir_all(&root_path).unwrap();
    fs::rename(&moved_root, &root_path).unwrap();
    let root = SessionRootLease::open(&root_path).unwrap();
    let directory = root.open_directory("sessions/2026/07/26").unwrap();
    let day = root_path.join("sessions/2026/07/26");
    let moved_day = root_path.join("sessions/2026/07/moved-26");
    fs::rename(&day, &moved_day).unwrap();
    fs::create_dir(&day).unwrap();
    fs::write(
        root_path.join("sessions/2026/07/26/session.jsonl"),
        b"replacement",
    )
    .unwrap();

    assert!(matches!(
        directory.entries(8),
        Err(SessionError::UnsafePath)
    ));
}

#[test]
fn root_intermediate_and_final_links_are_never_followed() {
    let fixture = SessionFixture::new(b"inside");
    let external = tempfile::tempdir().unwrap();
    let external_file = external.path().join("external.jsonl");
    fs::write(&external_file, b"external").unwrap();

    let root_link = fixture.root.path().with_extension("link");
    create_directory_link(fixture.root.path(), &root_link);
    assert!(matches!(
        SessionRootLease::open(&root_link),
        Err(SessionError::UnsafePath)
    ));

    let intermediate_link = fixture.root.path().join("linked-directory");
    create_directory_link(external.path(), &intermediate_link);
    let root = SessionRootLease::open(fixture.root.path()).unwrap();
    assert!(matches!(
        root.open_file("linked-directory/external.jsonl", 1024),
        Err(SessionError::UnsafePath)
    ));

    let final_link = fixture.file.parent().unwrap().join("linked-session.jsonl");
    create_file_link(&external_file, &final_link);
    assert!(matches!(
        root.open_file("sessions/2026/07/26/linked-session.jsonl", 1024),
        Err(SessionError::UnsafePath)
    ));
    assert_eq!(fs::read(external_file).unwrap(), b"external");
}

#[test]
fn only_regular_files_are_opened_and_missing_files_are_never_created() {
    let fixture = SessionFixture::new(b"inside");
    let root = SessionRootLease::open(fixture.root.path()).unwrap();
    let before = directory_entries(fixture.file.parent().unwrap());

    assert!(matches!(
        root.open_file("sessions/2026/07/26", 1024),
        Err(SessionError::UnsafePath)
    ));
    assert!(
        root.open_file("sessions/2026/07/26/missing.jsonl", 1024)
            .is_err()
    );
    assert_eq!(directory_entries(fixture.file.parent().unwrap()), before);
}

#[test]
fn a_file_replaced_after_enumeration_fails_the_post_open_identity_recheck() {
    let fixture = SessionFixture::new(b"original");
    let root = SessionRootLease::open(fixture.root.path()).unwrap();
    let directory = root.open_directory("sessions/2026/07/26").unwrap();
    let entry = directory.entries(8).unwrap().remove(0);
    let moved = fixture.file.with_extension("original");
    fs::rename(&fixture.file, &moved).unwrap();
    fs::write(&fixture.file, b"replacement").unwrap();

    assert!(matches!(
        directory.open_file(&entry, 1024),
        Err(SessionError::UnsafePath)
    ));
    assert_eq!(fs::read(moved).unwrap(), b"original");
    assert_eq!(fs::read(&fixture.file).unwrap(), b"replacement");
}

#[test]
fn bounded_reads_reject_growth_beyond_the_caller_limit() {
    let fixture = SessionFixture::new(b"12345678");
    let root = SessionRootLease::open(fixture.root.path()).unwrap();

    assert!(matches!(
        root.open_file(&fixture.relative_file, 7),
        Err(SessionError::ReadLimitExceeded)
    ));
    let mut opened = root.open_file(&fixture.relative_file, 8).unwrap();
    assert_eq!(opened.read_bounded(8).unwrap(), b"12345678");

    OpenOptions::new()
        .append(true)
        .open(&fixture.file)
        .unwrap()
        .write_all(b"9")
        .unwrap();
    assert!(matches!(
        opened.read_bounded(8),
        Err(SessionError::SessionFileChanged)
    ));
}

#[test]
fn pinned_range_reads_are_bounded_seekable_and_eof_safe() {
    let fixture = SessionFixture::new(b"0123456789");
    let root = SessionRootLease::open(fixture.root.path()).unwrap();
    let mut opened = root.open_file(&fixture.relative_file, u64::MAX).unwrap();

    assert_eq!(opened.read_range_bounded(3, 4).unwrap(), b"3456");
    assert_eq!(opened.read_range_bounded(8, usize::MAX).unwrap(), b"89");
    assert!(opened.read_range_bounded(10, 1).unwrap().is_empty());
    assert!(opened.read_range_bounded(u64::MAX, 1).unwrap().is_empty());
    assert!(opened.read_range_bounded(0, 0).unwrap().is_empty());
}

#[test]
fn pinned_range_read_revalidates_every_writer_mutation() {
    for mutation in [
        BoundedReadMutation::Append,
        BoundedReadMutation::Truncate,
        BoundedReadMutation::Rename,
        BoundedReadMutation::Delete,
        BoundedReadMutation::Replace,
    ] {
        let fixture = SessionFixture::new(b"original");
        let root = SessionRootLease::open(fixture.root.path()).unwrap();
        let mut opened = root.open_file(&fixture.relative_file, u64::MAX).unwrap();
        let moved = fixture.file.with_extension("range-moved");
        match mutation {
            BoundedReadMutation::Append => OpenOptions::new()
                .append(true)
                .open(&fixture.file)
                .unwrap()
                .write_all(b"-append")
                .unwrap(),
            BoundedReadMutation::Truncate => {
                OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(&fixture.file)
                    .unwrap();
            }
            BoundedReadMutation::Rename => fs::rename(&fixture.file, &moved).unwrap(),
            BoundedReadMutation::Delete => fs::remove_file(&fixture.file).unwrap(),
            BoundedReadMutation::Replace => {
                fs::rename(&fixture.file, &moved).unwrap();
                fs::write(&fixture.file, b"replacement").unwrap();
            }
        }
        assert!(
            matches!(
                opened.read_range_bounded(0, 4),
                Err(SessionError::SessionFileChanged | SessionError::SessionFileUnavailable)
            ),
            "range read accepted mutation {}",
            mutation.name()
        );
    }
}

#[test]
fn bounded_read_revalidates_the_parent_relative_entry_after_every_writer_mutation() {
    for mutation in [
        BoundedReadMutation::Append,
        BoundedReadMutation::Truncate,
        BoundedReadMutation::Rename,
        BoundedReadMutation::Delete,
        BoundedReadMutation::Replace,
    ] {
        let fixture = SessionFixture::new(b"original");
        let root = SessionRootLease::open(fixture.root.path()).unwrap();
        let mut opened = root.open_file(&fixture.relative_file, 1024).unwrap();
        let moved = fixture.file.with_extension("moved");

        match mutation {
            BoundedReadMutation::Append => {
                OpenOptions::new()
                    .append(true)
                    .open(&fixture.file)
                    .unwrap()
                    .write_all(b"-append")
                    .unwrap();
                assert_eq!(fs::read(&fixture.file).unwrap(), b"original-append");
            }
            BoundedReadMutation::Truncate => {
                OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(&fixture.file)
                    .unwrap();
                assert!(fs::read(&fixture.file).unwrap().is_empty());
            }
            BoundedReadMutation::Rename => {
                fs::rename(&fixture.file, &moved).unwrap();
                assert_eq!(fs::read(&moved).unwrap(), b"original");
            }
            BoundedReadMutation::Delete => {
                fs::remove_file(&fixture.file).unwrap();
                assert!(!fixture.file.exists());
            }
            BoundedReadMutation::Replace => {
                fs::rename(&fixture.file, &moved).unwrap();
                fs::write(&fixture.file, b"replacement").unwrap();
                assert_eq!(fs::read(&fixture.file).unwrap(), b"replacement");
            }
        }

        assert!(
            matches!(
                opened.read_bounded(1024),
                Err(SessionError::SessionFileChanged | SessionError::SessionFileUnavailable)
            ),
            "bounded read accepted mutation {}",
            mutation.name()
        );
    }
}

#[derive(Clone, Copy)]
enum BoundedReadMutation {
    Append,
    Truncate,
    Rename,
    Delete,
    Replace,
}

impl BoundedReadMutation {
    fn name(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::Truncate => "truncate",
            Self::Rename => "rename",
            Self::Delete => "delete",
            Self::Replace => "replace",
        }
    }
}

#[cfg(windows)]
#[test]
fn windows_read_handle_never_blocks_writer_append_rename_truncate_delete_or_replace() {
    assert_writer_mutation_is_observed(WriterMutation::Append);
    assert_writer_mutation_is_observed(WriterMutation::Rename);
    assert_writer_mutation_is_observed(WriterMutation::Truncate);
    assert_writer_mutation_is_observed(WriterMutation::Delete);
    assert_writer_mutation_is_observed(WriterMutation::Replace);
}

#[cfg(windows)]
#[test]
fn windows_alternate_data_stream_is_not_a_regular_session_entry() {
    let fixture = SessionFixture::new(b"base");
    let stream = PathBuf::from(format!("{}:synthetic", fixture.file.display()));
    fs::write(&stream, b"stream").unwrap();
    let root = SessionRootLease::open(fixture.root.path()).unwrap();

    assert!(matches!(
        root.open_file("sessions/2026/07/26/session.jsonl:synthetic", 1024),
        Err(SessionError::UnsafePath)
    ));
    assert_eq!(fs::read(&stream).unwrap(), b"stream");
    assert_eq!(fs::read(&fixture.file).unwrap(), b"base");
}

#[cfg(windows)]
fn assert_writer_mutation_is_observed(mutation: WriterMutation) {
    let fixture = SessionFixture::new(b"original");
    let root = SessionRootLease::open(fixture.root.path()).unwrap();
    let reader = root.open_file(&fixture.relative_file, 1024).unwrap();
    let original = reader.snapshot().clone();
    let moved = fixture.file.with_extension("moved");

    match mutation {
        WriterMutation::Append => {
            OpenOptions::new()
                .append(true)
                .open(&fixture.file)
                .unwrap()
                .write_all(b"-append")
                .unwrap();
            let current = root.open_file(&fixture.relative_file, 1024).unwrap();
            assert_eq!(current.snapshot().identity, original.identity);
            assert_ne!(current.snapshot().size, original.size);
        }
        WriterMutation::Rename => {
            fs::rename(&fixture.file, &moved).unwrap();
            assert!(root.open_file(&fixture.relative_file, 1024).is_err());
        }
        WriterMutation::Truncate => {
            OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&fixture.file)
                .unwrap();
            let current = root.open_file(&fixture.relative_file, 1024).unwrap();
            assert_eq!(current.snapshot().identity, original.identity);
            assert_ne!(current.snapshot().size, original.size);
        }
        WriterMutation::Delete => {
            fs::remove_file(&fixture.file).unwrap();
            assert!(root.open_file(&fixture.relative_file, 1024).is_err());
        }
        WriterMutation::Replace => {
            fs::rename(&fixture.file, &moved).unwrap();
            fs::write(&fixture.file, b"replacement").unwrap();
            let current = root.open_file(&fixture.relative_file, 1024).unwrap();
            assert_ne!(current.snapshot().identity, original.identity);
        }
    }

    drop(reader);
}

#[cfg(windows)]
#[derive(Clone, Copy)]
enum WriterMutation {
    Append,
    Rename,
    Truncate,
    Delete,
    Replace,
}

struct SessionFixture {
    root: tempfile::TempDir,
    file: PathBuf,
    relative_file: PathBuf,
}

impl SessionFixture {
    fn new(contents: &[u8]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let relative_file = PathBuf::from("sessions/2026/07/26/session.jsonl");
        let file = root.path().join(&relative_file);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, contents).unwrap();
        Self {
            root,
            file,
            relative_file,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct SourceObservation {
    contents: Vec<u8>,
    size: u64,
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    attributes: u64,
    entries: Vec<OsString>,
}

impl SourceObservation {
    fn capture(path: &Path) -> Self {
        let metadata = fs::metadata(path).unwrap();
        Self {
            contents: fs::read(path).unwrap(),
            size: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            attributes: platform_attributes(&metadata),
            entries: directory_entries(path.parent().unwrap()),
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
fn platform_attributes(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;

    u64::from(metadata.mode())
}

#[cfg(windows)]
fn platform_attributes(metadata: &fs::Metadata) -> u64 {
    use std::os::windows::fs::MetadataExt;

    u64::from(metadata.file_attributes())
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

#[cfg(unix)]
fn assert_native_identity(identity: SessionFileIdentity) {
    assert!(matches!(identity, SessionFileIdentity::Unix { .. }));
}

#[cfg(windows)]
fn assert_native_identity(identity: SessionFileIdentity) {
    assert!(matches!(identity, SessionFileIdentity::Windows { .. }));
}
