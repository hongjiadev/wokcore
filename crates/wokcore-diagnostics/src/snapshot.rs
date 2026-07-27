use std::{
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Notify, mpsc};
use wokcore_platform::diagnostics::{DiagnosticDirectory, DiagnosticEntry, DiagnosticStoreError};

use crate::event::PreparedDiagnosticEvent;

pub const SNAPSHOT_QUEUE_CAPACITY: usize = 16;
pub const MAX_FAILURE_SNAPSHOTS: usize = 10;
pub const MAX_FAILURE_SNAPSHOT_BYTES: usize = 2 * 1024 * 1024;
pub const SNAPSHOT_COOLDOWN_SECONDS: u64 = 60;
pub const SNAPSHOT_WRITE_BUDGET_DEFAULT: usize = 4 * 1024 * 1024;
pub const SNAPSHOT_WRITE_BUDGET_HARD_MAX: usize = 8 * 1024 * 1024;
const MAX_COOLDOWN_KEYS: usize = 256;
pub const MAX_SNAPSHOT_CAUSAL_EVENTS: usize = 64;
pub const MAX_SNAPSHOT_ERROR_CHAIN: usize = 8;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum SnapshotCause {
    UpstreamFailure,
    ProviderUnavailable,
    ProtocolViolation,
    StorageFailure,
    InternalFailure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotLifecycleState {
    Starting,
    Ready,
    Degraded,
    Stopping,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct SnapshotResourceState {
    memory_pressure: bool,
    storage_pressure: bool,
    durable_queue_depth: u16,
}

impl SnapshotResourceState {
    pub fn new(memory_pressure: bool, storage_pressure: bool, durable_queue_depth: u16) -> Self {
        Self {
            memory_pressure,
            storage_pressure,
            durable_queue_depth,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotErrorCode {
    UpstreamTimeout,
    UpstreamUnavailable,
    InvalidResponse,
    StorageUnavailable,
    InternalInvariant,
    ResourceLimit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct SnapshotRedactionSummary {
    removed_fields: u64,
    truncated_summaries: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct SnapshotConfigurationSummary {
    diagnostics_enabled: bool,
    retention_days: u8,
    segment_mib: u8,
    capability_version: u32,
}

impl SnapshotConfigurationSummary {
    pub fn new(
        diagnostics_enabled: bool,
        retention_days: u8,
        segment_mib: u8,
        capability_version: u32,
    ) -> Result<Self, SnapshotError> {
        if retention_days > 7 || !(1..=4).contains(&segment_mib) || capability_version == 0 {
            return Err(SnapshotError::InvalidPolicy);
        }
        Ok(Self {
            diagnostics_enabled,
            retention_days,
            segment_mib,
            capability_version,
        })
    }
}

impl SnapshotRedactionSummary {
    pub const fn new(removed_fields: u64, truncated_summaries: u16) -> Self {
        Self {
            removed_fields,
            truncated_summaries,
        }
    }
}

#[derive(Clone)]
pub struct FailureSnapshot {
    events: Box<[PreparedDiagnosticEvent]>,
    lifecycle: SnapshotLifecycleState,
    resources: SnapshotResourceState,
    error_chain: Box<[SnapshotErrorCode]>,
    redaction: SnapshotRedactionSummary,
    configuration: SnapshotConfigurationSummary,
    encoded_len: usize,
}

impl FailureSnapshot {
    pub fn new(
        events: Vec<PreparedDiagnosticEvent>,
        lifecycle: SnapshotLifecycleState,
        resources: SnapshotResourceState,
        error_chain: Vec<SnapshotErrorCode>,
        redaction: SnapshotRedactionSummary,
        configuration: SnapshotConfigurationSummary,
    ) -> Result<Self, SnapshotError> {
        if events.is_empty()
            || events.len() > MAX_SNAPSHOT_CAUSAL_EVENTS
            || error_chain.len() > MAX_SNAPSHOT_ERROR_CHAIN
        {
            return Err(SnapshotError::InvalidPolicy);
        }
        let mut previous = 0_u64;
        let mut encoded_len = 1_024_usize.checked_add(1).ok_or(SnapshotError::TooLarge)?;
        for event in &events {
            if event.sequence() <= previous {
                return Err(SnapshotError::InvalidPolicy);
            }
            previous = event.sequence();
            encoded_len = encoded_len
                .checked_add(event.encoded_len())
                .and_then(|length| length.checked_add(1))
                .ok_or(SnapshotError::TooLarge)?;
        }
        if encoded_len > MAX_FAILURE_SNAPSHOT_BYTES {
            return Err(SnapshotError::TooLarge);
        }
        Ok(Self {
            events: events.into_boxed_slice(),
            lifecycle,
            resources,
            error_chain: error_chain.into_boxed_slice(),
            redaction,
            configuration,
            encoded_len,
        })
    }

    fn single(event: PreparedDiagnosticEvent) -> Self {
        Self::new(
            vec![event],
            SnapshotLifecycleState::Degraded,
            SnapshotResourceState::new(false, false, 0),
            Vec::new(),
            SnapshotRedactionSummary::new(0, 0),
            SnapshotConfigurationSummary::new(true, 7, 4, 1)
                .expect("standard snapshot configuration is valid"),
        )
        .expect("one prepared event is a bounded failure snapshot")
    }

    pub const fn encoded_len(&self) -> usize {
        self.encoded_len
    }

    fn encoded_len_for(
        &self,
        cause: SnapshotCause,
        correlation: SnapshotCorrelation,
    ) -> Result<usize, SnapshotError> {
        let mut bytes = snapshot_header(
            cause,
            correlation,
            self.lifecycle,
            self.resources,
            &self.error_chain,
            self.redaction,
            self.configuration,
        )?
        .len()
        .checked_add(1)
        .ok_or(SnapshotError::TooLarge)?;
        for event in &self.events {
            bytes = bytes
                .checked_add(event.encoded_len())
                .and_then(|length| length.checked_add(1))
                .ok_or(SnapshotError::TooLarge)?;
        }
        Ok(bytes)
    }
}

impl fmt::Debug for FailureSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("FailureSnapshot([redacted])")
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct SnapshotCorrelation([u8; 16]);

impl SnapshotCorrelation {
    pub const fn from_u128(value: u128) -> Self {
        Self(value.to_be_bytes())
    }
}

impl fmt::Debug for SnapshotCorrelation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SnapshotCorrelation([redacted])")
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SnapshotError {
    #[error("invalid failure snapshot policy")]
    InvalidPolicy,
    #[error("failure snapshot operation failed")]
    Io,
    #[error("failure snapshot exceeds its byte limit")]
    TooLarge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotPolicy {
    write_budget_per_minute: usize,
}

impl SnapshotPolicy {
    pub const fn standard() -> Self {
        Self {
            write_budget_per_minute: SNAPSHOT_WRITE_BUDGET_DEFAULT,
        }
    }

    pub fn with_write_budget(write_budget_per_minute: usize) -> Result<Self, SnapshotError> {
        if write_budget_per_minute == 0 || write_budget_per_minute > SNAPSHOT_WRITE_BUDGET_HARD_MAX
        {
            return Err(SnapshotError::InvalidPolicy);
        }
        Ok(Self {
            write_budget_per_minute,
        })
    }
}

impl Default for SnapshotPolicy {
    fn default() -> Self {
        Self::standard()
    }
}

pub struct SnapshotRequest {
    cause: SnapshotCause,
    correlation: SnapshotCorrelation,
    snapshot: FailureSnapshot,
    observed_at_seconds: u64,
}

impl SnapshotRequest {
    pub fn new(
        cause: SnapshotCause,
        correlation: SnapshotCorrelation,
        snapshot: impl Into<FailureSnapshot>,
        observed_at_seconds: u64,
    ) -> Self {
        Self {
            cause,
            correlation,
            snapshot: snapshot.into(),
            observed_at_seconds,
        }
    }

    pub fn with_correlation(
        cause: SnapshotCause,
        correlation: SnapshotCorrelation,
        snapshot: impl Into<FailureSnapshot>,
        observed_at_seconds: u64,
    ) -> Self {
        Self::new(cause, correlation, snapshot, observed_at_seconds)
    }
}

impl From<PreparedDiagnosticEvent> for FailureSnapshot {
    fn from(event: PreparedDiagnosticEvent) -> Self {
        Self::single(event)
    }
}

impl fmt::Debug for SnapshotRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SnapshotRequest([redacted])")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotRequestOutcome {
    Accepted,
    DroppedFull,
    DroppedClosed,
}

#[derive(Default)]
struct SnapshotMetricAtoms {
    queue_full: AtomicU64,
    queue_closed: AtomicU64,
    cooldown_suppressed: AtomicU64,
    budget_suppressed: AtomicU64,
    written: AtomicU64,
    io_errors: AtomicU64,
}

#[derive(Clone, Default)]
pub struct SnapshotMetrics {
    atoms: Arc<SnapshotMetricAtoms>,
}

impl SnapshotMetrics {
    pub fn queue_full(&self) -> u64 {
        self.atoms.queue_full.load(Ordering::Relaxed)
    }

    pub fn queue_closed(&self) -> u64 {
        self.atoms.queue_closed.load(Ordering::Relaxed)
    }

    pub fn cooldown_suppressed(&self) -> u64 {
        self.atoms.cooldown_suppressed.load(Ordering::Relaxed)
    }

    pub fn budget_suppressed(&self) -> u64 {
        self.atoms.budget_suppressed.load(Ordering::Relaxed)
    }

    pub fn written(&self) -> u64 {
        self.atoms.written.load(Ordering::Relaxed)
    }

    pub fn io_errors(&self) -> u64 {
        self.atoms.io_errors.load(Ordering::Relaxed)
    }
}

impl fmt::Debug for SnapshotMetrics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SnapshotMetrics")
            .field("queue_full", &self.queue_full())
            .field("queue_closed", &self.queue_closed())
            .field("cooldown_suppressed", &self.cooldown_suppressed())
            .field("budget_suppressed", &self.budget_suppressed())
            .field("written", &self.written())
            .field("io_errors", &self.io_errors())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SnapshotSuppressionSummary {
    queue_full: u64,
    queue_closed: u64,
    cooldown_suppressed: u64,
    budget_suppressed: u64,
    io_errors: u64,
}

impl SnapshotSuppressionSummary {
    pub const fn queue_full(self) -> u64 {
        self.queue_full
    }

    pub const fn queue_closed(self) -> u64 {
        self.queue_closed
    }

    pub const fn cooldown_suppressed(self) -> u64 {
        self.cooldown_suppressed
    }

    pub const fn budget_suppressed(self) -> u64 {
        self.budget_suppressed
    }

    pub const fn io_errors(self) -> u64 {
        self.io_errors
    }

    pub const fn total(self) -> u64 {
        self.queue_full
            .saturating_add(self.queue_closed)
            .saturating_add(self.cooldown_suppressed)
            .saturating_add(self.budget_suppressed)
            .saturating_add(self.io_errors)
    }
}

#[derive(Clone)]
pub struct SnapshotRecorder {
    sender: mpsc::Sender<SnapshotRequest>,
    metrics: SnapshotMetrics,
    pending_suppressions: Arc<SnapshotMetricAtoms>,
    shutdown: Arc<SnapshotShutdownState>,
}

impl SnapshotRecorder {
    pub fn new(root: impl AsRef<Path>) -> (Self, SnapshotOwner) {
        Self::with_policy(root, SnapshotPolicy::standard())
    }

    pub fn with_policy(root: impl AsRef<Path>, policy: SnapshotPolicy) -> (Self, SnapshotOwner) {
        let (sender, receiver) = mpsc::channel(SNAPSHOT_QUEUE_CAPACITY);
        let metrics = SnapshotMetrics::default();
        let pending_suppressions = Arc::new(SnapshotMetricAtoms::default());
        let shutdown = Arc::new(SnapshotShutdownState::default());
        (
            Self {
                sender,
                metrics: metrics.clone(),
                pending_suppressions: Arc::clone(&pending_suppressions),
                shutdown: Arc::clone(&shutdown),
            },
            SnapshotOwner {
                receiver,
                metrics,
                pending_suppressions,
                root: root.as_ref().to_path_buf(),
                directory: None,
                policy,
                cooldowns: Vec::with_capacity(MAX_COOLDOWN_KEYS),
                budget_clock: 0,
                budget_buckets: Vec::with_capacity(60),
                next_snapshot: 1,
                shutdown,
                #[cfg(test)]
                fail_completed_after_commit: false,
            },
        )
    }

    pub fn try_request(&self, request: SnapshotRequest) -> SnapshotRequestOutcome {
        if self.shutdown.requested.load(Ordering::Acquire) {
            saturating_increment(&self.metrics.atoms.queue_closed);
            saturating_increment(&self.pending_suppressions.queue_closed);
            return SnapshotRequestOutcome::DroppedClosed;
        }
        match self.sender.try_send(request) {
            Ok(()) => SnapshotRequestOutcome::Accepted,
            Err(mpsc::error::TrySendError::Full(_)) => {
                saturating_increment(&self.metrics.atoms.queue_full);
                saturating_increment(&self.pending_suppressions.queue_full);
                SnapshotRequestOutcome::DroppedFull
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                saturating_increment(&self.metrics.atoms.queue_closed);
                saturating_increment(&self.pending_suppressions.queue_closed);
                SnapshotRequestOutcome::DroppedClosed
            }
        }
    }

    pub fn metrics(&self) -> SnapshotMetrics {
        self.metrics.clone()
    }
}

impl fmt::Debug for SnapshotRecorder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SnapshotRecorder([redacted])")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotProcessOutcome {
    Idle,
    Written,
    WrittenCleanupDeferred,
    Suppressed,
}

impl SnapshotProcessOutcome {
    pub const fn written(self) -> bool {
        matches!(self, Self::Written | Self::WrittenCleanupDeferred)
    }

    pub const fn suppressed(self) -> bool {
        matches!(self, Self::Suppressed)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct SnapshotKey {
    cause: SnapshotCause,
    request: SnapshotCorrelation,
}

pub struct SnapshotOwner {
    receiver: mpsc::Receiver<SnapshotRequest>,
    metrics: SnapshotMetrics,
    pending_suppressions: Arc<SnapshotMetricAtoms>,
    root: PathBuf,
    directory: Option<DiagnosticDirectory>,
    policy: SnapshotPolicy,
    cooldowns: Vec<(SnapshotKey, u64)>,
    budget_clock: u64,
    budget_buckets: Vec<(u64, usize)>,
    next_snapshot: u64,
    shutdown: Arc<SnapshotShutdownState>,
    #[cfg(test)]
    fail_completed_after_commit: bool,
}

#[derive(Default)]
struct SnapshotShutdownState {
    requested: AtomicBool,
    wake: Notify,
}

#[derive(Clone)]
pub struct SnapshotShutdown {
    state: Arc<SnapshotShutdownState>,
}

impl SnapshotShutdown {
    pub fn request(&self) {
        self.state.requested.store(true, Ordering::Release);
        self.state.wake.notify_one();
    }
}

impl fmt::Debug for SnapshotShutdown {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SnapshotShutdown([redacted])")
    }
}

impl SnapshotOwner {
    pub fn shutdown_handle(&self) -> SnapshotShutdown {
        SnapshotShutdown {
            state: Arc::clone(&self.shutdown),
        }
    }

    pub fn drain_suppression_summary(&self) -> Option<SnapshotSuppressionSummary> {
        let summary = SnapshotSuppressionSummary {
            queue_full: self
                .pending_suppressions
                .queue_full
                .swap(0, Ordering::AcqRel),
            queue_closed: self
                .pending_suppressions
                .queue_closed
                .swap(0, Ordering::AcqRel),
            cooldown_suppressed: self
                .pending_suppressions
                .cooldown_suppressed
                .swap(0, Ordering::AcqRel),
            budget_suppressed: self
                .pending_suppressions
                .budget_suppressed
                .swap(0, Ordering::AcqRel),
            io_errors: self
                .pending_suppressions
                .io_errors
                .swap(0, Ordering::AcqRel),
        };
        (summary.total() != 0).then_some(summary)
    }

    pub fn try_process_next(&mut self) -> Result<SnapshotProcessOutcome, SnapshotError> {
        let request = match self.receiver.try_recv() {
            Ok(request) => request,
            Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                return Ok(SnapshotProcessOutcome::Idle);
            }
        };
        self.process(request)
    }

    pub async fn run(mut self) {
        let mut stopping = false;
        loop {
            if !stopping && self.shutdown.requested.load(Ordering::Acquire) {
                self.receiver.close();
                stopping = true;
            }
            tokio::select! {
                biased;
                request = self.receiver.recv() => {
                    let Some(request) = request else {
                        return;
                    };
                    let _ = self.process(request);
                }
                () = self.shutdown.wake.notified(), if !stopping => {}
            }
        }
    }

    fn process(
        &mut self,
        request: SnapshotRequest,
    ) -> Result<SnapshotProcessOutcome, SnapshotError> {
        let key = SnapshotKey {
            cause: request.cause,
            request: request.correlation,
        };
        if self
            .cooldowns
            .iter()
            .find(|(candidate, _)| *candidate == key)
            .is_some_and(|(_, last)| {
                request.observed_at_seconds.saturating_sub(*last) < SNAPSHOT_COOLDOWN_SECONDS
            })
        {
            saturating_increment(&self.metrics.atoms.cooldown_suppressed);
            saturating_increment(&self.pending_suppressions.cooldown_suppressed);
            return Ok(SnapshotProcessOutcome::Suppressed);
        }

        let snapshot_bytes = request
            .snapshot
            .encoded_len_for(request.cause, request.correlation)?;
        if snapshot_bytes > MAX_FAILURE_SNAPSHOT_BYTES {
            return Err(SnapshotError::TooLarge);
        }
        self.budget_clock = self.budget_clock.max(request.observed_at_seconds);
        self.budget_buckets
            .retain(|(second, _)| self.budget_clock.saturating_sub(*second) < 60);
        let current_budget = self
            .budget_buckets
            .iter()
            .fold(0_usize, |total, (_, bytes)| total.saturating_add(*bytes));
        let next_budget = current_budget
            .checked_add(snapshot_bytes)
            .ok_or(SnapshotError::TooLarge)?;
        if next_budget > self.policy.write_budget_per_minute {
            saturating_increment(&self.metrics.atoms.budget_suppressed);
            saturating_increment(&self.pending_suppressions.budget_suppressed);
            return Ok(SnapshotProcessOutcome::Suppressed);
        }

        self.record_cooldown(key, request.observed_at_seconds);
        let cleanup_deferred =
            match self.write_snapshot(&request.snapshot, request.cause, request.correlation) {
                Ok(cleanup_deferred) => cleanup_deferred,
                Err(error) => {
                    saturating_increment(&self.metrics.atoms.io_errors);
                    saturating_increment(&self.pending_suppressions.io_errors);
                    return Err(error);
                }
            };
        Ok(self.account_published(snapshot_bytes, cleanup_deferred))
    }

    fn account_published(
        &mut self,
        snapshot_bytes: usize,
        cleanup_deferred: bool,
    ) -> SnapshotProcessOutcome {
        if let Some((_, bytes)) = self
            .budget_buckets
            .iter_mut()
            .find(|(second, _)| *second == self.budget_clock)
        {
            *bytes = bytes.saturating_add(snapshot_bytes);
        } else {
            self.budget_buckets
                .push((self.budget_clock, snapshot_bytes));
        }
        saturating_increment(&self.metrics.atoms.written);
        if cleanup_deferred {
            saturating_increment(&self.metrics.atoms.io_errors);
            saturating_increment(&self.pending_suppressions.io_errors);
            SnapshotProcessOutcome::WrittenCleanupDeferred
        } else {
            SnapshotProcessOutcome::Written
        }
    }

    fn record_cooldown(&mut self, key: SnapshotKey, observed_at_seconds: u64) {
        if let Some((_, last)) = self
            .cooldowns
            .iter_mut()
            .find(|(candidate, _)| *candidate == key)
        {
            *last = observed_at_seconds;
            return;
        }
        if self.cooldowns.len() == MAX_COOLDOWN_KEYS {
            self.cooldowns.remove(0);
        }
        self.cooldowns.push((key, observed_at_seconds));
    }

    fn write_snapshot(
        &mut self,
        snapshot: &FailureSnapshot,
        cause: SnapshotCause,
        correlation: SnapshotCorrelation,
    ) -> Result<bool, SnapshotError> {
        if self.directory.is_none() {
            self.directory =
                Some(DiagnosticDirectory::open(&self.root).map_err(map_platform_error)?);
        }
        let directory = self.directory.as_ref().ok_or(SnapshotError::Io)?;
        let _ = completed(directory, &mut self.next_snapshot)?;
        let name = OsString::from(format!("snapshot-{:020}.jsonl", self.next_snapshot));
        let mut staged = directory
            .create_staged(
                &name,
                u64::try_from(MAX_FAILURE_SNAPSHOT_BYTES).map_err(|_| SnapshotError::TooLarge)?,
            )
            .map_err(map_platform_error)?;
        let header = snapshot_header(
            cause,
            correlation,
            snapshot.lifecycle,
            snapshot.resources,
            &snapshot.error_chain,
            snapshot.redaction,
            snapshot.configuration,
        )?;
        staged.write_chunk(&header).map_err(map_platform_error)?;
        staged.write_chunk(b"\n").map_err(map_platform_error)?;
        for event in &snapshot.events {
            staged
                .write_chunk(event.encoded())
                .map_err(map_platform_error)?;
            staged.write_chunk(b"\n").map_err(map_platform_error)?;
        }
        drop(staged.commit().map_err(map_platform_error)?);
        self.next_snapshot = self.next_snapshot.saturating_add(1);
        #[cfg(test)]
        if self.fail_completed_after_commit {
            return Ok(true);
        }
        let retained = match completed(directory, &mut self.next_snapshot) {
            Ok(retained) => retained,
            Err(_) => return Ok(true),
        };
        Ok(remove_unretained(directory, &retained).is_err())
    }
}

impl fmt::Debug for SnapshotOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SnapshotOwner([redacted])")
    }
}

fn parse_snapshot_index(name: &str) -> Option<u64> {
    let value = name.strip_prefix("snapshot-")?.strip_suffix(".jsonl")?;
    if value.len() != 20 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok().filter(|index| *index != 0)
}

fn completed(
    directory: &DiagnosticDirectory,
    next_snapshot: &mut u64,
) -> Result<Vec<(u64, DiagnosticEntry)>, SnapshotError> {
    let mut completed = Vec::new();
    let mut after = None::<OsString>;
    loop {
        let page = directory
            .entries_page(after.as_deref(), 128)
            .map_err(map_platform_error)?;
        let next = page.next_after().map(OsString::from);
        for entry in page.into_entries() {
            let Some(name) = entry.name().to_str() else {
                continue;
            };
            let Some(index) = parse_snapshot_index(name) else {
                continue;
            };
            if !owned_snapshot(directory, &entry)? {
                continue;
            }
            *next_snapshot = (*next_snapshot).max(index.saturating_add(1));
            completed.push((index, entry));
            completed.sort_by_key(|(index, _)| *index);
            if completed.len() > MAX_FAILURE_SNAPSHOTS {
                completed.remove(0);
            }
        }
        let Some(next) = next else {
            break;
        };
        after = Some(next);
    }
    Ok(completed)
}

fn remove_unretained(
    directory: &DiagnosticDirectory,
    retained: &[(u64, DiagnosticEntry)],
) -> Result<(), SnapshotError> {
    let mut after = None::<OsString>;
    loop {
        let page = directory
            .entries_page(after.as_deref(), 128)
            .map_err(map_platform_error)?;
        let next = page.next_after().map(OsString::from);
        for entry in page.into_entries() {
            let Some(index) = entry.name().to_str().and_then(parse_snapshot_index) else {
                continue;
            };
            if retained.iter().any(|(kept, _)| *kept == index)
                || !owned_snapshot(directory, &entry)?
            {
                continue;
            }
            directory.remove(&entry).map_err(map_platform_error)?;
        }
        let Some(next) = next else {
            break;
        };
        after = Some(next);
    }
    Ok(())
}

fn owned_snapshot(
    directory: &DiagnosticDirectory,
    entry: &DiagnosticEntry,
) -> Result<bool, SnapshotError> {
    let mut file = match directory.open_read(
        entry,
        u64::try_from(MAX_FAILURE_SNAPSHOT_BYTES).map_err(|_| SnapshotError::TooLarge)?,
    ) {
        Ok(file) => file,
        Err(_) => return Ok(false),
    };
    let length = usize::try_from(file.len()).map_err(|_| SnapshotError::TooLarge)?;
    if length == 0 || length > MAX_FAILURE_SNAPSHOT_BYTES {
        return Ok(false);
    }
    let bytes = file.read_range(0, length).map_err(map_platform_error)?;
    Ok(bytes.len() == length && valid_snapshot_bytes(&bytes))
}

#[derive(Serialize)]
struct SnapshotHeader<'a> {
    schema_version: u8,
    kind: &'static str,
    cause: SnapshotCause,
    correlation: SnapshotCorrelationWire,
    lifecycle: SnapshotLifecycleState,
    resources: SnapshotResourceState,
    error_chain: &'a [SnapshotErrorCode],
    redaction: SnapshotRedactionSummary,
    configuration: SnapshotConfigurationSummary,
}

struct SnapshotCorrelationWire([u8; 16]);

impl Serialize for SnapshotCorrelationWire {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        struct Hex([u8; 16]);
        impl fmt::Display for Hex {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                for byte in self.0 {
                    write!(formatter, "{byte:02x}")?;
                }
                Ok(())
            }
        }
        serializer.collect_str(&Hex(self.0))
    }
}

fn snapshot_header(
    cause: SnapshotCause,
    correlation: SnapshotCorrelation,
    lifecycle: SnapshotLifecycleState,
    resources: SnapshotResourceState,
    error_chain: &[SnapshotErrorCode],
    redaction: SnapshotRedactionSummary,
    configuration: SnapshotConfigurationSummary,
) -> Result<Vec<u8>, SnapshotError> {
    serde_json::to_vec(&SnapshotHeader {
        schema_version: 1,
        kind: "failure_snapshot",
        cause,
        correlation: SnapshotCorrelationWire(correlation.0),
        lifecycle,
        resources,
        error_chain,
        redaction,
        configuration,
    })
    .map_err(|_| SnapshotError::Io)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodeSnapshotHeader {
    schema_version: u8,
    kind: Box<str>,
    cause: Box<str>,
    correlation: Box<str>,
    lifecycle: Box<str>,
    resources: DecodeSnapshotResources,
    error_chain: Vec<Box<str>>,
    redaction: DecodeSnapshotRedaction,
    configuration: DecodeSnapshotConfiguration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodeSnapshotResources {
    memory_pressure: bool,
    storage_pressure: bool,
    durable_queue_depth: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodeSnapshotRedaction {
    removed_fields: u64,
    truncated_summaries: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodeSnapshotConfiguration {
    diagnostics_enabled: bool,
    retention_days: u8,
    segment_mib: u8,
    capability_version: u32,
}

fn valid_snapshot_bytes(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.len() > MAX_FAILURE_SNAPSHOT_BYTES || !bytes.ends_with(b"\n") {
        return false;
    }
    let mut lines = bytes[..bytes.len() - 1].split(|byte| *byte == b'\n');
    let Some(header) = lines.next() else {
        return false;
    };
    if !validate_snapshot_header_line(header) {
        return false;
    }
    let mut count = 0_usize;
    let mut last_sequence = 0_u64;
    for line in lines {
        count = count.saturating_add(1);
        if count > MAX_SNAPSHOT_CAUSAL_EVENTS {
            return false;
        }
        let Ok(event) = crate::event::decode_trusted_prepared_encoding(line) else {
            return false;
        };
        if event.sequence() <= last_sequence {
            return false;
        }
        last_sequence = event.sequence();
    }
    count != 0
}

pub(crate) fn validate_snapshot_header_line(line: &[u8]) -> bool {
    let Ok(header) = serde_json::from_slice::<DecodeSnapshotHeader>(line) else {
        return false;
    };
    if header.schema_version != 1
        || header.kind.as_ref() != "failure_snapshot"
        || !matches!(
            header.cause.as_ref(),
            "upstream_failure"
                | "provider_unavailable"
                | "protocol_violation"
                | "storage_failure"
                | "internal_failure"
        )
        || header.correlation.len() != 32
        || !header
            .correlation
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || !matches!(
            header.lifecycle.as_ref(),
            "starting" | "ready" | "degraded" | "stopping"
        )
        || header.error_chain.len() > MAX_SNAPSHOT_ERROR_CHAIN
        || header.configuration.retention_days > 7
        || !(1..=4).contains(&header.configuration.segment_mib)
        || header.configuration.capability_version == 0
        || !header.error_chain.iter().all(|code| {
            matches!(
                code.as_ref(),
                "upstream_timeout"
                    | "upstream_unavailable"
                    | "invalid_response"
                    | "storage_unavailable"
                    | "internal_invariant"
                    | "resource_limit"
            )
        })
    {
        return false;
    }
    let _typed_resources = (
        header.resources.memory_pressure,
        header.resources.storage_pressure,
        header.resources.durable_queue_depth,
    );
    let _typed_redaction = (
        header.redaction.removed_fields,
        header.redaction.truncated_summaries,
    );
    let _typed_configuration = header.configuration.diagnostics_enabled;
    true
}

fn saturating_increment(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(1))
    });
}

fn map_platform_error(error: DiagnosticStoreError) -> SnapshotError {
    match error {
        DiagnosticStoreError::SizeLimitExceeded => SnapshotError::TooLarge,
        _ => SnapshotError::Io,
    }
}

#[cfg(test)]
mod tests {
    use crate::event::{
        BuildIdentity, CapabilityVersion, DiagnosticComponent, DiagnosticEventCode,
        DiagnosticEventDraft, DiagnosticLevel, EventId, GitCommit, UtcTimestamp, WokcoreVersion,
    };
    use tempfile::tempdir;

    use super::*;

    fn prepared_event() -> PreparedDiagnosticEvent {
        DiagnosticEventDraft::new(
            EventId::parse("018f47a2-4c1d-7a8f-9b2d-000000000001").unwrap(),
            UtcTimestamp::parse("2026-07-26T12:30:00Z").unwrap(),
            DiagnosticLevel::Error,
            DiagnosticComponent::Diagnostics,
            DiagnosticEventCode::RequestFailed,
            BuildIdentity::new(
                WokcoreVersion::parse("0.1.0").unwrap(),
                GitCommit::parse("0123456789abcdef0123456789abcdef01234567").unwrap(),
                1,
                CapabilityVersion::new(3),
            ),
        )
        .prepare_template()
        .unwrap()
        .finalize(1)
        .unwrap()
    }

    #[test]
    fn published_snapshot_cleanup_failure_is_still_charged_and_counted() {
        let directory = tempdir().unwrap();
        let (recorder, mut owner) = SnapshotRecorder::with_policy(
            directory.path(),
            SnapshotPolicy::with_write_budget(100).unwrap(),
        );
        owner.budget_clock = 10;

        assert_eq!(
            owner.account_published(100, true),
            SnapshotProcessOutcome::WrittenCleanupDeferred
        );
        assert_eq!(recorder.metrics().written(), 1);
        assert_eq!(recorder.metrics().io_errors(), 1);
        assert_eq!(
            owner
                .budget_buckets
                .iter()
                .fold(0_usize, |total, (_, bytes)| total.saturating_add(*bytes)),
            100
        );
        assert!(
            owner
                .budget_buckets
                .iter()
                .fold(0_usize, |total, (_, bytes)| total.saturating_add(*bytes))
                .saturating_add(1)
                > owner.policy.write_budget_per_minute
        );
        let summary = owner.drain_suppression_summary().unwrap();
        assert_eq!(summary.io_errors(), 1);
        assert!(owner.drain_suppression_summary().is_none());
    }

    #[test]
    fn completed_failure_after_commit_is_infallible_cleanup_deferred_and_accounted() {
        let directory = tempdir().unwrap();
        let (recorder, mut owner) = SnapshotRecorder::with_policy(
            directory.path(),
            SnapshotPolicy::with_write_budget(SNAPSHOT_WRITE_BUDGET_HARD_MAX).unwrap(),
        );
        owner.next_snapshot = u64::MAX;
        owner.fail_completed_after_commit = true;
        assert_eq!(
            recorder.try_request(SnapshotRequest::new(
                SnapshotCause::InternalFailure,
                SnapshotCorrelation::from_u128(1),
                prepared_event(),
                10,
            )),
            SnapshotRequestOutcome::Accepted
        );

        assert_eq!(
            owner.try_process_next().unwrap(),
            SnapshotProcessOutcome::WrittenCleanupDeferred
        );
        assert_eq!(owner.next_snapshot, u64::MAX);
        assert_eq!(recorder.metrics().written(), 1);
        assert_eq!(recorder.metrics().io_errors(), 1);
        assert!(
            directory
                .path()
                .join("snapshot-18446744073709551615.jsonl")
                .exists()
        );
        assert!(
            owner
                .budget_buckets
                .iter()
                .map(|(_, bytes)| *bytes)
                .sum::<usize>()
                > 0
        );
        assert_eq!(owner.drain_suppression_summary().unwrap().io_errors(), 1);
    }
}
