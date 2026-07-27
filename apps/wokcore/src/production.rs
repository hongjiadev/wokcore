use std::{
    future::Future,
    io::{self, Write},
    pin::Pin,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use uuid::Uuid;
use wokcore_platform::{AppPaths, is_process_running};
use wokcore_server::{
    auth::{EntropySource, OsEntropy},
    observability::SessionRootPaths,
    runtime::{generate_uuid_v4, utc_timestamp_from_epoch_seconds},
};
use wokcore_storage::NativeSecretStore;

use crate::{
    Clock, CommandOutput, ExitCode, IdSource, ProcessIdentity, RunDependencies, RuntimeValueError,
    ShutdownSignal,
    cli::{Cli, Command},
    run_with_dependencies,
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
    let mut dependencies = RunDependencies::new(
        paths,
        Arc::new(NativeSecretStore::new()),
        entropy.clone(),
        Arc::new(SystemClock),
        Arc::new(SystemIds::new(entropy)),
        Arc::new(SystemProcessIdentity),
        Arc::new(ControlCSignal),
    );
    if let Some(session_roots) = SessionRootPaths::discover() {
        dependencies = dependencies.with_session_roots(session_roots);
    }
    run_with_dependencies(cli, &dependencies, &mut output).await
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use wokcore_server::auth::{EntropySource, TokenError};

    use super::{IdSource, SystemIds};

    struct FailingEntropy;

    impl EntropySource for FailingEntropy {
        fn fill(&self, _output: &mut [u8; 32]) -> Result<(), TokenError> {
            Err(TokenError::EntropyUnavailable)
        }
    }

    #[test]
    fn production_ids_map_entropy_failure_without_panicking() {
        let ids = SystemIds::new(Arc::new(FailingEntropy));

        assert!(ids.new_instance_id().is_err());
        assert!(ids.new_token_id().is_err());
    }
}
