//! WokCore local service command behavior.

use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use secrecy::{SecretString, zeroize::Zeroizing};
use url::{Host, Url};
use uuid::Uuid;
use wokcore_platform::{AppPaths, DiscoveryRecord, DiscoveryStore, PlatformError};
use wokcore_server::{
    auth::EntropySource, data_plane::UpstreamExecutor, lifecycle::ServiceLifecycle,
    observability::SessionRootPaths,
};
use wokcore_storage::SecretStore;

pub mod cli;
mod commands;
mod production;
pub mod runtime;

pub use production::run_production;

pub(crate) const PRODUCTION_UPDATE_ORIGIN: &str =
    "https://github.com/hongjiadev/wokcore/releases/latest/download/";

/// Stable process exit codes for local service commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ExitCode {
    Success = 0,
    InternalFailure = 1,
    InvalidInput = 2,
    NotRunning = 3,
    AlreadyRunning = 4,
    PortOccupied = 5,
    AuthenticationFailure = 6,
    StorageCorruption = 7,
}

impl ExitCode {
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    pub const fn as_i32(self) -> i32 {
        self as i32
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("an injected runtime value is unavailable")]
pub struct RuntimeValueError;

pub trait Clock: Send + Sync {
    fn now(&self) -> Result<String, RuntimeValueError>;
}

pub trait IdSource: Send + Sync {
    fn new_instance_id(&self) -> Result<Uuid, RuntimeValueError>;

    fn new_token_id(&self) -> Result<String, RuntimeValueError>;
}

pub trait ProcessIdentity: Send + Sync {
    fn current_pid(&self) -> u32;

    fn is_running(&self, pid: u32) -> bool;

    fn matches_executable(&self, pid: u32, _expected: &Path) -> bool {
        self.is_running(pid)
    }
}

#[async_trait]
pub(crate) trait UpdateProcess: Send + Sync {
    fn current_executable(&self) -> Result<PathBuf, RuntimeValueError>;

    async fn spawn_service(
        &self,
        executable: &Path,
    ) -> Result<Box<dyn UpdateChild>, RuntimeValueError>;
}

#[async_trait]
pub(crate) trait UpdateChild: Send {
    fn pid(&self) -> Option<u32>;

    async fn kill(&mut self) -> Result<(), RuntimeValueError>;

    fn detach(&mut self);
}

pub trait ShutdownSignal: Send + Sync {
    fn wait(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

pub trait DiscoveryPublisher: Send + Sync {
    fn publish(
        &self,
        store: &DiscoveryStore,
        record: &DiscoveryRecord,
    ) -> Result<(), PlatformError>;
}

pub trait LifecycleObserver: Send + Sync {
    fn observe(&self, lifecycle: &ServiceLifecycle);
}

pub trait CommandOutput: Send {
    fn write_stdout(&mut self, value: &str) -> io::Result<()>;

    fn write_stderr(&mut self, value: &str) -> io::Result<()>;
}

pub trait SecretInput: Send + Sync {
    fn read_secret(&self, maximum_bytes: usize) -> io::Result<SecretString>;
}

#[derive(Default)]
pub struct BufferOutput {
    stdout: Zeroizing<String>,
    stderr: Zeroizing<String>,
}

impl BufferOutput {
    pub fn stdout(&self) -> &str {
        &self.stdout
    }

    pub fn stderr(&self) -> &str {
        &self.stderr
    }
}

impl CommandOutput for BufferOutput {
    fn write_stdout(&mut self, value: &str) -> io::Result<()> {
        self.stdout.push_str(value);
        Ok(())
    }

    fn write_stderr(&mut self, value: &str) -> io::Result<()> {
        self.stderr.push_str(value);
        Ok(())
    }
}

#[derive(Clone)]
pub struct RunDependencies {
    pub(crate) paths: AppPaths,
    pub(crate) secrets: Arc<dyn SecretStore>,
    pub(crate) entropy: Arc<dyn EntropySource>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) ids: Arc<dyn IdSource>,
    pub(crate) process: Arc<dyn ProcessIdentity>,
    pub(crate) shutdown: Arc<dyn ShutdownSignal>,
    pub(crate) discovery_publisher: Arc<dyn DiscoveryPublisher>,
    pub(crate) lifecycle_observer: Arc<dyn LifecycleObserver>,
    pub(crate) secret_input: Arc<dyn SecretInput>,
    pub(crate) session_roots: Option<SessionRootPaths>,
    pub(crate) upstream_executor: Option<Arc<dyn UpstreamExecutor>>,
    pub(crate) drain_timeout: Duration,
    pub(crate) update_source: Option<UpdateSource>,
    pub(crate) update_process: Arc<dyn UpdateProcess>,
}

#[derive(Clone)]
pub(crate) struct UpdateSource {
    pub(crate) origin: Url,
    pub(crate) public_key: Arc<str>,
}

impl RunDependencies {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        paths: AppPaths,
        secrets: Arc<dyn SecretStore>,
        entropy: Arc<dyn EntropySource>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
        process: Arc<dyn ProcessIdentity>,
        shutdown: Arc<dyn ShutdownSignal>,
    ) -> Self {
        Self {
            paths,
            secrets,
            entropy,
            clock,
            ids,
            process,
            shutdown,
            discovery_publisher: Arc::new(PlatformDiscoveryPublisher),
            lifecycle_observer: Arc::new(NoopLifecycleObserver),
            secret_input: Arc::new(UnavailableSecretInput),
            session_roots: None,
            upstream_executor: None,
            drain_timeout: Duration::from_secs(30),
            update_source: None,
            update_process: Arc::new(UnavailableUpdateProcess),
        }
    }

    pub fn with_discovery_publisher(
        mut self,
        discovery_publisher: Arc<dyn DiscoveryPublisher>,
    ) -> Self {
        self.discovery_publisher = discovery_publisher;
        self
    }

    pub fn with_lifecycle_observer(
        mut self,
        lifecycle_observer: Arc<dyn LifecycleObserver>,
    ) -> Self {
        self.lifecycle_observer = lifecycle_observer;
        self
    }

    pub fn with_session_roots(mut self, session_roots: SessionRootPaths) -> Self {
        self.session_roots = Some(session_roots);
        self
    }

    pub fn with_secret_input(mut self, secret_input: Arc<dyn SecretInput>) -> Self {
        self.secret_input = secret_input;
        self
    }

    pub fn with_upstream_executor(mut self, upstream_executor: Arc<dyn UpstreamExecutor>) -> Self {
        self.upstream_executor = Some(upstream_executor);
        self
    }

    pub fn with_drain_timeout(mut self, drain_timeout: Duration) -> Self {
        self.drain_timeout = drain_timeout;
        self
    }

    pub(crate) fn with_update_process(mut self, update_process: Arc<dyn UpdateProcess>) -> Self {
        self.update_process = update_process;
        self
    }

    #[doc(hidden)]
    pub fn with_loopback_update_source(
        mut self,
        origin: Url,
        public_key: impl Into<Arc<str>>,
    ) -> Result<Self, RuntimeValueError> {
        if origin.scheme() != "http"
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.query().is_some()
            || origin.fragment().is_some()
            || !origin.path().ends_with('/')
            || !matches!(origin.host(), Some(Host::Ipv4(address)) if address.is_loopback())
        {
            return Err(RuntimeValueError);
        }
        self.update_source = Some(UpdateSource {
            origin,
            public_key: public_key.into(),
        });
        Ok(self)
    }
}

struct PlatformDiscoveryPublisher;

impl DiscoveryPublisher for PlatformDiscoveryPublisher {
    fn publish(
        &self,
        store: &DiscoveryStore,
        record: &DiscoveryRecord,
    ) -> Result<(), PlatformError> {
        store.publish(record)
    }
}

struct NoopLifecycleObserver;

impl LifecycleObserver for NoopLifecycleObserver {
    fn observe(&self, _lifecycle: &ServiceLifecycle) {}
}

struct UnavailableSecretInput;

impl SecretInput for UnavailableSecretInput {
    fn read_secret(&self, _maximum_bytes: usize) -> io::Result<SecretString> {
        Err(io::Error::other("secret input is unavailable"))
    }
}

struct UnavailableUpdateProcess;

#[async_trait]
impl UpdateProcess for UnavailableUpdateProcess {
    fn current_executable(&self) -> Result<PathBuf, RuntimeValueError> {
        Err(RuntimeValueError)
    }

    async fn spawn_service(
        &self,
        _executable: &Path,
    ) -> Result<Box<dyn UpdateChild>, RuntimeValueError> {
        Err(RuntimeValueError)
    }
}

pub async fn run_with_dependencies(
    cli: cli::Cli,
    dependencies: &RunDependencies,
    output: &mut dyn CommandOutput,
) -> ExitCode {
    commands::run(cli.command, dependencies, output).await
}
