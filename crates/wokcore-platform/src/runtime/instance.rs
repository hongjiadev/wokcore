use std::fs::File;

use fs4::fs_std::FileExt;

use crate::{AppPaths, PlatformError};

#[cfg(unix)]
use super::namespace_lock;
use super::permissions::{open_or_create_runtime_directory, open_or_create_secure_file};

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
}
