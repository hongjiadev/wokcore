use std::{
    future::Future,
    io::{self, Write},
    pin::Pin,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use uuid::Uuid;
use wokcore_platform::{AppPaths, is_process_running};
use wokcore_server::auth::OsEntropy;
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
    let dependencies = RunDependencies::new(
        paths,
        Arc::new(NativeSecretStore::new()),
        Arc::new(OsEntropy),
        Arc::new(SystemClock),
        Arc::new(SystemIds),
        Arc::new(SystemProcessIdentity),
        Arc::new(ControlCSignal),
    );
    run_with_dependencies(cli, &dependencies, &mut output).await
}

fn requests_json(cli: &Cli) -> bool {
    match &cli.command {
        Command::Serve(options)
        | Command::Status(options)
        | Command::Stop(options)
        | Command::Doctor(options) => options.json,
        Command::Authorize(options) => options.json,
    }
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Result<String, RuntimeValueError> {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| RuntimeValueError)?
            .as_secs();
        Ok(seconds.to_string())
    }
}

struct SystemIds;

impl IdSource for SystemIds {
    fn new_instance_id(&self) -> Result<Uuid, RuntimeValueError> {
        Ok(Uuid::new_v4())
    }

    fn new_token_id(&self) -> Result<String, RuntimeValueError> {
        Ok(Uuid::new_v4().to_string())
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
