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
pub(super) fn open_or_create_secure_file(path: &Path) -> Result<File, PlatformError> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW);
    let file = match options.open(path) {
        Ok(file) => {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut existing = fs::OpenOptions::new();
            existing
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOFOLLOW);
            existing
                .open(path)
                .map_err(|source| match source.raw_os_error() {
                    Some(libc::ELOOP | libc::EISDIR) => PlatformError::UnsafeRuntimePath,
                    _ => PlatformError::Io { source },
                })?
        }
        Err(source) => return Err(PlatformError::Io { source }),
    };
    verify_unix_file(&file)?;
    Ok(file)
}

#[cfg(unix)]
pub(super) fn open_existing_secure_file(path: &Path) -> Result<File, PlatformError> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|source| match source.raw_os_error() {
            Some(libc::ELOOP | libc::EISDIR) => PlatformError::UnsafeRuntimePath,
            _ => PlatformError::Io { source },
        })?;
    verify_unix_file(&file)?;
    Ok(file)
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
pub(super) fn remove_open_secure_file(file: File, path: &Path) -> Result<(), PlatformError> {
    use std::os::unix::fs::MetadataExt;

    let opened = file.metadata()?;
    let current = fs::symlink_metadata(path)?;
    if !current.is_file() || current.dev() != opened.dev() || current.ino() != opened.ino() {
        return Err(PlatformError::UnsafeRuntimePath);
    }
    fs::remove_file(path)?;
    Ok(())
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
pub(super) fn open_or_create_secure_file(path: &Path) -> Result<File, PlatformError> {
    use std::{os::windows::io::FromRawHandle, ptr};

    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_ALWAYS,
        },
    };

    let wide_path = wide_path(path);
    let handle = with_current_user_security_attributes(false, |attributes| {
        let handle = unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                attributes,
                OPEN_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(PlatformError::Io {
                source: std::io::Error::last_os_error(),
            });
        }
        Ok(handle)
    })?;
    let file = unsafe { File::from_raw_handle(handle) };
    verify_windows_file(&file, WindowsFileKind::Regular)?;
    verify_windows_permissions(&file)?;
    Ok(file)
}

#[cfg(windows)]
pub(super) fn open_existing_secure_file(path: &Path) -> Result<File, PlatformError> {
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
pub(super) fn remove_open_secure_file(file: File, path: &Path) -> Result<(), PlatformError> {
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
pub(super) fn open_or_create_secure_file(_path: &Path) -> Result<File, PlatformError> {
    Err(PlatformError::UnsafeRuntimePath)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn open_existing_runtime_directory(_path: &Path) -> Result<File, PlatformError> {
    Err(PlatformError::UnsafeRuntimePath)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn open_existing_secure_file(_path: &Path) -> Result<File, PlatformError> {
    Err(PlatformError::UnsafeRuntimePath)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn harden_secure_file(_file: &File, _path: &Path) -> Result<(), PlatformError> {
    Err(PlatformError::UnsafeRuntimePath)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn remove_open_secure_file(_file: File, _path: &Path) -> Result<(), PlatformError> {
    Err(PlatformError::UnsafeRuntimePath)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn sync_parent_directory(_directory: &File) -> Result<(), PlatformError> {
    Err(PlatformError::UnsafeRuntimePath)
}
