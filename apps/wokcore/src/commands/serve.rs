use std::{
    io,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
};

use serde_json::json;
use tokio::net::TcpListener;
use wokcore_core::{
    id::ProviderId,
    secret::{SecretPurpose, SecretScope},
};
use wokcore_platform::{DiscoveryRecord, DiscoveryStore, PlatformError, RuntimeLease};
use wokcore_server::{
    RunningServer, ServerState,
    auth::{AuthRegistry, StateAuthMetadataStore},
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
    let _lease = RuntimeLease::acquire(&dependencies.paths).map_err(ServeError::Platform)?;
    let config = ConfigStore::new(&dependencies.paths.config_file)
        .load()
        .map_err(ServeError::Storage)?;
    let port = config.config.server.port;
    if port == 0 {
        return Err(ServeError::InvalidConfig);
    }

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
    .map_err(|_| ServeError::Auth)?;

    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
        .await
        .map_err(ServeError::Bind)?;
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
    lifecycle.mark_running().map_err(|_| ServeError::Server)?;

    let record = DiscoveryRecord {
        base_url: format!("http://127.0.0.1:{port}"),
        pid,
        instance_id,
        wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
        api_major: 1,
    };
    if verify_identity(&record).await.is_err() {
        shutdown.request();
        let _ = running.wait().await;
        return Err(ServeError::Readiness);
    }

    let discovery = DiscoveryStore::new(&dependencies.paths).map_err(ServeError::Platform)?;
    if let Err(error) = dependencies
        .discovery_publisher
        .publish(&discovery, &record)
    {
        shutdown.request();
        let _ = running.wait().await;
        let _ = discovery.remove_if_owned(instance_id);
        return Err(ServeError::Platform(error));
    }
    let announced = if options.json {
        write_json(
            output,
            &json!({
                "api_major": 1,
                "code": "started",
                "instance_id": instance_id,
                "pid": pid,
                "port": port,
            }),
        )
    } else {
        output.write_stdout("WokCore local service started.\n")
    };
    if announced.is_err() {
        shutdown.request();
    }

    let mut wait = Box::pin(running.wait());
    let server_result = tokio::select! {
        result = &mut wait => result,
        () = dependencies.shutdown.wait() => {
            let _ = lifecycle.begin_drain(dependencies.drain_timeout).await;
            lifecycle.wait_for_zero_active().await;
            let _ = lifecycle.request_stop();
            shutdown.request();
            wait.await
        }
    };
    let cleanup_result = discovery.remove_if_owned(instance_id);
    server_result.map_err(|_| ServeError::Server)?;
    cleanup_result.map_err(ServeError::Platform)?;
    announced.map_err(|_| ServeError::Io)?;
    Ok(())
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
    Readiness,
    Server,
}
