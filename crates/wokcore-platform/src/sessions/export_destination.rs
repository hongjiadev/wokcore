use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
};

use super::SessionError;
#[cfg(windows)]
use super::file::recheck_regular_child_identity;
use super::file::{
    DirectoryChain, SessionFileIdentity, SessionRootLease, child_exists, file_identity,
    validate_child_name, validate_directory_chain,
};

pub struct PinnedExportDestination {
    parent: DirectoryChain,
    session_roots: Vec<DirectoryChain>,
    target_name: OsString,
    temporary_name: Option<OsString>,
    temporary: Option<File>,
    temporary_identity: SessionFileIdentity,
    committed: bool,
}

struct CreatedTemporary {
    file: File,
    source_name: Option<OsString>,
}

struct TemporaryCreationGuard<'a> {
    parent: &'a DirectoryChain,
    temporary: Option<CreatedTemporary>,
}

impl TemporaryCreationGuard<'_> {
    fn file(&self) -> &File {
        &self
            .temporary
            .as_ref()
            .expect("a creation guard owns its temporary")
            .file
    }

    fn disarm(mut self) -> CreatedTemporary {
        self.temporary
            .take()
            .expect("a creation guard owns its temporary")
    }
}

impl Drop for TemporaryCreationGuard<'_> {
    fn drop(&mut self) {
        if let Some(temporary) = self.temporary.take() {
            let _ = remove_unidentified_temporary(self.parent, temporary);
        }
    }
}

impl PinnedExportDestination {
    pub fn create(
        destination: impl AsRef<Path>,
        session_roots: &[&SessionRootLease],
    ) -> Result<Self, SessionError> {
        let destination = absolute_path(destination.as_ref())?;
        let parent_path = destination.parent().ok_or(SessionError::UnsafePath)?;
        let target_name = destination
            .file_name()
            .ok_or(SessionError::UnsafePath)?
            .to_os_string();
        validate_child_name(&target_name)?;

        let parent = SessionRootLease::open(parent_path)?.into_chain();
        let session_roots = session_roots
            .iter()
            .map(|root| root.clone_chain())
            .collect::<Result<Vec<_>, _>>()?;
        validate_export_boundary(&parent, &session_roots)?;
        if child_exists(&parent, &target_name)? {
            return Err(SessionError::UnsafePath);
        }

        let temporary_name = OsString::from(format!(
            ".wokcore-export-{}.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        let temporary = create_relative_temporary(&parent, &temporary_name)?;
        let temporary = TemporaryCreationGuard {
            parent: &parent,
            temporary: Some(temporary),
        };
        #[cfg(test)]
        if temporary_identity_failure_injected() {
            return Err(SessionError::Io {
                source: io::Error::other("injected temporary identity failure"),
            });
        }
        let temporary_identity = file_identity(temporary.file())?;
        let temporary = temporary.disarm();
        Ok(Self {
            parent,
            session_roots,
            target_name,
            temporary_name: temporary.source_name,
            temporary: Some(temporary.file),
            temporary_identity,
            committed: false,
        })
    }

    pub fn commit(mut self) -> Result<(), SessionError> {
        let temporary = self
            .temporary
            .as_mut()
            .expect("an uncommitted export owns its temporary file");
        temporary.sync_all()?;
        validate_export_boundary(&self.parent, &self.session_roots)?;
        ensure_unpublished_temporary(temporary)?;
        #[cfg(windows)]
        recheck_regular_child_identity(
            &self.parent,
            self.temporary_name
                .as_deref()
                .expect("a Windows temporary has a source name"),
            self.temporary_identity,
        )?;
        if child_exists(&self.parent, &self.target_name)? {
            return Err(SessionError::UnsafePath);
        }
        publish_relative_noreplace(
            &self.parent,
            temporary,
            self.temporary_name.as_deref(),
            &self.target_name,
        )?;
        self.committed = true;
        drop(self.temporary.take());
        Ok(())
    }
}

impl Write for PinnedExportDestination {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.temporary
            .as_mut()
            .expect("an uncommitted export owns its temporary file")
            .write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.temporary
            .as_mut()
            .expect("an uncommitted export owns its temporary file")
            .flush()
    }
}

impl Drop for PinnedExportDestination {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Some(temporary) = self.temporary.take() {
            let _ = remove_owned_temporary(
                &self.parent,
                temporary,
                self.temporary_name.as_deref(),
                self.temporary_identity,
            );
        }
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, SessionError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(path))
}

fn validate_export_boundary(
    parent: &DirectoryChain,
    session_roots: &[DirectoryChain],
) -> Result<(), SessionError> {
    validate_directory_chain(parent)?;
    for session_root in session_roots {
        validate_directory_chain(session_root)?;
        let session_identity = session_root
            .directories
            .last()
            .expect("a Session root chain is never empty")
            .identity;
        if parent
            .directories
            .iter()
            .any(|directory| directory.identity == session_identity)
        {
            return Err(SessionError::UnsafePath);
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn create_relative_temporary(
    parent: &DirectoryChain,
    _name: &OsStr,
) -> Result<CreatedTemporary, SessionError> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let descriptor = unsafe {
        libc::openat(
            parent.file().as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDWR | libc::O_TMPFILE | libc::O_CLOEXEC,
            0o600,
        )
    };
    if descriptor < 0 {
        let source = std::io::Error::last_os_error();
        return Err(if source.kind() == io::ErrorKind::AlreadyExists {
            SessionError::UnsafePath
        } else {
            SessionError::Io { source }
        });
    }
    Ok(CreatedTemporary {
        file: unsafe { File::from_raw_fd(descriptor) },
        source_name: None,
    })
}

#[cfg(target_vendor = "apple")]
fn create_relative_temporary(
    parent: &DirectoryChain,
    name: &OsStr,
) -> Result<CreatedTemporary, SessionError> {
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    };

    let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| SessionError::UnsafePath)?;
    let descriptor = unsafe {
        libc::openat(
            parent.file().as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if descriptor < 0 {
        let source = std::io::Error::last_os_error();
        return Err(if source.kind() == io::ErrorKind::AlreadyExists {
            SessionError::UnsafePath
        } else {
            SessionError::Io { source }
        });
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    if unsafe { libc::unlinkat(parent.file().as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(SessionError::Io {
            source: io::Error::last_os_error(),
        });
    }
    Ok(CreatedTemporary {
        file,
        source_name: None,
    })
}

#[cfg(windows)]
fn create_relative_temporary(
    parent: &DirectoryChain,
    name: &OsStr,
) -> Result<CreatedTemporary, SessionError> {
    use std::{
        ffi::c_void,
        mem::size_of,
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle},
        },
        ptr,
    };

    use windows_sys::Win32::{
        Foundation::{
            ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, GENERIC_READ, GENERIC_WRITE, HANDLE,
            INVALID_HANDLE_VALUE,
        },
        Storage::FileSystem::{
            DELETE, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            SYNCHRONIZE,
        },
    };

    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }

    #[repr(C)]
    struct ObjectAttributes {
        length: u32,
        root_directory: HANDLE,
        object_name: *mut UnicodeString,
        attributes: u32,
        security_descriptor: *mut c_void,
        security_quality_of_service: *mut c_void,
    }

    #[repr(C)]
    struct IoStatusBlock {
        status_or_pointer: usize,
        information: usize,
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtCreateFile(
            file_handle: *mut HANDLE,
            desired_access: u32,
            object_attributes: *mut ObjectAttributes,
            io_status_block: *mut IoStatusBlock,
            allocation_size: *const i64,
            file_attributes: u32,
            share_access: u32,
            create_disposition: u32,
            create_options: u32,
            ea_buffer: *const c_void,
            ea_length: u32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }

    let mut wide_name = name.encode_wide().collect::<Vec<_>>();
    let byte_length = wide_name
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|length| u16::try_from(length).ok())
        .ok_or(SessionError::UnsafePath)?;
    let mut unicode = UnicodeString {
        length: byte_length,
        maximum_length: byte_length,
        buffer: wide_name.as_mut_ptr(),
    };
    let mut handle = INVALID_HANDLE_VALUE;
    let mut io_status = IoStatusBlock {
        status_or_pointer: 0,
        information: 0,
    };

    let status = crate::runtime::permissions::with_current_user_security_attributes(
        false,
        |security_attributes| {
            let mut attributes = ObjectAttributes {
                length: size_of::<ObjectAttributes>() as u32,
                root_directory: parent.file().as_raw_handle(),
                object_name: &mut unicode,
                attributes: 0x40,
                security_descriptor: unsafe { (*security_attributes).lpSecurityDescriptor },
                security_quality_of_service: ptr::null_mut(),
            };
            Ok(unsafe {
                NtCreateFile(
                    &mut handle,
                    GENERIC_READ | GENERIC_WRITE | DELETE | SYNCHRONIZE,
                    &mut attributes,
                    &mut io_status,
                    ptr::null(),
                    FILE_ATTRIBUTE_NORMAL,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    2,
                    0x40 | 0x20 | 0x0020_0000,
                    ptr::null(),
                    0,
                )
            })
        },
    )
    .map_err(|error| match error {
        crate::PlatformError::Io { source } => SessionError::Io { source },
        _ => SessionError::UnsafePath,
    })?;
    if status < 0 {
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(
            if code == ERROR_FILE_EXISTS || code == ERROR_ALREADY_EXISTS {
                SessionError::UnsafePath
            } else {
                SessionError::Io {
                    source: io::Error::from_raw_os_error(code as i32),
                }
            },
        );
    }
    if handle == INVALID_HANDLE_VALUE {
        return Err(SessionError::UnsafePath);
    }
    Ok(CreatedTemporary {
        file: unsafe { File::from_raw_handle(handle) },
        source_name: Some(name.to_os_string()),
    })
}

#[cfg(unix)]
fn ensure_unpublished_temporary(file: &File) -> Result<(), SessionError> {
    use std::os::unix::fs::MetadataExt;

    if file.metadata()?.nlink() != 0 {
        return Err(SessionError::UnsafePath);
    }
    Ok(())
}

#[cfg(windows)]
fn ensure_unpublished_temporary(file: &File) -> Result<(), SessionError> {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
        return Err(SessionError::Io {
            source: io::Error::last_os_error(),
        });
    }
    if information.nNumberOfLinks != 1 {
        return Err(SessionError::UnsafePath);
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn publish_relative_noreplace(
    parent: &DirectoryChain,
    temporary: &File,
    temporary_name: Option<&OsStr>,
    target_name: &OsStr,
) -> Result<(), SessionError> {
    use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};

    if temporary_name.is_some() {
        return Err(SessionError::UnsafePath);
    }
    let target_name =
        std::ffi::CString::new(target_name.as_bytes()).map_err(|_| SessionError::UnsafePath)?;
    let result = unsafe {
        libc::linkat(
            temporary.as_raw_fd(),
            c"".as_ptr(),
            parent.file().as_raw_fd(),
            target_name.as_ptr(),
            libc::AT_EMPTY_PATH,
        )
    };
    if result != 0 {
        let source = io::Error::last_os_error();
        return Err(if source.kind() == io::ErrorKind::AlreadyExists {
            SessionError::UnsafePath
        } else {
            SessionError::Io { source }
        });
    }
    Ok(())
}

#[cfg(target_vendor = "apple")]
fn publish_relative_noreplace(
    parent: &DirectoryChain,
    temporary: &File,
    temporary_name: Option<&OsStr>,
    target_name: &OsStr,
) -> Result<(), SessionError> {
    use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};

    if temporary_name.is_some() {
        return Err(SessionError::UnsafePath);
    }
    let target_name =
        std::ffi::CString::new(target_name.as_bytes()).map_err(|_| SessionError::UnsafePath)?;
    if unsafe {
        libc::fclonefileat(
            temporary.as_raw_fd(),
            parent.file().as_raw_fd(),
            target_name.as_ptr(),
            0,
        )
    } != 0
    {
        let source = io::Error::last_os_error();
        return Err(if source.kind() == io::ErrorKind::AlreadyExists {
            SessionError::UnsafePath
        } else {
            SessionError::Io { source }
        });
    }
    Ok(())
}

#[cfg(windows)]
fn publish_relative_noreplace(
    parent: &DirectoryChain,
    temporary: &File,
    _temporary_name: Option<&OsStr>,
    target_name: &OsStr,
) -> Result<(), SessionError> {
    use std::{
        ffi::c_void,
        mem::size_of,
        os::windows::{ffi::OsStrExt, io::AsRawHandle},
    };

    use windows_sys::Win32::{
        Foundation::{ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, HANDLE},
        Storage::FileSystem::{FILE_RENAME_INFO, FILE_RENAME_INFO_0},
    };

    #[repr(C)]
    struct IoStatusBlock {
        status_or_pointer: usize,
        information: usize,
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtSetInformationFile(
            file_handle: HANDLE,
            io_status_block: *mut IoStatusBlock,
            file_information: *const c_void,
            length: u32,
            file_information_class: u32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }

    let target_name = target_name.encode_wide().collect::<Vec<_>>();
    let name_bytes = target_name
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|length| u32::try_from(length).ok())
        .ok_or(SessionError::UnsafePath)?;
    let total = size_of::<FILE_RENAME_INFO>()
        .checked_add(name_bytes as usize)
        .ok_or(SessionError::UnsafePath)?;
    let word_count = total.div_ceil(size_of::<usize>());
    let mut buffer = vec![0_usize; word_count];
    let information = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        (*information).Anonymous = FILE_RENAME_INFO_0 {
            ReplaceIfExists: false,
        };
        (*information).RootDirectory = parent.file().as_raw_handle();
        (*information).FileNameLength = name_bytes;
        std::ptr::copy_nonoverlapping(
            target_name.as_ptr(),
            std::ptr::addr_of_mut!((*information).FileName).cast::<u16>(),
            target_name.len(),
        );
    }
    let mut io_status = IoStatusBlock {
        status_or_pointer: 0,
        information: 0,
    };
    let status = unsafe {
        NtSetInformationFile(
            temporary.as_raw_handle(),
            &mut io_status,
            buffer.as_ptr().cast(),
            total as u32,
            10,
        )
    };
    if status < 0 {
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(
            if code == ERROR_FILE_EXISTS || code == ERROR_ALREADY_EXISTS {
                SessionError::UnsafePath
            } else {
                SessionError::Io {
                    source: io::Error::from_raw_os_error(code as i32),
                }
            },
        );
    }
    Ok(())
}

#[cfg(unix)]
fn remove_owned_temporary(
    _parent: &DirectoryChain,
    temporary: File,
    temporary_name: Option<&OsStr>,
    expected: SessionFileIdentity,
) -> Result<(), SessionError> {
    if temporary_name.is_some() || file_identity(&temporary)? != expected {
        return Err(SessionError::UnsafePath);
    }
    drop(temporary);
    Ok(())
}

#[cfg(unix)]
fn remove_unidentified_temporary(
    _parent: &DirectoryChain,
    temporary: CreatedTemporary,
) -> Result<(), SessionError> {
    if temporary.source_name.is_some() {
        return Err(SessionError::UnsafePath);
    }
    drop(temporary.file);
    Ok(())
}

#[cfg(windows)]
fn remove_owned_temporary(
    _parent: &DirectoryChain,
    temporary: File,
    _temporary_name: Option<&OsStr>,
    expected: SessionFileIdentity,
) -> Result<(), SessionError> {
    remove_windows_temporary(temporary, Some(expected))
}

#[cfg(windows)]
fn remove_unidentified_temporary(
    _parent: &DirectoryChain,
    temporary: CreatedTemporary,
) -> Result<(), SessionError> {
    remove_windows_temporary(temporary.file, None)
}

#[cfg(windows)]
fn remove_windows_temporary(
    temporary: File,
    expected: Option<SessionFileIdentity>,
) -> Result<(), SessionError> {
    use std::{mem::size_of, os::windows::io::AsRawHandle};

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
    };

    if let Some(expected) = expected
        && file_identity(&temporary)? != expected
    {
        return Err(SessionError::UnsafePath);
    }
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    if unsafe {
        SetFileInformationByHandle(
            temporary.as_raw_handle(),
            FileDispositionInfo,
            (&raw const disposition).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        return Err(SessionError::Io {
            source: io::Error::last_os_error(),
        });
    }
    drop(temporary);
    Ok(())
}

#[cfg(test)]
std::thread_local! {
    static FAIL_TEMPORARY_IDENTITY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn temporary_identity_failure_injected() -> bool {
    FAIL_TEMPORARY_IDENTITY.with(|injected| injected.replace(false))
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, fs};

    #[cfg(windows)]
    use std::io::Write;

    use crate::sessions::{SessionError, SessionRootLease};

    use super::PinnedExportDestination;

    #[cfg(windows)]
    #[test]
    fn injected_parent_identity_change_fails_closed_and_cleans_only_the_owned_temporary() {
        let root = tempfile::tempdir().unwrap();
        let session_path = root.path().join("sessions");
        fs::create_dir(&session_path).unwrap();
        let session = SessionRootLease::open(&session_path).unwrap();
        let export_parent = root.path().join("exports");
        fs::create_dir(&export_parent).unwrap();
        let target = export_parent.join("diagnostics.zip");
        let decoy = export_parent.join("decoy");
        fs::write(&decoy, b"decoy").unwrap();
        let replacement_parent = root.path().join("replacement-parent");
        fs::create_dir(&replacement_parent).unwrap();
        let replacement_identity = SessionRootLease::open(&replacement_parent)
            .unwrap()
            .identity();
        let mut destination = PinnedExportDestination::create(&target, &[&session]).unwrap();
        destination.write_all(b"owned temporary").unwrap();
        destination.parent.directories.last_mut().unwrap().identity = replacement_identity;

        assert!(matches!(
            destination.commit(),
            Err(SessionError::UnsafePath)
        ));
        assert_eq!(
            directory_entries(&export_parent),
            vec![OsString::from("decoy")]
        );
        assert_eq!(fs::read(decoy).unwrap(), b"decoy");
        assert!(!target.exists());
        assert!(directory_entries(&session_path).is_empty());
    }

    #[test]
    fn identity_failure_before_destination_construction_cleans_the_owned_temporary() {
        let root = tempfile::tempdir().unwrap();
        let session_path = root.path().join("sessions");
        fs::create_dir(&session_path).unwrap();
        let session = SessionRootLease::open(&session_path).unwrap();
        let export_parent = root.path().join("exports");
        fs::create_dir(&export_parent).unwrap();
        let target = export_parent.join("diagnostics.zip");
        super::FAIL_TEMPORARY_IDENTITY.with(|injected| injected.set(true));

        assert!(matches!(
            PinnedExportDestination::create(&target, &[&session]),
            Err(SessionError::Io { .. })
        ));
        assert!(directory_entries(&export_parent).is_empty());
        assert!(directory_entries(&session_path).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn drop_deletes_the_owned_handle_without_deleting_a_source_name_replacement() {
        let root = tempfile::tempdir().unwrap();
        let session_path = root.path().join("sessions");
        fs::create_dir(&session_path).unwrap();
        let session = SessionRootLease::open(&session_path).unwrap();
        let export_parent = root.path().join("exports");
        fs::create_dir(&export_parent).unwrap();
        let target = export_parent.join("diagnostics.zip");
        let mut destination = PinnedExportDestination::create(&target, &[&session]).unwrap();
        destination.write_all(b"owned temporary").unwrap();
        let temporary_name = destination.temporary_name.clone().unwrap();
        let temporary_path = export_parent.join(&temporary_name);
        let moved_owned = export_parent.join("moved-owned.tmp");
        fs::rename(&temporary_path, &moved_owned).unwrap();
        fs::write(&temporary_path, b"replacement").unwrap();

        drop(destination);

        assert!(!moved_owned.exists());
        assert_eq!(fs::read(&temporary_path).unwrap(), b"replacement");
        assert_eq!(directory_entries(&export_parent), vec![temporary_name]);
    }

    #[cfg(unix)]
    #[test]
    fn unix_destination_never_exposes_a_temporary_source_name() {
        let root = tempfile::tempdir().unwrap();
        let session_path = root.path().join("sessions");
        fs::create_dir(&session_path).unwrap();
        let session = SessionRootLease::open(&session_path).unwrap();
        let export_parent = root.path().join("exports");
        fs::create_dir(&export_parent).unwrap();
        let target = export_parent.join("diagnostics.zip");
        let destination = PinnedExportDestination::create(&target, &[&session]).unwrap();

        assert!(destination.temporary_name.is_none());
        assert!(directory_entries(&export_parent).is_empty());
        drop(destination);
        assert!(directory_entries(&export_parent).is_empty());
    }

    fn directory_entries(path: &std::path::Path) -> Vec<OsString> {
        let mut entries = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }
}
