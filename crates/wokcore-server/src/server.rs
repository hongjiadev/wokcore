use std::{
    net::{IpAddr, Ipv4Addr},
    sync::Arc,
};

use tokio::{net::TcpListener, sync::watch, task::JoinHandle};
use uuid::Uuid;

use crate::{api::build_router, auth::AuthRegistry, lifecycle::ServiceLifecycle};

#[derive(Clone)]
pub struct ServerState {
    pub(crate) authority: Arc<str>,
    pub(crate) instance_id: Uuid,
    pub(crate) auth: Arc<AuthRegistry>,
    pub(crate) lifecycle: ServiceLifecycle,
    shutdown: watch::Sender<bool>,
}

impl ServerState {
    pub fn new(
        authority: impl Into<Arc<str>>,
        instance_id: Uuid,
        auth: Arc<AuthRegistry>,
        lifecycle: ServiceLifecycle,
    ) -> Self {
        let (shutdown, _) = watch::channel(false);
        Self {
            authority: authority.into(),
            instance_id,
            auth,
            lifecycle,
            shutdown,
        }
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

impl RunningServer {
    pub async fn start(listener: TcpListener, state: ServerState) -> Result<Self, ServerError> {
        let local_addr = listener.local_addr().map_err(ServerError::Listener)?;
        if local_addr.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
            return Err(ServerError::NotIpv4Loopback);
        }
        if state.authority.as_ref() != local_addr.to_string() {
            return Err(ServerError::AuthorityMismatch);
        }
        let mut shutdown = state.shutdown_receiver();
        let owner_shutdown = state.shutdown.clone();
        let app = build_router(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    loop {
                        if *shutdown.borrow_and_update() {
                            return;
                        }
                        if shutdown.changed().await.is_err() {
                            return;
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

    pub async fn shutdown(mut self) -> Result<(), ServerError> {
        self.shutdown.send_replace(true);
        self.join().await
    }

    pub async fn wait(mut self) -> Result<(), ServerError> {
        self.join().await
    }

    async fn join(&mut self) -> Result<(), ServerError> {
        let result = self
            .task
            .as_mut()
            .expect("running server task is joined at most once")
            .await
            .map_err(|_| ServerError::ServerTask)?
            .map_err(ServerError::Listener);
        self.task.take();
        result
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
