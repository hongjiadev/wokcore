use std::{
    fs,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
};

use fs4::fs_std::FileExt;
use tempfile::NamedTempFile;

use crate::{StorageError, VersionedConfig};

use super::AppConfig;

#[cfg(unix)]
use std::fs::File;

#[derive(Clone, Debug)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn load(&self) -> Result<VersionedConfig, StorageError> {
        let document = match fs::read_to_string(&self.path) {
            Ok(document) => document,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(VersionedConfig {
                    revision: 0,
                    config: AppConfig::default(),
                });
            }
            Err(source) => return Err(StorageError::Io { source }),
        };

        let loaded: VersionedConfig =
            toml_edit::de::from_str(&document).map_err(|error| StorageError::InvalidConfig {
                message: error.to_string(),
            })?;
        validate(&loaded.config)?;
        Ok(loaded)
    }

    pub fn commit(
        &self,
        expected_revision: u64,
        candidate: &AppConfig,
    ) -> Result<VersionedConfig, StorageError> {
        validate(candidate)?;

        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.lock_path())
            .map_err(|source| StorageError::Io { source })?;
        lock.lock_exclusive()
            .map_err(|source| StorageError::Io { source })?;
        let result = self.commit_locked(expected_revision, candidate);
        let unlock_result = FileExt::unlock(&lock).map_err(|source| StorageError::Io { source });

        match (result, unlock_result) {
            (Ok(committed), Ok(())) => Ok(committed),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    fn commit_locked(
        &self,
        expected_revision: u64,
        candidate: &AppConfig,
    ) -> Result<VersionedConfig, StorageError> {
        let current = self.load()?;
        if expected_revision != current.revision {
            return Err(StorageError::RevisionConflict {
                expected: expected_revision,
                actual: current.revision,
            });
        }

        let committed = VersionedConfig {
            revision: current.revision.checked_add(1).ok_or_else(|| {
                StorageError::InvalidConfig {
                    message: "configuration revision cannot exceed u64::MAX".to_owned(),
                }
            })?,
            config: candidate.clone(),
        };
        let document = toml_edit::ser::to_string_pretty(&committed).map_err(|error| {
            StorageError::SerializeConfig {
                message: error.to_string(),
            }
        })?;

        self.replace_atomically(document.as_bytes())?;
        Ok(committed)
    }

    fn lock_path(&self) -> PathBuf {
        let mut path = self.path.as_os_str().to_os_string();
        path.push(".lock");
        path.into()
    }

    fn replace_atomically(&self, contents: &[u8]) -> Result<(), StorageError> {
        let parent = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut temporary =
            NamedTempFile::new_in(parent).map_err(|source| StorageError::Io { source })?;
        temporary
            .write_all(contents)
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|source| StorageError::Io { source })?;

        let temporary_path = temporary.into_temp_path();
        replace_file(temporary_path.as_ref(), &self.path)
            .map_err(|source| StorageError::Io { source })?;
        sync_parent_directory(parent).map_err(|source| StorageError::Io { source })?;
        Ok(())
    }
}

fn validate(config: &AppConfig) -> Result<(), StorageError> {
    if config.server.port == 0 {
        return Err(StorageError::InvalidConfig {
            message: "server port must be non-zero".to_owned(),
        });
    }
    Ok(())
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    if !destination.exists() {
        return fs::rename(source, destination);
    }

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let replaced = unsafe {
        windows_sys::Win32::Storage::FileSystem::ReplaceFileW(
            destination.as_ptr(),
            source.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if replaced == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> std::io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::ConfigStore;

    #[test]
    fn failed_replace_removes_the_same_directory_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::create_dir(&path).unwrap();
        let store = ConfigStore::new(&path);

        assert!(store.replace_atomically(b"replacement").is_err());

        let entries = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec!["config.toml"]);
    }
}
