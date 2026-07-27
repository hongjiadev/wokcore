use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};

use crate::StorageError;

use super::{
    MAX_SESSION_BATCH_ROWS, ProviderMetadataBatch, ProviderMetadataBatchOutcome,
    RequestSupplementalMetadata, SessionBatch, StateStore, SupplementalBatchOutcome,
    WAL_CHECKPOINT_THRESHOLD_BYTES,
};

pub const STATE_STORE_WRITER_QUEUE_CAPACITY: usize = 4;
pub const SUPPLEMENTAL_CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

type StateStoreOperation = Box<dyn FnOnce(&mut StateStore) + Send + 'static>;

enum StateStoreCommand {
    Execute(StateStoreOperation),
    Flush(oneshot::Sender<Result<(), StorageError>>),
    Checkpoint {
        expected_activity_revision: u64,
        truncate_if_idle_for: Option<Duration>,
        reply: oneshot::Sender<Result<(), StorageError>>,
    },
    Shutdown(oneshot::Sender<()>),
}

#[derive(Clone)]
pub struct StateStoreWriterClient {
    sender: mpsc::Sender<StateStoreCommand>,
    activity_revision: Arc<AtomicU64>,
    last_activity_at: Arc<Mutex<Instant>>,
    last_supplemental_cleanup_at: Arc<Mutex<Option<Instant>>>,
}

pub struct StateStoreWriter {
    store: StateStore,
    receiver: mpsc::Receiver<StateStoreCommand>,
    shutdown_acknowledgement: Option<oneshot::Sender<()>>,
    activity_revision: Arc<AtomicU64>,
    last_activity_at: Arc<Mutex<Instant>>,
}

pub struct StateStoreWriteReceipt<T> {
    receiver: oneshot::Receiver<Result<T, StorageError>>,
}

pub struct StateStoreWriterShutdownHandle {
    sender: mpsc::Sender<StateStoreCommand>,
}

pub struct StateStoreWriterShutdownReceipt {
    receiver: oneshot::Receiver<()>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum StateStoreWriterSubmitError {
    #[error("the StateStore writer queue is full")]
    QueueFull,
    #[error("the StateStore writer is closed")]
    WriterClosed,
}

#[derive(Debug, thiserror::Error)]
pub enum StateStoreWriteError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("the StateStore writer stopped before returning a result")]
    WriterStopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("the StateStore writer stopped before acknowledging shutdown")]
pub struct StateStoreWriterShutdownError;

pub fn state_store_writer(
    store: StateStore,
) -> (
    StateStoreWriterClient,
    StateStoreWriterShutdownHandle,
    StateStoreWriter,
) {
    let (sender, receiver) = mpsc::channel(STATE_STORE_WRITER_QUEUE_CAPACITY);
    let activity_revision = Arc::new(AtomicU64::new(0));
    let last_activity_at = Arc::new(Mutex::new(Instant::now()));
    let last_supplemental_cleanup_at = Arc::new(Mutex::new(None));
    (
        StateStoreWriterClient {
            sender: sender.clone(),
            activity_revision: Arc::clone(&activity_revision),
            last_activity_at: Arc::clone(&last_activity_at),
            last_supplemental_cleanup_at,
        },
        StateStoreWriterShutdownHandle { sender },
        StateStoreWriter {
            store,
            receiver,
            shutdown_acknowledgement: None,
            activity_revision: Arc::clone(&activity_revision),
            last_activity_at: Arc::clone(&last_activity_at),
        },
    )
}

impl StateStoreWriterClient {
    pub fn activity_revision(&self) -> u64 {
        self.activity_revision.load(Ordering::Acquire)
    }

    pub fn has_been_idle_for(&self, duration: Duration) -> bool {
        self.last_activity_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed()
            >= duration
    }

    pub fn try_flush(&self) -> Result<StateStoreWriteReceipt<()>, StateStoreWriterSubmitError> {
        let (reply, receiver) = oneshot::channel();
        self.sender
            .try_send(StateStoreCommand::Flush(reply))
            .map_err(map_try_send_error)?;
        Ok(StateStoreWriteReceipt { receiver })
    }

    pub fn try_checkpoint(
        &self,
        truncate_if_idle_for: Option<Duration>,
    ) -> Result<StateStoreWriteReceipt<()>, StateStoreWriterSubmitError> {
        let (reply, receiver) = oneshot::channel();
        let expected_activity_revision = self.activity_revision();
        self.sender
            .try_send(StateStoreCommand::Checkpoint {
                expected_activity_revision,
                truncate_if_idle_for,
                reply,
            })
            .map_err(map_try_send_error)?;
        Ok(StateStoreWriteReceipt { receiver })
    }

    pub fn try_commit_session_batch(
        &self,
        batch: SessionBatch,
    ) -> Result<StateStoreWriteReceipt<SupplementalBatchOutcome>, StateStoreWriterSubmitError> {
        self.try_execute(move |store| store.commit_session_batch(&batch))
    }

    pub fn try_commit_candidate_batch(
        &self,
        batch: SessionBatch,
    ) -> Result<StateStoreWriteReceipt<SupplementalBatchOutcome>, StateStoreWriterSubmitError> {
        self.try_execute(move |store| store.commit_candidate_batch(&batch))
    }

    pub fn try_record_provider_metadata_batch(
        &self,
        batch: ProviderMetadataBatch,
    ) -> Result<StateStoreWriteReceipt<ProviderMetadataBatchOutcome>, StateStoreWriterSubmitError>
    {
        self.try_execute(move |store| store.record_provider_metadata_batch(&batch))
    }

    pub fn try_record_request_supplemental_batch(
        &self,
        metadata: Vec<RequestSupplementalMetadata>,
    ) -> Result<StateStoreWriteReceipt<SupplementalBatchOutcome>, StateStoreWriterSubmitError> {
        let cleanup_at = metadata
            .iter()
            .map(|row| row.occurred_at.as_str())
            .max()
            .map(str::to_owned);
        let mut last_cleanup = self
            .last_supplemental_cleanup_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cleanup_due = cleanup_at.is_some()
            && last_cleanup
                .as_ref()
                .is_none_or(|last| last.elapsed() >= SUPPLEMENTAL_CLEANUP_INTERVAL);
        let receipt = self.try_execute(move |store| {
            let outcome = store.record_request_supplemental_batch(&metadata)?;
            if cleanup_due {
                let cleanup_at = cleanup_at
                    .as_deref()
                    .expect("a due cleanup has a batch timestamp");
                let _ = store.cleanup_request_supplemental(cleanup_at, MAX_SESSION_BATCH_ROWS)?;
            }
            Ok(outcome)
        })?;
        if cleanup_due {
            *last_cleanup = Some(Instant::now());
        }
        Ok(receipt)
    }

    pub fn try_execute<T, F>(
        &self,
        operation: F,
    ) -> Result<StateStoreWriteReceipt<T>, StateStoreWriterSubmitError>
    where
        T: Send + 'static,
        F: FnOnce(&mut StateStore) -> Result<T, StorageError> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let completion_revision = Arc::clone(&self.activity_revision);
        let completion_activity = Arc::clone(&self.last_activity_at);
        let command = Box::new(move |store: &mut StateStore| {
            let result = operation(store).and_then(|value| {
                let _ = store.checkpoint_passive_if_at_least(WAL_CHECKPOINT_THRESHOLD_BYTES)?;
                Ok(value)
            });
            let _ = sender.send(result);
            observe_activity(&completion_revision, &completion_activity);
        });
        self.sender
            .try_send(StateStoreCommand::Execute(command))
            .map_err(map_try_send_error)?;
        observe_activity(&self.activity_revision, &self.last_activity_at);
        Ok(StateStoreWriteReceipt { receiver })
    }
}

fn map_try_send_error<T>(error: mpsc::error::TrySendError<T>) -> StateStoreWriterSubmitError {
    match error {
        mpsc::error::TrySendError::Full(_) => StateStoreWriterSubmitError::QueueFull,
        mpsc::error::TrySendError::Closed(_) => StateStoreWriterSubmitError::WriterClosed,
    }
}

fn observe_activity(revision: &AtomicU64, last_activity_at: &Mutex<Instant>) {
    *last_activity_at
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
    let _ = revision.fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
        value.checked_add(1)
    });
}

impl StateStoreWriter {
    pub async fn run_one(&mut self) -> bool {
        let command = match self.receiver.recv().await {
            Some(command) => command,
            None => {
                if let Some(sender) = self.shutdown_acknowledgement.take() {
                    let _ = sender.send(());
                }
                return false;
            }
        };
        match command {
            StateStoreCommand::Execute(operation) => operation(&mut self.store),
            StateStoreCommand::Flush(reply) => {
                let _ = reply.send(Ok(()));
            }
            StateStoreCommand::Checkpoint {
                expected_activity_revision,
                truncate_if_idle_for,
                reply,
            } => {
                let result = self
                    .store
                    .checkpoint_passive_if_at_least(WAL_CHECKPOINT_THRESHOLD_BYTES)
                    .map(|_| ())
                    .and_then(|()| {
                        let revision_unchanged =
                            expected_activity_revision == self.activity_revision();
                        let idle = truncate_if_idle_for
                            .is_some_and(|duration| self.has_been_idle_for(duration));
                        if revision_unchanged && idle {
                            let _ = self.store.checkpoint_truncate()?;
                        }
                        Ok(())
                    });
                let _ = reply.send(result);
            }
            StateStoreCommand::Shutdown(sender) => {
                self.receiver.close();
                self.shutdown_acknowledgement = Some(sender);
            }
        }
        true
    }

    pub async fn run(mut self) {
        while self.run_one().await {}
    }

    pub fn close(&mut self) {
        self.receiver.close();
    }

    fn activity_revision(&self) -> u64 {
        self.activity_revision.load(Ordering::Acquire)
    }

    fn has_been_idle_for(&self, duration: Duration) -> bool {
        self.last_activity_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed()
            >= duration
    }
}

impl StateStoreWriterShutdownHandle {
    pub async fn shutdown(
        self,
    ) -> Result<StateStoreWriterShutdownReceipt, StateStoreWriterSubmitError> {
        let (sender, receiver) = oneshot::channel();
        self.sender
            .send(StateStoreCommand::Shutdown(sender))
            .await
            .map_err(|_| StateStoreWriterSubmitError::WriterClosed)?;
        Ok(StateStoreWriterShutdownReceipt { receiver })
    }
}

impl StateStoreWriterShutdownReceipt {
    pub async fn wait(self) -> Result<(), StateStoreWriterShutdownError> {
        self.receiver
            .await
            .map_err(|_| StateStoreWriterShutdownError)
    }
}

impl<T> StateStoreWriteReceipt<T> {
    pub async fn wait(self) -> Result<T, StateStoreWriteError> {
        self.receiver
            .await
            .map_err(|_| StateStoreWriteError::WriterStopped)?
            .map_err(StateStoreWriteError::Storage)
    }

    pub fn blocking_wait(self) -> Result<T, StateStoreWriteError> {
        self.receiver
            .blocking_recv()
            .map_err(|_| StateStoreWriteError::WriterStopped)?
            .map_err(StateStoreWriteError::Storage)
    }
}
