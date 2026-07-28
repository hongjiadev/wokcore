use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
};

use fs4::fs_std::FileExt;
use sha2::{Digest, Sha256};
use tempfile::Builder;

use super::{
    manifest::{MAX_UPDATE_ARTIFACT_BYTES, UpdateArtifact, UpdateError, current_target},
    rollback::{atomic_replace, open_replacement, restore_previous},
};

const COPY_BUFFER_BYTES: usize = 64 * 1024;
const MAX_RELEASE_DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;
const RELEASE_DOCUMENTS: [&str; 4] = ["LICENSE-APACHE", "LICENSE-MIT", "NOTICE.md", "README.md"];

#[derive(Debug)]
pub struct UpdateLease {
    lock: File,
}

impl Drop for UpdateLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.lock);
    }
}

pub fn acquire_update_lease(target: &Path) -> Result<UpdateLease, UpdateError> {
    let parent = target.parent().ok_or(UpdateError::StagingFailed)?;
    let parent_metadata = fs::symlink_metadata(parent).map_err(|_| UpdateError::StagingFailed)?;
    if !safe_directory(&parent_metadata) {
        return Err(UpdateError::StagingFailed);
    }
    let target_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(UpdateError::StagingFailed)?;
    let lock_path = parent.join(format!(".{target_name}.update.lock"));
    if let Ok(metadata) = fs::symlink_metadata(&lock_path)
        && !safe_regular_file(&metadata)
    {
        return Err(UpdateError::StagingFailed);
    }

    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    configure_no_follow(&mut options);
    let lock = options
        .open(&lock_path)
        .map_err(|_| UpdateError::StagingFailed)?;
    let identity = handle_identity(&lock)?;
    verify_path_identity(&lock_path, identity)?;
    match lock.try_lock_exclusive() {
        Ok(true) => Ok(UpdateLease { lock }),
        Ok(false) => Err(UpdateError::UpdateInProgress),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            Err(UpdateError::UpdateInProgress)
        }
        Err(_) => Err(UpdateError::StagingFailed),
    }
}

#[cfg(unix)]
fn configure_no_follow(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.custom_flags(libc::O_NOFOLLOW);
}

#[cfg(windows)]
fn configure_no_follow(options: &mut OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    options
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
}

#[derive(Debug)]
pub struct PreparedInstall {
    candidate: tempfile::TempPath,
    candidate_file: File,
    candidate_identity: FileIdentity,
    target: PathBuf,
    target_identity: FileIdentity,
    backup: PathBuf,
}

impl PreparedInstall {
    pub fn candidate_path(&self) -> &Path {
        &self.candidate
    }

    pub fn begin(self) -> Result<InstallTransaction, UpdateError> {
        if verify_handle_identity(&self.candidate_file, self.candidate_identity).is_err()
            || verify_path_identity(&self.candidate, self.candidate_identity).is_err()
        {
            return if verify_path_identity(&self.target, self.target_identity).is_ok() {
                Err(UpdateError::StagingFailed)
            } else {
                Err(UpdateError::RecoveryRequired)
            };
        }
        if verify_path_identity(&self.target, self.target_identity).is_err() {
            return Err(UpdateError::RecoveryRequired);
        }
        if let Err(error) = atomic_replace(
            &self.target,
            &self.candidate,
            &self.candidate_file,
            &self.backup,
        ) {
            return if verify_path_identity(&self.target, self.target_identity).is_ok() {
                Err(error)
            } else {
                Err(UpdateError::RecoveryRequired)
            };
        }
        if verify_handle_identity(&self.candidate_file, self.candidate_identity).is_err()
            || verify_path_identity(&self.target, self.candidate_identity).is_err()
        {
            return Err(UpdateError::RecoveryRequired);
        }
        let backup_file =
            open_read_no_follow(&self.backup).map_err(|_| UpdateError::RecoveryRequired)?;
        if verify_handle_identity(&backup_file, self.target_identity).is_err()
            || verify_path_identity(&self.backup, self.target_identity).is_err()
        {
            return Err(UpdateError::RecoveryRequired);
        }
        Ok(InstallTransaction {
            target: self.target.clone(),
            backup: self.backup.clone(),
            installed_file: self.candidate_file,
            installed_identity: self.candidate_identity,
            backup_file,
            previous_identity: self.target_identity,
            active: true,
        })
    }
}

#[derive(Debug)]
pub struct InstallTransaction {
    target: PathBuf,
    backup: PathBuf,
    installed_file: File,
    installed_identity: FileIdentity,
    backup_file: File,
    previous_identity: FileIdentity,
    active: bool,
}

impl InstallTransaction {
    pub fn commit(mut self) -> Result<(), UpdateError> {
        if !self.paths_are_unchanged() {
            self.active = false;
            return Err(UpdateError::RecoveryRequired);
        }
        self.active = false;
        cleanup_previous(&self.backup);
        Ok(())
    }

    pub fn rollback(mut self) -> Result<(), UpdateError> {
        if !self.paths_are_unchanged() {
            self.active = false;
            return Err(UpdateError::RecoveryRequired);
        }
        match restore_previous(&self.target, &self.backup, &self.backup_file) {
            Ok(()) => {
                self.active = false;
                cleanup_previous(&self.backup);
                Ok(())
            }
            Err(_) if verify_path_identity(&self.target, self.previous_identity).is_ok() => {
                self.active = false;
                cleanup_previous(&self.backup);
                Err(UpdateError::RollbackDurabilityFailed)
            }
            Err(_) => {
                self.active = false;
                Err(UpdateError::RollbackFailed)
            }
        }
    }

    pub fn preserve_for_recovery(mut self) {
        self.active = false;
    }

    fn paths_are_unchanged(&self) -> bool {
        verify_handle_identity(&self.installed_file, self.installed_identity).is_ok()
            && verify_path_identity(&self.target, self.installed_identity).is_ok()
            && verify_handle_identity(&self.backup_file, self.previous_identity).is_ok()
            && verify_path_identity(&self.backup, self.previous_identity).is_ok()
    }
}

impl Drop for InstallTransaction {
    fn drop(&mut self) {
        if self.active
            && self.paths_are_unchanged()
            && (restore_previous(&self.target, &self.backup, &self.backup_file).is_ok()
                || verify_path_identity(&self.target, self.previous_identity).is_ok())
        {
            self.active = false;
            cleanup_previous(&self.backup);
        }
    }
}

pub fn verify_artifact(path: &Path, artifact: &UpdateArtifact) -> Result<(), UpdateError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| UpdateError::StagingFailed)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(UpdateError::StagingFailed);
    }
    let file = open_read_no_follow(path)?;
    verify_artifact_file(&file, artifact)
}

pub fn verify_artifact_file(file: &File, artifact: &UpdateArtifact) -> Result<(), UpdateError> {
    let metadata = file.metadata().map_err(|_| UpdateError::StagingFailed)?;
    if !metadata.file_type().is_file() || metadata.len() != artifact.size() {
        return Err(UpdateError::ArtifactSizeMismatch);
    }
    let mut file = file.try_clone().map_err(|_| UpdateError::StagingFailed)?;
    file.rewind().map_err(|_| UpdateError::StagingFailed)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| UpdateError::StagingFailed)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = format!("{:x}", hasher.finalize());
    if digest != artifact.sha256() {
        return Err(UpdateError::ArtifactHashMismatch);
    }
    Ok(())
}

pub fn prepare_install(
    archive: &Path,
    artifact: &UpdateArtifact,
    target: &Path,
) -> Result<PreparedInstall, UpdateError> {
    let metadata = fs::symlink_metadata(archive).map_err(|_| UpdateError::StagingFailed)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(UpdateError::StagingFailed);
    }
    let archive = open_read_no_follow(archive)?;
    prepare_install_file(&archive, artifact, target)
}

pub fn prepare_install_file(
    archive: &File,
    artifact: &UpdateArtifact,
    target: &Path,
) -> Result<PreparedInstall, UpdateError> {
    verify_artifact_file(archive, artifact)?;
    if artifact.target() != current_target()
        || target.file_name().and_then(|name| name.to_str()) != Some(artifact.executable())
    {
        return Err(UpdateError::TargetMismatch);
    }
    let target_identity = path_identity(target)?;
    let parent = target.parent().ok_or(UpdateError::StagingFailed)?;
    let parent_metadata = fs::symlink_metadata(parent).map_err(|_| UpdateError::StagingFailed)?;
    if !safe_directory(&parent_metadata) {
        return Err(UpdateError::StagingFailed);
    }

    let mut staged = Builder::new()
        .prefix(".wokcore-update-candidate-")
        .tempfile_in(parent)
        .map_err(|_| UpdateError::StagingFailed)?;
    extract_executable(archive, artifact, staged.as_file_mut())?;
    staged
        .as_file_mut()
        .flush()
        .map_err(|_| UpdateError::StagingFailed)?;
    staged
        .as_file()
        .sync_all()
        .map_err(|_| UpdateError::StagingFailed)?;
    set_executable_permissions(staged.path())?;
    let candidate_identity = handle_identity(staged.as_file())?;
    let candidate_file = open_replacement(staged.path()).map_err(|_| UpdateError::StagingFailed)?;
    verify_handle_identity(&candidate_file, candidate_identity)?;
    let (staging_file, candidate) = staged.into_parts();
    drop(staging_file);
    let backup = parent.join(format!(
        ".{}.previous",
        target
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(UpdateError::StagingFailed)?
    ));
    Ok(PreparedInstall {
        candidate,
        candidate_file,
        candidate_identity,
        target: target.to_path_buf(),
        target_identity,
        backup,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    volume: u64,
    file: u64,
}

fn verify_handle_identity(file: &File, expected: FileIdentity) -> Result<(), UpdateError> {
    (handle_identity(file)? == expected)
        .then_some(())
        .ok_or(UpdateError::StagingFailed)
}

fn verify_path_identity(path: &Path, expected: FileIdentity) -> Result<(), UpdateError> {
    (path_identity(path)? == expected)
        .then_some(())
        .ok_or(UpdateError::StagingFailed)
}

#[cfg(unix)]
fn handle_identity(file: &File) -> Result<FileIdentity, UpdateError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata().map_err(|_| UpdateError::StagingFailed)?;
    if !safe_regular_file(&metadata) {
        return Err(UpdateError::StagingFailed);
    }
    Ok(FileIdentity {
        volume: metadata.dev(),
        file: metadata.ino(),
    })
}

#[cfg(windows)]
fn handle_identity(file: &File) -> Result<FileIdentity, UpdateError> {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let metadata = file.metadata().map_err(|_| UpdateError::StagingFailed)?;
    if !safe_regular_file(&metadata) {
        return Err(UpdateError::StagingFailed);
    }
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &raw mut information) } == 0
    {
        return Err(UpdateError::StagingFailed);
    }
    Ok(FileIdentity {
        volume: u64::from(information.dwVolumeSerialNumber),
        file: (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    })
}

fn path_identity(path: &Path) -> Result<FileIdentity, UpdateError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| UpdateError::StagingFailed)?;
    if !safe_regular_file(&metadata) {
        return Err(UpdateError::StagingFailed);
    }
    let file = open_read_no_follow(path)?;
    handle_identity(&file)
}

#[cfg(unix)]
fn safe_regular_file(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_file() && !metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn safe_regular_file(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_type().is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
}

#[cfg(unix)]
fn safe_directory(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_dir() && !metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn safe_directory(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_type().is_dir() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
}

fn open_read_no_follow(path: &Path) -> Result<File, UpdateError> {
    let mut options = OpenOptions::new();
    options.read(true);
    configure_no_follow(&mut options);
    options.open(path).map_err(|_| UpdateError::StagingFailed)
}

#[cfg(windows)]
fn extract_executable(
    archive_file: &File,
    artifact: &UpdateArtifact,
    destination: &mut File,
) -> Result<(), UpdateError> {
    let mut file = archive_file
        .try_clone()
        .map_err(|_| UpdateError::InvalidArchive)?;
    file.rewind().map_err(|_| UpdateError::InvalidArchive)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|_| UpdateError::InvalidArchive)?;
    let expected = artifact.executable().as_bytes();
    let mut found = false;
    let mut documents = [false; RELEASE_DOCUMENTS.len()];
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|_| UpdateError::InvalidArchive)?;
        if entry.name_raw() != expected {
            let name =
                std::str::from_utf8(entry.name_raw()).map_err(|_| UpdateError::InvalidArchive)?;
            let document = RELEASE_DOCUMENTS
                .iter()
                .position(|expected| *expected == name)
                .ok_or(UpdateError::InvalidArchive)?;
            if documents[document]
                || entry.is_dir()
                || entry.size() > MAX_RELEASE_DOCUMENT_BYTES
                || entry
                    .unix_mode()
                    .is_some_and(|mode| mode & 0o170000 == 0o120000)
            {
                return Err(UpdateError::InvalidArchive);
            }
            documents[document] = true;
            continue;
        }
        let size = entry.size();
        if found
            || entry.is_dir()
            || size == 0
            || size > MAX_UPDATE_ARTIFACT_BYTES
            || entry
                .unix_mode()
                .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err(UpdateError::InvalidArchive);
        }
        let copied = io::copy(
            &mut entry.take(MAX_UPDATE_ARTIFACT_BYTES.saturating_add(1)),
            destination,
        )
        .map_err(|_| UpdateError::InvalidArchive)?;
        if copied != size {
            return Err(UpdateError::InvalidArchive);
        }
        found = true;
    }
    if !found {
        return Err(UpdateError::InvalidArchive);
    }
    Ok(())
}

#[cfg(unix)]
fn extract_executable(
    archive_file: &File,
    artifact: &UpdateArtifact,
    destination: &mut File,
) -> Result<(), UpdateError> {
    let mut file = archive_file
        .try_clone()
        .map_err(|_| UpdateError::InvalidArchive)?;
    file.rewind().map_err(|_| UpdateError::InvalidArchive)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut found = false;
    let mut documents = [false; RELEASE_DOCUMENTS.len()];
    let entries = archive.entries().map_err(|_| UpdateError::InvalidArchive)?;
    for entry in entries {
        let entry = entry.map_err(|_| UpdateError::InvalidArchive)?;
        let path = entry.path().map_err(|_| UpdateError::InvalidArchive)?;
        if path.as_os_str() != artifact.executable() {
            let name = path.to_str().ok_or(UpdateError::InvalidArchive)?;
            let document = RELEASE_DOCUMENTS
                .iter()
                .position(|expected| *expected == name)
                .ok_or(UpdateError::InvalidArchive)?;
            if documents[document]
                || !entry.header().entry_type().is_file()
                || entry.size() > MAX_RELEASE_DOCUMENT_BYTES
            {
                return Err(UpdateError::InvalidArchive);
            }
            documents[document] = true;
            continue;
        }
        let size = entry.size();
        if found
            || !entry.header().entry_type().is_file()
            || size == 0
            || size > MAX_UPDATE_ARTIFACT_BYTES
        {
            return Err(UpdateError::InvalidArchive);
        }
        let copied = io::copy(
            &mut entry.take(MAX_UPDATE_ARTIFACT_BYTES.saturating_add(1)),
            destination,
        )
        .map_err(|_| UpdateError::InvalidArchive)?;
        if copied != size {
            return Err(UpdateError::InvalidArchive);
        }
        found = true;
    }
    if !found {
        return Err(UpdateError::InvalidArchive);
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable_permissions(path: &Path) -> Result<(), UpdateError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(|_| UpdateError::StagingFailed)
}

#[cfg(windows)]
fn set_executable_permissions(_path: &Path) -> Result<(), UpdateError> {
    Ok(())
}

fn cleanup_previous(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound
                    | io::ErrorKind::PermissionDenied
                    | io::ErrorKind::WouldBlock
            ) => {}
        Err(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::{
        super::rollback::fail_next_restore_for_test, InstallTransaction, handle_identity,
        open_read_no_follow,
    };

    #[test]
    fn explicit_rollback_failure_is_not_retried_by_drop() {
        let directory = tempdir().unwrap();
        let target = directory.path().join("wokcore");
        let backup = directory.path().join(".wokcore.previous");
        std::fs::write(&target, b"new executable").unwrap();
        std::fs::write(&backup, b"old executable").unwrap();
        let installed_file = open_read_no_follow(&target).unwrap();
        let installed_identity = handle_identity(&installed_file).unwrap();
        let backup_file = open_read_no_follow(&backup).unwrap();
        let previous_identity = handle_identity(&backup_file).unwrap();
        let transaction = InstallTransaction {
            target: target.clone(),
            backup: backup.clone(),
            installed_file,
            installed_identity,
            backup_file,
            previous_identity,
            active: true,
        };

        fail_next_restore_for_test();
        assert!(transaction.rollback().is_err());

        assert_eq!(std::fs::read(&target).unwrap(), b"new executable");
        assert_eq!(std::fs::read(&backup).unwrap(), b"old executable");
    }
}
