use std::{
    fmt,
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime},
};

use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time,
};
use wokcore_diagnostics::{
    event::{
        BuildIdentity, CapabilityVersion, DiagnosticBuildError, DiagnosticComponent,
        DiagnosticDropCounts, DiagnosticEventCode, DiagnosticEventDraft, DiagnosticLevel, EventId,
        GitCommit, UtcTimestamp, WokcoreVersion,
    },
    recorder::{
        BarrierAdmissionError, DiagnosticRecorder, RecordOutcome, RecorderOwner, RecorderShutdown,
    },
    segment::{
        BoxedDurableWriterOwner, DiagnosticDropSummary, DurableDropRequests, DurableProcessError,
        DurableProcessOutcome, DurableProducer, DurableWorkOutcome,
    },
    snapshot::{SnapshotOwner, SnapshotRecorder, SnapshotShutdown},
};
use wokcore_storage::{
    StateStore, StateStoreWriter, StateStoreWriterClient, StateStoreWriterShutdownHandle,
    state_store_writer,
};

use crate::{auth::EntropySource, observability::ScanTimestampSource, runtime::generate_uuid_v4};

pub const SESSION_BATCH_ROWS: usize = 512;
pub const SESSION_BATCH_UTF8_BYTES: usize = 512 * 1024;
pub const SESSION_BATCH_QUEUE_CAPACITY: usize = 4;
pub const SESSION_PRODUCER_SLICE: Duration = Duration::from_millis(25);
pub const SESSION_PARTIAL_FLUSH_INTERVAL: Duration = Duration::from_millis(250);
pub const DIAGNOSTIC_PARTIAL_FLUSH_INTERVAL: Duration = Duration::from_millis(250);
pub const IDLE_TRUNCATE_INTERVAL: Duration = Duration::from_secs(60);
const WRITER_COMMAND_CAPACITY: usize = 1;
const BARRIER_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct StateWriterHandle {
    client: StateStoreWriterClient,
}

impl StateWriterHandle {
    pub fn client(&self) -> &StateStoreWriterClient {
        &self.client
    }

    pub fn has_been_idle_for(&self, duration: Duration) -> bool {
        self.client.has_been_idle_for(duration)
    }
}

impl fmt::Debug for StateWriterHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StateWriterHandle([redacted])")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StateWriterError {
    #[error("state writer storage operation failed")]
    Storage,
    #[error("state writer command admission failed")]
    Admission,
    #[error("state writer task failed")]
    Task,
}

pub struct PreparedStateWriter {
    handle: StateWriterHandle,
    shutdown: Option<StateStoreWriterShutdownHandle>,
    writer: Option<StateStoreWriter>,
}

impl PreparedStateWriter {
    pub fn open(
        state_path: impl AsRef<Path>,
    ) -> Result<(StateWriterHandle, Self), StateWriterError> {
        let store = StateStore::open(state_path).map_err(|_| StateWriterError::Storage)?;
        store.health().map_err(|_| StateWriterError::Storage)?;
        let (client, shutdown, writer) = state_store_writer(store);
        let handle = StateWriterHandle { client };
        Ok((
            handle.clone(),
            Self {
                handle,
                shutdown: Some(shutdown),
                writer: Some(writer),
            },
        ))
    }

    pub fn start(mut self) -> Result<RunningStateWriter, StateWriterError> {
        let writer = self.writer.take().ok_or(StateWriterError::Task)?;
        let thread = thread::Builder::new()
            .name("wokcore-state-writer".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .map_err(|_| StateWriterError::Task)?;
                runtime.block_on(writer.run());
                Ok(())
            })
            .map_err(|_| StateWriterError::Task)?;
        Ok(RunningStateWriter {
            handle: self.handle,
            shutdown: self.shutdown.take(),
            thread: Some(thread),
        })
    }
}

impl fmt::Debug for PreparedStateWriter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PreparedStateWriter([redacted])")
    }
}

pub struct RunningStateWriter {
    handle: StateWriterHandle,
    shutdown: Option<StateStoreWriterShutdownHandle>,
    thread: Option<thread::JoinHandle<Result<(), StateWriterError>>>,
}

impl RunningStateWriter {
    pub fn handle(&self) -> StateWriterHandle {
        self.handle.clone()
    }

    pub async fn flush(&self) -> Result<(), StateWriterError> {
        loop {
            match self.handle.client.try_flush() {
                Ok(receipt) => {
                    return receipt.wait().await.map_err(|_| StateWriterError::Storage);
                }
                Err(wokcore_storage::StateStoreWriterSubmitError::QueueFull) => {
                    tokio::task::yield_now().await;
                }
                Err(wokcore_storage::StateStoreWriterSubmitError::WriterClosed) => {
                    return Err(StateWriterError::Admission);
                }
            }
        }
    }

    pub async fn checkpoint_and_shutdown(
        mut self,
        proxy_idle: bool,
    ) -> Result<(), StateWriterError> {
        let mut first_error = self.flush().await.err();
        let truncate_if_idle_for = proxy_idle.then_some(IDLE_TRUNCATE_INTERVAL);
        let checkpoint = loop {
            match self.handle.client.try_checkpoint(truncate_if_idle_for) {
                Ok(receipt) => break Some(receipt),
                Err(wokcore_storage::StateStoreWriterSubmitError::QueueFull) => {
                    tokio::task::yield_now().await;
                }
                Err(wokcore_storage::StateStoreWriterSubmitError::WriterClosed) => {
                    remember_first(&mut first_error, StateWriterError::Admission);
                    break None;
                }
            }
        };
        if let Some(checkpoint) = checkpoint
            && checkpoint.wait().await.is_err()
        {
            remember_first(&mut first_error, StateWriterError::Storage);
        }
        match self.shutdown.take() {
            Some(shutdown) => match shutdown.shutdown().await {
                Ok(stopped) => {
                    if stopped.wait().await.is_err() {
                        remember_first(&mut first_error, StateWriterError::Task);
                    }
                }
                Err(_) => remember_first(&mut first_error, StateWriterError::Admission),
            },
            None => remember_first(&mut first_error, StateWriterError::Task),
        }
        match self.thread.take() {
            Some(thread) => match tokio::task::spawn_blocking(move || thread.join()).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => remember_first(&mut first_error, error),
                Ok(Err(_)) | Err(_) => {
                    remember_first(&mut first_error, StateWriterError::Task);
                }
            },
            None => remember_first(&mut first_error, StateWriterError::Task),
        }
        first_error.map_or(Ok(()), Err)
    }
}

fn remember_first<T>(first: &mut Option<T>, error: T) {
    if first.is_none() {
        *first = Some(error);
    }
}

impl Drop for RunningStateWriter {
    fn drop(&mut self) {
        let shutdown = self.shutdown.take();
        let thread = self.thread.take();
        if shutdown.is_none() && thread.is_none() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        std::mem::drop(runtime.spawn(async move {
            if let Some(shutdown) = shutdown
                && let Ok(stopped) = shutdown.shutdown().await
            {
                let _ = stopped.wait().await;
            }
            if let Some(thread) = thread {
                let _ = tokio::task::spawn_blocking(move || thread.join()).await;
            }
        }));
    }
}

#[derive(Clone)]
pub struct DiagnosticWriterHandle {
    recorder: DiagnosticRecorder,
    snapshots: SnapshotRecorder,
    last_durable_activity_at: Arc<Mutex<Instant>>,
}

impl DiagnosticWriterHandle {
    pub fn recorder(&self) -> &DiagnosticRecorder {
        &self.recorder
    }

    pub fn snapshots(&self) -> &SnapshotRecorder {
        &self.snapshots
    }

    pub fn has_been_idle_for(&self, duration: Duration) -> bool {
        self.last_durable_activity_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed()
            >= duration
    }
}

impl fmt::Debug for DiagnosticWriterHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DiagnosticWriterHandle([redacted])")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DiagnosticWriterError {
    #[error("diagnostic writer filesystem setup failed")]
    Io,
    #[error("diagnostic writer event construction failed")]
    Build,
    #[error("diagnostic writer persistence failed")]
    Durable,
    #[error("diagnostic writer task failed")]
    Task,
    #[error("diagnostic writer recorder barrier failed")]
    Barrier,
}

pub struct PreparedDiagnosticWriter {
    handle: DiagnosticWriterHandle,
    recorder_owner: Option<RecorderOwner>,
    durable_owner: Option<BoxedDurableWriterOwner>,
    drop_requests: Option<DurableDropRequests>,
    snapshot_owner: Option<SnapshotOwner>,
    factory: DiagnosticDropEventFactory,
}

impl PreparedDiagnosticWriter {
    pub fn open(
        root: impl AsRef<Path>,
        entropy: Arc<dyn EntropySource>,
        clock: Arc<dyn ScanTimestampSource>,
    ) -> Result<(DiagnosticWriterHandle, Self), DiagnosticWriterError> {
        let root = root.as_ref();
        std::fs::create_dir_all(root).map_err(|_| DiagnosticWriterError::Io)?;
        let (producer, mut durable_owner, drop_requests) =
            DurableProducer::with_drop_requests(root);
        let recovery = durable_owner
            .recover_startup(SystemTime::now())
            .map_err(|_| DiagnosticWriterError::Durable)?;
        let (recorder, recorder_owner) = DiagnosticRecorder::new();
        let recorder_owner = recorder_owner
            .with_recovered_durable_producer(producer, recovery)
            .map_err(|_| DiagnosticWriterError::Build)?;
        let (snapshots, snapshot_owner) = SnapshotRecorder::new(root);
        let handle = DiagnosticWriterHandle {
            recorder,
            snapshots,
            last_durable_activity_at: Arc::new(Mutex::new(Instant::now())),
        };
        Ok((
            handle.clone(),
            Self {
                handle,
                recorder_owner: Some(recorder_owner),
                durable_owner: Some(durable_owner),
                drop_requests: Some(drop_requests),
                snapshot_owner: Some(snapshot_owner),
                factory: DiagnosticDropEventFactory { entropy, clock },
            },
        ))
    }

    pub fn start(mut self) -> Result<RunningDiagnosticWriter, DiagnosticWriterError> {
        let recorder_owner = self
            .recorder_owner
            .take()
            .ok_or(DiagnosticWriterError::Task)?;
        let recorder_shutdown = recorder_owner.shutdown_handle();
        let snapshot_owner = self
            .snapshot_owner
            .take()
            .ok_or(DiagnosticWriterError::Task)?;
        let durable_owner = self
            .durable_owner
            .take()
            .ok_or(DiagnosticWriterError::Task)?;
        let drop_requests = self
            .drop_requests
            .take()
            .ok_or(DiagnosticWriterError::Task)?;
        let snapshot_shutdown = snapshot_owner.shutdown_handle();
        let snapshot_thread = spawn_owner_thread("wokcore-snapshot-writer", async move {
            snapshot_owner.run().await;
            Ok(())
        })?;

        let recorder = self.handle.recorder.clone();
        let factory = self.factory;
        let last_durable_activity_at = Arc::clone(&self.handle.last_durable_activity_at);
        let (commands, command_receiver) = mpsc::channel(WRITER_COMMAND_CAPACITY);
        let durable_thread = match spawn_owner_thread("wokcore-diagnostic-writer", async move {
            run_durable_writer(
                durable_owner,
                drop_requests,
                recorder,
                factory,
                command_receiver,
                last_durable_activity_at,
            )
            .await
        }) {
            Ok(thread) => thread,
            Err(error) => {
                snapshot_shutdown.request();
                let _ = snapshot_thread.join();
                return Err(error);
            }
        };
        let recorder_task = tokio::spawn(recorder_owner.run());

        Ok(RunningDiagnosticWriter {
            handle: self.handle,
            recorder_shutdown,
            recorder_task: Some(recorder_task),
            commands: Some(commands),
            durable_thread: Some(durable_thread),
            snapshot_shutdown,
            snapshot_thread: Some(snapshot_thread),
        })
    }
}

impl fmt::Debug for PreparedDiagnosticWriter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PreparedDiagnosticWriter([redacted])")
    }
}

pub struct RunningDiagnosticWriter {
    handle: DiagnosticWriterHandle,
    recorder_shutdown: RecorderShutdown,
    recorder_task: Option<JoinHandle<()>>,
    commands: Option<mpsc::Sender<WriterCommand>>,
    durable_thread: Option<thread::JoinHandle<Result<(), DiagnosticWriterError>>>,
    snapshot_shutdown: SnapshotShutdown,
    snapshot_thread: Option<thread::JoinHandle<Result<(), DiagnosticWriterError>>>,
}

impl RunningDiagnosticWriter {
    pub fn handle(&self) -> DiagnosticWriterHandle {
        self.handle.clone()
    }

    pub async fn shutdown(mut self) -> Result<(), DiagnosticWriterError> {
        let mut first_error = None;
        if let Err(error) = close_recorder_ingress(&self.handle.recorder).await {
            remember_first(&mut first_error, error);
        }
        match self.commands.as_ref() {
            Some(commands) => {
                let (reply, receipt) = oneshot::channel();
                if commands.send(WriterCommand::Flush { reply }).await.is_err() {
                    remember_first(&mut first_error, DiagnosticWriterError::Task);
                } else {
                    match receipt.await {
                        Ok(Ok(())) => {}
                        Ok(Err(())) => {
                            remember_first(&mut first_error, DiagnosticWriterError::Durable);
                        }
                        Err(_) => {
                            remember_first(&mut first_error, DiagnosticWriterError::Task);
                        }
                    }
                }
            }
            None => remember_first(&mut first_error, DiagnosticWriterError::Task),
        }
        self.recorder_shutdown.request();
        match self.recorder_task.take() {
            Some(recorder_task) => {
                if recorder_task.await.is_err() {
                    remember_first(&mut first_error, DiagnosticWriterError::Task);
                }
            }
            None => remember_first(&mut first_error, DiagnosticWriterError::Task),
        }
        drop(self.commands.take());

        self.snapshot_shutdown.request();
        if let Err(error) = join_owner_thread(self.durable_thread.take()).await {
            remember_first(&mut first_error, error);
        }
        if let Err(error) = join_owner_thread(self.snapshot_thread.take()).await {
            remember_first(&mut first_error, error);
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for RunningDiagnosticWriter {
    fn drop(&mut self) {
        self.recorder_shutdown.close_ingress();
        let recorder = self.handle.recorder.clone();
        let recorder_shutdown = self.recorder_shutdown.clone();
        let snapshot_shutdown = self.snapshot_shutdown.clone();
        let commands = self.commands.take();
        let recorder_task = self.recorder_task.take();
        let durable_thread = self.durable_thread.take();
        let snapshot_thread = self.snapshot_thread.take();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            recorder_shutdown.request();
            snapshot_shutdown.request();
            drop(commands);
            return;
        };
        std::mem::drop(runtime.spawn(async move {
            let _ = close_recorder_ingress(&recorder).await;
            if let Some(commands) = commands.as_ref() {
                let (reply, receipt) = oneshot::channel();
                if commands.send(WriterCommand::Flush { reply }).await.is_ok() {
                    let _ = receipt.await;
                }
            }
            recorder_shutdown.request();
            if let Some(recorder_task) = recorder_task {
                let _ = recorder_task.await;
            }
            drop(commands);
            snapshot_shutdown.request();
            let _ = join_owner_thread(durable_thread).await;
            let _ = join_owner_thread(snapshot_thread).await;
        }));
    }
}

enum WriterCommand {
    Flush {
        reply: oneshot::Sender<Result<(), ()>>,
    },
}

struct DiagnosticDropEventFactory {
    entropy: Arc<dyn EntropySource>,
    clock: Arc<dyn ScanTimestampSource>,
}

impl DiagnosticDropEventFactory {
    fn draft(
        &self,
        summary: DiagnosticDropSummary,
    ) -> Result<DiagnosticEventDraft, DiagnosticWriterError> {
        let event_id = generate_uuid_v4(self.entropy.as_ref())
            .map_err(|_| DiagnosticWriterError::Build)?
            .to_string();
        let occurred_at = self.clock.now().ok_or(DiagnosticWriterError::Build)?;
        let version = WokcoreVersion::parse(env!("CARGO_PKG_VERSION"))
            .map_err(|_| DiagnosticWriterError::Build)?;
        let git_commit = GitCommit::parse(
            option_env!("WOKCORE_GIT_COMMIT").unwrap_or("0000000000000000000000000000000000000000"),
        )
        .map_err(|_| DiagnosticWriterError::Build)?;
        Ok(DiagnosticEventDraft::new(
            EventId::parse(&event_id).map_err(|_| DiagnosticWriterError::Build)?,
            UtcTimestamp::parse(&occurred_at).map_err(|_| DiagnosticWriterError::Build)?,
            DiagnosticLevel::Warn,
            DiagnosticComponent::Diagnostics,
            DiagnosticEventCode::DiagnosticDrop,
            BuildIdentity::new(version, git_commit, 1, CapabilityVersion::new(1)),
        )
        .with_diagnostic_drop_counts(DiagnosticDropCounts::new(
            summary.ingress_full(),
            summary.ingress_closed(),
            summary.writer_unavailable(),
            summary.invalid_event(),
            summary.oversized_event(),
        )))
    }
}

async fn run_durable_writer(
    mut owner: BoxedDurableWriterOwner,
    mut drop_requests: DurableDropRequests,
    recorder: DiagnosticRecorder,
    factory: DiagnosticDropEventFactory,
    mut commands: mpsc::Receiver<WriterCommand>,
    last_durable_activity_at: Arc<Mutex<Instant>>,
) -> Result<(), DiagnosticWriterError> {
    let mut commands_open = true;
    loop {
        tokio::select! {
            biased;
            command = commands.recv(), if commands_open => {
                match command {
                    Some(WriterCommand::Flush { reply }) => {
                        let result = flush_durable(
                            &mut owner,
                            &mut drop_requests,
                            &recorder,
                            &factory,
                            &last_durable_activity_at,
                        )
                        .await
                        .map_err(|_| ());
                        let failed = result.is_err();
                        let _ = reply.send(result);
                        if failed {
                            return Err(DiagnosticWriterError::Durable);
                        }
                    }
                    None => commands_open = false,
                }
            }
            work = owner.wait_process_next_batched(DIAGNOSTIC_PARTIAL_FLUSH_INTERVAL) => {
                match work.map_err(|_| DiagnosticWriterError::Durable)? {
                    DurableWorkOutcome::Closed => return Ok(()),
                    DurableWorkOutcome::Written { .. } => {
                        observe_durable_activity(&last_durable_activity_at);
                    }
                    DurableWorkOutcome::DropSummaryRequested => {
                        forward_drop_summary(
                            &mut drop_requests,
                            &recorder,
                            &factory,
                        )
                        .await?;
                    }
                }
            }
        }
    }
}

async fn flush_durable(
    owner: &mut BoxedDurableWriterOwner,
    drop_requests: &mut DurableDropRequests,
    recorder: &DiagnosticRecorder,
    factory: &DiagnosticDropEventFactory,
    last_durable_activity_at: &Mutex<Instant>,
) -> Result<(), DiagnosticWriterError> {
    loop {
        match owner
            .try_process_next()
            .map_err(|_| DiagnosticWriterError::Durable)?
        {
            DurableProcessOutcome::Idle => return Ok(()),
            DurableProcessOutcome::Written { .. } => {
                observe_durable_activity(last_durable_activity_at);
            }
            DurableProcessOutcome::DropSummaryRequested => {
                forward_drop_summary(drop_requests, recorder, factory).await?;
            }
        }
    }
}

async fn forward_drop_summary(
    drop_requests: &mut DurableDropRequests,
    recorder: &DiagnosticRecorder,
    factory: &DiagnosticDropEventFactory,
) -> Result<(), DiagnosticWriterError> {
    let request = drop_requests
        .recv()
        .await
        .ok_or(DiagnosticWriterError::Durable)?;
    let draft = factory.draft(request.summary())?;
    let outcome = recorder.try_record_internal(Ok::<_, DiagnosticBuildError>(draft));
    if outcome != RecordOutcome::Accepted {
        return Err(DiagnosticWriterError::Durable);
    }
    recorder_barrier(recorder).await?;
    request.acknowledge();
    Ok(())
}

fn observe_durable_activity(last_activity_at: &Mutex<Instant>) {
    *last_activity_at
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
}

async fn recorder_barrier(recorder: &DiagnosticRecorder) -> Result<(), DiagnosticWriterError> {
    let deadline = time::Instant::now() + BARRIER_DEADLINE;
    loop {
        match recorder.try_barrier() {
            Ok(barrier) => {
                return barrier
                    .wait_with_deadline(BARRIER_DEADLINE)
                    .await
                    .map_err(|_| DiagnosticWriterError::Barrier);
            }
            Err(BarrierAdmissionError::Busy) if time::Instant::now() < deadline => {
                tokio::task::yield_now().await;
            }
            Err(BarrierAdmissionError::Busy | BarrierAdmissionError::Closed) => {
                return Err(DiagnosticWriterError::Barrier);
            }
        }
    }
}

async fn close_recorder_ingress(
    recorder: &DiagnosticRecorder,
) -> Result<(), DiagnosticWriterError> {
    let deadline = time::Instant::now() + BARRIER_DEADLINE;
    loop {
        match recorder.try_close_ingress_barrier() {
            Ok(barrier) => {
                return barrier
                    .wait_with_deadline(BARRIER_DEADLINE)
                    .await
                    .map_err(|_| DiagnosticWriterError::Barrier);
            }
            Err(BarrierAdmissionError::Busy) if time::Instant::now() < deadline => {
                tokio::task::yield_now().await;
            }
            Err(BarrierAdmissionError::Busy | BarrierAdmissionError::Closed) => {
                return Err(DiagnosticWriterError::Barrier);
            }
        }
    }
}

fn spawn_owner_thread(
    name: &'static str,
    future: impl FutureResult,
) -> Result<thread::JoinHandle<Result<(), DiagnosticWriterError>>, DiagnosticWriterError> {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .map_err(|_| DiagnosticWriterError::Task)?;
            runtime.block_on(future)
        })
        .map_err(|_| DiagnosticWriterError::Task)
}

trait FutureResult:
    std::future::Future<Output = Result<(), DiagnosticWriterError>> + Send + 'static
{
}

impl<T> FutureResult for T where
    T: std::future::Future<Output = Result<(), DiagnosticWriterError>> + Send + 'static
{
}

async fn join_owner_thread(
    thread: Option<thread::JoinHandle<Result<(), DiagnosticWriterError>>>,
) -> Result<(), DiagnosticWriterError> {
    let thread = thread.ok_or(DiagnosticWriterError::Task)?;
    tokio::task::spawn_blocking(move || thread.join())
        .await
        .map_err(|_| DiagnosticWriterError::Task)?
        .map_err(|_| DiagnosticWriterError::Task)?
}

impl From<DiagnosticBuildError> for DiagnosticWriterError {
    fn from(_: DiagnosticBuildError) -> Self {
        Self::Build
    }
}

impl From<DurableProcessError> for DiagnosticWriterError {
    fn from(_: DurableProcessError) -> Self {
        Self::Durable
    }
}
