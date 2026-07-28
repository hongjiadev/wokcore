use std::time::Duration;

use reqwest::StatusCode;
use secrecy::SecretString;
use serde::Deserialize;
use serde_json::json;
use tokio::{task::JoinError, time::timeout};

use crate::{CommandOutput, ExitCode, RunDependencies, cli::JsonOutput};

use super::{
    client::{ControlClient, ControlClientError, response_body},
    write_json,
};

const CANCEL_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

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
    map_owned_stop(tokio::spawn(stop_owned(client, management)).await)
}

async fn stop_owned(
    client: ControlClient,
    management: SecretString,
) -> Result<(), ControlClientError> {
    let result = drain_and_stop(&client, &management).await;
    if result.is_err() {
        best_effort_cancel_drain(&client, &management).await;
    }
    result
}

pub(super) async fn drain_and_stop(
    client: &ControlClient,
    management: &SecretString,
) -> Result<(), ControlClientError> {
    let drained = request_drain(client, management).await?;
    if drained.phase != "draining" || drained.active_requests != 0 {
        return Err(ControlClientError::Internal);
    }

    let stopped = request_stop(client, management).await?;
    if stopped.phase != "stopping" || stopped.active_requests != 0 {
        return Err(ControlClientError::Internal);
    }
    Ok(())
}

pub(super) async fn request_drain(
    client: &ControlClient,
    management: &SecretString,
) -> Result<LifecycleResponse, ControlClientError> {
    let drain = client
        .post_json::<()>("/wokcore/v1/service/drain", management, None)
        .await?;
    let drain_status = drain.status();
    let drain_body = response_body(drain).await?;
    if drain_status == StatusCode::UNAUTHORIZED {
        return Err(ControlClientError::Authentication);
    }
    if !drain_status.is_success() {
        return Err(ControlClientError::Internal);
    }
    serde_json::from_slice(&drain_body).map_err(|_| ControlClientError::Internal)
}

pub(super) async fn request_stop(
    client: &ControlClient,
    management: &SecretString,
) -> Result<LifecycleResponse, ControlClientError> {
    let stop = client
        .post_json::<()>("/wokcore/v1/service/stop", management, None)
        .await?;
    let stop_status = stop.status();
    let stop_body = response_body(stop).await?;
    if stop_status == StatusCode::UNAUTHORIZED {
        return Err(ControlClientError::Authentication);
    }
    if !stop_status.is_success() {
        return Err(ControlClientError::Internal);
    }
    serde_json::from_slice(&stop_body).map_err(|_| ControlClientError::Internal)
}

pub(super) async fn best_effort_cancel_drain(client: &ControlClient, management: &SecretString) {
    let _ = request_cancel_drain(client, management).await;
}

pub(super) async fn request_cancel_drain(
    client: &ControlClient,
    management: &SecretString,
) -> Result<LifecycleResponse, ControlClientError> {
    timeout(CANCEL_DRAIN_TIMEOUT, async {
        let response = client
            .post_json::<()>("/wokcore/v1/service/drain/cancel", management, None)
            .await?;
        let status = response.status();
        let body = response_body(response).await?;
        if !status.is_success() {
            return Err(ControlClientError::Internal);
        }
        serde_json::from_slice(&body).map_err(|_| ControlClientError::Internal)
    })
    .await
    .map_err(|_| ControlClientError::Internal)?
}

fn map_owned_stop(
    result: Result<Result<(), ControlClientError>, JoinError>,
) -> Result<(), ControlClientError> {
    result.unwrap_or(Err(ControlClientError::Internal))
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
        ControlClientError::InvalidInput => (
            ExitCode::InvalidInput,
            "invalid_input",
            "WokCore stop input is invalid.\n",
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
pub(super) struct LifecycleResponse {
    pub(super) phase: String,
    pub(super) active_requests: usize,
}

#[cfg(test)]
mod tests {
    use super::{ControlClientError, map_owned_stop};

    #[tokio::test]
    async fn owned_stop_join_failure_maps_to_internal() {
        let joined = tokio::spawn(async {
            panic!("simulated owned stop failure");
            #[allow(unreachable_code)]
            Ok::<(), ControlClientError>(())
        })
        .await;

        assert_eq!(map_owned_stop(joined), Err(ControlClientError::Internal));
    }
}
