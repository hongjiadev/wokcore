use std::{
    fs::File,
    io::{self, Read},
};

use reqwest::{Response, StatusCode};
use secrecy::ExposeSecret;
use serde::Serialize;
use serde_json::{Value, json};
use wokcore_core::secret::SecretRef;
use wokcore_server::providers::ProviderCandidate;

use crate::{
    CommandOutput, ExitCode, RunDependencies,
    cli::{
        ProviderCandidateFile, ProviderCommitFile, ProviderSecretCreate, ProviderSecretDelete,
        ProviderSecretReplace, ProviderSecrets, ProviderSecretsCommand, Providers,
        ProvidersCommand,
    },
};

use super::{
    client::{ControlClient, ControlClientError, response_body},
    write_json,
};

const MAX_CANDIDATE_FILE_BYTES: usize = 16 * 1024;
const MAX_SECRET_INPUT_BYTES: usize = 8 * 1024;

pub(super) async fn run(
    options: Providers,
    dependencies: &RunDependencies,
    output: &mut dyn CommandOutput,
) -> ExitCode {
    let result = match options.command {
        ProvidersCommand::Catalog(_) => get("/wokcore/v1/providers/catalog", dependencies).await,
        ProvidersCommand::Status(_) => get("/wokcore/v1/providers/runtime", dependencies).await,
        ProvidersCommand::Models(_) => get("/wokcore/v1/providers/models", dependencies).await,
        ProvidersCommand::Validate(options) => validate(options, dependencies).await,
        ProvidersCommand::Commit(options) => commit(options, dependencies).await,
        ProvidersCommand::Reload(_) => {
            post_empty("/wokcore/v1/providers/reload", dependencies).await
        }
        ProvidersCommand::Secret(options) => secrets(options, dependencies).await,
    };
    match result {
        Ok(response) => render_response(response, output).await,
        Err(error) => render_client_error(error, output),
    }
}

async fn get(path: &str, dependencies: &RunDependencies) -> Result<Response, ControlClientError> {
    let (client, management) = connect(dependencies).await?;
    client.get(path, &management).await
}

async fn post_empty(
    path: &str,
    dependencies: &RunDependencies,
) -> Result<Response, ControlClientError> {
    let (client, management) = connect(dependencies).await?;
    client.post_json::<Value>(path, &management, None).await
}

async fn validate(
    options: ProviderCandidateFile,
    dependencies: &RunDependencies,
) -> Result<Response, ControlClientError> {
    let candidate = read_candidate(&options.file).map_err(|_| ControlClientError::InvalidInput)?;
    let (client, management) = connect(dependencies).await?;
    client
        .post_json(
            "/wokcore/v1/providers/config/validate",
            &management,
            Some(&candidate),
        )
        .await
}

async fn commit(
    options: ProviderCommitFile,
    dependencies: &RunDependencies,
) -> Result<Response, ControlClientError> {
    let candidate = read_candidate(&options.file).map_err(|_| ControlClientError::InvalidInput)?;
    let request = CommitRequest {
        expected_revision: options.expected_revision,
        providers: &candidate.providers,
        routing: &candidate.routing,
    };
    let (client, management) = connect(dependencies).await?;
    client
        .put_json("/wokcore/v1/providers/config", &management, &request)
        .await
}

async fn secrets(
    options: ProviderSecrets,
    dependencies: &RunDependencies,
) -> Result<Response, ControlClientError> {
    match options.command {
        ProviderSecretsCommand::Create(options) => create_secret(options, dependencies).await,
        ProviderSecretsCommand::Replace(options) => replace_secret(options, dependencies).await,
        ProviderSecretsCommand::Delete(options) => delete_secret(options, dependencies).await,
    }
}

async fn create_secret(
    options: ProviderSecretCreate,
    dependencies: &RunDependencies,
) -> Result<Response, ControlClientError> {
    let secret = dependencies
        .secret_input
        .read_secret(MAX_SECRET_INPUT_BYTES)
        .map_err(|_| ControlClientError::InvalidInput)?;
    let request = CreateSecretRequest {
        provider_id: &options.provider,
        account_id: options.account.as_deref(),
        purpose: &options.purpose,
        secret: secret.expose_secret(),
    };
    let (client, management) = connect(dependencies).await?;
    client
        .post_json("/wokcore/v1/provider-secrets", &management, Some(&request))
        .await
}

async fn replace_secret(
    options: ProviderSecretReplace,
    dependencies: &RunDependencies,
) -> Result<Response, ControlClientError> {
    let secret_ref =
        SecretRef::parse(options.secret_ref).map_err(|_| ControlClientError::InvalidInput)?;
    let secret = dependencies
        .secret_input
        .read_secret(MAX_SECRET_INPUT_BYTES)
        .map_err(|_| ControlClientError::InvalidInput)?;
    let request = ReplaceSecretRequest {
        secret: secret.expose_secret(),
    };
    let (client, management) = connect(dependencies).await?;
    client
        .put_json(
            &format!("/wokcore/v1/provider-secrets/{}", secret_ref.as_str()),
            &management,
            &request,
        )
        .await
}

async fn delete_secret(
    options: ProviderSecretDelete,
    dependencies: &RunDependencies,
) -> Result<Response, ControlClientError> {
    let secret_ref =
        SecretRef::parse(options.secret_ref).map_err(|_| ControlClientError::InvalidInput)?;
    let (client, management) = connect(dependencies).await?;
    client
        .delete(
            &format!("/wokcore/v1/provider-secrets/{}", secret_ref.as_str()),
            &management,
        )
        .await
}

async fn connect(
    dependencies: &RunDependencies,
) -> Result<(ControlClient, secrecy::SecretString), ControlClientError> {
    let client = ControlClient::connect(dependencies).await?;
    let management = client.management_secret(dependencies).await?;
    Ok((client, management))
}

fn read_candidate(path: &std::path::Path) -> Result<ProviderCandidate, io::Error> {
    let file = File::open(path)?;
    let mut bytes = Vec::with_capacity(MAX_CANDIDATE_FILE_BYTES);
    file.take((MAX_CANDIDATE_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() > MAX_CANDIDATE_FILE_BYTES {
        return Err(io::Error::other("Provider candidate file size is invalid"));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| io::Error::other("Provider candidate file is invalid"))
}

async fn render_response(response: Response, output: &mut dyn CommandOutput) -> ExitCode {
    let status = response.status();
    let body = match response_body(response).await {
        Ok(body) => body,
        Err(error) => return render_client_error(error, output),
    };
    let value = match serde_json::from_slice::<Value>(&body) {
        Ok(value) => value,
        Err(_) => return render_client_error(ControlClientError::Internal, output),
    };
    if write_json(output, &value).is_err() {
        return ExitCode::InternalFailure;
    }
    exit_for_status(status)
}

fn exit_for_status(status: StatusCode) -> ExitCode {
    match status {
        status if status.is_success() => ExitCode::Success,
        StatusCode::BAD_REQUEST
        | StatusCode::NOT_FOUND
        | StatusCode::CONFLICT
        | StatusCode::PAYLOAD_TOO_LARGE
        | StatusCode::UNSUPPORTED_MEDIA_TYPE => ExitCode::InvalidInput,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ExitCode::AuthenticationFailure,
        _ => ExitCode::InternalFailure,
    }
}

fn render_client_error(error: ControlClientError, output: &mut dyn CommandOutput) -> ExitCode {
    let (exit, code) = match error {
        ControlClientError::NotRunning => (ExitCode::NotRunning, "not_running"),
        ControlClientError::InvalidRuntime | ControlClientError::IdentityMismatch => {
            (ExitCode::InvalidInput, "invalid_runtime")
        }
        ControlClientError::Authentication => {
            (ExitCode::AuthenticationFailure, "authentication_failure")
        }
        ControlClientError::StorageCorruption => (ExitCode::StorageCorruption, "storage_corrupt"),
        ControlClientError::InvalidInput => (ExitCode::InvalidInput, "invalid_input"),
        ControlClientError::Internal => (ExitCode::InternalFailure, "internal_error"),
    };
    if write_json(output, &json!({"code": code})).is_ok() {
        exit
    } else {
        ExitCode::InternalFailure
    }
}

#[derive(Serialize)]
struct CommitRequest<'a> {
    expected_revision: u64,
    providers: &'a wokcore_core::config::ProviderConfig,
    routing: &'a wokcore_core::config::RoutingConfig,
}

#[derive(Serialize)]
struct CreateSecretRequest<'a> {
    provider_id: &'a str,
    account_id: Option<&'a str>,
    purpose: &'a str,
    secret: &'a str,
}

#[derive(Serialize)]
struct ReplaceSecretRequest<'a> {
    secret: &'a str,
}
