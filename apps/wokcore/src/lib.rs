//! WokCore local service command behavior.

use std::{future::Future, io, pin::Pin, sync::Arc, time::Duration};

use secrecy::zeroize::Zeroizing;
use uuid::Uuid;
use wokcore_platform::{AppPaths, DiscoveryRecord, DiscoveryStore, PlatformError};
use wokcore_server::{auth::EntropySource, lifecycle::ServiceLifecycle};
use wokcore_storage::SecretStore;

pub mod cli;
mod commands;
mod production;

pub use production::run_production;

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
    pub(crate) drain_timeout: Duration,
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
            drain_timeout: Duration::from_secs(30),
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

    pub fn with_drain_timeout(mut self, drain_timeout: Duration) -> Self {
        self.drain_timeout = drain_timeout;
        self
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

pub async fn run_with_dependencies(
    cli: cli::Cli,
    dependencies: &RunDependencies,
    output: &mut dyn CommandOutput,
) -> ExitCode {
    commands::run(cli.command, dependencies, output).await
}
