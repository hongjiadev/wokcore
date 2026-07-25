use std::fs::File;

use fs4::fs_std::FileExt;

use crate::{AppPaths, PlatformError};

use super::permissions::{open_or_create_runtime_directory, open_or_create_secure_file};

pub struct RuntimeLease {
    _runtime_directory: File,
    _lock: File,
}

impl RuntimeLease {
    pub fn acquire(paths: &AppPaths) -> Result<Self, PlatformError> {
        if paths.instance_lock.parent() != Some(paths.runtime_dir.as_path()) {
            return Err(PlatformError::UnsafeRuntimePath);
        }

        let runtime_directory = open_or_create_runtime_directory(&paths.runtime_dir)?;
        let lock = open_or_create_secure_file(&runtime_directory, &paths.instance_lock)?;
        match lock.try_lock_exclusive() {
            Ok(true) => Ok(Self {
                _runtime_directory: runtime_directory,
                _lock: lock,
            }),
            Ok(false) => Err(PlatformError::AlreadyRunning),
            Err(source) => Err(PlatformError::Io { source }),
        }
    }
}
