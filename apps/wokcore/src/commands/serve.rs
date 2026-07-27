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
    auth::{AuthError, AuthMetadataStore, AuthRegistry, StateAuthMetadataStore},
    lifecycle::ServiceLifecycle,
    observability::{
        PreparedDiagnosticWriter, PreparedScheduler, PreparedStateWriter,
        ProductionSessionScanBackend, RunningDiagnosticWriter, RunningScheduler,
        RunningStateWriter, ScanTimestampSource, SchedulerConfig,
    },
    providers::{ProviderManagement, ProviderManagementError},
    query::{DEFAULT_QUERY_WORKERS, QueryRuntime, QueryService},
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
    let mut started = match start_service(&dependencies, &mut owner, &cancelled).await {
        Ok(started) => started,
        Err(error) => {
            let _ = owner.cleanup().await;
            let _ = startup.send(Err(error));
            return Ok(());
        }
    };
    if *cancelled.borrow() {
        let observability_result = started.shutdown_observability().await;
        let cleanup_result = owner.cleanup().await;
        observability_result?;
        cleanup_result?;
        return Ok(());
    }
    if startup.send(Ok(started.ready)).is_err() {
        let observability_result = started.shutdown_observability().await;
        let cleanup_result = owner.cleanup().await;
        observability_result?;
        cleanup_result?;
        return Ok(());
    }
    match announced.await {
        Ok(true) => {}
        Ok(false) => {
            let observability_result = started.shutdown_observability().await;
            let cleanup_result = owner.cleanup().await;
            observability_result?;
            cleanup_result?;
            return Err(ServeError::Io);
        }
        Err(_) => {
            let observability_result = started.shutdown_observability().await;
            let cleanup_result = owner.cleanup().await;
            observability_result?;
            cleanup_result?;
            return Ok(());
        }
    }
    if let Err(error) = started.start_observability() {
        let _ = started.shutdown_observability().await;
        let _ = owner.cleanup().await;
        return Err(error);
    }

    let mut stop_requests = started.stop_requests.clone();
    let mut graceful_result = Ok(());
    let server_result = {
        let Some(running) = owner.running.as_mut() else {
            let _ = owner.cleanup().await;
            return Err(ServeError::Server);
        };
        let mut wait = Box::pin(running.wait_mut());
        tokio::select! {
            result = &mut wait => result,
            changed = cancelled.changed() => {
                if changed.is_ok() && *cancelled.borrow() {
                    graceful_result = started
                        .prepare_graceful_shutdown(dependencies.drain_timeout)
                        .await;
                } else {
                    started.shutdown.request();
                }
                wait.await
            }
            changed = stop_requests.changed() => {
                if changed.is_ok() && *stop_requests.borrow() {
                    graceful_result = started
                        .prepare_graceful_shutdown(dependencies.drain_timeout)
                        .await;
                } else {
                    started.shutdown.request();
                }
                wait.await
            }
            () = dependencies.shutdown.wait() => {
                graceful_result = started
                    .prepare_graceful_shutdown(dependencies.drain_timeout)
                    .await;
                wait.await
            }
        }
    };
    let observability_result = started.shutdown_observability().await;
    owner.running.take();
    let cleanup_result = owner.cleanup().await;
    server_result.map_err(|_| ServeError::Server)?;
    graceful_result?;
    observability_result?;
    cleanup_result?;
    Ok(())
}

async fn start_service(
    dependencies: &RunDependencies,
    owner: &mut ServiceLifetime,
    cancelled: &watch::Receiver<bool>,
) -> Result<StartedService, ServeError> {
    let config_store = ConfigStore::new(&dependencies.paths.config_file);
    let config = config_store.load().map_err(ServeError::Storage)?;
    let port = config.config.server.port;
    let provider_management = Arc::new(
        ProviderManagement::from_loaded(config_store, config, dependencies.secrets.clone())
            .map_err(map_provider_management_error)?,
    );
    let account_health = provider_management.account_health();
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
        metadata.clone(),
        dependencies.entropy.clone(),
        management_scope,
        created_at,
    )
    .await
    .map_err(map_auth_error)?;
    let management_binding = metadata
        .runtime_secret_binding("management")
        .map_err(ServeError::Storage)?
        .ok_or(ServeError::Auth)?;
    provider_management
        .protect_secret_ref(management_binding.secret_ref)
        .await
        .map_err(map_provider_management_error)?;
    let session_domain_key = auth.session_domain_key();
    ensure_not_cancelled(cancelled)?;
    let (diagnostics, prepared_diagnostics) = PreparedDiagnosticWriter::open(
        &dependencies.paths.log_dir,
        dependencies.entropy.clone(),
        Arc::new(InjectedScanTimestamp {
            clock: dependencies.clock.clone(),
        }),
    )
    .map_err(|_| ServeError::Server)?;
    let (state_writer, prepared_state_writer) =
        PreparedStateWriter::open(&dependencies.paths.state_db).map_err(|_| ServeError::Server)?;
    let scheduler = if let Some(roots) = dependencies.session_roots.clone() {
        let notification_roots = roots.clone();
        let backend = ProductionSessionScanBackend::open_with_writer(
            roots,
            &dependencies.paths.state_db,
            auth.session_domain_key(),
            Arc::new(InjectedScanTimestamp {
                clock: dependencies.clock.clone(),
            }),
            state_writer.client().clone(),
        )
        .map_err(ServeError::Storage)?;
        let (handle, prepared) =
            PreparedScheduler::new(Arc::new(backend), SchedulerConfig::default())
                .map_err(|_| ServeError::Server)?;
        Some((
            handle,
            prepared.with_filesystem_notifications(notification_roots),
        ))
    } else {
        None
    };

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
    let mut server_state = ServerState::new_with_runtime_sources(
        authority,
        instance_id,
        Arc::new(auth),
        lifecycle.clone(),
        token_metadata,
        dependencies.entropy.clone(),
    );
    let mut prepared_scheduler = None;
    if let Some((handle, prepared)) = scheduler {
        server_state = server_state.with_scheduler(handle);
        prepared_scheduler = Some(prepared);
    }
    server_state = server_state.with_diagnostics(diagnostics);
    server_state = server_state.with_state_writer(state_writer);
    server_state = server_state.with_provider_management(provider_management);
    if let Some(upstream_executor) = dependencies.upstream_executor.clone() {
        server_state = server_state.with_upstream_executor(upstream_executor, account_health);
    }
    let running_diagnostics = prepared_diagnostics
        .start()
        .map_err(|_| ServeError::Server)?;
    let running_state_writer = match prepared_state_writer.start() {
        Ok(running) => running,
        Err(_) => {
            let _ = running_diagnostics.shutdown().await;
            return Err(ServeError::Server);
        }
    };
    let query = match QueryService::start(DEFAULT_QUERY_WORKERS) {
        Ok(query) => query,
        Err(_) => {
            let _ = running_diagnostics.shutdown().await;
            let _ = running_state_writer.checkpoint_and_shutdown(false).await;
            return Err(ServeError::Server);
        }
    };
    server_state = server_state.with_query_runtime(QueryRuntime::new(
        query.handle(),
        &dependencies.paths.state_db,
        dependencies.session_roots.clone(),
        session_domain_key,
        &dependencies.paths.log_dir,
    ));
    let (server_state, stop_requests) = server_state.with_coordinated_shutdown();
    let listener = match TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await {
        Ok(listener) => listener,
        Err(error) => {
            shutdown_unpublished_observability(query, running_diagnostics, running_state_writer)
                .await;
            return Err(ServeError::Bind(error));
        }
    };
    if let Err(error) = ensure_not_cancelled(cancelled) {
        shutdown_unpublished_observability(query, running_diagnostics, running_state_writer).await;
        return Err(error);
    }
    let running = RunningServer::start(listener, server_state).await;
    let running = match running {
        Ok(running) => running,
        Err(_) => {
            shutdown_unpublished_observability(query, running_diagnostics, running_state_writer)
                .await;
            return Err(ServeError::Server);
        }
    };
    let shutdown = running.shutdown_handle();
    owner.running = Some(running);
    if lifecycle.mark_running().is_err() {
        shutdown.request();
        shutdown_unpublished_observability(query, running_diagnostics, running_state_writer).await;
        return Err(ServeError::Server);
    }

    let record = DiscoveryRecord {
        base_url: format!("http://127.0.0.1:{port}"),
        pid,
        instance_id,
        wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
        api_major: 1,
    };
    if verify_identity(&record).await.is_err() {
        shutdown.request();
        shutdown_unpublished_observability(query, running_diagnostics, running_state_writer).await;
        return Err(ServeError::Readiness);
    }
    if let Err(error) = ensure_not_cancelled(cancelled) {
        shutdown.request();
        shutdown_unpublished_observability(query, running_diagnostics, running_state_writer).await;
        return Err(error);
    }

    owner.discovery = Some((discovery, instance_id));
    let Some((discovery, _)) = owner.discovery.as_ref() else {
        shutdown.request();
        shutdown_unpublished_observability(query, running_diagnostics, running_state_writer).await;
        return Err(ServeError::Server);
    };
    if let Err(error) = dependencies.discovery_publisher.publish(discovery, &record) {
        shutdown.request();
        shutdown_unpublished_observability(query, running_diagnostics, running_state_writer).await;
        return Err(ServeError::Platform(error));
    }
    Ok(StartedService {
        ready: ReadyService {
            instance_id,
            pid,
            port,
        },
        lifecycle,
        shutdown,
        prepared_scheduler,
        running_scheduler: None,
        running_query: Some(query),
        running_diagnostics: Some(running_diagnostics),
        running_state_writer: Some(running_state_writer),
        stop_requests,
    })
}

async fn shutdown_unpublished_observability(
    query: QueryService,
    diagnostics: RunningDiagnosticWriter,
    state: RunningStateWriter,
) {
    let _ = query.shutdown().await;
    let _ = diagnostics.shutdown().await;
    let _ = state.checkpoint_and_shutdown(false).await;
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
        let Some(join) = self.join.take() else {
            return Err(ServeError::Server);
        };
        join.await.map_err(|_| ServeError::Server)?
    }
}

impl Drop for OwnedServeTask {
    fn drop(&mut self) {
        self.cancellation.send_replace(true);
    }
}

struct ServiceLifetime {
    lease: Option<RuntimeLease>,
    running: Option<RunningServer>,
    discovery: Option<(DiscoveryStore, uuid::Uuid)>,
}

impl ServiceLifetime {
    fn new(lease: RuntimeLease) -> Self {
        Self {
            lease: Some(lease),
            running: None,
            discovery: None,
        }
    }

    async fn cleanup(&mut self) -> Result<(), ServeError> {
        let Some(cleanup) = self.take_cleanup() else {
            return Ok(());
        };
        cleanup.finish().await
    }

    fn take_cleanup(&mut self) -> Option<ServiceCleanup> {
        let lease = self.lease.take()?;
        Some(ServiceCleanup {
            running: self.running.take(),
            discovery: self.discovery.take(),
            lease: Some(lease),
        })
    }
}

impl Drop for ServiceLifetime {
    fn drop(&mut self) {
        let Some(cleanup) = self.take_cleanup() else {
            return;
        };
        cleanup.request_shutdown();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            ServiceCleanup::leak(cleanup);
            return;
        };
        std::mem::drop(runtime.spawn(async move {
            let _ = cleanup.finish().await;
        }));
    }
}

struct ServiceCleanup {
    running: Option<RunningServer>,
    discovery: Option<(DiscoveryStore, uuid::Uuid)>,
    lease: Option<RuntimeLease>,
}

impl ServiceCleanup {
    fn request_shutdown(&self) {
        if let Some(running) = self.running.as_ref() {
            running.shutdown_handle().request();
        }
    }

    async fn finish(mut self) -> Result<(), ServeError> {
        self.request_shutdown();
        let server_result = if let Some(running) = self.running.take() {
            running.shutdown().await.map_err(|_| ServeError::Server)
        } else {
            Ok(())
        };
        let discovery_result = if let Some((discovery, instance_id)) = self.discovery.take() {
            discovery
                .remove_if_owned(instance_id)
                .map(|_| ())
                .map_err(ServeError::Platform)
        } else {
            Ok(())
        };
        drop(self.lease.take());
        server_result?;
        discovery_result
    }

    fn leak(cleanup: Self) {
        let _ = Box::leak(Box::new(cleanup));
    }
}

impl Drop for ServiceCleanup {
    fn drop(&mut self) {
        if self.lease.is_none() {
            return;
        }
        Self::leak(Self {
            running: self.running.take(),
            discovery: self.discovery.take(),
            lease: self.lease.take(),
        });
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
    prepared_scheduler: Option<PreparedScheduler>,
    running_scheduler: Option<RunningScheduler>,
    running_query: Option<QueryService>,
    running_diagnostics: Option<RunningDiagnosticWriter>,
    running_state_writer: Option<RunningStateWriter>,
    stop_requests: watch::Receiver<bool>,
}

impl StartedService {
    fn start_observability(&mut self) -> Result<(), ServeError> {
        let Some(prepared) = self.prepared_scheduler.take() else {
            return Ok(());
        };
        self.running_scheduler = Some(
            prepared
                .start_after_readiness()
                .map_err(|_| ServeError::Server)?,
        );
        Ok(())
    }

    async fn shutdown_observability(&mut self) -> Result<(), ServeError> {
        let scheduler_result = if let Some(running) = self.running_scheduler.take() {
            running.shutdown().await.map_err(|_| ServeError::Server)
        } else {
            Ok(())
        };
        let state_flush_result = if let Some(running) = self.running_state_writer.as_ref() {
            running.flush().await.map_err(|_| ServeError::Server)
        } else {
            Ok(())
        };
        let query_result = if let Some(running) = self.running_query.take() {
            running.shutdown().await.map_err(|_| ServeError::Server)
        } else {
            Ok(())
        };
        let (diagnostics_result, diagnostics_idle) =
            if let Some(running) = self.running_diagnostics.take() {
                let handle = running.handle();
                let result = running.shutdown().await.map_err(|_| ServeError::Server);
                (
                    result,
                    handle.has_been_idle_for(wokcore_server::observability::IDLE_TRUNCATE_INTERVAL),
                )
            } else {
                (Ok(()), true)
            };
        let state_shutdown_result = if let Some(running) = self.running_state_writer.take() {
            let proxy_idle = self
                .lifecycle
                .has_been_idle_for(wokcore_server::observability::IDLE_TRUNCATE_INTERVAL);
            running
                .checkpoint_and_shutdown(proxy_idle && diagnostics_idle)
                .await
                .map_err(|_| ServeError::Server)
        } else {
            Ok(())
        };
        scheduler_result
            .and(state_flush_result)
            .and(query_result)
            .and(diagnostics_result)
            .and(state_shutdown_result)
    }

    async fn prepare_graceful_shutdown(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<(), ServeError> {
        let _ = self.lifecycle.begin_drain(timeout).await;
        self.lifecycle.wait_for_zero_active().await;
        let _ = self.lifecycle.request_stop();
        let result = self.shutdown_observability().await;
        self.shutdown.request();
        result
    }
}

fn map_auth_error(error: AuthError) -> ServeError {
    match error {
        AuthError::Storage(error @ StorageError::StateDatabaseCorrupt { .. }) => {
            ServeError::Storage(error)
        }
        _ => ServeError::Auth,
    }
}

fn map_provider_management_error(error: ProviderManagementError) -> ServeError {
    match error {
        ProviderManagementError::InvalidCatalog | ProviderManagementError::InvalidConfiguration => {
            ServeError::InvalidConfig
        }
        _ => ServeError::Server,
    }
}

struct InjectedTokenMetadata {
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdSource>,
}

struct InjectedScanTimestamp {
    clock: Arc<dyn Clock>,
}

impl ScanTimestampSource for InjectedScanTimestamp {
    fn now(&self) -> Option<String> {
        self.clock.now().ok()
    }
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
