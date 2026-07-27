use std::{
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime},
};

use thiserror::Error;
use wokcore_platform::diagnostics::{
    DiagnosticDirectory, DiagnosticEntry, DiagnosticIdentity, DiagnosticReadLease,
    DiagnosticStoreError,
};

pub const MAX_RETENTION_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
pub const MAX_CLOSED_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;
pub const RETENTION_PAGE_ENTRIES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetentionTrigger {
    Startup,
    Rotation,
    BatchFlush,
    IdleTick,
    Query,
}

impl RetentionTrigger {
    const fn performs_retention(self) -> bool {
        matches!(self, Self::Startup | Self::Rotation)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RetentionError {
    #[error("invalid diagnostic retention policy")]
    InvalidPolicy,
    #[error("diagnostic retention boundary is unsafe")]
    UnsafeBoundary,
    #[error("diagnostic retention operation failed")]
    Io,
    #[error("diagnostic retention enumeration limit reached")]
    EntryLimit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionPolicy {
    max_age: Duration,
    max_bytes: u64,
}

impl RetentionPolicy {
    pub const fn standard() -> Self {
        Self {
            max_age: MAX_RETENTION_AGE,
            max_bytes: MAX_CLOSED_SEGMENT_BYTES,
        }
    }

    pub fn with_limits(max_age: Duration, max_bytes: u64) -> Result<Self, RetentionError> {
        if max_age > MAX_RETENTION_AGE || max_bytes > MAX_CLOSED_SEGMENT_BYTES {
            return Err(RetentionError::InvalidPolicy);
        }
        Ok(Self { max_age, max_bytes })
    }
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self::standard()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RetentionReport {
    noop: bool,
    removed_files: usize,
    removed_bytes: u64,
    pages_scanned: usize,
}

impl RetentionReport {
    pub const fn noop(self) -> bool {
        self.noop
    }

    pub const fn removed_files(self) -> usize {
        self.removed_files
    }

    pub const fn removed_bytes(self) -> u64 {
        self.removed_bytes
    }

    pub const fn pages_scanned(self) -> usize {
        self.pages_scanned
    }
}

pub struct ClosedSegmentLease {
    file: DiagnosticReadLease,
}

impl ClosedSegmentLease {
    pub fn open(
        directory: &DiagnosticDirectory,
        entry: &DiagnosticEntry,
    ) -> Result<Self, RetentionError> {
        let mut file = directory
            .open_read(entry, u64::MAX)
            .map_err(map_platform_error)?;
        if parse_segment_index(file.name().to_str().ok_or(RetentionError::UnsafeBoundary)?)
            .is_none()
        {
            return Err(RetentionError::UnsafeBoundary);
        }
        let bytes = read_owned_segment(&mut file)?;
        crate::segment::validate_complete_segment_bytes(&bytes)
            .ok_or(RetentionError::UnsafeBoundary)?;
        Ok(Self { file })
    }

    fn identity(&self) -> DiagnosticIdentity {
        self.file.identity()
    }
}

impl fmt::Debug for ClosedSegmentLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ClosedSegmentLease([redacted])")
    }
}

pub struct RetentionManager {
    root: PathBuf,
    policy: RetentionPolicy,
    directory: Mutex<Option<DiagnosticDirectory>>,
}

impl RetentionManager {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self::with_policy(root, RetentionPolicy::standard())
    }

    pub fn with_policy(root: impl AsRef<Path>, policy: RetentionPolicy) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            policy,
            directory: Mutex::new(None),
        }
    }

    pub fn enforce(
        &self,
        trigger: RetentionTrigger,
        now: SystemTime,
        leases: &[&ClosedSegmentLease],
    ) -> Result<RetentionReport, RetentionError> {
        self.enforce_with_active(trigger, now, leases, None)
    }

    pub fn enforce_with_active(
        &self,
        trigger: RetentionTrigger,
        now: SystemTime,
        leases: &[&ClosedSegmentLease],
        active_segment: Option<u64>,
    ) -> Result<RetentionReport, RetentionError> {
        if !trigger.performs_retention() {
            return Ok(RetentionReport {
                noop: true,
                ..RetentionReport::default()
            });
        }
        let mut directory_guard = self.directory.lock().map_err(|_| RetentionError::Io)?;
        if directory_guard.is_none() {
            let directory = match DiagnosticDirectory::open(&self.root) {
                Ok(directory) => directory,
                Err(DiagnosticStoreError::Unavailable) => return Ok(RetentionReport::default()),
                Err(error) => return Err(map_platform_error(error)),
            };
            *directory_guard = Some(directory);
        }
        let directory = directory_guard
            .as_ref()
            .ok_or(RetentionError::UnsafeBoundary)?;
        let mut report = RetentionReport::default();
        let (validation_pages, discovered_active) =
            scan_owned(directory, active_segment, |_| Ok(()))?;
        report.pages_scanned = report.pages_scanned.saturating_add(validation_pages);
        if active_segment.is_some_and(|active| active != discovered_active) {
            return Err(RetentionError::UnsafeBoundary);
        }
        let highest_index = active_segment.unwrap_or(discovered_active);

        let mut retained_bytes = 0_u64;
        report.pages_scanned =
            report
                .pages_scanned
                .saturating_add(scan_canonical_metadata(directory, |candidate| {
                    if candidate.index == highest_index {
                        return Ok(());
                    }
                    if expired_by_age(candidate.modified, now, self.policy.max_age)
                        && !is_leased(candidate.identity, leases)
                    {
                        revalidate_candidate_for_removal(directory, &candidate)?;
                        remove_candidate(directory, &candidate, &mut report)
                    } else {
                        retained_bytes = retained_bytes.saturating_add(candidate.bytes);
                        Ok(())
                    }
                })?);

        if retained_bytes > self.policy.max_bytes {
            report.pages_scanned = report.pages_scanned.saturating_add(scan_canonical_metadata(
                directory,
                |candidate| {
                    if retained_bytes <= self.policy.max_bytes {
                        return Ok(());
                    }
                    if candidate.index == highest_index || is_leased(candidate.identity, leases) {
                        return Ok(());
                    }
                    revalidate_candidate_for_removal(directory, &candidate)?;
                    remove_candidate(directory, &candidate, &mut report)?;
                    retained_bytes = retained_bytes.saturating_sub(candidate.bytes);
                    Ok(())
                },
            )?);
        }
        Ok(report)
    }
}

fn scan_owned<F>(
    directory: &DiagnosticDirectory,
    active_segment: Option<u64>,
    mut visit: F,
) -> Result<(usize, u64), RetentionError>
where
    F: FnMut(Candidate) -> Result<(), RetentionError>,
{
    let maximum_size = u64::try_from(crate::segment::MAX_SEGMENT_BYTES)
        .map_err(|_| RetentionError::UnsafeBoundary)?;
    let mut after = None::<OsString>;
    let mut pages = 0_usize;
    let mut last_sequence = 0_u64;
    let mut highest_index = 0_u64;
    loop {
        let page = directory
            .entries_page(after.as_deref(), RETENTION_PAGE_ENTRIES)
            .map_err(map_platform_error)?;
        pages = pages.saturating_add(1);
        let next = page.next_after().map(OsString::from);
        for entry in page.into_entries() {
            let Some(index) = entry.name().to_str().and_then(parse_segment_index) else {
                continue;
            };
            highest_index = highest_index.max(index);
            if active_segment == Some(index) {
                continue;
            }
            let mut file = directory
                .open_read(&entry, maximum_size)
                .map_err(map_platform_error)?;
            let bytes = read_owned_segment(&mut file)?;
            let (first_sequence, candidate_last) =
                crate::segment::validate_complete_segment_bytes(&bytes)
                    .ok_or(RetentionError::UnsafeBoundary)?;
            if first_sequence <= last_sequence {
                return Err(RetentionError::UnsafeBoundary);
            }
            last_sequence = candidate_last;
            drop(file);
            visit(Candidate {
                identity: entry.identity(),
                bytes: entry.len(),
                modified: entry.modified(),
                entry,
                index,
            })?;
        }
        let Some(next) = next else {
            break;
        };
        after = Some(next);
    }
    Ok((pages, highest_index))
}

fn scan_canonical_metadata<F>(
    directory: &DiagnosticDirectory,
    mut visit: F,
) -> Result<usize, RetentionError>
where
    F: FnMut(Candidate) -> Result<(), RetentionError>,
{
    let mut after = None::<OsString>;
    let mut pages = 0_usize;
    loop {
        let page = directory
            .entries_page(after.as_deref(), RETENTION_PAGE_ENTRIES)
            .map_err(map_platform_error)?;
        pages = pages.saturating_add(1);
        let next = page.next_after().map(OsString::from);
        for entry in page.into_entries() {
            let Some(index) = entry.name().to_str().and_then(parse_segment_index) else {
                continue;
            };
            visit(Candidate {
                identity: entry.identity(),
                bytes: entry.len(),
                modified: entry.modified(),
                entry,
                index,
            })?;
        }
        let Some(next) = next else {
            break;
        };
        after = Some(next);
    }
    Ok(pages)
}

fn read_owned_segment(file: &mut DiagnosticReadLease) -> Result<Vec<u8>, RetentionError> {
    let length = usize::try_from(file.len()).map_err(|_| RetentionError::UnsafeBoundary)?;
    if length == 0 || length > crate::segment::MAX_SEGMENT_BYTES {
        return Err(RetentionError::UnsafeBoundary);
    }
    let bytes = file.read_range(0, length).map_err(map_platform_error)?;
    if bytes.len() != length {
        return Err(RetentionError::UnsafeBoundary);
    }
    Ok(bytes)
}

impl fmt::Debug for RetentionManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RetentionManager([redacted])")
    }
}

struct Candidate {
    entry: DiagnosticEntry,
    index: u64,
    identity: DiagnosticIdentity,
    bytes: u64,
    modified: Option<SystemTime>,
}

fn expired_by_age(modified: Option<SystemTime>, now: SystemTime, maximum_age: Duration) -> bool {
    modified
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age > maximum_age)
}

fn parse_segment_index(name: &str) -> Option<u64> {
    let value = name.strip_prefix("segment-")?.strip_suffix(".jsonl")?;
    if value.len() != 20 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok().filter(|index| *index != 0)
}

fn is_leased(identity: DiagnosticIdentity, leases: &[&ClosedSegmentLease]) -> bool {
    leases.iter().any(|lease| lease.identity() == identity)
}

fn revalidate_candidate_for_removal(
    directory: &DiagnosticDirectory,
    candidate: &Candidate,
) -> Result<(), RetentionError> {
    let maximum_size = u64::try_from(crate::segment::MAX_SEGMENT_BYTES)
        .map_err(|_| RetentionError::UnsafeBoundary)?;
    let mut file = directory
        .open_read(&candidate.entry, maximum_size)
        .map_err(map_platform_error)?;
    let bytes = read_owned_segment(&mut file)?;
    crate::segment::validate_complete_segment_bytes(&bytes)
        .ok_or(RetentionError::UnsafeBoundary)?;
    Ok(())
}

fn remove_candidate(
    directory: &DiagnosticDirectory,
    candidate: &Candidate,
    report: &mut RetentionReport,
) -> Result<(), RetentionError> {
    directory
        .remove(&candidate.entry)
        .map_err(map_platform_error)?;
    report.removed_files = report.removed_files.saturating_add(1);
    report.removed_bytes = report.removed_bytes.saturating_add(candidate.bytes);
    Ok(())
}

fn map_platform_error(error: DiagnosticStoreError) -> RetentionError {
    match error {
        DiagnosticStoreError::UnsafePath
        | DiagnosticStoreError::Changed
        | DiagnosticStoreError::Unavailable
        | DiagnosticStoreError::SizeLimitExceeded => RetentionError::UnsafeBoundary,
        DiagnosticStoreError::EnumerationLimitExceeded => RetentionError::EntryLimit,
        _ => RetentionError::Io,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_modified_time_counts_toward_bytes_but_never_expires_by_age() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let candidates = [
            (40_u64, None, 1_u64),
            (
                60_u64,
                Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1)),
                2_u64,
            ),
        ];

        assert_eq!(
            candidates.iter().map(|(bytes, _, _)| *bytes).sum::<u64>(),
            100
        );
        assert!(!expired_by_age(None, now, Duration::ZERO));
    }
}
