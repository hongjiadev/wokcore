use std::{
    collections::VecDeque,
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, RwLock,
        atomic::{AtomicU8, Ordering},
    },
    thread,
    time::Duration,
};

use notify::{Config as NotifyConfig, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::{
    sync::{Notify, mpsc, watch},
    task::JoinHandle,
    time,
};
use wokcore_sessions::{
    claude::{ClaudeScanner, ClaudeScannerError},
    codex::{CodexScanSummary, CodexScanner, CodexScannerError, ScanControl, ScanOutcome},
    discovery::{MAX_SESSION_DISCOVERY_HARD_DEADLINE, SessionDiscoverySliceBudget},
    gemini::{GeminiScanner, GeminiScannerError},
    model::{SessionScanControl, SessionScanOutcome, SessionScanSummary, SessionSourceScanSummary},
};
use wokcore_storage::{
    MAX_SESSION_BATCH_ROWS, SessionSourceErrorCode, SessionSourceKind, SessionSourceStatus,
    StateStore, StateStoreWriterClient, StorageError,
};

pub const DEFAULT_SCANNER_WORKERS: usize = 2;
pub const MAX_SCANNER_WORKERS: usize = 4;
pub const ENUMERATION_SLICE_ENTRIES: usize = 256;
pub const MAX_ENUMERATION_SLICE_ENTRIES: usize = 1_024;
pub const ENUMERATION_SLICE_TIME: Duration = Duration::from_millis(25);
pub const MAX_ENUMERATION_SLICE_TIME: Duration = Duration::from_millis(100);
pub const FALLBACK_SCAN_INTERVAL: Duration = Duration::from_secs(5 * 60);
const DEFAULT_NOTIFICATION_DEBOUNCE: Duration = Duration::from_millis(250);
const ALL_KINDS: u8 = 0b111;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionKind {
    Codex,
    Claude,
    Gemini,
}

impl SessionKind {
    const ALL: [Self; 3] = [Self::Codex, Self::Claude, Self::Gemini];

    const fn index(self) -> usize {
        match self {
            Self::Codex => 0,
            Self::Claude => 1,
            Self::Gemini => 2,
        }
    }

    const fn bit(self) -> u8 {
        1 << self.index()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexPhase {
    Starting,
    Scanning,
    Idle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceIndexStatus {
    pub kind: SessionKind,
    pub status: SessionSourceStatus,
    pub error_code: Option<SessionSourceErrorCode>,
    pub last_transition_at: Option<String>,
}

impl SourceIndexStatus {
    fn undiscovered(kind: SessionKind) -> Self {
        Self {
            kind,
            status: SessionSourceStatus::Undiscovered,
            error_code: None,
            last_transition_at: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexStatus {
    pub phase: IndexPhase,
    pub sources: [SourceIndexStatus; 3],
}

impl Default for IndexStatus {
    fn default() -> Self {
        Self {
            phase: IndexPhase::Starting,
            sources: [
                SourceIndexStatus::undiscovered(SessionKind::Codex),
                SourceIndexStatus::undiscovered(SessionKind::Claude),
                SourceIndexStatus::undiscovered(SessionKind::Gemini),
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScanSliceBudget {
    pub maximum_entries: usize,
    pub maximum_duration: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanRootObservation {
    Readable,
    Missing,
    Unreadable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanFileObservation {
    pub source_key: String,
    pub current_generation_visible: bool,
    pub status: SessionSourceStatus,
    pub error_code: Option<SessionSourceErrorCode>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanSliceReport {
    pub complete: bool,
    pub transition_at: String,
    pub root: ScanRootObservation,
    pub files: Vec<ScanFileObservation>,
}

pub trait SessionScanBackend: Send + Sync + 'static {
    fn scan_slice(&self, kind: SessionKind, budget: ScanSliceBudget) -> ScanSliceReport;
}

pub trait ScanTimestampSource: Send + Sync + 'static {
    fn now(&self) -> Option<String>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRootPaths {
    pub codex: PathBuf,
    pub claude: PathBuf,
    pub gemini: PathBuf,
}

impl SessionRootPaths {
    pub fn from_home(home: impl AsRef<Path>) -> Self {
        let home = home.as_ref();
        Self {
            codex: home.join(".codex"),
            claude: home.join(".claude"),
            gemini: home.join(".gemini"),
        }
    }

    pub fn discover() -> Option<Self> {
        let home = if cfg!(windows) {
            std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
        } else {
            std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
        }?;
        Some(Self::from_home(PathBuf::from(home)))
    }

    fn for_kind(&self, kind: SessionKind) -> &Path {
        match kind {
            SessionKind::Codex => &self.codex,
            SessionKind::Claude => &self.claude,
            SessionKind::Gemini => &self.gemini,
        }
    }
}

pub struct ProductionSessionScanBackend {
    roots: SessionRootPaths,
    state_path: PathBuf,
    domain_key: [u8; 32],
    clock: Arc<dyn ScanTimestampSource>,
    state_writer: Option<StateStoreWriterClient>,
    state: Mutex<StateStore>,
    codex: Mutex<Option<CodexScanner>>,
    claude: Mutex<Option<ClaudeScanner>>,
    gemini: Mutex<Option<GeminiScanner>>,
}

impl ProductionSessionScanBackend {
    pub fn open(
        roots: SessionRootPaths,
        state_path: impl AsRef<Path>,
        domain_key: [u8; 32],
        clock: Arc<dyn ScanTimestampSource>,
    ) -> Result<Self, StorageError> {
        Self::open_internal(roots, state_path, domain_key, clock, None)
    }

    pub fn open_with_writer(
        roots: SessionRootPaths,
        state_path: impl AsRef<Path>,
        domain_key: [u8; 32],
        clock: Arc<dyn ScanTimestampSource>,
        state_writer: StateStoreWriterClient,
    ) -> Result<Self, StorageError> {
        Self::open_internal(roots, state_path, domain_key, clock, Some(state_writer))
    }

    fn open_internal(
        roots: SessionRootPaths,
        state_path: impl AsRef<Path>,
        domain_key: [u8; 32],
        clock: Arc<dyn ScanTimestampSource>,
        state_writer: Option<StateStoreWriterClient>,
    ) -> Result<Self, StorageError> {
        let state_path = state_path.as_ref().to_path_buf();
        let state = if state_writer.is_some() {
            StateStore::open_live_reader(&state_path)?
        } else {
            StateStore::open(&state_path)?
        };
        let _ = state.health()?;
        Ok(Self {
            roots,
            state_path,
            domain_key,
            clock,
            state_writer,
            state: Mutex::new(state),
            codex: Mutex::new(None),
            claude: Mutex::new(None),
            gemini: Mutex::new(None),
        })
    }

    fn transition_at(&self) -> String {
        self.clock
            .now()
            .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_owned())
    }

    fn current_generation_visible(&self, kind: SessionKind) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut after = None;
        loop {
            let Ok(page) = state.load_session_sources_page(after.as_ref(), MAX_SESSION_BATCH_ROWS)
            else {
                return false;
            };
            if page.items.iter().any(|source| {
                source.source_kind == storage_kind(kind) && source.current_generation.is_some()
            }) {
                return true;
            }
            let Some(next) = page.next_page_key else {
                return false;
            };
            after = Some(next);
        }
    }

    fn root_failure_report(&self, kind: SessionKind, transition_at: String) -> ScanSliceReport {
        let root = match std::fs::metadata(self.roots.for_kind(kind)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ScanRootObservation::Missing
            }
            _ => ScanRootObservation::Unreadable,
        };
        let error_code = match root {
            ScanRootObservation::Missing => SessionSourceErrorCode::SourceRootMissing,
            ScanRootObservation::Unreadable => SessionSourceErrorCode::SourceRootUnreadable,
            ScanRootObservation::Readable => unreachable!(),
        };
        ScanSliceReport {
            complete: true,
            transition_at,
            root,
            files: self
                .current_generation_visible(kind)
                .then(|| ScanFileObservation {
                    source_key: String::new(),
                    current_generation_visible: true,
                    status: SessionSourceStatus::Stale,
                    error_code: Some(error_code),
                })
                .into_iter()
                .collect(),
        }
    }
}

impl fmt::Debug for ProductionSessionScanBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProductionSessionScanBackend([redacted])")
    }
}

impl SessionScanBackend for ProductionSessionScanBackend {
    fn scan_slice(&self, kind: SessionKind, budget: ScanSliceBudget) -> ScanSliceReport {
        let transition_at = self.transition_at();
        let root = self.roots.for_kind(kind);
        let discovery_budget = match SessionDiscoverySliceBudget::new(
            budget.maximum_entries,
            budget.maximum_duration,
            MAX_SESSION_DISCOVERY_HARD_DEADLINE,
        ) {
            Ok(budget) => budget,
            Err(_) => {
                return scanner_error_report(
                    transition_at,
                    self.current_generation_visible(kind),
                    SessionSourceErrorCode::SourceReplayLimit,
                );
            }
        };
        match kind {
            SessionKind::Codex => {
                let mut scanner = self
                    .codex
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if scanner.is_none() {
                    let opened = match self.state_writer.as_ref() {
                        Some(writer) => CodexScanner::open_with_writer(
                            root,
                            &self.state_path,
                            self.domain_key,
                            writer.clone(),
                        ),
                        None => CodexScanner::open(root, &self.state_path, self.domain_key),
                    };
                    match opened {
                        Ok(opened) => *scanner = Some(opened),
                        Err(CodexScannerError::Root) => {
                            return self.root_failure_report(kind, transition_at);
                        }
                        Err(error) => {
                            return scanner_error_report(
                                transition_at,
                                self.current_generation_visible(kind),
                                map_codex_error(&error),
                            );
                        }
                    }
                }
                match scanner
                    .as_mut()
                    .expect("a scanner slot was initialized")
                    .scan_slice(
                        &transition_at,
                        ScanControl {
                            stop_after_committed_batches: Some(1),
                        },
                        discovery_budget,
                    ) {
                    Ok(summary) => codex_report(transition_at, summary),
                    Err(error) => scanner_error_report(
                        transition_at,
                        self.current_generation_visible(kind),
                        map_codex_error(&error),
                    ),
                }
            }
            SessionKind::Claude => {
                let mut scanner = self
                    .claude
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if scanner.is_none() {
                    let opened = match self.state_writer.as_ref() {
                        Some(writer) => ClaudeScanner::open_with_writer(
                            root,
                            &self.state_path,
                            self.domain_key,
                            writer.clone(),
                        ),
                        None => ClaudeScanner::open(root, &self.state_path, self.domain_key),
                    };
                    match opened {
                        Ok(opened) => *scanner = Some(opened),
                        Err(ClaudeScannerError::Root) => {
                            return self.root_failure_report(kind, transition_at);
                        }
                        Err(error) => {
                            return scanner_error_report(
                                transition_at,
                                self.current_generation_visible(kind),
                                map_claude_error(&error),
                            );
                        }
                    }
                }
                match scanner
                    .as_mut()
                    .expect("a scanner slot was initialized")
                    .scan_slice(
                        &transition_at,
                        SessionScanControl {
                            stop_after_committed_batches: Some(1),
                        },
                        discovery_budget,
                    ) {
                    Ok(summary) => common_report(transition_at, summary),
                    Err(error) => scanner_error_report(
                        transition_at,
                        self.current_generation_visible(kind),
                        map_claude_error(&error),
                    ),
                }
            }
            SessionKind::Gemini => {
                let mut scanner = self
                    .gemini
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if scanner.is_none() {
                    let opened = match self.state_writer.as_ref() {
                        Some(writer) => GeminiScanner::open_with_writer(
                            root,
                            &self.state_path,
                            self.domain_key,
                            writer.clone(),
                        ),
                        None => GeminiScanner::open(root, &self.state_path, self.domain_key),
                    };
                    match opened {
                        Ok(opened) => *scanner = Some(opened),
                        Err(GeminiScannerError::Root) => {
                            return self.root_failure_report(kind, transition_at);
                        }
                        Err(error) => {
                            return scanner_error_report(
                                transition_at,
                                self.current_generation_visible(kind),
                                map_gemini_error(&error),
                            );
                        }
                    }
                }
                match scanner
                    .as_mut()
                    .expect("a scanner slot was initialized")
                    .scan_slice(
                        &transition_at,
                        SessionScanControl {
                            stop_after_committed_batches: Some(1),
                        },
                        discovery_budget,
                    ) {
                    Ok(summary) => common_report(transition_at, summary),
                    Err(error) => scanner_error_report(
                        transition_at,
                        self.current_generation_visible(kind),
                        map_gemini_error(&error),
                    ),
                }
            }
        }
    }
}

fn codex_report(transition_at: String, summary: CodexScanSummary) -> ScanSliceReport {
    ScanSliceReport {
        complete: summary.outcome == ScanOutcome::Complete,
        transition_at,
        root: ScanRootObservation::Readable,
        files: summary
            .sources
            .into_iter()
            .map(|source| ScanFileObservation {
                source_key: source.source_key,
                current_generation_visible: source.session_key.is_some(),
                status: source.status,
                error_code: source.error_code,
            })
            .collect(),
    }
}

fn common_report(transition_at: String, summary: SessionScanSummary) -> ScanSliceReport {
    ScanSliceReport {
        complete: summary.outcome == SessionScanOutcome::Complete,
        transition_at,
        root: ScanRootObservation::Readable,
        files: summary
            .sources
            .into_iter()
            .map(common_file_observation)
            .collect(),
    }
}

fn common_file_observation(source: SessionSourceScanSummary) -> ScanFileObservation {
    ScanFileObservation {
        source_key: source.source_key,
        current_generation_visible: source.session_key.is_some(),
        status: source.status,
        error_code: source.error_code,
    }
}

fn scanner_error_report(
    transition_at: String,
    current_generation_visible: bool,
    error_code: SessionSourceErrorCode,
) -> ScanSliceReport {
    ScanSliceReport {
        complete: true,
        transition_at,
        root: ScanRootObservation::Readable,
        files: vec![ScanFileObservation {
            source_key: String::new(),
            current_generation_visible,
            status: if is_resource_error(error_code) {
                SessionSourceStatus::ResourceLimited
            } else if current_generation_visible {
                SessionSourceStatus::Stale
            } else {
                SessionSourceStatus::Unavailable
            },
            error_code: Some(error_code),
        }],
    }
}

fn map_codex_error(error: &CodexScannerError) -> SessionSourceErrorCode {
    match error {
        CodexScannerError::Storage(StorageError::ReplaySignatureLimitExceeded)
        | CodexScannerError::ReplayLimit => SessionSourceErrorCode::SourceReplayLimit,
        CodexScannerError::Discovery(wokcore_sessions::discovery::DiscoveryError::Unsafe) => {
            SessionSourceErrorCode::SourceEntryUnsafe
        }
        CodexScannerError::Discovery(wokcore_sessions::discovery::DiscoveryError::Limit)
        | CodexScannerError::RecordTooLarge => SessionSourceErrorCode::SourceRecordTooLarge,
        CodexScannerError::Discovery(wokcore_sessions::discovery::DiscoveryError::Failed)
        | CodexScannerError::Read => SessionSourceErrorCode::SourceIoFailed,
        CodexScannerError::Parse => SessionSourceErrorCode::SourceParseInvalid,
        CodexScannerError::ReplayInconsistent => SessionSourceErrorCode::SourceReplayInconsistent,
        CodexScannerError::CleanupPending => SessionSourceErrorCode::SourceCandidateInterrupted,
        CodexScannerError::Root => SessionSourceErrorCode::SourceRootUnreadable,
        CodexScannerError::Storage(_) => SessionSourceErrorCode::SourceIoFailed,
    }
}

fn map_claude_error(error: &ClaudeScannerError) -> SessionSourceErrorCode {
    match error {
        ClaudeScannerError::Discovery => SessionSourceErrorCode::SourceEntryUnsafe,
        ClaudeScannerError::Read => SessionSourceErrorCode::SourceIoFailed,
        ClaudeScannerError::Parse => SessionSourceErrorCode::SourceParseInvalid,
        ClaudeScannerError::ResourceLimit => SessionSourceErrorCode::SourceRecordTooLarge,
        ClaudeScannerError::CleanupPending => SessionSourceErrorCode::SourceCandidateInterrupted,
        ClaudeScannerError::Root => SessionSourceErrorCode::SourceRootUnreadable,
        ClaudeScannerError::Storage(_) => SessionSourceErrorCode::SourceIoFailed,
    }
}

fn map_gemini_error(error: &GeminiScannerError) -> SessionSourceErrorCode {
    match error {
        GeminiScannerError::Discovery => SessionSourceErrorCode::SourceEntryUnsafe,
        GeminiScannerError::Read => SessionSourceErrorCode::SourceIoFailed,
        GeminiScannerError::Parse => SessionSourceErrorCode::SourceParseInvalid,
        GeminiScannerError::ResourceLimit => SessionSourceErrorCode::SourceRecordTooLarge,
        GeminiScannerError::CleanupPending => SessionSourceErrorCode::SourceCandidateInterrupted,
        GeminiScannerError::Root => SessionSourceErrorCode::SourceRootUnreadable,
        GeminiScannerError::Storage(_) => SessionSourceErrorCode::SourceIoFailed,
    }
}

const fn storage_kind(kind: SessionKind) -> SessionSourceKind {
    match kind {
        SessionKind::Codex => SessionSourceKind::Codex,
        SessionKind::Claude => SessionSourceKind::Claude,
        SessionKind::Gemini => SessionSourceKind::Gemini,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchedulerConfig {
    pub workers: usize,
    pub enumeration_slice_entries: usize,
    pub enumeration_slice_time: Duration,
    pub notification_debounce: Duration,
    pub fallback_interval: Duration,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            workers: DEFAULT_SCANNER_WORKERS,
            enumeration_slice_entries: ENUMERATION_SLICE_ENTRIES,
            enumeration_slice_time: ENUMERATION_SLICE_TIME,
            notification_debounce: DEFAULT_NOTIFICATION_DEBOUNCE,
            fallback_interval: FALLBACK_SCAN_INTERVAL,
        }
    }
}

impl SchedulerConfig {
    fn validate(self) -> Result<Self, SchedulerError> {
        if self.workers == 0
            || self.workers > MAX_SCANNER_WORKERS
            || self.enumeration_slice_entries == 0
            || self.enumeration_slice_entries > MAX_ENUMERATION_SLICE_ENTRIES
            || self.enumeration_slice_time.is_zero()
            || self.enumeration_slice_time > MAX_ENUMERATION_SLICE_TIME
            || self.notification_debounce.is_zero()
            || self.fallback_interval.is_zero()
        {
            return Err(SchedulerError::InvalidConfig);
        }
        Ok(self)
    }

    const fn slice_budget(self) -> ScanSliceBudget {
        ScanSliceBudget {
            maximum_entries: self.enumeration_slice_entries,
            maximum_duration: self.enumeration_slice_time,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SchedulerError {
    #[error("Session scheduler configuration is invalid")]
    InvalidConfig,
    #[error("Session scheduler is already running")]
    AlreadyStarted,
    #[error("Session scheduler task failed")]
    TaskFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationOutcome {
    Scheduled,
    Closed,
}

#[derive(Clone)]
pub struct SchedulerHandle {
    status: Arc<RwLock<IndexStatus>>,
    notified_kinds: Arc<AtomicU8>,
    notification: Arc<Notify>,
    closed: Arc<AtomicU8>,
}

impl SchedulerHandle {
    pub fn status(&self) -> IndexStatus {
        self.status
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn notify(&self, kind: SessionKind) -> NotificationOutcome {
        if self.closed.load(Ordering::Acquire) != 0 {
            return NotificationOutcome::Closed;
        }
        self.notified_kinds.fetch_or(kind.bit(), Ordering::AcqRel);
        self.notification.notify_one();
        NotificationOutcome::Scheduled
    }
}

impl fmt::Debug for SchedulerHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SchedulerHandle([redacted])")
    }
}

pub struct PreparedScheduler {
    backend: Arc<dyn SessionScanBackend>,
    config: SchedulerConfig,
    handle: SchedulerHandle,
    watcher: Option<SessionFilesystemWatcher>,
    started: bool,
}

impl PreparedScheduler {
    pub fn new(
        backend: Arc<dyn SessionScanBackend>,
        config: SchedulerConfig,
    ) -> Result<(SchedulerHandle, Self), SchedulerError> {
        let config = config.validate()?;
        let handle = SchedulerHandle {
            status: Arc::new(RwLock::new(IndexStatus::default())),
            notified_kinds: Arc::new(AtomicU8::new(0)),
            notification: Arc::new(Notify::new()),
            closed: Arc::new(AtomicU8::new(0)),
        };
        Ok((
            handle.clone(),
            Self {
                backend,
                config,
                handle,
                watcher: None,
                started: false,
            },
        ))
    }

    pub fn with_filesystem_notifications(mut self, roots: SessionRootPaths) -> Self {
        self.watcher = SessionFilesystemWatcher::new(roots, self.handle.clone());
        self
    }

    pub fn start_after_readiness(mut self) -> Result<RunningScheduler, SchedulerError> {
        if self.started {
            return Err(SchedulerError::AlreadyStarted);
        }
        self.started = true;
        set_phase(&self.handle.status, IndexPhase::Idle);
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let backend = Arc::clone(&self.backend);
        let config = self.config;
        let handle = self.handle.clone();
        let task = tokio::spawn(async move {
            run_scheduler(backend, config, handle, shutdown_receiver).await;
        });
        Ok(RunningScheduler {
            handle: self.handle,
            shutdown,
            task: Some(task),
            watcher: self.watcher,
        })
    }
}

impl fmt::Debug for PreparedScheduler {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PreparedScheduler([redacted])")
    }
}

pub struct RunningScheduler {
    handle: SchedulerHandle,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
    watcher: Option<SessionFilesystemWatcher>,
}

impl RunningScheduler {
    pub fn handle(&self) -> SchedulerHandle {
        self.handle.clone()
    }

    pub async fn shutdown(mut self) -> Result<(), SchedulerError> {
        self.shutdown.send_replace(true);
        self.handle.closed.store(1, Ordering::Release);
        self.handle.notification.notify_waiters();
        drop(self.watcher.take());
        let Some(task) = self.task.take() else {
            return Err(SchedulerError::TaskFailed);
        };
        task.await.map_err(|_| SchedulerError::TaskFailed)
    }
}

impl Drop for RunningScheduler {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
        self.handle.closed.store(1, Ordering::Release);
        self.handle.notification.notify_waiters();
        drop(self.watcher.take());
    }
}

struct SessionFilesystemWatcher {
    watcher: RecommendedWatcher,
}

impl SessionFilesystemWatcher {
    fn new(roots: SessionRootPaths, handle: SchedulerHandle) -> Option<Self> {
        let watched_roots = [
            (SessionKind::Codex, roots.codex),
            (SessionKind::Claude, roots.claude),
            (SessionKind::Gemini, roots.gemini),
        ];
        let callback_roots = watched_roots.clone();
        let mut watcher = RecommendedWatcher::new(
            move |result: notify::Result<notify::Event>| {
                let Ok(event) = result else {
                    return;
                };
                for (kind, root) in &callback_roots {
                    if event.paths.iter().any(|path| path.starts_with(root)) {
                        let _ = handle.notify(*kind);
                    }
                }
            },
            NotifyConfig::default(),
        )
        .ok()?;
        let mut watching = false;
        for (_, root) in watched_roots {
            if root.is_dir() && watcher.watch(&root, RecursiveMode::Recursive).is_ok() {
                watching = true;
            }
        }
        watching.then_some(Self { watcher })
    }
}

impl fmt::Debug for SessionFilesystemWatcher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = &self.watcher;
        formatter.write_str("SessionFilesystemWatcher([redacted])")
    }
}

struct WorkerQueue {
    state: Mutex<WorkerQueueState>,
    available: Condvar,
}

#[derive(Default)]
struct WorkerQueueState {
    jobs: VecDeque<WorkerJob>,
    stopping: bool,
}

#[derive(Clone, Copy)]
struct WorkerJob {
    kind: SessionKind,
    budget: ScanSliceBudget,
}

struct WorkerResult {
    kind: SessionKind,
    report: ScanSliceReport,
}

struct WorkerPool {
    queue: Arc<WorkerQueue>,
    threads: Vec<thread::JoinHandle<()>>,
}

impl WorkerPool {
    fn new(
        workers: usize,
        backend: Arc<dyn SessionScanBackend>,
        result_sender: mpsc::Sender<WorkerResult>,
    ) -> Self {
        let queue = Arc::new(WorkerQueue {
            state: Mutex::new(WorkerQueueState::default()),
            available: Condvar::new(),
        });
        let mut threads = Vec::with_capacity(workers);
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let backend = Arc::clone(&backend);
            let result_sender = result_sender.clone();
            threads.push(thread::spawn(move || {
                loop {
                    let job = {
                        let mut state = queue
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        while state.jobs.is_empty() && !state.stopping {
                            state = queue
                                .available
                                .wait(state)
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                        }
                        if state.stopping {
                            return;
                        }
                        state.jobs.pop_front()
                    };
                    let Some(job) = job else {
                        continue;
                    };
                    let report = catch_unwind(AssertUnwindSafe(|| {
                        backend.scan_slice(job.kind, job.budget)
                    }))
                    .unwrap_or_else(|_| ScanSliceReport {
                        complete: true,
                        transition_at: "1970-01-01T00:00:00Z".to_owned(),
                        root: ScanRootObservation::Unreadable,
                        files: Vec::new(),
                    });
                    if result_sender
                        .blocking_send(WorkerResult {
                            kind: job.kind,
                            report,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            }));
        }
        Self { queue, threads }
    }

    fn push(&self, job: WorkerJob) {
        let mut state = self
            .queue
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.stopping {
            return;
        }
        state.jobs.push_back(job);
        drop(state);
        self.queue.available.notify_one();
    }

    fn stop(mut self) {
        {
            let mut state = self
                .queue
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.stopping = true;
            state.jobs.clear();
        }
        self.queue.available.notify_all();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

#[derive(Clone)]
struct KindAccumulator {
    transition_at: String,
    root: ScanRootObservation,
    observed_files: usize,
    has_current: bool,
    all_available: bool,
    best_error: Option<(SessionSourceErrorCode, String)>,
}

impl KindAccumulator {
    fn new(report: &ScanSliceReport) -> Self {
        Self {
            transition_at: report.transition_at.clone(),
            root: report.root,
            observed_files: 0,
            has_current: false,
            all_available: true,
            best_error: None,
        }
    }

    fn fold(&mut self, report: &ScanSliceReport, maximum_entries: usize) {
        self.transition_at = report.transition_at.clone();
        self.root = worst_root(self.root, report.root);
        if report.files.len() > maximum_entries {
            self.observe_error(SessionSourceErrorCode::SourceReplayLimit, String::new());
            self.all_available = false;
            return;
        }
        self.observed_files = self.observed_files.saturating_add(report.files.len());
        for file in &report.files {
            self.has_current |= file.current_generation_visible;
            self.all_available &= file.status == SessionSourceStatus::Available;
            if let Some(code) = file.error_code {
                self.observe_error(code, file.source_key.clone());
            }
        }
    }

    fn observe_error(&mut self, code: SessionSourceErrorCode, source_key: String) {
        let replace = self.best_error.as_ref().is_none_or(|(existing, key)| {
            error_priority(code) < error_priority(*existing)
                || (error_priority(code) == error_priority(*existing) && source_key < *key)
        });
        if replace {
            self.best_error = Some((code, source_key));
        }
    }

    fn finish(mut self, kind: SessionKind) -> SourceIndexStatus {
        match self.root {
            ScanRootObservation::Readable => {}
            ScanRootObservation::Missing => {
                self.observe_error(SessionSourceErrorCode::SourceRootMissing, String::new())
            }
            ScanRootObservation::Unreadable => {
                self.observe_error(SessionSourceErrorCode::SourceRootUnreadable, String::new())
            }
        }
        if self.observed_files == 0 && self.best_error.is_none() {
            self.observe_error(SessionSourceErrorCode::SourceSessionsAbsent, String::new());
        }
        let error_code = self.best_error.map(|(code, _)| code);
        let status = if error_code.is_some_and(is_resource_error) {
            SessionSourceStatus::ResourceLimited
        } else if error_code.is_some() && self.has_current {
            SessionSourceStatus::Stale
        } else if error_code.is_some() || !self.all_available {
            SessionSourceStatus::Unavailable
        } else {
            SessionSourceStatus::Available
        };
        SourceIndexStatus {
            kind,
            status,
            error_code,
            last_transition_at: Some(self.transition_at),
        }
    }
}

async fn run_scheduler(
    backend: Arc<dyn SessionScanBackend>,
    config: SchedulerConfig,
    handle: SchedulerHandle,
    mut shutdown: watch::Receiver<bool>,
) {
    let (results, mut result_receiver) = mpsc::channel(MAX_SCANNER_WORKERS);
    let pool = WorkerPool::new(config.workers, backend, results);
    let mut pending = ALL_KINDS;
    let mut in_flight = [false; 3];
    let mut accumulators: [Option<KindAccumulator>; 3] = [None, None, None];
    let mut fallback = time::interval(config.fallback_interval);
    fallback.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let _ = fallback.tick().await;
    let mut debounce = Box::pin(time::sleep(Duration::from_secs(365 * 24 * 60 * 60)));
    let mut debounce_armed = false;
    set_phase(&handle.status, IndexPhase::Scanning);

    loop {
        dispatch_pending(&pool, config, &mut pending, &mut in_flight);
        if pending == 0 && !in_flight.iter().any(|active| *active) {
            set_phase(&handle.status, IndexPhase::Idle);
        } else {
            set_phase(&handle.status, IndexPhase::Scanning);
        }
        if *shutdown.borrow() && !in_flight.iter().any(|active| *active) {
            break;
        }

        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    pending = 0;
                    handle.closed.store(1, Ordering::Release);
                }
            }
            result = result_receiver.recv() => {
                let Some(result) = result else {
                    break;
                };
                let index = result.kind.index();
                in_flight[index] = false;
                let accumulator = accumulators[index]
                    .get_or_insert_with(|| KindAccumulator::new(&result.report));
                accumulator.fold(&result.report, config.enumeration_slice_entries);
                if result.report.complete {
                    let completed = accumulators[index]
                        .take()
                        .expect("scan accumulator exists")
                        .finish(result.kind);
                    apply_source_status(&handle.status, completed);
                } else if !*shutdown.borrow() {
                    pending |= result.kind.bit();
                }
            }
            () = handle.notification.notified(), if !*shutdown.borrow() => {
                debounce.as_mut().reset(time::Instant::now() + config.notification_debounce);
                debounce_armed = true;
            }
            () = &mut debounce, if debounce_armed && !*shutdown.borrow() => {
                pending |= handle.notified_kinds.swap(0, Ordering::AcqRel);
                debounce_armed = false;
            }
            _ = fallback.tick(), if !*shutdown.borrow() => {
                pending |= ALL_KINDS;
            }
        }
    }

    handle.closed.store(1, Ordering::Release);
    pool.stop();
    set_phase(&handle.status, IndexPhase::Idle);
}

fn dispatch_pending(
    pool: &WorkerPool,
    config: SchedulerConfig,
    pending: &mut u8,
    in_flight: &mut [bool; 3],
) {
    let mut active = in_flight.iter().filter(|active| **active).count();
    if active >= config.workers {
        return;
    }
    for kind in SessionKind::ALL {
        if active >= config.workers {
            break;
        }
        let index = kind.index();
        if *pending & kind.bit() == 0 || in_flight[index] {
            continue;
        }
        *pending &= !kind.bit();
        in_flight[index] = true;
        active += 1;
        pool.push(WorkerJob {
            kind,
            budget: config.slice_budget(),
        });
    }
}

fn set_phase(status: &RwLock<IndexStatus>, phase: IndexPhase) {
    status
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .phase = phase;
}

fn apply_source_status(status: &RwLock<IndexStatus>, mut next: SourceIndexStatus) {
    let mut status = status
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let current = &status.sources[next.kind.index()];
    if current.status == next.status && current.error_code == next.error_code {
        next.last_transition_at = current.last_transition_at.clone();
    }
    let index = next.kind.index();
    status.sources[index] = next;
}

const fn worst_root(left: ScanRootObservation, right: ScanRootObservation) -> ScanRootObservation {
    match (left, right) {
        (ScanRootObservation::Unreadable, _) | (_, ScanRootObservation::Unreadable) => {
            ScanRootObservation::Unreadable
        }
        (ScanRootObservation::Missing, _) | (_, ScanRootObservation::Missing) => {
            ScanRootObservation::Missing
        }
        _ => ScanRootObservation::Readable,
    }
}

const fn is_resource_error(code: SessionSourceErrorCode) -> bool {
    matches!(
        code,
        SessionSourceErrorCode::SourceReplayLimit | SessionSourceErrorCode::SourceRecordTooLarge
    )
}

const fn error_priority(code: SessionSourceErrorCode) -> u8 {
    match code {
        SessionSourceErrorCode::SourceReplayLimit => 0,
        SessionSourceErrorCode::SourceRecordTooLarge => 1,
        SessionSourceErrorCode::SourceEntryUnsafe => 2,
        SessionSourceErrorCode::SourceRootUnreadable => 3,
        SessionSourceErrorCode::SourceIoFailed => 4,
        SessionSourceErrorCode::SourceParseInvalid => 5,
        SessionSourceErrorCode::SourceReplayInconsistent => 6,
        SessionSourceErrorCode::SourceReplayParentAmbiguous => 7,
        SessionSourceErrorCode::SourceReplayParentMissing => 8,
        SessionSourceErrorCode::SourceCandidateInterrupted => 9,
        SessionSourceErrorCode::SourceRootMissing => 10,
        SessionSourceErrorCode::SourceSessionsAbsent => 11,
    }
}
