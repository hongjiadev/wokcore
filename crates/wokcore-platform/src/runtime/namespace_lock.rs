use std::{
    ffi::CString,
    fs::{self, File},
    os::{
        fd::FromRawFd,
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path},
};

use fs4::fs_std::FileExt;

use crate::PlatformError;

pub(super) fn acquire(path: &Path) -> Result<File, PlatformError> {
    let name = namespace_name(path)?;
    let (descriptor, created) = open_namespace_object(&name)?;
    let file = unsafe { File::from_raw_fd(descriptor) };
    if created {
        use std::os::unix::fs::PermissionsExt;

        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    verify_namespace_object(&file)?;
    match file.try_lock_exclusive() {
        Ok(true) => Ok(file),
        Ok(false) => Err(PlatformError::AlreadyRunning),
        Err(source) => Err(PlatformError::Io { source }),
    }
}

fn namespace_name(path: &Path) -> Result<CString, PlatformError> {
    if !path.is_absolute() {
        return Err(PlatformError::UnsafeRuntimePath);
    }

    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    hash_bytes(&mut hash, b"/");
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(component) => {
                let bytes = component.as_bytes();
                hash_bytes(&mut hash, &(bytes.len() as u64).to_le_bytes());
                hash_bytes(&mut hash, bytes);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                return Err(PlatformError::UnsafeRuntimePath);
            }
        }
    }

    CString::new(format!("/wc-{:08x}-{hash:016x}", unsafe {
        libc::geteuid()
    }))
    .map_err(|_| PlatformError::UnsafeRuntimePath)
}

fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

fn open_namespace_object(name: &std::ffi::CStr) -> Result<(libc::c_int, bool), PlatformError> {
    let descriptor = unsafe {
        libc::shm_open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
            0o600,
        )
    };
    if descriptor >= 0 {
        return Ok((descriptor, true));
    }

    let source = std::io::Error::last_os_error();
    if source.raw_os_error() != Some(libc::EEXIST) {
        return Err(PlatformError::Io { source });
    }
    let descriptor =
        unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC, 0o600) };
    if descriptor < 0 {
        let source = std::io::Error::last_os_error();
        return Err(match source.raw_os_error() {
            Some(libc::EACCES) => PlatformError::UnsafeRuntimePath,
            _ => PlatformError::Io { source },
        });
    }
    Ok((descriptor, false))
}

fn verify_namespace_object(file: &File) -> Result<(), PlatformError> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o600
    {
        return Err(PlatformError::UnsafeRuntimePath);
    }
    Ok(())
}
