use std::{
    ffi::OsStr,
    fmt, io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use wokcore_platform::sessions::{
    SessionDirectoryEntry, SessionDirectoryPageKey, SessionError, SessionFile, SessionFileIdentity,
    SessionFileKind, SessionRootLease,
};

use super::{
    DiscoveryError, DiscoveryLimits, SessionLocation, is_calendar_day, is_calendar_month, is_jsonl,
    is_numeric_component,
};

pub const DEFAULT_SESSION_DISCOVERY_SLICE_ENTRIES: usize = 256;
pub const MAX_SESSION_DISCOVERY_SLICE_ENTRIES: usize = 1_024;
pub const DEFAULT_SESSION_DISCOVERY_SOFT_DEADLINE: Duration = Duration::from_millis(25);
pub const MAX_SESSION_DISCOVERY_HARD_DEADLINE: Duration = Duration::from_millis(100);

const MAX_CLAUDE_SUBAGENT_DIRECTORY_DEPTH: usize = 16;
const MAX_CLAUDE_SUBAGENT_DIRECTORIES: usize = 16_384;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionDiscoveryKind {
    Codex,
    Claude,
    Gemini,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionDiscoverySourceFormat {
    CodexLiveJsonl,
    CodexArchiveJsonl,
    ClaudeJsonl,
    GeminiCurrentJsonl,
    GeminiLegacyJson,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionDiscoverySliceOutcome {
    Complete,
    EntryLimitReached,
    SoftDeadlineReached,
    HardDeadlineReached,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionDiscoverySliceBudget {
    maximum_entries: usize,
    soft_deadline: Duration,
    hard_deadline: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("the Session discovery slice budget is outside its hard bounds")]
pub struct SessionDiscoverySliceBudgetError;

impl SessionDiscoverySliceBudget {
    pub fn new(
        maximum_entries: usize,
        soft_deadline: Duration,
        hard_deadline: Duration,
    ) -> Result<Self, SessionDiscoverySliceBudgetError> {
        if maximum_entries == 0
            || maximum_entries > MAX_SESSION_DISCOVERY_SLICE_ENTRIES
            || soft_deadline.is_zero()
            || soft_deadline > hard_deadline
            || hard_deadline > MAX_SESSION_DISCOVERY_HARD_DEADLINE
        {
            return Err(SessionDiscoverySliceBudgetError);
        }
        Ok(Self {
            maximum_entries,
            soft_deadline,
            hard_deadline,
        })
    }

    pub const fn maximum_entries(self) -> usize {
        self.maximum_entries
    }

    pub const fn soft_deadline(self) -> Duration {
        self.soft_deadline
    }

    pub const fn hard_deadline(self) -> Duration {
        self.hard_deadline
    }
}

impl Default for SessionDiscoverySliceBudget {
    fn default() -> Self {
        Self {
            maximum_entries: DEFAULT_SESSION_DISCOVERY_SLICE_ENTRIES,
            soft_deadline: DEFAULT_SESSION_DISCOVERY_SOFT_DEADLINE,
            hard_deadline: MAX_SESSION_DISCOVERY_HARD_DEADLINE,
        }
    }
}

pub trait SessionDiscoveryClock {
    fn now(&self) -> Instant;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemSessionDiscoveryClock;

impl SessionDiscoveryClock for SystemSessionDiscoveryClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[derive(Clone)]
pub struct SessionDiscoveryEntry {
    relative_path: PathBuf,
    file_name: String,
    identity: SessionFileIdentity,
    format: SessionDiscoverySourceFormat,
}

impl SessionDiscoveryEntry {
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    pub const fn identity(&self) -> SessionFileIdentity {
        self.identity
    }

    pub const fn format(&self) -> SessionDiscoverySourceFormat {
        self.format
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

    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }
}

impl fmt::Debug for SessionDiscoveryEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionDiscoveryEntry")
            .field("identity", &self.identity)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct SessionDiscoverySlice {
    pub outcome: SessionDiscoverySliceOutcome,
    pub processed_entries: usize,
    pub entries: Vec<SessionDiscoveryEntry>,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionDiscoverySliceError {
    #[error("the Session discovery cursor belongs to another source kind")]
    CursorKindMismatch,
    #[error(transparent)]
    Discovery(#[from] DiscoveryError),
}

#[derive(Clone)]
pub struct SessionDiscoveryCursor {
    kind: SessionDiscoveryKind,
    limits: DiscoveryLimits,
    stack: Vec<DirectoryFrame>,
    total_entries: usize,
    total_sessions: usize,
    claude_subagent_directories: usize,
}

impl SessionDiscoveryCursor {
    pub fn new(kind: SessionDiscoveryKind) -> Self {
        Self::with_limits(kind, DiscoveryLimits::default())
            .expect("default Session discovery limits are valid")
    }

    pub fn with_limits(
        kind: SessionDiscoveryKind,
        limits: DiscoveryLimits,
    ) -> Result<Self, DiscoveryError> {
        if limits.maximum_entries_per_directory == 0
            || limits.maximum_total_entries == 0
            || limits.maximum_total_sessions == 0
        {
            return Err(DiscoveryError::Limit);
        }
        Ok(Self {
            kind,
            limits,
            stack: initial_stack(kind),
            total_entries: 0,
            total_sessions: 0,
            claude_subagent_directories: 0,
        })
    }

    pub const fn kind(&self) -> SessionDiscoveryKind {
        self.kind
    }

    pub fn is_complete(&self) -> bool {
        self.stack.is_empty()
    }
}

impl fmt::Debug for SessionDiscoveryCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionDiscoveryCursor")
            .field("kind", &self.kind)
            .field("depth", &self.stack.len())
            .field("total_entries", &self.total_entries)
            .field("total_sessions", &self.total_sessions)
            .finish_non_exhaustive()
    }
}

pub fn discover_codex_sessions_slice(
    root: &SessionRootLease,
    cursor: &mut SessionDiscoveryCursor,
    budget: SessionDiscoverySliceBudget,
) -> Result<SessionDiscoverySlice, SessionDiscoverySliceError> {
    discover_codex_sessions_slice_with_clock(root, cursor, budget, &SystemSessionDiscoveryClock)
}

pub fn discover_codex_sessions_slice_with_clock<C>(
    root: &SessionRootLease,
    cursor: &mut SessionDiscoveryCursor,
    budget: SessionDiscoverySliceBudget,
    clock: &C,
) -> Result<SessionDiscoverySlice, SessionDiscoverySliceError>
where
    C: SessionDiscoveryClock + ?Sized,
{
    discover_sessions_slice(root, cursor, budget, SessionDiscoveryKind::Codex, clock)
}

pub fn discover_claude_sessions_slice(
    root: &SessionRootLease,
    cursor: &mut SessionDiscoveryCursor,
    budget: SessionDiscoverySliceBudget,
) -> Result<SessionDiscoverySlice, SessionDiscoverySliceError> {
    discover_claude_sessions_slice_with_clock(root, cursor, budget, &SystemSessionDiscoveryClock)
}

pub fn discover_claude_sessions_slice_with_clock<C>(
    root: &SessionRootLease,
    cursor: &mut SessionDiscoveryCursor,
    budget: SessionDiscoverySliceBudget,
    clock: &C,
) -> Result<SessionDiscoverySlice, SessionDiscoverySliceError>
where
    C: SessionDiscoveryClock + ?Sized,
{
    discover_sessions_slice(root, cursor, budget, SessionDiscoveryKind::Claude, clock)
}

pub fn discover_gemini_sessions_slice(
    root: &SessionRootLease,
    cursor: &mut SessionDiscoveryCursor,
    budget: SessionDiscoverySliceBudget,
) -> Result<SessionDiscoverySlice, SessionDiscoverySliceError> {
    discover_gemini_sessions_slice_with_clock(root, cursor, budget, &SystemSessionDiscoveryClock)
}

pub fn discover_gemini_sessions_slice_with_clock<C>(
    root: &SessionRootLease,
    cursor: &mut SessionDiscoveryCursor,
    budget: SessionDiscoverySliceBudget,
    clock: &C,
) -> Result<SessionDiscoverySlice, SessionDiscoverySliceError>
where
    C: SessionDiscoveryClock + ?Sized,
{
    discover_sessions_slice(root, cursor, budget, SessionDiscoveryKind::Gemini, clock)
}

fn discover_sessions_slice<C>(
    root: &SessionRootLease,
    cursor: &mut SessionDiscoveryCursor,
    budget: SessionDiscoverySliceBudget,
    expected_kind: SessionDiscoveryKind,
    clock: &C,
) -> Result<SessionDiscoverySlice, SessionDiscoverySliceError>
where
    C: SessionDiscoveryClock + ?Sized,
{
    if cursor.kind != expected_kind {
        return Err(SessionDiscoverySliceError::CursorKindMismatch);
    }
    let mut working = cursor.clone();
    let slice = scan_slice(root, &mut working, budget, clock)?;
    *cursor = working;
    Ok(slice)
}

fn scan_slice<C>(
    root: &SessionRootLease,
    cursor: &mut SessionDiscoveryCursor,
    budget: SessionDiscoverySliceBudget,
    clock: &C,
) -> Result<SessionDiscoverySlice, SessionDiscoverySliceError>
where
    C: SessionDiscoveryClock + ?Sized,
{
    let started_at = clock.now();
    let mut processed_entries = 0;
    let mut output = Vec::new();

    loop {
        if cursor.stack.is_empty() {
            return Ok(SessionDiscoverySlice {
                outcome: SessionDiscoverySliceOutcome::Complete,
                processed_entries,
                entries: output,
            });
        }
        if processed_entries == budget.maximum_entries {
            return Ok(SessionDiscoverySlice {
                outcome: SessionDiscoverySliceOutcome::EntryLimitReached,
                processed_entries,
                entries: output,
            });
        }

        let frame_index = cursor.stack.len() - 1;
        let frame_snapshot = cursor.stack[frame_index].clone();
        let Some(directory) = open_optional_directory(root, &frame_snapshot.relative_path)? else {
            cursor.stack.pop();
            if let Some(outcome) = deadline_outcome(started_at, budget, clock) {
                return Ok(SessionDiscoverySlice {
                    outcome,
                    processed_entries,
                    entries: output,
                });
            }
            continue;
        };
        count_claude_subagent_directory(cursor, frame_index)?;

        let per_directory_remaining = cursor
            .limits
            .maximum_entries_per_directory
            .saturating_sub(cursor.stack[frame_index].entries_seen);
        let total_remaining = cursor
            .limits
            .maximum_total_entries
            .saturating_sub(cursor.total_entries);
        if per_directory_remaining == 0 || total_remaining == 0 {
            let probe = directory
                .entries_page_keyed(cursor.stack[frame_index].after.as_ref(), 1)
                .map_err(DiscoveryError::from)?;
            if probe.entries().is_empty() {
                cursor.stack.pop();
                continue;
            }
            return Err(DiscoveryError::Limit.into());
        }

        let request_entries = (budget.maximum_entries - processed_entries)
            .min(per_directory_remaining)
            .min(total_remaining);
        let page = directory
            .entries_page_keyed(cursor.stack[frame_index].after.as_ref(), request_entries)
            .map_err(DiscoveryError::from)?;
        let page_has_more = page.next_page_key().is_some();
        let (entries, _) = page.into_parts();
        if entries.is_empty() {
            cursor.stack.pop();
            if let Some(outcome) = deadline_outcome(started_at, budget, clock) {
                return Ok(SessionDiscoverySlice {
                    outcome,
                    processed_entries,
                    entries: output,
                });
            }
            continue;
        }

        let page_entries = entries.len();
        let mut handled_page_entries = 0;
        let mut descended = false;
        for entry in entries {
            let action = entry_action(root, &frame_snapshot, &entry)?;
            let resume_key = entry.resume_key();
            {
                let frame = &mut cursor.stack[frame_index];
                frame.after = Some(resume_key);
                frame.entries_seen += 1;
            }
            cursor.total_entries += 1;
            processed_entries += 1;
            handled_page_entries += 1;

            match action {
                EntryAction::Skip => {}
                EntryAction::Emit(format) => {
                    if cursor.total_sessions == cursor.limits.maximum_total_sessions {
                        return Err(DiscoveryError::Limit.into());
                    }
                    cursor.total_sessions += 1;
                    output.push(SessionDiscoveryEntry {
                        relative_path: frame_snapshot.relative_path.join(entry.name()),
                        file_name: entry.name().to_string_lossy().into_owned(),
                        identity: entry.snapshot().identity,
                        format,
                    });
                }
                EntryAction::Descend(frame) => {
                    cursor.stack.push(frame);
                    descended = true;
                }
            }

            if let Some(outcome) = deadline_outcome(started_at, budget, clock) {
                return Ok(SessionDiscoverySlice {
                    outcome,
                    processed_entries,
                    entries: output,
                });
            }
            if processed_entries == budget.maximum_entries {
                return Ok(SessionDiscoverySlice {
                    outcome: SessionDiscoverySliceOutcome::EntryLimitReached,
                    processed_entries,
                    entries: output,
                });
            }
            if descended {
                break;
            }
        }

        if !descended && handled_page_entries == page_entries && !page_has_more {
            cursor.stack.pop();
        }
    }
}

fn deadline_outcome<C>(
    started_at: Instant,
    budget: SessionDiscoverySliceBudget,
    clock: &C,
) -> Option<SessionDiscoverySliceOutcome>
where
    C: SessionDiscoveryClock + ?Sized,
{
    let elapsed = clock.now().saturating_duration_since(started_at);
    if elapsed >= budget.hard_deadline {
        Some(SessionDiscoverySliceOutcome::HardDeadlineReached)
    } else if elapsed >= budget.soft_deadline {
        Some(SessionDiscoverySliceOutcome::SoftDeadlineReached)
    } else {
        None
    }
}

fn count_claude_subagent_directory(
    cursor: &mut SessionDiscoveryCursor,
    frame_index: usize,
) -> Result<(), DiscoveryError> {
    if !matches!(
        cursor.stack[frame_index].role,
        DirectoryRole::ClaudeSubagents { .. }
    ) || cursor.stack[frame_index].counted_claude_directory
    {
        return Ok(());
    }
    let maximum = MAX_CLAUDE_SUBAGENT_DIRECTORIES.min(cursor.limits.maximum_total_entries);
    if cursor.claude_subagent_directories == maximum {
        return Err(DiscoveryError::Limit);
    }
    cursor.claude_subagent_directories += 1;
    cursor.stack[frame_index].counted_claude_directory = true;
    Ok(())
}

fn open_optional_directory(
    root: &SessionRootLease,
    relative_path: &Path,
) -> Result<Option<wokcore_platform::sessions::SessionDirectoryLease>, DiscoveryError> {
    match root.open_directory(relative_path) {
        Ok(directory) => Ok(Some(directory)),
        Err(SessionError::Io { source }) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(SessionError::UnsafePath) => Err(DiscoveryError::Unsafe),
        Err(error) => Err(error.into()),
    }
}

fn entry_action(
    root: &SessionRootLease,
    frame: &DirectoryFrame,
    entry: &SessionDirectoryEntry,
) -> Result<EntryAction, DiscoveryError> {
    let kind = entry.snapshot().kind;
    match &frame.role {
        DirectoryRole::CodexYears => {
            let name = entry.name().to_string_lossy();
            if kind == SessionFileKind::Directory && is_numeric_component(&name, 4) {
                Ok(EntryAction::Descend(DirectoryFrame::new(
                    frame.relative_path.join(entry.name()),
                    DirectoryRole::CodexMonths {
                        year: name.into_owned(),
                    },
                )))
            } else {
                Ok(EntryAction::Skip)
            }
        }
        DirectoryRole::CodexMonths { year } => {
            let name = entry.name().to_string_lossy();
            if kind == SessionFileKind::Directory && is_calendar_month(&name) {
                Ok(EntryAction::Descend(DirectoryFrame::new(
                    frame.relative_path.join(entry.name()),
                    DirectoryRole::CodexDays {
                        year: year.clone(),
                        month: name.into_owned(),
                    },
                )))
            } else {
                Ok(EntryAction::Skip)
            }
        }
        DirectoryRole::CodexDays { year, month } => {
            let name = entry.name().to_string_lossy();
            if kind == SessionFileKind::Directory && is_calendar_day(year, month, &name) {
                Ok(EntryAction::Descend(DirectoryFrame::new(
                    frame.relative_path.join(entry.name()),
                    DirectoryRole::CodexFiles {
                        location: SessionLocation::Live,
                    },
                )))
            } else {
                Ok(EntryAction::Skip)
            }
        }
        DirectoryRole::CodexFiles { location } => {
            if kind != SessionFileKind::RegularFile || !is_jsonl(entry.name()) {
                return Ok(EntryAction::Skip);
            }
            Ok(EntryAction::Emit(match location {
                SessionLocation::Live => SessionDiscoverySourceFormat::CodexLiveJsonl,
                SessionLocation::Archive => SessionDiscoverySourceFormat::CodexArchiveJsonl,
            }))
        }
        DirectoryRole::ClaudeProjects => {
            if kind == SessionFileKind::Directory {
                Ok(EntryAction::Descend(DirectoryFrame::new(
                    frame.relative_path.join(entry.name()),
                    DirectoryRole::ClaudeProject,
                )))
            } else {
                Ok(EntryAction::Skip)
            }
        }
        DirectoryRole::ClaudeProject => match kind {
            SessionFileKind::RegularFile if is_jsonl(entry.name()) => {
                Ok(EntryAction::Emit(SessionDiscoverySourceFormat::ClaudeJsonl))
            }
            SessionFileKind::Directory => Ok(EntryAction::Descend(DirectoryFrame::new(
                frame.relative_path.join(entry.name()).join("subagents"),
                DirectoryRole::ClaudeSubagents { depth: 0 },
            ))),
            _ => Ok(EntryAction::Skip),
        },
        DirectoryRole::ClaudeSubagents { depth } => match kind {
            SessionFileKind::RegularFile if is_jsonl(entry.name()) => {
                Ok(EntryAction::Emit(SessionDiscoverySourceFormat::ClaudeJsonl))
            }
            SessionFileKind::Directory if *depth == MAX_CLAUDE_SUBAGENT_DIRECTORY_DEPTH => {
                Err(DiscoveryError::Limit)
            }
            SessionFileKind::Directory => Ok(EntryAction::Descend(DirectoryFrame::new(
                frame.relative_path.join(entry.name()),
                DirectoryRole::ClaudeSubagents { depth: depth + 1 },
            ))),
            _ => Ok(EntryAction::Skip),
        },
        DirectoryRole::GeminiProjects => {
            if kind == SessionFileKind::Directory {
                Ok(EntryAction::Descend(DirectoryFrame::new(
                    frame.relative_path.join(entry.name()).join("chats"),
                    DirectoryRole::GeminiChats,
                )))
            } else {
                Ok(EntryAction::Skip)
            }
        }
        DirectoryRole::GeminiChats => match kind {
            SessionFileKind::Directory => Ok(EntryAction::Descend(DirectoryFrame::new(
                frame.relative_path.join(entry.name()),
                DirectoryRole::GeminiChatDirectory,
            ))),
            SessionFileKind::RegularFile => gemini_file_action(root, frame, entry, true),
        },
        DirectoryRole::GeminiChatDirectory => {
            if kind == SessionFileKind::RegularFile
                && Path::new(entry.name())
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
            {
                Ok(EntryAction::Emit(
                    SessionDiscoverySourceFormat::GeminiCurrentJsonl,
                ))
            } else {
                Ok(EntryAction::Skip)
            }
        }
    }
}

fn gemini_file_action(
    root: &SessionRootLease,
    frame: &DirectoryFrame,
    entry: &SessionDirectoryEntry,
    allow_legacy: bool,
) -> Result<EntryAction, DiscoveryError> {
    let path = Path::new(entry.name());
    let starts_with_session = path
        .file_stem()
        .and_then(OsStr::to_str)
        .is_some_and(|stem| stem.starts_with("session-"));
    match path.extension().and_then(OsStr::to_str) {
        Some(extension)
            if extension.eq_ignore_ascii_case("jsonl")
                && (!allow_legacy || starts_with_session) =>
        {
            Ok(EntryAction::Emit(
                SessionDiscoverySourceFormat::GeminiCurrentJsonl,
            ))
        }
        Some(extension)
            if allow_legacy && extension.eq_ignore_ascii_case("json") && starts_with_session =>
        {
            let mut current_name = PathBuf::from(entry.name());
            current_name.set_extension("jsonl");
            let current_path = frame.relative_path.join(current_name);
            match root.open_file(&current_path, u64::MAX) {
                Ok(_) => Ok(EntryAction::Skip),
                Err(SessionError::Io { source }) if source.kind() == io::ErrorKind::NotFound => Ok(
                    EntryAction::Emit(SessionDiscoverySourceFormat::GeminiLegacyJson),
                ),
                Err(SessionError::UnsafePath) => Err(DiscoveryError::Unsafe),
                Err(error) => Err(error.into()),
            }
        }
        _ => Ok(EntryAction::Skip),
    }
}

enum EntryAction {
    Skip,
    Emit(SessionDiscoverySourceFormat),
    Descend(DirectoryFrame),
}

#[derive(Clone)]
struct DirectoryFrame {
    relative_path: PathBuf,
    role: DirectoryRole,
    after: Option<SessionDirectoryPageKey>,
    entries_seen: usize,
    counted_claude_directory: bool,
}

impl DirectoryFrame {
    fn new(relative_path: PathBuf, role: DirectoryRole) -> Self {
        Self {
            relative_path,
            role,
            after: None,
            entries_seen: 0,
            counted_claude_directory: false,
        }
    }
}

#[derive(Clone)]
enum DirectoryRole {
    CodexYears,
    CodexMonths { year: String },
    CodexDays { year: String, month: String },
    CodexFiles { location: SessionLocation },
    ClaudeProjects,
    ClaudeProject,
    ClaudeSubagents { depth: usize },
    GeminiProjects,
    GeminiChats,
    GeminiChatDirectory,
}

fn initial_stack(kind: SessionDiscoveryKind) -> Vec<DirectoryFrame> {
    match kind {
        SessionDiscoveryKind::Codex => vec![
            DirectoryFrame::new(
                PathBuf::from("archived_sessions"),
                DirectoryRole::CodexFiles {
                    location: SessionLocation::Archive,
                },
            ),
            DirectoryFrame::new(PathBuf::from("sessions"), DirectoryRole::CodexYears),
        ],
        SessionDiscoveryKind::Claude => vec![DirectoryFrame::new(
            PathBuf::from("projects"),
            DirectoryRole::ClaudeProjects,
        )],
        SessionDiscoveryKind::Gemini => vec![DirectoryFrame::new(
            PathBuf::from("tmp"),
            DirectoryRole::GeminiProjects,
        )],
    }
}
