use std::{
    ffi::CString,
    fs::{self, File},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path},
};

use fs4::fs_std::FileExt;

use crate::PlatformError;

const NAMESPACE_LOCK_NAME: &str = ".wokcore-runtime-namespace.lock";

pub(super) fn acquire(runtime_path: &Path) -> Result<File, PlatformError> {
    validate_runtime_path(runtime_path)?;
    let parent_path = runtime_path
        .parent()
        .ok_or(PlatformError::UnsafeRuntimePath)?;
    let parent = open_or_create_private_parent(parent_path)?;
    let lock = open_or_create_lock_file(&parent)?;
    match lock.try_lock_exclusive() {
        Ok(true) => Ok(lock),
        Ok(false) => Err(PlatformError::AlreadyRunning),
        Err(source) => Err(PlatformError::Io { source }),
    }
}

fn validate_runtime_path(path: &Path) -> Result<(), PlatformError> {
    if !path.is_absolute() {
        return Err(PlatformError::UnsafeRuntimePath);
    }
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::Prefix(_) | Component::CurDir
        )
    }) {
        return Err(PlatformError::UnsafeRuntimePath);
    }
    Ok(())
}

fn open_or_create_private_parent(path: &Path) -> Result<File, PlatformError> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            match builder.create(path) {
                Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700))?,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(source) => return Err(PlatformError::Io { source }),
            }
        }
        Err(source) => return Err(PlatformError::Io { source }),
    }

    let parent = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(map_unsafe_open_error)?;
    let metadata = parent.metadata()?;
    if !metadata.is_dir()
        || !directory_is_private_to(metadata.mode(), metadata.uid(), unsafe { libc::geteuid() })
    {
        return Err(PlatformError::UnsafeRuntimePath);
    }
    Ok(parent)
}

fn open_or_create_lock_file(parent: &File) -> Result<File, PlatformError> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let name =
        CString::new(NAMESPACE_LOCK_NAME).expect("fixed namespace lock name contains no NUL");
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    let (descriptor, created) = if descriptor >= 0 {
        (descriptor, true)
    } else {
        let source = std::io::Error::last_os_error();
        if source.raw_os_error() != Some(libc::EEXIST) {
            return Err(map_unsafe_open_error(source));
        }
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0,
            )
        };
        if descriptor < 0 {
            return Err(map_unsafe_open_error(std::io::Error::last_os_error()));
        }
        (descriptor, false)
    };
    let file = unsafe { File::from_raw_fd(descriptor) };
    if created {
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o7777 != 0o600
    {
        return Err(PlatformError::UnsafeRuntimePath);
    }
    Ok(file)
}

fn map_unsafe_open_error(source: std::io::Error) -> PlatformError {
    match source.raw_os_error() {
        Some(libc::ELOOP | libc::EISDIR | libc::ENOTDIR) => PlatformError::UnsafeRuntimePath,
        _ => PlatformError::Io { source },
    }
}

fn directory_is_private_to(mode: u32, owner: u32, user: u32) -> bool {
    owner == user && mode & 0o7777 == 0o700
}

#[cfg(test)]
mod tests {
    use super::directory_is_private_to;

    #[test]
    fn owner_only_parent_excludes_another_nonprivileged_uid() {
        let owner = 1000;
        let other_user = 1001;

        assert!(directory_is_private_to(0o700, owner, owner));
        assert!(!directory_is_private_to(0o700, owner, other_user));
        assert!(!directory_is_private_to(0o770, owner, owner));
        assert!(!directory_is_private_to(0o707, owner, owner));
    }
}
