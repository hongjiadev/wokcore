use std::{
    net::{IpAddr, Ipv4Addr},
    sync::Arc,
};

use tokio::{net::TcpListener, sync::watch, task::JoinHandle};
use uuid::Uuid;

use crate::{
    api::build_router,
    auth::{AuthRegistry, EntropySource, OsEntropy},
    lifecycle::ServiceLifecycle,
    observability::{DiagnosticWriterHandle, SchedulerHandle, StateWriterHandle},
    query::QueryRuntime,
    runtime::{SystemTokenMetadata, TokenMetadataSource},
};

#[derive(Clone)]
pub struct ServerState {
    pub(crate) authority: Arc<str>,
    pub(crate) instance_id: Uuid,
    pub(crate) auth: Arc<AuthRegistry>,
    pub(crate) lifecycle: ServiceLifecycle,
    pub(crate) token_metadata: Arc<dyn TokenMetadataSource>,
    pub(crate) request_id_entropy: Arc<dyn EntropySource>,
    pub(crate) scheduler: Option<SchedulerHandle>,
    pub(crate) diagnostics: Option<DiagnosticWriterHandle>,
    pub(crate) state_writer: Option<StateWriterHandle>,
    pub(crate) query: Option<QueryRuntime>,
    shutdown: watch::Sender<bool>,
    coordinated_shutdown: bool,
}

impl ServerState {
    pub fn new(
        authority: impl Into<Arc<str>>,
        instance_id: Uuid,
        auth: Arc<AuthRegistry>,
        lifecycle: ServiceLifecycle,
    ) -> Self {
        Self::new_with_runtime_sources(
            authority,
            instance_id,
            auth,
            lifecycle,
            Arc::new(SystemTokenMetadata::default()),
            Arc::new(OsEntropy),
        )
    }

    pub fn new_with_token_metadata(
        authority: impl Into<Arc<str>>,
        instance_id: Uuid,
        auth: Arc<AuthRegistry>,
        lifecycle: ServiceLifecycle,
        token_metadata: Arc<dyn TokenMetadataSource>,
    ) -> Self {
        Self::new_with_runtime_sources(
            authority,
            instance_id,
            auth,
            lifecycle,
            token_metadata,
            Arc::new(OsEntropy),
        )
    }

    pub fn new_with_runtime_sources(
        authority: impl Into<Arc<str>>,
        instance_id: Uuid,
        auth: Arc<AuthRegistry>,
        lifecycle: ServiceLifecycle,
        token_metadata: Arc<dyn TokenMetadataSource>,
        request_id_entropy: Arc<dyn EntropySource>,
    ) -> Self {
        let (shutdown, _) = watch::channel(false);
        Self {
            authority: authority.into(),
            instance_id,
            auth,
            lifecycle,
            token_metadata,
            request_id_entropy,
            scheduler: None,
            diagnostics: None,
            state_writer: None,
            query: None,
            shutdown,
            coordinated_shutdown: false,
        }
    }

    pub fn with_scheduler(mut self, scheduler: SchedulerHandle) -> Self {
        self.scheduler = Some(scheduler);
        self
    }

    pub fn with_diagnostics(mut self, diagnostics: DiagnosticWriterHandle) -> Self {
        self.diagnostics = Some(diagnostics);
        self
    }

    pub fn with_state_writer(mut self, state_writer: StateWriterHandle) -> Self {
        self.state_writer = Some(state_writer);
        self
    }

    pub fn with_query_runtime(mut self, query: QueryRuntime) -> Self {
        self.query = Some(query);
        self
    }

    pub fn with_coordinated_shutdown(mut self) -> (Self, watch::Receiver<bool>) {
        self.coordinated_shutdown = true;
        let requests = self.shutdown.subscribe();
        (self, requests)
    }

    pub(crate) fn request_shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    fn shutdown_receiver(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }
}

pub struct RunningServer {
    local_addr: std::net::SocketAddr,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<Result<(), std::io::Error>>>,
}

#[derive(Clone)]
pub struct ServerShutdown {
    shutdown: watch::Sender<bool>,
}

impl ServerShutdown {
    pub fn request(&self) {
        self.shutdown.send_replace(true);
    }
}

impl RunningServer {
    pub async fn start(listener: TcpListener, state: ServerState) -> Result<Self, ServerError> {
        let local_addr = listener.local_addr().map_err(ServerError::Listener)?;
        if local_addr.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
            return Err(ServerError::NotIpv4Loopback);
        }
        if state.authority.as_ref() != local_addr.to_string() {
            return Err(ServerError::AuthorityMismatch);
        }
        let auto_shutdown_on_request = !state.coordinated_shutdown;
        let mut requested_shutdown = state.shutdown_receiver();
        let (owner_shutdown, mut listener_shutdown) = watch::channel(false);
        let app = build_router(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    loop {
                        if *listener_shutdown.borrow_and_update()
                            || (auto_shutdown_on_request && *requested_shutdown.borrow_and_update())
                        {
                            return;
                        }
                        tokio::select! {
                            changed = listener_shutdown.changed() => {
                                if changed.is_err() {
                                    return;
                                }
                            }
                            changed = requested_shutdown.changed(), if auto_shutdown_on_request => {
                                if changed.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                })
                .await
        });
        Ok(Self {
            local_addr,
            shutdown: owner_shutdown,
            task: Some(task),
        })
    }

    pub const fn local_addr(&self) -> std::net::SocketAddr {
        self.local_addr
    }

    pub fn shutdown_handle(&self) -> ServerShutdown {
        ServerShutdown {
            shutdown: self.shutdown.clone(),
        }
    }

    pub async fn shutdown(mut self) -> Result<(), ServerError> {
        self.shutdown.send_replace(true);
        self.join().await
    }

    pub async fn wait(mut self) -> Result<(), ServerError> {
        self.join().await
    }

    pub async fn wait_mut(&mut self) -> Result<(), ServerError> {
        self.join().await
    }

    async fn join(&mut self) -> Result<(), ServerError> {
        let Some(task) = self.task.as_mut() else {
            return Err(ServerError::ServerTask);
        };
        let result = task.await;
        self.task.take();
        result
            .map_err(|_| ServerError::ServerTask)?
            .map_err(ServerError::Listener)
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
        let Some(task) = self.task.take() else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            std::mem::drop(runtime.spawn(async move {
                let _ = task.await;
            }));
        } else {
            task.abort();
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("server listener operation failed")]
    Listener(#[source] std::io::Error),
    #[error("server listener must be bound to IPv4 loopback")]
    NotIpv4Loopback,
    #[error("server listener does not match configured authority")]
    AuthorityMismatch,
    #[error("server task failed")]
    ServerTask,
}

#[cfg(test)]
mod tests {
    use std::{future, net::SocketAddr};

    use tokio::sync::watch;
    use tokio::time::{Duration, timeout};

    use super::{RunningServer, ServerError};

    #[tokio::test]
    async fn join_error_consumes_the_completed_task_handle() {
        let (shutdown, _) = watch::channel(false);
        let task = tokio::spawn(async {
            future::pending::<()>().await;
            Ok::<(), std::io::Error>(())
        });
        task.abort();
        let mut server = RunningServer {
            local_addr: SocketAddr::from(([127, 0, 0, 1], 43127)),
            shutdown,
            task: Some(task),
        };

        let result = server.join().await;
        let task_remained = server.task.is_some();
        server.task.take();

        assert!(matches!(result, Err(ServerError::ServerTask)));
        assert!(!task_remained, "JoinError left a completed task handle");
    }

    #[tokio::test]
    async fn cancelled_join_keeps_the_server_task_owned_for_cleanup() {
        let (shutdown, _) = watch::channel(false);
        let task = tokio::spawn(async {
            future::pending::<()>().await;
            Ok::<(), std::io::Error>(())
        });
        let mut server = RunningServer {
            local_addr: SocketAddr::from(([127, 0, 0, 1], 43127)),
            shutdown,
            task: Some(task),
        };

        let result = timeout(Duration::from_millis(10), server.wait_mut()).await;

        assert!(result.is_err(), "pending server unexpectedly completed");
        assert!(
            server.task.is_some(),
            "cancelling listener join detached the server task from its owner"
        );
        let task = server.task.take().unwrap();
        task.abort();
        let _ = task.await;
    }
}
