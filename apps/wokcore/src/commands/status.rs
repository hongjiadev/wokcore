use std::io;

use reqwest::header::HOST;
use serde::Deserialize;
use serde_json::json;
use url::Url;
use wokcore_platform::{DiscoveryRecord, DiscoveryStore, PlatformError};

use crate::{CommandOutput, ExitCode, RunDependencies, cli::JsonOutput};

use super::{internal_failure, write_json};

const MAX_IDENTITY_BODY_BYTES: usize = 64 * 1024;

pub(super) async fn run(
    options: JsonOutput,
    dependencies: &RunDependencies,
    output: &mut dyn CommandOutput,
) -> ExitCode {
    let discovery = match read_discovery(dependencies) {
        Ok(record) => record,
        Err(ReadDiscoveryError::Absent) => {
            let result = if options.json {
                write_json(output, &json!({"code": "not_running"}))
            } else {
                output.write_stdout("WokCore is not running.\n")
            };
            return if result.is_ok() {
                ExitCode::NotRunning
            } else {
                ExitCode::InternalFailure
            };
        }
        Err(ReadDiscoveryError::Invalid) => {
            return invalid_runtime(output, options.json);
        }
        Err(ReadDiscoveryError::Internal) => return internal_failure(output),
    };
    if !dependencies.process.is_running(discovery.pid) {
        return not_running(output, options.json);
    }
    if verify_identity(&discovery).await.is_err() {
        return not_running(output, options.json);
    }

    let result = if options.json {
        write_json(
            output,
            &json!({
                "api_major": discovery.api_major,
                "code": "running",
                "instance_id": discovery.instance_id,
                "pid": discovery.pid,
                "wokcore_version": discovery.wokcore_version,
            }),
        )
    } else {
        output.write_stdout("WokCore is running.\n")
    };
    if result.is_ok() {
        ExitCode::Success
    } else {
        ExitCode::InternalFailure
    }
}

pub(crate) async fn verify_identity(record: &DiscoveryRecord) -> Result<(), IdentityError> {
    let authority = validated_authority(&record.base_url)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|_| IdentityError::Internal)?;
    let health: HealthResponse =
        get_json(&client, &record.base_url, &authority, "/wokcore/v1/health").await?;
    if health.status != "ok" || health.instance_id != record.instance_id {
        return Err(IdentityError::InstanceMismatch);
    }
    let capabilities: CapabilitiesResponse = get_json(
        &client,
        &record.base_url,
        &authority,
        "/wokcore/v1/capabilities",
    )
    .await?;
    if capabilities.instance_id != record.instance_id {
        return Err(IdentityError::InstanceMismatch);
    }
    if u32::from(capabilities.management_api_major) != record.api_major
        || capabilities.wokcore_version != record.wokcore_version
    {
        return Err(IdentityError::ApiMismatch);
    }
    Ok(())
}

pub(crate) fn validated_authority(base_url: &str) -> Result<String, IdentityError> {
    let url = Url::parse(base_url).map_err(|_| IdentityError::InvalidAuthority)?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none_or(|port| port == 0)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(IdentityError::InvalidAuthority);
    }
    Ok(format!(
        "127.0.0.1:{}",
        url.port().ok_or(IdentityError::InvalidAuthority)?
    ))
}

async fn get_json<T: for<'de> Deserialize<'de>>(
    client: &reqwest::Client,
    base_url: &str,
    authority: &str,
    path: &str,
) -> Result<T, IdentityError> {
    let response = client
        .get(format!("{base_url}{path}"))
        .header(HOST, authority)
        .send()
        .await
        .map_err(|_| IdentityError::Unreachable)?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length > MAX_IDENTITY_BODY_BYTES as u64)
    {
        return Err(IdentityError::InvalidResponse);
    }
    let body = response
        .bytes()
        .await
        .map_err(|_| IdentityError::InvalidResponse)?;
    if body.len() > MAX_IDENTITY_BODY_BYTES {
        return Err(IdentityError::InvalidResponse);
    }
    serde_json::from_slice(&body).map_err(|_| IdentityError::InvalidResponse)
}

fn read_discovery(dependencies: &RunDependencies) -> Result<DiscoveryRecord, ReadDiscoveryError> {
    let store = DiscoveryStore::new(&dependencies.paths).map_err(map_discovery_error)?;
    store.read().map_err(map_discovery_error)
}

fn map_discovery_error(error: PlatformError) -> ReadDiscoveryError {
    match error {
        PlatformError::Io { source } if source.kind() == io::ErrorKind::NotFound => {
            ReadDiscoveryError::Absent
        }
        PlatformError::UnsafeRuntimePath
        | PlatformError::InvalidDiscovery
        | PlatformError::DiscoveryTooLarge => ReadDiscoveryError::Invalid,
        PlatformError::AlreadyRunning | PlatformError::MissingPlatformData { .. } => {
            ReadDiscoveryError::Internal
        }
        PlatformError::Io { .. } => ReadDiscoveryError::Internal,
    }
}

fn not_running(output: &mut dyn CommandOutput, json: bool) -> ExitCode {
    let result = if json {
        write_json(output, &serde_json::json!({"code": "not_running"}))
    } else {
        output.write_stdout("WokCore is not running.\n")
    };
    if result.is_ok() {
        ExitCode::NotRunning
    } else {
        ExitCode::InternalFailure
    }
}

fn invalid_runtime(output: &mut dyn CommandOutput, json: bool) -> ExitCode {
    let result = if json {
        write_json(output, &serde_json::json!({"code": "invalid_runtime"}))
    } else {
        output.write_stderr("WokCore runtime metadata is invalid.\n")
    };
    if result.is_ok() {
        ExitCode::InvalidInput
    } else {
        ExitCode::InternalFailure
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IdentityError {
    Unreachable,
    InvalidAuthority,
    InstanceMismatch,
    ApiMismatch,
    InvalidResponse,
    Internal,
}

enum ReadDiscoveryError {
    Absent,
    Invalid,
    Internal,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HealthResponse {
    status: String,
    instance_id: uuid::Uuid,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CapabilitiesResponse {
    wokcore_version: String,
    management_api_major: u8,
    #[serde(rename = "minimum_management_api_major")]
    _minimum_management_api_major: u8,
    #[serde(rename = "maximum_management_api_major")]
    _maximum_management_api_major: u8,
    #[serde(rename = "provider_protocols")]
    _provider_protocols: Vec<String>,
    #[serde(rename = "capabilities")]
    _capabilities: Vec<String>,
    instance_id: uuid::Uuid,
}
