use std::{
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

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
    pub kind: SessionFileKind,
}

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
        validate_directory_chain(&self.chain)?;
        let mut names = Vec::new();
        for entry in fs::read_dir(self.chain.path())? {
            if names.len() == maximum_entries {
                return Err(SessionError::EnumerationLimitExceeded);
            }
            names.push(entry?.file_name());
        }
        names.sort();
        validate_directory_chain(&self.chain)?;

        let mut entries = Vec::with_capacity(names.len());
        for name in names {
            validate_child_name(&name)?;
            let snapshot = snapshot_child(&self.chain, &name)?;
            entries.push(SessionDirectoryEntry { name, snapshot });
        }
        validate_directory_chain(&self.chain)?;
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
        validate_directory_chain(&self.chain)?;
        let file = open_child(&self.chain, name, false)?;
        let snapshot = snapshot_file(&file)?;
        if snapshot.kind != SessionFileKind::RegularFile {
            return Err(SessionError::UnsafePath);
        }
        validate_directory_chain(&self.chain)?;
        let rechecked = open_child(&self.chain, name, false)?;
        if file_identity(&rechecked)? != snapshot.identity {
            return Err(SessionError::UnsafePath);
        }
        validate_directory_chain(&self.chain)?;
        if snapshot.size > maximum_size {
            return Err(SessionError::ReadLimitExceeded);
        }
        Ok(SessionFile { file, snapshot })
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
}

impl SessionFile {
    pub fn snapshot(&self) -> &SessionFileSnapshot {
        &self.snapshot
    }

    pub fn read_bounded(&mut self, maximum_bytes: u64) -> Result<Vec<u8>, SessionError> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        self.file
            .by_ref()
            .take(maximum_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > maximum_bytes {
            return Err(SessionError::ReadLimitExceeded);
        }
        Ok(bytes)
    }
}

pub(super) struct DirectoryChain {
    pub(super) directories: Vec<PinnedDirectory>,
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
}

pub(super) struct PinnedDirectory {
    pub(super) path: PathBuf,
    pub(super) file: File,
    pub(super) identity: SessionFileIdentity,
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
    Ok(DirectoryChain { directories })
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
        kind,
    })
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
                let file = open_windows_path(&current, WindowsExpectedKind::Directory)?;
                let identity = file_identity(&file)?;
                directories.push(PinnedDirectory {
                    path: current.clone(),
                    file,
                    identity,
                });
            }
            Component::Normal(name) => {
                current.push(name);
                let file = open_windows_path(&current, WindowsExpectedKind::Directory)?;
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
    Ok(DirectoryChain { directories })
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
    open_windows_path(&chain.path().join(name), WindowsExpectedKind::Directory)
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
    let file = open_windows_path(&chain.path().join(name), expected)?;
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
fn open_windows_path(path: &Path, expected: WindowsExpectedKind) -> Result<File, SessionError> {
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
