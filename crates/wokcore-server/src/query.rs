use std::{
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use tokio::sync::{mpsc, oneshot};
use wokcore_diagnostics::export::ExportCoordinator;

use crate::observability::SessionRootPaths;

pub const DEFAULT_QUERY_WORKERS: usize = 2;
pub const MAX_QUERY_WORKERS: usize = 4;
pub const QUERY_QUEUE_CAPACITY: usize = 32;
pub const QUERY_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct QueryRuntime {
    handle: QueryServiceHandle,
    state_path: Arc<PathBuf>,
    session_roots: Option<SessionRootPaths>,
    session_domain_key: [u8; 32],
    diagnostics_root: Arc<PathBuf>,
    export: ExportCoordinator,
}

impl QueryRuntime {
    pub fn new(
        handle: QueryServiceHandle,
        state_path: impl AsRef<Path>,
        session_roots: Option<SessionRootPaths>,
        session_domain_key: [u8; 32],
        diagnostics_root: impl AsRef<Path>,
    ) -> Self {
        Self {
            handle,
            state_path: Arc::new(state_path.as_ref().to_path_buf()),
            session_roots,
            session_domain_key,
            diagnostics_root: Arc::new(diagnostics_root.as_ref().to_path_buf()),
            export: ExportCoordinator::new(),
        }
    }

    pub(crate) fn handle(&self) -> &QueryServiceHandle {
        &self.handle
    }

    pub(crate) fn state_path(&self) -> &Path {
        &self.state_path
    }

    pub(crate) fn session_roots(&self) -> Option<&SessionRootPaths> {
        self.session_roots.as_ref()
    }

    pub(crate) const fn session_domain_key(&self) -> &[u8; 32] {
        &self.session_domain_key
    }

    pub(crate) fn diagnostics_root(&self) -> &Path {
        &self.diagnostics_root
    }

    pub(crate) fn export_coordinator(&self) -> &ExportCoordinator {
        &self.export
    }
}

impl fmt::Debug for QueryRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("QueryRuntime([redacted])")
    }
}

type QueryOperation =
    Box<dyn FnOnce(&QueryCancellation) -> Result<Vec<u8>, QueryServiceError> + Send + 'static>;

enum QueryCommand {
    Execute {
        operation: QueryOperation,
        cancellation: Arc<AtomicBool>,
        response: oneshot::Sender<Result<Vec<u8>, QueryServiceError>>,
    },
    Shutdown,
}

#[derive(Clone)]
pub struct QueryServiceHandle {
    sender: mpsc::Sender<QueryCommand>,
    shutting_down: Arc<AtomicBool>,
}

impl QueryServiceHandle {
    pub fn try_submit<F>(&self, operation: F) -> Result<PendingQuery, QueryServiceError>
    where
        F: FnOnce(&QueryCancellation) -> Result<Vec<u8>, QueryServiceError> + Send + 'static,
    {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(QueryServiceError::Closed);
        }
        let cancellation = Arc::new(AtomicBool::new(false));
        let (response, result) = oneshot::channel();
        self.sender
            .try_send(QueryCommand::Execute {
                operation: Box::new(operation),
                cancellation: Arc::clone(&cancellation),
                response,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => QueryServiceError::Busy,
                mpsc::error::TrySendError::Closed(_) => QueryServiceError::Closed,
            })?;
        Ok(PendingQuery {
            cancellation,
            result: Some(result),
        })
    }
}

impl fmt::Debug for QueryServiceHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("QueryServiceHandle([redacted])")
    }
}

pub struct PendingQuery {
    cancellation: Arc<AtomicBool>,
    result: Option<oneshot::Receiver<Result<Vec<u8>, QueryServiceError>>>,
}

impl PendingQuery {
    pub async fn wait(mut self) -> Result<Vec<u8>, QueryServiceError> {
        let result = self.result.take().ok_or(QueryServiceError::Closed)?;
        match tokio::time::timeout(QUERY_DEADLINE, result).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(QueryServiceError::Worker),
            Err(_) => {
                self.cancellation.store(true, Ordering::Release);
                Err(QueryServiceError::Timeout)
            }
        }
    }
}

impl fmt::Debug for PendingQuery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PendingQuery([redacted])")
    }
}

impl Drop for PendingQuery {
    fn drop(&mut self) {
        if self.result.is_some() {
            self.cancellation.store(true, Ordering::Release);
        }
    }
}

pub struct QueryCancellation {
    request: Arc<AtomicBool>,
    service: Arc<AtomicBool>,
}

impl QueryCancellation {
    pub fn is_cancelled(&self) -> bool {
        self.request.load(Ordering::Acquire) || self.service.load(Ordering::Acquire)
    }

    pub fn check(&self) -> Result<(), QueryServiceError> {
        if self.is_cancelled() {
            Err(QueryServiceError::Cancelled)
        } else {
            Ok(())
        }
    }
}

pub struct QueryService {
    handle: QueryServiceHandle,
    worker_count: usize,
    workers: Vec<thread::JoinHandle<()>>,
}

impl QueryService {
    pub fn start(worker_count: usize) -> Result<Self, QueryServiceError> {
        if !(1..=MAX_QUERY_WORKERS).contains(&worker_count) {
            return Err(QueryServiceError::InvalidConfig);
        }
        let (sender, receiver) = mpsc::channel(QUERY_QUEUE_CAPACITY);
        let receiver = Arc::new(Mutex::new(receiver));
        let shutting_down = Arc::new(AtomicBool::new(false));
        let handle = QueryServiceHandle {
            sender,
            shutting_down: Arc::clone(&shutting_down),
        };
        let mut workers = Vec::with_capacity(worker_count);
        for index in 0..worker_count {
            let receiver = Arc::clone(&receiver);
            let service_cancelled = Arc::clone(&shutting_down);
            match thread::Builder::new()
                .name(format!("wokcore-query-{index}"))
                .spawn(move || query_worker(receiver, service_cancelled))
            {
                Ok(worker) => workers.push(worker),
                Err(_) => {
                    shutting_down.store(true, Ordering::Release);
                    for _ in 0..workers.len() {
                        let _ = handle.sender.blocking_send(QueryCommand::Shutdown);
                    }
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(QueryServiceError::Worker);
                }
            }
        }
        Ok(Self {
            handle,
            worker_count,
            workers,
        })
    }

    pub fn handle(&self) -> QueryServiceHandle {
        self.handle.clone()
    }

    pub async fn shutdown(self) -> Result<(), QueryServiceError> {
        self.handle.shutting_down.store(true, Ordering::Release);
        for _ in 0..self.worker_count {
            self.handle
                .sender
                .send(QueryCommand::Shutdown)
                .await
                .map_err(|_| QueryServiceError::Closed)?;
        }
        tokio::task::spawn_blocking(move || {
            for worker in self.workers {
                worker.join().map_err(|_| QueryServiceError::Worker)?;
            }
            Ok(())
        })
        .await
        .map_err(|_| QueryServiceError::Worker)?
    }
}

impl fmt::Debug for QueryService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QueryService")
            .field("worker_count", &self.worker_count)
            .finish_non_exhaustive()
    }
}

fn query_worker(
    receiver: Arc<Mutex<mpsc::Receiver<QueryCommand>>>,
    service_cancelled: Arc<AtomicBool>,
) {
    loop {
        let command = receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .blocking_recv();
        match command {
            Some(QueryCommand::Execute {
                operation,
                cancellation,
                response,
            }) => {
                let context = QueryCancellation {
                    request: cancellation,
                    service: Arc::clone(&service_cancelled),
                };
                let result = if context.is_cancelled() {
                    Err(QueryServiceError::Cancelled)
                } else {
                    catch_unwind(AssertUnwindSafe(|| operation(&context)))
                        .unwrap_or(Err(QueryServiceError::Worker))
                };
                let _ = response.send(result);
            }
            Some(QueryCommand::Shutdown) | None => return,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum QueryServiceError {
    #[error("query service configuration is invalid")]
    InvalidConfig,
    #[error("query queue is full")]
    Busy,
    #[error("query deadline elapsed")]
    Timeout,
    #[error("query was cancelled")]
    Cancelled,
    #[error("query service is closed")]
    Closed,
    #[error("query worker failed")]
    Worker,
    #[error("query execution failed")]
    Execution,
    #[error("query cursor is invalid")]
    InvalidCursor,
    #[error("query response exceeds its hard limit")]
    ResponseLimit,
    #[error("Session was not found")]
    SessionNotFound,
    #[error("Session source is unavailable")]
    SessionUnavailable,
    #[error("Session cursor is stale")]
    SessionCursorStale,
}
