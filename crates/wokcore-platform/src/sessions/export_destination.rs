#[cfg(not(target_vendor = "apple"))]
use std::fs::File;
use std::{
    ffi::{OsStr, OsString},
    io::{self, Write},
    path::{Path, PathBuf},
};

use super::SessionError;
#[cfg(not(target_vendor = "apple"))]
use super::file::SessionFileIdentity;
#[cfg(windows)]
use super::file::recheck_regular_child_identity;
use super::file::{
    DirectoryChain, SessionRootLease, child_exists, file_identity, validate_child_name,
    validate_directory_chain,
};

pub struct PinnedExportDestination {
    parent: DirectoryChain,
    session_roots: Vec<DirectoryChain>,
    target_name: OsString,
    temporary_name: Option<OsString>,
    temporary: Option<ExportTemporary>,
    #[cfg(not(target_vendor = "apple"))]
    temporary_identity: SessionFileIdentity,
    committed: bool,
}

#[cfg(not(target_vendor = "apple"))]
type ExportTemporary = File;
#[cfg(target_vendor = "apple")]
type ExportTemporary = apple::Temporary;

struct CreatedTemporary {
    file: ExportTemporary,
    source_name: Option<OsString>,
}

struct TemporaryCreationGuard<'a> {
    parent: &'a DirectoryChain,
    temporary: Option<CreatedTemporary>,
}

impl TemporaryCreationGuard<'_> {
    #[cfg(not(target_vendor = "apple"))]
    fn file(&self) -> &ExportTemporary {
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
        #[cfg(not(target_vendor = "apple"))]
        let temporary_identity = file_identity(temporary.file())?;
        let temporary = temporary.disarm();
        Ok(Self {
            parent,
            session_roots,
            target_name,
            temporary_name: temporary.source_name,
            temporary: Some(temporary.file),
            #[cfg(not(target_vendor = "apple"))]
            temporary_identity,
            committed: false,
        })
    }

    pub fn commit(mut self) -> Result<(), SessionError> {
        let temporary = self
            .temporary
            .as_mut()
            .expect("an uncommitted export owns its temporary file");
        sync_temporary(temporary)?;
        #[cfg(all(test, windows))]
        commit_synchronization_tests::hit(CommitSynchronizationPoint::BeforeBoundaryValidation);
        validate_export_boundary(&self.parent, &self.session_roots)?;
        ensure_unpublished_temporary(&self.parent, temporary)?;
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
        prepare_temporary_for_publish(temporary)?;
        publish_relative_noreplace(
            &self.parent,
            temporary,
            self.temporary_name.as_deref(),
            &self.target_name,
        )?;
        self.committed = true;
        self.temporary = None;
        Ok(())
    }
}

impl Write for PinnedExportDestination {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        write_temporary(
            self.temporary
                .as_mut()
                .expect("an uncommitted export owns its temporary file"),
            buffer,
        )
    }

    fn flush(&mut self) -> io::Result<()> {
        flush_temporary(
            self.temporary
                .as_mut()
                .expect("an uncommitted export owns its temporary file"),
        )
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
                #[cfg(not(target_vendor = "apple"))]
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

fn write_temporary(temporary: &mut ExportTemporary, buffer: &[u8]) -> io::Result<usize> {
    temporary.write(buffer)
}

fn flush_temporary(temporary: &mut ExportTemporary) -> io::Result<()> {
    temporary.flush()
}

fn sync_temporary(temporary: &mut ExportTemporary) -> Result<(), SessionError> {
    temporary.sync_all()?;
    Ok(())
}

#[cfg(not(target_vendor = "apple"))]
fn prepare_temporary_for_publish(_temporary: &mut ExportTemporary) -> Result<(), SessionError> {
    Ok(())
}

#[cfg(target_vendor = "apple")]
fn prepare_temporary_for_publish(temporary: &mut ExportTemporary) -> Result<(), SessionError> {
    temporary.close_fork()
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
    Ok(CreatedTemporary {
        file: apple::Temporary::create(parent, name)?,
        source_name: Some(name.to_os_string()),
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

#[cfg(any(target_os = "linux", target_os = "android"))]
fn ensure_unpublished_temporary(_parent: &DirectoryChain, file: &File) -> Result<(), SessionError> {
    use std::os::unix::fs::MetadataExt;

    if file.metadata()?.nlink() != 0 {
        return Err(SessionError::UnsafePath);
    }
    Ok(())
}

#[cfg(target_vendor = "apple")]
fn ensure_unpublished_temporary(
    _parent: &DirectoryChain,
    temporary: &ExportTemporary,
) -> Result<(), SessionError> {
    temporary.verify_current_parent()
}

#[cfg(windows)]
fn ensure_unpublished_temporary(_parent: &DirectoryChain, file: &File) -> Result<(), SessionError> {
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
    temporary: &mut File,
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
    _parent: &DirectoryChain,
    temporary: &mut ExportTemporary,
    temporary_name: Option<&OsStr>,
    target_name: &OsStr,
) -> Result<(), SessionError> {
    if temporary_name.is_none() {
        return Err(SessionError::UnsafePath);
    }
    temporary.rename_noreplace(target_name)
}

#[cfg(windows)]
fn publish_relative_noreplace(
    parent: &DirectoryChain,
    temporary: &mut File,
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

#[cfg(any(target_os = "linux", target_os = "android"))]
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

#[cfg(any(target_os = "linux", target_os = "android"))]
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

#[cfg(target_vendor = "apple")]
fn remove_owned_temporary(
    _parent: &DirectoryChain,
    mut temporary: ExportTemporary,
    temporary_name: Option<&OsStr>,
) -> Result<(), SessionError> {
    if temporary_name.is_none() {
        return Err(SessionError::UnsafePath);
    }
    temporary.remove_owned()
}

#[cfg(target_vendor = "apple")]
fn remove_unidentified_temporary(
    _parent: &DirectoryChain,
    mut temporary: CreatedTemporary,
) -> Result<(), SessionError> {
    if temporary.source_name.is_none() {
        return Err(SessionError::UnsafePath);
    }
    temporary.file.remove_owned()
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

#[cfg(target_vendor = "apple")]
mod apple {
    use std::{
        ffi::{CStr, CString, OsStr, c_void},
        fs::File,
        io,
        mem::{MaybeUninit, align_of, offset_of, size_of},
        os::{fd::FromRawFd, unix::ffi::OsStrExt},
        ptr,
    };

    use crate::sessions::SessionFileIdentity;

    use super::{DirectoryChain, SessionError, file_identity};

    // CoreServices/CarbonCore/Files.h 64-bit ABI, as mirrored by objc2-generated
    // CoreServices/CarbonCore/Files.rs: FSIORefNum is c_int, OSStatus is i32,
    // OSErr is i16, UniCharCount/FSCatalogInfoBitmap/OptionBits are u32.
    type OsStatus = i32;
    type OsErr = i16;
    type FsIoRefNum = libc::c_int;
    type UniCharCount = u32;
    type CatalogInfoBitmap = u32;
    type OptionBits = u32;
    type ByteCount = usize;
    type CfIndex = isize;
    type CfStringRef = *const CfString;

    const NO_ERR: OsErr = 0;
    const DUPLICATE_FILE_NAME_ERROR: OsErr = -48;
    const CATALOG_NODE_ID: CatalogInfoBitmap = 0x0000_0010;
    const CATALOG_PERMISSIONS: CatalogInfoBitmap = 0x0000_0400;
    const READ_WRITE_PERMISSION: i8 = 3;
    const AT_MARK: u16 = 0;
    const DO_NOT_MOVE_ACROSS_VOLUMES: OptionBits = 4;

    #[repr(C)]
    struct CfString {
        _private: [u8; 0],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct FsRef {
        hidden: [u8; 80],
    }

    #[repr(C)]
    struct HfsUniStr255 {
        length: u16,
        unicode: [u16; 255],
    }

    #[repr(C, packed(2))]
    struct UtcDateTime {
        high_seconds: u16,
        low_seconds: u32,
        fraction: u16,
    }

    #[repr(C)]
    struct PermissionInfo {
        user_id: u32,
        group_id: u32,
        reserved1: u8,
        user_access: u8,
        mode: u16,
        reserved2: u32,
    }

    #[repr(C, packed(2))]
    struct CatalogInfo {
        node_flags: u16,
        volume: i16,
        parent_dir_id: u32,
        node_id: u32,
        sharing_flags: u8,
        user_privileges: u8,
        reserved1: u8,
        reserved2: u8,
        create_date: UtcDateTime,
        content_modified_date: UtcDateTime,
        attribute_modified_date: UtcDateTime,
        access_date: UtcDateTime,
        backup_date: UtcDateTime,
        permissions: PermissionInfo,
        finder_info: [u8; 16],
        extended_finder_info: [u8; 16],
        data_logical_size: u64,
        data_physical_size: u64,
        resource_logical_size: u64,
        resource_physical_size: u64,
        valence: u32,
        text_encoding_hint: u32,
    }

    const _: () = assert!(size_of::<FsRef>() == 80);
    const _: () = assert!(align_of::<FsRef>() == 1);
    const _: () = assert!(size_of::<HfsUniStr255>() == 512);
    const _: () = assert!(align_of::<HfsUniStr255>() == 2);
    const _: () = assert!(size_of::<UtcDateTime>() == 8);
    const _: () = assert!(size_of::<PermissionInfo>() == 16);
    const _: () = assert!(size_of::<CatalogInfo>() == 144);
    const _: () = assert!(align_of::<CatalogInfo>() == 2);
    const _: () = assert!(offset_of!(CatalogInfo, permissions) == 56);
    const _: () = assert!(offset_of!(CatalogInfo, finder_info) == 72);
    const _: () = assert!(offset_of!(CatalogInfo, data_logical_size) == 104);
    const _: () = assert!(offset_of!(CatalogInfo, valence) == 136);
    const _: () = assert!(offset_of!(CatalogInfo, text_encoding_hint) == 140);

    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn FSPathMakeRef(path: *const u8, reference: *mut FsRef, is_directory: *mut u8)
        -> OsStatus;
        fn FSRefMakePath(reference: *const FsRef, path: *mut u8, maximum_size: u32) -> OsStatus;
        fn FSGetDataForkName(name: *mut HfsUniStr255) -> OsErr;
        fn FSCreateFileUnicode(
            parent: *const FsRef,
            name_length: UniCharCount,
            name: *const u16,
            which_info: CatalogInfoBitmap,
            catalog_info: *const CatalogInfo,
            reference: *mut FsRef,
            specification: *mut c_void,
        ) -> OsErr;
        fn FSOpenFork(
            reference: *const FsRef,
            fork_name_length: UniCharCount,
            fork_name: *const u16,
            permissions: i8,
            fork: *mut FsIoRefNum,
        ) -> OsErr;
        fn FSWriteFork(
            fork: FsIoRefNum,
            position_mode: u16,
            position_offset: i64,
            request_count: ByteCount,
            buffer: *const c_void,
            actual_count: *mut ByteCount,
        ) -> OsErr;
        fn FSFlushFork(fork: FsIoRefNum) -> OsErr;
        fn FSCloseFork(fork: FsIoRefNum) -> OsErr;
        fn FSGetCatalogInfo(
            reference: *const FsRef,
            which_info: CatalogInfoBitmap,
            catalog_info: *mut CatalogInfo,
            name: *mut HfsUniStr255,
            specification: *mut c_void,
            parent: *mut FsRef,
        ) -> OsErr;
        fn FSCompareFSRefs(first: *const FsRef, second: *const FsRef) -> OsErr;
        fn FSMoveObjectSync(
            source: *const FsRef,
            destination_directory: *const FsRef,
            destination_name: CfStringRef,
            target: *mut FsRef,
            options: OptionBits,
        ) -> OsStatus;
        fn FSUnlinkObject(reference: *const FsRef) -> OsErr;
        fn FSDeleteObject(reference: *const FsRef) -> OsErr;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCharacters(
            allocator: *const c_void,
            characters: *const u16,
            count: CfIndex,
        ) -> CfStringRef;
        fn CFRelease(value: *const c_void);
    }

    pub(super) struct Temporary {
        fork: Option<ForkOwner>,
        object: ObjectOwner,
        original_parent: FsRef,
    }

    struct ForkOwner {
        raw: Option<FsIoRefNum>,
    }

    impl ForkOwner {
        fn close(&mut self) -> Result<(), SessionError> {
            let Some(raw) = self.raw else {
                return Ok(());
            };
            #[cfg(test)]
            if super::apple_close_failure_injected() {
                return Err(carbon_error("FSCloseFork", -1));
            }
            let status = unsafe { FSCloseFork(raw) };
            if status != NO_ERR {
                return Err(carbon_error("FSCloseFork", i32::from(status)));
            }
            self.raw = None;
            Ok(())
        }
    }

    impl Drop for ForkOwner {
        fn drop(&mut self) {
            let _ = self.close();
        }
    }

    struct ObjectOwner {
        reference: FsRef,
        owned: bool,
    }

    impl ObjectOwner {
        fn unlink(&mut self) -> Result<(), SessionError> {
            if !self.owned {
                return Ok(());
            }
            #[cfg(test)]
            let status = if super::apple_unlink_failure_injected() {
                -1
            } else {
                unsafe { FSUnlinkObject(&raw const self.reference) }
            };
            #[cfg(not(test))]
            let status = unsafe { FSUnlinkObject(&raw const self.reference) };
            if status != NO_ERR {
                return Err(carbon_error("FSUnlinkObject", i32::from(status)));
            }
            self.owned = false;
            Ok(())
        }

        fn delete(&mut self) -> Result<(), SessionError> {
            if !self.owned {
                return Ok(());
            }
            #[cfg(test)]
            let status = if super::apple_delete_failure_injected() {
                -1
            } else {
                unsafe { FSDeleteObject(&raw const self.reference) }
            };
            #[cfg(not(test))]
            let status = unsafe { FSDeleteObject(&raw const self.reference) };
            if status != NO_ERR {
                return Err(carbon_error("FSDeleteObject", i32::from(status)));
            }
            self.owned = false;
            Ok(())
        }
    }

    impl Drop for ObjectOwner {
        fn drop(&mut self) {
            if self.unlink().is_err() {
                let _ = self.delete();
            }
        }
    }

    struct OwnedCfString(CfStringRef);

    impl OwnedCfString {
        fn new(characters: &[u16]) -> Result<Self, SessionError> {
            let count =
                CfIndex::try_from(characters.len()).map_err(|_| SessionError::UnsafePath)?;
            let raw =
                unsafe { CFStringCreateWithCharacters(ptr::null(), characters.as_ptr(), count) };
            if raw.is_null() {
                return Err(carbon_error("CFStringCreateWithCharacters", -1));
            }
            Ok(Self(raw))
        }
    }

    impl Drop for OwnedCfString {
        fn drop(&mut self) {
            unsafe { CFRelease(self.0.cast()) };
        }
    }

    impl Temporary {
        pub(super) fn create(parent: &DirectoryChain, name: &OsStr) -> Result<Self, SessionError> {
            let original_parent = pinned_directory_ref(parent)?;
            let name = unicode_name(name)?;
            let mut fork_name = MaybeUninit::<HfsUniStr255>::uninit();
            let status = unsafe { FSGetDataForkName(fork_name.as_mut_ptr()) };
            if status != NO_ERR {
                return Err(carbon_error("FSGetDataForkName", i32::from(status)));
            }
            let fork_name = unsafe { fork_name.assume_init() };
            let mut catalog_info = unsafe { MaybeUninit::<CatalogInfo>::zeroed().assume_init() };
            catalog_info.permissions.mode = 0o600;
            let mut reference = MaybeUninit::<FsRef>::uninit();
            let status = unsafe {
                FSCreateFileUnicode(
                    &raw const original_parent,
                    name.len() as UniCharCount,
                    name.as_ptr(),
                    CATALOG_PERMISSIONS,
                    &raw const catalog_info,
                    reference.as_mut_ptr(),
                    ptr::null_mut(),
                )
            };
            if status != NO_ERR {
                return Err(if status == DUPLICATE_FILE_NAME_ERROR {
                    SessionError::UnsafePath
                } else {
                    carbon_error("FSCreateFileUnicode", i32::from(status))
                });
            }
            let object = ObjectOwner {
                reference: unsafe { reference.assume_init() },
                owned: true,
            };
            #[cfg(test)]
            if super::apple_open_failure_injected() {
                return Err(carbon_error("FSOpenFork", -1));
            }
            let mut fork = 0;
            let status = unsafe {
                FSOpenFork(
                    &raw const object.reference,
                    UniCharCount::from(fork_name.length),
                    fork_name.unicode.as_ptr(),
                    READ_WRITE_PERMISSION,
                    &mut fork,
                )
            };
            if status != NO_ERR {
                return Err(carbon_error("FSOpenFork", i32::from(status)));
            }
            Ok(Self {
                fork: Some(ForkOwner { raw: Some(fork) }),
                object,
                original_parent,
            })
        }

        pub(super) fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            let fork = self
                .fork
                .as_ref()
                .and_then(|fork| fork.raw)
                .ok_or_else(|| io::Error::other("Apple export fork is closed"))?;
            let mut actual_count = 0;
            let status = unsafe {
                FSWriteFork(
                    fork,
                    AT_MARK,
                    0,
                    buffer.len(),
                    buffer.as_ptr().cast(),
                    &mut actual_count,
                )
            };
            if status == NO_ERR {
                Ok(actual_count)
            } else {
                Err(carbon_io_error("FSWriteFork", i32::from(status)))
            }
        }

        pub(super) fn flush(&mut self) -> io::Result<()> {
            self.flush_fork().map_err(session_error_to_io)
        }

        pub(super) fn sync_all(&mut self) -> Result<(), SessionError> {
            self.flush_fork()
        }

        fn flush_fork(&self) -> Result<(), SessionError> {
            let fork = self
                .fork
                .as_ref()
                .and_then(|fork| fork.raw)
                .ok_or(SessionError::UnsafePath)?;
            let status = unsafe { FSFlushFork(fork) };
            if status == NO_ERR {
                Ok(())
            } else {
                Err(carbon_error("FSFlushFork", i32::from(status)))
            }
        }

        pub(super) fn close_fork(&mut self) -> Result<(), SessionError> {
            let Some(fork) = self.fork.as_mut() else {
                return Ok(());
            };
            fork.close()?;
            self.fork = None;
            Ok(())
        }

        pub(super) fn verify_current_parent(&self) -> Result<(), SessionError> {
            let mut current_parent = MaybeUninit::<FsRef>::uninit();
            let status = unsafe {
                FSGetCatalogInfo(
                    &raw const self.object.reference,
                    0,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    current_parent.as_mut_ptr(),
                )
            };
            if status != NO_ERR {
                return Err(carbon_error("FSGetCatalogInfo", i32::from(status)));
            }
            let current_parent = unsafe { current_parent.assume_init() };
            if unsafe {
                FSCompareFSRefs(&raw const current_parent, &raw const self.original_parent)
            } != NO_ERR
            {
                return Err(SessionError::UnsafePath);
            }
            Ok(())
        }

        pub(super) fn rename_noreplace(&mut self, name: &OsStr) -> Result<(), SessionError> {
            if self.fork.is_some() || !self.object.owned {
                return Err(SessionError::UnsafePath);
            }
            self.verify_current_parent()?;
            #[cfg(test)]
            super::apple_synchronization_tests::hit(
                super::AppleSynchronizationPoint::BeforeObjectBoundPublish,
            );
            let name = unicode_name(name)?;
            let name = OwnedCfString::new(&name)?;
            let mut published = MaybeUninit::<FsRef>::uninit();
            let status = unsafe {
                FSMoveObjectSync(
                    &raw const self.object.reference,
                    &raw const self.original_parent,
                    name.0,
                    published.as_mut_ptr(),
                    DO_NOT_MOVE_ACROSS_VOLUMES,
                )
            };
            if status != OsStatus::from(NO_ERR) {
                return Err(if status == OsStatus::from(DUPLICATE_FILE_NAME_ERROR) {
                    SessionError::UnsafePath
                } else {
                    carbon_error("FSMoveObjectSync", status)
                });
            }
            self.object.reference = unsafe { published.assume_init() };
            self.object.owned = false;
            Ok(())
        }

        pub(super) fn remove_owned(&mut self) -> Result<(), SessionError> {
            if !self.object.owned {
                return self.close_fork();
            }
            let unlink_result = self.object.unlink();
            let close_result = self.close_fork();
            if unlink_result.is_ok() {
                return close_result;
            }
            self.object.delete()?;
            close_result
        }
    }

    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = self.remove_owned();
        }
    }

    fn pinned_directory_ref(parent: &DirectoryChain) -> Result<FsRef, SessionError> {
        let stability = parent.capture_stability()?;
        let path = CString::new(parent.path().as_os_str().as_bytes())
            .map_err(|_| SessionError::UnsafePath)?;
        let mut reference = MaybeUninit::<FsRef>::uninit();
        let mut is_directory = 0;
        let status = unsafe {
            FSPathMakeRef(
                path.as_ptr().cast(),
                reference.as_mut_ptr(),
                &mut is_directory,
            )
        };
        if status != i32::from(NO_ERR) {
            return Err(carbon_error("FSPathMakeRef", status));
        }
        if is_directory == 0 {
            return Err(SessionError::UnsafePath);
        }
        let reference = unsafe { reference.assume_init() };
        #[cfg(test)]
        super::apple_synchronization_tests::hit(super::AppleSynchronizationPoint::AfterPathMakeRef);
        parent.verify_stability(&stability)?;
        let mut verified_path = [0_u8; libc::PATH_MAX as usize];
        let status = unsafe {
            FSRefMakePath(
                &raw const reference,
                verified_path.as_mut_ptr(),
                verified_path.len() as u32,
            )
        };
        if status != i32::from(NO_ERR) {
            return Err(carbon_error("FSRefMakePath", status));
        }
        #[cfg(test)]
        super::apple_synchronization_tests::hit(
            super::AppleSynchronizationPoint::BeforeIdentityOpen,
        );
        let verified_path = unsafe { CStr::from_ptr(verified_path.as_ptr().cast()) };
        let descriptor = unsafe {
            libc::open(
                verified_path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            return Err(SessionError::Io {
                source: io::Error::last_os_error(),
            });
        }
        let verified = unsafe { File::from_raw_fd(descriptor) };
        let pinned_identity = file_identity(parent.file())?;
        if file_identity(&verified)? != pinned_identity {
            return Err(SessionError::UnsafePath);
        }
        verify_catalog_node_identity(&reference, pinned_identity)?;
        parent.verify_stability(&stability)?;
        Ok(reference)
    }

    fn verify_catalog_node_identity(
        reference: &FsRef,
        pinned_identity: SessionFileIdentity,
    ) -> Result<(), SessionError> {
        let SessionFileIdentity::Unix { inode, .. } = pinned_identity;
        let Ok(inode) = u32::try_from(inode) else {
            return Ok(());
        };
        let mut catalog = unsafe { MaybeUninit::<CatalogInfo>::zeroed().assume_init() };
        let status = unsafe {
            FSGetCatalogInfo(
                reference,
                CATALOG_NODE_ID,
                &mut catalog,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if status != NO_ERR {
            return Err(carbon_error("FSGetCatalogInfo", i32::from(status)));
        }
        let node_id = unsafe { ptr::addr_of!(catalog.node_id).read_unaligned() };
        if node_id != inode {
            return Err(SessionError::UnsafePath);
        }
        Ok(())
    }

    fn unicode_name(name: &OsStr) -> Result<Vec<u16>, SessionError> {
        let name = name.to_str().ok_or(SessionError::UnsafePath)?;
        let name = name.encode_utf16().collect::<Vec<_>>();
        if name.is_empty() || name.len() > u32::MAX as usize {
            return Err(SessionError::UnsafePath);
        }
        Ok(name)
    }

    fn carbon_error(operation: &'static str, status: i32) -> SessionError {
        SessionError::Io {
            source: carbon_io_error(operation, status),
        }
    }

    fn carbon_io_error(operation: &'static str, status: i32) -> io::Error {
        io::Error::other(format!("{operation} failed with Carbon OSStatus {status}"))
    }

    fn session_error_to_io(error: SessionError) -> io::Error {
        match error {
            SessionError::Io { source } => source,
            _ => io::Error::other("unsafe Apple export temporary state"),
        }
    }
}

#[cfg(test)]
std::thread_local! {
    static FAIL_TEMPORARY_IDENTITY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    #[cfg(target_vendor = "apple")]
    static FAIL_APPLE_UNLINK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    #[cfg(target_vendor = "apple")]
    static FAIL_APPLE_OPEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    #[cfg(target_vendor = "apple")]
    static FAIL_APPLE_CLOSE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    #[cfg(target_vendor = "apple")]
    static FAIL_APPLE_DELETE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn temporary_identity_failure_injected() -> bool {
    FAIL_TEMPORARY_IDENTITY.with(|injected| injected.replace(false))
}

#[cfg(all(test, target_vendor = "apple"))]
fn apple_unlink_failure_injected() -> bool {
    FAIL_APPLE_UNLINK.with(|injected| injected.replace(false))
}

#[cfg(all(test, target_vendor = "apple"))]
fn apple_open_failure_injected() -> bool {
    FAIL_APPLE_OPEN.with(|injected| injected.replace(false))
}

#[cfg(all(test, target_vendor = "apple"))]
fn apple_close_failure_injected() -> bool {
    FAIL_APPLE_CLOSE.with(|injected| injected.replace(false))
}

#[cfg(all(test, target_vendor = "apple"))]
fn apple_delete_failure_injected() -> bool {
    FAIL_APPLE_DELETE.with(|injected| injected.replace(false))
}

#[cfg(all(test, target_vendor = "apple"))]
#[derive(Clone, Copy, Eq, PartialEq)]
enum AppleSynchronizationPoint {
    AfterPathMakeRef,
    BeforeIdentityOpen,
    BeforeObjectBoundPublish,
}

#[cfg(all(test, target_vendor = "apple"))]
mod apple_synchronization_tests {
    use std::{
        sync::{Arc, Barrier, Mutex},
        thread::{self, ThreadId},
    };

    use super::AppleSynchronizationPoint;

    struct InstalledHook {
        thread: ThreadId,
        point: AppleSynchronizationPoint,
        window: Arc<HookWindow>,
    }

    pub(super) struct HookWindow {
        reached: Barrier,
        resume: Barrier,
    }

    impl HookWindow {
        pub(super) fn new() -> Self {
            Self {
                reached: Barrier::new(2),
                resume: Barrier::new(2),
            }
        }

        fn pause_operation(&self) {
            self.reached.wait();
            self.resume.wait();
        }

        pub(super) fn wait_until_reached(&self) {
            self.reached.wait();
        }

        pub(super) fn resume_operation(&self) {
            self.resume.wait();
        }
    }

    static INSTALLED_HOOKS: Mutex<Vec<InstalledHook>> = Mutex::new(Vec::new());

    pub(super) fn install(
        thread: ThreadId,
        point: AppleSynchronizationPoint,
        window: Arc<HookWindow>,
    ) {
        INSTALLED_HOOKS.lock().unwrap().push(InstalledHook {
            thread,
            point,
            window,
        });
    }

    pub(super) fn uninstall(thread: ThreadId) {
        INSTALLED_HOOKS
            .lock()
            .unwrap()
            .retain(|hook| hook.thread != thread);
    }

    pub(super) fn hit(point: AppleSynchronizationPoint) {
        let window = {
            let hooks = INSTALLED_HOOKS.lock().unwrap();
            hooks
                .iter()
                .find(|hook| hook.thread == thread::current().id() && hook.point == point)
                .map(|hook| Arc::clone(&hook.window))
        };
        if let Some(window) = window {
            window.pause_operation();
        }
    }
}

#[cfg(all(test, windows))]
#[derive(Clone, Copy, Eq, PartialEq)]
enum CommitSynchronizationPoint {
    BeforeBoundaryValidation,
}

#[cfg(all(test, windows))]
mod commit_synchronization_tests {
    use std::{
        sync::{Arc, Barrier, Mutex},
        thread::{self, ThreadId},
    };

    use super::CommitSynchronizationPoint;

    struct InstalledHook {
        thread: ThreadId,
        point: CommitSynchronizationPoint,
        window: Arc<HookWindow>,
    }

    pub(super) struct HookWindow {
        reached: Barrier,
        resume: Barrier,
    }

    impl HookWindow {
        pub(super) fn new() -> Self {
            Self {
                reached: Barrier::new(2),
                resume: Barrier::new(2),
            }
        }

        fn pause_operation(&self) {
            self.reached.wait();
            self.resume.wait();
        }

        pub(super) fn wait_until_reached(&self) {
            self.reached.wait();
        }

        pub(super) fn resume_operation(&self) {
            self.resume.wait();
        }
    }

    static INSTALLED_HOOKS: Mutex<Vec<InstalledHook>> = Mutex::new(Vec::new());

    pub(super) fn install(
        thread: ThreadId,
        point: CommitSynchronizationPoint,
        window: Arc<HookWindow>,
    ) {
        INSTALLED_HOOKS.lock().unwrap().push(InstalledHook {
            thread,
            point,
            window,
        });
    }

    pub(super) fn uninstall(thread: ThreadId) {
        INSTALLED_HOOKS
            .lock()
            .unwrap()
            .retain(|hook| hook.thread != thread);
    }

    pub(super) fn hit(point: CommitSynchronizationPoint) {
        let window = {
            let hooks = INSTALLED_HOOKS.lock().unwrap();
            hooks
                .iter()
                .find(|hook| hook.thread == thread::current().id() && hook.point == point)
                .map(|hook| Arc::clone(&hook.window))
        };
        if let Some(window) = window {
            window.pause_operation();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, fs};

    #[cfg(any(windows, target_vendor = "apple"))]
    use std::io::Write;
    #[cfg(windows)]
    use std::{
        sync::{Arc, mpsc},
        thread,
    };

    use crate::sessions::{SessionError, SessionRootLease};

    use super::PinnedExportDestination;

    #[cfg(windows)]
    #[test]
    fn actual_parent_directory_swap_at_commit_fails_closed_and_deletes_owned_handle() {
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

        let replacement_parent = root.path().join("replacement-parent");
        fs::create_dir(&replacement_parent).unwrap();
        fs::write(
            replacement_parent.join("diagnostics.zip"),
            b"replacement target",
        )
        .unwrap();
        fs::write(replacement_parent.join("decoy"), b"replacement decoy").unwrap();
        let moved_parent = root.path().join("moved-export-parent");
        let holding_parent = root.path().join("holding");
        fs::create_dir(&holding_parent).unwrap();
        let moved_temporary = holding_parent.join("owned-temporary");

        let (thread_tx, thread_rx) = mpsc::channel();
        let (start_tx, start_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            thread_tx.send(thread::current().id()).unwrap();
            start_rx.recv().unwrap();
            destination.commit()
        });
        let worker_id = thread_rx.recv().unwrap();
        let window = Arc::new(super::commit_synchronization_tests::HookWindow::new());
        super::commit_synchronization_tests::install(
            worker_id,
            super::CommitSynchronizationPoint::BeforeBoundaryValidation,
            Arc::clone(&window),
        );
        start_tx.send(()).unwrap();
        window.wait_until_reached();

        fs::rename(export_parent.join(&temporary_name), &moved_temporary).unwrap();
        fs::rename(&export_parent, &moved_parent).unwrap();
        fs::rename(&replacement_parent, &export_parent).unwrap();

        window.resume_operation();
        let result = worker.join().unwrap();
        super::commit_synchronization_tests::uninstall(worker_id);

        assert!(matches!(result, Err(SessionError::UnsafePath)));
        assert_eq!(fs::read(&target).unwrap(), b"replacement target");
        assert_eq!(
            fs::read(export_parent.join("decoy")).unwrap(),
            b"replacement decoy"
        );
        assert!(directory_entries(&moved_parent).is_empty());
        assert_eq!(
            directory_entries(&export_parent),
            vec![OsString::from("decoy"), OsString::from("diagnostics.zip")]
        );
        assert!(directory_entries(&holding_parent).is_empty());
        assert!(!moved_temporary.exists());
        assert!(directory_entries(&session_path).is_empty());
    }

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

    #[cfg(any(windows, target_vendor = "apple"))]
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

    #[cfg(target_vendor = "apple")]
    #[test]
    fn moved_temporary_fails_parent_membership_check_and_is_removed_by_fsref() {
        let root = tempfile::tempdir().unwrap();
        let session_path = root.path().join("sessions");
        fs::create_dir(&session_path).unwrap();
        let session = SessionRootLease::open(&session_path).unwrap();
        let export_parent = root.path().join("exports");
        fs::create_dir(&export_parent).unwrap();
        let other_parent = root.path().join("other");
        fs::create_dir(&other_parent).unwrap();
        fs::write(other_parent.join("decoy"), b"decoy").unwrap();
        let target = export_parent.join("diagnostics.zip");
        let mut destination = PinnedExportDestination::create(&target, &[&session]).unwrap();
        destination.write_all(b"owned temporary").unwrap();
        let temporary_name = destination.temporary_name.clone().unwrap();
        let moved_owned = other_parent.join("moved-owned.tmp");
        fs::rename(export_parent.join(&temporary_name), &moved_owned).unwrap();

        assert!(matches!(
            destination.commit(),
            Err(SessionError::UnsafePath)
        ));
        assert!(!target.exists());
        assert!(!moved_owned.exists());
        assert_eq!(fs::read(other_parent.join("decoy")).unwrap(), b"decoy");
        assert!(directory_entries(&export_parent).is_empty());
        assert_eq!(
            directory_entries(&other_parent),
            vec![OsString::from("decoy")]
        );
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn unlink_failure_falls_back_to_object_bound_delete_without_residue() {
        let root = tempfile::tempdir().unwrap();
        let session_path = root.path().join("sessions");
        fs::create_dir(&session_path).unwrap();
        let session = SessionRootLease::open(&session_path).unwrap();
        let export_parent = root.path().join("exports");
        fs::create_dir(&export_parent).unwrap();
        let target = export_parent.join("diagnostics.zip");
        let destination = PinnedExportDestination::create(&target, &[&session]).unwrap();
        super::FAIL_APPLE_UNLINK.with(|injected| injected.set(true));

        drop(destination);

        assert!(directory_entries(&export_parent).is_empty());
        assert!(!target.exists());
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
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

#[cfg(all(test, target_vendor = "apple"))]
mod apple_regression_tests {
    use std::{
        ffi::OsString,
        fs,
        io::Write,
        path::Path,
        sync::{Arc, mpsc},
        thread,
    };

    use crate::sessions::{SessionError, SessionRootLease};

    use super::{AppleSynchronizationPoint, PinnedExportDestination};

    #[test]
    fn parent_fsref_conversion_detects_swap_restore_after_path_lookup() {
        assert_parent_fsref_swap_restore_fails_closed(AppleSynchronizationPoint::AfterPathMakeRef);
    }

    #[test]
    fn parent_fsref_conversion_detects_swap_restore_before_identity_open() {
        assert_parent_fsref_swap_restore_fails_closed(
            AppleSynchronizationPoint::BeforeIdentityOpen,
        );
    }

    #[test]
    fn object_bound_publish_never_renames_inside_a_synthetic_session_parent() {
        let fixture = AppleFixture::new();
        let target = fixture.export_parent.join("diagnostics.zip");
        let mut destination =
            PinnedExportDestination::create(&target, &[&fixture.session]).unwrap();
        destination.write_all(b"owned temporary").unwrap();
        let temporary_name = destination.temporary_name.clone().unwrap();
        let synthetic_source = fixture.session_path.join("synthetic-owned.tmp");

        let result = run_commit_at_hook(
            destination,
            AppleSynchronizationPoint::BeforeObjectBoundPublish,
            || {
                fs::rename(
                    fixture.export_parent.join(&temporary_name),
                    &synthetic_source,
                )
                .unwrap();
            },
        );

        match result {
            Ok(()) => assert_eq!(fs::read(&target).unwrap(), b"owned temporary"),
            Err(SessionError::UnsafePath | SessionError::Io { .. }) => assert!(!target.exists()),
            Err(error) => panic!("unexpected publish result: {error:?}"),
        }
        assert!(!synthetic_source.exists());
        assert!(directory_entries(&fixture.session_path).is_empty());
        assert!(
            directory_entries(&fixture.export_parent).is_empty()
                || directory_entries(&fixture.export_parent)
                    == vec![OsString::from("diagnostics.zip")]
        );
    }

    #[test]
    fn object_bound_publish_never_overwrites_a_raced_target() {
        let fixture = AppleFixture::new();
        let target = fixture.export_parent.join("diagnostics.zip");
        let mut destination =
            PinnedExportDestination::create(&target, &[&fixture.session]).unwrap();
        destination.write_all(b"owned temporary").unwrap();

        let result = run_commit_at_hook(
            destination,
            AppleSynchronizationPoint::BeforeObjectBoundPublish,
            || fs::write(&target, b"raced target").unwrap(),
        );

        assert!(matches!(result, Err(SessionError::UnsafePath)));
        assert_eq!(fs::read(&target).unwrap(), b"raced target");
        assert_eq!(
            directory_entries(&fixture.export_parent),
            vec![OsString::from("diagnostics.zip")]
        );
        assert!(directory_entries(&fixture.session_path).is_empty());
    }

    #[test]
    fn fork_open_failure_cleans_the_created_object_without_handle_growth() {
        let fixture = AppleFixture::new();
        let before = open_handle_count();

        for index in 0..32 {
            super::FAIL_APPLE_OPEN.with(|injected| injected.set(true));
            let target = fixture
                .export_parent
                .join(format!("diagnostics-{index}.zip"));
            assert!(matches!(
                PinnedExportDestination::create(&target, &[&fixture.session]),
                Err(SessionError::Io { .. })
            ));
        }

        assert!(directory_entries(&fixture.export_parent).is_empty());
        assert!(open_handle_count() <= before + 2);
    }

    #[test]
    fn fork_close_failure_retains_ownership_until_drop_cleanup() {
        let fixture = AppleFixture::new();
        let before = open_handle_count();

        for index in 0..32 {
            let target = fixture
                .export_parent
                .join(format!("diagnostics-{index}.zip"));
            let mut destination =
                PinnedExportDestination::create(&target, &[&fixture.session]).unwrap();
            destination.write_all(b"owned temporary").unwrap();
            super::FAIL_APPLE_CLOSE.with(|injected| injected.set(true));

            assert!(matches!(destination.commit(), Err(SessionError::Io { .. })));
            assert!(!target.exists());
        }

        assert!(directory_entries(&fixture.export_parent).is_empty());
        assert!(open_handle_count() <= before + 2);
    }

    #[test]
    fn unlink_and_delete_failures_retain_object_for_drop_retry() {
        let fixture = AppleFixture::new();
        let target = fixture.export_parent.join("diagnostics.zip");
        let destination = PinnedExportDestination::create(&target, &[&fixture.session]).unwrap();
        super::FAIL_APPLE_UNLINK.with(|injected| injected.set(true));
        super::FAIL_APPLE_DELETE.with(|injected| injected.set(true));

        drop(destination);

        assert!(directory_entries(&fixture.export_parent).is_empty());
        assert!(!target.exists());
    }

    fn assert_parent_fsref_swap_restore_fails_closed(point: AppleSynchronizationPoint) {
        let fixture = AppleFixture::new();
        let replacement_parent = fixture.root.path().join("replacement");
        fs::create_dir(&replacement_parent).unwrap();
        let moved_parent = fixture.root.path().join("moved-export");
        let target = fixture.export_parent.join("diagnostics.zip");
        let session = fixture.session;

        let (thread_tx, thread_rx) = mpsc::channel();
        let (start_tx, start_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            thread_tx.send(thread::current().id()).unwrap();
            start_rx.recv().unwrap();
            PinnedExportDestination::create(&target, &[&session])
        });
        let worker_id = thread_rx.recv().unwrap();
        let window = Arc::new(super::apple_synchronization_tests::HookWindow::new());
        super::apple_synchronization_tests::install(worker_id, point, Arc::clone(&window));
        start_tx.send(()).unwrap();
        window.wait_until_reached();

        fs::rename(&fixture.export_parent, &moved_parent).unwrap();
        fs::rename(&replacement_parent, &fixture.export_parent).unwrap();
        fs::rename(&fixture.export_parent, &replacement_parent).unwrap();
        fs::rename(&moved_parent, &fixture.export_parent).unwrap();

        window.resume_operation();
        let result = worker.join().unwrap();
        super::apple_synchronization_tests::uninstall(worker_id);

        assert!(matches!(result, Err(SessionError::UnsafePath)));
        assert!(directory_entries(&fixture.export_parent).is_empty());
        assert!(directory_entries(&replacement_parent).is_empty());
        assert!(directory_entries(&fixture.session_path).is_empty());
    }

    fn run_commit_at_hook(
        destination: PinnedExportDestination,
        point: AppleSynchronizationPoint,
        action: impl FnOnce(),
    ) -> Result<(), SessionError> {
        let (thread_tx, thread_rx) = mpsc::channel();
        let (start_tx, start_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            thread_tx.send(thread::current().id()).unwrap();
            start_rx.recv().unwrap();
            destination.commit()
        });
        let worker_id = thread_rx.recv().unwrap();
        let window = Arc::new(super::apple_synchronization_tests::HookWindow::new());
        super::apple_synchronization_tests::install(worker_id, point, Arc::clone(&window));
        start_tx.send(()).unwrap();
        window.wait_until_reached();
        action();
        window.resume_operation();
        let result = worker.join().unwrap();
        super::apple_synchronization_tests::uninstall(worker_id);
        result
    }

    struct AppleFixture {
        root: tempfile::TempDir,
        session_path: std::path::PathBuf,
        session: SessionRootLease,
        export_parent: std::path::PathBuf,
    }

    impl AppleFixture {
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

    fn open_handle_count() -> usize {
        fs::read_dir("/dev/fd").unwrap().count()
    }
}
