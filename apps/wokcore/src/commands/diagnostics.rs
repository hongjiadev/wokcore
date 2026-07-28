use std::io::Write;

use reqwest::{StatusCode, header};
use wokcore_platform::sessions::{PinnedExportDestination, SessionRootLease};

use crate::{
    CommandOutput, ExitCode, RunDependencies,
    cli::{Diagnostics, DiagnosticsCommand, DiagnosticsExport},
};

use super::client::{ControlClient, ControlClientError};

const MAX_EXPORT_BYTES: u64 = 64 * 1024 * 1024;

enum DiagnosticExportError {
    Control(ControlClientError),
    Stage(DiagnosticExportStage),
}

#[derive(Clone, Copy)]
enum DiagnosticExportStage {
    ResponseMetadata,
    ResponseBody,
    DestinationWrite,
    DestinationSync,
    Commit,
}

impl DiagnosticExportStage {
    const fn event_code(self) -> &'static str {
        match self {
            Self::ResponseMetadata => "diagnostics_export_response_metadata_invalid",
            Self::ResponseBody => "diagnostics_export_response_body_failed",
            Self::DestinationWrite => "diagnostics_export_destination_write_failed",
            Self::DestinationSync => "diagnostics_export_destination_sync_failed",
            Self::Commit => "diagnostics_export_commit_failed",
        }
    }
}

impl From<ControlClientError> for DiagnosticExportError {
    fn from(error: ControlClientError) -> Self {
        Self::Control(error)
    }
}

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
) -> Result<(), DiagnosticExportError> {
    let roots = session_root_leases(dependencies)?;
    let root_refs = roots.iter().collect::<Vec<_>>();
    let mut destination = PinnedExportDestination::create(&options.output, &root_refs)
        .map_err(|_| DiagnosticExportError::Control(ControlClientError::InvalidRuntime))?;
    let client = ControlClient::connect(dependencies).await?;
    let management = client.management_secret(dependencies).await?;
    let mut response = client
        .get("/wokcore/v1/diagnostics/export", &management)
        .await?;
    if response.status() == StatusCode::UNAUTHORIZED {
        return Err(DiagnosticExportError::Control(
            ControlClientError::Authentication,
        ));
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
        return Err(DiagnosticExportError::Stage(
            DiagnosticExportStage::ResponseMetadata,
        ));
    }
    let mut written = 0_u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| DiagnosticExportError::Stage(DiagnosticExportStage::ResponseBody))?
    {
        written =
            written
                .checked_add(u64::try_from(chunk.len()).map_err(|_| {
                    DiagnosticExportError::Stage(DiagnosticExportStage::ResponseBody)
                })?)
                .ok_or(DiagnosticExportError::Stage(
                    DiagnosticExportStage::ResponseBody,
                ))?;
        if written > MAX_EXPORT_BYTES {
            return Err(DiagnosticExportError::Stage(
                DiagnosticExportStage::ResponseBody,
            ));
        }
        destination
            .write_all(&chunk)
            .map_err(|_| DiagnosticExportError::Stage(DiagnosticExportStage::DestinationWrite))?;
    }
    if written == 0 {
        return Err(DiagnosticExportError::Stage(
            DiagnosticExportStage::ResponseBody,
        ));
    }
    destination
        .sync_data()
        .map_err(|_| DiagnosticExportError::Stage(DiagnosticExportStage::DestinationSync))?;
    destination
        .commit()
        .map_err(|_| DiagnosticExportError::Stage(DiagnosticExportStage::Commit))
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

fn render_error(error: DiagnosticExportError, output: &mut dyn CommandOutput) -> ExitCode {
    let error = match error {
        DiagnosticExportError::Control(error) => error,
        DiagnosticExportError::Stage(stage) => {
            if output
                .write_stderr(&format!(
                    "wokcore diagnostics event_code={}\n",
                    stage.event_code()
                ))
                .is_err()
            {
                return ExitCode::InternalFailure;
            }
            ControlClientError::Internal
        }
    };
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
        ControlClientError::InvalidInput => (
            ExitCode::InvalidInput,
            "Diagnostic export input is invalid.\n",
        ),
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

#[cfg(test)]
mod tests {
    use crate::{BufferOutput, ExitCode};

    use super::{ControlClientError, DiagnosticExportError, DiagnosticExportStage, render_error};

    #[test]
    fn internal_export_failures_emit_only_a_stable_stage_code() {
        let mut output = BufferOutput::default();
        let secret_canary = ["private", "export", "path"].join("-");

        let exit = render_error(
            DiagnosticExportError::Stage(DiagnosticExportStage::Commit),
            &mut output,
        );

        assert_eq!(exit, ExitCode::InternalFailure);
        assert_eq!(
            output.stderr(),
            concat!(
                "wokcore diagnostics event_code=diagnostics_export_commit_failed\n",
                "WokCore diagnostic export failed.\n",
            )
        );
        assert!(!output.stderr().contains(&secret_canary));
    }

    #[test]
    fn expected_control_failures_do_not_claim_an_internal_export_stage() {
        let mut output = BufferOutput::default();

        let exit = render_error(
            DiagnosticExportError::Control(ControlClientError::InvalidRuntime),
            &mut output,
        );

        assert_eq!(exit, ExitCode::InvalidInput);
        assert_eq!(
            output.stderr(),
            "Diagnostic export destination or runtime metadata is invalid.\n"
        );
    }
}
