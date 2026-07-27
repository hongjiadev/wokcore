use std::{
    ffi::{OsStr, OsString},
    fmt,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    time::SystemTime,
};

use crate::{
    PlatformError,
    runtime::permissions::{harden_current_user_directory, verify_current_user_directory},
    sessions::{
        PinnedExportDestination, PinnedPublishedFile, SessionDirectoryEntry, SessionError,
        SessionFileIdentity, SessionFileKind, SessionFileSnapshot, SessionRootLease,
        file::{
            DirectoryChain, ensure_single_link, file_identity, open_child,
            open_child_for_stable_read, open_child_for_update, snapshot_file, validate_child_name,
            validate_directory_chain,
        },
    },
};
#[cfg(unix)]
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

pub const MAX_DIAGNOSTIC_WRITE_CHUNK_BYTES: usize = 64 * 1024;
pub const MAX_DIAGNOSTIC_DELETE_TOMBSTONES: usize = 16_384;
pub const DIAGNOSTIC_EXPORT_TEMPORARY_PREFIX: &str = ".wokcore-diagnostics-export-";
const DIAGNOSTIC_DELETE_TOMBSTONE_PREFIX: &str = ".wokcore-diagnostic-delete-";
#[cfg(unix)]
const DIAGNOSTIC_DELETE_TOMBSTONE_MARKER_SUFFIX: &str = ".owner";
#[cfg(unix)]
const DIAGNOSTIC_DELETE_TOMBSTONE_MARKER_MAGIC: &[u8; 8] = b"WOKDTM01";
#[cfg(unix)]
const DIAGNOSTIC_DELETE_TOMBSTONE_MARKER_BYTES: usize = 24;
#[cfg(target_os = "macos")]
const DIAGNOSTIC_PARENT_LOCK_NAME: &str = ".wokcore-diagnostic-parent.lock";

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DiagnosticStoreError {
    #[error("diagnostic storage path is unsafe")]
    UnsafePath,
    #[error("diagnostic storage enumeration exceeds the limit")]
    EnumerationLimitExceeded,
    #[error("diagnostic storage size exceeds the limit")]
    SizeLimitExceeded,
    #[error("diagnostic cleanup tombstone budget is exhausted")]
    CleanupLimitExceeded,
    #[error("diagnostic storage object changed")]
    Changed,
    #[error("diagnostic storage object is unavailable")]
    Unavailable,
    #[error("diagnostic storage I/O failed")]
    Io,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DiagnosticIdentity(SessionFileIdentity);

#[derive(Clone, Eq, PartialEq)]
pub struct DiagnosticEntry {
    name: OsString,
    snapshot: SessionFileSnapshot,
}

impl DiagnosticEntry {
    pub fn name(&self) -> &OsStr {
        &self.name
    }

    pub const fn len(&self) -> u64 {
        self.snapshot.size
    }

    pub const fn is_empty(&self) -> bool {
        self.snapshot.size == 0
    }

    pub const fn identity(&self) -> DiagnosticIdentity {
        DiagnosticIdentity(self.snapshot.identity)
    }

    pub const fn modified(&self) -> Option<SystemTime> {
        self.snapshot.modified
    }
}

impl fmt::Debug for DiagnosticEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DiagnosticEntry([redacted])")
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct DiagnosticEntriesPage {
    entries: Vec<DiagnosticEntry>,
    next_after: Option<OsString>,
}

impl fmt::Debug for DiagnosticEntriesPage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiagnosticEntriesPage")
            .field("entry_count", &self.entries.len())
            .field("has_more", &self.next_after.is_some())
            .finish()
    }
}

impl DiagnosticEntriesPage {
    pub fn entries(&self) -> &[DiagnosticEntry] {
        &self.entries
    }

    pub fn into_entries(self) -> Vec<DiagnosticEntry> {
        self.entries
    }

    pub fn next_after(&self) -> Option<&OsStr> {
        self.next_after.as_deref()
    }
}

pub struct DiagnosticDirectory {
    root: SessionRootLease,
    #[cfg(unix)]
    next_delete_tombstone_slot: Arc<AtomicUsize>,
}

impl DiagnosticDirectory {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DiagnosticStoreError> {
        let path = path.as_ref();
        let preliminary_root = SessionRootLease::open(path).map_err(map_session_error)?;
        let chain = preliminary_root.clone_chain().map_err(map_session_error)?;
        let root = match verify_current_user_directory(chain.file()) {
            Ok(()) => {
                drop(chain);
                preliminary_root
            }
            Err(PlatformError::UnsafeRuntimePath) => {
                harden_current_user_directory(chain.file(), path).map_err(map_platform_error)?;
                drop(chain);
                drop(preliminary_root);
                let root = SessionRootLease::open(path).map_err(map_session_error)?;
                let chain = root.clone_chain().map_err(map_session_error)?;
                verify_current_user_directory(chain.file()).map_err(map_platform_error)?;
                root
            }
            Err(error) => return Err(map_platform_error(error)),
        };
        #[cfg(unix)]
        let next_delete_tombstone_slot = {
            let parent = root.clone_chain().map_err(map_session_error)?;
            Arc::new(AtomicUsize::new(initial_delete_tombstone_slot(&parent)?))
        };
        let directory = Self {
            root,
            #[cfg(unix)]
            next_delete_tombstone_slot,
        };
        directory.revalidate()?;
        Ok(directory)
    }

    pub fn revalidate(&self) -> Result<(), DiagnosticStoreError> {
        self.root
            .open_directory("")
            .map(|_| ())
            .map_err(map_session_error)
    }

    pub fn entries(
        &self,
        maximum_entries: usize,
    ) -> Result<Vec<DiagnosticEntry>, DiagnosticStoreError> {
        let scan_limit = maximum_entries
            .checked_add(1)
            .ok_or(DiagnosticStoreError::EnumerationLimitExceeded)?;
        let (mut entries, has_more) = self.scan_visible_entries(None, scan_limit)?;
        if has_more || entries.len() > maximum_entries {
            return Err(DiagnosticStoreError::EnumerationLimitExceeded);
        }
        entries.truncate(maximum_entries);
        Ok(entries)
    }

    pub fn entries_page(
        &self,
        after: Option<&OsStr>,
        maximum_entries: usize,
    ) -> Result<DiagnosticEntriesPage, DiagnosticStoreError> {
        if maximum_entries == 0 {
            return Err(DiagnosticStoreError::EnumerationLimitExceeded);
        }
        let scan_limit = maximum_entries
            .checked_add(1)
            .ok_or(DiagnosticStoreError::EnumerationLimitExceeded)?;
        let (mut entries, scanned_more) = self.scan_visible_entries(after, scan_limit)?;
        let has_more = scanned_more || entries.len() > maximum_entries;
        entries.truncate(maximum_entries);
        let next_after = has_more
            .then(|| entries.last().map(|entry| entry.name.clone()))
            .flatten();
        Ok(DiagnosticEntriesPage {
            entries,
            next_after,
        })
    }

    pub fn create_new(
        &self,
        name: &OsStr,
        contents: &[u8],
        maximum_size: u64,
    ) -> Result<DiagnosticFile, DiagnosticStoreError> {
        validate_diagnostic_name(name)?;
        if contents.len() as u64 > maximum_size {
            return Err(DiagnosticStoreError::SizeLimitExceeded);
        }
        self.revalidate()?;
        let parent = self.root.clone_chain().map_err(map_session_error)?;
        let mut writer = PinnedExportDestination::create_in_directory(parent, name)
            .map_err(map_session_error)?;
        writer
            .write_all(contents)
            .map_err(|_| DiagnosticStoreError::Io)?;
        writer
            .validate_frozen_regular_file(maximum_size)
            .map_err(map_session_error)?;
        let published = writer.commit_pinned().map_err(map_session_error)?;
        Ok(DiagnosticFile::from_published(
            published,
            maximum_size,
            #[cfg(unix)]
            Arc::clone(&self.next_delete_tombstone_slot),
        ))
    }

    pub fn create_staged(
        &self,
        name: &OsStr,
        maximum_size: u64,
    ) -> Result<DiagnosticStagedFile, DiagnosticStoreError> {
        validate_diagnostic_name(name)?;
        self.revalidate()?;
        let parent = self.root.clone_chain().map_err(map_session_error)?;
        let destination = PinnedExportDestination::create_in_directory(parent, name)
            .map_err(map_session_error)?;
        Ok(DiagnosticStagedFile {
            destination,
            maximum_size,
            written: 0,
            failed: false,
            #[cfg(unix)]
            next_delete_tombstone_slot: Arc::clone(&self.next_delete_tombstone_slot),
        })
    }

    pub fn open_read(
        &self,
        entry: &DiagnosticEntry,
        maximum_size: u64,
    ) -> Result<DiagnosticReadLease, DiagnosticStoreError> {
        let pinned = self.open_entry(entry, maximum_size, false)?;
        Ok(DiagnosticReadLease { pinned })
    }

    pub fn open_update(
        &self,
        entry: &DiagnosticEntry,
        maximum_size: u64,
    ) -> Result<DiagnosticFile, DiagnosticStoreError> {
        self.open_entry(entry, maximum_size, true)
            .map(|pinned| DiagnosticFile {
                pinned,
                #[cfg(unix)]
                next_delete_tombstone_slot: Arc::clone(&self.next_delete_tombstone_slot),
            })
    }

    pub fn open_name_read(
        &self,
        name: &OsStr,
        maximum_size: u64,
    ) -> Result<DiagnosticReadLease, DiagnosticStoreError> {
        self.open_name(name, maximum_size, false)
            .map(|pinned| DiagnosticReadLease { pinned })
    }

    pub fn open_name_update(
        &self,
        name: &OsStr,
        maximum_size: u64,
    ) -> Result<DiagnosticFile, DiagnosticStoreError> {
        self.open_name(name, maximum_size, true)
            .map(|pinned| DiagnosticFile {
                pinned,
                #[cfg(unix)]
                next_delete_tombstone_slot: Arc::clone(&self.next_delete_tombstone_slot),
            })
    }

    pub fn remove(&self, entry: &DiagnosticEntry) -> Result<(), DiagnosticStoreError> {
        self.open_update(entry, u64::MAX)?.remove()
    }

    fn open_name(
        &self,
        name: &OsStr,
        maximum_size: u64,
        update: bool,
    ) -> Result<PinnedDiagnostic, DiagnosticStoreError> {
        validate_diagnostic_name(name)?;
        let parent = self.root.clone_chain().map_err(map_session_error)?;
        open_pinned(parent, name.to_os_string(), None, maximum_size, update)
    }

    fn scan_visible_entries(
        &self,
        after: Option<&OsStr>,
        maximum_visible: usize,
    ) -> Result<(Vec<DiagnosticEntry>, bool), DiagnosticStoreError> {
        const RAW_PAGE_ENTRIES: usize = 256;

        let directory = self.root.open_directory("").map_err(map_session_error)?;
        let parent = self.root.clone_chain().map_err(map_session_error)?;
        let mut cursor = after.map(OsStr::to_os_string);
        let mut visible = Vec::with_capacity(maximum_visible.min(RAW_PAGE_ENTRIES));
        let mut tombstones = 0_usize;
        loop {
            let (raw, raw_has_more) = directory
                .entries_page(cursor.as_deref(), RAW_PAGE_ENTRIES)
                .map_err(map_session_error)?;
            if raw.is_empty() {
                return Ok((visible, false));
            }
            cursor = raw.last().map(|entry| entry.name().to_os_string());
            let raw_count = raw.len();
            for (index, entry) in raw.into_iter().enumerate() {
                if is_export_temporary(entry.name()) || is_parent_lock(entry.name()) {
                    continue;
                }
                if is_delete_tombstone(entry.name()) {
                    tombstones = tombstones
                        .checked_add(1)
                        .ok_or(DiagnosticStoreError::CleanupLimitExceeded)?;
                    if tombstones > MAX_DIAGNOSTIC_DELETE_TOMBSTONES * 2 {
                        return Err(DiagnosticStoreError::CleanupLimitExceeded);
                    }
                    continue;
                }
                visible.push(validated_diagnostic_entry(&parent, entry)?);
                if visible.len() == maximum_visible {
                    return Ok((visible, index + 1 < raw_count || raw_has_more));
                }
            }
            if !raw_has_more {
                return Ok((visible, false));
            }
        }
    }

    fn open_entry(
        &self,
        entry: &DiagnosticEntry,
        maximum_size: u64,
        update: bool,
    ) -> Result<PinnedDiagnostic, DiagnosticStoreError> {
        let parent = self.root.clone_chain().map_err(map_session_error)?;
        open_pinned(
            parent,
            entry.name.clone(),
            Some(&entry.snapshot),
            maximum_size,
            update,
        )
    }
}

fn validate_diagnostic_name(name: &OsStr) -> Result<(), DiagnosticStoreError> {
    validate_child_name(name).map_err(map_session_error)?;
    if is_delete_tombstone(name) || is_export_temporary(name) || is_parent_lock(name) {
        return Err(DiagnosticStoreError::UnsafePath);
    }
    Ok(())
}

fn is_export_temporary(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.starts_with(DIAGNOSTIC_EXPORT_TEMPORARY_PREFIX))
}

fn is_delete_tombstone(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.starts_with(DIAGNOSTIC_DELETE_TOMBSTONE_PREFIX))
}

#[cfg(target_os = "macos")]
fn is_parent_lock(name: &OsStr) -> bool {
    name == OsStr::new(DIAGNOSTIC_PARENT_LOCK_NAME)
}

#[cfg(not(target_os = "macos"))]
fn is_parent_lock(_name: &OsStr) -> bool {
    false
}

fn validated_diagnostic_entry(
    parent: &DirectoryChain,
    entry: SessionDirectoryEntry,
) -> Result<DiagnosticEntry, DiagnosticStoreError> {
    if entry.snapshot().kind != SessionFileKind::RegularFile {
        return Err(DiagnosticStoreError::UnsafePath);
    }
    let file = open_child(parent, entry.name(), false).map_err(map_session_error)?;
    ensure_single_link(&file).map_err(map_session_error)?;
    if snapshot_file(&file).map_err(map_session_error)? != *entry.snapshot() {
        return Err(DiagnosticStoreError::Changed);
    }
    validate_directory_chain(parent).map_err(map_session_error)?;
    Ok(DiagnosticEntry {
        name: entry.name().to_os_string(),
        snapshot: entry.snapshot().clone(),
    })
}

impl fmt::Debug for DiagnosticDirectory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DiagnosticDirectory([redacted])")
    }
}

pub struct DiagnosticStagedFile {
    destination: PinnedExportDestination,
    maximum_size: u64,
    written: u64,
    failed: bool,
    #[cfg(unix)]
    next_delete_tombstone_slot: Arc<AtomicUsize>,
}

impl DiagnosticStagedFile {
    pub const fn len(&self) -> u64 {
        self.written
    }

    pub const fn is_empty(&self) -> bool {
        self.written == 0
    }

    pub fn write_chunk(&mut self, bytes: &[u8]) -> Result<(), DiagnosticStoreError> {
        if self.failed {
            return Err(DiagnosticStoreError::Io);
        }
        if bytes.len() > MAX_DIAGNOSTIC_WRITE_CHUNK_BYTES {
            self.failed = true;
            return Err(DiagnosticStoreError::SizeLimitExceeded);
        }
        let next = self
            .written
            .checked_add(
                u64::try_from(bytes.len()).map_err(|_| DiagnosticStoreError::SizeLimitExceeded)?,
            )
            .ok_or(DiagnosticStoreError::SizeLimitExceeded);
        let next = match next {
            Ok(next) if next <= self.maximum_size => next,
            Ok(_) | Err(_) => {
                self.failed = true;
                return Err(DiagnosticStoreError::SizeLimitExceeded);
            }
        };
        if bytes.is_empty() {
            return Ok(());
        }
        if self.destination.write_all(bytes).is_err() {
            self.failed = true;
            return Err(DiagnosticStoreError::Io);
        }
        self.written = next;
        Ok(())
    }

    pub fn commit(self) -> Result<DiagnosticFile, DiagnosticStoreError> {
        let Self {
            mut destination,
            maximum_size,
            written,
            failed,
            #[cfg(unix)]
            next_delete_tombstone_slot,
        } = self;
        if failed {
            return Err(DiagnosticStoreError::Io);
        }
        if destination.len().map_err(map_session_error)? != written {
            return Err(DiagnosticStoreError::Changed);
        }
        destination
            .validate_frozen_regular_file(maximum_size)
            .map_err(map_session_error)?;
        let published = destination.commit_pinned().map_err(map_session_error)?;
        Ok(DiagnosticFile::from_published(
            published,
            maximum_size,
            #[cfg(unix)]
            next_delete_tombstone_slot,
        ))
    }
}

impl fmt::Debug for DiagnosticStagedFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiagnosticStagedFile")
            .field("written", &self.written)
            .field("failed", &self.failed)
            .finish()
    }
}

struct PinnedDiagnostic {
    file: File,
    parent: DirectoryChain,
    name: OsString,
    snapshot: SessionFileSnapshot,
    snapshot_requires_adoption: bool,
    maximum_size: u64,
}

impl PinnedDiagnostic {
    fn identity(&self) -> DiagnosticIdentity {
        DiagnosticIdentity(self.snapshot.identity)
    }

    fn len(&self) -> u64 {
        self.snapshot.size
    }

    fn read_range(
        &mut self,
        offset: u64,
        maximum_bytes: usize,
    ) -> Result<Vec<u8>, DiagnosticStoreError> {
        self.revalidate()?;
        if offset >= self.snapshot.size || maximum_bytes == 0 {
            return Ok(Vec::new());
        }
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|_| DiagnosticStoreError::Io)?;
        let maximum = u64::try_from(maximum_bytes)
            .unwrap_or(u64::MAX)
            .min(self.snapshot.size - offset);
        let mut bytes = Vec::with_capacity(usize::try_from(maximum).unwrap_or(maximum_bytes));
        Read::by_ref(&mut self.file)
            .take(maximum)
            .read_to_end(&mut bytes)
            .map_err(|_| DiagnosticStoreError::Io)?;
        self.revalidate()?;
        Ok(bytes)
    }

    fn revalidate(&mut self) -> Result<(), DiagnosticStoreError> {
        self.adopt_published_snapshot()?;
        validate_directory_chain(&self.parent).map_err(map_session_error)?;
        ensure_single_link(&self.file).map_err(map_session_error)?;
        if snapshot_file(&self.file).map_err(map_session_error)? != self.snapshot {
            return Err(DiagnosticStoreError::Changed);
        }
        let current = open_child(&self.parent, &self.name, false).map_err(map_session_error)?;
        ensure_single_link(&current).map_err(map_session_error)?;
        if snapshot_file(&current).map_err(map_session_error)? != self.snapshot {
            return Err(DiagnosticStoreError::Changed);
        }
        validate_directory_chain(&self.parent).map_err(map_session_error)
    }

    fn adopt_published_snapshot(&mut self) -> Result<(), DiagnosticStoreError> {
        if !self.snapshot_requires_adoption {
            return Ok(());
        }
        validate_directory_chain(&self.parent).map_err(map_session_error)?;
        ensure_single_link(&self.file).map_err(map_session_error)?;
        let candidate = snapshot_file(&self.file).map_err(map_session_error)?;
        if candidate.identity != self.snapshot.identity
            || candidate.size != self.snapshot.size
            || candidate.kind != self.snapshot.kind
        {
            return Err(DiagnosticStoreError::Changed);
        }
        let current = open_child(&self.parent, &self.name, false).map_err(map_session_error)?;
        ensure_single_link(&current).map_err(map_session_error)?;
        if snapshot_file(&current).map_err(map_session_error)? != candidate {
            return Err(DiagnosticStoreError::Changed);
        }
        validate_directory_chain(&self.parent).map_err(map_session_error)?;
        self.snapshot = candidate;
        self.snapshot_requires_adoption = false;
        Ok(())
    }

    fn refresh_snapshot(&mut self) -> Result<(), DiagnosticStoreError> {
        self.snapshot = snapshot_file(&self.file).map_err(map_session_error)?;
        self.snapshot_requires_adoption = false;
        self.revalidate()
    }
}

pub struct DiagnosticReadLease {
    pinned: PinnedDiagnostic,
}

impl DiagnosticReadLease {
    pub fn name(&self) -> &OsStr {
        &self.pinned.name
    }

    pub fn identity(&self) -> DiagnosticIdentity {
        self.pinned.identity()
    }

    pub fn len(&self) -> u64 {
        self.pinned.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pinned.len() == 0
    }

    pub fn read_range(
        &mut self,
        offset: u64,
        maximum_bytes: usize,
    ) -> Result<Vec<u8>, DiagnosticStoreError> {
        self.pinned.read_range(offset, maximum_bytes)
    }

    /// Returns heap bytes owned by this lease, excluding its inline struct storage.
    pub fn resident_allocation_bytes(&self) -> Result<usize, DiagnosticStoreError> {
        self.pinned
            .name
            .capacity()
            .checked_add(
                self.pinned
                    .parent
                    .resident_allocation_bytes()
                    .ok_or(DiagnosticStoreError::SizeLimitExceeded)?,
            )
            .ok_or(DiagnosticStoreError::SizeLimitExceeded)
    }
}

impl fmt::Debug for DiagnosticReadLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DiagnosticReadLease([redacted])")
    }
}

pub struct DiagnosticFile {
    pinned: PinnedDiagnostic,
    #[cfg(unix)]
    next_delete_tombstone_slot: Arc<AtomicUsize>,
}

impl DiagnosticFile {
    fn from_published(
        published: PinnedPublishedFile,
        maximum_size: u64,
        #[cfg(unix)] next_delete_tombstone_slot: Arc<AtomicUsize>,
    ) -> Self {
        let pinned = PinnedDiagnostic {
            file: published.file,
            parent: published.parent,
            name: published.name,
            snapshot: published.snapshot,
            snapshot_requires_adoption: published.snapshot_requires_adoption,
            maximum_size,
        };
        Self {
            pinned,
            #[cfg(unix)]
            next_delete_tombstone_slot,
        }
    }

    pub fn name(&self) -> &OsStr {
        &self.pinned.name
    }

    pub fn identity(&self) -> DiagnosticIdentity {
        self.pinned.identity()
    }

    pub fn len(&self) -> u64 {
        self.pinned.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pinned.len() == 0
    }

    pub fn read_range(
        &mut self,
        offset: u64,
        maximum_bytes: usize,
    ) -> Result<Vec<u8>, DiagnosticStoreError> {
        self.pinned.read_range(offset, maximum_bytes)
    }

    pub fn append(&mut self, bytes: &[u8]) -> Result<(), DiagnosticStoreError> {
        let next_size = self
            .pinned
            .snapshot
            .size
            .checked_add(
                u64::try_from(bytes.len()).map_err(|_| DiagnosticStoreError::SizeLimitExceeded)?,
            )
            .ok_or(DiagnosticStoreError::SizeLimitExceeded)?;
        if next_size > self.pinned.maximum_size {
            return Err(DiagnosticStoreError::SizeLimitExceeded);
        }
        self.pinned.revalidate()?;
        self.pinned
            .file
            .seek(SeekFrom::End(0))
            .map_err(|_| DiagnosticStoreError::Io)?;
        self.pinned
            .file
            .write_all(bytes)
            .map_err(|_| DiagnosticStoreError::Io)?;
        self.pinned
            .file
            .sync_data()
            .map_err(|_| DiagnosticStoreError::Io)?;
        self.pinned.refresh_snapshot()
    }

    pub fn truncate(&mut self, length: u64) -> Result<(), DiagnosticStoreError> {
        if length > self.pinned.snapshot.size || length > self.pinned.maximum_size {
            return Err(DiagnosticStoreError::SizeLimitExceeded);
        }
        self.pinned.revalidate()?;
        self.pinned
            .file
            .set_len(length)
            .map_err(|_| DiagnosticStoreError::Io)?;
        self.pinned
            .file
            .sync_data()
            .map_err(|_| DiagnosticStoreError::Io)?;
        self.pinned.refresh_snapshot()
    }

    pub fn sync(&mut self) -> Result<(), DiagnosticStoreError> {
        self.pinned.revalidate()?;
        self.pinned
            .file
            .sync_data()
            .map_err(|_| DiagnosticStoreError::Io)?;
        self.pinned.revalidate()
    }

    pub fn remove(mut self) -> Result<(), DiagnosticStoreError> {
        self.pinned.revalidate()?;
        remove_open_file(
            &self.pinned.parent,
            &self.pinned.name,
            &self.pinned.file,
            self.pinned.snapshot.identity,
            #[cfg(unix)]
            &self.next_delete_tombstone_slot,
        )
        .map_err(map_session_error)
    }
}

impl fmt::Debug for DiagnosticFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DiagnosticFile([redacted])")
    }
}

fn open_pinned(
    parent: DirectoryChain,
    name: OsString,
    expected: Option<&SessionFileSnapshot>,
    maximum_size: u64,
    update: bool,
) -> Result<PinnedDiagnostic, DiagnosticStoreError> {
    validate_directory_chain(&parent).map_err(map_session_error)?;
    let file = if update {
        open_child_for_update(&parent, &name)
    } else {
        open_child_for_stable_read(&parent, &name)
    }
    .map_err(map_session_error)?;
    let snapshot = snapshot_file(&file).map_err(map_session_error)?;
    if snapshot.kind != SessionFileKind::RegularFile
        || snapshot.size > maximum_size
        || expected.is_some_and(|expected| expected != &snapshot)
    {
        return Err(DiagnosticStoreError::UnsafePath);
    }
    ensure_single_link(&file).map_err(map_session_error)?;
    let current = open_child(&parent, &name, false).map_err(map_session_error)?;
    ensure_single_link(&current).map_err(map_session_error)?;
    if file_identity(&current).map_err(map_session_error)? != snapshot.identity {
        return Err(DiagnosticStoreError::Changed);
    }
    validate_directory_chain(&parent).map_err(map_session_error)?;
    Ok(PinnedDiagnostic {
        file,
        parent,
        name,
        snapshot,
        snapshot_requires_adoption: false,
        maximum_size,
    })
}

fn map_platform_error(error: PlatformError) -> DiagnosticStoreError {
    match error {
        PlatformError::Io { .. } => DiagnosticStoreError::Io,
        _ => DiagnosticStoreError::UnsafePath,
    }
}

fn map_session_error(error: SessionError) -> DiagnosticStoreError {
    match error {
        SessionError::EnumerationLimitExceeded => DiagnosticStoreError::EnumerationLimitExceeded,
        SessionError::CleanupLimitExceeded => DiagnosticStoreError::CleanupLimitExceeded,
        SessionError::ReadLimitExceeded => DiagnosticStoreError::SizeLimitExceeded,
        SessionError::SessionFileChanged => DiagnosticStoreError::Changed,
        SessionError::SessionFileUnavailable => DiagnosticStoreError::Unavailable,
        SessionError::Io { .. } => DiagnosticStoreError::Io,
        SessionError::MissingPlatformData { .. } | SessionError::UnsafePath => {
            DiagnosticStoreError::UnsafePath
        }
    }
}

#[cfg(unix)]
fn remove_open_file(
    parent: &DirectoryChain,
    name: &OsStr,
    file: &File,
    expected: SessionFileIdentity,
    next_delete_tombstone_slot: &AtomicUsize,
) -> Result<(), SessionError> {
    use std::os::fd::AsRawFd;

    validate_directory_chain(parent)?;
    if file_identity(file)? != expected {
        return Err(SessionError::UnsafePath);
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(SessionError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    let quarantine = reserve_delete_tombstone(
        parent,
        name,
        expected,
        MAX_DIAGNOSTIC_DELETE_TOMBSTONES,
        next_delete_tombstone_slot,
    )?;
    #[cfg(test)]
    delete_synchronization_tests::hit(DeleteSynchronizationPoint::QuarantineRename);
    let quarantined = open_child_for_update(parent, &quarantine.name)?;
    let initially_matches = file_identity(&quarantined)? == expected;
    #[cfg(test)]
    delete_synchronization_tests::hit(DeleteSynchronizationPoint::QuarantineIdentityBeforeUnlink);
    validate_directory_chain(parent)?;
    file.set_len(0)?;
    file.sync_all()?;
    if file_identity(file)? != expected || file.metadata()?.len() != 0 {
        return Err(SessionError::UnsafePath);
    }
    if !initially_matches
        || !reclaim_delete_tombstone_slot(
            parent,
            quarantine.slot,
            Some(expected),
            Some(&quarantine.marker),
        )?
    {
        return Err(SessionError::UnsafePath);
    }
    Ok(())
}

#[cfg(unix)]
struct DeleteTombstoneReservation {
    slot: usize,
    name: OsString,
    marker: File,
}

#[cfg(unix)]
fn reserve_delete_tombstone(
    parent: &DirectoryChain,
    source: &OsStr,
    expected: SessionFileIdentity,
    maximum_tombstones: usize,
    next_delete_tombstone_slot: &AtomicUsize,
) -> Result<DeleteTombstoneReservation, SessionError> {
    let Some(parent_lock) = try_lock_diagnostic_parent(parent)? else {
        return Err(SessionError::CleanupLimitExceeded);
    };
    reserve_delete_tombstone_locked(
        parent,
        source,
        expected,
        maximum_tombstones,
        next_delete_tombstone_slot,
        &parent_lock,
    )
}

#[cfg(unix)]
fn reserve_delete_tombstone_locked(
    parent: &DirectoryChain,
    source: &OsStr,
    expected: SessionFileIdentity,
    maximum_tombstones: usize,
    next_delete_tombstone_slot: &AtomicUsize,
    parent_lock: &DiagnosticParentLock,
) -> Result<DeleteTombstoneReservation, SessionError> {
    for _ in 0..maximum_tombstones {
        let slot = claim_delete_tombstone_slot(next_delete_tombstone_slot, maximum_tombstones)?;
        if !reclaim_delete_tombstone_slot_locked(parent, slot, None, None, parent_lock)? {
            continue;
        }
        let marker_name = delete_tombstone_marker_name(slot);
        let Some(marker) =
            create_delete_tombstone_marker_locked(parent, &marker_name, expected, parent_lock)?
        else {
            continue;
        };
        let tombstone = delete_tombstone_name(slot);
        let current = match open_child_for_update(parent, source) {
            Ok(current) => current,
            Err(error) => {
                discard_delete_tombstone_marker(parent, &marker_name, &marker)?;
                return Err(error);
            }
        };
        if file_identity(&current)? != expected {
            discard_delete_tombstone_marker(parent, &marker_name, &marker)?;
            return Err(SessionError::UnsafePath);
        }
        match rename_relative_noreplace(parent, source, &tombstone) {
            Ok(()) => {
                let current = open_child_for_update(parent, &tombstone)?;
                if file_identity(&current)? != expected {
                    return Err(SessionError::UnsafePath);
                }
                validate_directory_chain(parent)?;
                return Ok(DeleteTombstoneReservation {
                    slot,
                    name: tombstone,
                    marker,
                });
            }
            Err(error) => {
                let tombstone_exists = tombstone_child_exists(parent, &tombstone)?;
                discard_delete_tombstone_marker(parent, &marker_name, &marker)?;
                if !tombstone_exists {
                    return Err(error);
                }
            }
        }
    }
    Err(SessionError::CleanupLimitExceeded)
}

#[cfg(unix)]
fn claim_delete_tombstone_slot(
    next_delete_tombstone_slot: &AtomicUsize,
    maximum_tombstones: usize,
) -> Result<usize, SessionError> {
    if maximum_tombstones == 0 {
        return Err(SessionError::CleanupLimitExceeded);
    }
    let mut slot = next_delete_tombstone_slot.load(Ordering::Relaxed);
    loop {
        let claimed = if slot >= maximum_tombstones { 0 } else { slot };
        let next = if claimed + 1 == maximum_tombstones {
            0
        } else {
            claimed + 1
        };
        match next_delete_tombstone_slot.compare_exchange_weak(
            slot,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Ok(claimed),
            Err(current) => slot = current,
        }
    }
}

#[cfg(unix)]
fn initial_delete_tombstone_slot(parent: &DirectoryChain) -> Result<usize, DiagnosticStoreError> {
    initial_delete_tombstone_slot_with_limit(parent, MAX_DIAGNOSTIC_DELETE_TOMBSTONES)
        .map_err(map_session_error)
}

#[cfg(unix)]
fn initial_delete_tombstone_slot_with_limit(
    parent: &DirectoryChain,
    maximum_tombstones: usize,
) -> Result<usize, SessionError> {
    let Some(parent_lock) = try_lock_diagnostic_parent(parent)? else {
        return Ok(maximum_tombstones);
    };
    validate_directory_chain(parent)?;
    let mut first_available = None;
    for slot in 0..maximum_tombstones {
        let tombstone = delete_tombstone_name(slot);
        let marker = delete_tombstone_marker_name(slot);
        let occupied =
            tombstone_child_exists(parent, &tombstone)? || tombstone_child_exists(parent, &marker)?;
        if !occupied {
            validate_directory_chain(parent)?;
            return Ok(first_available.unwrap_or(slot));
        }
        if reclaim_delete_tombstone_slot_locked(parent, slot, None, None, &parent_lock)? {
            first_available.get_or_insert(slot);
        }
    }
    validate_directory_chain(parent)?;
    Ok(first_available.unwrap_or(maximum_tombstones))
}

#[cfg(unix)]
fn delete_tombstone_name(slot: usize) -> OsString {
    OsString::from(format!("{DIAGNOSTIC_DELETE_TOMBSTONE_PREFIX}{slot:05}"))
}

#[cfg(unix)]
fn delete_tombstone_marker_name(slot: usize) -> OsString {
    OsString::from(format!(
        "{DIAGNOSTIC_DELETE_TOMBSTONE_PREFIX}{slot:05}{DIAGNOSTIC_DELETE_TOMBSTONE_MARKER_SUFFIX}"
    ))
}

#[cfg(unix)]
fn encode_delete_tombstone_marker(identity: SessionFileIdentity) -> [u8; 24] {
    let SessionFileIdentity::Unix { device, inode } = identity;
    let mut bytes = [0_u8; DIAGNOSTIC_DELETE_TOMBSTONE_MARKER_BYTES];
    bytes[..8].copy_from_slice(DIAGNOSTIC_DELETE_TOMBSTONE_MARKER_MAGIC);
    bytes[8..16].copy_from_slice(&device.to_le_bytes());
    bytes[16..24].copy_from_slice(&inode.to_le_bytes());
    bytes
}

#[cfg(unix)]
fn decode_delete_tombstone_marker(
    marker: &File,
) -> Result<Option<SessionFileIdentity>, SessionError> {
    if !strict_internal_file(
        marker,
        InternalFileSize::Exact(DIAGNOSTIC_DELETE_TOMBSTONE_MARKER_BYTES as u64),
    )? {
        return Ok(None);
    }
    let mut reader = marker.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    let mut bytes = [0_u8; DIAGNOSTIC_DELETE_TOMBSTONE_MARKER_BYTES];
    reader.read_exact(&mut bytes)?;
    if &bytes[..8] != DIAGNOSTIC_DELETE_TOMBSTONE_MARKER_MAGIC {
        return Ok(None);
    }
    let mut device = [0_u8; 8];
    device.copy_from_slice(&bytes[8..16]);
    let mut inode = [0_u8; 8];
    inode.copy_from_slice(&bytes[16..24]);
    Ok(Some(SessionFileIdentity::Unix {
        device: u64::from_le_bytes(device),
        inode: u64::from_le_bytes(inode),
    }))
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum InternalFileSize {
    Any,
    Exact(u64),
    AtMost(u64),
}

#[cfg(unix)]
struct DiagnosticParentLock {
    file: File,
}

#[cfg(unix)]
impl Drop for DiagnosticParentLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;

        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn try_lock_diagnostic_parent(
    parent: &DirectoryChain,
) -> Result<Option<DiagnosticParentLock>, SessionError> {
    use std::os::fd::{AsRawFd, FromRawFd};

    validate_directory_chain(parent)?;
    let descriptor = unsafe {
        libc::openat(
            parent.file().as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDONLY
                | libc::O_DIRECTORY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        return Err(SessionError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    let directory = unsafe { File::from_raw_fd(descriptor) };
    if file_identity(&directory)? != file_identity(parent.file())? {
        return Err(SessionError::UnsafePath);
    }
    if unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let source = std::io::Error::last_os_error();
        return if source
            .raw_os_error()
            .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
        {
            Ok(None)
        } else {
            Err(SessionError::Io { source })
        };
    }
    validate_directory_chain(parent)?;
    Ok(Some(DiagnosticParentLock { file: directory }))
}

#[cfg(target_os = "macos")]
fn try_lock_diagnostic_parent(
    parent: &DirectoryChain,
) -> Result<Option<DiagnosticParentLock>, SessionError> {
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::PermissionsExt,
    };

    validate_directory_chain(parent)?;
    let descriptor = unsafe {
        libc::openat(
            parent.file().as_raw_fd(),
            c".wokcore-diagnostic-parent.lock".as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if descriptor < 0 {
        return Err(SessionError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    if !safe_internal_file(&file, InternalFileSize::Exact(0), false)? {
        return Err(SessionError::UnsafePath);
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    if !strict_internal_file(&file, InternalFileSize::Exact(0))? {
        return Err(SessionError::UnsafePath);
    }
    let expected = file_identity(&file)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let source = std::io::Error::last_os_error();
        return if source
            .raw_os_error()
            .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
        {
            Ok(None)
        } else {
            Err(SessionError::Io { source })
        };
    }
    validate_directory_chain(parent)?;
    let current = open_child_for_update(parent, OsStr::new(DIAGNOSTIC_PARENT_LOCK_NAME))?;
    if !strict_internal_file(&current, InternalFileSize::Exact(0))?
        || file_identity(&current)? != expected
    {
        return Err(SessionError::UnsafePath);
    }
    validate_directory_chain(parent)?;
    Ok(Some(DiagnosticParentLock { file }))
}

#[cfg(unix)]
fn safe_internal_file(
    file: &File,
    size: InternalFileSize,
    require_reserved_mode: bool,
) -> Result<bool, SessionError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    Ok(metadata.file_type().is_file()
        && (!require_reserved_mode || metadata.mode() & 0o7777 == 0o600)
        && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.nlink() == 1
        && match size {
            InternalFileSize::Any => true,
            InternalFileSize::Exact(expected) => metadata.len() == expected,
            InternalFileSize::AtMost(maximum) => metadata.len() <= maximum,
        })
}

#[cfg(unix)]
fn strict_internal_file(file: &File, size: InternalFileSize) -> Result<bool, SessionError> {
    safe_internal_file(file, size, true)
}

#[cfg(unix)]
fn is_bounded_delete_tombstone_marker_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(slot) = name
        .strip_prefix(DIAGNOSTIC_DELETE_TOMBSTONE_PREFIX)
        .and_then(|name| name.strip_suffix(DIAGNOSTIC_DELETE_TOMBSTONE_MARKER_SUFFIX))
    else {
        return false;
    };
    slot.len() == 5
        && slot.bytes().all(|byte| byte.is_ascii_digit())
        && slot
            .parse::<usize>()
            .is_ok_and(|slot| slot < MAX_DIAGNOSTIC_DELETE_TOMBSTONES)
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum ReservedMarkerState {
    Partial,
    Complete,
}

#[cfg(unix)]
fn reserved_marker_identity(
    parent: &DirectoryChain,
    name: &OsStr,
    state: ReservedMarkerState,
) -> Result<Option<SessionFileIdentity>, SessionError> {
    use std::{
        mem::MaybeUninit,
        os::{fd::AsRawFd, unix::ffi::OsStrExt},
    };

    if !is_bounded_delete_tombstone_marker_name(name) {
        return Ok(None);
    }
    let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| SessionError::UnsafePath)?;
    let mut metadata = MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.file().as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let source = std::io::Error::last_os_error();
        return if source.kind() == std::io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(SessionError::Io { source })
        };
    }
    let metadata = unsafe { metadata.assume_init() };
    let permissions = metadata.st_mode & 0o7777;
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG
        || metadata.st_uid != unsafe { libc::geteuid() }
        || metadata.st_nlink != 1
        || metadata.st_size < 0
        || match state {
            ReservedMarkerState::Partial => {
                permissions & !0o600 != 0
                    || metadata.st_size as u64 > DIAGNOSTIC_DELETE_TOMBSTONE_MARKER_BYTES as u64
            }
            ReservedMarkerState::Complete => {
                permissions != 0o600
                    || metadata.st_size as u64 != DIAGNOSTIC_DELETE_TOMBSTONE_MARKER_BYTES as u64
            }
        }
    {
        return Ok(None);
    }
    // libc exposes different native dev_t and ino_t widths across supported Unix targets.
    #[allow(clippy::unnecessary_cast)]
    let identity = SessionFileIdentity::Unix {
        device: metadata.st_dev as u64,
        inode: metadata.st_ino as u64,
    };
    Ok(Some(identity))
}

#[cfg(unix)]
fn unlink_reserved_marker_only(
    parent: &DirectoryChain,
    name: &OsStr,
    expected: Option<SessionFileIdentity>,
    _parent_lock: &DiagnosticParentLock,
) -> Result<bool, SessionError> {
    use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};

    let Some(observed) = reserved_marker_identity(parent, name, ReservedMarkerState::Partial)?
    else {
        return Ok(false);
    };
    if expected.is_some_and(|expected| expected != observed) {
        return Ok(false);
    }
    validate_directory_chain(parent)?;
    if reserved_marker_identity(parent, name, ReservedMarkerState::Partial)? != Some(observed) {
        return Ok(false);
    }
    let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| SessionError::UnsafePath)?;
    if unsafe { libc::unlinkat(parent.file().as_raw_fd(), name.as_ptr(), 0) } != 0 {
        let source = std::io::Error::last_os_error();
        return if source.kind() == std::io::ErrorKind::NotFound {
            Ok(false)
        } else {
            Err(SessionError::Io { source })
        };
    }
    Ok(true)
}

#[cfg(unix)]
struct DeleteTombstoneMarkerCreationGuard<'a> {
    parent: &'a DirectoryChain,
    parent_lock: &'a DiagnosticParentLock,
    name: OsString,
    marker: Option<File>,
}

#[cfg(unix)]
impl DeleteTombstoneMarkerCreationGuard<'_> {
    fn marker(&self) -> &File {
        self.marker
            .as_ref()
            .expect("a marker creation guard owns its marker")
    }

    fn marker_mut(&mut self) -> &mut File {
        self.marker
            .as_mut()
            .expect("a marker creation guard owns its marker")
    }

    fn disarm(mut self) -> File {
        self.marker
            .take()
            .expect("a marker creation guard owns its marker")
    }
}

#[cfg(unix)]
impl Drop for DeleteTombstoneMarkerCreationGuard<'_> {
    fn drop(&mut self) {
        let Some(marker) = self.marker.as_ref() else {
            return;
        };
        let Ok(expected) = file_identity(marker) else {
            return;
        };
        if unlink_reserved_marker_only(self.parent, &self.name, Some(expected), self.parent_lock)
            .unwrap_or(false)
        {
            let _ = self.parent.file().sync_all();
            let _ = validate_directory_chain(self.parent);
        }
    }
}

#[cfg(all(unix, test))]
fn create_delete_tombstone_marker(
    parent: &DirectoryChain,
    name: &OsStr,
    expected: SessionFileIdentity,
) -> Result<Option<File>, SessionError> {
    let Some(parent_lock) = try_lock_diagnostic_parent(parent)? else {
        return Ok(None);
    };
    create_delete_tombstone_marker_locked(parent, name, expected, &parent_lock)
}

#[cfg(unix)]
fn create_delete_tombstone_marker_locked(
    parent: &DirectoryChain,
    name: &OsStr,
    expected: SessionFileIdentity,
    parent_lock: &DiagnosticParentLock,
) -> Result<Option<File>, SessionError> {
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::PermissionsExt},
    };

    validate_directory_chain(parent)?;
    let marker_name = name.to_os_string();
    let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| SessionError::UnsafePath)?;
    let descriptor = unsafe {
        libc::openat(
            parent.file().as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if descriptor < 0 {
        let source = std::io::Error::last_os_error();
        return match source.raw_os_error() {
            Some(libc::EEXIST | libc::ELOOP | libc::EISDIR | libc::ENOTDIR) => Ok(None),
            _ => Err(SessionError::Io { source }),
        };
    }
    let marker = unsafe { File::from_raw_fd(descriptor) };
    let mut marker = DeleteTombstoneMarkerCreationGuard {
        parent,
        parent_lock,
        name: marker_name.clone(),
        marker: Some(marker),
    };
    #[cfg(test)]
    delete_synchronization_tests::hit(DeleteSynchronizationPoint::MarkerCreatedBeforeHardening);
    marker
        .marker()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    if unsafe { libc::flock(marker.marker().as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(SessionError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    let contents = encode_delete_tombstone_marker(expected);
    #[cfg(test)]
    if let Some(written_bytes) = DELETE_MARKER_WRITE_LIMIT.with(std::cell::Cell::get) {
        marker
            .marker_mut()
            .write_all(&contents[..written_bytes.min(contents.len())])?;
        return Err(SessionError::Io {
            source: std::io::Error::other("injected marker creation failure"),
        });
    }
    marker.marker_mut().write_all(&contents)?;
    marker.marker().sync_all()?;
    if !strict_internal_file(
        marker.marker(),
        InternalFileSize::Exact(DIAGNOSTIC_DELETE_TOMBSTONE_MARKER_BYTES as u64),
    )? {
        return Err(SessionError::UnsafePath);
    }
    let created_identity = file_identity(marker.marker())?;
    validate_directory_chain(parent)?;
    parent.file().sync_all()?;
    let current = open_child_for_update(parent, &marker_name)?;
    if !strict_internal_file(
        &current,
        InternalFileSize::Exact(DIAGNOSTIC_DELETE_TOMBSTONE_MARKER_BYTES as u64),
    )? || file_identity(&current)? != created_identity
    {
        return Err(SessionError::UnsafePath);
    }
    validate_directory_chain(parent)?;
    Ok(Some(marker.disarm()))
}

#[cfg(unix)]
fn discard_delete_tombstone_marker(
    parent: &DirectoryChain,
    name: &OsStr,
    marker: &File,
) -> Result<(), SessionError> {
    let expected = file_identity(marker)?;
    if !unlink_verified_internal_child(
        parent,
        name,
        expected,
        InternalFileSize::Exact(DIAGNOSTIC_DELETE_TOMBSTONE_MARKER_BYTES as u64),
    )? {
        return Err(SessionError::UnsafePath);
    }
    parent.file().sync_all()?;
    validate_directory_chain(parent)
}

#[cfg(unix)]
fn reclaim_delete_tombstone_slot(
    parent: &DirectoryChain,
    slot: usize,
    expected: Option<SessionFileIdentity>,
    active_marker: Option<&File>,
) -> Result<bool, SessionError> {
    let Some(parent_lock) = try_lock_diagnostic_parent(parent)? else {
        return Ok(false);
    };
    reclaim_delete_tombstone_slot_locked(parent, slot, expected, active_marker, &parent_lock)
}

#[cfg(unix)]
fn reclaim_delete_tombstone_slot_locked(
    parent: &DirectoryChain,
    slot: usize,
    expected: Option<SessionFileIdentity>,
    active_marker: Option<&File>,
    parent_lock: &DiagnosticParentLock,
) -> Result<bool, SessionError> {
    use std::os::fd::AsRawFd;

    validate_directory_chain(parent)?;
    let tombstone_name = delete_tombstone_name(slot);
    let marker_name = delete_tombstone_marker_name(slot);
    let marker_exists = tombstone_child_exists(parent, &marker_name)?;
    let initially_has_tombstone = tombstone_child_exists(parent, &tombstone_name)?;
    #[cfg(test)]
    delete_synchronization_tests::hit(
        DeleteSynchronizationPoint::MarkerAndTombstoneProbeBeforeMarkerLock,
    );
    if !marker_exists {
        return Ok(!initially_has_tombstone);
    }
    if !initially_has_tombstone && !tombstone_child_exists(parent, &tombstone_name)? {
        let removed = unlink_reserved_marker_only(parent, &marker_name, None, parent_lock)?;
        if removed {
            parent.file().sync_all()?;
            validate_directory_chain(parent)?;
        }
        return Ok(removed);
    }
    let Some(probed_marker_identity) =
        reserved_marker_identity(parent, &marker_name, ReservedMarkerState::Complete)?
    else {
        return Ok(false);
    };
    let marker = match open_child_for_update(parent, &marker_name) {
        Ok(marker) => marker,
        Err(SessionError::UnsafePath) => return Ok(false),
        Err(SessionError::Io { source }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    let marker_file_identity = file_identity(&marker)?;
    if marker_file_identity != probed_marker_identity {
        return Ok(false);
    }
    if let Some(active_marker) = active_marker
        && file_identity(active_marker)? != marker_file_identity
    {
        return Ok(false);
    }
    if active_marker.is_none()
        && unsafe { libc::flock(marker.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0
    {
        return Ok(false);
    }
    if !strict_internal_file(
        &marker,
        InternalFileSize::AtMost(DIAGNOSTIC_DELETE_TOMBSTONE_MARKER_BYTES as u64),
    )? {
        return Ok(false);
    }
    let tombstone_exists = tombstone_child_exists(parent, &tombstone_name)?;
    if !tombstone_exists {
        let removed = unlink_reserved_marker_only(
            parent,
            &marker_name,
            Some(marker_file_identity),
            parent_lock,
        )?;
        if removed {
            parent.file().sync_all()?;
            validate_directory_chain(parent)?;
        }
        return Ok(removed);
    }
    let Some(documented_identity) = decode_delete_tombstone_marker(&marker)? else {
        return Ok(false);
    };
    if expected.is_some_and(|expected| expected != documented_identity) {
        return Ok(false);
    }
    let tombstone = match open_child_for_update(parent, &tombstone_name) {
        Ok(tombstone) => tombstone,
        Err(SessionError::UnsafePath) => return Ok(false),
        Err(SessionError::Io { source }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    if !strict_internal_file(&tombstone, InternalFileSize::Any)?
        || file_identity(&tombstone)? != documented_identity
    {
        return Ok(false);
    }
    if expected.is_none()
        && unsafe { libc::flock(tombstone.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0
    {
        return Ok(false);
    }
    if expected.is_some() {
        if tombstone.metadata()?.len() != 0 {
            return Ok(false);
        }
    } else {
        tombstone.set_len(0)?;
        tombstone.sync_all()?;
        if file_identity(&tombstone)? != documented_identity || tombstone.metadata()?.len() != 0 {
            return Err(SessionError::UnsafePath);
        }
    }
    if !unlink_verified_internal_child(
        parent,
        &tombstone_name,
        documented_identity,
        InternalFileSize::Exact(0),
    )? {
        return Ok(false);
    }
    if !unlink_verified_internal_child(
        parent,
        &marker_name,
        marker_file_identity,
        InternalFileSize::Exact(DIAGNOSTIC_DELETE_TOMBSTONE_MARKER_BYTES as u64),
    )? {
        parent.file().sync_all()?;
        return Ok(false);
    }
    parent.file().sync_all()?;
    validate_directory_chain(parent)?;
    Ok(true)
}

#[cfg(unix)]
fn unlink_verified_internal_child(
    parent: &DirectoryChain,
    name: &OsStr,
    expected: SessionFileIdentity,
    size: InternalFileSize,
) -> Result<bool, SessionError> {
    unlink_verified_internal_child_with_mode(parent, name, expected, size, true)
}

#[cfg(unix)]
fn unlink_verified_internal_child_with_mode(
    parent: &DirectoryChain,
    name: &OsStr,
    expected: SessionFileIdentity,
    size: InternalFileSize,
    require_reserved_mode: bool,
) -> Result<bool, SessionError> {
    use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};

    let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| SessionError::UnsafePath)?;
    let current = match open_child_for_update(parent, OsStr::from_bytes(name.as_bytes())) {
        Ok(current) => current,
        Err(SessionError::UnsafePath) => return Ok(false),
        Err(SessionError::Io { source }) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    if !safe_internal_file(&current, size, require_reserved_mode)? {
        return Ok(false);
    }
    validate_directory_chain(parent)?;
    let current_identity = file_identity(&current)?;
    if current_identity != expected {
        return Ok(false);
    }
    let result = unsafe { libc::unlinkat(parent.file().as_raw_fd(), name.as_ptr(), 0) };
    if result != 0 {
        return Err(SessionError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(true)
}

#[cfg(unix)]
fn tombstone_child_exists(parent: &DirectoryChain, name: &OsStr) -> Result<bool, SessionError> {
    use std::{
        mem::MaybeUninit,
        os::{fd::AsRawFd, unix::ffi::OsStrExt},
    };

    #[cfg(test)]
    TOMBSTONE_OCCUPANCY_CHECKS.with(|checks| checks.set(checks.get().saturating_add(1)));
    validate_child_name(name)?;
    validate_directory_chain(parent)?;
    let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| SessionError::UnsafePath)?;
    let mut metadata = MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.file().as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let source = std::io::Error::last_os_error();
        return if source.kind() == std::io::ErrorKind::NotFound {
            validate_directory_chain(parent)?;
            Ok(false)
        } else {
            Err(SessionError::Io { source })
        };
    }
    validate_directory_chain(parent)?;
    Ok(true)
}

#[cfg(all(test, unix))]
std::thread_local! {
    static TOMBSTONE_OCCUPANCY_CHECKS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
    static DELETE_MARKER_WRITE_LIMIT: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_relative_noreplace(
    parent: &DirectoryChain,
    source: &OsStr,
    destination: &OsStr,
) -> Result<(), SessionError> {
    use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};

    let source = std::ffi::CString::new(source.as_bytes()).map_err(|_| SessionError::UnsafePath)?;
    let destination =
        std::ffi::CString::new(destination.as_bytes()).map_err(|_| SessionError::UnsafePath)?;
    let result = unsafe {
        libc::renameat2(
            parent.file().as_raw_fd(),
            source.as_ptr(),
            parent.file().as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result != 0 {
        return Err(SessionError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

#[cfg(target_vendor = "apple")]
fn rename_relative_noreplace(
    parent: &DirectoryChain,
    source: &OsStr,
    destination: &OsStr,
) -> Result<(), SessionError> {
    use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};

    let source = std::ffi::CString::new(source.as_bytes()).map_err(|_| SessionError::UnsafePath)?;
    let destination =
        std::ffi::CString::new(destination.as_bytes()).map_err(|_| SessionError::UnsafePath)?;
    let result = unsafe {
        libc::renameatx_np(
            parent.file().as_raw_fd(),
            source.as_ptr(),
            parent.file().as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result != 0 {
        return Err(SessionError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

#[cfg(windows)]
fn remove_open_file(
    parent: &DirectoryChain,
    name: &OsStr,
    file: &File,
    expected: SessionFileIdentity,
) -> Result<(), SessionError> {
    use std::{mem::size_of, os::windows::io::AsRawHandle};

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
    };

    use crate::sessions::file::open_child_for_delete;

    validate_directory_chain(parent)?;
    if file_identity(file)? != expected {
        return Err(SessionError::UnsafePath);
    }
    let delete = open_child_for_delete(parent, name)?;
    if file_identity(&delete)? != expected {
        return Err(SessionError::UnsafePath);
    }
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    if unsafe {
        SetFileInformationByHandle(
            delete.as_raw_handle(),
            FileDispositionInfo,
            (&raw const disposition).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        return Err(SessionError::Io {
            source: std::io::Error::last_os_error(),
        });
    }
    validate_directory_chain(parent)
}

#[cfg(all(test, unix))]
#[derive(Clone, Copy, Eq, PartialEq)]
enum DeleteSynchronizationPoint {
    QuarantineRename,
    QuarantineIdentityBeforeUnlink,
    MarkerAndTombstoneProbeBeforeMarkerLock,
    MarkerCreatedBeforeHardening,
}

#[cfg(all(test, unix))]
mod delete_synchronization_tests {
    use std::{
        sync::{Arc, Barrier, Mutex},
        thread::ThreadId,
    };

    use super::DeleteSynchronizationPoint;

    struct Hook {
        thread: ThreadId,
        point: DeleteSynchronizationPoint,
        window: Arc<HookWindow>,
    }

    pub(super) struct HookWindow {
        reached: Barrier,
        resume: Barrier,
    }

    impl HookWindow {
        pub(super) fn new() -> Self {
            Self {
                reached: Barrier::new(2),
                resume: Barrier::new(2),
            }
        }

        pub(super) fn wait_until_reached(&self) {
            self.reached.wait();
        }

        pub(super) fn resume(&self) {
            self.resume.wait();
        }
    }

    static HOOK: Mutex<Option<Hook>> = Mutex::new(None);

    pub(super) fn install(
        thread: ThreadId,
        point: DeleteSynchronizationPoint,
        window: Arc<HookWindow>,
    ) {
        *HOOK.lock().expect("delete hook mutex is not poisoned") = Some(Hook {
            thread,
            point,
            window,
        });
    }

    pub(super) fn uninstall(thread: ThreadId) {
        let mut hook = HOOK.lock().expect("delete hook mutex is not poisoned");
        if hook.as_ref().is_some_and(|hook| hook.thread == thread) {
            *hook = None;
        }
    }

    pub(super) fn hit(point: DeleteSynchronizationPoint) {
        let window = HOOK
            .lock()
            .expect("delete hook mutex is not poisoned")
            .as_ref()
            .filter(|hook| hook.thread == std::thread::current().id() && hook.point == point)
            .map(|hook| Arc::clone(&hook.window));
        if let Some(window) = window {
            window.reached.wait();
            window.resume.wait();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        ffi::{OsStr, OsString},
        fs::{self, File, OpenOptions},
        io::Write,
        os::unix::fs::PermissionsExt,
        path::Path,
        sync::{
            Arc, Barrier,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        thread,
    };

    use tempfile::tempdir;

    use super::{
        DIAGNOSTIC_DELETE_TOMBSTONE_PREFIX, DeleteSynchronizationPoint, DiagnosticDirectory,
        SessionFileIdentity,
        delete_synchronization_tests::{self, HookWindow},
    };

    fn write_owned_crash_tombstone(
        root: &Path,
        slot: usize,
        contents: &[u8],
    ) -> SessionFileIdentity {
        let tombstone = root.join(format!("{DIAGNOSTIC_DELETE_TOMBSTONE_PREFIX}{slot:05}"));
        fs::write(&tombstone, contents).unwrap();
        fs::set_permissions(&tombstone, fs::Permissions::from_mode(0o600)).unwrap();
        let identity = super::file_identity(&File::open(&tombstone).unwrap()).unwrap();
        let marker = root.join(super::delete_tombstone_marker_name(slot));
        fs::write(marker, super::encode_delete_tombstone_marker(identity)).unwrap();
        fs::set_permissions(
            root.join(super::delete_tombstone_marker_name(slot)),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        identity
    }

    fn write_crash_marker(root: &Path, slot: usize, contents: &[u8], mode: u32) {
        let marker = root.join(super::delete_tombstone_marker_name(slot));
        fs::write(&marker, contents).unwrap();
        fs::set_permissions(marker, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn write_open_crash_marker(root: &Path, slot: usize, contents: &[u8], mode: u32) -> File {
        let marker = root.join(super::delete_tombstone_marker_name(slot));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(marker)
            .unwrap();
        file.write_all(contents).unwrap();
        file.set_permissions(fs::Permissions::from_mode(mode))
            .unwrap();
        file
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_diagnostic_directory_opens_and_publishes_on_private_system_temp_path() {
        let fixture = tempdir().unwrap();
        let root = fixture.path().join("diagnostics");
        fs::create_dir(&root).unwrap();
        let lease = super::SessionRootLease::open(&root).unwrap();
        let parent = lease.clone_chain().unwrap();

        let parent_lock = match super::try_lock_diagnostic_parent(&parent) {
            Ok(Some(parent_lock)) => parent_lock,
            Ok(None) => panic!("a new private directory must not have a contended parent lock"),
            Err(error) => panic!("macOS diagnostic parent lock failed: {error:?}"),
        };
        drop(parent_lock);

        let directory = DiagnosticDirectory::open(&root).unwrap_or_else(|error| {
            panic!("macOS diagnostic directory failed after parent lock verification: {error:?}")
        });
        directory
            .create_new(OsStr::new("segment-00000000000000000001.jsonl"), b"", 1024)
            .unwrap_or_else(|error| {
                panic!("macOS diagnostic segment publication failed: {error:?}")
            });
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_parent_lock_is_private_exclusive_and_hidden_from_diagnostic_entries() {
        use std::os::unix::fs::MetadataExt;

        let fixture = tempdir().unwrap();
        let root = fixture.path().join("diagnostics");
        fs::create_dir(&root).unwrap();
        let directory = DiagnosticDirectory::open(&root).unwrap();
        let parent = directory.root.clone_chain().unwrap();
        let first = super::try_lock_diagnostic_parent(&parent).unwrap().unwrap();

        assert!(
            super::try_lock_diagnostic_parent(&parent)
                .unwrap()
                .is_none()
        );
        assert!(directory.entries(0).unwrap().is_empty());
        let metadata = fs::metadata(root.join(super::DIAGNOSTIC_PARENT_LOCK_NAME)).unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0o600);
        assert_eq!(metadata.nlink(), 1);

        drop(first);
        assert!(
            super::try_lock_diagnostic_parent(&parent)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn reclaimer_reprobes_after_marker_lock_before_cleaning_a_marker_only_slot() {
        let fixture = tempdir().unwrap();
        let root = fixture.path().join("diagnostics");
        fs::create_dir(&root).unwrap();
        let directory = DiagnosticDirectory::open(&root).unwrap();
        let parent = directory.root.clone_chain().unwrap();
        let source = root.join("segment.jsonl");
        fs::write(&source, b"owned crash payload").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
        let expected = super::file_identity(&File::open(&source).unwrap()).unwrap();
        let marker_name = super::delete_tombstone_marker_name(0);
        let owner_marker = super::create_delete_tombstone_marker(&parent, &marker_name, expected)
            .unwrap()
            .unwrap();
        let tombstone = root.join(super::delete_tombstone_name(0));
        let marker = root.join(&marker_name);

        let start = Arc::new(Barrier::new(2));
        let worker_start = Arc::clone(&start);
        let (thread_tx, thread_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            thread_tx.send(thread::current().id()).unwrap();
            worker_start.wait();
            super::reclaim_delete_tombstone_slot(&parent, 0, None, None)
        });
        let worker_id = thread_rx.recv().unwrap();
        let window = Arc::new(HookWindow::new());
        delete_synchronization_tests::install(
            worker_id,
            DeleteSynchronizationPoint::MarkerAndTombstoneProbeBeforeMarkerLock,
            Arc::clone(&window),
        );
        start.wait();
        window.wait_until_reached();

        fs::rename(&source, &tombstone).unwrap();
        drop(owner_marker);
        window.resume();

        assert!(worker.join().unwrap().unwrap());
        delete_synchronization_tests::uninstall(worker_id);
        assert!(!tombstone.exists());
        assert!(!marker.exists());
    }

    #[test]
    fn restart_reclaims_bounded_incomplete_marker_only_crashes_indefinitely() {
        const TOMBSTONE_LIMIT: usize = 1;
        const INVALID_MAGIC: [u8; 24] = *b"INVALID!................";
        let residues: [&[u8]; 3] = [b"", b"WOKDTM01partial", &INVALID_MAGIC];

        let fixture = tempdir().unwrap();
        let root = fixture.path().join("diagnostics");
        fs::create_dir(&root).unwrap();
        let directory = DiagnosticDirectory::open(&root).unwrap();
        let parent = directory.root.clone_chain().unwrap();

        for iteration in 0..12 {
            let residue = residues[iteration % residues.len()];
            write_crash_marker(&root, 0, residue, 0o600);

            assert_eq!(
                super::initial_delete_tombstone_slot_with_limit(&parent, TOMBSTONE_LIMIT).unwrap(),
                0
            );
            assert!(!root.join(super::delete_tombstone_marker_name(0)).exists());
        }
    }

    #[test]
    fn restart_and_next_reservation_reclaim_umask_restricted_marker_only_slots() {
        const TOMBSTONE_LIMIT: usize = 1;

        for mode in [0o400, 0o000] {
            let fixture = tempdir().unwrap();
            let root = fixture.path().join("diagnostics");
            fs::create_dir(&root).unwrap();
            let directory = DiagnosticDirectory::open(&root).unwrap();
            let parent = directory.root.clone_chain().unwrap();
            let marker_name = super::delete_tombstone_marker_name(0);

            drop(write_open_crash_marker(&root, 0, b"", mode));
            assert_eq!(
                super::initial_delete_tombstone_slot_with_limit(&parent, TOMBSTONE_LIMIT).unwrap(),
                0
            );
            assert!(!root.join(&marker_name).exists());

            drop(write_open_crash_marker(&root, 0, b"", mode));
            let source_name = OsStr::new("segment.jsonl");
            let source = root.join(source_name);
            fs::write(&source, b"source remains canonical").unwrap();
            fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
            let expected = super::file_identity(&File::open(&source).unwrap()).unwrap();
            let next_slot = AtomicUsize::new(0);

            let reservation = super::reserve_delete_tombstone(
                &parent,
                source_name,
                expected,
                TOMBSTONE_LIMIT,
                &next_slot,
            )
            .unwrap();

            assert_eq!(reservation.slot, 0);
            assert_eq!(next_slot.load(Ordering::Relaxed), 0);
            assert!(
                !root
                    .join(&marker_name)
                    .metadata()
                    .unwrap()
                    .permissions()
                    .readonly()
            );
        }
    }

    #[test]
    fn marker_creation_guard_reclaims_umask_restricted_returned_errors() {
        for mode in [0o400, 0o000] {
            let fixture = tempdir().unwrap();
            let root = fixture.path().join("diagnostics");
            fs::create_dir(&root).unwrap();
            let directory = DiagnosticDirectory::open(&root).unwrap();
            let parent = directory.root.clone_chain().unwrap();
            let marker_name = super::delete_tombstone_marker_name(0);
            let marker = write_open_crash_marker(&root, 0, b"WOKDTM01partial", mode);
            let parent_lock = super::try_lock_diagnostic_parent(&parent).unwrap().unwrap();

            drop(super::DeleteTombstoneMarkerCreationGuard {
                parent: &parent,
                parent_lock: &parent_lock,
                name: marker_name.clone(),
                marker: Some(marker),
            });

            assert!(!root.join(marker_name).exists());
        }
    }

    #[test]
    fn marker_only_reclaimer_cannot_cross_a_live_creator_before_rename() {
        const TOMBSTONE_LIMIT: usize = 1;

        for mode in [0o400, 0o000] {
            let fixture = tempdir().unwrap();
            let root = fixture.path().join("diagnostics");
            fs::create_dir(&root).unwrap();
            let directory = DiagnosticDirectory::open(&root).unwrap();
            let parent = directory.root.clone_chain().unwrap();
            let source_name = OsString::from("segment.jsonl");
            let source = root.join(&source_name);
            fs::write(&source, b"source remains canonical").unwrap();
            fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
            let expected = super::file_identity(&File::open(&source).unwrap()).unwrap();
            let worker_parent = directory.root.clone_chain().unwrap();
            let worker_source = source_name.clone();
            let next_slot = Arc::new(AtomicUsize::new(0));
            let worker_next_slot = Arc::clone(&next_slot);
            let start = Arc::new(Barrier::new(2));
            let worker_start = Arc::clone(&start);
            let (thread_tx, thread_rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                thread_tx.send(thread::current().id()).unwrap();
                worker_start.wait();
                super::reserve_delete_tombstone(
                    &worker_parent,
                    &worker_source,
                    expected,
                    TOMBSTONE_LIMIT,
                    &worker_next_slot,
                )
            });
            let worker_id = thread_rx.recv().unwrap();
            let window = Arc::new(HookWindow::new());
            delete_synchronization_tests::install(
                worker_id,
                DeleteSynchronizationPoint::MarkerCreatedBeforeHardening,
                Arc::clone(&window),
            );
            start.wait();
            window.wait_until_reached();

            let marker = root.join(super::delete_tombstone_marker_name(0));
            fs::set_permissions(&marker, fs::Permissions::from_mode(mode)).unwrap();
            assert!(!super::reclaim_delete_tombstone_slot(&parent, 0, None, None).unwrap());

            window.resume();
            let reservation = worker.join().unwrap().unwrap();
            delete_synchronization_tests::uninstall(worker_id);
            assert_eq!(reservation.slot, 0);
            assert!(!source.exists());
            assert!(root.join(&reservation.name).exists());
            assert!(marker.exists());
        }
    }

    #[test]
    fn invalid_marker_with_a_tombstone_remains_fail_closed() {
        const TOMBSTONE_LIMIT: usize = 1;

        let fixture = tempdir().unwrap();
        let root = fixture.path().join("diagnostics");
        fs::create_dir(&root).unwrap();
        let directory = DiagnosticDirectory::open(&root).unwrap();
        let parent = directory.root.clone_chain().unwrap();
        let tombstone = root.join(super::delete_tombstone_name(0));
        fs::write(&tombstone, b"untrusted").unwrap();
        fs::set_permissions(&tombstone, fs::Permissions::from_mode(0o600)).unwrap();
        write_crash_marker(&root, 0, b"invalid marker", 0o600);

        assert_eq!(
            super::initial_delete_tombstone_slot_with_limit(&parent, TOMBSTONE_LIMIT).unwrap(),
            TOMBSTONE_LIMIT
        );
        assert_eq!(fs::read(tombstone).unwrap(), b"untrusted");
        assert_eq!(
            fs::read(root.join(super::delete_tombstone_marker_name(0))).unwrap(),
            b"invalid marker"
        );
    }

    #[test]
    fn permission_restricted_marker_with_a_tombstone_remains_fail_closed() {
        const TOMBSTONE_LIMIT: usize = 1;

        for mode in [0o400, 0o000] {
            let fixture = tempdir().unwrap();
            let root = fixture.path().join("diagnostics");
            fs::create_dir(&root).unwrap();
            let directory = DiagnosticDirectory::open(&root).unwrap();
            let parent = directory.root.clone_chain().unwrap();
            let tombstone = root.join(super::delete_tombstone_name(0));
            fs::write(&tombstone, b"untrusted").unwrap();
            fs::set_permissions(&tombstone, fs::Permissions::from_mode(0o600)).unwrap();
            let identity = super::file_identity(&File::open(&tombstone).unwrap()).unwrap();
            let marker_contents = super::encode_delete_tombstone_marker(identity);
            drop(write_open_crash_marker(&root, 0, &marker_contents, mode));

            assert_eq!(
                super::initial_delete_tombstone_slot_with_limit(&parent, TOMBSTONE_LIMIT).unwrap(),
                TOMBSTONE_LIMIT
            );
            assert_eq!(fs::read(&tombstone).unwrap(), b"untrusted");
            assert!(root.join(super::delete_tombstone_marker_name(0)).exists());
        }
    }

    #[test]
    fn unsafe_marker_only_residue_is_not_reclaimed() {
        const TOMBSTONE_LIMIT: usize = 4;

        let fixture = tempdir().unwrap();
        let root = fixture.path().join("diagnostics");
        fs::create_dir(&root).unwrap();
        let directory = DiagnosticDirectory::open(&root).unwrap();
        let parent = directory.root.clone_chain().unwrap();
        write_crash_marker(&root, 0, b"", 0o640);
        write_crash_marker(
            &root,
            1,
            &[b'x'; super::DIAGNOSTIC_DELETE_TOMBSTONE_MARKER_BYTES + 1],
            0o600,
        );
        write_crash_marker(&root, 2, b"", 0o600);
        fs::hard_link(
            root.join(super::delete_tombstone_marker_name(2)),
            root.join("marker-hardlink"),
        )
        .unwrap();
        fs::create_dir(root.join(super::delete_tombstone_marker_name(3))).unwrap();

        assert_eq!(
            super::initial_delete_tombstone_slot_with_limit(&parent, TOMBSTONE_LIMIT).unwrap(),
            TOMBSTONE_LIMIT
        );
        for slot in 0..TOMBSTONE_LIMIT {
            assert!(
                root.join(super::delete_tombstone_marker_name(slot))
                    .exists()
            );
        }
    }

    #[test]
    fn returned_marker_creation_errors_remove_zero_and_partial_markers() {
        let fixture = tempdir().unwrap();
        let root = fixture.path().join("diagnostics");
        fs::create_dir(&root).unwrap();
        let directory = DiagnosticDirectory::open(&root).unwrap();
        let parent = directory.root.clone_chain().unwrap();
        let source = root.join("segment.jsonl");
        fs::write(&source, b"source remains canonical").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
        let expected = super::file_identity(&File::open(&source).unwrap()).unwrap();
        let marker_name = super::delete_tombstone_marker_name(0);
        let marker = root.join(&marker_name);

        for written_bytes in [0, 7] {
            super::DELETE_MARKER_WRITE_LIMIT.with(|limit| limit.set(Some(written_bytes)));
            let result = super::create_delete_tombstone_marker(&parent, &marker_name, expected);
            super::DELETE_MARKER_WRITE_LIMIT.with(|limit| limit.set(None));

            assert!(result.is_err());
            assert!(!marker.exists());
        }
        assert_eq!(fs::read(source).unwrap(), b"source remains canonical");
    }

    #[test]
    fn quarantine_identity_race_restores_foreign_file_without_deleting_it() {
        let fixture = tempdir().unwrap();
        let root = fixture.path().join("diagnostics");
        fs::create_dir(&root).unwrap();
        let directory = DiagnosticDirectory::open(&root).unwrap();
        let file = directory
            .create_new(OsStr::new("segment.jsonl"), b"owned", 64)
            .unwrap();
        let start = Arc::new(Barrier::new(2));
        let worker_start = Arc::clone(&start);
        let (thread_tx, thread_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            thread_tx.send(thread::current().id()).unwrap();
            worker_start.wait();
            file.remove()
        });
        let worker_id = thread_rx.recv().unwrap();
        let window = Arc::new(HookWindow::new());
        delete_synchronization_tests::install(
            worker_id,
            DeleteSynchronizationPoint::QuarantineRename,
            Arc::clone(&window),
        );
        start.wait();
        window.wait_until_reached();

        let quarantine = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with(".wokcore-diagnostic-delete-") && !name.ends_with(".owner")
                })
            })
            .unwrap();
        let relocated = root.join("owned-relocated");
        fs::rename(&quarantine, &relocated).unwrap();
        fs::write(&quarantine, b"foreign").unwrap();
        fs::set_permissions(&quarantine, fs::Permissions::from_mode(0o600)).unwrap();
        window.resume();

        assert!(worker.join().unwrap().is_err());
        delete_synchronization_tests::uninstall(worker_id);
        assert!(!root.join("segment.jsonl").exists());
        assert_eq!(fs::read(quarantine).unwrap(), b"foreign");
        assert!(fs::read(relocated).unwrap().is_empty());
    }

    #[test]
    fn quarantine_replacement_after_identity_check_never_deletes_the_foreign_file() {
        let fixture = tempdir().unwrap();
        let root = fixture.path().join("diagnostics");
        fs::create_dir(&root).unwrap();
        let directory = DiagnosticDirectory::open(&root).unwrap();
        let file = directory
            .create_new(OsStr::new("segment.jsonl"), b"owned", 64)
            .unwrap();
        let start = Arc::new(Barrier::new(2));
        let worker_start = Arc::clone(&start);
        let (thread_tx, thread_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            thread_tx.send(thread::current().id()).unwrap();
            worker_start.wait();
            file.remove()
        });
        let worker_id = thread_rx.recv().unwrap();
        let window = Arc::new(HookWindow::new());
        delete_synchronization_tests::install(
            worker_id,
            DeleteSynchronizationPoint::QuarantineIdentityBeforeUnlink,
            Arc::clone(&window),
        );
        start.wait();
        window.wait_until_reached();

        let quarantine = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with(".wokcore-diagnostic-delete-") && !name.ends_with(".owner")
                })
            })
            .unwrap();
        let relocated = root.join("owned-relocated");
        fs::rename(&quarantine, &relocated).unwrap();
        fs::write(&quarantine, b"foreign").unwrap();
        fs::set_permissions(&quarantine, fs::Permissions::from_mode(0o600)).unwrap();
        window.resume();

        assert!(worker.join().unwrap().is_err());
        delete_synchronization_tests::uninstall(worker_id);
        assert_eq!(fs::read(&quarantine).unwrap(), b"foreign");
        assert!(fs::read(relocated).unwrap().is_empty());
    }

    #[test]
    fn tombstone_budget_exhaustion_preserves_the_original_file() {
        let fixture = tempdir().unwrap();
        let root = fixture.path().join("diagnostics");
        fs::create_dir(&root).unwrap();
        let directory = DiagnosticDirectory::open(&root).unwrap();
        directory
            .create_new(OsStr::new("segment.jsonl"), b"owned", 64)
            .unwrap();
        for slot in 0..4 {
            let tombstone = root.join(format!("{DIAGNOSTIC_DELETE_TOMBSTONE_PREFIX}{slot:05}"));
            fs::write(&tombstone, b"foreign").unwrap();
            fs::set_permissions(&tombstone, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let parent = directory.root.clone_chain().unwrap();
        let next_slot = std::sync::atomic::AtomicUsize::new(0);
        let expected =
            super::file_identity(&File::open(root.join("segment.jsonl")).unwrap()).unwrap();

        assert!(matches!(
            super::reserve_delete_tombstone(
                &parent,
                OsStr::new("segment.jsonl"),
                expected,
                4,
                &next_slot,
            ),
            Err(crate::sessions::SessionError::CleanupLimitExceeded)
        ));
        assert_eq!(fs::read(root.join("segment.jsonl")).unwrap(), b"owned");
    }

    #[test]
    fn reclaimed_tombstone_slots_are_reused_beyond_a_small_budget() {
        const TOMBSTONE_LIMIT: usize = 4;
        const DELETE_COUNT: usize = TOMBSTONE_LIMIT * 3;

        let fixture = tempdir().unwrap();
        let root = fixture.path().join("diagnostics");
        fs::create_dir(&root).unwrap();
        let directory = DiagnosticDirectory::open(&root).unwrap();
        let parent = directory.root.clone_chain().unwrap();
        let next_slot = std::sync::atomic::AtomicUsize::new(0);

        for index in 0..DELETE_COUNT {
            let source = format!("source-{index:02}");
            fs::write(root.join(&source), []).unwrap();
            fs::set_permissions(root.join(&source), fs::Permissions::from_mode(0o600)).unwrap();
            let expected = super::file_identity(&File::open(root.join(&source)).unwrap()).unwrap();
            let tombstone = super::reserve_delete_tombstone(
                &parent,
                OsStr::new(&source),
                expected,
                TOMBSTONE_LIMIT,
                &next_slot,
            )
            .unwrap();
            fs::remove_file(root.join(&tombstone.name)).unwrap();
            fs::remove_file(root.join(super::delete_tombstone_marker_name(tombstone.slot)))
                .unwrap();
        }
    }

    #[test]
    fn restart_reclaims_owned_crash_tombstones_within_a_small_budget() {
        const TOMBSTONE_LIMIT: usize = 4;

        let fixture = tempdir().unwrap();
        let root = fixture.path().join("diagnostics");
        fs::create_dir(&root).unwrap();
        let directory = DiagnosticDirectory::open(&root).unwrap();
        let parent = directory.root.clone_chain().unwrap();
        for slot in 0..TOMBSTONE_LIMIT {
            write_owned_crash_tombstone(&root, slot, b"owned crash payload");
        }

        let next_slot =
            super::initial_delete_tombstone_slot_with_limit(&parent, TOMBSTONE_LIMIT).unwrap();

        assert_eq!(next_slot, 0);
        for slot in 0..TOMBSTONE_LIMIT {
            assert!(
                !root
                    .join(format!("{DIAGNOSTIC_DELETE_TOMBSTONE_PREFIX}{slot:05}"))
                    .exists()
            );
            assert!(
                !root
                    .join(super::delete_tombstone_marker_name(slot))
                    .exists()
            );
        }
        fs::write(root.join("segment.jsonl"), b"owned").unwrap();
        fs::set_permissions(
            root.join("segment.jsonl"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let expected =
            super::file_identity(&File::open(root.join("segment.jsonl")).unwrap()).unwrap();
        super::reserve_delete_tombstone(
            &parent,
            OsStr::new("segment.jsonl"),
            expected,
            TOMBSTONE_LIMIT,
            &std::sync::atomic::AtomicUsize::new(next_slot),
        )
        .unwrap();
    }

    #[test]
    fn restart_preserves_foreign_tombstones_while_reclaiming_owned_slots() {
        const TOMBSTONE_LIMIT: usize = 4;

        let fixture = tempdir().unwrap();
        let root = fixture.path().join("diagnostics");
        fs::create_dir(&root).unwrap();
        let directory = DiagnosticDirectory::open(&root).unwrap();
        let parent = directory.root.clone_chain().unwrap();
        let foreign = root.join(format!("{DIAGNOSTIC_DELETE_TOMBSTONE_PREFIX}{:05}", 0));
        fs::write(&foreign, b"foreign").unwrap();
        fs::set_permissions(&foreign, fs::Permissions::from_mode(0o600)).unwrap();
        let mismatch_source = root.join("mismatch-source");
        fs::write(&mismatch_source, b"different object").unwrap();
        fs::set_permissions(&mismatch_source, fs::Permissions::from_mode(0o600)).unwrap();
        let mismatched_identity =
            super::file_identity(&File::open(&mismatch_source).unwrap()).unwrap();
        let foreign_marker = root.join(super::delete_tombstone_marker_name(0));
        fs::write(
            &foreign_marker,
            super::encode_delete_tombstone_marker(mismatched_identity),
        )
        .unwrap();
        fs::set_permissions(&foreign_marker, fs::Permissions::from_mode(0o600)).unwrap();
        for slot in 1..TOMBSTONE_LIMIT {
            write_owned_crash_tombstone(&root, slot, b"owned crash payload");
        }

        let next_slot =
            super::initial_delete_tombstone_slot_with_limit(&parent, TOMBSTONE_LIMIT).unwrap();

        assert_eq!(next_slot, 1);
        assert_eq!(fs::read(&foreign).unwrap(), b"foreign");
        assert_eq!(
            fs::read(&foreign_marker).unwrap(),
            super::encode_delete_tombstone_marker(mismatched_identity)
        );
        for slot in 1..TOMBSTONE_LIMIT {
            assert!(
                !root
                    .join(format!("{DIAGNOSTIC_DELETE_TOMBSTONE_PREFIX}{slot:05}"))
                    .exists()
            );
            assert!(
                !root
                    .join(super::delete_tombstone_marker_name(slot))
                    .exists()
            );
        }
    }

    #[test]
    fn active_tombstone_lock_blocks_reclamation_until_the_owner_drops() {
        const TOMBSTONE_LIMIT: usize = 1;

        let fixture = tempdir().unwrap();
        let root = fixture.path().join("diagnostics");
        fs::create_dir(&root).unwrap();
        let directory = DiagnosticDirectory::open(&root).unwrap();
        let parent = directory.root.clone_chain().unwrap();
        fs::write(root.join("segment.jsonl"), b"active owned payload").unwrap();
        fs::set_permissions(
            root.join("segment.jsonl"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let expected =
            super::file_identity(&File::open(root.join("segment.jsonl")).unwrap()).unwrap();
        let reservation = super::reserve_delete_tombstone(
            &parent,
            OsStr::new("segment.jsonl"),
            expected,
            TOMBSTONE_LIMIT,
            &std::sync::atomic::AtomicUsize::new(0),
        )
        .unwrap();

        assert_eq!(
            super::initial_delete_tombstone_slot_with_limit(&parent, TOMBSTONE_LIMIT).unwrap(),
            TOMBSTONE_LIMIT
        );
        assert!(root.join(&reservation.name).exists());
        assert!(
            root.join(super::delete_tombstone_marker_name(reservation.slot))
                .exists()
        );

        drop(reservation);
        assert_eq!(
            super::initial_delete_tombstone_slot_with_limit(&parent, TOMBSTONE_LIMIT).unwrap(),
            0
        );
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    }

    #[test]
    fn restart_reclaims_a_strict_marker_left_before_rename() {
        const TOMBSTONE_LIMIT: usize = 1;

        let fixture = tempdir().unwrap();
        let root = fixture.path().join("diagnostics");
        fs::create_dir(&root).unwrap();
        let directory = DiagnosticDirectory::open(&root).unwrap();
        let parent = directory.root.clone_chain().unwrap();
        let source = root.join("segment.jsonl");
        fs::write(&source, b"source remains canonical").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
        let expected = super::file_identity(&File::open(&source).unwrap()).unwrap();
        let marker = root.join(super::delete_tombstone_marker_name(0));
        fs::write(&marker, super::encode_delete_tombstone_marker(expected)).unwrap();
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();

        assert_eq!(
            super::initial_delete_tombstone_slot_with_limit(&parent, TOMBSTONE_LIMIT).unwrap(),
            0
        );
        assert!(!marker.exists());
        assert_eq!(fs::read(source).unwrap(), b"source remains canonical");
    }

    #[test]
    fn sequential_tombstone_reservations_do_not_rescan_occupied_prefixes() {
        const DELETE_COUNT: usize = 4_100;
        const {
            assert!(super::MAX_DIAGNOSTIC_DELETE_TOMBSTONES == 16_384);
        }

        let fixture = tempdir().unwrap();
        let root = fixture.path().join("diagnostics");
        fs::create_dir(&root).unwrap();
        let directory = DiagnosticDirectory::open(&root).unwrap();
        let parent = directory.root.clone_chain().unwrap();
        super::TOMBSTONE_OCCUPANCY_CHECKS.with(|checks| checks.set(0));

        for index in 0..DELETE_COUNT {
            let source = format!("source-{index:05}");
            fs::write(root.join(&source), []).unwrap();
            fs::set_permissions(root.join(&source), fs::Permissions::from_mode(0o600)).unwrap();
            let expected = super::file_identity(&File::open(root.join(&source)).unwrap()).unwrap();
            super::reserve_delete_tombstone(
                &parent,
                OsStr::new(&source),
                expected,
                super::MAX_DIAGNOSTIC_DELETE_TOMBSTONES,
                &directory.next_delete_tombstone_slot,
            )
            .unwrap();
        }

        let occupancy_checks = super::TOMBSTONE_OCCUPANCY_CHECKS.with(std::cell::Cell::get);
        assert!(
            occupancy_checks <= DELETE_COUNT * 2,
            "{DELETE_COUNT} sequential reservations performed {occupancy_checks} occupancy checks"
        );
    }
}

#[cfg(test)]
mod resident_allocation_tests {
    use std::{ffi::OsStr, fs, mem::size_of};

    use tempfile::tempdir;

    use super::{DiagnosticDirectory, DiagnosticReadLease};

    #[test]
    fn read_lease_reports_every_nested_heap_allocation_without_its_inline_slot() {
        let fixture = tempdir().unwrap();
        let root = fixture.path().join("diagnostics");
        fs::create_dir(&root).unwrap();
        let directory = DiagnosticDirectory::open(&root).unwrap();
        drop(
            directory
                .create_new(OsStr::new("segment.jsonl"), b"safe", 64)
                .unwrap(),
        );
        let entry = directory.entries(1).unwrap().pop().unwrap();
        let lease = directory.open_read(&entry, 64).unwrap();
        let expected = lease
            .pinned
            .name
            .capacity()
            .checked_add(lease.pinned.parent.resident_allocation_bytes().unwrap())
            .unwrap();

        assert_eq!(lease.resident_allocation_bytes().unwrap(), expected);
        assert!(
            lease.resident_allocation_bytes().unwrap()
                < size_of::<DiagnosticReadLease>() + expected
        );
    }
}
