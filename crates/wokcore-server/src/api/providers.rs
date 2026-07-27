use std::sync::Arc;

use axum::{
    Json,
    body::Bytes,
    extract::{
        Extension, Path, State,
        rejection::{JsonRejection, PathRejection},
    },
    http::{HeaderMap, StatusCode, header::CONTENT_TYPE},
};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use wokcore_core::{
    id::{AccountId, ProviderId},
    secret::{SecretPurpose, SecretRef, SecretScope},
};
use wokcore_engine::{catalog::ProviderDefinition, models::PublicModelMetadata};

use crate::{
    ServerState,
    providers::{
        ProviderCandidate, ProviderCommit, ProviderManagement, ProviderManagementError,
        ProviderRuntimeStatus, ProviderSecretMetadata, ProviderValidation,
    },
};

use super::{error::ApiError, request_id::RequestId};

const SCHEMA_VERSION: u8 = 1;

#[derive(Serialize)]
pub(crate) struct CatalogResponse {
    schema_version: u8,
    catalog_schema_version: u32,
    baseline_commit: String,
    providers: Vec<ProviderDefinition>,
}

#[derive(Serialize)]
pub(crate) struct RuntimeResponse {
    schema_version: u8,
    #[serde(flatten)]
    runtime: ProviderRuntimeStatus,
}

#[derive(Serialize)]
pub(crate) struct ModelsResponse {
    schema_version: u8,
    models: Vec<PublicModelMetadata>,
}

#[derive(Serialize)]
pub(crate) struct ValidationResponse {
    schema_version: u8,
    valid: bool,
    #[serde(flatten)]
    validation: ProviderValidation,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommitRequest {
    expected_revision: u64,
    #[serde(flatten)]
    candidate: ProviderCandidate,
}

#[derive(Serialize)]
pub(crate) struct CommitResponse {
    schema_version: u8,
    #[serde(flatten)]
    commit: ProviderCommit,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateSecretRequest {
    provider_id: ProviderId,
    account_id: Option<AccountId>,
    purpose: SecretPurpose,
    secret: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplaceSecretRequest {
    secret: String,
}

#[derive(Serialize)]
pub(crate) struct SecretResponse {
    schema_version: u8,
    operation: &'static str,
    #[serde(flatten)]
    metadata: ProviderSecretMetadata,
}

pub(crate) async fn catalog(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
) -> Result<Json<CatalogResponse>, ApiError> {
    let providers = provider_management(&state, request_id)?;
    let catalog = providers.catalog();
    Ok(Json(CatalogResponse {
        schema_version: SCHEMA_VERSION,
        catalog_schema_version: catalog.schema_version(),
        baseline_commit: catalog.baseline_commit().to_owned(),
        providers: catalog.providers().to_vec(),
    }))
}

pub(crate) async fn runtime_status(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
) -> Result<Json<RuntimeResponse>, ApiError> {
    let providers = provider_management(&state, request_id)?;
    Ok(Json(RuntimeResponse {
        schema_version: SCHEMA_VERSION,
        runtime: providers.status(),
    }))
}

pub(crate) async fn models(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
) -> Result<Json<ModelsResponse>, ApiError> {
    let providers = provider_management(&state, request_id)?;
    Ok(Json(ModelsResponse {
        schema_version: SCHEMA_VERSION,
        models: providers.models(),
    }))
}

pub(crate) async fn validate_configuration(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
    body: Result<Json<ProviderCandidate>, JsonRejection>,
) -> Result<Json<ValidationResponse>, ApiError> {
    let candidate = json_body(body, request_id)?;
    let providers = provider_management(&state, request_id)?;
    let validation = providers
        .validate(&candidate)
        .map_err(|error| map_provider_error(error, request_id))?;
    Ok(Json(ValidationResponse {
        schema_version: SCHEMA_VERSION,
        valid: true,
        validation,
    }))
}

pub(crate) async fn commit_configuration(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
    body: Result<Json<CommitRequest>, JsonRejection>,
) -> Result<Json<CommitResponse>, ApiError> {
    let request = json_body(body, request_id)?;
    let providers = provider_management(&state, request_id)?;
    let commit = providers
        .commit(request.expected_revision, request.candidate)
        .await
        .map_err(|error| map_provider_error(error, request_id))?;
    Ok(Json(CommitResponse {
        schema_version: SCHEMA_VERSION,
        commit,
    }))
}

pub(crate) async fn reload(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<CommitResponse>, ApiError> {
    require_empty_body(&headers, &body, request_id)?;
    let providers = provider_management(&state, request_id)?;
    let commit = providers
        .reload()
        .await
        .map_err(|error| map_provider_error(error, request_id))?;
    Ok(Json(CommitResponse {
        schema_version: SCHEMA_VERSION,
        commit,
    }))
}

pub(crate) async fn create_secret(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
    body: Result<Json<CreateSecretRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<SecretResponse>), ApiError> {
    let request = json_body(body, request_id)?;
    if request.secret.is_empty() {
        return Err(ApiError::invalid_request_body(request_id));
    }
    let providers = provider_management(&state, request_id)?;
    let metadata = providers
        .create_secret(
            SecretScope {
                provider_id: request.provider_id,
                account_id: request.account_id,
                purpose: request.purpose,
            },
            SecretString::from(request.secret),
        )
        .await
        .map_err(|error| map_provider_error(error, request_id))?;
    Ok((
        StatusCode::CREATED,
        Json(SecretResponse {
            schema_version: SCHEMA_VERSION,
            operation: "created",
            metadata,
        }),
    ))
}

pub(crate) async fn replace_secret(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<ReplaceSecretRequest>, JsonRejection>,
) -> Result<Json<SecretResponse>, ApiError> {
    let secret_ref = secret_ref_path(path, request_id)?;
    let request = json_body(body, request_id)?;
    if request.secret.is_empty() {
        return Err(ApiError::invalid_request_body(request_id));
    }
    let providers = provider_management(&state, request_id)?;
    let metadata = providers
        .replace_secret(&secret_ref, SecretString::from(request.secret))
        .await
        .map_err(|error| map_provider_error(error, request_id))?;
    Ok(Json(SecretResponse {
        schema_version: SCHEMA_VERSION,
        operation: "replaced",
        metadata,
    }))
}

pub(crate) async fn delete_secret(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<SecretResponse>, ApiError> {
    require_empty_body(&headers, &body, request_id)?;
    let secret_ref = secret_ref_path(path, request_id)?;
    let providers = provider_management(&state, request_id)?;
    let metadata = providers
        .delete_secret(&secret_ref)
        .await
        .map_err(|error| map_provider_error(error, request_id))?;
    Ok(Json(SecretResponse {
        schema_version: SCHEMA_VERSION,
        operation: "deleted",
        metadata,
    }))
}

fn provider_management(
    state: &ServerState,
    request_id: RequestId,
) -> Result<Arc<ProviderManagement>, ApiError> {
    state
        .providers
        .clone()
        .ok_or_else(|| ApiError::provider_runtime_unavailable(request_id))
}

fn json_body<T>(
    body: Result<Json<T>, JsonRejection>,
    request_id: RequestId,
) -> Result<T, ApiError> {
    match body {
        Ok(Json(body)) => Ok(body),
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            Err(ApiError::payload_too_large(request_id))
        }
        Err(rejection) if rejection.status() == StatusCode::UNSUPPORTED_MEDIA_TYPE => {
            Err(ApiError::unsupported_media_type(request_id))
        }
        Err(_) => Err(ApiError::invalid_request_body(request_id)),
    }
}

fn secret_ref_path(
    path: Result<Path<String>, PathRejection>,
    request_id: RequestId,
) -> Result<SecretRef, ApiError> {
    let Path(value) = path.map_err(|_| ApiError::invalid_path_parameters(request_id))?;
    SecretRef::parse(value).map_err(|_| ApiError::invalid_path_parameters(request_id))
}

fn require_empty_body(
    headers: &HeaderMap,
    body: &Bytes,
    request_id: RequestId,
) -> Result<(), ApiError> {
    if headers.contains_key(CONTENT_TYPE) {
        return Err(ApiError::unsupported_media_type(request_id));
    }
    if !body.is_empty() {
        return Err(ApiError::invalid_request_body(request_id));
    }
    Ok(())
}

fn map_provider_error(error: ProviderManagementError, request_id: RequestId) -> ApiError {
    match error {
        ProviderManagementError::InvalidCatalog => ApiError::provider_internal_failure(request_id),
        ProviderManagementError::StorageFailure => ApiError::provider_storage_failure(request_id),
        ProviderManagementError::InvalidConfiguration => {
            ApiError::provider_config_invalid(request_id)
        }
        ProviderManagementError::RevisionConflict => {
            ApiError::provider_config_revision_conflict(request_id)
        }
        ProviderManagementError::SecretNotFound => ApiError::provider_secret_not_found(request_id),
        ProviderManagementError::SecretAlreadyExists => {
            ApiError::provider_secret_already_exists(request_id)
        }
        ProviderManagementError::SecretInUse => ApiError::provider_secret_in_use(request_id),
        ProviderManagementError::SecretProtected => ApiError::provider_secret_protected(request_id),
        ProviderManagementError::ReadOnlySecretStore => {
            ApiError::provider_secret_store_read_only(request_id)
        }
    }
}
