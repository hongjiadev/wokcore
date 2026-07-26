use std::io;

use reqwest::StatusCode;
use secrecy::{ExposeSecret, SecretString, zeroize::Zeroizing};
use serde::{Deserialize, Serialize};
use serde_json::json;
use wokcore_core::id::ClientId;

use crate::{CommandOutput, ExitCode, RunDependencies, cli::Authorize};

use super::{
    client::{ControlClient, ControlClientError, response_body},
    write_json,
};

pub(super) async fn run(
    options: Authorize,
    dependencies: &RunDependencies,
    output: &mut dyn CommandOutput,
) -> ExitCode {
    let client_id = match ClientId::new(options.client) {
        Ok(client_id) => client_id,
        Err(_) => return render_error(ControlClientError::InvalidRuntime, output),
    };
    let result = authorize(client_id, dependencies).await;
    match result {
        Ok(authorized) => {
            let rendered = write_authorize_json(output, &authorized);
            if rendered.is_ok() {
                ExitCode::Success
            } else {
                ExitCode::InternalFailure
            }
        }
        Err(error) => render_error(error, output),
    }
}

async fn authorize(
    client_id: ClientId,
    dependencies: &RunDependencies,
) -> Result<AuthorizeResponse, ControlClientError> {
    let client = ControlClient::connect(dependencies).await?;
    let management = client.management_secret(dependencies).await?;
    let request = AuthorizeRequest {
        client_id: client_id.clone(),
    };
    let response = client
        .post_json("/wokcore/v1/clients/authorize", &management, Some(&request))
        .await?;
    let status = response.status();
    let body = response_body(response).await?;
    if status == StatusCode::UNAUTHORIZED {
        return Err(ControlClientError::Authentication);
    }
    if status != StatusCode::CREATED {
        return Err(ControlClientError::Internal);
    }
    let authorized: AuthorizeResponseWire =
        serde_json::from_slice(&body).map_err(|_| ControlClientError::Internal)?;
    let authorized = AuthorizeResponse {
        client_id: authorized.client_id,
        token_id: authorized.token_id,
        token: SecretString::from(authorized.token),
    };
    if authorized.client_id != client_id
        || authorized.token_id.is_empty()
        || !authorized
            .token
            .expose_secret()
            .starts_with("wok_proxy_v1_")
    {
        return Err(ControlClientError::Internal);
    }
    Ok(authorized)
}

fn write_authorize_json(
    output: &mut dyn CommandOutput,
    authorized: &AuthorizeResponse,
) -> io::Result<()> {
    let response = AuthorizeOutput {
        client_id: &authorized.client_id,
        token_id: &authorized.token_id,
        token: authorized.token.expose_secret(),
    };
    let mut rendered = Zeroizing::new(serde_json::to_string(&response).map_err(io::Error::other)?);
    rendered.push('\n');
    output.write_stdout(&rendered)
}

fn render_error(error: ControlClientError, output: &mut dyn CommandOutput) -> ExitCode {
    let (exit, code) = match error {
        ControlClientError::NotRunning => (ExitCode::NotRunning, "not_running"),
        ControlClientError::InvalidRuntime | ControlClientError::IdentityMismatch => {
            (ExitCode::InvalidInput, "invalid_runtime")
        }
        ControlClientError::Authentication => {
            (ExitCode::AuthenticationFailure, "authentication_failure")
        }
        ControlClientError::StorageCorruption => (ExitCode::StorageCorruption, "storage_corrupt"),
        ControlClientError::Internal => (ExitCode::InternalFailure, "internal_error"),
    };
    if write_json(output, &json!({"code": code})).is_ok() {
        exit
    } else {
        ExitCode::InternalFailure
    }
}

#[derive(Serialize)]
struct AuthorizeRequest {
    client_id: ClientId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorizeResponseWire {
    client_id: ClientId,
    token_id: String,
    token: String,
}

struct AuthorizeResponse {
    client_id: ClientId,
    token_id: String,
    token: SecretString,
}

#[derive(Serialize)]
struct AuthorizeOutput<'a> {
    client_id: &'a ClientId,
    token_id: &'a str,
    token: &'a str,
}
