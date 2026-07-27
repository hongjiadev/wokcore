use std::io::Write;

use reqwest::{StatusCode, header};
use wokcore_platform::sessions::{PinnedExportDestination, SessionRootLease};

use crate::{
    CommandOutput, ExitCode, RunDependencies,
    cli::{Diagnostics, DiagnosticsCommand, DiagnosticsExport},
};

use super::client::{ControlClient, ControlClientError};

const MAX_EXPORT_BYTES: u64 = 64 * 1024 * 1024;

pub(super) async fn run(
    options: Diagnostics,
    dependencies: &RunDependencies,
    output: &mut dyn CommandOutput,
) -> ExitCode {
    match options.command {
        DiagnosticsCommand::Export(options) => export(options, dependencies, output).await,
    }
}

async fn export(
    options: DiagnosticsExport,
    dependencies: &RunDependencies,
    output: &mut dyn CommandOutput,
) -> ExitCode {
    match export_package(options, dependencies).await {
        Ok(()) => {
            if output
                .write_stdout("Diagnostic support package created.\n")
                .is_ok()
            {
                ExitCode::Success
            } else {
                ExitCode::InternalFailure
            }
        }
        Err(error) => render_error(error, output),
    }
}

async fn export_package(
    options: DiagnosticsExport,
    dependencies: &RunDependencies,
) -> Result<(), ControlClientError> {
    let roots = session_root_leases(dependencies)?;
    let root_refs = roots.iter().collect::<Vec<_>>();
    let mut destination = PinnedExportDestination::create(&options.output, &root_refs)
        .map_err(|_| ControlClientError::InvalidRuntime)?;
    let client = ControlClient::connect(dependencies).await?;
    let management = client.management_secret(dependencies).await?;
    let mut response = client
        .get("/wokcore/v1/diagnostics/export", &management)
        .await?;
    if response.status() == StatusCode::UNAUTHORIZED {
        return Err(ControlClientError::Authentication);
    }
    if !response.status().is_success()
        || response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            != Some("application/zip")
        || response
            .content_length()
            .is_some_and(|length| length > MAX_EXPORT_BYTES)
    {
        return Err(ControlClientError::Internal);
    }
    let mut written = 0_u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ControlClientError::Internal)?
    {
        written = written
            .checked_add(u64::try_from(chunk.len()).map_err(|_| ControlClientError::Internal)?)
            .ok_or(ControlClientError::Internal)?;
        if written > MAX_EXPORT_BYTES {
            return Err(ControlClientError::Internal);
        }
        destination
            .write_all(&chunk)
            .map_err(|_| ControlClientError::Internal)?;
    }
    if written == 0 {
        return Err(ControlClientError::Internal);
    }
    destination
        .sync_data()
        .and_then(|()| destination.commit())
        .map_err(|_| ControlClientError::Internal)
}

fn session_root_leases(
    dependencies: &RunDependencies,
) -> Result<Vec<SessionRootLease>, ControlClientError> {
    let Some(roots) = dependencies.session_roots.as_ref() else {
        return Ok(Vec::new());
    };
    [&roots.codex, &roots.claude, &roots.gemini]
        .into_iter()
        .filter(|path| path.exists())
        .map(|path| SessionRootLease::open(path).map_err(|_| ControlClientError::InvalidRuntime))
        .collect()
}

fn render_error(error: ControlClientError, output: &mut dyn CommandOutput) -> ExitCode {
    let (exit, human) = match error {
        ControlClientError::NotRunning => (ExitCode::NotRunning, "WokCore is not running.\n"),
        ControlClientError::InvalidRuntime | ControlClientError::IdentityMismatch => (
            ExitCode::InvalidInput,
            "Diagnostic export destination or runtime metadata is invalid.\n",
        ),
        ControlClientError::Authentication => (
            ExitCode::AuthenticationFailure,
            "WokCore management authentication failed.\n",
        ),
        ControlClientError::StorageCorruption => {
            (ExitCode::StorageCorruption, "WokCore storage is corrupt.\n")
        }
        ControlClientError::Internal => (
            ExitCode::InternalFailure,
            "WokCore diagnostic export failed.\n",
        ),
    };
    if output.write_stderr(human).is_ok() {
        exit
    } else {
        ExitCode::InternalFailure
    }
}
