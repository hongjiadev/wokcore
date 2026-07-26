use std::fs::File;

use fs4::fs_std::FileExt;

use crate::{AppPaths, PlatformError};

#[cfg(unix)]
use super::namespace_lock;
use super::permissions::{
    open_existing_runtime_directory, open_existing_secure_file_for_update,
    open_or_create_runtime_directory, open_or_create_secure_file,
};

pub struct RuntimeLease {
    #[cfg(unix)]
    _namespace_lock: File,
    _runtime_directory: File,
    _lock: File,
}

impl RuntimeLease {
    pub fn acquire(paths: &AppPaths) -> Result<Self, PlatformError> {
        if paths.instance_lock.parent() != Some(paths.runtime_dir.as_path()) {
            return Err(PlatformError::UnsafeRuntimePath);
        }

        #[cfg(unix)]
        let namespace_lock = namespace_lock::acquire(&paths.runtime_dir)?;
        let runtime_directory = open_or_create_runtime_directory(&paths.runtime_dir)?;
        let lock = open_or_create_secure_file(&runtime_directory, &paths.instance_lock)?;
        match lock.try_lock_exclusive() {
            Ok(true) => Ok(Self {
                #[cfg(unix)]
                _namespace_lock: namespace_lock,
                _runtime_directory: runtime_directory,
                _lock: lock,
            }),
            Ok(false) => Err(PlatformError::AlreadyRunning),
            Err(source) => Err(PlatformError::Io { source }),
        }
    }

    pub fn acquire_existing(paths: &AppPaths) -> Result<Self, PlatformError> {
        if paths.instance_lock.parent() != Some(paths.runtime_dir.as_path()) {
            return Err(PlatformError::UnsafeRuntimePath);
        }

        #[cfg(unix)]
        let namespace_lock =
            namespace_lock::acquire_existing(&paths.runtime_dir).map_err(map_missing_to_unsafe)?;
        let runtime_directory =
            open_existing_runtime_directory(&paths.runtime_dir).map_err(map_missing_to_unsafe)?;
        let lock = open_existing_secure_file_for_update(&runtime_directory, &paths.instance_lock)
            .map_err(map_existing_lock_error)?;
        match lock.try_lock_exclusive() {
            Ok(true) => Ok(Self {
                #[cfg(unix)]
                _namespace_lock: namespace_lock,
                _runtime_directory: runtime_directory,
                _lock: lock,
            }),
            Ok(false) => Err(PlatformError::AlreadyRunning),
            Err(source) => Err(PlatformError::Io { source }),
        }
    }
}

fn map_missing_to_unsafe(error: PlatformError) -> PlatformError {
    match error {
        PlatformError::Io { source } if source.kind() == std::io::ErrorKind::NotFound => {
            PlatformError::UnsafeRuntimePath
        }
        error => error,
    }
}

fn map_existing_lock_error(error: PlatformError) -> PlatformError {
    #[cfg(windows)]
    if matches!(
        &error,
        PlatformError::Io { source }
            if source.raw_os_error()
                == Some(windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION as i32)
    ) {
        return PlatformError::AlreadyRunning;
    }
    map_missing_to_unsafe(error)
}
