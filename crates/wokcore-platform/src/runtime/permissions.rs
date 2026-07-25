use std::{
    fs::{self, File},
    path::Path,
};

use crate::PlatformError;

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

    let descriptor = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, mode) };
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
pub(super) fn publish_secure_file(
    directory: &File,
    destination: &Path,
    document: &[u8],
) -> Result<(), PlatformError> {
    use std::{io::Write, os::fd::AsRawFd};

    struct TemporaryEntry<'a> {
        directory: &'a File,
        name: std::ffi::CString,
        file: File,
        committed: bool,
    }

    impl Drop for TemporaryEntry<'_> {
        fn drop(&mut self) {
            if !self.committed {
                use std::os::fd::AsRawFd;

                unsafe {
                    libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0);
                }
            }
        }
    }

    let destination = unix_child_name(destination)?;
    let mut temporary = loop {
        let name = std::ffi::CString::new(format!(".wokcore-publish-{}.tmp", uuid::Uuid::new_v4()))
            .expect("generated temporary name contains no NUL");
        match unix_openat(
            directory,
            &name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            0o600,
        ) {
            Ok(file) => {
                break TemporaryEntry {
                    directory,
                    name,
                    file,
                    committed: false,
                };
            }
            Err(PlatformError::Io { source }) if source.raw_os_error() == Some(libc::EEXIST) => {}
            Err(error) => return Err(error),
        }
    };
    harden_secure_file(&temporary.file, Path::new(""))?;
    temporary.file.write_all(document)?;
    temporary.file.sync_all()?;
    #[cfg(test)]
    run_publish_after_temp_creation_hook()?;

    if unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            temporary.name.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
        )
    } != 0
    {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    temporary.committed = true;
    Ok(())
}

#[cfg(unix)]
pub(super) fn remove_open_secure_file(
    directory: &File,
    file: File,
    path: &Path,
) -> Result<(), PlatformError> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::io::AsRawFd;

    let opened = file.metadata()?;
    #[cfg(test)]
    run_unix_remove_after_identity_check_hook();

    let canonical = unix_child_name(path)?;
    let quarantine =
        std::ffi::CString::new(format!(".wokcore-remove-{}.tmp", uuid::Uuid::new_v4()))
            .expect("generated quarantine name contains no NUL");
    if unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            canonical.as_ptr(),
            directory.as_raw_fd(),
            quarantine.as_ptr(),
        )
    } != 0
    {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
    }

    let quarantined = unix_openat(directory, &quarantine, libc::O_RDONLY | libc::O_NOFOLLOW, 0);
    let matches_opened = match quarantined {
        Ok(file) => file.metadata().is_ok_and(|metadata| {
            metadata.is_file() && metadata.dev() == opened.dev() && metadata.ino() == opened.ino()
        }),
        Err(_) => false,
    };
    if !matches_opened {
        restore_unix_quarantine(directory, &quarantine, &canonical)?;
        return Err(PlatformError::UnsafeRuntimePath);
    }

    if unsafe { libc::unlinkat(directory.as_raw_fd(), quarantine.as_ptr(), 0) } != 0 {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn restore_unix_quarantine(
    directory: &File,
    quarantine: &std::ffi::CStr,
    canonical: &std::ffi::CStr,
) -> Result<(), PlatformError> {
    use std::os::fd::AsRawFd;

    let descriptor = directory.as_raw_fd();
    if unsafe {
        libc::linkat(
            descriptor,
            quarantine.as_ptr(),
            descriptor,
            canonical.as_ptr(),
            0,
        )
    } != 0
    {
        return Err(PlatformError::UnsafeRuntimePath);
    }
    if unsafe { libc::unlinkat(descriptor, quarantine.as_ptr(), 0) } != 0 {
        return Err(PlatformError::Io {
            source: std::io::Error::last_os_error(),
        });
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
        None => open_windows_existing_file(path, false),
    }
}

#[cfg(windows)]
pub(super) fn open_existing_secure_file(
    _directory: &File,
    path: &Path,
) -> Result<File, PlatformError> {
    open_windows_existing_file(path, false)
}

#[cfg(windows)]
#[derive(Clone, Copy)]
enum WindowsFileKind {
    Directory,
    Regular,
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
fn open_windows_existing_file(path: &Path, for_removal: bool) -> Result<File, PlatformError> {
    use std::{
        os::windows::{fs::MetadataExt, io::FromRawHandle},
        ptr,
    };

    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            CreateFileW, DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING, READ_CONTROL,
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
    let desired_access = GENERIC_READ | READ_CONTROL | if for_removal { DELETE } else { 0 };
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            desired_access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
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
pub(super) fn publish_secure_file(
    _directory: &File,
    destination: &Path,
    document: &[u8],
) -> Result<(), PlatformError> {
    use std::io::Write;

    let parent = destination
        .parent()
        .ok_or(PlatformError::UnsafeRuntimePath)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    harden_secure_file(temporary.as_file(), temporary.path())?;
    temporary.write_all(document)?;
    temporary.as_file().sync_all()?;
    #[cfg(test)]
    run_publish_after_temp_creation_hook()?;

    let temporary_path = temporary.into_temp_path();
    replace_windows_file(temporary_path.as_ref(), destination)?;
    Ok(())
}

#[cfg(windows)]
fn replace_windows_file(source: &Path, destination: &Path) -> Result<(), PlatformError> {
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    if !destination.exists() {
        return fs::rename(source, destination).map_err(PlatformError::from);
    }

    let source = wide_path(source);
    let destination = wide_path(destination);
    if unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            source.as_ptr(),
            std::ptr::null(),
            0,
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
pub(super) fn remove_open_secure_file(
    _directory: &File,
    file: File,
    path: &Path,
) -> Result<(), PlatformError> {
    use std::{mem::size_of, os::windows::io::AsRawHandle};

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
    };

    fn identity(file: &File) -> Result<(u32, u32, u32), PlatformError> {
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

    let expected = identity(&file)?;
    drop(file);
    let file = open_windows_existing_file(path, true)?;
    if identity(&file)? != expected {
        return Err(PlatformError::UnsafeRuntimePath);
    }
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
pub(super) fn publish_secure_file(
    _directory: &File,
    _destination: &Path,
    _document: &[u8],
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

    use super::{
        open_or_create_runtime_directory, publish_secure_file,
        with_publish_after_temp_creation_hook,
    };

    #[test]
    fn failed_publish_after_temporary_creation_leaves_no_residue() {
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
            || publish_secure_file(&runtime, &discovery, b"temporary document"),
        );

        assert!(matches!(result, Err(PlatformError::Io { .. })));
        assert_eq!(
            directory_entries(&runtime_path),
            Vec::<std::ffi::OsString>::new()
        );
    }

    fn directory_entries(path: &Path) -> Vec<std::ffi::OsString> {
        let mut entries = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{fs, io::Read, os::unix::fs::PermissionsExt, sync::mpsc, thread};

    use crate::PlatformError;

    use super::{
        open_existing_runtime_directory, open_existing_secure_file, remove_open_secure_file,
        with_unix_remove_after_identity_check_hook,
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
        assert_eq!(fs::read(path).unwrap(), b"replacement");
        assert_eq!(fs::read(moved).unwrap(), b"owned");
    }
}
