use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{Router, body::Body, response::Response, routing::get};
use clap::Parser;
use serde_json::json;
use tempfile::tempdir;
use tokio::net::TcpListener;
use url::Url;
use uuid::Uuid;
use wokcore::{
    BufferOutput, Clock, ExitCode, IdSource, ProcessIdentity, RunDependencies, RuntimeValueError,
    ShutdownSignal,
    cli::{Cli, parse_command},
    run_with_dependencies,
};
use wokcore_platform::AppPaths;
use wokcore_server::auth::{EntropySource, TokenError};
use wokcore_storage::MemorySecretStore;

const MIGRATION_PUBLIC_KEY: &str = include_str!("fixtures/update/migration-minisign.pub");
const MIGRATION_V1: &[u8] = include_bytes!("fixtures/update/migration-wokcore-update-v1.json");
const MIGRATION_V1_SIGNATURE: &[u8] =
    include_bytes!("fixtures/update/migration-wokcore-update-v1.json.minisig");
const MIGRATION_V2: &[u8] = include_bytes!("fixtures/update/migration-wokcore-update-v2.json");
const MIGRATION_V2_SIGNATURE: &[u8] =
    include_bytes!("fixtures/update/migration-wokcore-update-v2.json.minisig");

struct UnusedRuntime;

impl Clock for UnusedRuntime {
    fn now(&self) -> Result<String, RuntimeValueError> {
        panic!("update availability must not request a clock")
    }
}

impl IdSource for UnusedRuntime {
    fn new_instance_id(&self) -> Result<Uuid, RuntimeValueError> {
        panic!("update availability must not generate an instance ID")
    }

    fn new_token_id(&self) -> Result<String, RuntimeValueError> {
        panic!("update availability must not generate a token ID")
    }
}

impl ProcessIdentity for UnusedRuntime {
    fn current_pid(&self) -> u32 {
        panic!("update availability must not request a PID")
    }

    fn is_running(&self, _pid: u32) -> bool {
        panic!("update availability must not inspect processes")
    }
}

impl EntropySource for UnusedRuntime {
    fn fill(&self, _output: &mut [u8; 32]) -> Result<(), TokenError> {
        panic!("update availability must not request entropy")
    }
}

impl ShutdownSignal for UnusedRuntime {
    fn wait(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async { panic!("update availability must not wait for shutdown") })
    }
}

fn dependencies() -> (tempfile::TempDir, RunDependencies) {
    let directory = tempdir().unwrap();
    let runtime_dir = directory.path().join("runtime");
    let paths = AppPaths {
        config_file: directory.path().join("config").join("config.toml"),
        state_db: directory.path().join("state").join("state.sqlite3"),
        log_dir: directory.path().join("state").join("logs"),
        discovery_file: runtime_dir.join("discovery.json"),
        instance_lock: runtime_dir.join("instance.lock"),
        runtime_dir,
    };
    let unused = Arc::new(UnusedRuntime);
    let dependencies = RunDependencies::new(
        paths,
        Arc::new(MemorySecretStore::default()),
        unused.clone(),
        unused.clone(),
        unused.clone(),
        unused.clone(),
        unused,
    );
    (directory, dependencies)
}

#[test]
fn update_cli_accepts_exactly_one_explicit_action_with_json_output() {
    assert!(parse_command(["wokcore", "update", "--check", "--json"]).is_ok());
    assert!(parse_command(["wokcore", "update", "--install", "--json"]).is_ok());
    assert!(
        parse_command([
            "wokcore",
            "update",
            "--install",
            "--json",
            "--progress-jsonl",
        ])
        .is_ok()
    );
    assert!(
        parse_command(["wokcore", "update", "--check", "--json", "--progress-jsonl",]).is_err()
    );
}

#[test]
fn update_cli_rejects_missing_or_conflicting_actions() {
    assert!(parse_command(["wokcore", "update", "--json"]).is_err());
    assert!(parse_command(["wokcore", "update", "--check", "--install", "--json",]).is_err());
}

#[tokio::test]
async fn update_check_without_a_verification_key_fails_closed_before_network_access() {
    let (_directory, dependencies) = dependencies();
    let mut output = BufferOutput::default();

    let exit = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "update", "--check", "--json"]).unwrap(),
        &dependencies,
        &mut output,
    )
    .await;

    assert_eq!(exit, ExitCode::InternalFailure);
    assert_eq!(output.stdout(), "{\"code\":\"update_unavailable\"}\n");
    assert_eq!(output.stderr(), "");
}

#[tokio::test]
async fn update_check_falls_back_to_v1_only_when_v2_is_not_found() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route(
            "/wokcore-update-v2.json",
            get(|| async { Response::builder().status(404).body(Body::empty()).unwrap() }),
        )
        .route(
            "/wokcore-update-v1.json",
            get(|| async { Response::new(Body::from(MIGRATION_V1)) }),
        )
        .route(
            "/wokcore-update-v1.json.minisig",
            get(|| async { Response::new(Body::from(MIGRATION_V1_SIGNATURE)) }),
        );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let origin = Url::parse(&format!("http://{address}/")).unwrap();
    let (_directory, dependencies) = dependencies();
    let dependencies = dependencies
        .with_loopback_update_source(origin, MIGRATION_PUBLIC_KEY)
        .unwrap();
    let mut output = BufferOutput::default();

    let exit = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "update", "--check", "--json"]).unwrap(),
        &dependencies,
        &mut output,
    )
    .await;

    server.abort();
    assert_eq!(exit, ExitCode::Success);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(output.stdout()).unwrap(),
        json!({
            "code": "update_available",
            "current_version": env!("CARGO_PKG_VERSION"),
            "target": wokcore_platform::update::current_target(),
            "version": "1.2.3",
        }),
    );
    assert_eq!(output.stderr(), "");
}

#[tokio::test]
async fn update_check_rejects_v1_manifest_from_v2_url_without_falling_back() {
    let v1_manifest_requests = Arc::new(AtomicUsize::new(0));
    let v1_signature_requests = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route(
            "/wokcore-update-v2.json",
            get(|| async { Response::new(Body::from(MIGRATION_V1)) }),
        )
        .route(
            "/wokcore-update-v2.json.minisig",
            get(|| async { Response::new(Body::from(MIGRATION_V1_SIGNATURE)) }),
        )
        .route(
            "/wokcore-update-v1.json",
            get({
                let requests = v1_manifest_requests.clone();
                move || {
                    let requests = requests.clone();
                    async move {
                        requests.fetch_add(1, Ordering::AcqRel);
                        Response::new(Body::from(MIGRATION_V1))
                    }
                }
            }),
        )
        .route(
            "/wokcore-update-v1.json.minisig",
            get({
                let requests = v1_signature_requests.clone();
                move || {
                    let requests = requests.clone();
                    async move {
                        requests.fetch_add(1, Ordering::AcqRel);
                        Response::new(Body::from(MIGRATION_V1_SIGNATURE))
                    }
                }
            }),
        );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let origin = Url::parse(&format!("http://{address}/")).unwrap();
    let (_directory, dependencies) = dependencies();
    let dependencies = dependencies
        .with_loopback_update_source(origin, MIGRATION_PUBLIC_KEY)
        .unwrap();
    let mut output = BufferOutput::default();

    let exit = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "update", "--check", "--json"]).unwrap(),
        &dependencies,
        &mut output,
    )
    .await;

    server.abort();
    assert_eq!(exit, ExitCode::InternalFailure);
    assert_eq!(v1_manifest_requests.load(Ordering::Acquire), 0);
    assert_eq!(v1_signature_requests.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn update_check_rejects_v2_manifest_from_v1_url() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route(
            "/wokcore-update-v2.json",
            get(|| async { Response::builder().status(404).body(Body::empty()).unwrap() }),
        )
        .route(
            "/wokcore-update-v1.json",
            get(|| async { Response::new(Body::from(MIGRATION_V2)) }),
        )
        .route(
            "/wokcore-update-v1.json.minisig",
            get(|| async { Response::new(Body::from(MIGRATION_V2_SIGNATURE)) }),
        );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let origin = Url::parse(&format!("http://{address}/")).unwrap();
    let (_directory, dependencies) = dependencies();
    let dependencies = dependencies
        .with_loopback_update_source(origin, MIGRATION_PUBLIC_KEY)
        .unwrap();
    let mut output = BufferOutput::default();

    let exit = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "update", "--check", "--json"]).unwrap(),
        &dependencies,
        &mut output,
    )
    .await;

    server.abort();
    assert_eq!(exit, ExitCode::InternalFailure);
}

#[tokio::test]
async fn update_check_does_not_fall_back_when_present_v2_has_an_invalid_signature() {
    let mut corrupted_signature = MIGRATION_V2_SIGNATURE.to_vec();
    let signature_payload = corrupted_signature
        .split_mut(|byte| *byte == b'\n')
        .nth(1)
        .unwrap();
    let index = signature_payload
        .iter()
        .skip(20)
        .position(|byte| byte.is_ascii_alphanumeric())
        .map(|index| index + 20)
        .unwrap();
    signature_payload[index] = if signature_payload[index] == b'A' {
        b'B'
    } else {
        b'A'
    };

    let v1_manifest_requests = Arc::new(AtomicUsize::new(0));
    let v1_signature_requests = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route(
            "/wokcore-update-v2.json",
            get(|| async { Response::new(Body::from(MIGRATION_V2)) }),
        )
        .route(
            "/wokcore-update-v2.json.minisig",
            get({
                let corrupted_signature = corrupted_signature.clone();
                move || {
                    let signature = corrupted_signature.clone();
                    async move { Response::new(Body::from(signature)) }
                }
            }),
        )
        .route(
            "/wokcore-update-v1.json",
            get({
                let requests = v1_manifest_requests.clone();
                move || {
                    let requests = requests.clone();
                    async move {
                        requests.fetch_add(1, Ordering::AcqRel);
                        Response::new(Body::from(MIGRATION_V1))
                    }
                }
            }),
        )
        .route(
            "/wokcore-update-v1.json.minisig",
            get({
                let requests = v1_signature_requests.clone();
                move || {
                    let requests = requests.clone();
                    async move {
                        requests.fetch_add(1, Ordering::AcqRel);
                        Response::new(Body::from(MIGRATION_V1_SIGNATURE))
                    }
                }
            }),
        );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let origin = Url::parse(&format!("http://{address}/")).unwrap();
    let (_directory, dependencies) = dependencies();
    let dependencies = dependencies
        .with_loopback_update_source(origin, MIGRATION_PUBLIC_KEY)
        .unwrap();
    let mut output = BufferOutput::default();

    let exit = run_with_dependencies(
        Cli::try_parse_from(["wokcore", "update", "--check", "--json"]).unwrap(),
        &dependencies,
        &mut output,
    )
    .await;

    server.abort();
    assert_eq!(exit, ExitCode::InternalFailure);
    assert_eq!(v1_manifest_requests.load(Ordering::Acquire), 0);
    assert_eq!(v1_signature_requests.load(Ordering::Acquire), 0);
}
