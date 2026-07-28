use std::{
    future::Future,
    net::TcpListener as StdTcpListener,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use clap::Parser;
use reqwest::header::{AUTHORIZATION, HOST};
use secrecy::ExposeSecret;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Notify,
    time::timeout,
};
use uuid::Uuid;
use wokcore::{
    BufferOutput, Clock, DiscoveryPublisher, ExitCode, IdSource, LifecycleObserver,
    ProcessIdentity, RunDependencies, RuntimeValueError, ShutdownSignal, cli::Cli,
    run_with_dependencies,
};
use wokcore_core::{
    id::{ClientId, ProviderId},
    secret::{SecretPurpose, SecretScope},
};
use wokcore_platform::{AppPaths, DiscoveryRecord, DiscoveryStore, PlatformError, RuntimeLease};
use wokcore_server::auth::{
    AuthRegistry, EntropySource, StateAuthMetadataStore, TokenError, TokenMaterial,
};
use wokcore_server::lifecycle::{LifecyclePhase, ServiceLifecycle};
use wokcore_server::observability::SessionRootPaths;
use wokcore_storage::{
    AppConfig, ClientTokenMetadata, ConfigStore, MemorySecretStore, ReadOnlyStateStore,
    SecretStore, ServerConfig, StateStore,
};

const INSTANCE_ID: &str = "019844f0-4de0-7000-8000-000000000010";

struct UnexpectedRuntimeValues;

impl Clock for UnexpectedRuntimeValues {
    fn now(&self) -> Result<String, RuntimeValueError> {
        panic!("read-only absent diagnostics must not ask for a clock")
    }
}

impl IdSource for UnexpectedRuntimeValues {
    fn new_instance_id(&self) -> Result<Uuid, RuntimeValueError> {
        panic!("read-only absent diagnostics must not generate an instance ID")
    }

    fn new_token_id(&self) -> Result<String, RuntimeValueError> {
        panic!("read-only absent diagnostics must not generate a token ID")
    }
}

impl ProcessIdentity for UnexpectedRuntimeValues {
    fn current_pid(&self) -> u32 {
        panic!("read-only absent diagnostics must not ask for the current PID")
    }

    fn is_running(&self, _pid: u32) -> bool {
        panic!("an absent discovery document has no PID to inspect")
    }
}

impl EntropySource for UnexpectedRuntimeValues {
    fn fill(&self, _output: &mut [u8; 32]) -> Result<(), TokenError> {
        panic!("read-only absent diagnostics must not generate secret entropy")
    }
}

impl ShutdownSignal for UnexpectedRuntimeValues {
    fn wait(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async { panic!("read-only commands must not install a shutdown waiter") })
    }
}

#[derive(Debug)]
struct FixedRuntimeValues;

impl Clock for FixedRuntimeValues {
    fn now(&self) -> Result<String, RuntimeValueError> {
        Ok("2026-07-26T12:00:00Z".to_owned())
    }
}

impl IdSource for FixedRuntimeValues {
    fn new_instance_id(&self) -> Result<Uuid, RuntimeValueError> {
        Ok(Uuid::parse_str(INSTANCE_ID).unwrap())
    }

    fn new_token_id(&self) -> Result<String, RuntimeValueError> {
        Ok("019844f0-4de0-7000-8000-000000000011".to_owned())
    }
}

impl ProcessIdentity for FixedRuntimeValues {
    fn current_pid(&self) -> u32 {
        4242
    }

    fn is_running(&self, pid: u32) -> bool {
        pid == 4242
    }
}

impl EntropySource for FixedRuntimeValues {
    fn fill(&self, output: &mut [u8; 32]) -> Result<(), TokenError> {
        output.fill(0x42);
        Ok(())
    }
}

struct GatedEntropy {
    calls: AtomicUsize,
    block_on_call: usize,
    blocked: AtomicBool,
    released: Mutex<bool>,
    release: Condvar,
}

impl GatedEntropy {
    fn new(block_on_call: usize) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            block_on_call,
            blocked: AtomicBool::new(false),
            released: Mutex::new(false),
            release: Condvar::new(),
        }
    }

    async fn wait_until_blocked(&self) {
        timeout(Duration::from_secs(5), async {
            while !self.blocked.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("entropy call did not reach its deterministic gate");
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
}

impl EntropySource for GatedEntropy {
    fn fill(&self, output: &mut [u8; 32]) -> Result<(), TokenError> {
        output.fill(0x42);
        let call = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
        if call == self.block_on_call {
            self.blocked.store(true, Ordering::Release);
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.release.wait(released).unwrap();
            }
        }
        Ok(())
    }
}

struct FailingOnCallEntropy {
    calls: AtomicUsize,
    fail_on_call: usize,
}

impl FailingOnCallEntropy {
    fn new(fail_on_call: usize) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            fail_on_call,
        }
    }
}

impl EntropySource for FailingOnCallEntropy {
    fn fill(&self, output: &mut [u8; 32]) -> Result<(), TokenError> {
        let call = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
        if call == self.fail_on_call {
            return Err(TokenError::EntropyUnavailable);
        }
        output.fill(call as u8);
        Ok(())
    }
}

#[derive(Default)]
struct GatedIds {
    blocked: AtomicBool,
    released: Mutex<bool>,
    release: Condvar,
}

impl GatedIds {
    async fn wait_until_blocked(&self) {
        timeout(Duration::from_secs(5), async {
            while !self.blocked.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("token ID generation did not reach its deterministic gate");
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
}

impl IdSource for GatedIds {
    fn new_instance_id(&self) -> Result<Uuid, RuntimeValueError> {
        Ok(Uuid::parse_str(INSTANCE_ID).unwrap())
    }

    fn new_token_id(&self) -> Result<String, RuntimeValueError> {
        self.blocked.store(true, Ordering::Release);
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.release.wait(released).unwrap();
        }
        Ok("019844f0-4de0-7000-8000-000000000011".to_owned())
    }
}

struct DeadProcess;

impl ProcessIdentity for DeadProcess {
    fn current_pid(&self) -> u32 {
        4242
    }

    fn is_running(&self, _pid: u32) -> bool {
        false
    }
}

#[derive(Default)]
struct ManualShutdown(Notify);

impl ManualShutdown {
    fn trigger(&self) {
        self.0.notify_one();
    }
}

impl ShutdownSignal for ManualShutdown {
    fn wait(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(self.0.notified())
    }
}

struct PublishThenFail {
    replacement: Option<Uuid>,
}

impl DiscoveryPublisher for PublishThenFail {
    fn publish(
        &self,
        store: &DiscoveryStore,
        record: &DiscoveryRecord,
    ) -> Result<(), PlatformError> {
        store.publish(record)?;
        if let Some(instance_id) = self.replacement {
            let mut replacement = record.clone();
            replacement.instance_id = instance_id;
            store.publish(&replacement)?;
        }
        Err(PlatformError::Io {
            source: std::io::Error::other("injected post-commit publish failure"),
        })
    }
}

#[derive(Default)]
struct PublishThenPanic {
    published: AtomicBool,
    panic_requested: Mutex<bool>,
    panic_request: Condvar,
}

impl PublishThenPanic {
    async fn wait_until_published(&self) {
        timeout(Duration::from_secs(5), async {
            while !self.published.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("discovery publisher did not commit before its deterministic panic");
    }

    fn trigger_panic(&self) {
        *self.panic_requested.lock().unwrap() = true;
        self.panic_request.notify_all();
    }
}

impl DiscoveryPublisher for PublishThenPanic {
    fn publish(
        &self,
        store: &DiscoveryStore,
        record: &DiscoveryRecord,
    ) -> Result<(), PlatformError> {
        store.publish(record)?;
        self.published.store(true, Ordering::Release);
        let mut panic_requested = self.panic_requested.lock().unwrap();
        while !*panic_requested {
            panic_requested = self.panic_request.wait(panic_requested).unwrap();
        }
        drop(panic_requested);
        panic!("injected panic after discovery publication")
    }
}

#[derive(Default)]
struct RecordingDiscoveryPublisher(AtomicBool);

impl DiscoveryPublisher for RecordingDiscoveryPublisher {
    fn publish(
        &self,
        store: &DiscoveryStore,
        record: &DiscoveryRecord,
    ) -> Result<(), PlatformError> {
        self.0.store(true, Ordering::Release);
        store.publish(record)
    }
}

#[derive(Default)]
struct CapturingLifecycle(Mutex<Option<ServiceLifecycle>>);

impl CapturingLifecycle {
    fn get(&self) -> Option<ServiceLifecycle> {
        self.0.lock().unwrap().clone()
    }
}

impl LifecycleObserver for CapturingLifecycle {
    fn observe(&self, lifecycle: &ServiceLifecycle) {
        *self.0.lock().unwrap() = Some(lifecycle.clone());
    }
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("wokcore-task5-{}", Uuid::new_v4()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700).create(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        #[cfg(not(unix))]
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

#[cfg(unix)]
#[test]
fn test_directory_is_private_to_the_current_user() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TestDirectory::new();
    assert_eq!(
        std::fs::metadata(directory.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o700
    );
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        if self.0.starts_with(std::env::temp_dir()) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn paths(base: &Path) -> AppPaths {
    let runtime_dir = base.join("runtime");
    AppPaths {
        config_file: base.join("config").join("config.toml"),
        state_db: base.join("state").join("state.sqlite3"),
        log_dir: base.join("state").join("logs"),
        discovery_file: runtime_dir.join("discovery.json"),
        instance_lock: runtime_dir.join("instance.lock"),
        runtime_dir,
    }
}

fn dependencies(paths: AppPaths) -> RunDependencies {
    let unexpected = Arc::new(UnexpectedRuntimeValues);
    RunDependencies::new(
        paths,
        Arc::new(MemorySecretStore::default()),
        unexpected.clone(),
        unexpected.clone(),
        unexpected.clone(),
        unexpected.clone(),
        unexpected,
    )
}

fn runtime_dependencies(
    paths: AppPaths,
    secrets: Arc<MemorySecretStore>,
    shutdown: Arc<ManualShutdown>,
) -> RunDependencies {
    let values = Arc::new(FixedRuntimeValues);
    RunDependencies::new(
        paths,
        secrets,
        values.clone(),
        values.clone(),
        values.clone(),
        values,
        shutdown,
    )
}

fn runtime_dependencies_with_entropy(
    paths: AppPaths,
    secrets: Arc<MemorySecretStore>,
    entropy: Arc<dyn EntropySource>,
    shutdown: Arc<ManualShutdown>,
) -> RunDependencies {
    let values = Arc::new(FixedRuntimeValues);
    RunDependencies::new(
        paths,
        secrets,
        entropy,
        values.clone(),
        values.clone(),
        values,
        shutdown,
    )
}

fn doctor_dependencies(paths: AppPaths, process: Arc<dyn ProcessIdentity>) -> RunDependencies {
    let values = Arc::new(FixedRuntimeValues);
    RunDependencies::new(
        paths,
        Arc::new(MemorySecretStore::default()),
        values.clone(),
        values.clone(),
        values,
        process,
        Arc::new(ManualShutdown::default()),
    )
}

fn reserve_port() -> u16 {
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn persist_port(paths: &AppPaths, port: u16) {
    std::fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
    ConfigStore::new(&paths.config_file)
        .commit(
            0,
            &AppConfig {
                server: ServerConfig { port },
                ..AppConfig::default()
            },
        )
        .unwrap();
}

#[tokio::test]
async fn absent_status_and_doctor_have_stable_json_without_creating_files() {
    let directory = TestDirectory::new();
    let dependencies = dependencies(paths(directory.path()));

    let mut status_output = BufferOutput::default();
    let status = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "status", "--json"]).unwrap(),
        &dependencies,
        &mut status_output,
    )
    .await;
    assert_eq!(status, ExitCode::NotRunning);
    assert_eq!(status_output.stdout(), "{\"code\":\"not_running\"}\n");
    assert_eq!(status_output.stderr(), "");

    let mut doctor_output = BufferOutput::default();
    let doctor = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "doctor", "--json"]).unwrap(),
        &dependencies,
        &mut doctor_output,
    )
    .await;
    assert_eq!(doctor, ExitCode::NotRunning);
    assert_eq!(doctor_output.stdout(), "{\"code\":\"absent\"}\n");
    assert_eq!(doctor_output.stderr(), "");

    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn status_times_out_when_loopback_accepts_without_response_headers() {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    publish_record(
        &paths,
        &DiscoveryRecord {
            base_url: format!("http://127.0.0.1:{port}"),
            pid: 4242,
            instance_id: Uuid::parse_str(INSTANCE_ID).unwrap(),
            wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
            api_major: 1,
        },
    );
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request).await.unwrap();
        std::future::pending::<()>().await;
    });
    let dependencies = doctor_dependencies(paths, Arc::new(FixedRuntimeValues));
    let mut output = BufferOutput::default();

    let exit = timeout(
        Duration::from_secs(3),
        run_with_dependencies(
            Cli::try_parse_from(["wokcore", "status", "--json"]).unwrap(),
            &dependencies,
            &mut output,
        ),
    )
    .await
    .expect("identity request must have a bounded header deadline");

    assert_eq!(exit, ExitCode::NotRunning);
    server.abort();
}

#[tokio::test]
async fn status_times_out_when_loopback_stalls_after_response_headers() {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    publish_record(
        &paths,
        &DiscoveryRecord {
            base_url: format!("http://127.0.0.1:{port}"),
            pid: 4242,
            instance_id: Uuid::parse_str(INSTANCE_ID).unwrap(),
            wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
            api_major: 1,
        },
    );
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request).await.unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 64\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        std::future::pending::<()>().await;
    });
    let dependencies = doctor_dependencies(paths, Arc::new(FixedRuntimeValues));
    let mut output = BufferOutput::default();

    let exit = timeout(
        Duration::from_secs(3),
        run_with_dependencies(
            Cli::try_parse_from(["wokcore", "status", "--json"]).unwrap(),
            &dependencies,
            &mut output,
        ),
    )
    .await
    .expect("identity request must have a bounded body-read deadline");

    assert_eq!(exit, ExitCode::NotRunning);
    server.abort();
}

#[tokio::test]
async fn status_rejects_an_oversized_chunk_before_the_stream_ends() {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    publish_record(
        &paths,
        &DiscoveryRecord {
            base_url: format!("http://127.0.0.1:{port}"),
            pid: 4242,
            instance_id: Uuid::parse_str(INSTANCE_ID).unwrap(),
            wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
            api_major: 1,
        },
    );
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request).await.unwrap();
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n10001\r\n",
            )
            .await
            .unwrap();
        stream.write_all(&vec![b' '; 64 * 1024 + 1]).await.unwrap();
        stream.write_all(b"\r\n").await.unwrap();
        std::future::pending::<()>().await;
    });
    let dependencies = doctor_dependencies(paths, Arc::new(FixedRuntimeValues));
    let mut output = BufferOutput::default();

    let exit = timeout(
        Duration::from_secs(3),
        run_with_dependencies(
            Cli::try_parse_from(["wokcore", "status", "--json"]).unwrap(),
            &dependencies,
            &mut output,
        ),
    )
    .await
    .expect("identity reader must reject before an oversized stream terminates");

    assert_eq!(exit, ExitCode::NotRunning);
    server.abort();
}

async fn status_with_loopback_responses(
    health: String,
    capabilities: String,
) -> (ExitCode, BufferOutput) {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    publish_record(
        &paths,
        &DiscoveryRecord {
            base_url: format!("http://127.0.0.1:{port}"),
            pid: 4242,
            instance_id: Uuid::parse_str(INSTANCE_ID).unwrap(),
            wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
            api_major: 1,
        },
    );
    let mut server = tokio::spawn(async move {
        for (expected_path, body) in [
            ("/wokcore/v1/health", health),
            ("/wokcore/v1/capabilities", capabilities),
        ] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            assert_eq!(request.split_whitespace().nth(1), Some(expected_path));
            write_http_response(&mut stream, "200 OK", &body).await;
        }
    });
    let dependencies = doctor_dependencies(paths, Arc::new(FixedRuntimeValues));
    let mut output = BufferOutput::default();

    let exit = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "status", "--json"]).unwrap(),
        &dependencies,
        &mut output,
    )
    .await;

    if timeout(Duration::from_secs(1), &mut server).await.is_err() {
        server.abort();
        let _ = server.await;
    }
    (exit, output)
}

fn compatible_health_response(extra: &str) -> String {
    format!(r#"{{"status":"ok","instance_id":"{INSTANCE_ID}"{extra}}}"#)
}

fn compatible_capabilities_response(extra: &str) -> String {
    format!(
        r#"{{"wokcore_version":"{}","management_api_major":1,"minimum_management_api_major":1,"maximum_management_api_major":1,"provider_protocols":[],"capabilities":[],"instance_id":"{INSTANCE_ID}"{extra}}}"#,
        env!("CARGO_PKG_VERSION")
    )
}

#[tokio::test]
async fn status_accepts_unknown_health_response_fields_within_the_same_api_major() {
    let (exit, output) = status_with_loopback_responses(
        compatible_health_response(r#","future_health":"available""#),
        compatible_capabilities_response(""),
    )
    .await;

    assert_eq!(exit, ExitCode::Success);
    assert_eq!(output.stderr(), "");
}

#[tokio::test]
async fn status_accepts_unknown_capability_response_fields_within_the_same_api_major() {
    let (exit, output) = status_with_loopback_responses(
        compatible_health_response(""),
        compatible_capabilities_response(r#","future_capability":"available""#),
    )
    .await;

    assert_eq!(exit, ExitCode::Success);
    assert_eq!(output.stderr(), "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_publishes_only_a_ready_loopback_identity_and_removes_its_discovery() {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    let port = reserve_port();
    persist_port(&paths, port);
    let secrets = Arc::new(MemorySecretStore::default());
    let shutdown = Arc::new(ManualShutdown::default());
    let serve_dependencies = runtime_dependencies(paths.clone(), secrets.clone(), shutdown.clone());
    let serve = tokio::spawn(async move {
        let mut output = BufferOutput::default();
        let code = run_with_dependencies(
            Cli::try_parse_from(["wokcore", "serve", "--json"]).unwrap(),
            &serve_dependencies,
            &mut output,
        )
        .await;
        (code, output)
    });

    let discovery = timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(store) = DiscoveryStore::new(&paths)
                && let Ok(record) = store.read()
            {
                break record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(discovery.base_url, format!("http://127.0.0.1:{port}"));
    assert_eq!(discovery.pid, 4242);
    assert_eq!(discovery.instance_id.to_string(), INSTANCE_ID);
    assert_eq!(discovery.api_major, 1);

    let status_dependencies =
        runtime_dependencies(paths.clone(), secrets.clone(), shutdown.clone());
    let mut status_output = BufferOutput::default();
    let status = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "status", "--json"]).unwrap(),
        &status_dependencies,
        &mut status_output,
    )
    .await;
    assert_eq!(status, ExitCode::Success);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(status_output.stdout()).unwrap(),
        serde_json::json!({
            "api_major": 1,
            "code": "running",
            "instance_id": INSTANCE_ID,
            "pid": 4242,
            "wokcore_version": env!("CARGO_PKG_VERSION"),
        })
    );

    let mut provider_catalog_output = BufferOutput::default();
    let provider_catalog = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "providers", "catalog", "--json"]).unwrap(),
        &status_dependencies,
        &mut provider_catalog_output,
    )
    .await;
    assert_eq!(provider_catalog, ExitCode::Success);
    let provider_catalog_json =
        serde_json::from_str::<serde_json::Value>(provider_catalog_output.stdout()).unwrap();
    assert_eq!(provider_catalog_json["schema_version"], 1);
    assert_eq!(
        provider_catalog_json["providers"].as_array().unwrap().len(),
        58
    );
    assert_eq!(provider_catalog_output.stderr(), "");

    let mut provider_status_output = BufferOutput::default();
    let provider_status = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "providers", "status", "--json"]).unwrap(),
        &status_dependencies,
        &mut provider_status_output,
    )
    .await;
    assert_eq!(provider_status, ExitCode::Success);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(provider_status_output.stdout()).unwrap(),
        serde_json::json!({
            "schema_version": 1,
            "revision": 1,
            "snapshot_revision": 1,
            "reload_status": "ready",
            "provider_count": 0,
            "models": [],
            "providers": {"instances": [], "accounts": []},
            "routing": {"aliases": [], "rules": [], "default": null},
        })
    );
    assert_eq!(provider_status_output.stderr(), "");

    shutdown.trigger();
    let (serve_code, serve_output) = timeout(Duration::from_secs(5), serve)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(serve_code, ExitCode::Success);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(serve_output.stdout()).unwrap(),
        serde_json::json!({
            "api_major": 1,
            "code": "started",
            "instance_id": INSTANCE_ID,
            "pid": 4242,
            "port": port,
        })
    );
    assert_eq!(serve_output.stderr(), "");
    assert!(!paths.discovery_file.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_log_and_atomic_diagnostic_export_cli_use_the_live_control_plane() {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    persist_port(&paths, reserve_port());
    let roots = SessionRootPaths {
        codex: directory.path().join("sessions").join("codex"),
        claude: directory.path().join("sessions").join("claude"),
        gemini: directory.path().join("sessions").join("gemini"),
    };
    for root in [&roots.codex, &roots.claude, &roots.gemini] {
        std::fs::create_dir_all(root).unwrap();
    }
    let secrets = Arc::new(MemorySecretStore::default());
    let shutdown = Arc::new(ManualShutdown::default());
    let serve_dependencies = runtime_dependencies(paths.clone(), secrets.clone(), shutdown.clone())
        .with_session_roots(roots.clone());
    let serve = tokio::spawn(async move {
        let mut output = BufferOutput::default();
        run_with_dependencies(
            Cli::try_parse_from(["wokcore", "serve", "--json"]).unwrap(),
            &serve_dependencies,
            &mut output,
        )
        .await
    });
    wait_for_discovery(&paths).await;
    let client_dependencies =
        runtime_dependencies(paths.clone(), secrets.clone(), shutdown.clone())
            .with_session_roots(roots.clone());

    let mut sessions = BufferOutput::default();
    assert_eq!(
        run_with_dependencies(
            Cli::try_parse_from(["wokcore", "sessions", "list", "--json"]).unwrap(),
            &client_dependencies,
            &mut sessions,
        )
        .await,
        ExitCode::Success,
        "stdout={} stderr={}",
        sessions.stdout(),
        sessions.stderr()
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(sessions.stdout()).unwrap()["schema_version"],
        1
    );

    let mut logs = BufferOutput::default();
    let logs_exit = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "logs", "--jsonl"]).unwrap(),
        &client_dependencies,
        &mut logs,
    )
    .await;
    assert_eq!(
        logs_exit,
        ExitCode::Success,
        "stdout={} stderr={}",
        logs.stdout(),
        logs.stderr()
    );
    assert!(
        !logs.stdout().is_empty(),
        "the completed Session request must be visible in the live diagnostic ring"
    );
    let mut found_correlated_request = false;
    for line in logs.stdout().lines() {
        let event = serde_json::from_str::<serde_json::Value>(line).unwrap();
        if event["code"] == "request_completed" {
            found_correlated_request = event["correlations"]["request_id"]
                .as_str()
                .is_some_and(|value| Uuid::parse_str(value).is_ok());
        }
    }
    assert!(found_correlated_request, "logs={}", logs.stdout());

    let output_path = directory.path().join("support.zip");
    let mut exported = BufferOutput::default();
    assert_eq!(
        run_with_dependencies(
            Cli::try_parse_from([
                "wokcore",
                "diagnostics",
                "export",
                "--output",
                output_path.to_str().unwrap(),
            ])
            .unwrap(),
            &client_dependencies,
            &mut exported,
        )
        .await,
        ExitCode::Success,
        "stdout={} stderr={}",
        exported.stdout(),
        exported.stderr()
    );
    assert!(std::fs::read(&output_path).unwrap().starts_with(b"PK"));

    let mut existing = BufferOutput::default();
    assert_eq!(
        run_with_dependencies(
            Cli::try_parse_from([
                "wokcore",
                "diagnostics",
                "export",
                "--output",
                output_path.to_str().unwrap(),
            ])
            .unwrap(),
            &client_dependencies,
            &mut existing,
        )
        .await,
        ExitCode::InvalidInput
    );

    let unsafe_path = roots.codex.join("support.zip");
    let mut unsafe_export = BufferOutput::default();
    assert_eq!(
        run_with_dependencies(
            Cli::try_parse_from([
                "wokcore",
                "diagnostics",
                "export",
                "--output",
                unsafe_path.to_str().unwrap(),
            ])
            .unwrap(),
            &client_dependencies,
            &mut unsafe_export,
        )
        .await,
        ExitCode::InvalidInput
    );
    assert!(!unsafe_path.exists());

    shutdown.trigger();
    assert_eq!(
        timeout(Duration::from_secs(5), serve)
            .await
            .unwrap()
            .unwrap(),
        ExitCode::Success
    );
}

async fn wait_for_discovery(paths: &AppPaths) -> DiscoveryRecord {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(store) = DiscoveryStore::new(paths)
                && let Ok(record) = store.read()
            {
                break record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

async fn wait_for_lease_release(paths: &AppPaths) {
    timeout(Duration::from_secs(5), async {
        loop {
            match RuntimeLease::acquire(paths) {
                Ok(lease) => {
                    drop(lease);
                    break;
                }
                Err(PlatformError::AlreadyRunning) => tokio::task::yield_now().await,
                Err(error) => panic!("unexpected lease acquisition error: {error}"),
            }
        }
    })
    .await
    .expect("cancelled service did not finish releasing its lease");
}

async fn competing_serve_exit(
    paths: AppPaths,
    secrets: Arc<MemorySecretStore>,
) -> Option<ExitCode> {
    let shutdown = Arc::new(ManualShutdown::default());
    shutdown.trigger();
    let dependencies = runtime_dependencies(paths, secrets, shutdown);
    let mut output = BufferOutput::default();
    timeout(
        Duration::from_secs(5),
        run_with_dependencies(
            Cli::try_parse_from(["wokcore", "serve", "--json"]).unwrap(),
            &dependencies,
            &mut output,
        ),
    )
    .await
    .ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aborting_serve_during_bootstrap_retains_the_lease_until_bootstrap_finishes() {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    persist_port(&paths, reserve_port());
    let entropy = Arc::new(GatedEntropy::new(1));
    let publisher = Arc::new(RecordingDiscoveryPublisher::default());
    let secrets = Arc::new(MemorySecretStore::default());
    let dependencies = runtime_dependencies_with_entropy(
        paths.clone(),
        secrets.clone(),
        entropy.clone(),
        Arc::new(ManualShutdown::default()),
    )
    .with_discovery_publisher(publisher.clone());
    let serve = tokio::spawn(async move {
        let mut output = BufferOutput::default();
        run_with_dependencies(
            Cli::try_parse_from(["wokcore", "serve", "--json"]).unwrap(),
            &dependencies,
            &mut output,
        )
        .await
    });
    entropy.wait_until_blocked().await;

    serve.abort();
    assert!(serve.await.unwrap_err().is_cancelled());
    let competing_exit = competing_serve_exit(paths.clone(), secrets).await;

    entropy.release();
    wait_for_lease_release(&paths).await;
    assert!(
        competing_exit == Some(ExitCode::AlreadyRunning),
        "serve cancellation let a second instance start while bootstrap still owned mutation work; \
         observed {competing_exit:?}"
    );
    assert!(!paths.discovery_file.exists());
    assert!(
        !publisher.0.load(Ordering::Acquire),
        "cancelled startup published discovery after bootstrap finished"
    );
}

async fn abort_published_service_with_blocked_request(
    replace_discovery: bool,
) -> (
    Option<ExitCode>,
    Option<DiscoveryRecord>,
    AppPaths,
    TestDirectory,
) {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    let port = reserve_port();
    persist_port(&paths, port);
    let entropy = Arc::new(GatedEntropy::new(5));
    let secrets = Arc::new(MemorySecretStore::default());
    let dependencies = runtime_dependencies_with_entropy(
        paths.clone(),
        secrets.clone(),
        entropy.clone(),
        Arc::new(ManualShutdown::default()),
    );
    let serve = tokio::spawn(async move {
        let mut output = BufferOutput::default();
        run_with_dependencies(
            Cli::try_parse_from(["wokcore", "serve", "--json"]).unwrap(),
            &dependencies,
            &mut output,
        )
        .await
    });
    let record = wait_for_discovery(&paths).await;
    let state = ReadOnlyStateStore::open_live(&paths.state_db).unwrap();
    let management_ref = state
        .runtime_secret_binding("management")
        .unwrap()
        .unwrap()
        .secret_ref;
    drop(state);
    let management = secrets.get(&management_ref).await.unwrap();
    let request = tokio::spawn(async move {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("{}/wokcore/v1/clients/authorize", record.base_url))
            .header(HOST, format!("127.0.0.1:{port}"))
            .header(
                AUTHORIZATION,
                format!("Bearer {}", management.expose_secret()),
            )
            .json(&serde_json::json!({"client_id": "wokrouter"}))
            .send()
            .await
            .unwrap()
    });
    entropy.wait_until_blocked().await;

    serve.abort();
    assert!(serve.await.unwrap_err().is_cancelled());
    let competing_exit = competing_serve_exit(paths.clone(), secrets).await;
    let replacement = replace_discovery.then(|| {
        let mut replacement = DiscoveryStore::new(&paths).unwrap().read().unwrap();
        replacement.instance_id = Uuid::parse_str("019844f0-4de0-7000-8000-000000000088").unwrap();
        DiscoveryStore::new(&paths)
            .unwrap()
            .publish(&replacement)
            .unwrap();
        replacement
    });

    entropy.release();
    let _ = timeout(Duration::from_secs(5), request)
        .await
        .expect("blocked request did not finish after entropy release");
    wait_for_lease_release(&paths).await;
    (competing_exit, replacement, paths, directory)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aborting_serve_after_publish_joins_the_listener_and_removes_owned_discovery_first() {
    let (competing_exit, replacement, paths, _directory) =
        abort_published_service_with_blocked_request(false).await;

    assert!(replacement.is_none());
    assert!(
        competing_exit == Some(ExitCode::AlreadyRunning),
        "serve cancellation let a second instance start before its listener joined; observed \
         {competing_exit:?}"
    );
    assert!(!paths.discovery_file.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aborting_serve_after_publish_preserves_a_replacement_discovery() {
    let (competing_exit, replacement, paths, _directory) =
        abort_published_service_with_blocked_request(true).await;

    assert!(
        competing_exit == Some(ExitCode::AlreadyRunning),
        "replacement was installed after the old service had already released ownership; observed \
         {competing_exit:?}"
    );
    assert_eq!(
        DiscoveryStore::new(&paths).unwrap().read().unwrap(),
        replacement.unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_start_has_exactly_one_lease_owner_and_one_service() {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    persist_port(&paths, reserve_port());
    let secrets = Arc::new(MemorySecretStore::default());
    let shutdown = Arc::new(ManualShutdown::default());
    let owner_dependencies = runtime_dependencies(paths.clone(), secrets.clone(), shutdown.clone());
    let owner = tokio::spawn(async move {
        let mut output = BufferOutput::default();
        let code = run_with_dependencies(
            Cli::try_parse_from(["wokcore", "serve", "--json"]).unwrap(),
            &owner_dependencies,
            &mut output,
        )
        .await;
        (code, output)
    });
    let owned_record = timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(store) = DiscoveryStore::new(&paths)
                && let Ok(record) = store.read()
            {
                break record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let unexpected = Arc::new(UnexpectedRuntimeValues);
    let contender_dependencies = RunDependencies::new(
        paths.clone(),
        Arc::new(MemorySecretStore::default()),
        unexpected.clone(),
        unexpected.clone(),
        unexpected.clone(),
        unexpected.clone(),
        unexpected,
    );
    let mut contender_output = BufferOutput::default();
    let contender = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "serve", "--json"]).unwrap(),
        &contender_dependencies,
        &mut contender_output,
    )
    .await;
    assert_eq!(contender, ExitCode::AlreadyRunning);
    assert_eq!(
        contender_output.stdout(),
        "{\"code\":\"already_running\"}\n"
    );
    assert_eq!(
        DiscoveryStore::new(&paths).unwrap().read().unwrap(),
        owned_record
    );

    shutdown.trigger();
    let (owner_code, _) = timeout(Duration::from_secs(5), owner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owner_code, ExitCode::Success);
}

#[tokio::test]
async fn fixed_port_conflict_has_no_fallback_and_preserves_config_and_discovery() {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    let occupied = StdTcpListener::bind("127.0.0.1:0").unwrap();
    let port = occupied.local_addr().unwrap().port();
    persist_port(&paths, port);
    let config_before = std::fs::read(&paths.config_file).unwrap();
    let discovery_before = paths.discovery_file.try_exists().unwrap();
    let dependencies = runtime_dependencies(
        paths.clone(),
        Arc::new(MemorySecretStore::default()),
        Arc::new(ManualShutdown::default()),
    );
    let mut output = BufferOutput::default();

    let code = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "serve", "--json"]).unwrap(),
        &dependencies,
        &mut output,
    )
    .await;

    assert_eq!(code, ExitCode::PortOccupied);
    assert_eq!(output.stdout(), "{\"code\":\"port_occupied\"}\n");
    assert_eq!(output.stderr(), "");
    assert_eq!(std::fs::read(&paths.config_file).unwrap(), config_before);
    assert_eq!(paths.discovery_file.try_exists().unwrap(), discovery_before);
    assert!(
        StdTcpListener::bind(("127.0.0.1", port)).is_err(),
        "serve silently fell back and released the configured occupied port"
    );
}

#[tokio::test]
async fn serve_classifies_corrupt_active_token_metadata_as_storage_corruption() {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    persist_port(&paths, reserve_port());
    std::fs::create_dir_all(paths.state_db.parent().unwrap()).unwrap();
    let mut state = StateStore::open(&paths.state_db).unwrap();
    state
        .issue_client_token(&ClientTokenMetadata {
            token_id: "corrupt-token".to_owned(),
            client_id: ClientId::new("wokrouter").unwrap(),
            digest: [0x11; 32],
            issued_at: "2026-07-26T12:00:00Z".to_owned(),
        })
        .unwrap();
    drop(state);
    replace_state_bytes(paths.state_db.parent().unwrap(), b"wokrouter", b"bad/id___");
    let dependencies = runtime_dependencies(
        paths,
        Arc::new(MemorySecretStore::default()),
        Arc::new(ManualShutdown::default()),
    );
    let mut output = BufferOutput::default();

    let exit = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "serve", "--json"]).unwrap(),
        &dependencies,
        &mut output,
    )
    .await;

    assert_eq!(exit, ExitCode::StorageCorruption);
    assert_eq!(output.stdout(), "{\"code\":\"storage_corrupt\"}\n");
    assert_eq!(output.stderr(), "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publish_failure_after_commit_removes_only_the_owned_discovery_record() {
    let owned_directory = TestDirectory::new();
    let owned_paths = paths(owned_directory.path());
    persist_port(&owned_paths, reserve_port());
    let owned_dependencies = runtime_dependencies(
        owned_paths.clone(),
        Arc::new(MemorySecretStore::default()),
        Arc::new(ManualShutdown::default()),
    )
    .with_discovery_publisher(Arc::new(PublishThenFail { replacement: None }));
    let mut owned_output = BufferOutput::default();

    assert_eq!(
        run_with_dependencies(
            Cli::try_parse_from(["wokcore", "serve", "--json"]).unwrap(),
            &owned_dependencies,
            &mut owned_output,
        )
        .await,
        ExitCode::InternalFailure
    );
    assert_eq!(owned_output.stdout(), "{\"code\":\"internal_error\"}\n");
    assert!(
        !owned_paths.discovery_file.exists(),
        "a record committed before publish returned an error was left behind"
    );

    let replacement_directory = TestDirectory::new();
    let replacement_paths = paths(replacement_directory.path());
    persist_port(&replacement_paths, reserve_port());
    let replacement_id = Uuid::parse_str("019844f0-4de0-7000-8000-000000000088").unwrap();
    let replacement_dependencies = runtime_dependencies(
        replacement_paths.clone(),
        Arc::new(MemorySecretStore::default()),
        Arc::new(ManualShutdown::default()),
    )
    .with_discovery_publisher(Arc::new(PublishThenFail {
        replacement: Some(replacement_id),
    }));
    let mut replacement_output = BufferOutput::default();

    assert_eq!(
        run_with_dependencies(
            Cli::try_parse_from(["wokcore", "serve", "--json"]).unwrap(),
            &replacement_dependencies,
            &mut replacement_output,
        )
        .await,
        ExitCode::InternalFailure
    );
    assert_eq!(
        DiscoveryStore::new(&replacement_paths)
            .unwrap()
            .read()
            .unwrap()
            .instance_id,
        replacement_id,
        "cleanup removed a replacement record it did not own"
    );
}

async fn panic_after_publish_with_blocked_request(
    replacement_id: Option<Uuid>,
) -> (
    ExitCode,
    BufferOutput,
    Option<ExitCode>,
    AppPaths,
    Arc<MemorySecretStore>,
    Option<DiscoveryRecord>,
    TestDirectory,
) {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    let port = reserve_port();
    persist_port(&paths, port);
    let secrets = Arc::new(MemorySecretStore::default());
    let shutdown = Arc::new(ManualShutdown::default());
    let values = Arc::new(FixedRuntimeValues);
    let ids = Arc::new(GatedIds::default());
    let publisher = Arc::new(PublishThenPanic::default());
    let dependencies = RunDependencies::new(
        paths.clone(),
        secrets.clone(),
        values.clone(),
        values.clone(),
        ids.clone(),
        values,
        shutdown,
    )
    .with_discovery_publisher(publisher.clone());
    let serve = tokio::spawn(async move {
        let mut output = BufferOutput::default();
        let exit = run_with_dependencies(
            Cli::try_parse_from(["wokcore", "serve", "--json"]).unwrap(),
            &dependencies,
            &mut output,
        )
        .await;
        (exit, output)
    });

    publisher.wait_until_published().await;
    let record = DiscoveryStore::new(&paths).unwrap().read().unwrap();
    let state = ReadOnlyStateStore::open_live(&paths.state_db).unwrap();
    let management_ref = state
        .runtime_secret_binding("management")
        .unwrap()
        .unwrap()
        .secret_ref;
    drop(state);
    let management = secrets.get(&management_ref).await.unwrap();
    let authorize = tokio::spawn(async move {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("{}/wokcore/v1/clients/authorize", record.base_url))
            .header(HOST, format!("127.0.0.1:{port}"))
            .header(
                AUTHORIZATION,
                format!("Bearer {}", management.expose_secret()),
            )
            .json(&serde_json::json!({"client_id": "wokrouter"}))
            .send()
            .await
            .unwrap()
    });
    ids.wait_until_blocked().await;

    publisher.trigger_panic();
    let (serve_exit, serve_output) = timeout(Duration::from_secs(5), serve)
        .await
        .expect("serve did not convert its owned child panic")
        .expect("the public serve task panicked");
    let competing_exit = competing_serve_exit(paths.clone(), secrets.clone()).await;
    let replacement = replacement_id.map(|instance_id| {
        let mut replacement = DiscoveryStore::new(&paths).unwrap().read().unwrap();
        replacement.instance_id = instance_id;
        DiscoveryStore::new(&paths)
            .unwrap()
            .publish(&replacement)
            .unwrap();
        replacement
    });

    ids.release();
    let response = timeout(Duration::from_secs(5), authorize)
        .await
        .expect("blocked authorize request did not finish")
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    wait_for_lease_release(&paths).await;

    (
        serve_exit,
        serve_output,
        competing_exit,
        paths,
        secrets,
        replacement,
        directory,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publish_panic_keeps_lease_until_listener_and_owned_discovery_cleanup_finish() {
    let (serve_exit, serve_output, competing_exit, paths, secrets, replacement, _directory) =
        panic_after_publish_with_blocked_request(None).await;

    assert_eq!(serve_exit, ExitCode::InternalFailure);
    assert_eq!(serve_output.stdout(), "{\"code\":\"internal_error\"}\n");
    assert_eq!(serve_output.stderr(), "");
    assert_eq!(competing_exit, Some(ExitCode::AlreadyRunning));
    assert!(replacement.is_none());
    assert!(!paths.discovery_file.exists());

    assert_eq!(
        competing_serve_exit(paths.clone(), secrets).await,
        Some(ExitCode::Success),
        "a new owner could not start after panic cleanup released the listener and lease"
    );
    assert!(!paths.discovery_file.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publish_panic_cleanup_preserves_replacement_discovery() {
    let replacement_id = Uuid::parse_str("019844f0-4de0-7000-8000-000000000088").unwrap();
    let (serve_exit, serve_output, competing_exit, paths, _secrets, replacement, _directory) =
        panic_after_publish_with_blocked_request(Some(replacement_id)).await;

    assert_eq!(serve_exit, ExitCode::InternalFailure);
    assert_eq!(serve_output.stdout(), "{\"code\":\"internal_error\"}\n");
    assert_eq!(serve_output.stderr(), "");
    assert_eq!(competing_exit, Some(ExitCode::AlreadyRunning));
    assert_eq!(
        DiscoveryStore::new(&paths).unwrap().read().unwrap(),
        replacement.unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn composition_request_id_entropy_failure_is_stable_and_reads_entropy_once() {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    let port = reserve_port();
    persist_port(&paths, port);
    let entropy = Arc::new(FailingOnCallEntropy::new(4));
    let shutdown = Arc::new(ManualShutdown::default());
    let dependencies = runtime_dependencies_with_entropy(
        paths.clone(),
        Arc::new(MemorySecretStore::default()),
        entropy.clone(),
        shutdown.clone(),
    );
    let serve = tokio::spawn(async move {
        let mut output = BufferOutput::default();
        let exit = run_with_dependencies(
            Cli::try_parse_from(["wokcore", "serve", "--json"]).unwrap(),
            &dependencies,
            &mut output,
        )
        .await;
        (exit, output)
    });
    let record = wait_for_discovery(&paths).await;

    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("{}/wokcore/v1/health", record.base_url))
        .header(HOST, format!("127.0.0.1:{port}"))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let body: serde_json::Value = response.json().await.unwrap();
    let calls = entropy.calls.load(Ordering::Acquire);

    shutdown.trigger();
    let (serve_exit, _) = timeout(Duration::from_secs(5), serve)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(status, reqwest::StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(request_id, Uuid::nil().to_string());
    assert_eq!(
        body,
        serde_json::json!({
            "error": {
                "code": "internal_error",
                "message": "control-plane request failed",
                "request_id": Uuid::nil().to_string(),
            }
        })
    );
    assert_eq!(
        calls, 4,
        "request-ID failure performed a second entropy read"
    );
    assert_eq!(serve_exit, ExitCode::Success);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn composition_root_waits_for_admitted_work_after_shutdown_timeout() {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    persist_port(&paths, reserve_port());
    let shutdown = Arc::new(ManualShutdown::default());
    let observer = Arc::new(CapturingLifecycle::default());
    let dependencies = runtime_dependencies(
        paths.clone(),
        Arc::new(MemorySecretStore::default()),
        shutdown.clone(),
    )
    .with_lifecycle_observer(observer.clone())
    .with_drain_timeout(Duration::from_millis(25));
    let mut serve = tokio::spawn(async move {
        let mut output = BufferOutput::default();
        run_with_dependencies(
            Cli::try_parse_from(["wokcore", "serve", "--json"]).unwrap(),
            &dependencies,
            &mut output,
        )
        .await
    });

    let lifecycle = timeout(Duration::from_secs(5), async {
        loop {
            if let Some(lifecycle) = observer.get()
                && lifecycle.snapshot().phase == LifecyclePhase::Running
            {
                break lifecycle;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let admitted = lifecycle.admission_controller().try_enter().unwrap();
    shutdown.trigger();
    timeout(Duration::from_secs(1), async {
        loop {
            if lifecycle.snapshot().phase == LifecyclePhase::AwaitingCancellation {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        timeout(Duration::from_millis(50), &mut serve)
            .await
            .is_err(),
        "composition root exited while already-admitted mutation work was still active"
    );

    drop(admitted);
    assert_eq!(
        timeout(Duration::from_secs(5), serve)
            .await
            .unwrap()
            .unwrap(),
        ExitCode::Success
    );
    assert!(!paths.discovery_file.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_stop_caller_does_not_strand_the_service_in_drain() {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    persist_port(&paths, reserve_port());
    let secrets = Arc::new(MemorySecretStore::default());
    let shutdown = Arc::new(ManualShutdown::default());
    let observer = Arc::new(CapturingLifecycle::default());
    let serve_dependencies = runtime_dependencies(paths.clone(), secrets.clone(), shutdown.clone())
        .with_lifecycle_observer(observer.clone());
    let mut serve = tokio::spawn(async move {
        let mut output = BufferOutput::default();
        run_with_dependencies(
            Cli::try_parse_from(["wokcore", "serve", "--json"]).unwrap(),
            &serve_dependencies,
            &mut output,
        )
        .await
    });
    let lifecycle = timeout(Duration::from_secs(5), async {
        loop {
            if let Some(lifecycle) = observer.get()
                && lifecycle.snapshot().phase == LifecyclePhase::Running
            {
                break lifecycle;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            if DiscoveryStore::new(&paths)
                .and_then(|store| store.read())
                .is_ok()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let admitted = lifecycle.admission_controller().try_enter().unwrap();
    let stop_dependencies =
        runtime_dependencies(paths.clone(), secrets, Arc::new(ManualShutdown::default()));
    let stop = tokio::spawn(async move {
        let mut output = BufferOutput::default();
        run_with_dependencies(
            Cli::try_parse_from(["wokcore", "stop", "--json"]).unwrap(),
            &stop_dependencies,
            &mut output,
        )
        .await
    });
    timeout(Duration::from_secs(5), async {
        loop {
            if lifecycle.snapshot().phase == LifecyclePhase::Draining {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    stop.abort();
    assert!(stop.await.unwrap_err().is_cancelled());
    drop(admitted);
    let completed = timeout(Duration::from_secs(2), &mut serve).await;
    if completed.is_err() {
        let _ = lifecycle.cancel_drain();
        shutdown.trigger();
        let _ = timeout(Duration::from_secs(5), serve).await;
    }

    assert_eq!(
        completed
            .expect(
                "caller cancellation abandoned an accepted drain instead of completing owned stop work",
            )
            .unwrap(),
        ExitCode::Success
    );
    assert_eq!(lifecycle.snapshot().phase, LifecyclePhase::Stopping);
    assert!(!paths.discovery_file.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_drain_response_attempts_cancel_without_overwriting_the_original_error() {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let record = DiscoveryRecord {
        base_url: format!("http://127.0.0.1:{port}"),
        pid: 4242,
        instance_id: Uuid::parse_str(INSTANCE_ID).unwrap(),
        wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
        api_major: 1,
    };
    publish_record(&paths, &record);
    std::fs::create_dir_all(paths.state_db.parent().unwrap()).unwrap();
    let mut state = StateStore::open(&paths.state_db).unwrap();
    let secrets = Arc::new(MemorySecretStore::default());
    let scope = SecretScope {
        provider_id: ProviderId::new("wokcore-runtime").unwrap(),
        account_id: None,
        purpose: SecretPurpose::Auxiliary,
    };
    let management = TokenMaterial::generate_admin(&FixedRuntimeValues)
        .unwrap()
        .into_response_value();
    let secret_ref = secrets.put(&scope, management).await.unwrap();
    state
        .bind_runtime_secret_if_absent("management", &secret_ref, "2026-07-26T12:00:00Z")
        .unwrap();
    drop(state);
    let mut server = tokio::spawn(async move {
        let mut paths = Vec::new();
        for (expected_path, status, body) in [
            (
                "/wokcore/v1/health",
                "200 OK",
                format!(r#"{{"status":"ok","instance_id":"{INSTANCE_ID}"}}"#),
            ),
            (
                "/wokcore/v1/capabilities",
                "200 OK",
                format!(
                    r#"{{"wokcore_version":"{}","management_api_major":1,"minimum_management_api_major":1,"maximum_management_api_major":1,"provider_protocols":[],"capabilities":[],"instance_id":"{INSTANCE_ID}"}}"#,
                    env!("CARGO_PKG_VERSION")
                ),
            ),
            ("/wokcore/v1/service/drain", "200 OK", "{".to_owned()),
            (
                "/wokcore/v1/service/drain/cancel",
                "500 Internal Server Error",
                "{".to_owned(),
            ),
        ] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            let path = request.split_whitespace().nth(1).unwrap().to_owned();
            assert_eq!(path, expected_path);
            paths.push(path);
            write_http_response(&mut stream, status, &body).await;
        }
        paths
    });
    let dependencies = runtime_dependencies(paths, secrets, Arc::new(ManualShutdown::default()));
    let mut output = BufferOutput::default();

    let exit = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "stop", "--json"]).unwrap(),
        &dependencies,
        &mut output,
    )
    .await;
    let observed = timeout(Duration::from_secs(1), &mut server).await;
    if observed.is_err() {
        server.abort();
    }

    assert_eq!(exit, ExitCode::InternalFailure);
    assert_eq!(output.stdout(), "{\"code\":\"internal_error\"}\n");
    assert_eq!(
        observed.unwrap().unwrap(),
        [
            "/wokcore/v1/health",
            "/wokcore/v1/capabilities",
            "/wokcore/v1/service/drain",
            "/wokcore/v1/service/drain/cancel",
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn successful_stop_does_not_send_a_drain_cancellation() {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    publish_record(
        &paths,
        &DiscoveryRecord {
            base_url: format!("http://127.0.0.1:{port}"),
            pid: 4242,
            instance_id: Uuid::parse_str(INSTANCE_ID).unwrap(),
            wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
            api_major: 1,
        },
    );
    std::fs::create_dir_all(paths.state_db.parent().unwrap()).unwrap();
    let mut state = StateStore::open(&paths.state_db).unwrap();
    let secrets = Arc::new(MemorySecretStore::default());
    let scope = SecretScope {
        provider_id: ProviderId::new("wokcore-runtime").unwrap(),
        account_id: None,
        purpose: SecretPurpose::Auxiliary,
    };
    let management = TokenMaterial::generate_admin(&FixedRuntimeValues)
        .unwrap()
        .into_response_value();
    let secret_ref = secrets.put(&scope, management).await.unwrap();
    state
        .bind_runtime_secret_if_absent("management", &secret_ref, "2026-07-26T12:00:00Z")
        .unwrap();
    drop(state);
    let server = tokio::spawn(async move {
        let mut observed = Vec::new();
        for (expected_path, body) in [
            (
                "/wokcore/v1/health",
                format!(r#"{{"status":"ok","instance_id":"{INSTANCE_ID}"}}"#),
            ),
            (
                "/wokcore/v1/capabilities",
                format!(
                    r#"{{"wokcore_version":"{}","management_api_major":1,"minimum_management_api_major":1,"maximum_management_api_major":1,"provider_protocols":[],"capabilities":[],"instance_id":"{INSTANCE_ID}"}}"#,
                    env!("CARGO_PKG_VERSION")
                ),
            ),
            (
                "/wokcore/v1/service/drain",
                r#"{"phase":"draining","active_requests":0,"future_drain":"available"}"#.to_owned(),
            ),
            (
                "/wokcore/v1/service/stop",
                r#"{"phase":"stopping","active_requests":0,"future_stop":"available"}"#.to_owned(),
            ),
        ] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            let path = request.split_whitespace().nth(1).unwrap().to_owned();
            assert_eq!(path, expected_path);
            observed.push(path);
            write_http_response(&mut stream, "200 OK", &body).await;
        }
        let extra_request = timeout(Duration::from_millis(200), listener.accept())
            .await
            .is_ok();
        (observed, extra_request)
    });
    let dependencies = runtime_dependencies(paths, secrets, Arc::new(ManualShutdown::default()));
    let mut output = BufferOutput::default();

    let exit = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "stop", "--json"]).unwrap(),
        &dependencies,
        &mut output,
    )
    .await;
    let (observed, extra_request) = server.await.unwrap();

    assert_eq!(exit, ExitCode::Success);
    assert_eq!(output.stdout(), "{\"code\":\"stopped\"}\n");
    assert_eq!(
        observed,
        [
            "/wokcore/v1/health",
            "/wokcore/v1/capabilities",
            "/wokcore/v1/service/drain",
            "/wokcore/v1/service/stop",
        ]
    );
    assert!(!extra_request, "successful stop unexpectedly sent cancel");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authorize_accepts_unknown_response_fields_within_the_same_api_major() {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    publish_record(
        &paths,
        &DiscoveryRecord {
            base_url: format!("http://127.0.0.1:{port}"),
            pid: 4242,
            instance_id: Uuid::parse_str(INSTANCE_ID).unwrap(),
            wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
            api_major: 1,
        },
    );
    std::fs::create_dir_all(paths.state_db.parent().unwrap()).unwrap();
    let mut state = StateStore::open(&paths.state_db).unwrap();
    let secrets = Arc::new(MemorySecretStore::default());
    let scope = SecretScope {
        provider_id: ProviderId::new("wokcore-runtime").unwrap(),
        account_id: None,
        purpose: SecretPurpose::Auxiliary,
    };
    let management = TokenMaterial::generate_admin(&FixedRuntimeValues)
        .unwrap()
        .into_response_value();
    let secret_ref = secrets.put(&scope, management).await.unwrap();
    state
        .bind_runtime_secret_if_absent("management", &secret_ref, "2026-07-26T12:00:00Z")
        .unwrap();
    drop(state);
    let server = tokio::spawn(async move {
        for (expected_path, status, body) in [
            (
                "/wokcore/v1/health",
                "200 OK",
                compatible_health_response(""),
            ),
            (
                "/wokcore/v1/capabilities",
                "200 OK",
                compatible_capabilities_response(""),
            ),
            (
                "/wokcore/v1/clients/authorize",
                "201 Created",
                r#"{"client_id":"wokrouter","token_id":"future-token","token":"wok_proxy_v1_future","future_authorize":"available"}"#.to_owned(),
            ),
        ] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            assert_eq!(request.split_whitespace().nth(1), Some(expected_path));
            write_http_response(&mut stream, status, &body).await;
        }
    });
    let dependencies = runtime_dependencies(paths, secrets, Arc::new(ManualShutdown::default()));
    let mut output = BufferOutput::default();

    let exit = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "authorize", "--client", "wokrouter", "--json"]).unwrap(),
        &dependencies,
        &mut output,
    )
    .await;

    server.await.unwrap();
    assert_eq!(exit, ExitCode::Success);
    assert_eq!(output.stderr(), "");
    let rendered: serde_json::Value = serde_json::from_str(output.stdout()).unwrap();
    assert_eq!(rendered["token_id"], "future-token");
    assert_eq!(rendered["token"], "wok_proxy_v1_future");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authorize_emits_one_proxy_token_and_stop_completes_before_owner_cleanup() {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    let port = reserve_port();
    persist_port(&paths, port);
    let secrets = Arc::new(MemorySecretStore::default());
    let shutdown = Arc::new(ManualShutdown::default());
    let serve_dependencies = runtime_dependencies(paths.clone(), secrets.clone(), shutdown.clone());
    let serve = tokio::spawn(async move {
        let mut output = BufferOutput::default();
        let code = run_with_dependencies(
            Cli::try_parse_from(["wokcore", "serve", "--json"]).unwrap(),
            &serve_dependencies,
            &mut output,
        )
        .await;
        (code, output)
    });
    let record = timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(store) = DiscoveryStore::new(&paths)
                && let Ok(record) = store.read()
            {
                break record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let authorize_dependencies =
        runtime_dependencies(paths.clone(), secrets.clone(), shutdown.clone());
    let mut authorize_output = BufferOutput::default();
    let authorize = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "authorize", "--client", "wokrouter", "--json"]).unwrap(),
        &authorize_dependencies,
        &mut authorize_output,
    )
    .await;
    assert_eq!(authorize, ExitCode::Success);
    assert_eq!(authorize_output.stderr(), "");
    let authorized: serde_json::Value = serde_json::from_str(authorize_output.stdout()).unwrap();
    let token = authorized["token"].as_str().unwrap().to_owned();
    let management_canary = TokenMaterial::generate_admin(&FixedRuntimeValues)
        .unwrap()
        .into_response_value()
        .expose_secret()
        .to_owned();
    assert!(token.starts_with("wok_proxy_v1_"));
    assert_eq!(authorized["client_id"], "wokrouter");
    assert_eq!(
        authorize_output.stdout().matches(&token).count(),
        1,
        "one-time token appeared more than once"
    );
    assert_tree_does_not_contain(directory.path(), &token);
    assert_tree_does_not_contain(directory.path(), &management_canary);

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .unwrap();
    let proxy_as_management = client
        .get(format!("{}/wokcore/v1/service/status", record.base_url))
        .header(HOST, format!("127.0.0.1:{port}"))
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(proxy_as_management.status(), reqwest::StatusCode::FORBIDDEN);
    let proxy_error = proxy_as_management.text().await.unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&proxy_error).unwrap()["error"]["code"],
        "insufficient_scope"
    );
    assert!(!proxy_error.contains(&token));
    assert!(!proxy_error.contains(&management_canary));

    let observer_dependencies =
        runtime_dependencies(paths.clone(), secrets.clone(), shutdown.clone());
    let mut status_output = BufferOutput::default();
    assert_eq!(
        run_with_dependencies(
            Cli::try_parse_from(["wokcore", "status", "--json"]).unwrap(),
            &observer_dependencies,
            &mut status_output,
        )
        .await,
        ExitCode::Success
    );
    let mut doctor_output = BufferOutput::default();
    assert_eq!(
        run_with_dependencies(
            Cli::try_parse_from(["wokcore", "doctor", "--json"]).unwrap(),
            &observer_dependencies,
            &mut doctor_output,
        )
        .await,
        ExitCode::Success
    );

    let stop_dependencies = runtime_dependencies(paths.clone(), secrets.clone(), shutdown.clone());
    let mut stop_output = BufferOutput::default();
    let stop = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "stop", "--json"]).unwrap(),
        &stop_dependencies,
        &mut stop_output,
    )
    .await;
    assert_eq!(stop, ExitCode::Success);
    assert_eq!(stop_output.stdout(), "{\"code\":\"stopped\"}\n");
    assert_eq!(stop_output.stderr(), "");
    let (serve_code, serve_output) = timeout(Duration::from_secs(5), serve)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(serve_code, ExitCode::Success);
    assert!(!paths.discovery_file.exists());

    let metadata = Arc::new(StateAuthMetadataStore::new(
        StateStore::open(&paths.state_db).unwrap(),
    ));
    let scope = SecretScope {
        provider_id: ProviderId::new("wokcore-runtime").unwrap(),
        account_id: None,
        purpose: SecretPurpose::Auxiliary,
    };
    let registry = AuthRegistry::bootstrap(
        secrets,
        metadata,
        Arc::new(FixedRuntimeValues),
        scope,
        "2026-07-26T12:00:00Z".to_owned(),
    )
    .await
    .unwrap();
    let client = registry.validate_client(&token).unwrap();
    assert_eq!(client.client_id.as_str(), "wokrouter");
    assert!(!registry.validate_management(&token));

    for (name, rendered) in [
        ("status stdout", status_output.stdout()),
        ("status stderr", status_output.stderr()),
        ("doctor stdout", doctor_output.stdout()),
        ("doctor stderr", doctor_output.stderr()),
        ("stop stdout", stop_output.stdout()),
        ("stop stderr", stop_output.stderr()),
        ("serve stdout", serve_output.stdout()),
        ("serve stderr", serve_output.stderr()),
    ] {
        assert!(!rendered.contains(&token), "{name} leaked proxy token");
        assert!(
            !rendered.contains(&management_canary),
            "{name} leaked management token"
        );
    }
}

async fn doctor_code(dependencies: &RunDependencies) -> (ExitCode, String) {
    let mut output = BufferOutput::default();
    let exit = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "doctor", "--json"]).unwrap(),
        dependencies,
        &mut output,
    )
    .await;
    assert_eq!(output.stderr(), "");
    let code = serde_json::from_str::<serde_json::Value>(output.stdout()).unwrap()["code"]
        .as_str()
        .unwrap()
        .to_owned();
    (exit, code)
}

fn publish_record(paths: &AppPaths, record: &DiscoveryRecord) {
    let _lease = RuntimeLease::acquire(paths).unwrap();
    DiscoveryStore::new(paths).unwrap().publish(record).unwrap();
}

#[tokio::test]
async fn doctor_offline_matrix_has_stable_codes_and_never_writes() {
    let absent_dir = TestDirectory::new();
    let absent_paths = paths(absent_dir.path());
    let absent_dependencies = doctor_dependencies(absent_paths, Arc::new(UnexpectedRuntimeValues));
    let absent_before = tree_snapshot(absent_dir.path());
    assert_eq!(
        doctor_code(&absent_dependencies).await,
        (ExitCode::NotRunning, "absent".to_owned())
    );
    assert_eq!(tree_snapshot(absent_dir.path()), absent_before);

    let unreachable_dir = TestDirectory::new();
    let unreachable_paths = paths(unreachable_dir.path());
    let unreachable_port = reserve_port();
    publish_record(
        &unreachable_paths,
        &DiscoveryRecord {
            base_url: format!("http://127.0.0.1:{unreachable_port}"),
            pid: 4242,
            instance_id: Uuid::parse_str(INSTANCE_ID).unwrap(),
            wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
            api_major: 1,
        },
    );
    let unreachable_dependencies =
        doctor_dependencies(unreachable_paths, Arc::new(FixedRuntimeValues));
    let unreachable_before = tree_snapshot(unreachable_dir.path());
    assert_eq!(
        doctor_code(&unreachable_dependencies).await,
        (ExitCode::NotRunning, "unreachable".to_owned())
    );
    assert_eq!(tree_snapshot(unreachable_dir.path()), unreachable_before);

    let pid_dir = TestDirectory::new();
    let pid_paths = paths(pid_dir.path());
    publish_record(
        &pid_paths,
        &DiscoveryRecord {
            base_url: format!("http://127.0.0.1:{}", reserve_port()),
            pid: 4242,
            instance_id: Uuid::parse_str(INSTANCE_ID).unwrap(),
            wokcore_version: env!("CARGO_PKG_VERSION").to_owned(),
            api_major: 1,
        },
    );
    let pid_dependencies = doctor_dependencies(pid_paths, Arc::new(DeadProcess));
    let pid_before = tree_snapshot(pid_dir.path());
    assert_eq!(
        doctor_code(&pid_dependencies).await,
        (ExitCode::NotRunning, "pid_mismatch".to_owned())
    );
    assert_eq!(tree_snapshot(pid_dir.path()), pid_before);

    let unsafe_dir = TestDirectory::new();
    let unsafe_paths = paths(unsafe_dir.path());
    std::fs::write(&unsafe_paths.runtime_dir, b"not-a-directory").unwrap();
    let unsafe_dependencies = doctor_dependencies(unsafe_paths, Arc::new(UnexpectedRuntimeValues));
    let unsafe_before = tree_snapshot(unsafe_dir.path());
    assert_eq!(
        doctor_code(&unsafe_dependencies).await,
        (ExitCode::InvalidInput, "unsafe_runtime".to_owned())
    );
    assert_eq!(tree_snapshot(unsafe_dir.path()), unsafe_before);

    let occupied_dir = TestDirectory::new();
    let occupied_paths = paths(occupied_dir.path());
    let occupied = StdTcpListener::bind("127.0.0.1:0").unwrap();
    persist_port(&occupied_paths, occupied.local_addr().unwrap().port());
    let occupied_dependencies =
        doctor_dependencies(occupied_paths, Arc::new(UnexpectedRuntimeValues));
    let occupied_before = tree_snapshot(occupied_dir.path());
    assert_eq!(
        doctor_code(&occupied_dependencies).await,
        (ExitCode::PortOccupied, "port_occupied".to_owned())
    );
    assert_eq!(tree_snapshot(occupied_dir.path()), occupied_before);

    let corrupt_dir = TestDirectory::new();
    let corrupt_paths = paths(corrupt_dir.path());
    std::fs::create_dir_all(corrupt_paths.state_db.parent().unwrap()).unwrap();
    std::fs::write(&corrupt_paths.state_db, b"not a sqlite database").unwrap();
    drop(RuntimeLease::acquire(&corrupt_paths).unwrap());
    let corrupt_dependencies =
        doctor_dependencies(corrupt_paths, Arc::new(UnexpectedRuntimeValues));
    let corrupt_before = tree_snapshot(corrupt_dir.path());
    assert_eq!(
        doctor_code(&corrupt_dependencies).await,
        (ExitCode::StorageCorruption, "storage_corrupt".to_owned())
    );
    assert_eq!(tree_snapshot(corrupt_dir.path()), corrupt_before);
}

#[tokio::test]
async fn doctor_json_reports_a_truncated_main_database_as_storage_corrupt_without_writes() {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    std::fs::create_dir_all(paths.state_db.parent().unwrap()).unwrap();
    std::fs::write(&paths.state_db, vec![0_u8; 99]).unwrap();
    std::fs::write(paths.state_db.with_extension("sqlite3-wal"), vec![0_u8; 32]).unwrap();
    std::fs::write(
        paths.state_db.with_extension("sqlite3-shm"),
        b"unchanged shm",
    )
    .unwrap();
    drop(RuntimeLease::acquire(&paths).unwrap());
    let dependencies = doctor_dependencies(paths, Arc::new(UnexpectedRuntimeValues));
    let before = tree_snapshot(directory.path());
    let mut output = BufferOutput::default();

    let exit = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "doctor", "--json"]).unwrap(),
        &dependencies,
        &mut output,
    )
    .await;

    assert_eq!(exit, ExitCode::StorageCorruption);
    assert_eq!(output.stdout(), "{\"code\":\"storage_corrupt\"}\n");
    assert_eq!(output.stderr(), "");
    assert_eq!(tree_snapshot(directory.path()), before);
}

#[tokio::test]
async fn doctor_json_reports_a_truncated_nonempty_wal_as_storage_corrupt_without_writes() {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    std::fs::create_dir_all(paths.state_db.parent().unwrap()).unwrap();
    drop(StateStore::open(&paths.state_db).unwrap());
    std::fs::write(paths.state_db.with_extension("sqlite3-wal"), vec![0_u8; 31]).unwrap();
    std::fs::write(
        paths.state_db.with_extension("sqlite3-shm"),
        b"unchanged shm",
    )
    .unwrap();
    drop(RuntimeLease::acquire(&paths).unwrap());
    let dependencies = doctor_dependencies(paths, Arc::new(UnexpectedRuntimeValues));
    let before = tree_snapshot(directory.path());
    let mut output = BufferOutput::default();

    let exit = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "doctor", "--json"]).unwrap(),
        &dependencies,
        &mut output,
    )
    .await;

    assert_eq!(exit, ExitCode::StorageCorruption);
    assert_eq!(output.stdout(), "{\"code\":\"storage_corrupt\"}\n");
    assert_eq!(output.stderr(), "");
    assert_eq!(tree_snapshot(directory.path()), before);
}

#[tokio::test]
async fn doctor_offline_state_inspection_yields_to_the_runtime_writer_lease() {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    std::fs::create_dir_all(paths.state_db.parent().unwrap()).unwrap();
    StateStore::open(&paths.state_db).unwrap();
    let lease = RuntimeLease::acquire(&paths).unwrap();
    let dependencies = doctor_dependencies(paths, Arc::new(UnexpectedRuntimeValues));
    let before = tree_snapshot(directory.path());

    assert_eq!(
        doctor_code(&dependencies).await,
        (ExitCode::NotRunning, "unreachable".to_owned())
    );
    assert_eq!(tree_snapshot(directory.path()), before);

    drop(lease);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn doctor_online_identity_matrix_is_read_only() {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    persist_port(&paths, reserve_port());
    let secrets = Arc::new(MemorySecretStore::default());
    let shutdown = Arc::new(ManualShutdown::default());
    let serve_dependencies = runtime_dependencies(paths.clone(), secrets, shutdown.clone());
    let serve = tokio::spawn(async move {
        let mut output = BufferOutput::default();
        run_with_dependencies(
            Cli::try_parse_from(["wokcore", "serve", "--json"]).unwrap(),
            &serve_dependencies,
            &mut output,
        )
        .await
    });
    let original = timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(store) = DiscoveryStore::new(&paths)
                && let Ok(record) = store.read()
            {
                break record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let doctor_dependencies = doctor_dependencies(paths.clone(), Arc::new(FixedRuntimeValues));

    let healthy_before = tree_snapshot(directory.path());
    assert_eq!(
        doctor_code(&doctor_dependencies).await,
        (ExitCode::Success, "healthy".to_owned())
    );
    assert_eq!(tree_snapshot(directory.path()), healthy_before);

    let mut instance_mismatch = original.clone();
    instance_mismatch.instance_id =
        Uuid::parse_str("019844f0-4de0-7000-8000-000000000099").unwrap();
    DiscoveryStore::new(&paths)
        .unwrap()
        .publish(&instance_mismatch)
        .unwrap();
    let instance_before = tree_snapshot(directory.path());
    assert_eq!(
        doctor_code(&doctor_dependencies).await,
        (ExitCode::InvalidInput, "instance_mismatch".to_owned())
    );
    assert_eq!(tree_snapshot(directory.path()), instance_before);

    let mut api_mismatch = original.clone();
    api_mismatch.api_major = 2;
    DiscoveryStore::new(&paths)
        .unwrap()
        .publish(&api_mismatch)
        .unwrap();
    let api_before = tree_snapshot(directory.path());
    assert_eq!(
        doctor_code(&doctor_dependencies).await,
        (ExitCode::InvalidInput, "api_mismatch".to_owned())
    );
    assert_eq!(tree_snapshot(directory.path()), api_before);

    DiscoveryStore::new(&paths)
        .unwrap()
        .publish(&original)
        .unwrap();
    shutdown.trigger();
    assert_eq!(
        timeout(Duration::from_secs(5), serve)
            .await
            .unwrap()
            .unwrap(),
        ExitCode::Success
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repeated_identity_and_auth_validation_are_byte_and_mtime_read_only() {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    let port = reserve_port();
    persist_port(&paths, port);
    let secrets = Arc::new(MemorySecretStore::default());
    let shutdown = Arc::new(ManualShutdown::default());
    let serve_dependencies = runtime_dependencies(paths.clone(), secrets.clone(), shutdown.clone());
    let serve = tokio::spawn(async move {
        let mut output = BufferOutput::default();
        run_with_dependencies(
            Cli::try_parse_from(["wokcore", "serve", "--json"]).unwrap(),
            &serve_dependencies,
            &mut output,
        )
        .await
    });
    let record = timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(store) = DiscoveryStore::new(&paths)
                && let Ok(record) = store.read()
            {
                break record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let state = ReadOnlyStateStore::open_live(&paths.state_db).unwrap();
    let management_ref = state
        .runtime_secret_binding("management")
        .unwrap()
        .unwrap()
        .secret_ref;
    drop(state);
    let management = secrets.get(&management_ref).await.unwrap();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .unwrap();
    let dependencies = runtime_dependencies(paths.clone(), secrets.clone(), shutdown.clone());
    let before = tree_snapshot(directory.path());

    for _ in 0..20 {
        let mut status_output = BufferOutput::default();
        assert_eq!(
            run_with_dependencies(
                Cli::try_parse_from(["wokcore", "status", "--json"]).unwrap(),
                &dependencies,
                &mut status_output,
            )
            .await,
            ExitCode::Success
        );
        let mut doctor_output = BufferOutput::default();
        assert_eq!(
            run_with_dependencies(
                Cli::try_parse_from(["wokcore", "doctor", "--json"]).unwrap(),
                &dependencies,
                &mut doctor_output,
            )
            .await,
            ExitCode::Success
        );
        let authenticated = client
            .get(format!("{}/wokcore/v1/service/status", record.base_url))
            .header(HOST, format!("127.0.0.1:{port}"))
            .header(
                AUTHORIZATION,
                format!("Bearer {}", management.expose_secret()),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(authenticated.status(), reqwest::StatusCode::OK);
    }

    assert_eq!(tree_snapshot(directory.path()), before);
    shutdown.trigger();
    assert_eq!(
        timeout(Duration::from_secs(5), serve)
            .await
            .unwrap()
            .unwrap(),
        ExitCode::Success
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn owner_shutdown_preserves_a_replacement_discovery_record() {
    let directory = TestDirectory::new();
    let paths = paths(directory.path());
    persist_port(&paths, reserve_port());
    let secrets = Arc::new(MemorySecretStore::default());
    let shutdown = Arc::new(ManualShutdown::default());
    let serve_dependencies = runtime_dependencies(paths.clone(), secrets, shutdown.clone());
    let serve = tokio::spawn(async move {
        let mut output = BufferOutput::default();
        run_with_dependencies(
            Cli::try_parse_from(["wokcore", "serve", "--json"]).unwrap(),
            &serve_dependencies,
            &mut output,
        )
        .await
    });
    let mut replacement = timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(store) = DiscoveryStore::new(&paths)
                && let Ok(record) = store.read()
            {
                break record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    replacement.instance_id = Uuid::parse_str("019844f0-4de0-7000-8000-000000000088").unwrap();
    let store = DiscoveryStore::new(&paths).unwrap();
    store.publish(&replacement).unwrap();

    shutdown.trigger();
    assert_eq!(
        timeout(Duration::from_secs(5), serve)
            .await
            .unwrap()
            .unwrap(),
        ExitCode::Success
    );
    assert_eq!(store.read().unwrap(), replacement);
    assert!(store.remove_if_owned(replacement.instance_id).unwrap());
}

fn tree_snapshot(root: &Path) -> Vec<(String, Option<Vec<u8>>, u64, std::time::SystemTime)> {
    fn visit(
        root: &Path,
        path: &Path,
        entries: &mut Vec<(String, Option<Vec<u8>>, u64, std::time::SystemTime)>,
    ) {
        let Ok(children) = std::fs::read_dir(path) else {
            return;
        };
        for child in children {
            let child = child.unwrap();
            let metadata = child.metadata().unwrap();
            if metadata.is_dir() {
                visit(root, &child.path(), entries);
            } else {
                let contents = match std::fs::read(child.path()) {
                    Ok(contents) => Some(contents),
                    Err(_) if child.file_name() == "instance.lock" => None,
                    Err(error) => panic!("failed to snapshot {}: {error}", child.path().display()),
                };
                entries.push((
                    child
                        .path()
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    contents,
                    metadata.len(),
                    metadata.modified().unwrap(),
                ));
            }
        }
    }

    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries
}

fn assert_tree_does_not_contain(root: &Path, canary: &str) {
    for (path, contents, _, _) in tree_snapshot(root) {
        if let Some(contents) = contents {
            assert!(
                !contents
                    .windows(canary.len())
                    .any(|window| window == canary.as_bytes()),
                "{path} persisted a raw token"
            );
        }
    }
}

fn replace_state_bytes(directory: &Path, from: &[u8], to: &[u8]) {
    assert_eq!(from.len(), to.len());
    let mut replacements = 0;
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if !path.is_file() {
            continue;
        }
        let mut contents = std::fs::read(&path).unwrap();
        for offset in 0..=contents.len().saturating_sub(from.len()) {
            if contents[offset..].starts_with(from) {
                contents[offset..offset + from.len()].copy_from_slice(to);
                replacements += 1;
            }
        }
        std::fs::write(path, contents).unwrap();
    }
    assert_eq!(replacements, 1, "expected one active client identifier");
}

async fn read_http_request(stream: &mut tokio::net::TcpStream) -> String {
    let mut request = Vec::new();
    loop {
        let mut chunk = [0_u8; 1024];
        let read = stream.read(&mut chunk).await.unwrap();
        assert_ne!(read, 0, "client closed before sending complete headers");
        request.extend_from_slice(&chunk[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            return String::from_utf8(request).unwrap();
        }
    }
}

async fn write_http_response(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
}
