use std::{
    fs::{self, File, OpenOptions},
    path::Path,
};

#[cfg(windows)]
use std::path::PathBuf;
#[cfg(windows)]
use tempfile::Builder;

use super::manifest::UpdateError;

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_RESTORE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(super) fn fail_next_restore_for_test() {
    FAIL_NEXT_RESTORE.set(true);
}

pub(super) fn atomic_replace(
    target: &Path,
    candidate: &Path,
    candidate_file: &File,
    backup: &Path,
) -> Result<(), UpdateError> {
    remove_stale(backup)?;
    atomic_replace_platform(target, candidate, candidate_file, backup)
}

#[cfg(unix)]
pub(super) fn open_replacement(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(windows)]
pub(super) fn open_replacement(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::{
        Foundation::GENERIC_READ,
        Storage::FileSystem::{
            DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE,
        },
    };

    OpenOptions::new()
        .access_mode(GENERIC_READ | DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

pub(super) fn restore_previous(
    target: &Path,
    backup: &Path,
    backup_file: &File,
) -> Result<(), UpdateError> {
    #[cfg(test)]
    if FAIL_NEXT_RESTORE.replace(false) {
        return Err(UpdateError::RollbackFailed);
    }
    let metadata = fs::symlink_metadata(backup).map_err(|_| UpdateError::RollbackFailed)?;
    if !safe_regular_file(&metadata) {
        return Err(UpdateError::RollbackFailed);
    }
    restore_platform(target, backup, backup_file).map_err(|_| UpdateError::RollbackFailed)
}

fn remove_stale(path: &Path) -> Result<(), UpdateError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !safe_regular_file(&metadata) {
                return Err(UpdateError::AtomicReplaceFailed);
            }
            fs::remove_file(path).map_err(|_| UpdateError::AtomicReplaceFailed)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(UpdateError::AtomicReplaceFailed),
    }
}

#[cfg(unix)]
fn atomic_replace_platform(
    target: &Path,
    candidate: &Path,
    candidate_file: &File,
    backup: &Path,
) -> Result<(), UpdateError> {
    fs::hard_link(target, backup).map_err(|_| UpdateError::AtomicReplaceFailed)?;
    let backup_file = open_replacement(backup).map_err(|_| UpdateError::AtomicReplaceFailed)?;
    if !path_matches_open_file(candidate, candidate_file)
        || !path_matches_open_file(target, &backup_file)
        || !path_matches_open_file(backup, &backup_file)
    {
        return Err(UpdateError::AtomicReplaceFailed);
    }
    if fs::rename(candidate, target).is_err() {
        return Err(UpdateError::AtomicReplaceFailed);
    }
    if sync_parent(target).is_err() {
        restore_platform(target, backup, &backup_file).map_err(|_| UpdateError::RollbackFailed)?;
        return Err(UpdateError::AtomicReplaceFailed);
    }
    Ok(())
}

#[cfg(windows)]
fn atomic_replace_platform(
    target: &Path,
    candidate: &Path,
    candidate_file: &File,
    backup: &Path,
) -> Result<(), UpdateError> {
    if replace_platform(target, candidate, candidate_file, Some(backup)).is_ok() {
        return Ok(());
    }
    recover_partial_replace(target, backup)?;
    Err(UpdateError::AtomicReplaceFailed)
}

#[cfg(windows)]
fn recover_partial_replace(target: &Path, backup: &Path) -> Result<(), UpdateError> {
    match fs::symlink_metadata(target) {
        Ok(metadata) if safe_regular_file(&metadata) => {
            return Ok(());
        }
        Ok(_) => return Err(UpdateError::RollbackFailed),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(UpdateError::RollbackFailed),
    }
    let metadata = fs::symlink_metadata(backup).map_err(|_| UpdateError::RollbackFailed)?;
    if !safe_regular_file(&metadata) {
        return Err(UpdateError::RollbackFailed);
    }
    fs::rename(backup, target).map_err(|_| UpdateError::RollbackFailed)
}

#[cfg(unix)]
fn restore_platform(target: &Path, backup: &Path, backup_file: &File) -> std::io::Result<()> {
    if !path_matches_open_file(backup, backup_file) {
        return Err(std::io::Error::other("backup identity changed"));
    }
    fs::rename(backup, target)?;
    sync_parent(target)
}

#[cfg(unix)]
fn path_matches_open_file(path: &Path, file: &File) -> bool {
    use std::os::unix::fs::MetadataExt;

    let Ok(path_metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    let Ok(file_metadata) = file.metadata() else {
        return false;
    };
    safe_regular_file(&path_metadata)
        && path_metadata.dev() == file_metadata.dev()
        && path_metadata.ino() == file_metadata.ino()
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("target parent is missing"))?;
    fs::File::open(parent)?.sync_all()
}

#[cfg(windows)]
fn replace_platform(
    target: &Path,
    _candidate: &Path,
    candidate_file: &File,
    backup: Option<&Path>,
) -> std::io::Result<()> {
    use std::{
        mem::{offset_of, size_of},
        os::windows::io::AsRawHandle,
        ptr,
    };

    use windows_sys::Win32::Storage::FileSystem::{
        CreateHardLinkW, FILE_RENAME_INFO, FileRenameInfoEx, SetFileInformationByHandle,
    };

    const FILE_RENAME_FLAG_REPLACE_IF_EXISTS: u32 = 0x1;
    const FILE_RENAME_FLAG_POSIX_SEMANTICS: u32 = 0x2;

    let backup = backup.ok_or_else(|| std::io::Error::other("backup path is required"))?;
    let backup_wide = wide_path(backup)?;
    let target_wide = wide_path(target)?;
    if unsafe { CreateHardLinkW(backup_wide.as_ptr(), target_wide.as_ptr(), ptr::null()) } == 0 {
        return Err(std::io::Error::last_os_error());
    }

    let target_name = &target_wide[..target_wide.len().saturating_sub(1)];
    let name_bytes = target_name
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| std::io::Error::other("target path is too long"))?;
    let buffer_size = offset_of!(FILE_RENAME_INFO, FileName)
        .checked_add(name_bytes as usize)
        .ok_or_else(|| std::io::Error::other("target path is too long"))?;
    let mut buffer = vec![0_u8; buffer_size];
    let information = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        (*information).Anonymous.Flags =
            FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS;
        (*information).RootDirectory = ptr::null_mut();
        (*information).FileNameLength = name_bytes;
        ptr::copy_nonoverlapping(
            target_name.as_ptr(),
            buffer
                .as_mut_ptr()
                .add(offset_of!(FILE_RENAME_INFO, FileName))
                .cast::<u16>(),
            target_name.len(),
        );
    }
    let renamed = unsafe {
        SetFileInformationByHandle(
            candidate_file.as_raw_handle().cast(),
            FileRenameInfoEx,
            buffer.as_ptr().cast(),
            u32::try_from(buffer.len())
                .map_err(|_| std::io::Error::other("target path is too long"))?,
        )
    };
    if renamed == 0 {
        let error = std::io::Error::last_os_error();
        let _ = fs::remove_file(backup);
        Err(error)
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn restore_platform(target: &Path, _backup: &Path, backup_file: &File) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom};

    let parent = target
        .parent()
        .ok_or_else(|| std::io::Error::other("target parent is missing"))?;
    let mut rollback = Builder::new()
        .prefix(".wokcore-rollback-candidate-")
        .tempfile_in(parent)?;
    let mut source = backup_file.try_clone()?;
    source.seek(SeekFrom::Start(0))?;
    std::io::copy(&mut source, rollback.as_file_mut())?;
    rollback.as_file_mut().sync_all()?;
    let rollback_path = rollback.into_temp_path().keep()?;
    let failed = failed_path(target);
    let _ = fs::remove_file(&failed);
    let rollback_file = open_replacement(&rollback_path)?;
    let result = replace_platform(target, &rollback_path, &rollback_file, Some(&failed));
    if result.is_ok() {
        let _ = fs::remove_file(failed);
    } else {
        recover_partial_restore(target, &failed)?;
        let _ = fs::remove_file(rollback_path);
    }
    result
}

#[cfg(windows)]
fn recover_partial_restore(target: &Path, failed: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(target) {
        Ok(metadata) if safe_regular_file(&metadata) => {
            return Ok(());
        }
        Ok(_) => {
            return Err(std::io::Error::other(
                "partial rollback left an unsafe target",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(failed)?;
    if !safe_regular_file(&metadata) {
        return Err(std::io::Error::other(
            "partial rollback did not preserve the replaced executable",
        ));
    }
    fs::rename(failed, target)
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

#[cfg(windows)]
fn failed_path(target: &Path) -> PathBuf {
    target
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(
            ".{}.failed",
            target
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("wokcore.exe")
        ))
}

#[cfg(windows)]
fn wide_path(path: &Path) -> std::io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    let mut value = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if value.contains(&0) {
        return Err(std::io::Error::other("path contains a null character"));
    }
    value.push(0);
    Ok(value)
}

#[cfg(all(test, unix))]
mod unix_tests {
    use tempfile::tempdir;

    use super::{open_replacement, restore_platform, restore_previous};

    #[test]
    fn replaced_sync_recovery_ignores_a_recreated_candidate_path() {
        let directory = tempdir().unwrap();
        let target = directory.path().join("wokcore");
        let candidate = directory.path().join("candidate");
        let backup = directory.path().join(".wokcore.previous");
        std::fs::write(&target, b"new executable").unwrap();
        std::fs::write(&candidate, b"attacker replacement").unwrap();
        std::fs::write(&backup, b"old executable").unwrap();
        let backup_file = open_replacement(&backup).unwrap();

        restore_platform(&target, &backup, &backup_file).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"old executable");
        assert_eq!(std::fs::read(&candidate).unwrap(), b"attacker replacement");
        assert!(!backup.exists());
    }

    #[test]
    fn rollback_rejects_a_backup_path_swap_after_the_handle_is_opened() {
        let directory = tempdir().unwrap();
        let target = directory.path().join("wokcore");
        let backup = directory.path().join(".wokcore.previous");
        let preserved = directory.path().join("preserved.previous");
        std::fs::write(&target, b"new executable").unwrap();
        std::fs::write(&backup, b"old executable").unwrap();
        let backup_file = open_replacement(&backup).unwrap();
        std::fs::rename(&backup, &preserved).unwrap();
        std::fs::write(&backup, b"attacker replacement").unwrap();

        assert!(restore_previous(&target, &backup, &backup_file).is_err());

        assert_eq!(std::fs::read(&target).unwrap(), b"new executable");
        assert_eq!(std::fs::read(&preserved).unwrap(), b"old executable");
        assert_eq!(std::fs::read(&backup).unwrap(), b"attacker replacement");
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::{
        os::windows::process::CommandExt,
        process::{Child, Command, Stdio},
    };

    use tempfile::tempdir;

    use super::{
        atomic_replace, open_replacement, recover_partial_replace, recover_partial_restore,
        restore_previous,
    };

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn running_windows_image_is_atomically_replaced_and_rolled_back() {
        let directory = tempdir().unwrap();
        let target = directory.path().join("wokcore.exe");
        let candidate = directory.path().join("candidate.exe");
        let backup = directory.path().join(".wokcore.exe.previous");
        let system_root = std::env::var_os("SystemRoot").unwrap();
        std::fs::copy(
            std::path::Path::new(&system_root)
                .join("System32")
                .join("cmd.exe"),
            &target,
        )
        .unwrap();
        let original = std::fs::read(&target).unwrap();
        std::fs::write(&candidate, b"new executable").unwrap();
        let mut child = ChildGuard(
            Command::new(&target)
                .args(["/d", "/q", "/c", "set /p WOKCORE_UPDATE_TEST_INPUT="])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(0x0800_0000)
                .spawn()
                .unwrap(),
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(child.0.try_wait().unwrap().is_none());

        let candidate_file = open_replacement(&candidate).unwrap();
        atomic_replace(&target, &candidate, &candidate_file, &backup).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new executable");
        assert_eq!(std::fs::read(&backup).unwrap(), original);

        let backup_file = open_replacement(&backup).unwrap();
        restore_previous(&target, &backup, &backup_file).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), original);
        assert_eq!(std::fs::read(&backup).unwrap(), original);
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        std::fs::remove_file(&backup).unwrap();
    }

    #[test]
    fn partial_windows_replace_failures_restore_a_named_target() {
        let directory = tempdir().unwrap();
        let target = directory.path().join("wokcore.exe");
        let backup = directory.path().join(".wokcore.exe.previous");
        std::fs::write(&backup, b"old executable").unwrap();

        recover_partial_replace(&target, &backup).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"old executable");
        assert!(!backup.exists());
    }

    #[test]
    fn partial_windows_rollback_failures_restore_the_failed_target() {
        let directory = tempdir().unwrap();
        let target = directory.path().join("wokcore.exe");
        let failed = directory.path().join(".wokcore.exe.failed");
        std::fs::write(&failed, b"new executable").unwrap();

        recover_partial_restore(&target, &failed).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"new executable");
        assert!(!failed.exists());
    }
}
