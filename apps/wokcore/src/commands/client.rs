use reqwest::{Response, header::HOST};
use secrecy::{SecretString, zeroize::Zeroizing};
use serde::Serialize;
use wokcore_platform::{DiscoveryRecord, DiscoveryStore, PlatformError};
use wokcore_storage::{ReadOnlyStateStore, StorageError};

use crate::RunDependencies;

use super::status::{validated_authority, verify_identity};

const MAX_MANAGEMENT_BODY_BYTES: usize = 64 * 1024;

pub(super) struct ControlClient {
    record: DiscoveryRecord,
    authority: String,
    client: reqwest::Client,
}

impl ControlClient {
    pub(super) async fn connect(
        dependencies: &RunDependencies,
    ) -> Result<Self, ControlClientError> {
        let store = DiscoveryStore::new(&dependencies.paths).map_err(map_platform)?;
        let record = store.read().map_err(map_platform)?;
        if !dependencies.process.is_running(record.pid) {
            return Err(ControlClientError::NotRunning);
        }
        verify_identity(&record)
            .await
            .map_err(|_| ControlClientError::IdentityMismatch)?;
        let authority = validated_authority(&record.base_url)
            .map_err(|_| ControlClientError::InvalidRuntime)?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|_| ControlClientError::Internal)?;
        Ok(Self {
            record,
            authority,
            client,
        })
    }

    pub(super) async fn management_secret(
        &self,
        dependencies: &RunDependencies,
    ) -> Result<SecretString, ControlClientError> {
        let state =
            ReadOnlyStateStore::open_live(&dependencies.paths.state_db).map_err(map_storage)?;
        let binding = state
            .runtime_secret_binding("management")
            .map_err(map_storage)?
            .ok_or(ControlClientError::Authentication)?;
        dependencies
            .secrets
            .get(&binding.secret_ref)
            .await
            .map_err(|_| ControlClientError::Authentication)
    }

    pub(super) async fn post_json<T: Serialize + ?Sized>(
        &self,
        path: &str,
        management: &SecretString,
        body: Option<&T>,
    ) -> Result<Response, ControlClientError> {
        use secrecy::ExposeSecret;

        let mut request = self
            .client
            .post(format!("{}{path}", self.record.base_url))
            .header(HOST, &self.authority)
            .bearer_auth(management.expose_secret());
        if let Some(body) = body {
            request = request.json(body);
        }
        request
            .send()
            .await
            .map_err(|_| ControlClientError::NotRunning)
    }
}

pub(super) async fn response_body(
    response: Response,
) -> Result<Zeroizing<Vec<u8>>, ControlClientError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MANAGEMENT_BODY_BYTES as u64)
    {
        return Err(ControlClientError::Internal);
    }
    let body = response
        .bytes()
        .await
        .map_err(|_| ControlClientError::Internal)?;
    if body.len() > MAX_MANAGEMENT_BODY_BYTES {
        return Err(ControlClientError::Internal);
    }
    Ok(Zeroizing::new(body.to_vec()))
}

fn map_platform(error: PlatformError) -> ControlClientError {
    match error {
        PlatformError::Io { source } if source.kind() == std::io::ErrorKind::NotFound => {
            ControlClientError::NotRunning
        }
        PlatformError::UnsafeRuntimePath
        | PlatformError::InvalidDiscovery
        | PlatformError::DiscoveryTooLarge => ControlClientError::InvalidRuntime,
        _ => ControlClientError::Internal,
    }
}

fn map_storage(error: StorageError) -> ControlClientError {
    match error {
        StorageError::StateDatabaseCorrupt { .. } => ControlClientError::StorageCorruption,
        StorageError::Io { source } if source.kind() == std::io::ErrorKind::NotFound => {
            ControlClientError::StorageCorruption
        }
        _ => ControlClientError::Internal,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ControlClientError {
    NotRunning,
    InvalidRuntime,
    IdentityMismatch,
    Authentication,
    StorageCorruption,
    Internal,
}
