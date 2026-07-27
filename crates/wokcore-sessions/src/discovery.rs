use std::{
    collections::HashSet,
    ffi::OsStr,
    fmt,
    path::{Path, PathBuf},
};

use wokcore_platform::sessions::{
    SessionDirectoryLease, SessionError, SessionFile, SessionFileIdentity, SessionFileKind,
    SessionRootLease,
};

mod slice;

pub use slice::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionLocation {
    Live,
    Archive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiscoveryLimits {
    pub maximum_entries_per_directory: usize,
    pub maximum_total_sessions: usize,
    pub maximum_total_entries: usize,
}

impl Default for DiscoveryLimits {
    fn default() -> Self {
        Self {
            maximum_entries_per_directory: 4_096,
            maximum_total_sessions: 65_536,
            maximum_total_entries: 131_072,
        }
    }
}

pub struct DiscoveredSession {
    relative_path: PathBuf,
    file_name: String,
    location: SessionLocation,
    identity: SessionFileIdentity,
}

impl DiscoveredSession {
    pub(crate) fn from_slice(entry: &SessionDiscoveryEntry) -> Option<Self> {
        let location = match entry.format() {
            SessionDiscoverySourceFormat::CodexLiveJsonl => SessionLocation::Live,
            SessionDiscoverySourceFormat::CodexArchiveJsonl => SessionLocation::Archive,
            _ => return None,
        };
        Some(Self {
            relative_path: entry.relative_path().to_path_buf(),
            file_name: entry.file_name().to_owned(),
            location,
            identity: entry.identity(),
        })
    }

    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    pub fn location(&self) -> SessionLocation {
        self.location
    }

    pub fn identity(&self) -> SessionFileIdentity {
        self.identity
    }

    pub fn open(
        &self,
        root: &SessionRootLease,
        maximum_size: u64,
    ) -> Result<SessionFile, SessionError> {
        let file = root.open_file(&self.relative_path, maximum_size)?;
        if file.snapshot().identity != self.identity {
            return Err(SessionError::SessionFileChanged);
        }
        Ok(file)
    }

    pub(crate) fn relative_path(&self) -> &Path {
        &self.relative_path
    }
}

impl fmt::Debug for DiscoveredSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiscoveredSession")
            .field("location", &self.location)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("Session discovery exceeded its bounded entry limit")]
    Limit,
    #[error("Session discovery encountered an unsafe filesystem object")]
    Unsafe,
    #[error("Session discovery failed")]
    Failed,
}

impl DiscoveryError {
    pub fn stable_code(&self) -> &'static str {
        match self {
            Self::Limit => "session_discovery_limit",
            Self::Unsafe => "session_discovery_unsafe",
            Self::Failed => "session_discovery_failed",
        }
    }
}

impl From<SessionError> for DiscoveryError {
    fn from(error: SessionError) -> Self {
        match error {
            SessionError::EnumerationLimitExceeded
            | SessionError::ReadLimitExceeded
            | SessionError::CleanupLimitExceeded => Self::Limit,
            SessionError::UnsafePath | SessionError::SessionFileChanged => Self::Unsafe,
            SessionError::MissingPlatformData { .. }
            | SessionError::SessionFileUnavailable
            | SessionError::Io { .. } => Self::Failed,
        }
    }
}

pub fn discover_codex_sessions(
    root: &SessionRootLease,
    limits: DiscoveryLimits,
) -> Result<Vec<DiscoveredSession>, DiscoveryError> {
    if limits.maximum_entries_per_directory == 0 || limits.maximum_total_sessions == 0 {
        return Err(DiscoveryError::Limit);
    }
    let mut output = Vec::new();
    let mut identities = HashSet::new();
    let mut entry_budget = limits.maximum_total_entries;

    if let Some(sessions) = optional_directory(root, Path::new("sessions"))? {
        for year in directory_names(
            &sessions,
            limits.maximum_entries_per_directory,
            &mut entry_budget,
        )? {
            if !is_numeric_component(&year, 4) {
                continue;
            }
            let year_path = Path::new("sessions").join(&year);
            let year_directory = match optional_directory(root, &year_path)? {
                Some(directory) => directory,
                None => continue,
            };
            for month in directory_names(
                &year_directory,
                limits.maximum_entries_per_directory,
                &mut entry_budget,
            )? {
                if !is_calendar_month(&month) {
                    continue;
                }
                let month_path = year_path.join(&month);
                let month_directory = match optional_directory(root, &month_path)? {
                    Some(directory) => directory,
                    None => continue,
                };
                for day in directory_names(
                    &month_directory,
                    limits.maximum_entries_per_directory,
                    &mut entry_budget,
                )? {
                    if !is_calendar_day(&year, &month, &day) {
                        continue;
                    }
                    let day_path = month_path.join(&day);
                    let day_directory = match optional_directory(root, &day_path)? {
                        Some(directory) => directory,
                        None => continue,
                    };
                    collect_jsonl(
                        &day_directory,
                        &day_path,
                        SessionLocation::Live,
                        limits,
                        &mut identities,
                        &mut output,
                        &mut entry_budget,
                    )?;
                }
            }
        }
    }

    if let Some(archive) = optional_directory(root, Path::new("archived_sessions"))? {
        collect_jsonl(
            &archive,
            Path::new("archived_sessions"),
            SessionLocation::Archive,
            limits,
            &mut identities,
            &mut output,
            &mut entry_budget,
        )?;
    }
    Ok(output)
}

fn optional_directory(
    root: &SessionRootLease,
    relative: &Path,
) -> Result<Option<SessionDirectoryLease>, DiscoveryError> {
    match root.open_directory(relative) {
        Ok(directory) => Ok(Some(directory)),
        Err(SessionError::Io { source }) if source.kind() == std::io::ErrorKind::NotFound => {
            Ok(None)
        }
        Err(SessionError::UnsafePath) => Err(DiscoveryError::Unsafe),
        Err(error) => Err(error.into()),
    }
}

fn directory_names(
    directory: &SessionDirectoryLease,
    limit: usize,
    entry_budget: &mut usize,
) -> Result<Vec<String>, DiscoveryError> {
    let entries = directory.entries(limit)?;
    consume_entries(entry_budget, entries.len())?;
    Ok(entries
        .into_iter()
        .filter(|entry| entry.snapshot().kind == SessionFileKind::Directory)
        .map(|entry| entry.name().to_string_lossy().into_owned())
        .collect())
}

fn collect_jsonl(
    directory: &SessionDirectoryLease,
    base: &Path,
    location: SessionLocation,
    limits: DiscoveryLimits,
    identities: &mut HashSet<SessionFileIdentity>,
    output: &mut Vec<DiscoveredSession>,
    entry_budget: &mut usize,
) -> Result<(), DiscoveryError> {
    let entries = directory.entries(limits.maximum_entries_per_directory)?;
    consume_entries(entry_budget, entries.len())?;
    for entry in entries {
        if entry.snapshot().kind != SessionFileKind::RegularFile || !is_jsonl(entry.name()) {
            continue;
        }
        if identities.insert(entry.snapshot().identity) {
            if output.len() >= limits.maximum_total_sessions {
                return Err(DiscoveryError::Limit);
            }
            output.push(DiscoveredSession {
                relative_path: base.join(entry.name()),
                file_name: entry.name().to_string_lossy().into_owned(),
                location,
                identity: entry.snapshot().identity,
            });
        }
    }
    Ok(())
}

fn is_numeric_component(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_calendar_month(value: &str) -> bool {
    is_numeric_component(value, 2)
        && value
            .parse::<u8>()
            .is_ok_and(|month| (1..=12).contains(&month))
}

fn is_calendar_day(year: &str, month: &str, value: &str) -> bool {
    if !is_numeric_component(value, 2) {
        return false;
    }
    let Ok(year) = year.parse::<u32>() else {
        return false;
    };
    let Ok(month) = month.parse::<u8>() else {
        return false;
    };
    let Ok(day) = value.parse::<u8>() else {
        return false;
    };
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let maximum = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=maximum).contains(&day)
}

fn consume_entries(budget: &mut usize, entries: usize) -> Result<(), DiscoveryError> {
    *budget = budget.checked_sub(entries).ok_or(DiscoveryError::Limit)?;
    Ok(())
}

fn is_jsonl(name: &OsStr) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
}

#[cfg(test)]
mod tests {
    use super::DiscoveryError;
    use wokcore_platform::sessions::SessionError;

    #[test]
    fn cleanup_limit_maps_to_discovery_resource_limit() {
        let error = DiscoveryError::from(SessionError::CleanupLimitExceeded);
        assert_eq!(error.stable_code(), "session_discovery_limit");
    }
}
