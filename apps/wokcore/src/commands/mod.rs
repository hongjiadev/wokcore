mod authorize;
mod client;
mod doctor;
mod response;
mod serve;
mod status;
mod stop;

use std::io;

use serde_json::Value;

use crate::{CommandOutput, ExitCode, RunDependencies, cli::Command};

pub(crate) async fn run(
    command: Command,
    dependencies: &RunDependencies,
    output: &mut dyn CommandOutput,
) -> ExitCode {
    match command {
        Command::Serve(options) => serve::run(options, dependencies, output).await,
        Command::Status(options) => status::run(options, dependencies, output).await,
        Command::Doctor(options) => doctor::run(options, dependencies, output).await,
        Command::Stop(options) => stop::run(options, dependencies, output).await,
        Command::Authorize(options) => authorize::run(options, dependencies, output).await,
    }
}

pub(crate) fn write_json(output: &mut dyn CommandOutput, value: &Value) -> io::Result<()> {
    let mut rendered = serde_json::to_string(value).map_err(io::Error::other)?;
    rendered.push('\n');
    output.write_stdout(&rendered)
}

pub(crate) fn internal_failure(output: &mut dyn CommandOutput) -> ExitCode {
    let _ = output.write_stderr("WokCore command failed.\n");
    ExitCode::InternalFailure
}
