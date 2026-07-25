use std::{
    fs::{self, File},
    path::Path,
};

use crate::PlatformError;

const PUBLISH_STAGING_NAME: &str = ".wokcore-publish-staging";
const RETIRED_DISCOVERY_NAME: &str = ".wokcore-retired-discovery";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RuntimeDirectoryIdentity([u64; 2]);

#[cfg(unix)]
pub(super) fn open_or_create_runtime_directory(path: &Path) -> Result<File, PlatformError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(path) {
                Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700))?,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(source) => return Err(PlatformError::Io { source }),
            }
        }
        Err(source) => return Err(PlatformError::Io { source }),
    }

    open_existing_runtime_directory(path)
}

#[cfg(unix)]
pub(super) fn open_existing_runtime_directory(path: &Path) -> Result<File, PlatformError> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|source| match source.raw_os_error() {
            Some(libc::ELOOP | libc::ENOTDIR) => PlatformError::UnsafeRuntimePath,
            _ => PlatformError::Io { source },
        })?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o7777 != 0o700 {
        return Err(PlatformError::UnsafeRuntimePath);
    }
    verify_unix_owner(&metadata)?;
    Ok(file)
}

#[cfg(unix)]
pub(super) fn runtime_directory_identity(
    directory: &File,
) -> Result<RuntimeDirectoryIdentity, PlatformError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = directory.metadata()?;
    Ok(RuntimeDirectoryIdentity([metadata.dev(), metadata.ino()]))
}

#[cfg(unix)]
pub(super) fn open_or_create_secure_file(
    directory: &File,
    path: &Path,
) -> Result<File, PlatformError> {
    let name = unix_child_name(path)?;
    let file = match unix_openat(
        directory,
        &name,
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
        0o600,
    ) {
        Ok(file) => file,
        Err(PlatformError::Io { source }) if source.raw_os_error() == Some(libc::EEXIST) => {
            unix_openat(directory, &name, libc::O_RDWR | libc::O_NOFOLLOW, 0)?
        }
        Err(error) => return Err(error),
    };
    verify_unix_file(&file)?;
    Ok(file)
}

#[cfg(unix)]
pub(super) fn open_existing_secure_file(
    directory: &File,
    path: &Path,
) -> Result<File, PlatformError> {
    let file = unix_openat(
        directory,
        &unix_child_name(path)?,
        libc::O_RDONLY | libc::O_NOFOLLOW,
        0,
    )?;
    verify_unix_file(&file)?;
    Ok(file)
}

#[cfg(unix)]
pub(super) fn open_existing_secure_file_for_update(
    directory: &File,
    path: &Path,
) -> Result<File, PlatformError> {
    open_existing_secure_file(directory, path)
}

#[cfg(unix)]
fn unix_child_name(path: &Path) -> Result<std::ffi::CString, PlatformError> {
    use std::os::unix::ffi::OsStrExt;

    let name = path.file_name().ok_or(PlatformError::UnsafeRuntimePath)?;
    std::ffi::CString::new(name.as_bytes()).map_err(|_| PlatformError::UnsafeRuntimePath)
}

#[cfg(unix)]
fn unix_openat(
    directory: &File,
    name: &std::ffi::CStr,
    flags: i32,
    mode: libc::mode_t,
) -> Result<File, PlatformError> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode,
        )
    };
    if descriptor < 0 {
        let source = std::io::Error::last_os_error();
        return Err(match source.raw_os_error() {
            Some(libc::ELOOP | libc::EISDIR | libc::ENOTDIR) => PlatformError::UnsafeRuntimePath,
            _ => PlatformError::Io { source },
        });
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn unix_rename_noreplace(
    directory: &File,
    source: &std::ffi::CStr,
    destination: &std::ffi::CStr,
) -> Result<(), PlatformError> {
    use std::os::fd::AsRawFd;

    let descriptor = directory.as_raw_fd();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let result = unsafe {
        libc::renameat2(
            descriptor,
            source.as_ptr(),
            descriptor,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_vendor = "apple")]
    let result = unsafe {
        libc::renameatx_np(
            descriptor,
            source.as_ptr(),
            descriptor,
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    return Err(PlatformError::UnsafeRuntimePath);

    if result != 0 {
        let source = std::io::Error::last_os_error();
        return Err(match source.raw_os_error() {
            Some(libc::EEXIST) => PlatformError::UnsafeRuntimePath,
            _ => PlatformError::Io { source },
        });
    }
    Ok(())
}

#[cfg(unix)]
fn unix_exchange(
    directory: &File,
    first: &std::ffi::CStr,
    second: &std::ffi::CStr,
) -> Result<(), PlatformError> {
    use std::os::fd::AsRawFd;

    let descriptor = directory.as_raw_fd();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let result = unsafe {
        libc::renameat2(
            descriptor,
            first.as_ptr(),
            descriptor,
            second.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    #[cfg(target_vendor = "apple")]
    let result = unsafe {
        libc::renameatx_np(
            descriptor,
            first.as_ptr(),
            descriptor,
            second.as_ptr(),
            libc::RENAME_SWAP,
        )
    };
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    return Err(PlatformError::UnsafeRuntimePath);

    if result != 0 {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn verify_unix_file(file: &File) -> Result<(), PlatformError> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o7777 != 0o600 {
        return Err(PlatformError::UnsafeRuntimePath);
    }
    verify_unix_owner(&metadata)?;
    Ok(())
}

#[cfg(unix)]
pub(super) fn harden_secure_file(file: &File, _path: &Path) -> Result<(), PlatformError> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    verify_unix_file(file)
}

#[cfg(unix)]
pub(super) fn prepare_secure_publish(
    directory: &File,
    _runtime_path: &Path,
) -> Result<(), PlatformError> {
    for name in [PUBLISH_STAGING_NAME, RETIRED_DISCOVERY_NAME] {
        cleanup_unix_internal_entry(directory, name, None)?;
    }
    Ok(())
}

#[cfg(unix)]
fn cleanup_unix_internal_entry(
    directory: &File,
    reserved_name: &str,
    expected: Option<(u64, u64)>,
) -> Result<(), PlatformError> {
    use std::os::fd::AsRawFd;

    let name = std::ffi::CString::new(reserved_name)
        .expect("reserved internal runtime name contains no NUL");
    let file = match unix_openat(directory, &name, libc::O_RDONLY | libc::O_NOFOLLOW, 0) {
        Ok(file) => file,
        Err(PlatformError::Io { source }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    verify_unix_file(&file)?;
    let metadata = file.metadata()?;
    if expected.is_some_and(|identity| identity != unix_file_identity(&metadata)) {
        return Err(PlatformError::UnsafeRuntimePath);
    }
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn unix_file_identity(metadata: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;

    (metadata.dev(), metadata.ino())
}

#[cfg(unix)]
#[cfg(unix)]
pub(super) fn publish_secure_file(
    directory: &File,
    destination: &Path,
    document: &[u8],
    existing: Option<File>,
) -> Result<(), PlatformError> {
    use std::io::Write;

    let destination = unix_child_name(destination)?;
    let staging_name = std::ffi::CString::new(PUBLISH_STAGING_NAME)
        .expect("reserved internal runtime name contains no NUL");
    let mut temporary = unix_openat(
        directory,
        &staging_name,
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
        0o600,
    )?;
    harden_secure_file(&temporary, Path::new(""))?;
    temporary.write_all(document)?;
    temporary.sync_all()?;
    #[cfg(test)]
    run_publish_after_temp_creation_hook()?;

    if let Some(existing) = existing {
        let expected = existing.metadata()?;
        let current = unix_openat(
            directory,
            &destination,
            libc::O_RDONLY | libc::O_NOFOLLOW,
            0,
        )?;
        verify_unix_file(&current)?;
        if unix_file_identity(&current.metadata()?) != unix_file_identity(&expected) {
            return Err(PlatformError::UnsafeRuntimePath);
        }

        unix_exchange(directory, &staging_name, &destination)?;
        #[cfg(test)]
        run_publish_after_existing_commit_hook()?;
        cleanup_unix_internal_entry(
            directory,
            PUBLISH_STAGING_NAME,
            Some(unix_file_identity(&expected)),
        )?;
        return Ok(());
    }

    unix_rename_noreplace(directory, &staging_name, &destination)
}

#[cfg(unix)]
pub(super) fn remove_open_secure_file(
    directory: &File,
    file: File,
    path: &Path,
) -> Result<(), PlatformError> {
    use std::os::unix::fs::MetadataExt;

    let opened = file.metadata()?;
    #[cfg(test)]
    run_unix_remove_after_identity_check_hook();

    prepare_secure_publish(directory, Path::new(""))?;
    let canonical = unix_child_name(path)?;
    let tombstone = std::ffi::CString::new(RETIRED_DISCOVERY_NAME)
        .expect("fixed retired discovery name contains no NUL");
    unix_rename_noreplace(directory, &canonical, &tombstone)?;
    let tombstone_file = unix_openat(directory, &tombstone, libc::O_RDONLY | libc::O_NOFOLLOW, 0)?;
    let matches_opened = tombstone_file.metadata().is_ok_and(|metadata| {
        metadata.is_file() && metadata.dev() == opened.dev() && metadata.ino() == opened.ino()
    });
    #[cfg(test)]
    run_unix_remove_after_quarantine_verification_hook(&tombstone, matches_opened);
    if !matches_opened {
        return Err(PlatformError::UnsafeRuntimePath);
    }

    Ok(())
}

#[cfg(all(test, unix))]
thread_local! {
    static UNIX_REMOVE_AFTER_IDENTITY_CHECK_HOOK:
        std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, unix))]
fn run_unix_remove_after_identity_check_hook() {
    UNIX_REMOVE_AFTER_IDENTITY_CHECK_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(all(test, unix))]
fn with_unix_remove_after_identity_check_hook<T>(
    hook: impl FnOnce() + 'static,
    operation: impl FnOnce() -> T,
) -> T {
    UNIX_REMOVE_AFTER_IDENTITY_CHECK_HOOK.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(Box::new(hook));
    });

    struct ResetHook;

    impl Drop for ResetHook {
        fn drop(&mut self) {
            UNIX_REMOVE_AFTER_IDENTITY_CHECK_HOOK.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }

    let _reset = ResetHook;
    operation()
}

#[cfg(all(test, unix))]
type UnixRemoveAfterQuarantineVerificationHook = Box<dyn FnOnce(&std::ffi::CStr, bool)>;

#[cfg(all(test, unix))]
thread_local! {
    static UNIX_REMOVE_AFTER_QUARANTINE_VERIFICATION_HOOK:
        std::cell::RefCell<Option<UnixRemoveAfterQuarantineVerificationHook>> =
            const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, unix))]
fn run_unix_remove_after_quarantine_verification_hook(
    quarantine: &std::ffi::CStr,
    matches_opened: bool,
) {
    UNIX_REMOVE_AFTER_QUARANTINE_VERIFICATION_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook(quarantine, matches_opened);
        }
    });
}

#[cfg(all(test, unix))]
fn with_unix_remove_after_quarantine_verification_hook<T>(
    hook: impl FnOnce(&std::ffi::CStr, bool) + 'static,
    operation: impl FnOnce() -> T,
) -> T {
    UNIX_REMOVE_AFTER_QUARANTINE_VERIFICATION_HOOK.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(Box::new(hook));
    });

    struct ResetHook;

    impl Drop for ResetHook {
        fn drop(&mut self) {
            UNIX_REMOVE_AFTER_QUARANTINE_VERIFICATION_HOOK.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }

    let _reset = ResetHook;
    operation()
}

#[cfg(test)]
type PublishAfterTempCreationHook = Box<dyn FnOnce() -> Result<(), PlatformError>>;

#[cfg(test)]
thread_local! {
    static PUBLISH_AFTER_TEMP_CREATION_HOOK:
        std::cell::RefCell<Option<PublishAfterTempCreationHook>> =
            const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn run_publish_after_temp_creation_hook() -> Result<(), PlatformError> {
    PUBLISH_AFTER_TEMP_CREATION_HOOK.with(|slot| match slot.borrow_mut().take() {
        Some(hook) => hook(),
        None => Ok(()),
    })
}

#[cfg(test)]
fn with_publish_after_temp_creation_hook<T>(
    hook: impl FnOnce() -> Result<(), PlatformError> + 'static,
    operation: impl FnOnce() -> T,
) -> T {
    PUBLISH_AFTER_TEMP_CREATION_HOOK.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(Box::new(hook));
    });

    struct ResetHook;

    impl Drop for ResetHook {
        fn drop(&mut self) {
            PUBLISH_AFTER_TEMP_CREATION_HOOK.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }

    let _reset = ResetHook;
    operation()
}

#[cfg(test)]
type PublishAfterExistingCommitHook = Box<dyn FnOnce() -> Result<(), PlatformError>>;

#[cfg(test)]
thread_local! {
    static PUBLISH_AFTER_EXISTING_COMMIT_HOOK:
        std::cell::RefCell<Option<PublishAfterExistingCommitHook>> =
            const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn run_publish_after_existing_commit_hook() -> Result<(), PlatformError> {
    PUBLISH_AFTER_EXISTING_COMMIT_HOOK.with(|slot| match slot.borrow_mut().take() {
        Some(hook) => hook(),
        None => Ok(()),
    })
}

#[cfg(test)]
fn with_publish_after_existing_commit_hook<T>(
    hook: impl FnOnce() -> Result<(), PlatformError> + 'static,
    operation: impl FnOnce() -> T,
) -> T {
    PUBLISH_AFTER_EXISTING_COMMIT_HOOK.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(Box::new(hook));
    });

    struct ResetHook;

    impl Drop for ResetHook {
        fn drop(&mut self) {
            PUBLISH_AFTER_EXISTING_COMMIT_HOOK.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }

    let _reset = ResetHook;
    operation()
}

#[cfg(unix)]
pub(super) fn sync_parent_directory(directory: &File) -> Result<(), PlatformError> {
    directory.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn verify_unix_owner(metadata: &fs::Metadata) -> Result<(), PlatformError> {
    use std::os::unix::fs::MetadataExt;

    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(PlatformError::UnsafeRuntimePath);
    }
    Ok(())
}

#[cfg(windows)]
pub(super) fn open_or_create_runtime_directory(path: &Path) -> Result<File, PlatformError> {
    use windows_sys::Win32::{
        Foundation::ERROR_ALREADY_EXISTS, Storage::FileSystem::CreateDirectoryW,
    };

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let wide_path = wide_path(path);
    with_current_user_security_attributes(true, |attributes| {
        if unsafe { CreateDirectoryW(wide_path.as_ptr(), attributes) } == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32) {
                return Err(PlatformError::Io { source: error });
            }
        }
        Ok(())
    })?;

    open_existing_runtime_directory(path)
}

#[cfg(windows)]
pub(super) fn open_existing_runtime_directory(path: &Path) -> Result<File, PlatformError> {
    open_windows_file(path, WindowsFileKind::Directory)
}

#[cfg(windows)]
pub(super) fn runtime_directory_identity(
    directory: &File,
) -> Result<RuntimeDirectoryIdentity, PlatformError> {
    let (volume, high, low) = windows_file_identity(directory)?;
    Ok(RuntimeDirectoryIdentity([
        u64::from(volume),
        u64::from(high) << 32 | u64::from(low),
    ]))
}

#[cfg(windows)]
pub(super) fn open_or_create_secure_file(
    _directory: &File,
    path: &Path,
) -> Result<File, PlatformError> {
    use std::{os::windows::io::FromRawHandle, ptr};

    use windows_sys::Win32::{
        Foundation::{
            ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, GENERIC_READ, GENERIC_WRITE,
            INVALID_HANDLE_VALUE,
        },
        Storage::FileSystem::{
            CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_READ, FILE_SHARE_WRITE,
        },
    };

    let wide_path = wide_path(path);
    let created = with_current_user_security_attributes(false, |attributes| {
        let handle = unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let source = std::io::Error::last_os_error();
            return match source.raw_os_error() {
                Some(error)
                    if error == ERROR_FILE_EXISTS as i32
                        || error == ERROR_ALREADY_EXISTS as i32 =>
                {
                    Ok(None)
                }
                _ => Err(PlatformError::Io { source }),
            };
        }
        Ok(Some(handle))
    })?;
    match created {
        Some(handle) => {
            let file = unsafe { File::from_raw_handle(handle) };
            verify_windows_file(&file, WindowsFileKind::Regular)?;
            verify_windows_permissions(&file)?;
            Ok(file)
        }
        None => open_windows_existing_file(path, WindowsExistingFileMode::Read),
    }
}

#[cfg(windows)]
pub(super) fn open_existing_secure_file(
    _directory: &File,
    path: &Path,
) -> Result<File, PlatformError> {
    open_windows_existing_file(path, WindowsExistingFileMode::DiscoveryRead)
}

#[cfg(windows)]
pub(super) fn open_existing_secure_file_for_update(
    _directory: &File,
    path: &Path,
) -> Result<File, PlatformError> {
    open_windows_existing_file(path, WindowsExistingFileMode::Update)
}

#[cfg(windows)]
#[derive(Clone, Copy)]
enum WindowsFileKind {
    Directory,
    Regular,
}

#[cfg(windows)]
#[derive(Clone, Copy)]
enum WindowsExistingFileMode {
    Read,
    DiscoveryRead,
    Update,
}

#[cfg(windows)]
fn open_windows_file(path: &Path, kind: WindowsFileKind) -> Result<File, PlatformError> {
    use std::{os::windows::io::FromRawHandle, ptr};

    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
            FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL,
        },
    };

    let path = wide_path(path);
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE | READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    let file = unsafe { File::from_raw_handle(handle) };
    verify_windows_file(&file, kind)?;
    verify_windows_permissions(&file)?;
    Ok(file)
}

#[cfg(windows)]
fn open_windows_existing_file(
    path: &Path,
    mode: WindowsExistingFileMode,
) -> Result<File, PlatformError> {
    use std::{
        os::windows::{fs::MetadataExt, io::FromRawHandle},
        ptr,
    };

    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            CreateFileW, DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL,
        },
    };

    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    {
        return Err(PlatformError::UnsafeRuntimePath);
    }

    let path = wide_path(path);
    let desired_access = GENERIC_READ
        | READ_CONTROL
        | if matches!(mode, WindowsExistingFileMode::Update) {
            DELETE
        } else {
            0
        };
    let share_mode = FILE_SHARE_READ
        | FILE_SHARE_WRITE
        | if matches!(mode, WindowsExistingFileMode::DiscoveryRead) {
            FILE_SHARE_DELETE
        } else {
            0
        };
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            desired_access,
            share_mode,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let source = std::io::Error::last_os_error();
        return Err(match source.raw_os_error() {
            Some(error)
                if error == windows_sys::Win32::Foundation::ERROR_CANT_ACCESS_FILE as i32 =>
            {
                PlatformError::UnsafeRuntimePath
            }
            _ => PlatformError::Io { source },
        });
    }
    let file = unsafe { File::from_raw_handle(handle) };
    verify_windows_file(&file, WindowsFileKind::Regular)?;
    verify_windows_permissions(&file)?;
    Ok(file)
}

#[cfg(windows)]
fn verify_windows_file(file: &File, kind: WindowsFileKind) -> Result<(), PlatformError> {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        GetFileInformationByHandle,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    let attributes = information.dwFileAttributes;
    let is_directory = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    let expected_kind = match kind {
        WindowsFileKind::Directory => is_directory,
        WindowsFileKind::Regular => !is_directory,
    };
    if !expected_kind || attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(PlatformError::UnsafeRuntimePath);
    }
    Ok(())
}

#[cfg(windows)]
fn with_current_user_security_attributes<T>(
    inherit_to_children: bool,
    operation: impl FnOnce(
        *const windows_sys::Win32::Security::SECURITY_ATTRIBUTES,
    ) -> Result<T, PlatformError>,
) -> Result<T, PlatformError> {
    use std::{ffi::c_void, mem::size_of, ptr};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, GENERIC_ALL, HANDLE},
        Security::{
            ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAceEx, CONTAINER_INHERIT_ACE,
            GetLengthSid, GetTokenInformation, InitializeAcl, InitializeSecurityDescriptor,
            OBJECT_INHERIT_ACE, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
            SetSecurityDescriptorControl, SetSecurityDescriptorDacl, SetSecurityDescriptorOwner,
            TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        System::{
            SystemServices::SECURITY_DESCRIPTOR_REVISION,
            Threading::{GetCurrentProcess, OpenProcessToken},
        },
    };

    struct Token(HANDLE);

    impl Drop for Token {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    let mut token_handle: HANDLE = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token_handle) } == 0 {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    let token = Token(token_handle);
    let mut required = 0;
    unsafe {
        GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut required);
    }
    if required == 0 {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    let mut user_buffer = vec![0_usize; (required as usize).div_ceil(size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            user_buffer.as_mut_ptr().cast::<c_void>(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    let token_user = unsafe { &*(user_buffer.as_ptr().cast::<TOKEN_USER>()) };
    let user_sid = token_user.User.Sid;

    let acl_size = size_of::<ACL>() + size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>()
        + unsafe { GetLengthSid(user_sid) as usize };
    let mut acl_buffer = vec![0_usize; acl_size.div_ceil(size_of::<usize>())];
    let acl = acl_buffer.as_mut_ptr().cast::<ACL>();
    if unsafe { InitializeAcl(acl, acl_size as u32, ACL_REVISION) } == 0 {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    let ace_flags = if inherit_to_children {
        OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
    } else {
        0
    };
    if unsafe { AddAccessAllowedAceEx(acl, ACL_REVISION, ace_flags, GENERIC_ALL, user_sid) } == 0 {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
    }

    let mut descriptor = SECURITY_DESCRIPTOR::default();
    if unsafe {
        InitializeSecurityDescriptor(
            (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
            SECURITY_DESCRIPTOR_REVISION,
        )
    } == 0
        || unsafe {
            SetSecurityDescriptorOwner(
                (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                user_sid,
                0,
            )
        } == 0
        || unsafe {
            SetSecurityDescriptorDacl(
                (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                1,
                acl,
                0,
            )
        } == 0
        || unsafe {
            SetSecurityDescriptorControl(
                (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                SE_DACL_PROTECTED,
                SE_DACL_PROTECTED,
            )
        } == 0
    {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
    }

    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
        bInheritHandle: 0,
    };
    operation(&attributes)
}

#[cfg(windows)]
pub(super) fn harden_secure_file(file: &File, path: &Path) -> Result<(), PlatformError> {
    use std::ptr;

    use windows_sys::Win32::{
        Foundation::ERROR_SUCCESS,
        Security::{
            Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW},
            DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
            PROTECTED_DACL_SECURITY_INFORMATION, SECURITY_DESCRIPTOR,
        },
    };

    let wide_path = wide_path(path);
    with_current_user_security_attributes(false, |attributes| {
        let descriptor = unsafe {
            &*(attributes
                .as_ref()
                .unwrap()
                .lpSecurityDescriptor
                .cast::<SECURITY_DESCRIPTOR>())
        };
        let status = unsafe {
            SetNamedSecurityInfoW(
                wide_path.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                descriptor.Owner,
                ptr::null_mut(),
                descriptor.Dacl,
                ptr::null(),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(PlatformError::Io {
                source: std::io::Error::from_raw_os_error(status as i32),
            });
        }
        Ok(())
    })?;
    verify_windows_file(file, WindowsFileKind::Regular)?;
    verify_windows_permissions(file)
}

#[cfg(windows)]
pub(super) fn prepare_secure_publish(
    _directory: &File,
    runtime_path: &Path,
) -> Result<(), PlatformError> {
    for name in [PUBLISH_STAGING_NAME, RETIRED_DISCOVERY_NAME] {
        cleanup_windows_fixed_internal_entry(&runtime_path.join(name))?;
    }
    Ok(())
}

#[cfg(windows)]
fn cleanup_windows_fixed_internal_entry(path: &Path) -> Result<(), PlatformError> {
    let file = match open_windows_existing_file(path, WindowsExistingFileMode::Update) {
        Ok(file) => file,
        Err(PlatformError::Io { source }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    delete_windows_file(file)
}

#[cfg(windows)]
fn cleanup_windows_internal_entry(
    path: &Path,
    expected: (u32, u32, u32),
) -> Result<(), PlatformError> {
    let file = open_windows_existing_file(path, WindowsExistingFileMode::Update)?;
    if windows_file_identity(&file)? != expected {
        return Err(PlatformError::UnsafeRuntimePath);
    }
    delete_windows_file(file)
}

#[cfg(windows)]
fn windows_file_identity(file: &File) -> Result<(u32, u32, u32), PlatformError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    Ok((
        information.dwVolumeSerialNumber,
        information.nFileIndexHigh,
        information.nFileIndexLow,
    ))
}

#[cfg(windows)]
#[cfg(windows)]
pub(super) fn publish_secure_file(
    _directory: &File,
    destination: &Path,
    document: &[u8],
    existing: Option<File>,
) -> Result<(), PlatformError> {
    use std::io::Write;

    let parent = destination
        .parent()
        .ok_or(PlatformError::UnsafeRuntimePath)?;
    let mut temporary = create_windows_publish_temporary(parent)?;
    harden_secure_file(temporary.as_file(), temporary.path())?;
    temporary.write_all(document)?;
    temporary.as_file().sync_all()?;
    #[cfg(test)]
    run_publish_after_temp_creation_hook()?;

    if let Some(existing) = existing {
        let expected = windows_file_identity(&existing)?;
        let retired = parent.join(RETIRED_DISCOVERY_NAME);
        drop(existing);
        let (temporary_file, temporary_path) = temporary.into_parts();
        drop(temporary_file);
        replace_windows_file(destination, &temporary_path, &retired)?;
        #[cfg(test)]
        run_publish_after_existing_commit_hook()?;
        drop(open_windows_existing_file(
            destination,
            WindowsExistingFileMode::DiscoveryRead,
        )?);
        cleanup_windows_internal_entry(&retired, expected)?;
        return Ok(());
    }

    let persisted =
        temporary
            .persist_noclobber(destination)
            .map_err(|error| match error.error.kind() {
                std::io::ErrorKind::AlreadyExists => PlatformError::UnsafeRuntimePath,
                _ => PlatformError::Io {
                    source: error.error,
                },
            })?;
    persisted.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn create_windows_publish_temporary(
    parent: &Path,
) -> Result<tempfile::NamedTempFile, PlatformError> {
    use std::{os::windows::io::FromRawHandle, ptr};

    use windows_sys::Win32::{
        Foundation::{
            ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, GENERIC_READ, GENERIC_WRITE,
            INVALID_HANDLE_VALUE,
        },
        Storage::FileSystem::{
            CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        },
    };

    let path = parent.join(PUBLISH_STAGING_NAME);
    let wide_path = wide_path(&path);
    let handle = with_current_user_security_attributes(false, |attributes| {
        let handle = unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let source = std::io::Error::last_os_error();
            return Err(match source.raw_os_error() {
                Some(error)
                    if error == ERROR_FILE_EXISTS as i32
                        || error == ERROR_ALREADY_EXISTS as i32 =>
                {
                    PlatformError::UnsafeRuntimePath
                }
                _ => PlatformError::Io { source },
            });
        }
        Ok(handle)
    })?;
    let file = unsafe { File::from_raw_handle(handle) };
    let temporary_path = tempfile::TempPath::try_from_path(path)?;
    Ok(tempfile::NamedTempFile::from_parts(file, temporary_path))
}

#[cfg(windows)]
fn replace_windows_file(
    destination: &Path,
    replacement: &Path,
    retired: &Path,
) -> Result<(), PlatformError> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};

    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let replacement = replacement
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let retired = retired
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    if unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            replacement.as_ptr(),
            retired.as_ptr(),
            REPLACEFILE_WRITE_THROUGH,
            std::ptr::null(),
            std::ptr::null(),
        )
    } == 0
    {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

#[cfg(windows)]
fn delete_windows_file(file: File) -> Result<(), PlatformError> {
    use std::{mem::size_of, os::windows::io::AsRawHandle};

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
    };

    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&raw const disposition).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    drop(file);
    Ok(())
}

#[cfg(windows)]
pub(super) fn remove_open_secure_file(
    _directory: &File,
    file: File,
    path: &Path,
) -> Result<(), PlatformError> {
    let expected = windows_file_identity(&file)?;
    drop(file);
    let file = open_windows_existing_file(path, WindowsExistingFileMode::Update)?;
    if windows_file_identity(&file)? != expected {
        return Err(PlatformError::UnsafeRuntimePath);
    }
    delete_windows_file(file)
}

#[cfg(windows)]
pub(super) fn sync_parent_directory(directory: &File) -> Result<(), PlatformError> {
    directory.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn verify_windows_permissions(file: &File) -> Result<(), PlatformError> {
    use std::{ffi::c_void, mem::size_of, os::windows::io::AsRawHandle, ptr};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree},
        Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
            Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
            DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
            GetSecurityDescriptorControl, GetTokenInformation, OWNER_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        System::{
            SystemServices::ACCESS_ALLOWED_ACE_TYPE,
            Threading::{GetCurrentProcess, OpenProcessToken},
        },
    };

    struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    LocalFree(self.0.cast());
                }
            }
        }
    }

    struct Token(HANDLE);

    impl Drop for Token {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    let mut owner: PSID = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    let descriptor = SecurityDescriptor(descriptor);
    if status != ERROR_SUCCESS {
        return Err(PlatformError::Io {
            source: std::io::Error::from_raw_os_error(status as i32),
        });
    }
    if owner.is_null() || dacl.is_null() {
        return Err(PlatformError::UnsafeRuntimePath);
    }

    let mut control = 0;
    let mut revision = 0;
    if unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) } == 0 {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    if control & SE_DACL_PROTECTED == 0 {
        return Err(PlatformError::UnsafeRuntimePath);
    }

    let mut token_handle: HANDLE = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token_handle) } == 0 {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    let token = Token(token_handle);
    let mut required = 0;
    unsafe {
        GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut required);
    }
    if required == 0 {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    let mut user_buffer = vec![0_usize; (required as usize).div_ceil(size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            user_buffer.as_mut_ptr().cast::<c_void>(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    let token_user = unsafe { &*(user_buffer.as_ptr().cast::<TOKEN_USER>()) };
    if unsafe { EqualSid(owner, token_user.User.Sid) } == 0 {
        return Err(PlatformError::UnsafeRuntimePath);
    }

    let mut acl_info = ACL_SIZE_INFORMATION::default();
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut acl_info as *mut ACL_SIZE_INFORMATION).cast::<c_void>(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    if acl_info.AceCount != 1 {
        return Err(PlatformError::UnsafeRuntimePath);
    }

    let mut ace: *mut c_void = ptr::null_mut();
    if unsafe { GetAce(dacl, 0, &mut ace) } == 0 {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    let header = unsafe { &*ace.cast::<ACE_HEADER>() };
    if header.AceType as u32 != ACCESS_ALLOWED_ACE_TYPE {
        return Err(PlatformError::UnsafeRuntimePath);
    }
    let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
    let allowed_sid = (&raw const allowed.SidStart).cast_mut().cast::<c_void>();
    if unsafe { EqualSid(allowed_sid, token_user.User.Sid) } == 0 {
        return Err(PlatformError::UnsafeRuntimePath);
    }
    Ok(())
}

#[cfg(windows)]
fn wide_path(path: &Path) -> Vec<u16> {
    use std::{iter, os::windows::ffi::OsStrExt};

    path.as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect()
}

#[cfg(not(any(unix, windows)))]
pub(super) fn open_or_create_runtime_directory(_path: &Path) -> Result<File, PlatformError> {
    Err(PlatformError::UnsafeRuntimePath)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn runtime_directory_identity(
    _directory: &File,
) -> Result<RuntimeDirectoryIdentity, PlatformError> {
    Err(PlatformError::UnsafeRuntimePath)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn open_existing_secure_file_for_update(
    _directory: &File,
    _path: &Path,
) -> Result<File, PlatformError> {
    Err(PlatformError::UnsafeRuntimePath)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn open_or_create_secure_file(
    _directory: &File,
    _path: &Path,
) -> Result<File, PlatformError> {
    Err(PlatformError::UnsafeRuntimePath)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn open_existing_runtime_directory(_path: &Path) -> Result<File, PlatformError> {
    Err(PlatformError::UnsafeRuntimePath)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn open_existing_secure_file(
    _directory: &File,
    _path: &Path,
) -> Result<File, PlatformError> {
    Err(PlatformError::UnsafeRuntimePath)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn harden_secure_file(_file: &File, _path: &Path) -> Result<(), PlatformError> {
    Err(PlatformError::UnsafeRuntimePath)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn prepare_secure_publish(
    _directory: &File,
    _runtime_path: &Path,
) -> Result<(), PlatformError> {
    Err(PlatformError::UnsafeRuntimePath)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn publish_secure_file(
    _directory: &File,
    _destination: &Path,
    _document: &[u8],
    _existing: Option<File>,
) -> Result<(), PlatformError> {
    Err(PlatformError::UnsafeRuntimePath)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn remove_open_secure_file(
    _directory: &File,
    _file: File,
    _path: &Path,
) -> Result<(), PlatformError> {
    Err(PlatformError::UnsafeRuntimePath)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn sync_parent_directory(_directory: &File) -> Result<(), PlatformError> {
    Err(PlatformError::UnsafeRuntimePath)
}

#[cfg(test)]
mod publish_tests {
    use std::{fs, io, path::Path};

    use crate::PlatformError;
    #[cfg(windows)]
    use crate::{AppPaths, DiscoveryRecord, DiscoveryStore, RuntimeLease};

    #[cfg(windows)]
    use super::open_existing_secure_file;
    #[cfg(any(unix, windows))]
    use super::open_existing_secure_file_for_update;
    use super::{
        open_or_create_runtime_directory, prepare_secure_publish, publish_secure_file,
        with_publish_after_existing_commit_hook, with_publish_after_temp_creation_hook,
    };

    #[test]
    fn publish_preparation_does_not_enumerate_or_collect_runtime_directory_entries() {
        let source = include_str!("permissions.rs");
        let production = &source[..source.find("mod publish_tests").unwrap()];

        assert!(!production.contains("fs::read_dir"));
        assert!(!production.contains("libc::readdir"));
        assert!(!production.contains("list_unix_tombstones"));
    }

    #[test]
    fn failed_publish_after_temporary_creation_is_cleaned_at_next_safe_preparation() {
        let root = tempfile::tempdir().unwrap();
        let runtime_path = root.path().join("runtime");
        let runtime = open_or_create_runtime_directory(&runtime_path).unwrap();
        let discovery = runtime_path.join("discovery.json");

        let result = with_publish_after_temp_creation_hook(
            || {
                Err(PlatformError::Io {
                    source: io::Error::other("injected failure after temporary creation"),
                })
            },
            || publish_secure_file(&runtime, &discovery, b"temporary document", None),
        );

        assert!(matches!(result, Err(PlatformError::Io { .. })));
        assert_post_failure_entries(&directory_entries(&runtime_path));
        prepare_secure_publish(&runtime, &runtime_path).unwrap();
        assert!(directory_entries(&runtime_path).is_empty());
    }

    #[test]
    fn publish_preserves_a_destination_installed_after_precheck() {
        let root = tempfile::tempdir().unwrap();
        let runtime_path = root.path().join("runtime");
        let runtime = open_or_create_runtime_directory(&runtime_path).unwrap();
        let discovery = runtime_path.join("discovery.json");
        let replacement_path = discovery.clone();

        let result = with_publish_after_temp_creation_hook(
            move || {
                fs::write(&replacement_path, b"replacement").unwrap();
                harden_test_file(&replacement_path);
                Ok(())
            },
            || publish_secure_file(&runtime, &discovery, b"new document", None),
        );

        assert!(matches!(result, Err(PlatformError::UnsafeRuntimePath)));
        assert_eq!(fs::read(discovery).unwrap(), b"replacement");
    }

    #[test]
    fn interrupted_existing_publish_never_exposes_a_missing_canonical_entry() {
        use std::sync::{Arc, Mutex};

        let root = tempfile::tempdir().unwrap();
        let runtime_path = root.path().join("runtime");
        let runtime = open_or_create_runtime_directory(&runtime_path).unwrap();
        let discovery = runtime_path.join("discovery.json");
        publish_secure_file(&runtime, &discovery, b"old document", None).unwrap();
        let existing = open_existing_secure_file_for_update(&runtime, &discovery).unwrap();
        let observed = Arc::new(Mutex::new(None));
        let observed_during_commit = Arc::clone(&observed);
        let discovery_during_commit = discovery.clone();

        let result = with_publish_after_existing_commit_hook(
            move || {
                let read = fs::read(&discovery_during_commit);
                *observed_during_commit.lock().unwrap() = read.ok();
                Err(PlatformError::Io {
                    source: io::Error::other("injected interruption at the commit boundary"),
                })
            },
            || publish_secure_file(&runtime, &discovery, b"new document", Some(existing)),
        );

        assert!(
            matches!(
                &result,
                Err(PlatformError::Io { source })
                    if source.kind() == std::io::ErrorKind::Other
            ),
            "the commit hook must be the observed failure, got {result:?}"
        );
        assert_eq!(
            observed.lock().unwrap().take(),
            Some(b"new document".to_vec())
        );
        assert_eq!(fs::read(discovery).unwrap(), b"new document");
        let retained = fs::read_dir(&runtime_path)
            .unwrap()
            .map(|entry| entry.unwrap())
            .find(|entry| entry.file_name().to_string_lossy().starts_with(".wokcore-"))
            .expect("the interrupted commit must retain the old object");
        assert_eq!(fs::read(retained.path()).unwrap(), b"old document");
    }

    #[test]
    fn failed_existing_publish_before_commit_preserves_the_old_canonical_entry() {
        let root = tempfile::tempdir().unwrap();
        let runtime_path = root.path().join("runtime");
        let runtime = open_or_create_runtime_directory(&runtime_path).unwrap();
        let discovery = runtime_path.join("discovery.json");
        publish_secure_file(&runtime, &discovery, b"old document", None).unwrap();
        let existing = open_existing_secure_file_for_update(&runtime, &discovery).unwrap();

        let result = with_publish_after_temp_creation_hook(
            || {
                Err(PlatformError::Io {
                    source: io::Error::other("injected failure before the atomic commit"),
                })
            },
            || publish_secure_file(&runtime, &discovery, b"new document", Some(existing)),
        );

        assert!(matches!(result, Err(PlatformError::Io { .. })));
        assert_eq!(fs::read(&discovery).unwrap(), b"old document");
        prepare_secure_publish(&runtime, &runtime_path).unwrap();
        assert_eq!(
            directory_entries(&runtime_path),
            vec![discovery.file_name().unwrap().to_owned()]
        );
    }

    #[cfg(windows)]
    #[test]
    fn read_only_discovery_handle_allows_atomic_existing_publish() {
        use std::io::Read;

        let root = tempfile::tempdir().unwrap();
        let runtime_path = root.path().join("runtime");
        let runtime = open_or_create_runtime_directory(&runtime_path).unwrap();
        let discovery = runtime_path.join("discovery.json");
        publish_secure_file(&runtime, &discovery, b"complete old document", None).unwrap();
        let mut reader = open_existing_secure_file(&runtime, &discovery).unwrap();
        let existing = open_existing_secure_file_for_update(&runtime, &discovery).unwrap();

        publish_secure_file(
            &runtime,
            &discovery,
            b"complete new document",
            Some(existing),
        )
        .unwrap();

        let mut observed_old = Vec::new();
        reader.read_to_end(&mut observed_old).unwrap();
        assert_eq!(observed_old, b"complete old document");

        let mut new_reader = open_existing_secure_file(&runtime, &discovery).unwrap();
        let mut observed_new = Vec::new();
        new_reader.read_to_end(&mut observed_new).unwrap();
        assert_eq!(observed_new, b"complete new document");
    }

    #[cfg(windows)]
    #[test]
    fn crash_after_publish_temp_sync_is_recovered_with_constant_bound() {
        let root = tempfile::tempdir().unwrap();
        let paths = publish_test_paths(root.path());

        for _ in 0..4 {
            spawn_publish_crash(root.path());
            assert!(
                internal_publish_entries(&paths.runtime_dir).len() <= 1,
                "pre-commit crashes must not accumulate an unbounded publish namespace"
            );
        }

        let lease = RuntimeLease::acquire(&paths).unwrap();
        let store = DiscoveryStore::new(&paths).unwrap();
        let mut current = publish_test_record(5100);
        store.publish(&current).unwrap();
        assert_eq!(store.read().unwrap(), current);
        assert!(internal_publish_entries(&paths.runtime_dir).is_empty());
        drop(lease);

        for pid in 5101..5105 {
            spawn_publish_crash(root.path());
            assert_eq!(
                store.read().unwrap(),
                current,
                "a pre-commit crash must preserve the complete canonical document"
            );
            assert!(internal_publish_entries(&paths.runtime_dir).len() <= 1);

            let replacement = publish_test_record(pid);
            let lease = RuntimeLease::acquire(&paths).unwrap();
            store.publish(&replacement).unwrap();
            assert_eq!(store.read().unwrap(), replacement);
            assert!(internal_publish_entries(&paths.runtime_dir).is_empty());
            drop(lease);
            current = replacement;
        }
    }

    #[cfg(windows)]
    #[test]
    fn fixed_publish_staging_recovery_rejects_wrong_type_without_touching_canonical() {
        let root = tempfile::tempdir().unwrap();
        let runtime_path = root.path().join("runtime");
        let runtime = open_or_create_runtime_directory(&runtime_path).unwrap();
        let discovery = runtime_path.join("discovery.json");
        publish_secure_file(&runtime, &discovery, b"complete canonical", None).unwrap();
        let staging = runtime_path.join(super::PUBLISH_STAGING_NAME);
        fs::create_dir(&staging).unwrap();

        assert!(matches!(
            prepare_secure_publish(&runtime, &runtime_path),
            Err(PlatformError::UnsafeRuntimePath)
        ));
        assert_eq!(fs::read(discovery).unwrap(), b"complete canonical");
        assert!(staging.is_dir());
    }

    #[cfg(windows)]
    #[test]
    fn fixed_publish_staging_recovery_never_follows_an_external_reparse_target() {
        let root = tempfile::tempdir().unwrap();
        let runtime_path = root.path().join("runtime");
        let runtime = open_or_create_runtime_directory(&runtime_path).unwrap();
        let external = root.path().join("external");
        fs::write(&external, b"must remain external").unwrap();
        let staging = runtime_path.join(super::PUBLISH_STAGING_NAME);
        std::os::windows::fs::symlink_file(&external, &staging).unwrap();

        assert!(matches!(
            prepare_secure_publish(&runtime, &runtime_path),
            Err(PlatformError::UnsafeRuntimePath)
        ));
        assert_eq!(fs::read(external).unwrap(), b"must remain external");
        assert!(
            fs::symlink_metadata(staging)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "spawned only by crash_after_publish_temp_sync_is_recovered_with_constant_bound"]
    fn crash_after_publish_temp_sync_helper() {
        let Some(root) = std::env::var_os("WOKCORE_TEST_PUBLISH_CRASH_ROOT") else {
            return;
        };
        let paths = publish_test_paths(Path::new(&root));
        let _lease = RuntimeLease::acquire(&paths).unwrap();
        let store = DiscoveryStore::new(&paths).unwrap();
        let record = publish_test_record(5999);

        let _ = with_publish_after_temp_creation_hook(
            || std::process::exit(73),
            || store.publish(&record),
        );
        unreachable!("the injected abrupt exit must terminate the helper process");
    }

    #[cfg(unix)]
    #[test]
    fn unix_publish_retains_an_existing_destination_swapped_after_precheck() {
        let root = tempfile::tempdir().unwrap();
        let runtime_path = root.path().join("runtime");
        let runtime = open_or_create_runtime_directory(&runtime_path).unwrap();
        let discovery = runtime_path.join("discovery.json");
        let original = runtime_path.join("original-discovery");
        fs::write(&discovery, b"original").unwrap();
        harden_test_file(&discovery);
        let existing = open_existing_secure_file_for_update(&runtime, &discovery).unwrap();
        let discovery_for_hook = discovery.clone();
        let original_for_hook = original.clone();

        let result = with_publish_after_temp_creation_hook(
            move || {
                fs::rename(&discovery_for_hook, original_for_hook).unwrap();
                fs::write(&discovery_for_hook, b"replacement").unwrap();
                harden_test_file(&discovery_for_hook);
                Ok(())
            },
            || publish_secure_file(&runtime, &discovery, b"new document", Some(existing)),
        );

        assert!(matches!(result, Err(PlatformError::UnsafeRuntimePath)));
        assert_eq!(fs::read(discovery).unwrap(), b"replacement");
        assert_eq!(fs::read(original).unwrap(), b"original");
        let retained = fs::read_dir(&runtime_path)
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".wokcore-"))
            .map(|entry| fs::read(entry.path()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(retained, vec![b"new document".to_vec()]);
    }

    #[cfg(windows)]
    #[test]
    fn verified_directory_and_destination_handles_block_path_swaps_during_publish() {
        let root = tempfile::tempdir().unwrap();
        let runtime_path = root.path().join("runtime");
        let moved_runtime = root.path().join("moved-runtime");
        let runtime = open_or_create_runtime_directory(&runtime_path).unwrap();
        let discovery = runtime_path.join("discovery.json");
        publish_secure_file(&runtime, &discovery, b"original", None).unwrap();
        let existing = open_existing_secure_file_for_update(&runtime, &discovery).unwrap();
        let discovery_for_hook = discovery.clone();
        let moved_discovery = runtime_path.join("moved-discovery.json");
        let runtime_for_hook = runtime_path.clone();
        let moved_runtime_for_hook = moved_runtime.clone();

        with_publish_after_temp_creation_hook(
            move || {
                assert!(fs::rename(&discovery_for_hook, moved_discovery).is_err());
                assert!(fs::rename(&runtime_for_hook, moved_runtime_for_hook).is_err());
                Ok(())
            },
            || publish_secure_file(&runtime, &discovery, b"replacement", Some(existing)),
        )
        .unwrap();

        assert_eq!(fs::read(discovery).unwrap(), b"replacement");
        assert!(!moved_runtime.exists());
    }

    #[cfg(unix)]
    fn harden_test_file(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[cfg(windows)]
    fn harden_test_file(_path: &Path) {}

    #[cfg(unix)]
    fn assert_post_failure_entries(entries: &[std::ffi::OsString]) {
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], super::PUBLISH_STAGING_NAME);
    }

    #[cfg(windows)]
    fn assert_post_failure_entries(entries: &[std::ffi::OsString]) {
        assert!(entries.is_empty());
    }

    fn directory_entries(path: &Path) -> Vec<std::ffi::OsString> {
        let mut entries = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }

    #[cfg(windows)]
    fn spawn_publish_crash(root: &Path) {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime::permissions::publish_tests::crash_after_publish_temp_sync_helper",
                "--ignored",
                "--nocapture",
            ])
            .env("WOKCORE_TEST_PUBLISH_CRASH_ROOT", root)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(73));
    }

    #[cfg(windows)]
    fn publish_test_paths(root: &Path) -> AppPaths {
        let runtime_dir = root.join("runtime");
        AppPaths {
            config_file: root.join("config").join("config.toml"),
            state_db: root.join("state").join("state.sqlite3"),
            log_dir: root.join("logs"),
            discovery_file: runtime_dir.join("discovery.json"),
            instance_lock: runtime_dir.join("instance.lock"),
            runtime_dir,
        }
    }

    #[cfg(windows)]
    fn publish_test_record(pid: u32) -> DiscoveryRecord {
        DiscoveryRecord {
            base_url: "http://127.0.0.1:10101".to_owned(),
            pid,
            instance_id: uuid::Uuid::from_u128(u128::from(pid)),
            wokcore_version: "0.1.0".to_owned(),
            api_major: 1,
        }
    }

    #[cfg(windows)]
    fn internal_publish_entries(path: &Path) -> Vec<std::ffi::OsString> {
        directory_entries(path)
            .into_iter()
            .filter(|name| name.to_string_lossy().starts_with(".wokcore-"))
            .collect()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        ffi::OsStr,
        fs,
        io::Read,
        os::unix::{ffi::OsStrExt, fs::PermissionsExt},
        sync::{Arc, Mutex, mpsc},
        thread,
    };

    use crate::PlatformError;

    use super::{
        open_existing_runtime_directory, open_existing_secure_file, remove_open_secure_file,
        with_unix_remove_after_identity_check_hook,
        with_unix_remove_after_quarantine_verification_hook,
    };

    #[test]
    fn child_open_remains_anchored_to_verified_directory_handle_after_path_swap() {
        let root = tempfile::tempdir().unwrap();
        let runtime_path = root.path().join("runtime");
        let moved_runtime_path = root.path().join("verified-runtime");
        fs::create_dir(&runtime_path).unwrap();
        fs::set_permissions(&runtime_path, fs::Permissions::from_mode(0o700)).unwrap();
        let discovery_path = runtime_path.join("discovery.json");
        fs::write(&discovery_path, b"verified-directory").unwrap();
        fs::set_permissions(&discovery_path, fs::Permissions::from_mode(0o600)).unwrap();
        let directory_handle = open_existing_runtime_directory(&runtime_path).unwrap();

        fs::rename(&runtime_path, &moved_runtime_path).unwrap();
        fs::create_dir(&runtime_path).unwrap();
        fs::set_permissions(&runtime_path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(&discovery_path, b"replacement-directory").unwrap();
        fs::set_permissions(&discovery_path, fs::Permissions::from_mode(0o600)).unwrap();

        let mut opened = open_existing_secure_file(&directory_handle, &discovery_path).unwrap();
        let mut bytes = Vec::new();
        opened.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"verified-directory");
    }

    #[test]
    fn owned_removal_never_unlinks_an_entry_swapped_after_quarantine_verification() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("discovery.json");
        let verified_tombstone = directory.path().join("verified-owned-tombstone");
        fs::write(&path, b"owned").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let directory_handle = open_existing_runtime_directory(directory.path()).unwrap();
        let opened = open_existing_secure_file(&directory_handle, &path).unwrap();
        let replacement_tombstone = Arc::new(Mutex::new(None));
        let observed_tombstone = Arc::clone(&replacement_tombstone);
        let runtime_path = directory.path().to_path_buf();
        let verified_tombstone_for_hook = verified_tombstone.clone();

        let result = with_unix_remove_after_quarantine_verification_hook(
            move |quarantine, matches_opened| {
                assert!(matches_opened);
                let quarantine_path = runtime_path.join(OsStr::from_bytes(quarantine.to_bytes()));
                fs::rename(&quarantine_path, verified_tombstone_for_hook).unwrap();
                fs::write(&quarantine_path, b"replacement").unwrap();
                fs::set_permissions(&quarantine_path, fs::Permissions::from_mode(0o600)).unwrap();
                *observed_tombstone.lock().unwrap() = Some(quarantine_path);
            },
            || remove_open_secure_file(&directory_handle, opened, &path),
        );

        assert!(result.is_ok());
        assert!(!path.exists());
        assert_eq!(fs::read(verified_tombstone).unwrap(), b"owned");
        let replacement_tombstone = replacement_tombstone.lock().unwrap().take().unwrap();
        assert_eq!(fs::read(replacement_tombstone).unwrap(), b"replacement");
    }

    #[test]
    fn mismatched_removal_never_restores_an_entry_swapped_after_verification() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("discovery.json");
        let original = directory.path().join("original-owned");
        let verified_mismatch = directory.path().join("verified-mismatch-tombstone");
        fs::write(&path, b"owned").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let directory_handle = open_existing_runtime_directory(directory.path()).unwrap();
        let opened = open_existing_secure_file(&directory_handle, &path).unwrap();
        let replacement_tombstone = Arc::new(Mutex::new(None));
        let observed_tombstone = Arc::clone(&replacement_tombstone);
        let runtime_path = directory.path().to_path_buf();
        let verified_mismatch_for_hook = verified_mismatch.clone();
        let path_for_first_swap = path.clone();
        let original_for_first_swap = original.clone();

        let result = with_unix_remove_after_identity_check_hook(
            move || {
                fs::rename(&path_for_first_swap, original_for_first_swap).unwrap();
                fs::write(&path_for_first_swap, b"mismatch").unwrap();
                fs::set_permissions(&path_for_first_swap, fs::Permissions::from_mode(0o600))
                    .unwrap();
            },
            || {
                with_unix_remove_after_quarantine_verification_hook(
                    move |quarantine, matches_opened| {
                        assert!(!matches_opened);
                        let quarantine_path =
                            runtime_path.join(OsStr::from_bytes(quarantine.to_bytes()));
                        fs::rename(&quarantine_path, verified_mismatch_for_hook).unwrap();
                        fs::write(&quarantine_path, b"replacement").unwrap();
                        fs::set_permissions(&quarantine_path, fs::Permissions::from_mode(0o600))
                            .unwrap();
                        *observed_tombstone.lock().unwrap() = Some(quarantine_path);
                    },
                    || remove_open_secure_file(&directory_handle, opened, &path),
                )
            },
        );

        assert!(matches!(result, Err(PlatformError::UnsafeRuntimePath)));
        assert!(!path.exists());
        assert_eq!(fs::read(original).unwrap(), b"owned");
        assert_eq!(fs::read(verified_mismatch).unwrap(), b"mismatch");
        let replacement_tombstone = replacement_tombstone.lock().unwrap().take().unwrap();
        assert_eq!(fs::read(replacement_tombstone).unwrap(), b"replacement");
    }

    #[test]
    fn owned_removal_preserves_a_replacement_installed_after_identity_check() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("discovery.json");
        let moved = directory.path().join("original-discovery.json");
        fs::write(&path, b"owned").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let directory_handle = open_existing_runtime_directory(directory.path()).unwrap();
        let opened = open_existing_secure_file(&directory_handle, &path).unwrap();
        let (start_sender, start_receiver) = mpsc::channel();
        let (done_sender, done_receiver) = mpsc::channel();
        let replacement_path = path.clone();
        let replacement_moved = moved.clone();
        let replacement = thread::spawn(move || {
            start_receiver.recv().unwrap();
            fs::rename(&replacement_path, replacement_moved).unwrap();
            fs::write(&replacement_path, b"replacement").unwrap();
            fs::set_permissions(replacement_path, fs::Permissions::from_mode(0o600)).unwrap();
            done_sender.send(()).unwrap();
        });

        let result = with_unix_remove_after_identity_check_hook(
            move || {
                start_sender.send(()).unwrap();
                done_receiver.recv().unwrap();
            },
            || remove_open_secure_file(&directory_handle, opened, &path),
        );
        replacement.join().unwrap();

        assert!(matches!(result, Err(PlatformError::UnsafeRuntimePath)));
        assert!(!path.exists());
        let retired = directory.path().join(super::RETIRED_DISCOVERY_NAME);
        assert_eq!(fs::read(retired).unwrap(), b"replacement");
        assert_eq!(fs::read(moved).unwrap(), b"owned");
    }
}
