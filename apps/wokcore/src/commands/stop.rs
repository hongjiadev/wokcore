use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;

use crate::{CommandOutput, ExitCode, RunDependencies, cli::JsonOutput};

use super::{
    client::{ControlClient, ControlClientError, response_body},
    write_json,
};

pub(super) async fn run(
    options: JsonOutput,
    dependencies: &RunDependencies,
    output: &mut dyn CommandOutput,
) -> ExitCode {
    match stop(dependencies).await {
        Ok(()) => {
            let rendered = if options.json {
                write_json(output, &json!({"code": "stopped"}))
            } else {
                output.write_stdout("WokCore local service stopped.\n")
            };
            if rendered.is_ok() {
                ExitCode::Success
            } else {
                ExitCode::InternalFailure
            }
        }
        Err(error) => render_error(error, output, options.json),
    }
}

async fn stop(dependencies: &RunDependencies) -> Result<(), ControlClientError> {
    let client = ControlClient::connect(dependencies).await?;
    let management = client.management_secret(dependencies).await?;
    let drain = client
        .post_json::<()>("/wokcore/v1/service/drain", &management, None)
        .await?;
    let drain_status = drain.status();
    let drain_body = response_body(drain).await?;
    if drain_status == StatusCode::UNAUTHORIZED {
        return Err(ControlClientError::Authentication);
    }
    if !drain_status.is_success() {
        return Err(ControlClientError::Internal);
    }
    let drained: LifecycleResponse =
        serde_json::from_slice(&drain_body).map_err(|_| ControlClientError::Internal)?;
    if drained.active_requests != 0 {
        return Err(ControlClientError::Internal);
    }

    let stop = client
        .post_json::<()>("/wokcore/v1/service/stop", &management, None)
        .await?;
    let stop_status = stop.status();
    let stop_body = response_body(stop).await?;
    if stop_status == StatusCode::UNAUTHORIZED {
        return Err(ControlClientError::Authentication);
    }
    if !stop_status.is_success() {
        return Err(ControlClientError::Internal);
    }
    let stopped: LifecycleResponse =
        serde_json::from_slice(&stop_body).map_err(|_| ControlClientError::Internal)?;
    if stopped.phase != "stopping" || stopped.active_requests != 0 {
        return Err(ControlClientError::Internal);
    }
    Ok(())
}

fn render_error(error: ControlClientError, output: &mut dyn CommandOutput, json: bool) -> ExitCode {
    let (exit, code, human) = match error {
        ControlClientError::NotRunning => (
            ExitCode::NotRunning,
            "not_running",
            "WokCore is not running.\n",
        ),
        ControlClientError::InvalidRuntime | ControlClientError::IdentityMismatch => (
            ExitCode::InvalidInput,
            "invalid_runtime",
            "WokCore runtime metadata is invalid.\n",
        ),
        ControlClientError::Authentication => (
            ExitCode::AuthenticationFailure,
            "authentication_failure",
            "WokCore management authentication failed.\n",
        ),
        ControlClientError::StorageCorruption => (
            ExitCode::StorageCorruption,
            "storage_corrupt",
            "WokCore storage is corrupt.\n",
        ),
        ControlClientError::Internal => (
            ExitCode::InternalFailure,
            "internal_error",
            "WokCore stop failed.\n",
        ),
    };
    let rendered = if json {
        write_json(output, &json!({"code": code}))
    } else {
        output.write_stderr(human)
    };
    if rendered.is_ok() {
        exit
    } else {
        ExitCode::InternalFailure
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LifecycleResponse {
    phase: String,
    active_requests: usize,
}
