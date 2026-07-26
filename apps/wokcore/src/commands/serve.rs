use std::{
    io,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
};

use serde_json::json;
use tokio::{
    net::TcpListener,
    sync::{oneshot, watch},
    task::JoinHandle,
};
use wokcore_core::{
    id::ProviderId,
    secret::{SecretPurpose, SecretScope},
};
use wokcore_platform::{DiscoveryRecord, DiscoveryStore, PlatformError, RuntimeLease};
use wokcore_server::{
    RunningServer, ServerShutdown, ServerState,
    auth::{AuthError, AuthRegistry, StateAuthMetadataStore},
    lifecycle::ServiceLifecycle,
    runtime::{TokenMetadataError, TokenMetadataSource},
};
use wokcore_storage::{ConfigStore, StateStore, StorageError};

use crate::{
    Clock, CommandOutput, ExitCode, IdSource, RunDependencies, RuntimeValueError, cli::JsonOutput,
};

use super::{status::verify_identity, write_json};

pub(super) async fn run(
    options: JsonOutput,
    dependencies: &RunDependencies,
    output: &mut dyn CommandOutput,
) -> ExitCode {
    let json = options.json;
    match run_service(options, dependencies, output).await {
        Ok(()) => ExitCode::Success,
        Err(error) => render_error(error, output, json),
    }
}

async fn run_service(
    options: JsonOutput,
    dependencies: &RunDependencies,
    output: &mut dyn CommandOutput,
) -> Result<(), ServeError> {
    let lease = RuntimeLease::acquire(&dependencies.paths).map_err(ServeError::Platform)?;
    let (cancellation, cancelled) = watch::channel(false);
    let (startup, ready) = oneshot::channel();
    let (announcement, announced) = oneshot::channel();
    let owned_dependencies = dependencies.clone();
    let mut task = OwnedServeTask::new(
        cancellation,
        tokio::spawn(run_service_owned(
            owned_dependencies,
            lease,
            cancelled,
            startup,
            announced,
        )),
    );
    let ready = match ready.await {
        Ok(Ok(ready)) => ready,
        Ok(Err(error)) => {
            let _ = task.wait().await;
            return Err(error);
        }
        Err(_) => {
            task.wait().await?;
            return Err(ServeError::Server);
        }
    };
    let rendered = if options.json {
        write_json(
            output,
            &json!({
                "api_major": 1,
                "code": "started",
                "instance_id": ready.instance_id,
                "pid": ready.pid,
                "port": ready.port,
            }),
        )
    } else {
        output.write_stdout("WokCore local service started.\n")
    };
    let _ = announcement.send(rendered.is_ok());
    task.wait().await?;
    rendered.map_err(|_| ServeError::Io)
}

async fn run_service_owned(
    dependencies: RunDependencies,
    lease: RuntimeLease,
    mut cancelled: watch::Receiver<bool>,
    startup: oneshot::Sender<Result<ReadyService, ServeError>>,
    announced: oneshot::Receiver<bool>,
) -> Result<(), ServeError> {
    let mut owner = ServiceLifetime::new(lease);
    let started = match start_service(&dependencies, &mut owner, &cancelled).await {
        Ok(started) => started,
        Err(error) => {
            let _ = owner.cleanup().await;
            let _ = startup.send(Err(error));
            return Ok(());
        }
    };
    if *cancelled.borrow() {
        let _ = owner.cleanup().await;
        return Ok(());
    }
    if startup.send(Ok(started.ready)).is_err() {
        let _ = owner.cleanup().await;
        return Ok(());
    }
    match announced.await {
        Ok(true) => {}
        Ok(false) => {
            let _ = owner.cleanup().await;
            return Err(ServeError::Io);
        }
        Err(_) => {
            let _ = owner.cleanup().await;
            return Ok(());
        }
    }

    let running = owner
        .running
        .take()
        .expect("started service retains its listener owner");
    let mut wait = Box::pin(running.wait());
    let server_result = tokio::select! {
        result = &mut wait => result,
        changed = cancelled.changed() => {
            if changed.is_ok() && *cancelled.borrow() {
                started.shutdown.request();
            }
            wait.await
        }
        () = dependencies.shutdown.wait() => {
            let _ = started
                .lifecycle
                .begin_drain(dependencies.drain_timeout)
                .await;
            started.lifecycle.wait_for_zero_active().await;
            let _ = started.lifecycle.request_stop();
            started.shutdown.request();
            wait.await
        }
    };
    let cleanup_result = owner.remove_discovery();
    server_result.map_err(|_| ServeError::Server)?;
    cleanup_result?;
    Ok(())
}

async fn start_service(
    dependencies: &RunDependencies,
    owner: &mut ServiceLifetime,
    cancelled: &watch::Receiver<bool>,
) -> Result<StartedService, ServeError> {
    let config = ConfigStore::new(&dependencies.paths.config_file)
        .load()
        .map_err(ServeError::Storage)?;
    let port = config.config.server.port;
    if port == 0 {
        return Err(ServeError::InvalidConfig);
    }
    let discovery = DiscoveryStore::new(&dependencies.paths).map_err(ServeError::Platform)?;

    if let Some(parent) = dependencies.paths.state_db.parent() {
        std::fs::create_dir_all(parent).map_err(|_| ServeError::Io)?;
    }
    let state = StateStore::open(&dependencies.paths.state_db).map_err(ServeError::Storage)?;
    let metadata = Arc::new(StateAuthMetadataStore::new(state));
    let created_at = dependencies
        .clock
        .now()
        .map_err(|_| ServeError::RuntimeValue)?;
    let management_scope = SecretScope {
        provider_id: ProviderId::new("wokcore-runtime").map_err(|_| ServeError::RuntimeValue)?,
        account_id: None,
        purpose: SecretPurpose::Auxiliary,
    };
    let auth = AuthRegistry::bootstrap(
        dependencies.secrets.clone(),
        metadata,
        dependencies.entropy.clone(),
        management_scope,
        created_at,
    )
    .await
    .map_err(map_auth_error)?;
    ensure_not_cancelled(cancelled)?;

    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
        .await
        .map_err(ServeError::Bind)?;
    ensure_not_cancelled(cancelled)?;
    let authority = format!("127.0.0.1:{port}");
    let instance_id = dependencies
        .ids
        .new_instance_id()
        .map_err(|_| ServeError::RuntimeValue)?;
    let pid = dependencies.process.current_pid();
    if pid == 0 {
        return Err(ServeError::RuntimeValue);
    }
    let lifecycle = ServiceLifecycle::new();
    dependencies.lifecycle_observer.observe(&lifecycle);
    let token_metadata = Arc::new(InjectedTokenMetadata {
        clock: dependencies.clock.clone(),
        ids: dependencies.ids.clone(),
    });
    let server_state = ServerState::new_with_token_metadata(
        authority,
        instance_id,
        Arc::new(auth),
        lifecycle.clone(),
        token_metadata,
    );
    let running = RunningServer::start(listener, server_state)
        .await
        .map_err(|_| ServeError::Server)?;
    let shutdown = running.shutdown_handle();
    owner.running = Some(running);
    lifecycle.mark_running().map_err(|_| ServeError::Server)?;

    let record = DiscoveryRecord {
        base_url: format!("http://127.0.0.1:{port}"),
        pid,
        instance_id,
        wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
        api_major: 1,
    };
    if verify_identity(&record).await.is_err() {
        return Err(ServeError::Readiness);
    }
    ensure_not_cancelled(cancelled)?;

    owner.discovery = Some((discovery, instance_id));
    dependencies
        .discovery_publisher
        .publish(
            &owner
                .discovery
                .as_ref()
                .expect("owned discovery is installed before publication")
                .0,
            &record,
        )
        .map_err(ServeError::Platform)?;
    Ok(StartedService {
        ready: ReadyService {
            instance_id,
            pid,
            port,
        },
        lifecycle,
        shutdown,
    })
}

fn ensure_not_cancelled(cancelled: &watch::Receiver<bool>) -> Result<(), ServeError> {
    if *cancelled.borrow() {
        Err(ServeError::Cancelled)
    } else {
        Ok(())
    }
}

struct OwnedServeTask {
    cancellation: watch::Sender<bool>,
    join: Option<JoinHandle<Result<(), ServeError>>>,
}

impl OwnedServeTask {
    fn new(cancellation: watch::Sender<bool>, join: JoinHandle<Result<(), ServeError>>) -> Self {
        Self {
            cancellation,
            join: Some(join),
        }
    }

    async fn wait(&mut self) -> Result<(), ServeError> {
        self.join
            .take()
            .expect("owned serve task is joined at most once")
            .await
            .map_err(|_| ServeError::Server)?
    }
}

impl Drop for OwnedServeTask {
    fn drop(&mut self) {
        self.cancellation.send_replace(true);
    }
}

struct ServiceLifetime {
    _lease: RuntimeLease,
    running: Option<RunningServer>,
    discovery: Option<(DiscoveryStore, uuid::Uuid)>,
}

impl ServiceLifetime {
    fn new(lease: RuntimeLease) -> Self {
        Self {
            _lease: lease,
            running: None,
            discovery: None,
        }
    }

    async fn cleanup(&mut self) -> Result<(), ServeError> {
        let server_result = if let Some(running) = self.running.take() {
            running.shutdown().await.map_err(|_| ServeError::Server)
        } else {
            Ok(())
        };
        let discovery_result = self.remove_discovery();
        server_result?;
        discovery_result
    }

    fn remove_discovery(&mut self) -> Result<(), ServeError> {
        let Some((discovery, instance_id)) = self.discovery.take() else {
            return Ok(());
        };
        discovery
            .remove_if_owned(instance_id)
            .map(|_| ())
            .map_err(ServeError::Platform)
    }
}

#[derive(Clone, Copy)]
struct ReadyService {
    instance_id: uuid::Uuid,
    pid: u32,
    port: u16,
}

struct StartedService {
    ready: ReadyService,
    lifecycle: ServiceLifecycle,
    shutdown: ServerShutdown,
}

fn map_auth_error(error: AuthError) -> ServeError {
    match error {
        AuthError::Storage(error @ StorageError::StateDatabaseCorrupt { .. }) => {
            ServeError::Storage(error)
        }
        _ => ServeError::Auth,
    }
}

struct InjectedTokenMetadata {
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdSource>,
}

impl TokenMetadataSource for InjectedTokenMetadata {
    fn new_token_id(&self) -> Result<String, TokenMetadataError> {
        self.ids
            .new_token_id()
            .map_err(|RuntimeValueError| TokenMetadataError)
    }

    fn now(&self) -> Result<String, TokenMetadataError> {
        self.clock
            .now()
            .map_err(|RuntimeValueError| TokenMetadataError)
    }
}

fn render_error(error: ServeError, output: &mut dyn CommandOutput, json: bool) -> ExitCode {
    let (exit, code, human) = match error {
        ServeError::Platform(PlatformError::AlreadyRunning) => (
            ExitCode::AlreadyRunning,
            "already_running",
            "WokCore is already running.\n",
        ),
        ServeError::Bind(ref source) if source.kind() == io::ErrorKind::AddrInUse => (
            ExitCode::PortOccupied,
            "port_occupied",
            "The configured WokCore port is occupied.\n",
        ),
        ServeError::Storage(StorageError::InvalidConfig { .. }) | ServeError::InvalidConfig => (
            ExitCode::InvalidInput,
            "invalid_configuration",
            "WokCore configuration is invalid.\n",
        ),
        ServeError::Storage(StorageError::StateDatabaseCorrupt { .. }) => (
            ExitCode::StorageCorruption,
            "storage_corrupt",
            "WokCore storage is corrupt.\n",
        ),
        ServeError::Auth => (
            ExitCode::AuthenticationFailure,
            "authentication_failure",
            "WokCore authentication initialization failed.\n",
        ),
        _ => (
            ExitCode::InternalFailure,
            "internal_error",
            "WokCore service failed.\n",
        ),
    };
    let rendered = if json {
        write_json(output, &json!({"code": code}))
    } else {
        output.write_stderr(human)
    };
    if rendered.is_ok() {
        exit
    } else {
        ExitCode::InternalFailure
    }
}

#[derive(Debug)]
enum ServeError {
    Platform(PlatformError),
    Storage(StorageError),
    Auth,
    Bind(io::Error),
    Io,
    InvalidConfig,
    RuntimeValue,
    Cancelled,
    Readiness,
    Server,
}
