use std::{
    future::Future,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use secrecy::{SecretString, zeroize::Zeroizing};
use url::Url;
use uuid::Uuid;
use wokcore_platform::{AppPaths, is_process_running, process_matches_executable};
use wokcore_server::{
    auth::{EntropySource, OsEntropy},
    observability::SessionRootPaths,
    runtime::{generate_uuid_v4, utc_timestamp_from_epoch_seconds},
};
use wokcore_storage::NativeSecretStore;

use crate::{
    Clock, CommandOutput, ExitCode, IdSource, PRODUCTION_UPDATE_ORIGIN, ProcessIdentity,
    RunDependencies, RuntimeValueError, SecretInput, ShutdownSignal, UpdateChild, UpdateProcess,
    UpdateSource,
    cli::{Cli, Command},
    run_with_dependencies,
    runtime::production_upstream_executor,
};

pub async fn run_production(cli: Cli) -> ExitCode {
    let mut output = StandardOutput;
    let paths = match AppPaths::discover() {
        Ok(paths) => paths,
        Err(_) => {
            if requests_json(&cli) {
                let _ = output.write_stdout("{\"code\":\"invalid_runtime\"}\n");
            } else {
                let _ = output.write_stderr("WokCore application paths are unavailable.\n");
            }
            return ExitCode::InvalidInput;
        }
    };
    let entropy: Arc<dyn EntropySource> = Arc::new(OsEntropy);
    let secrets = Arc::new(NativeSecretStore::new());
    let update_source = match production_update_source() {
        Ok(source) => source,
        Err(_) => {
            if requests_json(&cli) {
                let _ = output.write_stdout("{\"code\":\"internal_error\"}\n");
            } else {
                let _ = output.write_stderr("WokCore update runtime is unavailable.\n");
            }
            return ExitCode::InternalFailure;
        }
    };
    let upstream_executor = if matches!(&cli.command, Command::Serve(_)) {
        match production_upstream_executor(secrets.clone()) {
            Ok(executor) => Some(executor),
            Err(_) => {
                if requests_json(&cli) {
                    let _ = output.write_stdout("{\"code\":\"invalid_runtime\"}\n");
                } else {
                    let _ = output.write_stderr("WokCore upstream runtime is unavailable.\n");
                }
                return ExitCode::InternalFailure;
            }
        }
    } else {
        None
    };
    let mut dependencies = RunDependencies::new(
        paths,
        secrets,
        entropy.clone(),
        Arc::new(SystemClock),
        Arc::new(SystemIds::new(entropy)),
        Arc::new(SystemProcessIdentity),
        Arc::new(ControlCSignal),
    )
    .with_secret_input(Arc::new(StandardSecretInput))
    .with_update_process(Arc::new(SystemUpdateProcess));
    dependencies.update_source = Some(update_source);
    if let Some(upstream_executor) = upstream_executor {
        dependencies = dependencies.with_upstream_executor(upstream_executor);
    }
    if let Some(session_roots) = SessionRootPaths::discover() {
        dependencies = dependencies.with_session_roots(session_roots);
    }
    run_with_dependencies(cli, &dependencies, &mut output).await
}

fn production_update_source() -> Result<UpdateSource, RuntimeValueError> {
    let origin = Url::parse(PRODUCTION_UPDATE_ORIGIN).map_err(|_| RuntimeValueError)?;
    Ok(UpdateSource {
        origin,
        public_key: Arc::from(include_str!("../../../release/minisign.pub")),
    })
}

fn requests_json(cli: &Cli) -> bool {
    match &cli.command {
        Command::Serve(options)
        | Command::Status(options)
        | Command::Stop(options)
        | Command::Doctor(options) => options.json,
        Command::Authorize(options) => options.json,
        Command::Sessions(options) => match &options.command {
            crate::cli::SessionsCommand::List(options) => options.json,
            crate::cli::SessionsCommand::Show(options) => options.json,
        },
        Command::Logs(options) => options.jsonl,
        Command::Diagnostics(_) => false,
        Command::Providers(_) => true,
        Command::Update(options) => options.json,
    }
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Result<String, RuntimeValueError> {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| RuntimeValueError)?
            .as_secs();
        utc_timestamp_from_epoch_seconds(seconds).ok_or(RuntimeValueError)
    }
}

struct SystemIds {
    entropy: Arc<dyn EntropySource>,
}

impl SystemIds {
    fn new(entropy: Arc<dyn EntropySource>) -> Self {
        Self { entropy }
    }
}

impl IdSource for SystemIds {
    fn new_instance_id(&self) -> Result<Uuid, RuntimeValueError> {
        generate_uuid_v4(self.entropy.as_ref()).map_err(|_| RuntimeValueError)
    }

    fn new_token_id(&self) -> Result<String, RuntimeValueError> {
        generate_uuid_v4(self.entropy.as_ref())
            .map(|uuid| uuid.to_string())
            .map_err(|_| RuntimeValueError)
    }
}

struct SystemProcessIdentity;

impl ProcessIdentity for SystemProcessIdentity {
    fn current_pid(&self) -> u32 {
        std::process::id()
    }

    fn is_running(&self, pid: u32) -> bool {
        is_process_running(pid)
    }

    fn matches_executable(&self, pid: u32, expected: &Path) -> bool {
        process_matches_executable(pid, expected)
    }
}

struct SystemUpdateProcess;

#[async_trait]
impl UpdateProcess for SystemUpdateProcess {
    fn current_executable(&self) -> Result<PathBuf, RuntimeValueError> {
        std::env::current_exe().map_err(|_| RuntimeValueError)
    }

    async fn spawn_service(
        &self,
        executable: &Path,
    ) -> Result<Box<dyn UpdateChild>, RuntimeValueError> {
        let child = tokio::process::Command::new(executable)
            .args(["serve", "--json"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(false)
            .spawn()
            .map_err(|_| RuntimeValueError)?;
        Ok(Box::new(SystemUpdateChild {
            child,
            detached: false,
        }))
    }
}

struct SystemUpdateChild {
    child: tokio::process::Child,
    detached: bool,
}

#[async_trait]
impl UpdateChild for SystemUpdateChild {
    fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    async fn kill(&mut self) -> Result<(), RuntimeValueError> {
        self.child.kill().await.map_err(|_| RuntimeValueError)
    }

    fn detach(&mut self) {
        self.detached = true;
    }
}

impl Drop for SystemUpdateChild {
    fn drop(&mut self) {
        if !self.detached {
            let _ = self.child.start_kill();
        }
    }
}

struct ControlCSignal;

impl ShutdownSignal for ControlCSignal {
    fn wait(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {
            let _ = tokio::signal::ctrl_c().await;
        })
    }
}

struct StandardOutput;

impl CommandOutput for StandardOutput {
    fn write_stdout(&mut self, value: &str) -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        stdout.write_all(value.as_bytes())?;
        stdout.flush()
    }

    fn write_stderr(&mut self, value: &str) -> io::Result<()> {
        let mut stderr = io::stderr().lock();
        stderr.write_all(value.as_bytes())?;
        stderr.flush()
    }
}

struct StandardSecretInput;

impl SecretInput for StandardSecretInput {
    fn read_secret(&self, maximum_bytes: usize) -> io::Result<SecretString> {
        read_bounded_secret(io::stdin().lock(), maximum_bytes)
    }
}

fn read_bounded_secret(input: impl Read, maximum_bytes: usize) -> io::Result<SecretString> {
    let limit = maximum_bytes
        .checked_add(1)
        .ok_or_else(|| io::Error::other("secret input limit is invalid"))?;
    let mut bytes = Zeroizing::new(Vec::with_capacity(limit));
    input.take(limit as u64).read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() > maximum_bytes {
        return Err(io::Error::other("secret input size is invalid"));
    }
    let value =
        std::str::from_utf8(&bytes).map_err(|_| io::Error::other("secret input is not UTF-8"))?;
    Ok(SecretString::from(value.to_owned()))
}

#[cfg(test)]
mod tests {
    use std::{io::Cursor, sync::Arc};

    use secrecy::ExposeSecret;
    use wokcore_server::auth::{EntropySource, TokenError};

    use super::{IdSource, SystemIds, production_update_source, read_bounded_secret};

    struct FailingEntropy;

    impl EntropySource for FailingEntropy {
        fn fill(&self, _output: &mut [u8; 32]) -> Result<(), TokenError> {
            Err(TokenError::EntropyUnavailable)
        }
    }

    #[test]
    fn production_update_source_uses_the_exact_trusted_release_origin_and_key() {
        const EXPECTED_ORIGIN: &str =
            "https://github.com/hongjiadev/wokcore/releases/latest/download/";
        const EXPECTED_PUBLIC_KEY: &str = "untrusted comment: minisign public key 7EF262CD8E9FE136\nRWQ24Z+OzWLyfjz0X7JFepiizNYEsUBt/cJisQWQ9o9EAK8TURVs9hts\n";
        let source = production_update_source().unwrap();

        assert_eq!(source.origin.as_str(), EXPECTED_ORIGIN);
        assert_eq!(source.public_key.as_ref(), EXPECTED_PUBLIC_KEY);
        assert_eq!(
            source.public_key.lines().next(),
            Some("untrusted comment: minisign public key 7EF262CD8E9FE136")
        );
    }

    #[test]
    fn production_ids_map_entropy_failure_without_panicking() {
        let ids = SystemIds::new(Arc::new(FailingEntropy));

        assert!(ids.new_instance_id().is_err());
        assert!(ids.new_token_id().is_err());
    }

    #[test]
    fn bounded_secret_input_accepts_only_nonempty_utf8_within_the_limit() {
        let accepted = read_bounded_secret(Cursor::new(b"exact-value"), 11).unwrap();
        assert_eq!(accepted.expose_secret(), "exact-value");

        assert!(read_bounded_secret(Cursor::new(Vec::<u8>::new()), 11).is_err());
        assert!(read_bounded_secret(Cursor::new(vec![0xff]), 11).is_err());
        assert!(read_bounded_secret(Cursor::new(b"one-byte-too-long"), 16).is_err());
    }
}
