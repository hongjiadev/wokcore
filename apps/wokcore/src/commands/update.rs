use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use reqwest::{Response, StatusCode, header::LOCATION};
use semver::Version;
use serde_json::{Value, json};
use tempfile::{Builder, TempPath};
use tokio::io::AsyncWriteExt;
use wokcore_platform::{
    DiscoveryRecord, DiscoveryStore, PlatformError,
    update::{
        InstallOutcome, InstallTransaction, MAX_UPDATE_ARTIFACT_BYTES, MAX_UPDATE_MANIFEST_BYTES,
        MAX_UPDATE_SIGNATURE_BYTES, PreparedInstall, UpdateArtifact, UpdateDecision, UpdateError,
        acquire_update_lease, current_target, prepare_install_file, verify_artifact_file,
        verify_manifest,
    },
};

use crate::{
    CommandOutput, ExitCode, PRODUCTION_UPDATE_ORIGIN, RunDependencies, UpdateChild, UpdateSource,
    cli::Update,
};

#[allow(dead_code)]
mod progress;

use progress::{
    DownloadProgressDetails, ProgressDetails, ProgressErrorCode, ProgressEvent, ProgressReporter,
};

use super::{
    client::{ControlClient, ControlClientError},
    status::verify_identity,
    stop::{LifecycleResponse, drain_and_stop, request_cancel_drain, request_drain, request_stop},
    write_json,
};

const UPDATE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const UPDATE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const UPDATE_READ_TIMEOUT: Duration = Duration::from_secs(30);
const UPDATE_PROCESS_TIMEOUT: Duration = Duration::from_secs(10);
const UPDATE_STOP_SETTLE_TIMEOUT: Duration = Duration::from_secs(50);
const UPDATE_PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(25);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OriginalServiceState {
    Running,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LifecyclePreparation {
    Ready(OriginalServiceState),
    ActiveRequestsRemain { count: usize },
}

#[derive(Debug)]
struct PreparedLifecycle {
    outcome: LifecyclePreparation,
    recovery: Option<StopRecoveryGuard>,
}

impl PreparedLifecycle {
    fn plain(outcome: LifecyclePreparation) -> Self {
        Self {
            outcome,
            recovery: None,
        }
    }

    fn guarded(outcome: LifecyclePreparation, recovery: StopRecoveryGuard) -> Self {
        Self {
            outcome,
            recovery: Some(recovery),
        }
    }

    fn outcome(&self) -> LifecyclePreparation {
        self.outcome
    }

    fn disarm(mut self) -> Result<(), ()> {
        match self.recovery.take() {
            Some(recovery) => recovery.disarm(),
            None => Ok(()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VerificationFailure {
    SafeToRollback,
    RecoveryRequired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InstallFailure {
    Failed,
    RecoveryRequired,
    OperationInProgress,
}

impl From<()> for InstallFailure {
    fn from((): ()) -> Self {
        Self::Failed
    }
}

fn rollback_install_failure(error: &UpdateError) -> InstallFailure {
    match error {
        UpdateError::RollbackDurabilityFailed | UpdateError::RecoveryRequired => {
            InstallFailure::RecoveryRequired
        }
        _ => InstallFailure::Failed,
    }
}

trait UpdateInstallTransaction: Send {
    fn commit(self: Box<Self>) -> Result<(), UpdateError>;
    fn rollback(self: Box<Self>) -> Result<(), UpdateError>;
    fn preserve_for_recovery(self: Box<Self>);
}

impl UpdateInstallTransaction for InstallTransaction {
    fn commit(self: Box<Self>) -> Result<(), UpdateError> {
        (*self).commit()
    }

    fn rollback(self: Box<Self>) -> Result<(), UpdateError> {
        (*self).rollback()
    }

    fn preserve_for_recovery(self: Box<Self>) {
        (*self).preserve_for_recovery();
    }
}

trait UpdateTransactionFactory: Send + Sync {
    fn begin(
        &self,
        prepared: PreparedInstall,
    ) -> Result<Box<dyn UpdateInstallTransaction>, UpdateError>;
}

struct PlatformUpdateTransactionFactory;

impl UpdateTransactionFactory for PlatformUpdateTransactionFactory {
    fn begin(
        &self,
        prepared: PreparedInstall,
    ) -> Result<Box<dyn UpdateInstallTransaction>, UpdateError> {
        Ok(Box::new(prepared.begin()?))
    }
}

#[async_trait]
trait UpdateLifecycle: Send + Sync {
    async fn prepare(
        &self,
        reporter: &mut ProgressReporter,
        output: &mut dyn CommandOutput,
        versions: &ProgressDetails,
    ) -> Result<PreparedLifecycle, ()>;

    async fn verify_installed(
        &self,
        version: &Version,
        original: OriginalServiceState,
        reporter: &mut ProgressReporter,
        output: &mut dyn CommandOutput,
        versions: &ProgressDetails,
    ) -> Result<(), VerificationFailure>;

    async fn restore_previous(
        &self,
        version: &Version,
        original: OriginalServiceState,
        reporter: &mut ProgressReporter,
        output: &mut dyn CommandOutput,
        versions: &ProgressDetails,
    ) -> Result<(), ()>;
}

struct ServiceUpdateLifecycle<'a> {
    dependencies: &'a RunDependencies,
    target: PathBuf,
    child_cleanup: Arc<ChildCleanupCoordinator>,
}

#[derive(Debug)]
struct ChildCleanupCoordinator {
    pending: AtomicUsize,
    failed: AtomicBool,
    changed: tokio::sync::Notify,
}

impl ChildCleanupCoordinator {
    fn new() -> Self {
        Self {
            pending: AtomicUsize::new(0),
            failed: AtomicBool::new(false),
            changed: tokio::sync::Notify::new(),
        }
    }

    fn begin(self: &Arc<Self>) -> ChildCleanupLease {
        self.pending.fetch_add(1, Ordering::AcqRel);
        ChildCleanupLease {
            coordinator: Some(self.clone()),
        }
    }

    async fn wait(&self) -> Result<(), ()> {
        loop {
            let changed = self.changed.notified();
            if self.pending.load(Ordering::Acquire) == 0 {
                return (!self.failed.load(Ordering::Acquire))
                    .then_some(())
                    .ok_or(());
            }
            changed.await;
        }
    }
}

struct ChildCleanupLease {
    coordinator: Option<Arc<ChildCleanupCoordinator>>,
}

impl ChildCleanupLease {
    fn finish(mut self, result: Result<(), ()>) {
        let Some(coordinator) = self.coordinator.take() else {
            return;
        };
        release_child_cleanup(coordinator, result.is_err());
    }
}

impl Drop for ChildCleanupLease {
    fn drop(&mut self) {
        let Some(coordinator) = self.coordinator.take() else {
            return;
        };
        release_child_cleanup(coordinator, true);
    }
}

fn release_child_cleanup(coordinator: Arc<ChildCleanupCoordinator>, failed: bool) {
    if failed {
        coordinator.failed.store(true, Ordering::Release);
    }
    {
        let previous = coordinator.pending.fetch_sub(1, Ordering::AcqRel);
        debug_assert_ne!(previous, 0);
    }
    coordinator.changed.notify_waiters();
}

struct ManagedUpdateChild {
    child: Option<Box<dyn UpdateChild>>,
    dependencies: RunDependencies,
    cleanup: Option<ChildCleanupLease>,
}

impl ManagedUpdateChild {
    fn new(
        child: Box<dyn UpdateChild>,
        dependencies: RunDependencies,
        coordinator: &Arc<ChildCleanupCoordinator>,
    ) -> Self {
        Self {
            child: Some(child),
            dependencies,
            cleanup: Some(coordinator.begin()),
        }
    }

    fn pid(&self) -> Option<u32> {
        self.child.as_deref().and_then(|child| child.pid())
    }

    fn detach(&mut self) {
        if let Some(child) = self.child.as_deref_mut() {
            child.detach();
        }
        if let Some(cleanup) = self.cleanup.take() {
            cleanup.finish(Ok(()));
        }
    }

    async fn terminate(&mut self, record: Option<&DiscoveryRecord>) -> Result<(), ()> {
        let child = self.child.as_deref_mut().ok_or(())?;
        terminate_update_child(&self.dependencies, child, record).await?;
        self.detach();
        Ok(())
    }
}

impl Drop for ManagedUpdateChild {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let Some(cleanup) = self.cleanup.take() else {
            return;
        };
        let dependencies = self.dependencies.clone();
        tokio::spawn(async move {
            let result = terminate_update_child(&dependencies, child.as_mut(), None).await;
            if result.is_ok() {
                child.detach();
            }
            drop(child);
            cleanup.finish(result);
        });
    }
}

async fn terminate_update_child(
    dependencies: &RunDependencies,
    child: &mut dyn UpdateChild,
    record: Option<&DiscoveryRecord>,
) -> Result<(), ()> {
    let pid = child.pid();
    let kill_result = child.kill().await;
    if let Some(pid) = pid {
        let deadline = tokio::time::Instant::now() + UPDATE_PROCESS_TIMEOUT;
        while dependencies.process.is_running(pid) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(UPDATE_PROCESS_POLL_INTERVAL).await;
        }
        if dependencies.process.is_running(pid) {
            return Err(());
        }
    } else if kill_result.is_err() {
        return Err(());
    }
    let owned = match record {
        Some(record) => Some(record.clone()),
        None => match read_startup_discovery(&dependencies.paths)? {
            Some(record) if Some(record.pid) == pid => Some(record),
            Some(_) => return Err(()),
            None => None,
        },
    };
    if let Some(owned) = owned {
        let store = DiscoveryStore::new(&dependencies.paths).map_err(|_| ())?;
        let removed = store.remove_if_owned(owned.instance_id).map_err(|_| ())?;
        if !removed && read_startup_discovery(&dependencies.paths)?.is_some() {
            return Err(());
        }
    }
    Ok(())
}

struct DownloadedArtifact {
    file: File,
    _path: TempPath,
}

#[derive(Debug)]
struct StopRecoveryGuard {
    completion: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<Result<(), ()>>>,
    stop_requested: Arc<AtomicBool>,
}

impl StopRecoveryGuard {
    fn new(
        dependencies: RunDependencies,
        target: PathBuf,
        client: ControlClient,
        management: secrecy::SecretString,
        original: DiscoveryRecord,
        child_cleanup: Arc<ChildCleanupCoordinator>,
    ) -> Self {
        let (completion, interrupted) = tokio::sync::oneshot::channel();
        let stop_requested = Arc::new(AtomicBool::new(false));
        let recovery_stop_requested = stop_requested.clone();
        let task = tokio::spawn(async move {
            match interrupted.await {
                Ok(()) => Ok(()),
                Err(_) => {
                    recover_interrupted_stop(
                        dependencies,
                        target,
                        client,
                        management,
                        original,
                        recovery_stop_requested,
                        child_cleanup,
                    )
                    .await
                }
            }
        });
        Self {
            completion: Some(completion),
            task: Some(task),
            stop_requested,
        }
    }

    fn mark_stop_requested(&self) {
        self.stop_requested.store(true, Ordering::Release);
    }

    fn disarm(mut self) -> Result<(), ()> {
        self.completion.take().ok_or(())?.send(()).map_err(|_| ())
    }

    async fn recover(mut self) -> Result<(), ()> {
        drop(self.completion.take());
        self.task.take().ok_or(())?.await.map_err(|_| ())?
    }
}

impl DownloadedArtifact {
    fn file(&self) -> &File {
        &self.file
    }

    #[cfg(test)]
    fn path(&self) -> &Path {
        &self._path
    }
}

impl<'a> ServiceUpdateLifecycle<'a> {
    fn new(dependencies: &'a RunDependencies, target: PathBuf) -> Self {
        Self {
            dependencies,
            target,
            child_cleanup: Arc::new(ChildCleanupCoordinator::new()),
        }
    }

    fn cleanup_stale_discovery(&self) -> Result<(), ()> {
        let store = match DiscoveryStore::new(&self.dependencies.paths) {
            Ok(store) => store,
            Err(PlatformError::Io { source }) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(_) => return Err(()),
        };
        let record = match store.read() {
            Ok(record) => record,
            Err(PlatformError::Io { source }) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(_) => return Err(()),
        };
        if self.dependencies.process.is_running(record.pid) {
            return Err(());
        }
        store
            .remove_if_owned(record.instance_id)
            .map(|_| ())
            .map_err(|_| ())
    }

    async fn wait_for_exit(&self, record: &DiscoveryRecord) -> Result<(), ()> {
        self.wait_for_exit_with_timeout(record, UPDATE_PROCESS_TIMEOUT)
            .await
    }

    async fn wait_for_exit_with_timeout(
        &self,
        record: &DiscoveryRecord,
        timeout: Duration,
    ) -> Result<(), ()> {
        let deadline = tokio::time::Instant::now() + timeout;
        while self.dependencies.process.is_running(record.pid) {
            if tokio::time::Instant::now() >= deadline {
                return Err(());
            }
            tokio::time::sleep(UPDATE_PROCESS_POLL_INTERVAL).await;
        }
        let Some(current) = read_startup_discovery(&self.dependencies.paths)? else {
            return Ok(());
        };
        if current.instance_id != record.instance_id {
            return Err(());
        }
        let store = DiscoveryStore::new(&self.dependencies.paths).map_err(|_| ())?;
        store
            .remove_if_owned(record.instance_id)
            .map_err(|_| ())?
            .then_some(())
            .ok_or(())
    }

    async fn wait_for_ready(&self, pid: u32, version: &Version) -> Result<DiscoveryRecord, ()> {
        let deadline = tokio::time::Instant::now() + UPDATE_PROCESS_TIMEOUT;
        loop {
            if !self.dependencies.process.is_running(pid) {
                return Err(());
            }
            if let Some(record) = read_startup_discovery(&self.dependencies.paths)? {
                if !matches_expected_service(
                    &record,
                    pid,
                    version,
                    self.dependencies
                        .process
                        .matches_executable(pid, &self.target),
                ) {
                    return Err(());
                }
                if verify_identity(&record).await.is_ok() {
                    return Ok(record);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(());
            }
            tokio::time::sleep(UPDATE_PROCESS_POLL_INTERVAL).await;
        }
    }

    async fn spawn_and_verify(
        &self,
        version: &Version,
        original: OriginalServiceState,
        mut progress: Option<(
            &mut ProgressReporter,
            &mut dyn CommandOutput,
            &ProgressDetails,
        )>,
    ) -> Result<(), VerificationFailure> {
        if let Some((reporter, output, versions)) = progress.as_mut() {
            reporter.running(*output, ProgressEvent::Starting((*versions).clone()));
        }
        let child = self
            .dependencies
            .update_process
            .spawn_service(&self.target)
            .await
            .map_err(|_| VerificationFailure::SafeToRollback)?;
        let mut child =
            ManagedUpdateChild::new(child, self.dependencies.clone(), &self.child_cleanup);
        let pid = match child.pid() {
            Some(pid) => pid,
            None => {
                return Err(if child.terminate(None).await.is_ok() {
                    VerificationFailure::SafeToRollback
                } else {
                    VerificationFailure::RecoveryRequired
                });
            }
        };
        if let Some((reporter, output, versions)) = progress.as_mut() {
            reporter.running(
                *output,
                ProgressEvent::VerifyingRuntime((*versions).clone()),
            );
        }
        let record = match self.wait_for_ready(pid, version).await {
            Ok(record) => record,
            Err(()) => {
                return Err(if child.terminate(None).await.is_ok() {
                    VerificationFailure::SafeToRollback
                } else {
                    VerificationFailure::RecoveryRequired
                });
            }
        };
        if original == OriginalServiceState::Running {
            child.detach();
            return Ok(());
        }

        let stopped = async {
            let client = ControlClient::connect(self.dependencies).await?;
            if client.record().instance_id != record.instance_id {
                return Err(ControlClientError::IdentityMismatch);
            }
            let management = client.management_secret(self.dependencies).await?;
            drain_and_stop(&client, &management).await?;
            self.wait_for_exit(&record)
                .await
                .map_err(|_| ControlClientError::Internal)
        }
        .await;
        if stopped.is_err() {
            return Err(if child.terminate(Some(&record)).await.is_ok() {
                VerificationFailure::SafeToRollback
            } else {
                VerificationFailure::RecoveryRequired
            });
        }
        child.detach();
        Ok(())
    }
}

#[async_trait]
impl UpdateLifecycle for ServiceUpdateLifecycle<'_> {
    async fn prepare(
        &self,
        reporter: &mut ProgressReporter,
        output: &mut dyn CommandOutput,
        versions: &ProgressDetails,
    ) -> Result<PreparedLifecycle, ()> {
        reporter.running(output, ProgressEvent::PreparingService(versions.clone()));
        let client = match ControlClient::connect(self.dependencies).await {
            Ok(client) => client,
            Err(ControlClientError::NotRunning) => {
                self.cleanup_stale_discovery()?;
                return Ok(PreparedLifecycle::plain(LifecyclePreparation::Ready(
                    OriginalServiceState::Stopped,
                )));
            }
            Err(_) => return Err(()),
        };
        let original = client.record().clone();
        if !matches_current_service(
            &original,
            self.dependencies
                .process
                .matches_executable(original.pid, &self.target),
        ) {
            return Err(());
        }
        let management = client
            .management_secret(self.dependencies)
            .await
            .map_err(|_| ())?;
        let recovery = StopRecoveryGuard::new(
            self.dependencies.clone(),
            self.target.clone(),
            client.clone(),
            management.clone(),
            original.clone(),
            self.child_cleanup.clone(),
        );
        reporter.running(output, ProgressEvent::Draining(versions.clone()));
        let drained = match request_drain(&client, &management).await {
            Ok(drained) => drained,
            Err(_) => {
                recovery.recover().await?;
                return Err(());
            }
        };
        reporter.running(
            output,
            ProgressEvent::Draining(ProgressDetails {
                active_requests: Some(drained.active_requests),
                ..versions.clone()
            }),
        );
        match classify_drain_response(&drained) {
            Ok(LifecyclePreparation::ActiveRequestsRemain { count }) => {
                recovery.recover().await?;
                return Ok(PreparedLifecycle::plain(
                    LifecyclePreparation::ActiveRequestsRemain { count },
                ));
            }
            Ok(LifecyclePreparation::Ready(OriginalServiceState::Running)) => {}
            Ok(LifecyclePreparation::Ready(OriginalServiceState::Stopped)) | Err(()) => {
                recovery.recover().await?;
                return Err(());
            }
        }
        recovery.mark_stop_requested();
        reporter.running(output, ProgressEvent::Stopping(versions.clone()));
        let stopped = match request_stop(&client, &management).await {
            Ok(stopped) => stopped,
            Err(_) => {
                recovery.recover().await?;
                return Err(());
            }
        };
        reporter.running(
            output,
            ProgressEvent::Draining(ProgressDetails {
                active_requests: Some(stopped.active_requests),
                ..versions.clone()
            }),
        );
        if stopped.phase != "stopping" || stopped.active_requests != 0 {
            recovery.recover().await?;
            return Err(());
        }
        if self.wait_for_exit(&original).await.is_err() {
            recovery.recover().await?;
            return Err(());
        }
        Ok(PreparedLifecycle::guarded(
            LifecyclePreparation::Ready(OriginalServiceState::Running),
            recovery,
        ))
    }

    async fn verify_installed(
        &self,
        version: &Version,
        original: OriginalServiceState,
        reporter: &mut ProgressReporter,
        output: &mut dyn CommandOutput,
        versions: &ProgressDetails,
    ) -> Result<(), VerificationFailure> {
        self.spawn_and_verify(version, original, Some((reporter, output, versions)))
            .await
    }

    async fn restore_previous(
        &self,
        version: &Version,
        original: OriginalServiceState,
        reporter: &mut ProgressReporter,
        output: &mut dyn CommandOutput,
        versions: &ProgressDetails,
    ) -> Result<(), ()> {
        if original == OriginalServiceState::Stopped {
            return Ok(());
        }
        self.spawn_and_verify(
            version,
            OriginalServiceState::Running,
            Some((reporter, output, versions)),
        )
        .await
        .map_err(|_| ())
    }
}

async fn recover_interrupted_stop(
    dependencies: RunDependencies,
    target: PathBuf,
    client: ControlClient,
    management: secrecy::SecretString,
    original: DiscoveryRecord,
    stop_requested: Arc<AtomicBool>,
    child_cleanup: Arc<ChildCleanupCoordinator>,
) -> Result<(), ()> {
    child_cleanup.wait().await?;
    let cancelled = request_cancel_drain(&client, &management).await;
    let lifecycle = ServiceUpdateLifecycle::new(&dependencies, target);
    if !stop_requested.load(Ordering::Acquire) {
        return verify_recovered_old_service(&lifecycle, &original, cancelled).await;
    }
    match lifecycle
        .wait_for_exit_with_timeout(&original, UPDATE_STOP_SETTLE_TIMEOUT)
        .await
    {
        Ok(()) => {}
        Err(()) if dependencies.process.is_running(original.pid) => {
            return verify_recovered_old_service(&lifecycle, &original, cancelled).await;
        }
        Err(()) => {
            let Some(record) = read_startup_discovery(&dependencies.paths)? else {
                return lifecycle
                    .spawn_and_verify(
                        &Version::parse(env!("CARGO_PKG_VERSION")).map_err(|_| ())?,
                        OriginalServiceState::Running,
                        None,
                    )
                    .await
                    .map_err(|_| ());
            };
            if record.instance_id != original.instance_id {
                return Err(());
            }
            let store = DiscoveryStore::new(&dependencies.paths).map_err(|_| ())?;
            store
                .remove_if_owned(original.instance_id)
                .map_err(|_| ())?
                .then_some(())
                .ok_or(())?;
        }
    }
    lifecycle
        .spawn_and_verify(
            &Version::parse(env!("CARGO_PKG_VERSION")).map_err(|_| ())?,
            OriginalServiceState::Running,
            None,
        )
        .await
        .map_err(|_| ())
}

async fn verify_recovered_old_service(
    lifecycle: &ServiceUpdateLifecycle<'_>,
    original: &DiscoveryRecord,
    cancelled: Result<LifecycleResponse, ControlClientError>,
) -> Result<(), ()> {
    let cancelled = cancelled.map_err(|_| ())?;
    let current = read_startup_discovery(&lifecycle.dependencies.paths)?
        .filter(|record| {
            recovered_old_service_matches(
                record,
                original,
                &cancelled,
                lifecycle
                    .dependencies
                    .process
                    .matches_executable(record.pid, &lifecycle.target),
            )
        })
        .ok_or(())?;
    verify_identity(&current).await.map_err(|_| ())
}

fn recovered_old_service_matches(
    current: &DiscoveryRecord,
    original: &DiscoveryRecord,
    cancelled: &LifecycleResponse,
    process_matches_target: bool,
) -> bool {
    cancelled.phase == "running"
        && cancelled.active_requests == 0
        && process_matches_target
        && current.base_url == original.base_url
        && current.pid == original.pid
        && current.instance_id == original.instance_id
        && current.wokcore_version == original.wokcore_version
        && current.api_major == original.api_major
        && matches_current_service(current, true)
}

fn classify_drain_response(response: &LifecycleResponse) -> Result<LifecyclePreparation, ()> {
    match (response.phase.as_str(), response.active_requests) {
        ("draining", 0) => Ok(LifecyclePreparation::Ready(OriginalServiceState::Running)),
        ("awaiting_cancellation", count) if count != 0 => {
            Ok(LifecyclePreparation::ActiveRequestsRemain { count })
        }
        _ => Err(()),
    }
}

fn matches_expected_discovery(record: &DiscoveryRecord, pid: u32, version: &Version) -> bool {
    record.pid == pid && record.api_major == 1 && record.wokcore_version == version.to_string()
}

fn matches_expected_service(
    record: &DiscoveryRecord,
    pid: u32,
    version: &Version,
    process_matches_target: bool,
) -> bool {
    process_matches_target && matches_expected_discovery(record, pid, version)
}

fn matches_current_service(record: &DiscoveryRecord, process_matches_target: bool) -> bool {
    process_matches_target
        && record.api_major == 1
        && record.wokcore_version == env!("CARGO_PKG_VERSION")
}

fn read_startup_discovery(
    paths: &wokcore_platform::AppPaths,
) -> Result<Option<DiscoveryRecord>, ()> {
    let store = match DiscoveryStore::new(paths) {
        Ok(store) => store,
        Err(PlatformError::Io { source }) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(_) => return Err(()),
    };
    match store.read() {
        Ok(record) => Ok(Some(record)),
        Err(PlatformError::Io { source }) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
    }
}

pub(super) async fn run(
    options: Update,
    dependencies: &RunDependencies,
    output: &mut dyn CommandOutput,
) -> ExitCode {
    let mut reporter = ProgressReporter::new(options.progress_jsonl);
    reporter.running(
        output,
        ProgressEvent::CheckingRelease(ProgressDetails {
            current_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            ..ProgressDetails::default()
        }),
    );
    let Some(source) = dependencies.update_source.as_ref() else {
        reporter.failed(
            output,
            ProgressEvent::Completed(ProgressDetails::default()),
            ProgressErrorCode::UpdateUnavailable,
        );
        return render(
            output,
            ExitCode::InternalFailure,
            json!({"code": "update_unavailable"}),
        );
    };
    let decision = check(source).await;
    report_install_check_result(&mut reporter, output, &decision);
    match decision {
        Ok(UpdateDecision::Current) => render(
            output,
            ExitCode::Success,
            json!({
                "code": "current",
                "current_version": env!("CARGO_PKG_VERSION"),
            }),
        ),
        Ok(UpdateDecision::Available(candidate)) if options.check => render(
            output,
            ExitCode::Success,
            json!({
                "code": "update_available",
                "current_version": env!("CARGO_PKG_VERSION"),
                "target": candidate.artifact().target(),
                "version": candidate.version().to_string(),
            }),
        ),
        Ok(UpdateDecision::Available(candidate)) => {
            let result =
                install_candidate(source, candidate, dependencies, &mut reporter, output).await;
            render_install_result(&mut reporter, output, result)
        }
        Ok(UpdateDecision::IncompatibleManifest) => render(
            output,
            ExitCode::InvalidInput,
            json!({"code": "incompatible_manifest"}),
        ),
        Err(()) => render(
            output,
            ExitCode::InternalFailure,
            json!({"code": "update_verification_failed"}),
        ),
    }
}

fn report_install_check_result(
    reporter: &mut ProgressReporter,
    output: &mut dyn CommandOutput,
    result: &Result<UpdateDecision, ()>,
) {
    match result {
        Ok(UpdateDecision::Current) => reporter.succeeded(
            output,
            ProgressEvent::Completed(ProgressDetails {
                current_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                ..ProgressDetails::default()
            }),
        ),
        Ok(UpdateDecision::IncompatibleManifest) => reporter.failed(
            output,
            ProgressEvent::Completed(ProgressDetails::default()),
            ProgressErrorCode::IncompatibleManifest,
        ),
        Err(()) => reporter.failed(
            output,
            ProgressEvent::Completed(ProgressDetails::default()),
            ProgressErrorCode::UpdateVerificationFailed,
        ),
        Ok(UpdateDecision::Available(_)) => {}
    }
}

fn render_install_result(
    reporter: &mut ProgressReporter,
    output: &mut dyn CommandOutput,
    result: Result<InstallOutcome, InstallFailure>,
) -> ExitCode {
    match &result {
        Ok(InstallOutcome::Installed { from, to }) => reporter.succeeded(
            output,
            ProgressEvent::Completed(ProgressDetails {
                current_version: Some(from.to_string()),
                target_version: Some(to.to_string()),
                active_requests: None,
            }),
        ),
        Ok(InstallOutcome::RolledBack { attempted }) => reporter.failed(
            output,
            ProgressEvent::Completed(ProgressDetails {
                current_version: None,
                target_version: Some(attempted.to_string()),
                active_requests: None,
            }),
            ProgressErrorCode::RolledBack,
        ),
        Ok(InstallOutcome::ActiveRequestsRemain { count }) => reporter.failed(
            output,
            ProgressEvent::Completed(ProgressDetails {
                current_version: None,
                target_version: None,
                active_requests: Some(*count),
            }),
            ProgressErrorCode::ActiveRequestsRemain,
        ),
        Err(InstallFailure::Failed) => reporter.failed(
            output,
            ProgressEvent::Completed(ProgressDetails::default()),
            ProgressErrorCode::UpdateInstallFailed,
        ),
        Err(InstallFailure::RecoveryRequired) => reporter.failed(
            output,
            ProgressEvent::Completed(ProgressDetails::default()),
            ProgressErrorCode::RecoveryRequired,
        ),
        Err(InstallFailure::OperationInProgress) => reporter.failed(
            output,
            ProgressEvent::Completed(ProgressDetails::default()),
            ProgressErrorCode::OperationInProgress,
        ),
    }
    match result {
        Ok(InstallOutcome::Installed { from, to }) => render(
            output,
            ExitCode::Success,
            json!({
                "code": "installed",
                "from": from.to_string(),
                "to": to.to_string(),
            }),
        ),
        Ok(InstallOutcome::RolledBack { attempted }) => render(
            output,
            ExitCode::InternalFailure,
            json!({
                "attempted": attempted.to_string(),
                "code": "rolled_back",
            }),
        ),
        Ok(InstallOutcome::ActiveRequestsRemain { count }) => render(
            output,
            ExitCode::InternalFailure,
            json!({
                "active_requests": count,
                "code": "active_requests_remain",
            }),
        ),
        Err(_) => render(
            output,
            ExitCode::InternalFailure,
            json!({"code": "update_install_failed"}),
        ),
    }
}

async fn install_candidate(
    source: &UpdateSource,
    candidate: wokcore_platform::update::UpdateCandidate,
    dependencies: &RunDependencies,
    reporter: &mut ProgressReporter,
    output: &mut dyn CommandOutput,
) -> Result<InstallOutcome, InstallFailure> {
    let target = dependencies
        .update_process
        .current_executable()
        .map_err(|_| InstallFailure::Failed)?;
    let target_directory = target.parent().ok_or(InstallFailure::Failed)?;
    let _lease = match acquire_update_lease(&target) {
        Ok(lease) => lease,
        Err(UpdateError::UpdateInProgress) => return Err(InstallFailure::OperationInProgress),
        Err(_) => return Err(InstallFailure::Failed),
    };
    let client = update_client()?;
    let current_version = Version::parse(env!("CARGO_PKG_VERSION")).map_err(|_| ())?;
    let versions = ProgressDetails {
        current_version: Some(current_version.to_string()),
        target_version: Some(candidate.version().to_string()),
        active_requests: None,
    };
    let download = download_artifact(
        &client,
        source,
        candidate.artifact(),
        target_directory,
        reporter,
        output,
        &versions,
    )
    .await?;
    let lifecycle = ServiceUpdateLifecycle::new(dependencies, target.clone());
    install_verified(
        download.file(),
        candidate.artifact(),
        &target,
        InstallVerificationContext {
            current_version: &current_version,
            next_version: candidate.version(),
            lifecycle: &lifecycle,
            transactions: &PlatformUpdateTransactionFactory,
        },
        reporter,
        output,
        &versions,
    )
    .await
}

async fn check(source: &UpdateSource) -> Result<UpdateDecision, ()> {
    validate_update_source(source)?;
    let client = update_client()?;
    let v2_url = source
        .origin
        .join("wokcore-update-v2.json")
        .map_err(|_| ())?;
    let (manifest, signature, expected_schema) =
        match fetch_document(&client, source, v2_url, MAX_UPDATE_MANIFEST_BYTES).await? {
            FetchDocument::Found(manifest) => {
                let signature = fetch_required_document(
                    &client,
                    source,
                    "wokcore-update-v2.json.minisig",
                    MAX_UPDATE_SIGNATURE_BYTES,
                )
                .await?;
                (manifest, signature, 2)
            }
            FetchDocument::NotFound => {
                let manifest = fetch_required_document(
                    &client,
                    source,
                    "wokcore-update-v1.json",
                    MAX_UPDATE_MANIFEST_BYTES,
                )
                .await?;
                let signature = fetch_required_document(
                    &client,
                    source,
                    "wokcore-update-v1.json.minisig",
                    MAX_UPDATE_SIGNATURE_BYTES,
                )
                .await?;
                (manifest, signature, 1)
            }
        };
    let current_version = Version::parse(env!("CARGO_PKG_VERSION")).map_err(|_| ())?;
    let decision = verify_manifest(
        &manifest,
        &signature,
        &source.public_key,
        &current_version,
        current_target(),
    )
    .map_err(|_| ())?;
    let schema_version = serde_json::from_slice::<Value>(&manifest)
        .map_err(|_| ())?
        .get("schema_version")
        .and_then(Value::as_u64);
    (schema_version == Some(expected_schema))
        .then_some(decision)
        .ok_or(())
}

fn update_client() -> Result<reqwest::Client, ()> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(UPDATE_CONNECT_TIMEOUT)
        .timeout(UPDATE_REQUEST_TIMEOUT)
        .read_timeout(UPDATE_READ_TIMEOUT)
        .build()
        .map_err(|_| ())
}

enum FetchDocument {
    Found(Vec<u8>),
    NotFound,
}

async fn fetch_document(
    client: &reqwest::Client,
    source: &UpdateSource,
    url: url::Url,
    maximum_bytes: usize,
) -> Result<FetchDocument, ()> {
    let mut response = send_update_request(client, source, url).await?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(FetchDocument::NotFound);
    }
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length > maximum_bytes as u64)
    {
        return Err(());
    }
    let capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or_default()
        .min(maximum_bytes);
    let mut body = Vec::with_capacity(capacity);
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        let next_length = body.len().checked_add(chunk.len()).ok_or(())?;
        if next_length > maximum_bytes {
            return Err(());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(FetchDocument::Found(body))
}

async fn fetch_required_document(
    client: &reqwest::Client,
    source: &UpdateSource,
    file: &str,
    maximum_bytes: usize,
) -> Result<Vec<u8>, ()> {
    let url = source.origin.join(file).map_err(|_| ())?;
    match fetch_document(client, source, url, maximum_bytes).await? {
        FetchDocument::Found(bytes) => Ok(bytes),
        FetchDocument::NotFound => Err(()),
    }
}

async fn download_artifact(
    client: &reqwest::Client,
    source: &UpdateSource,
    artifact: &UpdateArtifact,
    target_directory: &Path,
    reporter: &mut ProgressReporter,
    output: &mut dyn CommandOutput,
    versions: &ProgressDetails,
) -> Result<DownloadedArtifact, ()> {
    if artifact.size() == 0 || artifact.size() > MAX_UPDATE_ARTIFACT_BYTES {
        return Err(());
    }
    validate_update_source(source)?;
    let url = if source.origin.scheme() == "http" {
        source.origin.join(artifact.file()).map_err(|_| ())?
    } else {
        url::Url::parse(artifact.url()).map_err(|_| ())?
    };
    reporter.running(
        output,
        ProgressEvent::Downloading(DownloadProgressDetails {
            current_version: versions.current_version.clone(),
            target_version: versions.target_version.clone(),
            bytes_completed: 0,
            bytes_total: artifact.size(),
            active_requests: versions.active_requests,
        }),
    );
    let mut response = send_update_request(client, source, url).await?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length != artifact.size())
    {
        return Err(());
    }

    let staged = Builder::new()
        .prefix(".wokcore-update-download-")
        .tempfile_in(target_directory)
        .map_err(|_| ())?;
    let (file, path) = staged.into_parts();
    let mut file = tokio::fs::File::from_std(file);
    let mut received = 0_u64;
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        let next_received = received
            .checked_add(u64::try_from(chunk.len()).map_err(|_| ())?)
            .ok_or(())?;
        if next_received > artifact.size() {
            return Err(());
        }
        file.write_all(&chunk).await.map_err(|_| ())?;
        received = next_received;
        reporter.running(
            output,
            ProgressEvent::Downloading(DownloadProgressDetails {
                current_version: versions.current_version.clone(),
                target_version: versions.target_version.clone(),
                bytes_completed: received,
                bytes_total: artifact.size(),
                active_requests: versions.active_requests,
            }),
        );
    }
    if received != artifact.size() {
        return Err(());
    }
    file.flush().await.map_err(|_| ())?;
    file.sync_all().await.map_err(|_| ())?;
    let file = file.into_std().await;
    reporter.running(output, ProgressEvent::Verifying(versions.clone()));
    verify_artifact_file(&file, artifact).map_err(|_| ())?;
    Ok(DownloadedArtifact { file, _path: path })
}

fn validate_update_source(source: &UpdateSource) -> Result<(), ()> {
    if source.origin.scheme() == "http" {
        if matches!(
            source.origin.host(),
            Some(url::Host::Ipv4(address)) if address.is_loopback()
        ) {
            return Ok(());
        }
        return Err(());
    }
    (source.origin.as_str() == PRODUCTION_UPDATE_ORIGIN)
        .then_some(())
        .ok_or(())
}

async fn send_update_request(
    client: &reqwest::Client,
    source: &UpdateSource,
    url: url::Url,
) -> Result<Response, ()> {
    validate_initial_update_url(source, &url)?;
    let response = client.get(url.clone()).send().await.map_err(|_| ())?;
    if !response.status().is_redirection() {
        return Ok(response);
    }
    if source.origin.scheme() == "http" {
        return Err(());
    }
    let location = response
        .headers()
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
        .ok_or(())?;
    let redirect = url.join(location).map_err(|_| ())?;
    if validated_release_asset_redirect(&redirect).is_ok() {
        let redirected = client.get(redirect).send().await.map_err(|_| ())?;
        if redirected.status().is_redirection() {
            return Err(());
        }
        return Ok(redirected);
    }
    validated_latest_release_redirect(&url, &redirect)?;
    let versioned = client.get(redirect.clone()).send().await.map_err(|_| ())?;
    if !versioned.status().is_redirection() {
        return Ok(versioned);
    }
    let location = versioned
        .headers()
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
        .ok_or(())?;
    let release_asset = redirect.join(location).map_err(|_| ())?;
    validated_release_asset_redirect(&release_asset)?;
    let redirected = client.get(release_asset).send().await.map_err(|_| ())?;
    if redirected.status().is_redirection() {
        return Err(());
    }
    Ok(redirected)
}

fn validate_initial_update_url(source: &UpdateSource, url: &url::Url) -> Result<(), ()> {
    if source.origin.scheme() == "http" {
        return (url.origin() == source.origin.origin()
            && url.scheme() == "http"
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none())
        .then_some(())
        .ok_or(());
    }
    (url.scheme() == "https"
        && url.host_str() == Some("github.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && (matches!(
            url.path(),
            "/hongjiadev/wokcore/releases/latest/download/wokcore-update-v1.json"
                | "/hongjiadev/wokcore/releases/latest/download/wokcore-update-v1.json.minisig"
                | "/hongjiadev/wokcore/releases/latest/download/wokcore-update-v2.json"
                | "/hongjiadev/wokcore/releases/latest/download/wokcore-update-v2.json.minisig"
        ) || url
            .path()
            .starts_with("/hongjiadev/wokcore/releases/download/v")))
    .then_some(())
    .ok_or(())
}

fn validated_release_asset_redirect(url: &url::Url) -> Result<(), ()> {
    (url.scheme() == "https"
        && url.host_str() == Some("release-assets.githubusercontent.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && url.path().starts_with("/github-production-release-asset/"))
    .then_some(())
    .ok_or(())
}

fn validated_latest_release_redirect(initial: &url::Url, redirect: &url::Url) -> Result<(), ()> {
    const LATEST_PREFIX: &str = "/hongjiadev/wokcore/releases/latest/download/";
    const VERSION_PREFIX: &str = "/hongjiadev/wokcore/releases/download/v";

    let file = initial.path().strip_prefix(LATEST_PREFIX).ok_or(())?;
    if file.is_empty() || file.contains('/') {
        return Err(());
    }
    let versioned = redirect.path().strip_prefix(VERSION_PREFIX).ok_or(())?;
    let (version, redirected_file) = versioned.split_once('/').ok_or(())?;
    let parsed = Version::parse(version).map_err(|_| ())?;
    (redirect.scheme() == "https"
        && redirect.host_str() == Some("github.com")
        && redirect.port().is_none()
        && redirect.username().is_empty()
        && redirect.password().is_none()
        && redirect.query().is_none()
        && redirect.fragment().is_none()
        && parsed.to_string() == version
        && redirected_file == file
        && !redirected_file.contains('/'))
    .then_some(())
    .ok_or(())
}

struct InstallVerificationContext<'a> {
    current_version: &'a Version,
    next_version: &'a Version,
    lifecycle: &'a dyn UpdateLifecycle,
    transactions: &'a dyn UpdateTransactionFactory,
}

async fn install_verified(
    archive: &File,
    artifact: &UpdateArtifact,
    target: &Path,
    context: InstallVerificationContext<'_>,
    reporter: &mut ProgressReporter,
    output: &mut dyn CommandOutput,
    versions: &ProgressDetails,
) -> Result<InstallOutcome, InstallFailure> {
    let InstallVerificationContext {
        current_version,
        next_version,
        lifecycle,
        transactions,
    } = context;
    reporter.running(output, ProgressEvent::Verifying(versions.clone()));
    verify_artifact_file(archive, artifact).map_err(|_| InstallFailure::Failed)?;
    let prepared =
        prepare_install_file(archive, artifact, target).map_err(|_| InstallFailure::Failed)?;
    let preparation = lifecycle
        .prepare(reporter, output, versions)
        .await
        .map_err(|_| InstallFailure::Failed)?;
    let original = match preparation.outcome() {
        LifecyclePreparation::Ready(original) => original,
        LifecyclePreparation::ActiveRequestsRemain { count } => {
            preparation.disarm().map_err(|_| InstallFailure::Failed)?;
            return Ok(InstallOutcome::ActiveRequestsRemain { count });
        }
    };

    reporter.running(output, ProgressEvent::Installing(versions.clone()));
    let transaction = match transactions.begin(prepared) {
        Ok(transaction) => transaction,
        Err(UpdateError::RecoveryRequired) => {
            preparation.disarm().map_err(|_| InstallFailure::Failed)?;
            return Err(InstallFailure::RecoveryRequired);
        }
        Err(_) => {
            lifecycle
                .restore_previous(current_version, original, reporter, output, versions)
                .await
                .map_err(|_| InstallFailure::RecoveryRequired)?;
            preparation.disarm().map_err(|_| InstallFailure::Failed)?;
            return Err(InstallFailure::Failed);
        }
    };
    match lifecycle
        .verify_installed(next_version, original, reporter, output, versions)
        .await
    {
        Ok(()) => {
            match transaction.commit() {
                Ok(()) => {}
                Err(UpdateError::RecoveryRequired) => {
                    preparation.disarm().map_err(|_| InstallFailure::Failed)?;
                    return Err(InstallFailure::RecoveryRequired);
                }
                Err(_) => {
                    preparation.disarm().map_err(|_| InstallFailure::Failed)?;
                    return Err(InstallFailure::Failed);
                }
            }
            preparation.disarm().map_err(|_| InstallFailure::Failed)?;
            return Ok(InstallOutcome::Installed {
                from: current_version.clone(),
                to: next_version.clone(),
            });
        }
        Err(VerificationFailure::RecoveryRequired) => {
            transaction.preserve_for_recovery();
            preparation.disarm().map_err(|_| InstallFailure::Failed)?;
            return Err(InstallFailure::RecoveryRequired);
        }
        Err(VerificationFailure::SafeToRollback) => {}
    }

    reporter.running(output, ProgressEvent::RollingBack(versions.clone()));
    match transaction.rollback() {
        Ok(()) => {
            lifecycle
                .restore_previous(current_version, original, reporter, output, versions)
                .await
                .map_err(|_| InstallFailure::RecoveryRequired)?;
            preparation.disarm().map_err(|_| InstallFailure::Failed)?;
            Ok(InstallOutcome::RolledBack {
                attempted: next_version.clone(),
            })
        }
        Err(error @ UpdateError::RollbackDurabilityFailed) => {
            lifecycle
                .restore_previous(current_version, original, reporter, output, versions)
                .await
                .map_err(|_| InstallFailure::RecoveryRequired)?;
            preparation.disarm().map_err(|_| InstallFailure::Failed)?;
            Err(rollback_install_failure(&error))
        }
        Err(error) => {
            preparation.disarm().map_err(|_| InstallFailure::Failed)?;
            Err(rollback_install_failure(&error))
        }
    }
}

fn render(output: &mut dyn CommandOutput, exit: ExitCode, value: Value) -> ExitCode {
    if write_json(output, &value).is_ok() {
        exit
    } else {
        ExitCode::InternalFailure
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        future::Future,
        io,
        path::{Path, PathBuf},
        pin::Pin,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    #[cfg(windows)]
    use std::io::Write;

    use async_trait::async_trait;
    use axum::{
        Json, Router,
        body::Body,
        response::Response,
        routing::{get, post},
    };
    use secrecy::SecretString;
    use semver::Version;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;
    use tokio::{io::AsyncWriteExt, net::TcpListener};
    use url::Url;
    use uuid::Uuid;
    use wokcore_core::{
        id::ProviderId,
        secret::{SecretPurpose, SecretScope},
    };
    use wokcore_platform::{
        AppPaths, DiscoveryRecord, DiscoveryStore, RuntimeLease,
        update::{
            InstallTransaction, PreparedInstall, UpdateArtifact, UpdateDecision, UpdateError,
            current_target,
        },
    };
    use wokcore_server::auth::{EntropySource, TokenError};
    use wokcore_storage::{MemorySecretStore, SecretStore, StateStore};

    use super::progress::{ProgressDetails, ProgressReporter};
    use super::{
        ChildCleanupCoordinator, InstallFailure, InstallOutcome, InstallVerificationContext,
        LifecyclePreparation, ManagedUpdateChild, OriginalServiceState, PRODUCTION_UPDATE_ORIGIN,
        PlatformUpdateTransactionFactory, PreparedLifecycle, ServiceUpdateLifecycle,
        UpdateInstallTransaction, UpdateLifecycle, UpdateTransactionFactory, VerificationFailure,
        acquire_update_lease, classify_drain_response, download_artifact, install_verified,
        matches_current_service, matches_expected_discovery, matches_expected_service,
        read_startup_discovery, recovered_old_service_matches, render_install_result,
        report_install_check_result, run as run_update, update_client, validate_initial_update_url,
        validated_latest_release_redirect, validated_release_asset_redirect,
    };
    use crate::commands::stop::LifecycleResponse;
    use crate::{
        BufferOutput, Clock, CommandOutput, ExitCode, IdSource, ProcessIdentity, RunDependencies,
        RuntimeValueError, ShutdownSignal, UpdateChild, UpdateProcess, UpdateSource, cli::Update,
    };

    const ARCHIVE: &[u8] = b"synthetic update archive";

    fn private_tempdir() -> tempfile::TempDir {
        let directory = tempdir().expect("private update test temporary directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
                .expect("set private update test temporary directory permissions");
        }
        directory
    }

    fn progress_events(stderr: &str) -> Vec<Value> {
        let events = stderr
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        for (sequence, event) in events.iter().enumerate() {
            assert_eq!(event["sequence"], u64::try_from(sequence).unwrap());
        }
        events
    }

    fn assert_ordered_phase_groups(events: &[Value], expected: &[&str]) {
        let mut previous_last = None;
        for phase in expected {
            let indices = events
                .iter()
                .enumerate()
                .filter_map(|(index, event)| {
                    (event["phase"].as_str() == Some(*phase)).then_some(index)
                })
                .collect::<Vec<_>>();
            assert!(!indices.is_empty(), "missing phase {phase}: {events:?}");
            if let Some(previous_last) = previous_last {
                assert!(
                    indices[0] > previous_last,
                    "phase {phase} is out of order: {events:?}"
                );
            }
            previous_last = indices.last().copied();
        }
    }

    fn assert_ordered_phases(events: &[Value], expected: &[&str]) {
        let mut next_index = 0;
        for phase in expected {
            let Some(relative_index) = events[next_index..]
                .iter()
                .position(|event| event["phase"].as_str() == Some(*phase))
            else {
                panic!("missing phase {phase}: {events:?}");
            };
            next_index += relative_index + 1;
        }
    }
    const INSTALL_PUBLIC_KEY: &str = include_str!(
        "../../../../crates/wokcore-platform/tests/fixtures/update/install-minisign.pub"
    );
    const INSTALL_MANIFEST: &[u8] = include_bytes!(
        "../../../../crates/wokcore-platform/tests/fixtures/update/install-wokcore-update-v1.json"
    );
    const INSTALL_SIGNATURE: &[u8] = include_bytes!(
        "../../../../crates/wokcore-platform/tests/fixtures/update/install-wokcore-update-v1.json.minisig"
    );
    #[cfg(windows)]
    const INSTALL_ARCHIVE: &[u8] = &[
        80, 75, 3, 4, 20, 0, 0, 0, 0, 0, 0, 0, 33, 0, 248, 159, 107, 102, 14, 0, 0, 0, 14, 0, 0, 0,
        11, 0, 0, 0, 119, 111, 107, 99, 111, 114, 101, 46, 101, 120, 101, 110, 101, 119, 32, 101,
        120, 101, 99, 117, 116, 97, 98, 108, 101, 80, 75, 1, 2, 20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 33,
        0, 248, 159, 107, 102, 14, 0, 0, 0, 14, 0, 0, 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 128,
        1, 0, 0, 0, 0, 119, 111, 107, 99, 111, 114, 101, 46, 101, 120, 101, 80, 75, 5, 6, 0, 0, 0,
        0, 1, 0, 1, 0, 57, 0, 0, 0, 55, 0, 0, 0, 0, 0,
    ];
    #[cfg(not(windows))]
    const INSTALL_ARCHIVE: &[u8] = &[
        31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 237, 205, 65, 10, 130, 80, 20, 5, 208, 183, 20, 151,
        240, 165, 204, 245, 152, 188, 81, 145, 96, 138, 45, 191, 143, 147, 160, 121, 65, 116, 206,
        228, 94, 238, 228, 110, 211, 101, 156, 230, 140, 79, 42, 85, 223, 117, 123, 86, 239, 89,
        74, 123, 122, 245, 125, 239, 15, 199, 54, 154, 18, 95, 176, 222, 151, 97, 174, 151, 241,
        159, 110, 185, 53, 249, 200, 113, 93, 134, 243, 53, 3, 0, 0, 0, 0, 0, 0, 0, 0, 128, 31,
        241, 4, 159, 198, 218, 25, 0, 40, 0, 0,
    ];

    #[test]
    fn update_install_outcomes_without_progress_have_stable_json_and_exit_codes() {
        let cases = [
            (
                Ok(InstallOutcome::Installed {
                    from: Version::new(0, 1, 0),
                    to: Version::new(1, 2, 3),
                }),
                ExitCode::Success,
                json!({"code": "installed", "from": "0.1.0", "to": "1.2.3"}),
            ),
            (
                Ok(InstallOutcome::RolledBack {
                    attempted: Version::new(1, 2, 3),
                }),
                ExitCode::InternalFailure,
                json!({"attempted": "1.2.3", "code": "rolled_back"}),
            ),
            (
                Ok(InstallOutcome::ActiveRequestsRemain { count: 3 }),
                ExitCode::InternalFailure,
                json!({"active_requests": 3, "code": "active_requests_remain"}),
            ),
            (
                Err(InstallFailure::Failed),
                ExitCode::InternalFailure,
                json!({"code": "update_install_failed"}),
            ),
        ];

        for (result, expected_exit, expected_json) in cases {
            let mut output = BufferOutput::default();
            let mut reporter = ProgressReporter::new(false);

            let exit = render_install_result(&mut reporter, &mut output, result);

            assert_eq!(exit, expected_exit);
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(output.stdout()).unwrap(),
                expected_json
            );
            assert_eq!(output.stderr(), "");
        }
    }

    #[test]
    fn update_install_outcomes_emit_one_last_terminal_event_with_stable_codes() {
        let cases = [
            (
                Ok(InstallOutcome::Installed {
                    from: Version::new(0, 1, 0),
                    to: Version::new(1, 2, 3),
                }),
                "succeeded",
                None,
                None,
            ),
            (
                Ok(InstallOutcome::RolledBack {
                    attempted: Version::new(1, 2, 3),
                }),
                "failed",
                Some("rolled_back"),
                None,
            ),
            (
                Ok(InstallOutcome::ActiveRequestsRemain { count: 3 }),
                "failed",
                Some("active_requests_remain"),
                Some(3),
            ),
            (
                Err(InstallFailure::Failed),
                "failed",
                Some("update_install_failed"),
                None,
            ),
            (
                Err(InstallFailure::RecoveryRequired),
                "failed",
                Some("recovery_required"),
                None,
            ),
            (
                Err(InstallFailure::OperationInProgress),
                "failed",
                Some("operation_in_progress"),
                None,
            ),
        ];

        for (result, expected_state, expected_error_code, expected_active_requests) in cases {
            let mut output = BufferOutput::default();
            let mut reporter = ProgressReporter::new(true);

            render_install_result(&mut reporter, &mut output, result);

            let events = progress_events(output.stderr());
            assert_eq!(events.len(), 1);
            let terminal = events.last().unwrap();
            assert_eq!(terminal["state"], expected_state);
            assert_eq!(terminal["phase"], "completed");
            assert_eq!(
                terminal.get("error_code").and_then(Value::as_str),
                expected_error_code
            );
            assert_eq!(
                terminal.get("active_requests").and_then(Value::as_u64),
                expected_active_requests
            );
        }
    }

    #[test]
    fn update_install_check_results_emit_one_last_terminal_event_with_stable_codes() {
        let cases = [
            (
                Ok(UpdateDecision::Current),
                "succeeded",
                None,
                Some(env!("CARGO_PKG_VERSION")),
            ),
            (
                Ok(UpdateDecision::IncompatibleManifest),
                "failed",
                Some("incompatible_manifest"),
                None,
            ),
            (Err(()), "failed", Some("update_verification_failed"), None),
        ];

        for (result, expected_state, expected_error_code, expected_current_version) in cases {
            let mut output = BufferOutput::default();
            let mut reporter = ProgressReporter::new(true);

            report_install_check_result(&mut reporter, &mut output, &result);

            let events = progress_events(output.stderr());
            assert_eq!(events.len(), 1);
            let terminal = events.last().unwrap();
            assert_eq!(terminal["state"], expected_state);
            assert_eq!(terminal["phase"], "completed");
            assert_eq!(
                terminal.get("error_code").and_then(Value::as_str),
                expected_error_code
            );
            assert_eq!(
                terminal.get("current_version").and_then(Value::as_str),
                expected_current_version
            );
        }
    }

    #[tokio::test]
    async fn update_artifact_download_streams_to_the_target_volume_and_verifies_exact_bytes() {
        let (source, server) = serve_archive(ARCHIVE).await;
        let directory = private_tempdir();
        let artifact = artifact(ARCHIVE);
        let mut reporter = ProgressReporter::new(false);
        let mut output = BufferOutput::default();

        let download = download_artifact(
            &update_client().unwrap(),
            &source,
            &artifact,
            directory.path(),
            &mut reporter,
            &mut output,
            &ProgressDetails::default(),
        )
        .await
        .unwrap();

        server.abort();
        assert_eq!(
            download.file().metadata().unwrap().len(),
            ARCHIVE.len() as u64
        );
        assert_eq!(std::fs::read(download.path()).unwrap(), ARCHIVE);
        assert_eq!(download.path().parent(), Some(directory.path()));
    }

    #[tokio::test]
    async fn update_artifact_download_rejects_size_mismatch_and_cleans_the_partial_file() {
        let (source, server) = serve_archive(&ARCHIVE[..ARCHIVE.len() - 1]).await;
        let directory = private_tempdir();
        let artifact = artifact(ARCHIVE);
        let mut reporter = ProgressReporter::new(false);
        let mut output = BufferOutput::default();

        assert!(
            download_artifact(
                &update_client().unwrap(),
                &source,
                &artifact,
                directory.path(),
                &mut reporter,
                &mut output,
                &ProgressDetails::default(),
            )
            .await
            .is_err()
        );

        server.abort();
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn update_artifact_download_rejects_an_interrupted_body_and_cleans_partial_bytes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\npartial",
                ARCHIVE.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let source = UpdateSource {
            origin: Url::parse(&format!("http://{address}/")).unwrap(),
            public_key: "".into(),
        };
        let directory = private_tempdir();
        let artifact = artifact(ARCHIVE);
        let mut reporter = ProgressReporter::new(false);
        let mut output = BufferOutput::default();

        assert!(
            download_artifact(
                &update_client().unwrap(),
                &source,
                &artifact,
                directory.path(),
                &mut reporter,
                &mut output,
                &ProgressDetails::default(),
            )
            .await
            .is_err()
        );

        server.await.unwrap();
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn update_drain_requires_zero_active_requests_before_replacement() {
        assert_eq!(
            classify_drain_response(&LifecycleResponse {
                phase: "awaiting_cancellation".to_owned(),
                active_requests: 3,
            }),
            Ok(LifecyclePreparation::ActiveRequestsRemain { count: 3 })
        );
        assert_eq!(
            classify_drain_response(&LifecycleResponse {
                phase: "draining".to_owned(),
                active_requests: 0,
            }),
            Ok(LifecyclePreparation::Ready(OriginalServiceState::Running))
        );
        assert!(
            classify_drain_response(&LifecycleResponse {
                phase: "running".to_owned(),
                active_requests: 0,
            })
            .is_err()
        );
    }

    #[test]
    fn update_accepts_only_the_spawned_pid_target_version_and_api_major() {
        let record = DiscoveryRecord {
            base_url: "http://127.0.0.1:12345".to_owned(),
            pid: 4242,
            instance_id: Uuid::new_v4(),
            wokcore_version: "1.2.3".to_owned(),
            api_major: 1,
        };

        assert!(matches_expected_discovery(
            &record,
            4242,
            &Version::new(1, 2, 3)
        ));
        assert!(matches_expected_service(
            &record,
            4242,
            &Version::new(1, 2, 3),
            true,
        ));
        assert!(!matches_expected_service(
            &record,
            4242,
            &Version::new(1, 2, 3),
            false,
        ));
        assert!(!matches_expected_discovery(
            &record,
            4243,
            &Version::new(1, 2, 3)
        ));
        assert!(!matches_expected_discovery(
            &record,
            4242,
            &Version::new(1, 2, 4)
        ));
        let mut incompatible = record;
        incompatible.api_major = 2;
        assert!(!matches_expected_discovery(
            &incompatible,
            4242,
            &Version::new(1, 2, 3)
        ));
        incompatible.api_major = 1;
        incompatible.wokcore_version = env!("CARGO_PKG_VERSION").to_owned();
        assert!(matches_current_service(&incompatible, true));
        assert!(!matches_current_service(&incompatible, false));
        incompatible.wokcore_version = "9.9.9".to_owned();
        assert!(!matches_current_service(&incompatible, true));
    }

    #[test]
    fn recovery_requires_a_cancelled_drain_and_the_exact_old_service() {
        let original = DiscoveryRecord {
            base_url: "http://127.0.0.1:12345".to_owned(),
            pid: 4242,
            instance_id: Uuid::new_v4(),
            wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
            api_major: 1,
        };
        let running = LifecycleResponse {
            phase: "running".to_owned(),
            active_requests: 0,
        };

        assert!(recovered_old_service_matches(
            &original, &original, &running, true,
        ));
        assert!(!recovered_old_service_matches(
            &original, &original, &running, false,
        ));
        assert!(!recovered_old_service_matches(
            &original,
            &original,
            &LifecycleResponse {
                phase: "draining".to_owned(),
                active_requests: 0,
            },
            true,
        ));
        let mut replacement = original.clone();
        replacement.instance_id = Uuid::new_v4();
        assert!(!recovered_old_service_matches(
            &replacement,
            &original,
            &running,
            true,
        ));
    }

    #[test]
    fn update_redirects_accept_only_the_fixed_github_release_asset_host() {
        let source = UpdateSource {
            origin: Url::parse(PRODUCTION_UPDATE_ORIGIN).unwrap(),
            public_key: Arc::from("fixture"),
        };
        for file in [
            "wokcore-update-v1.json",
            "wokcore-update-v1.json.minisig",
            "wokcore-update-v2.json",
            "wokcore-update-v2.json.minisig",
        ] {
            assert!(
                validate_initial_update_url(&source, &source.origin.join(file).unwrap()).is_ok(),
                "{file}"
            );
        }
        let latest = Url::parse(
            "https://github.com/hongjiadev/wokcore/releases/latest/download/wokcore-update-v1.json",
        )
        .unwrap();
        assert!(
            validated_latest_release_redirect(
                &latest,
                &Url::parse(
                    "https://github.com/hongjiadev/wokcore/releases/download/v1.2.3/wokcore-update-v1.json"
                )
                .unwrap()
            )
            .is_ok()
        );
        for file in ["wokcore-update-v2.json", "wokcore-update-v2.json.minisig"] {
            let initial = source.origin.join(file).unwrap();
            let redirect = Url::parse(&format!(
                "https://github.com/hongjiadev/wokcore/releases/download/v1.2.3/{file}"
            ))
            .unwrap();
            assert!(
                validated_latest_release_redirect(&initial, &redirect).is_ok(),
                "{file}"
            );
        }
        for rejected in [
            "https://github.com/hongjiadev/other/releases/download/v1.2.3/wokcore-update-v1.json",
            "https://github.com/hongjiadev/wokcore/releases/download/not-semver/wokcore-update-v1.json",
            "https://github.com/hongjiadev/wokcore/releases/download/v1.2.3/other.json",
            "https://user@github.com/hongjiadev/wokcore/releases/download/v1.2.3/wokcore-update-v1.json",
            "https://github.com:444/hongjiadev/wokcore/releases/download/v1.2.3/wokcore-update-v1.json",
        ] {
            assert!(
                validated_latest_release_redirect(&latest, &Url::parse(rejected).unwrap()).is_err(),
                "{rejected}"
            );
        }
        assert!(
            validated_release_asset_redirect(
                &Url::parse(
                    "https://release-assets.githubusercontent.com/github-production-release-asset/123/file.zip?sp=r"
                )
                .unwrap()
            )
            .is_ok()
        );
        for rejected in [
            "https://example.com/file.zip",
            "http://release-assets.githubusercontent.com/file.zip",
            "https://user@release-assets.githubusercontent.com/file.zip",
            "https://release-assets.githubusercontent.com:444/file.zip",
            "https://release-assets.githubusercontent.com/file.zip#fragment",
        ] {
            assert!(
                validated_release_asset_redirect(&Url::parse(rejected).unwrap()).is_err(),
                "{rejected}"
            );
        }
    }

    #[test]
    fn update_startup_wait_treats_an_uncreated_runtime_directory_as_not_ready() {
        let directory = private_tempdir();
        let runtime_dir = directory.path().join("runtime");
        let paths = wokcore_platform::AppPaths {
            config_file: directory.path().join("config.toml"),
            state_db: directory.path().join("state.sqlite3"),
            runtime_dir: runtime_dir.clone(),
            log_dir: directory.path().join("logs"),
            discovery_file: runtime_dir.join("discovery.json"),
            instance_lock: directory.path().join("runtime").join("instance.lock"),
        };

        assert_eq!(read_startup_discovery(&paths).unwrap(), None);
    }

    #[tokio::test]
    async fn update_real_lifecycle_cancels_drain_when_loopback_reports_active_requests() {
        let directory = private_tempdir();
        let paths = test_paths(directory.path());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let instance_id = Uuid::new_v4();
        let app = Router::new()
            .route(
                "/wokcore/v1/health",
                get({
                    let observed = observed.clone();
                    move || {
                        let observed = observed.clone();
                        async move {
                            observed.lock().unwrap().push("health");
                            Json(json!({"status": "ok", "instance_id": instance_id}))
                        }
                    }
                }),
            )
            .route(
                "/wokcore/v1/capabilities",
                get({
                    let observed = observed.clone();
                    move || {
                        let observed = observed.clone();
                        async move {
                            observed.lock().unwrap().push("capabilities");
                            Json(json!({
                                "wokcore_version": env!("CARGO_PKG_VERSION"),
                                "management_api_major": 1,
                                "minimum_management_api_major": 1,
                                "maximum_management_api_major": 1,
                                "provider_protocols": [],
                                "capabilities": [],
                                "instance_id": instance_id,
                            }))
                        }
                    }
                }),
            )
            .route(
                "/wokcore/v1/service/drain",
                post({
                    let observed = observed.clone();
                    move || {
                        let observed = observed.clone();
                        async move {
                            observed.lock().unwrap().push("drain");
                            Json(json!({
                                "phase": "awaiting_cancellation",
                                "active_requests": 3,
                            }))
                        }
                    }
                }),
            )
            .route(
                "/wokcore/v1/service/drain/cancel",
                post({
                    let observed = observed.clone();
                    move || {
                        let observed = observed.clone();
                        async move {
                            observed.lock().unwrap().push("cancel");
                            Json(json!({"phase": "running", "active_requests": 0}))
                        }
                    }
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let _lease = RuntimeLease::acquire(&paths).unwrap();
        DiscoveryStore::new(&paths)
            .unwrap()
            .publish(&DiscoveryRecord {
                base_url: format!("http://{address}"),
                pid: 4242,
                instance_id,
                wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
                api_major: 1,
            })
            .unwrap();
        let secrets = Arc::new(MemorySecretStore::default());
        bind_management_secret(&paths, &secrets).await;
        let runtime = Arc::new(AlwaysRunningRuntime);
        let dependencies = RunDependencies::new(
            paths,
            secrets,
            runtime.clone(),
            runtime.clone(),
            runtime.clone(),
            runtime.clone(),
            runtime,
        );
        let lifecycle =
            ServiceUpdateLifecycle::new(&dependencies, directory.path().join("wokcore.exe"));

        let preparation = prepare_lifecycle_without_progress(&lifecycle)
            .await
            .unwrap();

        server.abort();
        assert_eq!(
            preparation.outcome(),
            LifecyclePreparation::ActiveRequestsRemain { count: 3 }
        );
        preparation.disarm().unwrap();
        assert_eq!(
            *observed.lock().unwrap(),
            [
                "health",
                "capabilities",
                "drain",
                "cancel",
                "health",
                "capabilities",
            ]
        );
    }

    #[tokio::test]
    async fn update_real_lifecycle_rejects_active_requests_when_drain_recovery_is_invalid() {
        let directory = private_tempdir();
        let paths = test_paths(directory.path());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let instance_id = Uuid::new_v4();
        let app =
            Router::new()
                .route(
                    "/wokcore/v1/health",
                    get(move || async move {
                        Json(json!({"status": "ok", "instance_id": instance_id}))
                    }),
                )
                .route(
                    "/wokcore/v1/capabilities",
                    get(move || async move {
                        Json(json!({
                            "wokcore_version": env!("CARGO_PKG_VERSION"),
                            "management_api_major": 1,
                            "minimum_management_api_major": 1,
                            "maximum_management_api_major": 1,
                            "provider_protocols": [],
                            "capabilities": [],
                            "instance_id": instance_id,
                        }))
                    }),
                )
                .route(
                    "/wokcore/v1/service/drain",
                    post(|| async {
                        Json(json!({
                            "phase": "awaiting_cancellation",
                            "active_requests": 3,
                        }))
                    }),
                )
                .route(
                    "/wokcore/v1/service/drain/cancel",
                    post(|| async { Json(json!({"phase": "draining", "active_requests": 3})) }),
                );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let _lease = RuntimeLease::acquire(&paths).unwrap();
        DiscoveryStore::new(&paths)
            .unwrap()
            .publish(&DiscoveryRecord {
                base_url: format!("http://{address}"),
                pid: 4242,
                instance_id,
                wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
                api_major: 1,
            })
            .unwrap();
        let secrets = Arc::new(MemorySecretStore::default());
        bind_management_secret(&paths, &secrets).await;
        let runtime = Arc::new(AlwaysRunningRuntime);
        let dependencies = RunDependencies::new(
            paths,
            secrets,
            runtime.clone(),
            runtime.clone(),
            runtime.clone(),
            runtime.clone(),
            runtime,
        );
        let lifecycle =
            ServiceUpdateLifecycle::new(&dependencies, directory.path().join("wokcore.exe"));

        assert!(
            prepare_lifecycle_without_progress(&lifecycle)
                .await
                .is_err()
        );
        server.abort();
    }

    #[tokio::test]
    async fn lifecycle_progress_reports_real_running_service_preparation_in_operation_order() {
        let directory = private_tempdir();
        let paths = test_paths(directory.path());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let running = Arc::new(AtomicBool::new(true));
        let instance_id = Uuid::new_v4();
        let app = Router::new()
            .route(
                "/wokcore/v1/health",
                get({
                    let observed = observed.clone();
                    move || {
                        let observed = observed.clone();
                        async move {
                            observed.lock().unwrap().push("health");
                            Json(json!({"status": "ok", "instance_id": instance_id}))
                        }
                    }
                }),
            )
            .route(
                "/wokcore/v1/capabilities",
                get({
                    let observed = observed.clone();
                    move || {
                        let observed = observed.clone();
                        async move {
                            observed.lock().unwrap().push("capabilities");
                            Json(json!({
                                "wokcore_version": env!("CARGO_PKG_VERSION"),
                                "management_api_major": 1,
                                "minimum_management_api_major": 1,
                                "maximum_management_api_major": 1,
                                "provider_protocols": [],
                                "capabilities": [],
                                "instance_id": instance_id,
                            }))
                        }
                    }
                }),
            )
            .route(
                "/wokcore/v1/service/drain",
                post({
                    let observed = observed.clone();
                    move || {
                        let observed = observed.clone();
                        async move {
                            observed.lock().unwrap().push("drain");
                            Json(json!({"phase": "draining", "active_requests": 0}))
                        }
                    }
                }),
            )
            .route(
                "/wokcore/v1/service/stop",
                post({
                    let observed = observed.clone();
                    let running = running.clone();
                    move || {
                        let observed = observed.clone();
                        let running = running.clone();
                        async move {
                            observed.lock().unwrap().push("stop");
                            running.store(false, Ordering::Release);
                            Json(json!({"phase": "stopping", "active_requests": 0}))
                        }
                    }
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let _lease = RuntimeLease::acquire(&paths).unwrap();
        DiscoveryStore::new(&paths)
            .unwrap()
            .publish(&DiscoveryRecord {
                base_url: format!("http://{address}"),
                pid: 4242,
                instance_id,
                wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
                api_major: 1,
            })
            .unwrap();
        let secrets = Arc::new(MemorySecretStore::default());
        bind_management_secret(&paths, &secrets).await;
        let runtime = Arc::new(MutableProcessRuntime {
            pid: 4242,
            running: running.clone(),
        });
        let dependencies = RunDependencies::new(
            paths.clone(),
            secrets,
            runtime.clone(),
            runtime.clone(),
            runtime.clone(),
            runtime.clone(),
            runtime,
        );
        let lifecycle =
            ServiceUpdateLifecycle::new(&dependencies, directory.path().join("wokcore.exe"));

        let mut reporter = ProgressReporter::new(true);
        let mut output = BufferOutput::default();
        let preparation = lifecycle
            .prepare(&mut reporter, &mut output, &ProgressDetails::default())
            .await
            .unwrap();

        server.abort();
        let events = progress_events(output.stderr());
        assert_ordered_phases(
            &events,
            &[
                "preparing_service",
                "draining",
                "draining",
                "stopping",
                "draining",
            ],
        );
        assert_eq!(events[2]["active_requests"], 0);
        assert_eq!(events[4]["active_requests"], 0);
        assert_eq!(
            preparation.outcome(),
            LifecyclePreparation::Ready(OriginalServiceState::Running)
        );
        assert!(!running.load(Ordering::Acquire));
        assert!(!paths.discovery_file.exists());
        assert_eq!(
            *observed.lock().unwrap(),
            ["health", "capabilities", "drain", "stop"]
        );
        preparation.disarm().unwrap();
    }

    #[tokio::test]
    async fn dropping_a_completed_preparation_restarts_the_stopped_old_service() {
        let directory = private_tempdir();
        let paths = test_paths(directory.path());
        let target = directory.path().join(if cfg!(windows) {
            "wokcore.exe"
        } else {
            "wokcore"
        });
        fs::write(&target, b"old executable").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let base_url = format!("http://{address}");
        let running = Arc::new(AtomicBool::new(true));
        let original_instance = Uuid::new_v4();
        let current_instance = Arc::new(Mutex::new(original_instance));
        let identity_checks = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/wokcore/v1/health",
                get({
                    let current_instance = current_instance.clone();
                    let identity_checks = identity_checks.clone();
                    move || {
                        let current_instance = current_instance.clone();
                        let identity_checks = identity_checks.clone();
                        async move {
                            identity_checks.fetch_add(1, Ordering::AcqRel);
                            Json(json!({
                                "status": "ok",
                                "instance_id": *current_instance.lock().unwrap(),
                            }))
                        }
                    }
                }),
            )
            .route(
                "/wokcore/v1/capabilities",
                get({
                    let current_instance = current_instance.clone();
                    let identity_checks = identity_checks.clone();
                    move || {
                        let current_instance = current_instance.clone();
                        let identity_checks = identity_checks.clone();
                        async move {
                            identity_checks.fetch_add(1, Ordering::AcqRel);
                            Json(json!({
                                "wokcore_version": env!("CARGO_PKG_VERSION"),
                                "management_api_major": 1,
                                "minimum_management_api_major": 1,
                                "maximum_management_api_major": 1,
                                "provider_protocols": [],
                                "capabilities": [],
                                "instance_id": *current_instance.lock().unwrap(),
                            }))
                        }
                    }
                }),
            )
            .route(
                "/wokcore/v1/service/drain",
                post(|| async { Json(json!({"phase": "draining", "active_requests": 0})) }),
            )
            .route(
                "/wokcore/v1/service/drain/cancel",
                post(|| async { Json(json!({"phase": "running", "active_requests": 0})) }),
            )
            .route(
                "/wokcore/v1/service/stop",
                post({
                    let running = running.clone();
                    move || {
                        let running = running.clone();
                        async move {
                            running.store(false, Ordering::Release);
                            Json(json!({"phase": "stopping", "active_requests": 0}))
                        }
                    }
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let _lease = RuntimeLease::acquire(&paths).unwrap();
        DiscoveryStore::new(&paths)
            .unwrap()
            .publish(&DiscoveryRecord {
                base_url: base_url.clone(),
                pid: 4242,
                instance_id: original_instance,
                wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
                api_major: 1,
            })
            .unwrap();
        let secrets = Arc::new(MemorySecretStore::default());
        bind_management_secret(&paths, &secrets).await;
        let runtime = Arc::new(MutableProcessRuntime {
            pid: 4242,
            running: running.clone(),
        });
        let spawns = Arc::new(AtomicUsize::new(0));
        let update_process = Arc::new(RecoveryUpdateProcess {
            target: target.clone(),
            paths: paths.clone(),
            base_url,
            current_instance: current_instance.clone(),
            running: running.clone(),
            spawns: spawns.clone(),
        });
        let dependencies = RunDependencies::new(
            paths.clone(),
            secrets,
            runtime.clone(),
            runtime.clone(),
            runtime.clone(),
            runtime.clone(),
            runtime,
        )
        .with_update_process(update_process);
        let lifecycle = ServiceUpdateLifecycle::new(&dependencies, target);

        let preparation = prepare_lifecycle_without_progress(&lifecycle)
            .await
            .unwrap();
        assert_eq!(
            preparation.outcome(),
            LifecyclePreparation::Ready(OriginalServiceState::Running)
        );
        assert!(!running.load(Ordering::Acquire));
        drop(preparation);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while (spawns.load(Ordering::Acquire) == 0
            || !running.load(Ordering::Acquire)
            || identity_checks.load(Ordering::Acquire) < 4)
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(spawns.load(Ordering::Acquire), 1);
        assert!(running.load(Ordering::Acquire));
        assert!(identity_checks.load(Ordering::Acquire) >= 4);
        let recovered = DiscoveryStore::new(&paths).unwrap().read().unwrap();
        assert_ne!(recovered.instance_id, original_instance);
        server.abort();
    }

    #[tokio::test]
    async fn cancelling_new_service_verification_terminates_the_unverified_child() {
        let directory = private_tempdir();
        let paths = test_paths(directory.path());
        let target = directory.path().join(if cfg!(windows) {
            "wokcore.exe"
        } else {
            "wokcore"
        });
        fs::write(&target, b"new executable").unwrap();
        let running = Arc::new(AtomicBool::new(false));
        let spawned = Arc::new(tokio::sync::Notify::new());
        let runtime = Arc::new(MutableProcessRuntime {
            pid: 6000,
            running: running.clone(),
        });
        let update_process = Arc::new(PendingUpdateProcess {
            target: target.clone(),
            running: running.clone(),
            spawned: spawned.clone(),
        });
        let dependencies = RunDependencies::new(
            paths,
            Arc::new(MemorySecretStore::default()),
            runtime.clone(),
            runtime.clone(),
            runtime.clone(),
            runtime.clone(),
            runtime,
        )
        .with_update_process(update_process);
        let lifecycle = ServiceUpdateLifecycle::new(&dependencies, target);
        let next_version = Version::new(1, 2, 3);
        let mut verification = Box::pin(lifecycle.spawn_and_verify(
            &next_version,
            OriginalServiceState::Running,
            None,
        ));

        tokio::select! {
            () = spawned.notified() => {}
            result = &mut verification => panic!("verification completed before cancellation: {result:?}"),
            () = tokio::time::sleep(Duration::from_secs(2)) => panic!("new service was not spawned"),
        }
        assert!(running.load(Ordering::Acquire));
        drop(verification);
        tokio::time::timeout(Duration::from_secs(2), async {
            while running.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancellation must wait for the unverified child to terminate");
    }

    #[tokio::test]
    async fn failed_background_child_cleanup_is_reported_to_recovery() {
        let directory = private_tempdir();
        let paths = test_paths(directory.path());
        fs::create_dir_all(&paths.runtime_dir).unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let runtime = Arc::new(MutableProcessRuntime {
            pid: 6100,
            running: running.clone(),
        });
        let dependencies = RunDependencies::new(
            paths.clone(),
            Arc::new(MemorySecretStore::default()),
            runtime.clone(),
            runtime.clone(),
            runtime.clone(),
            runtime.clone(),
            runtime,
        );
        let coordinator = Arc::new(ChildCleanupCoordinator::new());
        let child = CorruptingUpdateChild {
            paths,
            running,
            detached: false,
        };
        let managed = ManagedUpdateChild::new(Box::new(child), dependencies, &coordinator);

        drop(managed);

        let cleanup = tokio::time::timeout(Duration::from_secs(2), coordinator.wait())
            .await
            .expect("background cleanup must complete");
        assert!(
            cleanup.is_err(),
            "recovery must not restart the old service after cleanup failure"
        );
    }

    #[tokio::test]
    async fn cancelling_install_verification_rolls_back_and_restarts_the_old_service() {
        let fixture = install_fixture();
        let paths = test_paths(fixture._directory.path());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let base_url = format!("http://{address}");
        let running = Arc::new(AtomicBool::new(true));
        let original_instance = Uuid::new_v4();
        let current_instance = Arc::new(Mutex::new(original_instance));
        let identity_checks = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/wokcore/v1/health",
                get({
                    let current_instance = current_instance.clone();
                    let identity_checks = identity_checks.clone();
                    move || {
                        let current_instance = current_instance.clone();
                        let identity_checks = identity_checks.clone();
                        async move {
                            identity_checks.fetch_add(1, Ordering::AcqRel);
                            Json(json!({
                                "status": "ok",
                                "instance_id": *current_instance.lock().unwrap(),
                            }))
                        }
                    }
                }),
            )
            .route(
                "/wokcore/v1/capabilities",
                get({
                    let current_instance = current_instance.clone();
                    let identity_checks = identity_checks.clone();
                    move || {
                        let current_instance = current_instance.clone();
                        let identity_checks = identity_checks.clone();
                        async move {
                            identity_checks.fetch_add(1, Ordering::AcqRel);
                            Json(json!({
                                "wokcore_version": env!("CARGO_PKG_VERSION"),
                                "management_api_major": 1,
                                "minimum_management_api_major": 1,
                                "maximum_management_api_major": 1,
                                "provider_protocols": [],
                                "capabilities": [],
                                "instance_id": *current_instance.lock().unwrap(),
                            }))
                        }
                    }
                }),
            )
            .route(
                "/wokcore/v1/service/drain",
                post(|| async { Json(json!({"phase": "draining", "active_requests": 0})) }),
            )
            .route(
                "/wokcore/v1/service/drain/cancel",
                post(|| async { Json(json!({"phase": "running", "active_requests": 0})) }),
            )
            .route(
                "/wokcore/v1/service/stop",
                post({
                    let running = running.clone();
                    move || {
                        let running = running.clone();
                        async move {
                            running.store(false, Ordering::Release);
                            Json(json!({"phase": "stopping", "active_requests": 0}))
                        }
                    }
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let _lease = RuntimeLease::acquire(&paths).unwrap();
        DiscoveryStore::new(&paths)
            .unwrap()
            .publish(&DiscoveryRecord {
                base_url: base_url.clone(),
                pid: 4242,
                instance_id: original_instance,
                wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
                api_major: 1,
            })
            .unwrap();
        let secrets = Arc::new(MemorySecretStore::default());
        bind_management_secret(&paths, &secrets).await;
        let runtime = Arc::new(MutableProcessRuntime {
            pid: 4242,
            running: running.clone(),
        });
        let new_spawned = Arc::new(tokio::sync::Notify::new());
        let cleanup_started = Arc::new(tokio::sync::Notify::new());
        let allow_new_exit = Arc::new(tokio::sync::Notify::new());
        let old_spawns = Arc::new(AtomicUsize::new(0));
        let update_process = Arc::new(SwitchingUpdateProcess {
            target: fixture.target.clone(),
            paths: paths.clone(),
            base_url: base_url.clone(),
            current_instance,
            running: running.clone(),
            new_spawned: new_spawned.clone(),
            cleanup_started: cleanup_started.clone(),
            allow_new_exit: allow_new_exit.clone(),
            old_spawns: old_spawns.clone(),
        });
        let dependencies = RunDependencies::new(
            paths.clone(),
            secrets,
            runtime.clone(),
            runtime.clone(),
            runtime.clone(),
            runtime.clone(),
            runtime,
        )
        .with_update_process(update_process);
        let lifecycle = ServiceUpdateLifecycle::new(&dependencies, fixture.target.clone());
        let archive = fs::File::open(&fixture.archive).unwrap();
        let current_version = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        let next_version = Version::new(1, 2, 3);
        let mut install = Box::pin(install_verified_without_progress(
            &archive,
            &fixture.artifact,
            &fixture.target,
            &current_version,
            &next_version,
            &lifecycle,
        ));

        tokio::select! {
            () = new_spawned.notified() => {}
            result = &mut install => panic!("install completed before cancellation: {result:?}"),
            () = tokio::time::sleep(Duration::from_secs(2)) => panic!("new service was not spawned"),
        }
        assert_eq!(fs::read(&fixture.target).unwrap(), b"new executable");
        assert!(running.load(Ordering::Acquire));
        drop(install);

        tokio::time::timeout(Duration::from_secs(2), cleanup_started.notified())
            .await
            .expect("cancellation must begin terminating the unverified child");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            old_spawns.load(Ordering::Acquire),
            0,
            "the old service must not restart before the unverified child exits"
        );
        allow_new_exit.notify_one();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while (old_spawns.load(Ordering::Acquire) == 0
            || !running.load(Ordering::Acquire)
            || identity_checks.load(Ordering::Acquire) < 4)
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(fs::read(&fixture.target).unwrap(), b"old executable");
        assert_eq!(old_spawns.load(Ordering::Acquire), 1);
        assert!(running.load(Ordering::Acquire));
        assert!(identity_checks.load(Ordering::Acquire) >= 4);
        let recovered = DiscoveryStore::new(&paths).unwrap().read().unwrap();
        assert_eq!(recovered.wokcore_version, env!("CARGO_PKG_VERSION"));
        server.abort();
    }

    #[tokio::test]
    async fn cancelling_an_update_while_drain_is_accepted_cancels_the_drain() {
        let directory = private_tempdir();
        let paths = test_paths(directory.path());
        let target = directory.path().join(if cfg!(windows) {
            "wokcore.exe"
        } else {
            "wokcore"
        });
        fs::write(&target, b"old executable").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let drain_accepted = Arc::new(tokio::sync::Notify::new());
        let drain_cancelled = Arc::new(tokio::sync::Notify::new());
        let instance_id = Uuid::new_v4();
        let app = Router::new()
            .route(
                "/wokcore/v1/health",
                get(move || async move {
                    Json(json!({
                        "status": "ok",
                        "instance_id": instance_id,
                    }))
                }),
            )
            .route(
                "/wokcore/v1/capabilities",
                get(move || async move {
                    Json(json!({
                        "wokcore_version": env!("CARGO_PKG_VERSION"),
                        "management_api_major": 1,
                        "minimum_management_api_major": 1,
                        "maximum_management_api_major": 1,
                        "provider_protocols": [],
                        "capabilities": [],
                        "instance_id": instance_id,
                    }))
                }),
            )
            .route(
                "/wokcore/v1/service/drain",
                post({
                    let drain_accepted = drain_accepted.clone();
                    move || {
                        let drain_accepted = drain_accepted.clone();
                        async move {
                            drain_accepted.notify_one();
                            tokio::time::sleep(Duration::from_secs(30)).await;
                            Json(json!({"phase": "draining", "active_requests": 0}))
                        }
                    }
                }),
            )
            .route(
                "/wokcore/v1/service/drain/cancel",
                post({
                    let drain_cancelled = drain_cancelled.clone();
                    move || {
                        let drain_cancelled = drain_cancelled.clone();
                        async move {
                            drain_cancelled.notify_one();
                            Json(json!({"phase": "running", "active_requests": 0}))
                        }
                    }
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let _lease = RuntimeLease::acquire(&paths).unwrap();
        DiscoveryStore::new(&paths)
            .unwrap()
            .publish(&DiscoveryRecord {
                base_url: format!("http://{address}"),
                pid: 4242,
                instance_id,
                wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
                api_major: 1,
            })
            .unwrap();
        let secrets = Arc::new(MemorySecretStore::default());
        bind_management_secret(&paths, &secrets).await;
        let runtime = Arc::new(MutableProcessRuntime { pid: 4242, running });
        let dependencies = RunDependencies::new(
            paths,
            secrets,
            runtime.clone(),
            runtime.clone(),
            runtime.clone(),
            runtime.clone(),
            runtime,
        );
        let lifecycle = ServiceUpdateLifecycle::new(&dependencies, target);
        let mut preparation = Box::pin(prepare_lifecycle_without_progress(&lifecycle));

        tokio::select! {
            () = drain_accepted.notified() => {}
            result = &mut preparation => panic!("prepare completed before cancellation: {result:?}"),
            () = tokio::time::sleep(Duration::from_secs(2)) => panic!("drain was not accepted"),
        }
        drop(preparation);

        tokio::time::timeout(Duration::from_secs(2), drain_cancelled.notified())
            .await
            .expect("cancellation must undo a drain accepted before its response completes");
        server.abort();
    }

    #[tokio::test]
    async fn cancelling_an_update_after_stop_is_accepted_restarts_the_old_service() {
        let directory = private_tempdir();
        let paths = test_paths(directory.path());
        let target = directory.path().join(if cfg!(windows) {
            "wokcore.exe"
        } else {
            "wokcore"
        });
        fs::write(&target, b"old executable").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let base_url = format!("http://{address}");
        let running = Arc::new(AtomicBool::new(true));
        let stop_accepted = Arc::new(tokio::sync::Notify::new());
        let identity_checks = Arc::new(AtomicUsize::new(0));
        let original_instance = Uuid::new_v4();
        let current_instance = Arc::new(Mutex::new(original_instance));
        let app = Router::new()
            .route(
                "/wokcore/v1/health",
                get({
                    let current_instance = current_instance.clone();
                    let identity_checks = identity_checks.clone();
                    move || {
                        let current_instance = current_instance.clone();
                        let identity_checks = identity_checks.clone();
                        async move {
                            identity_checks.fetch_add(1, Ordering::AcqRel);
                            Json(json!({
                                "status": "ok",
                                "instance_id": *current_instance.lock().unwrap(),
                            }))
                        }
                    }
                }),
            )
            .route(
                "/wokcore/v1/capabilities",
                get({
                    let current_instance = current_instance.clone();
                    let identity_checks = identity_checks.clone();
                    move || {
                        let current_instance = current_instance.clone();
                        let identity_checks = identity_checks.clone();
                        async move {
                            identity_checks.fetch_add(1, Ordering::AcqRel);
                            Json(json!({
                                "wokcore_version": env!("CARGO_PKG_VERSION"),
                                "management_api_major": 1,
                                "minimum_management_api_major": 1,
                                "maximum_management_api_major": 1,
                                "provider_protocols": [],
                                "capabilities": [],
                                "instance_id": *current_instance.lock().unwrap(),
                            }))
                        }
                    }
                }),
            )
            .route(
                "/wokcore/v1/service/drain",
                post(|| async { Json(json!({"phase": "draining", "active_requests": 0})) }),
            )
            .route(
                "/wokcore/v1/service/drain/cancel",
                post(|| async { Json(json!({"phase": "running", "active_requests": 0})) }),
            )
            .route(
                "/wokcore/v1/service/stop",
                post({
                    let stop_accepted = stop_accepted.clone();
                    move || {
                        let stop_accepted = stop_accepted.clone();
                        async move {
                            stop_accepted.notify_one();
                            tokio::time::sleep(Duration::from_secs(30)).await;
                            Json(json!({"phase": "stopping", "active_requests": 0}))
                        }
                    }
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let _lease = RuntimeLease::acquire(&paths).unwrap();
        DiscoveryStore::new(&paths)
            .unwrap()
            .publish(&DiscoveryRecord {
                base_url: base_url.clone(),
                pid: 4242,
                instance_id: original_instance,
                wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
                api_major: 1,
            })
            .unwrap();
        let secrets = Arc::new(MemorySecretStore::default());
        bind_management_secret(&paths, &secrets).await;
        let runtime = Arc::new(MutableProcessRuntime {
            pid: 4242,
            running: running.clone(),
        });
        let spawns = Arc::new(AtomicUsize::new(0));
        let update_process = Arc::new(RecoveryUpdateProcess {
            target: target.clone(),
            paths: paths.clone(),
            base_url,
            current_instance: current_instance.clone(),
            running: running.clone(),
            spawns: spawns.clone(),
        });
        let dependencies = RunDependencies::new(
            paths.clone(),
            secrets,
            runtime.clone(),
            runtime.clone(),
            runtime.clone(),
            runtime.clone(),
            runtime,
        )
        .with_update_process(update_process);
        let lifecycle = ServiceUpdateLifecycle::new(&dependencies, target);
        let mut preparation = Box::pin(prepare_lifecycle_without_progress(&lifecycle));

        tokio::select! {
            () = stop_accepted.notified() => {}
            result = &mut preparation => panic!("prepare completed before cancellation: {result:?}"),
            () = tokio::time::sleep(Duration::from_secs(2)) => panic!("stop was not accepted"),
        }
        running.store(false, Ordering::Release);
        drop(preparation);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while (spawns.load(Ordering::Acquire) == 0
            || !running.load(Ordering::Acquire)
            || identity_checks.load(Ordering::Acquire) < 4)
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let spawn_count = spawns.load(Ordering::Acquire);
        let process_running = running.load(Ordering::Acquire);
        let identity_check_count = identity_checks.load(Ordering::Acquire);
        assert!(
            spawn_count == 1 && process_running && identity_check_count >= 4,
            "spawns={spawn_count} running={process_running} identity_checks={identity_check_count} discovery={:?}",
            DiscoveryStore::new(&paths)
                .ok()
                .and_then(|store| store.read().ok())
        );
        let recovered = DiscoveryStore::new(&paths).unwrap().read().unwrap();
        assert_ne!(recovered.instance_id, original_instance);
        assert_eq!(recovered.wokcore_version, env!("CARGO_PKG_VERSION"));
        server.abort();
    }

    #[tokio::test]
    async fn update_install_command_emits_progress_for_the_signed_stopped_service_flow_on_loopback()
    {
        assert_signed_service_update(true, false, false).await;
    }

    #[tokio::test]
    async fn lifecycle_progress_reports_the_signed_running_service_flow_in_operation_order() {
        assert_signed_service_update(true, false, true).await;
    }

    #[tokio::test]
    async fn update_install_command_without_progress_preserves_stdout_and_stderr() {
        assert_signed_service_update(false, false, false).await;
    }

    #[tokio::test]
    async fn update_install_command_reports_operation_in_progress_from_the_real_lease_path() {
        assert_signed_service_update(true, true, false).await;
    }

    async fn assert_signed_service_update(
        progress_jsonl: bool,
        hold_update_lease: bool,
        original_running: bool,
    ) {
        let directory = private_tempdir();
        let paths = test_paths(directory.path());
        let target = directory.path().join(if cfg!(windows) {
            "wokcore.exe"
        } else {
            "wokcore"
        });
        fs::write(&target, b"old executable").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let base_url = format!("http://{address}");
        let running = Arc::new(AtomicBool::new(original_running));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let instance_id = Uuid::new_v4();
        let artifact_path = format!(
            "/wokcore-v1.2.3-{}.{}",
            current_target(),
            if cfg!(windows) { "zip" } else { "tar.gz" }
        );
        let app = Router::new()
            .route(
                "/wokcore-update-v1.json",
                get({
                    let observed = observed.clone();
                    move || {
                        let observed = observed.clone();
                        async move {
                            observed.lock().unwrap().push("manifest");
                            Response::new(Body::from(INSTALL_MANIFEST))
                        }
                    }
                }),
            )
            .route(
                "/wokcore-update-v1.json.minisig",
                get({
                    let observed = observed.clone();
                    move || {
                        let observed = observed.clone();
                        async move {
                            observed.lock().unwrap().push("signature");
                            Response::new(Body::from(INSTALL_SIGNATURE))
                        }
                    }
                }),
            )
            .route(
                &artifact_path,
                get({
                    let observed = observed.clone();
                    move || {
                        let observed = observed.clone();
                        async move {
                            observed.lock().unwrap().push("artifact");
                            Response::new(Body::from(INSTALL_ARCHIVE))
                        }
                    }
                }),
            )
            .route(
                "/wokcore/v1/health",
                get({
                    let observed = observed.clone();
                    move || {
                        let observed = observed.clone();
                        async move {
                            observed.lock().unwrap().push("health");
                            Json(json!({"status": "ok", "instance_id": instance_id}))
                        }
                    }
                }),
            )
            .route(
                "/wokcore/v1/capabilities",
                get({
                    let observed = observed.clone();
                    let target = target.clone();
                    move || {
                        let observed = observed.clone();
                        let target = target.clone();
                        async move {
                            observed.lock().unwrap().push("capabilities");
                            let version = if fs::read(&target).unwrap() == b"old executable" {
                                env!("CARGO_PKG_VERSION")
                            } else {
                                "1.2.3"
                            };
                            Json(json!({
                                "wokcore_version": version,
                                "management_api_major": 1,
                                "minimum_management_api_major": 1,
                                "maximum_management_api_major": 1,
                                "provider_protocols": [],
                                "capabilities": [],
                                "instance_id": instance_id,
                            }))
                        }
                    }
                }),
            )
            .route(
                "/wokcore/v1/service/drain",
                post({
                    let observed = observed.clone();
                    move || {
                        let observed = observed.clone();
                        async move {
                            observed.lock().unwrap().push("drain");
                            Json(json!({"phase": "draining", "active_requests": 0}))
                        }
                    }
                }),
            )
            .route(
                "/wokcore/v1/service/stop",
                post({
                    let observed = observed.clone();
                    let running = running.clone();
                    move || {
                        let observed = observed.clone();
                        let running = running.clone();
                        async move {
                            observed.lock().unwrap().push("stop");
                            running.store(false, Ordering::Release);
                            Json(json!({"phase": "stopping", "active_requests": 0}))
                        }
                    }
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let _lease = RuntimeLease::acquire(&paths).unwrap();
        if original_running {
            DiscoveryStore::new(&paths)
                .unwrap()
                .publish(&DiscoveryRecord {
                    base_url: base_url.clone(),
                    pid: 5000,
                    instance_id,
                    wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
                    api_major: 1,
                })
                .unwrap();
        }
        let secrets = Arc::new(MemorySecretStore::default());
        bind_management_secret(&paths, &secrets).await;
        let runtime = Arc::new(MutableProcessRuntime {
            pid: 5000,
            running: running.clone(),
        });
        let update_process = Arc::new(SyntheticUpdateProcess {
            target: target.clone(),
            paths: paths.clone(),
            base_url,
            instance_id,
            running: running.clone(),
        });
        let dependencies = RunDependencies::new(
            paths.clone(),
            secrets,
            runtime.clone(),
            runtime.clone(),
            runtime.clone(),
            runtime.clone(),
            runtime,
        )
        .with_loopback_update_source(
            Url::parse(&format!("http://{address}/")).unwrap(),
            INSTALL_PUBLIC_KEY,
        )
        .unwrap()
        .with_update_process(update_process);
        let mut output = BufferOutput::default();
        let _held_update_lease = hold_update_lease.then(|| acquire_update_lease(&target).unwrap());

        let exit = run_update(
            Update {
                check: false,
                install: true,
                json: true,
                progress_jsonl,
            },
            &dependencies,
            &mut output,
        )
        .await;

        server.abort();
        if hold_update_lease {
            assert_eq!(exit, ExitCode::InternalFailure);
            assert_eq!(output.stdout(), "{\"code\":\"update_install_failed\"}\n");
            let events = progress_events(output.stderr());
            assert_eq!(events.first().unwrap()["phase"], "checking_release");
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event["phase"] == "completed")
                    .count(),
                1
            );
            let terminal = events.last().unwrap();
            assert_eq!(terminal["state"], "failed");
            assert_eq!(terminal["phase"], "completed");
            assert_eq!(terminal["error_code"], "operation_in_progress");
            assert_eq!(fs::read(&target).unwrap(), b"old executable");
            assert_eq!(*observed.lock().unwrap(), ["manifest", "signature"]);
            return;
        }
        assert_eq!(
            exit,
            ExitCode::Success,
            "stdout={} observed={:?} target={:?}",
            output.stdout(),
            *observed.lock().unwrap(),
            fs::read(&target).unwrap(),
        );
        assert_eq!(
            output.stdout(),
            format!(
                "{{\"code\":\"installed\",\"from\":\"{}\",\"to\":\"1.2.3\"}}\n",
                env!("CARGO_PKG_VERSION")
            )
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(output.stdout()).unwrap(),
            json!({
                "code": "installed",
                "from": env!("CARGO_PKG_VERSION"),
                "to": "1.2.3"
            })
        );
        if progress_jsonl {
            let events = progress_events(output.stderr());
            assert_ordered_phase_groups(
                &events,
                &[
                    "checking_release",
                    "downloading",
                    "verifying",
                    "installing",
                    "completed",
                ],
            );
            let downloading = events
                .iter()
                .filter(|event| event["phase"] == "downloading")
                .collect::<Vec<_>>();
            assert_eq!(downloading.first().unwrap()["bytes_completed"], 0);
            assert!(
                downloading
                    .iter()
                    .all(|event| event["bytes_total"] == INSTALL_ARCHIVE.len() as u64)
            );
            assert_eq!(
                downloading.last().unwrap()["bytes_completed"],
                INSTALL_ARCHIVE.len() as u64
            );
            assert!(events.iter().any(|event| event["phase"] == "verifying"));
            assert!(events.iter().any(|event| event["phase"] == "installing"));
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event["phase"] == "completed")
                    .count(),
                1
            );
            assert_eq!(events.last().unwrap()["state"], "succeeded");
            assert_eq!(events.last().unwrap()["phase"], "completed");
            if original_running {
                assert_ordered_phases(
                    &events,
                    &[
                        "preparing_service",
                        "draining",
                        "stopping",
                        "starting",
                        "verifying_runtime",
                        "completed",
                    ],
                );
                for event in events.iter().filter(|event| {
                    matches!(
                        event["phase"].as_str(),
                        Some(
                            "preparing_service"
                                | "draining"
                                | "stopping"
                                | "starting"
                                | "verifying_runtime"
                        )
                    )
                }) {
                    assert_eq!(event["current_version"], env!("CARGO_PKG_VERSION"));
                    assert_eq!(event["target_version"], "1.2.3");
                    assert!(event.get("bytes_completed").is_none());
                    assert!(event.get("bytes_total").is_none());
                }
            }
        } else {
            assert_eq!(output.stderr(), "");
        }
        assert_eq!(fs::read(&target).unwrap(), b"new executable");
        assert_eq!(running.load(Ordering::Acquire), original_running);
        if original_running {
            assert_eq!(
                DiscoveryStore::new(&paths)
                    .unwrap()
                    .read()
                    .unwrap()
                    .wokcore_version,
                "1.2.3"
            );
        } else {
            assert!(!paths.discovery_file.exists());
        }
        let expected_observed = if original_running {
            vec![
                "manifest",
                "signature",
                "artifact",
                "health",
                "capabilities",
                "drain",
                "stop",
                "health",
                "capabilities",
            ]
        } else {
            vec![
                "manifest",
                "signature",
                "artifact",
                "health",
                "capabilities",
                "health",
                "capabilities",
                "drain",
                "stop",
            ]
        };
        assert_eq!(*observed.lock().unwrap(), expected_observed);
    }

    #[tokio::test]
    async fn update_install_never_replaces_when_active_requests_remain() {
        let fixture = install_fixture();
        let archive = fs::File::open(&fixture.archive).unwrap();
        let lifecycle = FakeLifecycle::new(
            LifecyclePreparation::ActiveRequestsRemain { count: 3 },
            true,
        );

        let outcome = install_verified_without_progress(
            &archive,
            &fixture.artifact,
            &fixture.target,
            &Version::new(0, 1, 0),
            &Version::new(1, 2, 3),
            &lifecycle,
        )
        .await
        .unwrap();

        assert_eq!(outcome, InstallOutcome::ActiveRequestsRemain { count: 3 });
        assert_eq!(fs::read(&fixture.target).unwrap(), b"old executable");
        assert_eq!(lifecycle.events(), vec!["prepare"]);
    }

    #[tokio::test]
    async fn lifecycle_progress_reports_active_requests_without_installing() {
        let fixture = install_fixture();
        let archive = fs::File::open(&fixture.archive).unwrap();
        let lifecycle = FakeLifecycle::new(
            LifecyclePreparation::ActiveRequestsRemain { count: 3 },
            true,
        );
        let mut reporter = ProgressReporter::new(true);
        let mut output = BufferOutput::default();
        let versions = ProgressDetails {
            current_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            target_version: Some("1.2.3".to_owned()),
            active_requests: None,
        };

        let result = install_verified(
            &archive,
            &fixture.artifact,
            &fixture.target,
            InstallVerificationContext {
                current_version: &Version::new(0, 1, 0),
                next_version: &Version::new(1, 2, 3),
                lifecycle: &lifecycle,
                transactions: &PlatformUpdateTransactionFactory,
            },
            &mut reporter,
            &mut output,
            &versions,
        )
        .await;
        render_install_result(&mut reporter, &mut output, result);

        let events = progress_events(output.stderr());
        assert!(!events.iter().any(|event| event["phase"] == "installing"));
        assert!(!events.iter().any(|event| event["phase"] == "rolling_back"));
        let terminals = events
            .iter()
            .filter(|event| event["state"] != "running")
            .collect::<Vec<_>>();
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0], events.last().unwrap());
        let terminal = events.last().unwrap();
        assert_eq!(terminal["phase"], "completed");
        assert_eq!(terminal["error_code"], "active_requests_remain");
        assert_eq!(terminal["active_requests"], 3);
    }

    #[tokio::test]
    async fn update_install_rejects_an_invalid_archive_before_draining_the_service() {
        let mut fixture = install_fixture();
        fs::write(&fixture.archive, b"not an archive").unwrap();
        fixture.artifact = artifact(&fs::read(&fixture.archive).unwrap());
        let archive = fs::File::open(&fixture.archive).unwrap();
        let lifecycle = FakeLifecycle::new(
            LifecyclePreparation::Ready(OriginalServiceState::Running),
            true,
        );

        assert!(
            install_verified_without_progress(
                &archive,
                &fixture.artifact,
                &fixture.target,
                &Version::new(0, 1, 0),
                &Version::new(1, 2, 3),
                &lifecycle,
            )
            .await
            .is_err()
        );

        assert_eq!(fs::read(&fixture.target).unwrap(), b"old executable");
        assert!(lifecycle.events().is_empty());
    }

    #[tokio::test]
    async fn update_install_restores_a_running_service_when_atomic_begin_fails() {
        let fixture = install_fixture();
        let archive = fs::File::open(&fixture.archive).unwrap();
        let backup = fixture.target.parent().unwrap().join(format!(
            ".{}.previous",
            fixture.target.file_name().unwrap().to_string_lossy()
        ));
        let lifecycle = FakeLifecycle::new(
            LifecyclePreparation::Ready(OriginalServiceState::Running),
            true,
        )
        .with_prepare_hook(move || fs::create_dir(&backup).unwrap());

        assert!(
            install_verified_without_progress(
                &archive,
                &fixture.artifact,
                &fixture.target,
                &Version::new(0, 1, 0),
                &Version::new(1, 2, 3),
                &lifecycle,
            )
            .await
            .is_err()
        );

        assert_eq!(fs::read(&fixture.target).unwrap(), b"old executable");
        assert_eq!(lifecycle.events(), vec!["prepare", "restore"]);
    }

    #[tokio::test]
    async fn update_install_rolls_back_exact_bytes_when_new_health_verification_fails() {
        let fixture = install_fixture();
        let archive = fs::File::open(&fixture.archive).unwrap();
        let lifecycle = FakeLifecycle::new(
            LifecyclePreparation::Ready(OriginalServiceState::Running),
            false,
        );

        let outcome = install_verified_without_progress(
            &archive,
            &fixture.artifact,
            &fixture.target,
            &Version::new(0, 1, 0),
            &Version::new(1, 2, 3),
            &lifecycle,
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            InstallOutcome::RolledBack {
                attempted: Version::new(1, 2, 3),
            }
        );
        assert_eq!(fs::read(&fixture.target).unwrap(), b"old executable");
        assert_eq!(lifecycle.events(), vec!["prepare", "verify", "restore"]);
    }

    #[tokio::test]
    async fn broken_progress_pipe_preserves_full_install_rollback_outcome() {
        let normal_fixture = install_fixture();
        let normal_backup = install_backup_path(&normal_fixture.target);
        let normal_lifecycle = FullPathRollbackLifecycle::new(&normal_fixture.target);
        let mut normal_reporter = ProgressReporter::new(true);
        let mut normal_output = BufferOutput::default();
        let normal_exit = run_full_path_rollback(
            &normal_fixture,
            &normal_lifecycle,
            &mut normal_reporter,
            &mut normal_output,
        )
        .await;

        let broken_fixture = install_fixture();
        let broken_backup = install_backup_path(&broken_fixture.target);
        let broken_lifecycle = FullPathRollbackLifecycle::new(&broken_fixture.target);
        let mut broken_reporter = ProgressReporter::new(true);
        let mut broken_output = BrokenPipeOutput::default();
        let broken_exit = run_full_path_rollback(
            &broken_fixture,
            &broken_lifecycle,
            &mut broken_reporter,
            &mut broken_output,
        )
        .await;

        assert_eq!(normal_exit, ExitCode::InternalFailure);
        assert_eq!(broken_exit, normal_exit);
        assert_eq!(broken_output.stdout, normal_output.stdout());
        assert_eq!(
            serde_json::from_str::<Value>(normal_output.stdout()).unwrap()["code"],
            "rolled_back"
        );
        assert_eq!(fs::read(&normal_fixture.target).unwrap(), b"old executable");
        assert_eq!(fs::read(&broken_fixture.target).unwrap(), b"old executable");
        assert!(!normal_backup.exists());
        assert!(!broken_backup.exists());
        normal_lifecycle.assert_complete_rollback();
        broken_lifecycle.assert_complete_rollback();
        assert_eq!(broken_output.stderr_attempts, 1);
        let first_event = serde_json::from_str::<Value>(
            broken_output.first_stderr.as_deref().unwrap().trim_end(),
        )
        .unwrap();
        assert_eq!(first_event["phase"], "verifying");
        assert_eq!(first_event["state"], "running");

        let events = progress_events(normal_output.stderr());
        assert_ordered_phases(
            &events,
            &["verifying", "installing", "rolling_back", "completed"],
        );
        assert_eq!(events.last().unwrap()["error_code"], "rolled_back");
    }

    #[tokio::test]
    async fn lifecycle_progress_reports_rollback_before_the_terminal_result() {
        let fixture = install_fixture();
        let archive = fs::File::open(&fixture.archive).unwrap();
        let lifecycle = FakeLifecycle::new(
            LifecyclePreparation::Ready(OriginalServiceState::Running),
            false,
        );
        let mut reporter = ProgressReporter::new(true);
        let mut output = BufferOutput::default();

        let result = install_verified(
            &archive,
            &fixture.artifact,
            &fixture.target,
            InstallVerificationContext {
                current_version: &Version::new(0, 1, 0),
                next_version: &Version::new(1, 2, 3),
                lifecycle: &lifecycle,
                transactions: &PlatformUpdateTransactionFactory,
            },
            &mut reporter,
            &mut output,
            &ProgressDetails::default(),
        )
        .await;
        render_install_result(&mut reporter, &mut output, result);

        let events = progress_events(output.stderr());
        assert_ordered_phases(&events, &["installing", "rolling_back", "completed"]);
        assert_eq!(events.last().unwrap()["error_code"], "rolled_back");
    }

    #[tokio::test]
    async fn lifecycle_progress_reports_new_runtime_failure_and_real_restore_in_order() {
        let fixture = install_fixture();
        let paths = test_paths(fixture._directory.path());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let base_url = format!("http://{address}");
        let running = Arc::new(AtomicBool::new(true));
        let original_instance = Uuid::new_v4();
        let current_instance = Arc::new(Mutex::new(original_instance));
        let app = Router::new()
            .route(
                "/wokcore/v1/health",
                get({
                    let current_instance = current_instance.clone();
                    move || {
                        let current_instance = current_instance.clone();
                        async move {
                            Json(json!({
                                "status": "ok",
                                "instance_id": *current_instance.lock().unwrap(),
                            }))
                        }
                    }
                }),
            )
            .route(
                "/wokcore/v1/capabilities",
                get({
                    let current_instance = current_instance.clone();
                    move || {
                        let current_instance = current_instance.clone();
                        async move {
                            Json(json!({
                                "wokcore_version": env!("CARGO_PKG_VERSION"),
                                "management_api_major": 1,
                                "minimum_management_api_major": 1,
                                "maximum_management_api_major": 1,
                                "provider_protocols": [],
                                "capabilities": [],
                                "instance_id": *current_instance.lock().unwrap(),
                            }))
                        }
                    }
                }),
            )
            .route(
                "/wokcore/v1/service/drain",
                post(|| async { Json(json!({"phase": "draining", "active_requests": 0})) }),
            )
            .route(
                "/wokcore/v1/service/stop",
                post({
                    let running = running.clone();
                    move || {
                        let running = running.clone();
                        async move {
                            running.store(false, Ordering::Release);
                            Json(json!({"phase": "stopping", "active_requests": 0}))
                        }
                    }
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let _lease = RuntimeLease::acquire(&paths).unwrap();
        DiscoveryStore::new(&paths)
            .unwrap()
            .publish(&DiscoveryRecord {
                base_url: base_url.clone(),
                pid: 4242,
                instance_id: original_instance,
                wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
                api_major: 1,
            })
            .unwrap();
        let secrets = Arc::new(MemorySecretStore::default());
        bind_management_secret(&paths, &secrets).await;
        let runtime = Arc::new(MutableProcessRuntime {
            pid: 4242,
            running: running.clone(),
        });
        let spawns = Arc::new(AtomicUsize::new(0));
        let update_process = Arc::new(RollbackUpdateProcess {
            target: fixture.target.clone(),
            paths: paths.clone(),
            base_url,
            current_instance,
            running: running.clone(),
            spawns: spawns.clone(),
        });
        let dependencies = RunDependencies::new(
            paths,
            secrets,
            runtime.clone(),
            runtime.clone(),
            runtime.clone(),
            runtime.clone(),
            runtime,
        )
        .with_update_process(update_process);
        let lifecycle = ServiceUpdateLifecycle::new(&dependencies, fixture.target.clone());
        let archive = fs::File::open(&fixture.archive).unwrap();
        let mut reporter = ProgressReporter::new(true);
        let mut output = BufferOutput::default();
        let versions = ProgressDetails {
            current_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            target_version: Some("1.2.3".to_owned()),
            active_requests: None,
        };

        let result = install_verified(
            &archive,
            &fixture.artifact,
            &fixture.target,
            InstallVerificationContext {
                current_version: &Version::parse(env!("CARGO_PKG_VERSION")).unwrap(),
                next_version: &Version::new(1, 2, 3),
                lifecycle: &lifecycle,
                transactions: &PlatformUpdateTransactionFactory,
            },
            &mut reporter,
            &mut output,
            &versions,
        )
        .await;
        render_install_result(&mut reporter, &mut output, result);

        server.abort();
        let events = progress_events(output.stderr());
        assert_ordered_phases(
            &events,
            &[
                "starting",
                "verifying_runtime",
                "rolling_back",
                "starting",
                "verifying_runtime",
                "completed",
            ],
        );
        assert_eq!(events.last().unwrap()["error_code"], "rolled_back");
        assert_eq!(spawns.load(Ordering::Acquire), 2);
        assert_eq!(fs::read(&fixture.target).unwrap(), b"old executable");
        assert!(running.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn lifecycle_progress_propagates_verification_recovery_required_to_terminal() {
        let fixture = install_fixture();
        let archive = fs::File::open(&fixture.archive).unwrap();
        let lifecycle = FakeLifecycle::new(
            LifecyclePreparation::Ready(OriginalServiceState::Running),
            false,
        )
        .with_recovery_required();

        let terminal = install_failure_terminal(&archive, &fixture, &lifecycle).await;

        assert_eq!(terminal["error_code"], "recovery_required");
        assert_eq!(fs::read(&fixture.target).unwrap(), b"new executable");
    }

    #[tokio::test]
    async fn lifecycle_progress_propagates_atomic_begin_recovery_required_to_terminal() {
        let fixture = install_fixture();
        let archive = fs::File::open(&fixture.archive).unwrap();
        let displaced = fixture.target.parent().unwrap().join("displaced-target");
        let target = fixture.target.clone();
        let lifecycle = FakeLifecycle::new(
            LifecyclePreparation::Ready(OriginalServiceState::Running),
            true,
        )
        .with_prepare_hook(move || {
            fs::rename(&target, &displaced).unwrap();
            fs::write(&target, b"untrusted executable").unwrap();
        });

        let terminal = install_failure_terminal(&archive, &fixture, &lifecycle).await;

        assert_eq!(terminal["error_code"], "recovery_required");
        assert_eq!(fs::read(&fixture.target).unwrap(), b"untrusted executable");
    }

    #[tokio::test]
    async fn lifecycle_progress_propagates_previous_runtime_restore_failure_to_terminal() {
        let fixture = install_fixture();
        let archive = fs::File::open(&fixture.archive).unwrap();
        let lifecycle = FakeLifecycle::new(
            LifecyclePreparation::Ready(OriginalServiceState::Running),
            false,
        )
        .with_restore_failure();

        let terminal = install_failure_terminal(&archive, &fixture, &lifecycle).await;

        assert_eq!(terminal["error_code"], "recovery_required");
        assert_eq!(fs::read(&fixture.target).unwrap(), b"old executable");
    }

    struct RollbackDurabilityFailureFactory;

    impl UpdateTransactionFactory for RollbackDurabilityFailureFactory {
        fn begin(
            &self,
            prepared: PreparedInstall,
        ) -> Result<Box<dyn UpdateInstallTransaction>, UpdateError> {
            Ok(Box::new(RollbackDurabilityFailureTransaction {
                inner: prepared.begin()?,
            }))
        }
    }

    struct RollbackDurabilityFailureTransaction {
        inner: InstallTransaction,
    }

    impl UpdateInstallTransaction for RollbackDurabilityFailureTransaction {
        fn commit(self: Box<Self>) -> Result<(), UpdateError> {
            self.inner.commit()
        }

        fn rollback(self: Box<Self>) -> Result<(), UpdateError> {
            self.inner.rollback()?;
            Err(UpdateError::RollbackDurabilityFailed)
        }

        fn preserve_for_recovery(self: Box<Self>) {
            self.inner.preserve_for_recovery();
        }
    }

    #[tokio::test]
    async fn lifecycle_progress_propagates_rollback_durability_failure_to_terminal() {
        let fixture = install_fixture();
        let archive = fs::File::open(&fixture.archive).unwrap();
        let lifecycle = FakeLifecycle::new(
            LifecyclePreparation::Ready(OriginalServiceState::Running),
            false,
        );
        let mut reporter = ProgressReporter::new(true);
        let mut output = BufferOutput::default();

        let result = install_verified(
            &archive,
            &fixture.artifact,
            &fixture.target,
            InstallVerificationContext {
                current_version: &Version::new(0, 1, 0),
                next_version: &Version::new(1, 2, 3),
                lifecycle: &lifecycle,
                transactions: &RollbackDurabilityFailureFactory,
            },
            &mut reporter,
            &mut output,
            &ProgressDetails::default(),
        )
        .await;
        let exit = render_install_result(&mut reporter, &mut output, result);

        let events = progress_events(output.stderr());
        let terminals = events
            .iter()
            .filter(|event| event["state"] != "running")
            .collect::<Vec<_>>();
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0], events.last().unwrap());
        assert_eq!(terminals[0]["phase"], "completed");
        assert_eq!(terminals[0]["state"], "failed");
        assert_eq!(terminals[0]["error_code"], "recovery_required");
        assert_eq!(output.stdout(), "{\"code\":\"update_install_failed\"}\n");
        assert_eq!(exit, ExitCode::InternalFailure);
        assert_eq!(fs::read(&fixture.target).unwrap(), b"old executable");
        assert_eq!(lifecycle.events(), vec!["prepare", "verify", "restore"]);
    }

    #[tokio::test]
    async fn update_install_preserves_recovery_artifacts_when_new_process_cannot_terminate() {
        let fixture = install_fixture();
        let archive = fs::File::open(&fixture.archive).unwrap();
        let backup = fixture.target.parent().unwrap().join(format!(
            ".{}.previous",
            fixture.target.file_name().unwrap().to_string_lossy()
        ));
        let lifecycle = FakeLifecycle::new(
            LifecyclePreparation::Ready(OriginalServiceState::Running),
            false,
        )
        .with_recovery_required();

        assert!(
            install_verified_without_progress(
                &archive,
                &fixture.artifact,
                &fixture.target,
                &Version::new(0, 1, 0),
                &Version::new(1, 2, 3),
                &lifecycle,
            )
            .await
            .is_err()
        );

        assert_eq!(fs::read(&fixture.target).unwrap(), b"new executable");
        assert_eq!(fs::read(backup).unwrap(), b"old executable");
        assert_eq!(lifecycle.events(), vec!["prepare", "verify"]);
    }

    #[tokio::test]
    async fn update_install_preserves_a_stopped_service_after_successful_verification() {
        let fixture = install_fixture();
        let archive = fs::File::open(&fixture.archive).unwrap();
        let lifecycle = FakeLifecycle::new(
            LifecyclePreparation::Ready(OriginalServiceState::Stopped),
            true,
        );

        let outcome = install_verified_without_progress(
            &archive,
            &fixture.artifact,
            &fixture.target,
            &Version::new(0, 1, 0),
            &Version::new(1, 2, 3),
            &lifecycle,
        )
        .await
        .unwrap();

        assert_eq!(
            outcome,
            InstallOutcome::Installed {
                from: Version::new(0, 1, 0),
                to: Version::new(1, 2, 3),
            }
        );
        assert_eq!(fs::read(&fixture.target).unwrap(), b"new executable");
        assert_eq!(lifecycle.events(), vec!["prepare", "verify"]);
        assert_eq!(
            lifecycle.verified_state(),
            Some(OriginalServiceState::Stopped)
        );
    }

    async fn serve_archive(bytes: &'static [u8]) -> (UpdateSource, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/artifact.zip",
            get(move || async move { Response::new(Body::from(bytes)) }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (
            UpdateSource {
                origin: Url::parse(&format!("http://{address}/")).unwrap(),
                public_key: "".into(),
            },
            server,
        )
    }

    async fn install_verified_without_progress(
        archive: &std::fs::File,
        artifact: &UpdateArtifact,
        target: &Path,
        current_version: &Version,
        next_version: &Version,
        lifecycle: &dyn UpdateLifecycle,
    ) -> Result<InstallOutcome, InstallFailure> {
        let mut reporter = ProgressReporter::new(false);
        let mut output = BufferOutput::default();
        install_verified(
            archive,
            artifact,
            target,
            InstallVerificationContext {
                current_version,
                next_version,
                lifecycle,
                transactions: &PlatformUpdateTransactionFactory,
            },
            &mut reporter,
            &mut output,
            &ProgressDetails::default(),
        )
        .await
    }

    async fn prepare_lifecycle_without_progress(
        lifecycle: &dyn UpdateLifecycle,
    ) -> Result<PreparedLifecycle, ()> {
        let mut reporter = ProgressReporter::new(false);
        let mut output = BufferOutput::default();
        lifecycle
            .prepare(&mut reporter, &mut output, &ProgressDetails::default())
            .await
    }

    async fn install_failure_terminal(
        archive: &std::fs::File,
        fixture: &InstallFixture,
        lifecycle: &dyn UpdateLifecycle,
    ) -> Value {
        let mut reporter = ProgressReporter::new(true);
        let mut output = BufferOutput::default();
        let result = install_verified(
            archive,
            &fixture.artifact,
            &fixture.target,
            InstallVerificationContext {
                current_version: &Version::new(0, 1, 0),
                next_version: &Version::new(1, 2, 3),
                lifecycle,
                transactions: &PlatformUpdateTransactionFactory,
            },
            &mut reporter,
            &mut output,
            &ProgressDetails::default(),
        )
        .await;
        let exit = render_install_result(&mut reporter, &mut output, result);
        assert_eq!(exit, ExitCode::InternalFailure);
        assert_eq!(output.stdout(), "{\"code\":\"update_install_failed\"}\n");
        progress_events(output.stderr()).last().unwrap().clone()
    }

    fn artifact(bytes: &[u8]) -> UpdateArtifact {
        UpdateArtifact::for_test(
            current_target(),
            "artifact.zip",
            if cfg!(windows) {
                "wokcore.exe"
            } else {
                "wokcore"
            },
            u64::try_from(bytes.len()).unwrap(),
            format!("{:x}", Sha256::digest(bytes)),
            "https://github.com/hongjiadev/wokcore/releases/download/v1.2.3/artifact.zip",
        )
    }

    struct InstallFixture {
        _directory: tempfile::TempDir,
        archive: std::path::PathBuf,
        target: std::path::PathBuf,
        artifact: UpdateArtifact,
    }

    fn install_fixture() -> InstallFixture {
        let directory = private_tempdir();
        let archive = directory.path().join(if cfg!(windows) {
            "artifact.zip"
        } else {
            "artifact.tar.gz"
        });
        let target = directory.path().join(if cfg!(windows) {
            "wokcore.exe"
        } else {
            "wokcore"
        });
        fs::write(&target, b"old executable").unwrap();
        write_archive(&archive, b"new executable");
        let bytes = fs::read(&archive).unwrap();
        let artifact = UpdateArtifact::for_test(
            current_target(),
            archive.file_name().unwrap().to_string_lossy().into_owned(),
            target.file_name().unwrap().to_string_lossy().into_owned(),
            u64::try_from(bytes.len()).unwrap(),
            format!("{:x}", Sha256::digest(bytes)),
            "https://github.com/hongjiadev/wokcore/releases/download/v1.2.3/artifact",
        );
        InstallFixture {
            _directory: directory,
            archive,
            target,
            artifact,
        }
    }

    fn install_backup_path(target: &Path) -> PathBuf {
        target.parent().unwrap().join(format!(
            ".{}.previous",
            target.file_name().unwrap().to_string_lossy()
        ))
    }

    async fn run_full_path_rollback(
        fixture: &InstallFixture,
        lifecycle: &FullPathRollbackLifecycle,
        reporter: &mut ProgressReporter,
        output: &mut dyn CommandOutput,
    ) -> ExitCode {
        let archive = fs::File::open(&fixture.archive).unwrap();
        let result = install_verified(
            &archive,
            &fixture.artifact,
            &fixture.target,
            InstallVerificationContext {
                current_version: &Version::new(0, 1, 0),
                next_version: &Version::new(1, 2, 3),
                lifecycle,
                transactions: &PlatformUpdateTransactionFactory,
            },
            reporter,
            output,
            &ProgressDetails::default(),
        )
        .await;
        render_install_result(reporter, output, result)
    }

    #[cfg(windows)]
    fn write_archive(path: &Path, executable: &[u8]) {
        use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

        let file = fs::File::create(path).unwrap();
        let mut archive = ZipWriter::new(file);
        archive
            .start_file(
                "wokcore.exe",
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
            )
            .unwrap();
        archive.write_all(executable).unwrap();
        archive.finish().unwrap();
    }

    #[cfg(not(windows))]
    fn write_archive(path: &Path, executable: &[u8]) {
        use flate2::{Compression, write::GzEncoder};
        use tar::{Builder as TarBuilder, Header};

        let output = fs::File::create(path).unwrap();
        let encoder = GzEncoder::new(output, Compression::best());
        let mut archive = TarBuilder::new(encoder);
        let mut header = Header::new_ustar();
        header.set_size(u64::try_from(executable.len()).unwrap());
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(&mut header, "wokcore", executable)
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap();
    }

    fn test_paths(root: &Path) -> AppPaths {
        let runtime_dir = root.join("runtime");
        AppPaths {
            config_file: root.join("config.toml"),
            state_db: root.join("state.sqlite3"),
            runtime_dir: runtime_dir.clone(),
            log_dir: root.join("logs"),
            discovery_file: runtime_dir.join("discovery.json"),
            instance_lock: runtime_dir.join("instance.lock"),
        }
    }

    async fn bind_management_secret(paths: &AppPaths, secrets: &MemorySecretStore) {
        let mut state = StateStore::open(&paths.state_db).unwrap();
        let scope = SecretScope {
            provider_id: ProviderId::new("wokcore-runtime").unwrap(),
            account_id: None,
            purpose: SecretPurpose::Auxiliary,
        };
        let secret_ref = secrets
            .put(&scope, SecretString::from("synthetic-management-token"))
            .await
            .unwrap();
        state
            .bind_runtime_secret_if_absent("management", &secret_ref, "2026-07-28T00:00:00Z")
            .unwrap();
    }

    struct AlwaysRunningRuntime;

    impl Clock for AlwaysRunningRuntime {
        fn now(&self) -> Result<String, RuntimeValueError> {
            panic!("update lifecycle must not request a clock")
        }
    }

    impl IdSource for AlwaysRunningRuntime {
        fn new_instance_id(&self) -> Result<Uuid, RuntimeValueError> {
            panic!("update lifecycle must not generate an instance ID")
        }

        fn new_token_id(&self) -> Result<String, RuntimeValueError> {
            panic!("update lifecycle must not generate a token ID")
        }
    }

    impl ProcessIdentity for AlwaysRunningRuntime {
        fn current_pid(&self) -> u32 {
            4242
        }

        fn is_running(&self, pid: u32) -> bool {
            pid == 4242
        }
    }

    impl EntropySource for AlwaysRunningRuntime {
        fn fill(&self, _output: &mut [u8; 32]) -> Result<(), TokenError> {
            panic!("update lifecycle must not request entropy")
        }
    }

    impl ShutdownSignal for AlwaysRunningRuntime {
        fn wait(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            Box::pin(std::future::pending())
        }
    }

    struct MutableProcessRuntime {
        pid: u32,
        running: Arc<AtomicBool>,
    }

    impl Clock for MutableProcessRuntime {
        fn now(&self) -> Result<String, RuntimeValueError> {
            panic!("update lifecycle must not request a clock")
        }
    }

    impl IdSource for MutableProcessRuntime {
        fn new_instance_id(&self) -> Result<Uuid, RuntimeValueError> {
            panic!("update lifecycle must not generate an instance ID")
        }

        fn new_token_id(&self) -> Result<String, RuntimeValueError> {
            panic!("update lifecycle must not generate a token ID")
        }
    }

    impl ProcessIdentity for MutableProcessRuntime {
        fn current_pid(&self) -> u32 {
            self.pid
        }

        fn is_running(&self, pid: u32) -> bool {
            pid == self.pid && self.running.load(Ordering::Acquire)
        }
    }

    impl EntropySource for MutableProcessRuntime {
        fn fill(&self, _output: &mut [u8; 32]) -> Result<(), TokenError> {
            panic!("update lifecycle must not request entropy")
        }
    }

    impl ShutdownSignal for MutableProcessRuntime {
        fn wait(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            Box::pin(std::future::pending())
        }
    }

    struct SyntheticUpdateProcess {
        target: std::path::PathBuf,
        paths: AppPaths,
        base_url: String,
        instance_id: Uuid,
        running: Arc<AtomicBool>,
    }

    struct PendingUpdateProcess {
        target: std::path::PathBuf,
        running: Arc<AtomicBool>,
        spawned: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl UpdateProcess for PendingUpdateProcess {
        fn current_executable(&self) -> Result<std::path::PathBuf, RuntimeValueError> {
            Ok(self.target.clone())
        }

        async fn spawn_service(
            &self,
            executable: &Path,
        ) -> Result<Box<dyn UpdateChild>, RuntimeValueError> {
            assert_eq!(executable, self.target);
            self.running.store(true, Ordering::Release);
            self.spawned.notify_one();
            Ok(Box::new(PendingUpdateChild {
                running: self.running.clone(),
                detached: false,
            }))
        }
    }

    struct PendingUpdateChild {
        running: Arc<AtomicBool>,
        detached: bool,
    }

    #[async_trait]
    impl UpdateChild for PendingUpdateChild {
        fn pid(&self) -> Option<u32> {
            Some(6000)
        }

        async fn kill(&mut self) -> Result<(), RuntimeValueError> {
            self.running.store(false, Ordering::Release);
            Ok(())
        }

        fn detach(&mut self) {
            self.detached = true;
        }
    }

    impl Drop for PendingUpdateChild {
        fn drop(&mut self) {
            if !self.detached {
                self.running.store(false, Ordering::Release);
            }
        }
    }

    struct CorruptingUpdateChild {
        paths: AppPaths,
        running: Arc<AtomicBool>,
        detached: bool,
    }

    #[async_trait]
    impl UpdateChild for CorruptingUpdateChild {
        fn pid(&self) -> Option<u32> {
            Some(6100)
        }

        async fn kill(&mut self) -> Result<(), RuntimeValueError> {
            self.running.store(false, Ordering::Release);
            fs::write(&self.paths.discovery_file, b"{").unwrap();
            Ok(())
        }

        fn detach(&mut self) {
            self.detached = true;
        }
    }

    impl Drop for CorruptingUpdateChild {
        fn drop(&mut self) {
            if !self.detached {
                self.running.store(false, Ordering::Release);
            }
        }
    }

    struct SwitchingUpdateProcess {
        target: std::path::PathBuf,
        paths: AppPaths,
        base_url: String,
        current_instance: Arc<Mutex<Uuid>>,
        running: Arc<AtomicBool>,
        new_spawned: Arc<tokio::sync::Notify>,
        cleanup_started: Arc<tokio::sync::Notify>,
        allow_new_exit: Arc<tokio::sync::Notify>,
        old_spawns: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl UpdateProcess for SwitchingUpdateProcess {
        fn current_executable(&self) -> Result<std::path::PathBuf, RuntimeValueError> {
            Ok(self.target.clone())
        }

        async fn spawn_service(
            &self,
            executable: &Path,
        ) -> Result<Box<dyn UpdateChild>, RuntimeValueError> {
            assert_eq!(executable, self.target);
            self.running.store(true, Ordering::Release);
            if fs::read(executable).unwrap() == b"new executable" {
                self.new_spawned.notify_one();
                return Ok(Box::new(SwitchingUpdateChild {
                    running: self.running.clone(),
                    detached: false,
                    cleanup: Some(SwitchingCleanup {
                        paths: self.paths.clone(),
                        base_url: self.base_url.clone(),
                        current_instance: self.current_instance.clone(),
                        instance_id: Uuid::new_v4(),
                        cleanup_started: self.cleanup_started.clone(),
                        allow_exit: self.allow_new_exit.clone(),
                    }),
                }));
            }
            assert_eq!(fs::read(executable).unwrap(), b"old executable");
            let instance_id = Uuid::new_v4();
            *self.current_instance.lock().unwrap() = instance_id;
            DiscoveryStore::new(&self.paths)
                .unwrap()
                .publish(&DiscoveryRecord {
                    base_url: self.base_url.clone(),
                    pid: 4242,
                    instance_id,
                    wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
                    api_major: 1,
                })
                .unwrap();
            self.old_spawns.fetch_add(1, Ordering::AcqRel);
            Ok(Box::new(SwitchingUpdateChild {
                running: self.running.clone(),
                detached: false,
                cleanup: None,
            }))
        }
    }

    struct SwitchingUpdateChild {
        running: Arc<AtomicBool>,
        detached: bool,
        cleanup: Option<SwitchingCleanup>,
    }

    struct SwitchingCleanup {
        paths: AppPaths,
        base_url: String,
        current_instance: Arc<Mutex<Uuid>>,
        instance_id: Uuid,
        cleanup_started: Arc<tokio::sync::Notify>,
        allow_exit: Arc<tokio::sync::Notify>,
    }

    impl SwitchingCleanup {
        fn publish(&self) {
            *self.current_instance.lock().unwrap() = self.instance_id;
            DiscoveryStore::new(&self.paths)
                .unwrap()
                .publish(&DiscoveryRecord {
                    base_url: self.base_url.clone(),
                    pid: 4242,
                    instance_id: self.instance_id,
                    wokcore_version: "1.2.3".to_owned(),
                    api_major: 1,
                })
                .unwrap();
            self.cleanup_started.notify_one();
        }
    }

    #[async_trait]
    impl UpdateChild for SwitchingUpdateChild {
        fn pid(&self) -> Option<u32> {
            Some(4242)
        }

        async fn kill(&mut self) -> Result<(), RuntimeValueError> {
            if let Some(cleanup) = self.cleanup.take() {
                cleanup.publish();
                cleanup.allow_exit.notified().await;
            }
            self.running.store(false, Ordering::Release);
            Ok(())
        }

        fn detach(&mut self) {
            self.detached = true;
        }
    }

    impl Drop for SwitchingUpdateChild {
        fn drop(&mut self) {
            if !self.detached {
                if let Some(cleanup) = self.cleanup.take() {
                    cleanup.publish();
                    let running = self.running.clone();
                    tokio::spawn(async move {
                        cleanup.allow_exit.notified().await;
                        running.store(false, Ordering::Release);
                    });
                } else {
                    self.running.store(false, Ordering::Release);
                }
            }
        }
    }

    struct RecoveryUpdateProcess {
        target: std::path::PathBuf,
        paths: AppPaths,
        base_url: String,
        current_instance: Arc<Mutex<Uuid>>,
        running: Arc<AtomicBool>,
        spawns: Arc<AtomicUsize>,
    }

    struct RollbackUpdateProcess {
        target: std::path::PathBuf,
        paths: AppPaths,
        base_url: String,
        current_instance: Arc<Mutex<Uuid>>,
        running: Arc<AtomicBool>,
        spawns: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl UpdateProcess for RollbackUpdateProcess {
        fn current_executable(&self) -> Result<std::path::PathBuf, RuntimeValueError> {
            Ok(self.target.clone())
        }

        async fn spawn_service(
            &self,
            executable: &Path,
        ) -> Result<Box<dyn UpdateChild>, RuntimeValueError> {
            assert_eq!(executable, self.target);
            let is_new = fs::read(executable).unwrap() == b"new executable";
            let instance_id = Uuid::new_v4();
            *self.current_instance.lock().unwrap() = instance_id;
            self.running.store(true, Ordering::Release);
            DiscoveryStore::new(&self.paths)
                .unwrap()
                .publish(&DiscoveryRecord {
                    base_url: self.base_url.clone(),
                    pid: 4242,
                    instance_id,
                    wokcore_version: if is_new {
                        "9.9.9".to_owned()
                    } else {
                        env!("CARGO_PKG_VERSION").to_owned()
                    },
                    api_major: 1,
                })
                .unwrap();
            self.spawns.fetch_add(1, Ordering::AcqRel);
            Ok(Box::new(RollbackUpdateChild {
                running: self.running.clone(),
                detached: false,
            }))
        }
    }

    struct RollbackUpdateChild {
        running: Arc<AtomicBool>,
        detached: bool,
    }

    #[async_trait]
    impl UpdateChild for RollbackUpdateChild {
        fn pid(&self) -> Option<u32> {
            Some(4242)
        }

        async fn kill(&mut self) -> Result<(), RuntimeValueError> {
            self.running.store(false, Ordering::Release);
            Ok(())
        }

        fn detach(&mut self) {
            self.detached = true;
        }
    }

    impl Drop for RollbackUpdateChild {
        fn drop(&mut self) {
            if !self.detached {
                self.running.store(false, Ordering::Release);
            }
        }
    }

    #[async_trait]
    impl UpdateProcess for RecoveryUpdateProcess {
        fn current_executable(&self) -> Result<std::path::PathBuf, RuntimeValueError> {
            Ok(self.target.clone())
        }

        async fn spawn_service(
            &self,
            executable: &Path,
        ) -> Result<Box<dyn UpdateChild>, RuntimeValueError> {
            assert_eq!(executable, self.target);
            assert_eq!(fs::read(executable).unwrap(), b"old executable");
            let instance_id = Uuid::new_v4();
            *self.current_instance.lock().unwrap() = instance_id;
            self.running.store(true, Ordering::Release);
            DiscoveryStore::new(&self.paths)
                .unwrap()
                .publish(&DiscoveryRecord {
                    base_url: self.base_url.clone(),
                    pid: 4242,
                    instance_id,
                    wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
                    api_major: 1,
                })
                .unwrap();
            self.spawns.fetch_add(1, Ordering::AcqRel);
            Ok(Box::new(RecoveryUpdateChild {
                running: self.running.clone(),
                detached: false,
            }))
        }
    }

    struct RecoveryUpdateChild {
        running: Arc<AtomicBool>,
        detached: bool,
    }

    #[async_trait]
    impl UpdateChild for RecoveryUpdateChild {
        fn pid(&self) -> Option<u32> {
            Some(4242)
        }

        async fn kill(&mut self) -> Result<(), RuntimeValueError> {
            self.running.store(false, Ordering::Release);
            Ok(())
        }

        fn detach(&mut self) {
            self.detached = true;
        }
    }

    impl Drop for RecoveryUpdateChild {
        fn drop(&mut self) {
            if !self.detached {
                self.running.store(false, Ordering::Release);
            }
        }
    }

    #[async_trait]
    impl UpdateProcess for SyntheticUpdateProcess {
        fn current_executable(&self) -> Result<std::path::PathBuf, RuntimeValueError> {
            Ok(self.target.clone())
        }

        async fn spawn_service(
            &self,
            executable: &Path,
        ) -> Result<Box<dyn UpdateChild>, RuntimeValueError> {
            assert_eq!(executable, self.target);
            assert_eq!(fs::read(executable).unwrap(), b"new executable");
            self.running.store(true, Ordering::Release);
            DiscoveryStore::new(&self.paths)
                .unwrap()
                .publish(&DiscoveryRecord {
                    base_url: self.base_url.clone(),
                    pid: 5000,
                    instance_id: self.instance_id,
                    wokcore_version: "1.2.3".to_owned(),
                    api_major: 1,
                })
                .unwrap();
            Ok(Box::new(SyntheticUpdateChild {
                running: self.running.clone(),
                detached: false,
            }))
        }
    }

    struct SyntheticUpdateChild {
        running: Arc<AtomicBool>,
        detached: bool,
    }

    #[async_trait]
    impl UpdateChild for SyntheticUpdateChild {
        fn pid(&self) -> Option<u32> {
            Some(5000)
        }

        async fn kill(&mut self) -> Result<(), RuntimeValueError> {
            self.running.store(false, Ordering::Release);
            Ok(())
        }

        fn detach(&mut self) {
            self.detached = true;
        }
    }

    impl Drop for SyntheticUpdateChild {
        fn drop(&mut self) {
            if !self.detached {
                self.running.store(false, Ordering::Release);
            }
        }
    }

    struct FakeLifecycle {
        preparation: LifecyclePreparation,
        health_succeeds: bool,
        events: Arc<Mutex<Vec<&'static str>>>,
        verified_state: Mutex<Option<OriginalServiceState>>,
        prepare_hook: Option<Box<dyn Fn() + Send + Sync>>,
        recovery_required: bool,
        restore_succeeds: bool,
    }

    #[derive(Default)]
    struct BrokenPipeOutput {
        stdout: String,
        stderr_attempts: usize,
        first_stderr: Option<String>,
    }

    impl CommandOutput for BrokenPipeOutput {
        fn write_stdout(&mut self, value: &str) -> io::Result<()> {
            self.stdout.push_str(value);
            Ok(())
        }

        fn write_stderr(&mut self, value: &str) -> io::Result<()> {
            self.stderr_attempts += 1;
            self.first_stderr.get_or_insert_with(|| value.to_owned());
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "progress consumer closed",
            ))
        }
    }

    struct FullPathRollbackLifecycle {
        target: PathBuf,
        backup: PathBuf,
        events: Mutex<Vec<&'static str>>,
        target_during_verification: Mutex<Option<Vec<u8>>>,
        backup_during_verification: Mutex<Option<Vec<u8>>>,
        target_during_restore: Mutex<Option<Vec<u8>>>,
        backup_existed_during_restore: Mutex<Option<bool>>,
    }

    impl FullPathRollbackLifecycle {
        fn new(target: &Path) -> Self {
            Self {
                target: target.to_owned(),
                backup: install_backup_path(target),
                events: Mutex::new(Vec::new()),
                target_during_verification: Mutex::new(None),
                backup_during_verification: Mutex::new(None),
                target_during_restore: Mutex::new(None),
                backup_existed_during_restore: Mutex::new(None),
            }
        }

        fn assert_complete_rollback(&self) {
            assert_eq!(
                *self.target_during_verification.lock().unwrap(),
                Some(b"new executable".to_vec())
            );
            assert_eq!(
                *self.backup_during_verification.lock().unwrap(),
                Some(b"old executable".to_vec())
            );
            assert_eq!(
                *self.target_during_restore.lock().unwrap(),
                Some(b"old executable".to_vec())
            );
            assert_eq!(
                *self.backup_existed_during_restore.lock().unwrap(),
                Some(false)
            );
            assert_eq!(
                *self.events.lock().unwrap(),
                vec!["prepare", "verify", "restore"]
            );
        }
    }

    #[async_trait]
    impl UpdateLifecycle for FullPathRollbackLifecycle {
        async fn prepare(
            &self,
            _reporter: &mut ProgressReporter,
            _output: &mut dyn CommandOutput,
            _versions: &ProgressDetails,
        ) -> Result<PreparedLifecycle, ()> {
            self.events.lock().unwrap().push("prepare");
            Ok(PreparedLifecycle::plain(LifecyclePreparation::Ready(
                OriginalServiceState::Running,
            )))
        }

        async fn verify_installed(
            &self,
            _version: &Version,
            _original: OriginalServiceState,
            _reporter: &mut ProgressReporter,
            _output: &mut dyn CommandOutput,
            _versions: &ProgressDetails,
        ) -> Result<(), VerificationFailure> {
            self.events.lock().unwrap().push("verify");
            *self.target_during_verification.lock().unwrap() =
                Some(fs::read(&self.target).unwrap());
            *self.backup_during_verification.lock().unwrap() =
                Some(fs::read(&self.backup).unwrap());
            Err(VerificationFailure::SafeToRollback)
        }

        async fn restore_previous(
            &self,
            _version: &Version,
            _original: OriginalServiceState,
            _reporter: &mut ProgressReporter,
            _output: &mut dyn CommandOutput,
            _versions: &ProgressDetails,
        ) -> Result<(), ()> {
            self.events.lock().unwrap().push("restore");
            *self.target_during_restore.lock().unwrap() = Some(fs::read(&self.target).unwrap());
            *self.backup_existed_during_restore.lock().unwrap() = Some(self.backup.exists());
            Ok(())
        }
    }

    impl FakeLifecycle {
        fn new(preparation: LifecyclePreparation, health_succeeds: bool) -> Self {
            Self {
                preparation,
                health_succeeds,
                events: Arc::new(Mutex::new(Vec::new())),
                verified_state: Mutex::new(None),
                prepare_hook: None,
                recovery_required: false,
                restore_succeeds: true,
            }
        }

        fn with_prepare_hook(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
            self.prepare_hook = Some(Box::new(hook));
            self
        }

        fn with_recovery_required(mut self) -> Self {
            self.recovery_required = true;
            self
        }

        fn with_restore_failure(mut self) -> Self {
            self.restore_succeeds = false;
            self
        }

        fn events(&self) -> Vec<&'static str> {
            self.events.lock().unwrap().clone()
        }

        fn verified_state(&self) -> Option<OriginalServiceState> {
            *self.verified_state.lock().unwrap()
        }
    }

    #[async_trait]
    impl UpdateLifecycle for FakeLifecycle {
        async fn prepare(
            &self,
            _reporter: &mut ProgressReporter,
            _output: &mut dyn CommandOutput,
            _versions: &ProgressDetails,
        ) -> Result<PreparedLifecycle, ()> {
            self.events.lock().unwrap().push("prepare");
            if let Some(hook) = &self.prepare_hook {
                hook();
            }
            Ok(PreparedLifecycle::plain(self.preparation))
        }

        async fn verify_installed(
            &self,
            _version: &Version,
            original: OriginalServiceState,
            _reporter: &mut ProgressReporter,
            _output: &mut dyn CommandOutput,
            _versions: &ProgressDetails,
        ) -> Result<(), VerificationFailure> {
            self.events.lock().unwrap().push("verify");
            *self.verified_state.lock().unwrap() = Some(original);
            if self.health_succeeds {
                Ok(())
            } else if self.recovery_required {
                Err(VerificationFailure::RecoveryRequired)
            } else {
                Err(VerificationFailure::SafeToRollback)
            }
        }

        async fn restore_previous(
            &self,
            _version: &Version,
            _original: OriginalServiceState,
            _reporter: &mut ProgressReporter,
            _output: &mut dyn CommandOutput,
            _versions: &ProgressDetails,
        ) -> Result<(), ()> {
            self.events.lock().unwrap().push("restore");
            self.restore_succeeds.then_some(()).ok_or(())
        }
    }
}
