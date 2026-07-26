use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

#[cfg(unix)]
use std::fs;

use super::SessionError;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionFileIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(windows)]
    Windows { volume_serial: u64, file_index: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionFileKind {
    RegularFile,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionFileSnapshot {
    pub identity: SessionFileIdentity,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub mutation_marker: SessionFileMutationMarker,
    pub kind: SessionFileKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionFileMutationMarker([i64; 2]);

pub struct SessionRootLease {
    chain: DirectoryChain,
}

impl SessionRootLease {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SessionError> {
        let path = path.as_ref();
        if !path.is_absolute() {
            return Err(SessionError::UnsafePath);
        }
        let chain = open_absolute_directory_chain(path)?;
        validate_directory_chain(&chain)?;
        Ok(Self { chain })
    }

    pub fn identity(&self) -> SessionFileIdentity {
        self.chain
            .directories
            .last()
            .expect("an absolute root chain contains its filesystem root")
            .identity
    }

    pub fn open_directory(
        &self,
        relative: impl AsRef<Path>,
    ) -> Result<SessionDirectoryLease, SessionError> {
        validate_directory_chain(&self.chain)?;
        let chain = extend_directory_chain(&self.chain, relative.as_ref())?;
        validate_directory_chain(&chain)?;
        Ok(SessionDirectoryLease { chain })
    }

    pub fn open_file(
        &self,
        relative: impl AsRef<Path>,
        maximum_size: u64,
    ) -> Result<SessionFile, SessionError> {
        let (parent, name) = split_relative_file(relative.as_ref())?;
        let directory = self.open_directory(parent)?;
        directory.open_file_name(&name, maximum_size)
    }

    pub(super) fn clone_chain(&self) -> Result<DirectoryChain, SessionError> {
        clone_directory_chain(&self.chain)
    }

    pub(super) fn into_chain(self) -> DirectoryChain {
        self.chain
    }
}

pub struct SessionDirectoryLease {
    chain: DirectoryChain,
}

impl SessionDirectoryLease {
    pub fn entries(
        &self,
        maximum_entries: usize,
    ) -> Result<Vec<SessionDirectoryEntry>, SessionError> {
        let generation = directory_chain_generation(&self.chain)?;
        validate_directory_chain_generation(&self.chain, &generation)?;
        #[cfg(test)]
        synchronization_tests::hit(SynchronizationPoint::BeforeEnumeration);
        let mut names = enumerate_child_names(&self.chain, maximum_entries)?;
        names.sort();
        #[cfg(test)]
        synchronization_tests::hit(SynchronizationPoint::AfterEnumeration);
        validate_directory_chain_generation(&self.chain, &generation)?;

        let mut entries = Vec::with_capacity(names.len());
        for name in names {
            validate_child_name(&name)?;
            let snapshot = snapshot_child(&self.chain, &name)?;
            entries.push(SessionDirectoryEntry { name, snapshot });
        }
        validate_directory_chain_generation(&self.chain, &generation)?;
        Ok(entries)
    }

    pub fn open_file(
        &self,
        entry: &SessionDirectoryEntry,
        maximum_size: u64,
    ) -> Result<SessionFile, SessionError> {
        if entry.snapshot.kind != SessionFileKind::RegularFile {
            return Err(SessionError::UnsafePath);
        }
        let file = self.open_file_name(&entry.name, maximum_size)?;
        if file.snapshot.identity != entry.snapshot.identity {
            return Err(SessionError::UnsafePath);
        }
        Ok(file)
    }

    fn open_file_name(&self, name: &OsStr, maximum_size: u64) -> Result<SessionFile, SessionError> {
        validate_child_name(name)?;
        let parent_generation = directory_chain_generation(&self.chain)?;
        validate_directory_chain_generation(&self.chain, &parent_generation)?;
        #[cfg(test)]
        synchronization_tests::hit(SynchronizationPoint::BeforeOpen);
        let file = open_child(&self.chain, name, false)?;
        let snapshot = snapshot_file(&file)?;
        #[cfg(test)]
        synchronization_tests::hit(SynchronizationPoint::AfterOpen);
        if snapshot.kind != SessionFileKind::RegularFile {
            return Err(SessionError::UnsafePath);
        }
        validate_directory_chain_generation(&self.chain, &parent_generation)?;
        let rechecked = open_child(&self.chain, name, false)?;
        if file_identity(&rechecked)? != snapshot.identity {
            return Err(SessionError::UnsafePath);
        }
        validate_directory_chain_generation(&self.chain, &parent_generation)?;
        if snapshot.size > maximum_size {
            return Err(SessionError::ReadLimitExceeded);
        }
        Ok(SessionFile {
            file,
            snapshot,
            parent: clone_directory_chain(&self.chain)?,
            parent_generation,
            name: name.to_os_string(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionDirectoryEntry {
    name: OsString,
    snapshot: SessionFileSnapshot,
}

impl SessionDirectoryEntry {
    pub fn name(&self) -> &OsStr {
        &self.name
    }

    pub fn snapshot(&self) -> &SessionFileSnapshot {
        &self.snapshot
    }
}

pub struct SessionFile {
    file: File,
    snapshot: SessionFileSnapshot,
    parent: DirectoryChain,
    parent_generation: Vec<DirectoryGeneration>,
    name: OsString,
}

impl SessionFile {
    pub fn snapshot(&self) -> &SessionFileSnapshot {
        &self.snapshot
    }

    pub fn read_bounded(&mut self, maximum_bytes: u64) -> Result<Vec<u8>, SessionError> {
        self.revalidate_entry()?;
        self.file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        self.file
            .by_ref()
            .take(maximum_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        self.revalidate_entry()?;
        if bytes.len() as u64 > maximum_bytes {
            return Err(SessionError::ReadLimitExceeded);
        }
        Ok(bytes)
    }

    /// Reads at most `maximum_bytes` from the already pinned Session object.
    ///
    /// The path entry, parent chain, and opened object are revalidated before
    /// and after the range read. Callers can therefore process large Session
    /// sources in bounded chunks without reopening an ambient path.
    pub fn read_range_bounded(
        &mut self,
        offset: u64,
        maximum_bytes: usize,
    ) -> Result<Vec<u8>, SessionError> {
        self.revalidate_entry()?;
        if offset >= self.snapshot.size || maximum_bytes == 0 {
            #[cfg(test)]
            synchronization_tests::hit(SynchronizationPoint::BeforeEmptyRangeReturn);
            self.revalidate_entry()?;
            return Ok(Vec::new());
        }
        self.file.seek(SeekFrom::Start(offset))?;
        let remaining = self.snapshot.size - offset;
        let maximum_bytes = u64::try_from(maximum_bytes)
            .unwrap_or(u64::MAX)
            .min(remaining);
        let capacity = usize::try_from(maximum_bytes).unwrap_or(usize::MAX);
        let mut bytes = Vec::with_capacity(capacity);
        self.file
            .by_ref()
            .take(maximum_bytes)
            .read_to_end(&mut bytes)?;
        #[cfg(test)]
        synchronization_tests::hit(SynchronizationPoint::AfterRangeReadBeforeRevalidate);
        self.revalidate_entry()?;
        Ok(bytes)
    }

    fn revalidate_entry(&self) -> Result<(), SessionError> {
        validate_directory_chain_generation(&self.parent, &self.parent_generation)
            .map_err(map_revalidation_error)?;
        let current =
            open_child(&self.parent, &self.name, false).map_err(map_revalidation_error)?;
        let current_snapshot = snapshot_file(&current).map_err(map_revalidation_error)?;
        if current_snapshot != self.snapshot
            || snapshot_file(&self.file).map_err(map_revalidation_error)? != self.snapshot
        {
            return Err(SessionError::SessionFileChanged);
        }
        validate_directory_chain_generation(&self.parent, &self.parent_generation)
            .map_err(map_revalidation_error)
    }
}

fn map_revalidation_error(error: SessionError) -> SessionError {
    match error {
        SessionError::UnsafePath => SessionError::SessionFileChanged,
        SessionError::Io { source } if source.kind() == std::io::ErrorKind::NotFound => {
            SessionError::SessionFileUnavailable
        }
        error => error,
    }
}

pub(super) struct DirectoryChain {
    pub(super) directories: Vec<PinnedDirectory>,
    root_index: usize,
}

impl DirectoryChain {
    pub(super) fn path(&self) -> &Path {
        &self
            .directories
            .last()
            .expect("a directory chain is never empty")
            .path
    }

    pub(super) fn file(&self) -> &File {
        &self
            .directories
            .last()
            .expect("a directory chain is never empty")
            .file
    }

    #[cfg(target_vendor = "apple")]
    pub(super) fn capture_stability(&self) -> Result<DirectoryChainStability, SessionError> {
        let generation = absolute_directory_chain_generation(self)?;
        validate_absolute_directory_chain_generation(self, &generation)?;
        Ok(DirectoryChainStability { generation })
    }

    #[cfg(target_vendor = "apple")]
    pub(super) fn capture_ancestor_stability(
        &self,
    ) -> Result<DirectoryChainStability, SessionError> {
        let generation = absolute_directory_chain_ancestor_generation(self)?;
        validate_absolute_directory_chain_ancestor_generation(self, &generation)?;
        Ok(DirectoryChainStability { generation })
    }

    #[cfg(target_vendor = "apple")]
    pub(super) fn verify_stability(
        &self,
        stability: &DirectoryChainStability,
    ) -> Result<(), SessionError> {
        validate_absolute_directory_chain_generation(self, &stability.generation)
    }

    #[cfg(target_vendor = "apple")]
    pub(super) fn verify_ancestor_stability(
        &self,
        stability: &DirectoryChainStability,
    ) -> Result<(), SessionError> {
        validate_absolute_directory_chain_ancestor_generation(self, &stability.generation)
    }
}

#[cfg(target_vendor = "apple")]
pub(super) struct DirectoryChainStability {
    generation: Vec<DirectoryGeneration>,
}

pub(super) struct PinnedDirectory {
    pub(super) path: PathBuf,
    pub(super) file: File,
    pub(super) identity: SessionFileIdentity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DirectoryGeneration {
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanoseconds: i64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
    #[cfg(windows)]
    last_write_time: i64,
    #[cfg(windows)]
    change_time: i64,
}

fn split_relative_file(path: &Path) -> Result<(&Path, OsString), SessionError> {
    validate_relative_path(path, false)?;
    let name = path
        .file_name()
        .ok_or(SessionError::UnsafePath)?
        .to_os_string();
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    Ok((parent, name))
}

fn validate_relative_path(path: &Path, allow_empty: bool) -> Result<(), SessionError> {
    if path.is_absolute() {
        return Err(SessionError::UnsafePath);
    }
    let mut saw_component = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => saw_component = true,
            _ => return Err(SessionError::UnsafePath),
        }
    }
    if !allow_empty && !saw_component {
        return Err(SessionError::UnsafePath);
    }
    Ok(())
}

pub(super) fn validate_child_name(name: &OsStr) -> Result<(), SessionError> {
    let path = Path::new(name);
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(SessionError::UnsafePath);
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        if name
            .encode_wide()
            .any(|character| character == u16::from(b':'))
        {
            return Err(SessionError::UnsafePath);
        }
    }
    Ok(())
}

fn extend_directory_chain(
    base: &DirectoryChain,
    relative: &Path,
) -> Result<DirectoryChain, SessionError> {
    if relative.as_os_str().is_empty() {
        return clone_directory_chain(base);
    }
    validate_relative_path(relative, true)?;
    let mut chain = clone_directory_chain(base)?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(SessionError::UnsafePath);
        };
        let path = chain.path().join(name);
        let file = open_directory_child(&chain, name)?;
        let identity = file_identity(&file)?;
        chain.directories.push(PinnedDirectory {
            path,
            file,
            identity,
        });
    }
    Ok(chain)
}

fn clone_directory_chain(chain: &DirectoryChain) -> Result<DirectoryChain, SessionError> {
    let directories = chain
        .directories
        .iter()
        .map(|directory| {
            Ok(PinnedDirectory {
                path: directory.path.clone(),
                file: directory.file.try_clone()?,
                identity: directory.identity,
            })
        })
        .collect::<Result<Vec<_>, SessionError>>()?;
    Ok(DirectoryChain {
        directories,
        root_index: chain.root_index,
    })
}

pub(super) fn validate_directory_chain(chain: &DirectoryChain) -> Result<(), SessionError> {
    for directory in &chain.directories {
        if file_identity(&directory.file)? != directory.identity {
            return Err(SessionError::UnsafePath);
        }
    }
    let current = open_absolute_directory_chain(chain.path())?;
    if current.directories.len() != chain.directories.len()
        || current
            .directories
            .iter()
            .zip(&chain.directories)
            .any(|(current, pinned)| current.identity != pinned.identity)
    {
        return Err(SessionError::UnsafePath);
    }
    Ok(())
}

fn directory_chain_generation(
    chain: &DirectoryChain,
) -> Result<Vec<DirectoryGeneration>, SessionError> {
    chain
        .directories
        .iter()
        .skip(chain.root_index)
        .map(|directory| directory_generation(&directory.file))
        .collect()
}

fn validate_directory_chain_generation(
    chain: &DirectoryChain,
    expected: &[DirectoryGeneration],
) -> Result<(), SessionError> {
    validate_directory_chain(chain)?;
    if directory_chain_generation(chain)? != expected {
        return Err(SessionError::UnsafePath);
    }
    Ok(())
}

#[cfg(target_vendor = "apple")]
fn absolute_directory_chain_generation(
    chain: &DirectoryChain,
) -> Result<Vec<DirectoryGeneration>, SessionError> {
    chain
        .directories
        .iter()
        .map(|directory| directory_generation(&directory.file))
        .collect()
}

#[cfg(target_vendor = "apple")]
fn validate_absolute_directory_chain_generation(
    chain: &DirectoryChain,
    expected: &[DirectoryGeneration],
) -> Result<(), SessionError> {
    validate_directory_chain(chain)?;
    if absolute_directory_chain_generation(chain)? != expected {
        return Err(SessionError::UnsafePath);
    }
    Ok(())
}

#[cfg(target_vendor = "apple")]
fn absolute_directory_chain_ancestor_generation(
    chain: &DirectoryChain,
) -> Result<Vec<DirectoryGeneration>, SessionError> {
    chain
        .directories
        .iter()
        .take(chain.directories.len().saturating_sub(1))
        .map(|directory| directory_generation(&directory.file))
        .collect()
}

#[cfg(target_vendor = "apple")]
fn validate_absolute_directory_chain_ancestor_generation(
    chain: &DirectoryChain,
    expected: &[DirectoryGeneration],
) -> Result<(), SessionError> {
    validate_directory_chain(chain)?;
    if absolute_directory_chain_ancestor_generation(chain)? != expected {
        return Err(SessionError::UnsafePath);
    }
    Ok(())
}

#[cfg(unix)]
fn directory_generation(file: &File) -> Result<DirectoryGeneration, SessionError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    Ok(DirectoryGeneration {
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    })
}

#[cfg(windows)]
fn directory_generation(file: &File) -> Result<DirectoryGeneration, SessionError> {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx,
    };

    let mut information = FILE_BASIC_INFO::default();
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            (&raw mut information).cast(),
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    } == 0
    {
        return Err(SessionError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(DirectoryGeneration {
        last_write_time: information.LastWriteTime,
        change_time: information.ChangeTime,
    })
}

pub(super) fn child_exists(chain: &DirectoryChain, name: &OsStr) -> Result<bool, SessionError> {
    validate_child_name(name)?;
    validate_directory_chain(chain)?;
    match open_child(chain, name, true) {
        Ok(_) => {
            validate_directory_chain(chain)?;
            Ok(true)
        }
        Err(SessionError::Io { source }) if source.kind() == std::io::ErrorKind::NotFound => {
            validate_directory_chain(chain)?;
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
pub(super) fn recheck_regular_child_identity(
    chain: &DirectoryChain,
    name: &OsStr,
    expected: SessionFileIdentity,
) -> Result<(), SessionError> {
    validate_directory_chain(chain)?;
    let file = open_child(chain, name, false)?;
    if file_identity(&file)? != expected {
        return Err(SessionError::UnsafePath);
    }
    validate_directory_chain(chain)
}

fn snapshot_child(
    chain: &DirectoryChain,
    name: &OsStr,
) -> Result<SessionFileSnapshot, SessionError> {
    validate_directory_chain(chain)?;
    let file = open_child(chain, name, true)?;
    let snapshot = snapshot_file(&file)?;
    validate_directory_chain(chain)?;
    let rechecked = open_child(chain, name, true)?;
    if file_identity(&rechecked)? != snapshot.identity {
        return Err(SessionError::UnsafePath);
    }
    validate_directory_chain(chain)?;
    Ok(snapshot)
}

#[cfg(unix)]
fn enumerate_child_names(
    chain: &DirectoryChain,
    maximum_entries: usize,
) -> Result<Vec<OsString>, SessionError> {
    use std::os::{fd::AsRawFd, unix::ffi::OsStringExt};

    struct OwnedDirectory(*mut libc::DIR);

    impl Drop for OwnedDirectory {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }

    let descriptor = unsafe {
        libc::openat(
            chain.file().as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )
    };
    if descriptor < 0 {
        return Err(map_session_io(std::io::Error::last_os_error()));
    }
    let directory = unsafe { libc::fdopendir(descriptor) };
    if directory.is_null() {
        let source = std::io::Error::last_os_error();
        unsafe {
            libc::close(descriptor);
        }
        return Err(map_session_io(source));
    }
    let directory = OwnedDirectory(directory);
    let mut names = Vec::new();
    loop {
        clear_unix_errno();
        let entry = unsafe { libc::readdir(directory.0) };
        if entry.is_null() {
            let source = std::io::Error::last_os_error();
            if source.raw_os_error().unwrap_or(0) == 0 {
                break;
            }
            return Err(map_session_io(source));
        }
        let bytes = unsafe {
            std::ffi::CStr::from_ptr((*entry).d_name.as_ptr())
                .to_bytes()
                .to_vec()
        };
        if bytes == b"." || bytes == b".." {
            continue;
        }
        if names.len() == maximum_entries {
            return Err(SessionError::EnumerationLimitExceeded);
        }
        names.push(OsString::from_vec(bytes));
    }
    Ok(names)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn clear_unix_errno() {
    unsafe {
        *libc::__errno_location() = 0;
    }
}

#[cfg(target_vendor = "apple")]
fn clear_unix_errno() {
    unsafe {
        *libc::__error() = 0;
    }
}

#[cfg(windows)]
fn enumerate_child_names(
    chain: &DirectoryChain,
    maximum_entries: usize,
) -> Result<Vec<OsString>, SessionError> {
    use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};

    use windows_sys::Win32::{
        Foundation::ERROR_NO_MORE_FILES,
        Storage::FileSystem::{
            FILE_ID_BOTH_DIR_INFO, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo,
            GetFileInformationByHandleEx,
        },
    };

    const BUFFER_SIZE: usize = 64 * 1024;

    let mut buffer = vec![0_usize; BUFFER_SIZE / std::mem::size_of::<usize>()];
    let mut restart = true;
    let mut names = Vec::new();
    loop {
        buffer.fill(0);
        let information_class = if restart {
            FileIdBothDirectoryRestartInfo
        } else {
            FileIdBothDirectoryInfo
        };
        if unsafe {
            GetFileInformationByHandleEx(
                chain.file().as_raw_handle(),
                information_class,
                buffer.as_mut_ptr().cast(),
                BUFFER_SIZE as u32,
            )
        } == 0
        {
            let source = std::io::Error::last_os_error();
            if source.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                break;
            }
            return Err(map_session_io(source));
        }
        restart = false;

        let bytes = buffer.as_ptr().cast::<u8>();
        let mut offset = 0_usize;
        loop {
            if offset
                .checked_add(std::mem::size_of::<FILE_ID_BOTH_DIR_INFO>())
                .is_none_or(|end| end > BUFFER_SIZE)
            {
                return Err(SessionError::UnsafePath);
            }
            let information = unsafe { &*bytes.add(offset).cast::<FILE_ID_BOTH_DIR_INFO>() };
            let name_bytes = information.FileNameLength as usize;
            if !name_bytes.is_multiple_of(std::mem::size_of::<u16>()) {
                return Err(SessionError::UnsafePath);
            }
            let name_offset = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
            if offset
                .checked_add(name_offset)
                .and_then(|start| start.checked_add(name_bytes))
                .is_none_or(|end| end > BUFFER_SIZE)
            {
                return Err(SessionError::UnsafePath);
            }
            let name = unsafe {
                let start = bytes.add(offset + name_offset).cast::<u16>();
                let words =
                    std::slice::from_raw_parts(start, name_bytes / std::mem::size_of::<u16>());
                OsString::from_wide(words)
            };
            if name != "." && name != ".." {
                if names.len() == maximum_entries {
                    return Err(SessionError::EnumerationLimitExceeded);
                }
                names.push(name);
            }

            let next = information.NextEntryOffset as usize;
            if next == 0 {
                break;
            }
            if next < std::mem::size_of::<FILE_ID_BOTH_DIR_INFO>()
                || offset
                    .checked_add(next)
                    .is_none_or(|next_offset| next_offset >= BUFFER_SIZE)
            {
                return Err(SessionError::UnsafePath);
            }
            offset += next;
        }
    }
    Ok(names)
}

fn snapshot_file(file: &File) -> Result<SessionFileSnapshot, SessionError> {
    let metadata = file.metadata()?;
    let kind = if metadata.is_file() {
        SessionFileKind::RegularFile
    } else if metadata.is_dir() {
        SessionFileKind::Directory
    } else {
        return Err(SessionError::UnsafePath);
    };
    Ok(SessionFileSnapshot {
        identity: file_identity(file)?,
        size: metadata.len(),
        modified: metadata.modified().ok(),
        mutation_marker: file_mutation_marker(file, &metadata)?,
        kind,
    })
}

#[cfg(unix)]
fn file_mutation_marker(
    _file: &File,
    metadata: &std::fs::Metadata,
) -> Result<SessionFileMutationMarker, SessionError> {
    use std::os::unix::fs::MetadataExt;

    Ok(SessionFileMutationMarker([
        metadata.ctime(),
        metadata.ctime_nsec(),
    ]))
}

#[cfg(windows)]
fn file_mutation_marker(
    file: &File,
    _metadata: &std::fs::Metadata,
) -> Result<SessionFileMutationMarker, SessionError> {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx,
    };

    let mut information = FILE_BASIC_INFO::default();
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            (&raw mut information).cast(),
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    } == 0
    {
        return Err(SessionError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(SessionFileMutationMarker([information.ChangeTime, 0]))
}

#[cfg(unix)]
fn open_absolute_directory_chain(path: &Path) -> Result<DirectoryChain, SessionError> {
    use std::os::unix::fs::OpenOptionsExt;

    if !path.is_absolute() {
        return Err(SessionError::UnsafePath);
    }
    let root_path = PathBuf::from("/");
    let root_file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&root_path)
        .map_err(map_session_io)?;
    let root_identity = file_identity(&root_file)?;
    let mut chain = DirectoryChain {
        directories: vec![PinnedDirectory {
            path: root_path,
            file: root_file,
            identity: root_identity,
        }],
        root_index: 0,
    };
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                let component_path = chain.path().join(name);
                let file = open_directory_child(&chain, name)?;
                let identity = file_identity(&file)?;
                chain.directories.push(PinnedDirectory {
                    path: component_path,
                    file,
                    identity,
                });
            }
            _ => return Err(SessionError::UnsafePath),
        }
    }
    chain.root_index = chain.directories.len() - 1;
    Ok(chain)
}

#[cfg(windows)]
fn open_absolute_directory_chain(path: &Path) -> Result<DirectoryChain, SessionError> {
    if !path.is_absolute() {
        return Err(SessionError::UnsafePath);
    }
    let mut current = PathBuf::new();
    let mut directories = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => {
                current.push(component.as_os_str());
                let file = open_windows_absolute(&current, WindowsExpectedKind::Directory)?;
                let identity = file_identity(&file)?;
                directories.push(PinnedDirectory {
                    path: current.clone(),
                    file,
                    identity,
                });
            }
            Component::Normal(name) => {
                current.push(name);
                let parent = directories
                    .last()
                    .expect("a Windows absolute path has a filesystem root");
                let file =
                    open_windows_relative(&parent.file, name, WindowsExpectedKind::Directory)?;
                let identity = file_identity(&file)?;
                directories.push(PinnedDirectory {
                    path: current.clone(),
                    file,
                    identity,
                });
            }
            _ => return Err(SessionError::UnsafePath),
        }
    }
    if directories.is_empty() {
        return Err(SessionError::UnsafePath);
    }
    let root_index = directories.len() - 1;
    Ok(DirectoryChain {
        directories,
        root_index,
    })
}

#[cfg(unix)]
fn open_directory_child(chain: &DirectoryChain, name: &OsStr) -> Result<File, SessionError> {
    open_unix_child(
        chain,
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
    )
}

#[cfg(windows)]
fn open_directory_child(chain: &DirectoryChain, name: &OsStr) -> Result<File, SessionError> {
    open_windows_relative(chain.file(), name, WindowsExpectedKind::Directory)
}

#[cfg(unix)]
fn open_child(
    chain: &DirectoryChain,
    name: &OsStr,
    allow_directory: bool,
) -> Result<File, SessionError> {
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK;
    let file = open_unix_child(chain, name, flags)?;
    let snapshot = snapshot_file(&file)?;
    if !allow_directory && snapshot.kind != SessionFileKind::RegularFile {
        return Err(SessionError::UnsafePath);
    }
    Ok(file)
}

#[cfg(windows)]
fn open_child(
    chain: &DirectoryChain,
    name: &OsStr,
    allow_directory: bool,
) -> Result<File, SessionError> {
    let expected = if allow_directory {
        WindowsExpectedKind::Any
    } else {
        WindowsExpectedKind::RegularFile
    };
    let file = open_windows_relative(chain.file(), name, expected)?;
    let snapshot = snapshot_file(&file)?;
    if !allow_directory && snapshot.kind != SessionFileKind::RegularFile {
        return Err(SessionError::UnsafePath);
    }
    Ok(file)
}

#[cfg(unix)]
fn open_unix_child(chain: &DirectoryChain, name: &OsStr, flags: i32) -> Result<File, SessionError> {
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    };

    validate_child_name(name)?;
    let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| SessionError::UnsafePath)?;
    let directory = chain
        .directories
        .last()
        .expect("a directory chain is never empty");
    let descriptor = unsafe {
        libc::openat(
            directory.file.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            0,
        )
    };
    if descriptor < 0 {
        return Err(map_session_io(std::io::Error::last_os_error()));
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

#[cfg(windows)]
#[derive(Clone, Copy)]
enum WindowsExpectedKind {
    Any,
    Directory,
    RegularFile,
}

#[cfg(windows)]
fn open_windows_absolute(path: &Path, expected: WindowsExpectedKind) -> Result<File, SessionError> {
    use std::{
        os::windows::{ffi::OsStrExt, io::FromRawHandle},
        ptr,
    };

    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        },
    };

    let wide_path = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let flags = FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS;
    let handle = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            flags,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(map_session_io(std::io::Error::last_os_error()));
    }
    let file = unsafe { File::from_raw_handle(handle) };
    verify_windows_file(&file, expected)?;
    Ok(file)
}

#[cfg(windows)]
fn open_windows_relative(
    parent: &File,
    name: &OsStr,
    expected: WindowsExpectedKind,
) -> Result<File, SessionError> {
    use std::{
        ffi::c_void,
        mem::size_of,
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle},
        },
        ptr,
    };

    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, SYNCHRONIZE},
    };

    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }

    #[repr(C)]
    struct ObjectAttributes {
        length: u32,
        root_directory: HANDLE,
        object_name: *mut UnicodeString,
        attributes: u32,
        security_descriptor: *mut c_void,
        security_quality_of_service: *mut c_void,
    }

    #[repr(C)]
    struct IoStatusBlock {
        status_or_pointer: usize,
        information: usize,
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtCreateFile(
            file_handle: *mut HANDLE,
            desired_access: u32,
            object_attributes: *mut ObjectAttributes,
            io_status_block: *mut IoStatusBlock,
            allocation_size: *const i64,
            file_attributes: u32,
            share_access: u32,
            create_disposition: u32,
            create_options: u32,
            ea_buffer: *const c_void,
            ea_length: u32,
        ) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }

    validate_child_name(name)?;
    let mut wide_name = name.encode_wide().collect::<Vec<_>>();
    let byte_length = wide_name
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|length| u16::try_from(length).ok())
        .ok_or(SessionError::UnsafePath)?;
    let mut unicode = UnicodeString {
        length: byte_length,
        maximum_length: byte_length,
        buffer: wide_name.as_mut_ptr(),
    };
    let mut attributes = ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root_directory: parent.as_raw_handle(),
        object_name: &mut unicode,
        attributes: 0x40,
        security_descriptor: ptr::null_mut(),
        security_quality_of_service: ptr::null_mut(),
    };
    let mut io_status = IoStatusBlock {
        status_or_pointer: 0,
        information: 0,
    };
    let mut handle = INVALID_HANDLE_VALUE;
    let type_options = match expected {
        WindowsExpectedKind::Any => 0,
        WindowsExpectedKind::Directory => 0x1,
        WindowsExpectedKind::RegularFile => 0,
    };
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            GENERIC_READ | SYNCHRONIZE,
            &mut attributes,
            &mut io_status,
            ptr::null(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            1,
            type_options | 0x20 | 0x0020_0000,
            ptr::null(),
            0,
        )
    };
    if status < 0 {
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(map_session_io(std::io::Error::from_raw_os_error(
            code as i32,
        )));
    }
    if handle == INVALID_HANDLE_VALUE {
        return Err(SessionError::UnsafePath);
    }
    let file = unsafe { File::from_raw_handle(handle) };
    verify_windows_file(&file, expected)?;
    Ok(file)
}

#[cfg(windows)]
fn verify_windows_file(file: &File, expected: WindowsExpectedKind) -> Result<(), SessionError> {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        GetFileInformationByHandle,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
        return Err(SessionError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(SessionError::UnsafePath);
    }
    let is_directory = information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    let matches_expected = match expected {
        WindowsExpectedKind::Any => true,
        WindowsExpectedKind::Directory => is_directory,
        WindowsExpectedKind::RegularFile => !is_directory,
    };
    if !matches_expected {
        return Err(SessionError::UnsafePath);
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn file_identity(file: &File) -> Result<SessionFileIdentity, SessionError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    Ok(SessionFileIdentity::Unix {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
pub(super) fn file_identity(file: &File) -> Result<SessionFileIdentity, SessionError> {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
        return Err(SessionError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(SessionFileIdentity::Windows {
        volume_serial: u64::from(information.dwVolumeSerialNumber),
        file_index: u64::from(information.nFileIndexHigh) << 32
            | u64::from(information.nFileIndexLow),
    })
}

fn map_session_io(source: std::io::Error) -> SessionError {
    #[cfg(unix)]
    if source
        .raw_os_error()
        .is_some_and(|code| code == libc::ELOOP || code == libc::ENOTDIR || code == libc::EISDIR)
    {
        return SessionError::UnsafePath;
    }
    #[cfg(windows)]
    if source.raw_os_error().is_some_and(|code| {
        code == windows_sys::Win32::Foundation::ERROR_CANT_ACCESS_FILE as i32
            || code == windows_sys::Win32::Foundation::ERROR_DIRECTORY as i32
    }) {
        return SessionError::UnsafePath;
    }
    SessionError::Io { source }
}

#[cfg(test)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum SynchronizationPoint {
    BeforeEnumeration,
    AfterEnumeration,
    BeforeOpen,
    AfterOpen,
    BeforeEmptyRangeReturn,
    AfterRangeReadBeforeRevalidate,
}

#[cfg(test)]
mod synchronization_tests {
    use std::{
        ffi::{OsStr, OsString},
        fs::{self, FileTimes, OpenOptions},
        io::Write,
        path::PathBuf,
        sync::{Arc, Barrier, Mutex, mpsc},
        thread::{self, ThreadId},
    };

    use super::{SessionError, SessionRootLease, SynchronizationPoint, enumerate_child_names};
    #[cfg(windows)]
    use super::{file_identity, open_child, open_directory_child};
    #[cfg(windows)]
    use std::io::Read;

    struct InstalledHook {
        thread: ThreadId,
        windows: Vec<(SynchronizationPoint, Arc<HookWindow>)>,
    }

    struct HookWindow {
        reached: Barrier,
        resume: Barrier,
    }

    impl HookWindow {
        fn new() -> Self {
            Self {
                reached: Barrier::new(2),
                resume: Barrier::new(2),
            }
        }

        fn pause_operation(&self) {
            self.reached.wait();
            self.resume.wait();
        }

        fn wait_until_reached(&self) {
            self.reached.wait();
        }

        fn resume_operation(&self) {
            self.resume.wait();
        }
    }

    static INSTALLED_HOOKS: Mutex<Vec<InstalledHook>> = Mutex::new(Vec::new());

    pub(super) fn hit(point: SynchronizationPoint) {
        let window = {
            let hooks = INSTALLED_HOOKS.lock().unwrap();
            let Some(hook) = hooks
                .iter()
                .find(|hook| hook.thread == thread::current().id())
            else {
                return;
            };
            let Some((_, window)) = hook
                .windows
                .iter()
                .find(|(installed, _)| *installed == point)
            else {
                return;
            };
            Arc::clone(window)
        };
        window.pause_operation();
    }

    #[test]
    fn synchronized_directory_aba_during_enumeration_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let relative = PathBuf::from("sessions/2026/07/26");
        let directory_path = root.path().join(&relative);
        fs::create_dir_all(&directory_path).unwrap();
        fs::write(directory_path.join("session.jsonl"), b"original").unwrap();
        let replacement_path = root.path().join("replacement");
        fs::create_dir(&replacement_path).unwrap();
        fs::write(replacement_path.join("session.jsonl"), b"replacement").unwrap();
        let lease = SessionRootLease::open(root.path()).unwrap();
        let directory = lease.open_directory(&relative).unwrap();
        let moved_path = root.path().join("moved");
        let before_enumeration = Arc::new(HookWindow::new());
        let after_enumeration = Arc::new(HookWindow::new());
        let worker_before = Arc::clone(&before_enumeration);
        let worker_after = Arc::clone(&after_enumeration);
        let (ready_sender, ready_receiver) = mpsc::channel();

        let worker = thread::spawn(move || {
            install_hook(
                thread::current().id(),
                vec![
                    (SynchronizationPoint::BeforeEnumeration, worker_before),
                    (SynchronizationPoint::AfterEnumeration, worker_after),
                ],
                &ready_sender,
            );
            let result = directory.entries(8);
            clear_hook();
            result
        });

        ready_receiver.recv().unwrap();
        before_enumeration.wait_until_reached();
        fs::rename(&directory_path, &moved_path).unwrap();
        fs::rename(&replacement_path, &directory_path).unwrap();
        assert_eq!(
            fs::read(directory_path.join("session.jsonl")).unwrap(),
            b"replacement"
        );
        before_enumeration.resume_operation();

        after_enumeration.wait_until_reached();
        fs::rename(&directory_path, &replacement_path).unwrap();
        fs::rename(&moved_path, &directory_path).unwrap();
        assert_eq!(
            fs::read(directory_path.join("session.jsonl")).unwrap(),
            b"original"
        );
        after_enumeration.resume_operation();

        assert!(matches!(
            worker.join().unwrap(),
            Err(SessionError::UnsafePath)
        ));
    }

    #[test]
    fn synchronized_same_identity_entry_aba_during_file_open_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let relative = PathBuf::from("sessions/2026/07/26");
        let directory_path = root.path().join(&relative);
        fs::create_dir_all(&directory_path).unwrap();
        let file_path = directory_path.join("session.jsonl");
        fs::write(&file_path, b"original").unwrap();
        let replacement_path = directory_path.join("replacement.jsonl");
        fs::hard_link(&file_path, &replacement_path).unwrap();
        let lease = SessionRootLease::open(root.path()).unwrap();
        let directory = lease.open_directory(&relative).unwrap();
        let moved_path = directory_path.join("moved.jsonl");
        let before_open = Arc::new(HookWindow::new());
        let after_open = Arc::new(HookWindow::new());
        let worker_before = Arc::clone(&before_open);
        let worker_after = Arc::clone(&after_open);
        let (ready_sender, ready_receiver) = mpsc::channel();

        let worker = thread::spawn(move || {
            install_hook(
                thread::current().id(),
                vec![
                    (SynchronizationPoint::BeforeOpen, worker_before),
                    (SynchronizationPoint::AfterOpen, worker_after),
                ],
                &ready_sender,
            );
            let result = directory.open_file_name(OsStr::new("session.jsonl"), 1024);
            clear_hook();
            result
        });

        ready_receiver.recv().unwrap();
        before_open.wait_until_reached();
        fs::rename(&file_path, &moved_path).unwrap();
        fs::rename(&replacement_path, &file_path).unwrap();
        assert!(moved_path.exists());
        assert!(!replacement_path.exists());
        assert_eq!(fs::read(&file_path).unwrap(), b"original");
        before_open.resume_operation();

        after_open.wait_until_reached();
        fs::rename(&file_path, &replacement_path).unwrap();
        fs::rename(&moved_path, &file_path).unwrap();
        assert!(replacement_path.exists());
        assert!(!moved_path.exists());
        assert_eq!(fs::read(&file_path).unwrap(), b"original");
        after_open.resume_operation();

        assert!(matches!(
            worker.join().unwrap(),
            Err(SessionError::UnsafePath)
        ));
    }

    #[test]
    fn synchronized_empty_range_return_detects_truncate_and_replacement() {
        for replace in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let relative = PathBuf::from("sessions/2026/07/26/session.jsonl");
            let file_path = root.path().join(&relative);
            fs::create_dir_all(file_path.parent().unwrap()).unwrap();
            fs::write(&file_path, b"original").unwrap();
            let lease = SessionRootLease::open(root.path()).unwrap();
            let mut file = lease.open_file(&relative, u64::MAX).unwrap();
            let window = Arc::new(HookWindow::new());
            let worker_window = Arc::clone(&window);
            let (ready_sender, ready_receiver) = mpsc::channel();

            let worker = thread::spawn(move || {
                install_hook(
                    thread::current().id(),
                    vec![(SynchronizationPoint::BeforeEmptyRangeReturn, worker_window)],
                    &ready_sender,
                );
                let result = file.read_range_bounded(u64::MAX, 0);
                clear_hook();
                result
            });

            ready_receiver.recv().unwrap();
            window.wait_until_reached();
            if replace {
                let moved = file_path.with_extension("moved");
                fs::rename(&file_path, moved).unwrap();
                fs::write(&file_path, b"replacement").unwrap();
            } else {
                OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(&file_path)
                    .unwrap()
                    .write_all(b"")
                    .unwrap();
            }
            window.resume_operation();
            assert!(matches!(
                worker.join().unwrap(),
                Err(SessionError::SessionFileChanged | SessionError::SessionFileUnavailable)
            ));
        }
    }

    #[test]
    fn synchronized_range_read_rejects_same_length_rewrite_with_restored_mtime() {
        let root = tempfile::tempdir().unwrap();
        let relative = PathBuf::from("sessions/2026/07/26/session.jsonl");
        let file_path = root.path().join(&relative);
        fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        fs::write(&file_path, b"original").unwrap();
        let modified = fs::metadata(&file_path).unwrap().modified().unwrap();
        let lease = SessionRootLease::open(root.path()).unwrap();
        let mut file = lease.open_file(&relative, u64::MAX).unwrap();
        let window = Arc::new(HookWindow::new());
        let worker_window = Arc::clone(&window);
        let (ready_sender, ready_receiver) = mpsc::channel();

        let worker = thread::spawn(move || {
            install_hook(
                thread::current().id(),
                vec![(
                    SynchronizationPoint::AfterRangeReadBeforeRevalidate,
                    worker_window,
                )],
                &ready_sender,
            );
            let result = file.read_range_bounded(0, 8);
            clear_hook();
            result
        });

        ready_receiver.recv().unwrap();
        window.wait_until_reached();
        let mut replacement = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&file_path)
            .unwrap();
        replacement.write_all(b"replaced").unwrap();
        replacement
            .set_times(FileTimes::new().set_modified(modified))
            .unwrap();
        drop(replacement);
        window.resume_operation();

        assert!(matches!(
            worker.join().unwrap(),
            Err(SessionError::SessionFileChanged | SessionError::SessionFileUnavailable)
        ));
    }

    #[test]
    fn directory_enumeration_reads_the_pinned_handle_after_a_path_swap() {
        let root = tempfile::tempdir().unwrap();
        let directory_path = root.path().join("sessions");
        fs::create_dir(&directory_path).unwrap();
        fs::write(directory_path.join("original.jsonl"), b"original").unwrap();
        let replacement_path = root.path().join("replacement");
        fs::create_dir(&replacement_path).unwrap();
        fs::write(replacement_path.join("replacement.jsonl"), b"replacement").unwrap();
        let lease = SessionRootLease::open(root.path()).unwrap();
        let directory = lease.open_directory("sessions").unwrap();
        let moved_path = root.path().join("moved");
        fs::rename(&directory_path, &moved_path).unwrap();
        fs::rename(&replacement_path, &directory_path).unwrap();

        assert_eq!(
            enumerate_child_names(&directory.chain, 8).unwrap(),
            vec![OsString::from("original.jsonl")]
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_child_opens_are_relative_to_the_pinned_parent_handle() {
        let container = tempfile::tempdir().unwrap();
        let root_path = container.path().join("root");
        let day_path = root_path.join("sessions/day");
        fs::create_dir_all(&day_path).unwrap();
        fs::write(day_path.join("session.jsonl"), b"original").unwrap();
        let root = SessionRootLease::open(&root_path).unwrap();
        let original_sessions_identity =
            file_identity(&open_directory_child(&root.chain, OsStr::new("sessions")).unwrap())
                .unwrap();
        let moved_path = container.path().join("moved");
        fs::rename(&root_path, &moved_path).unwrap();
        fs::create_dir_all(root_path.join("sessions/day")).unwrap();
        fs::write(root_path.join("sessions/day/session.jsonl"), b"replacement").unwrap();

        let reopened_sessions = open_directory_child(&root.chain, OsStr::new("sessions")).unwrap();
        assert_eq!(
            file_identity(&reopened_sessions).unwrap(),
            original_sessions_identity
        );
        drop(reopened_sessions);
        fs::remove_dir_all(&root_path).unwrap();
        fs::rename(&moved_path, &root_path).unwrap();

        let day = root.open_directory("sessions/day").unwrap();
        let moved_day = root_path.join("sessions/moved-day");
        fs::rename(&day_path, &moved_day).unwrap();
        fs::create_dir(&day_path).unwrap();
        fs::write(day_path.join("session.jsonl"), b"replacement").unwrap();
        let mut reopened_file = open_child(&day.chain, OsStr::new("session.jsonl"), false).unwrap();
        let mut bytes = Vec::new();
        reopened_file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"original");
    }

    fn install_hook(
        thread: ThreadId,
        windows: Vec<(SynchronizationPoint, Arc<HookWindow>)>,
        ready_sender: &mpsc::Sender<()>,
    ) {
        let mut hooks = INSTALLED_HOOKS.lock().unwrap();
        assert!(hooks.iter().all(|hook| hook.thread != thread));
        hooks.push(InstalledHook { thread, windows });
        ready_sender.send(()).unwrap();
    }

    fn clear_hook() {
        let thread = thread::current().id();
        let mut hooks = INSTALLED_HOOKS.lock().unwrap();
        hooks.retain(|hook| hook.thread != thread);
    }
}
