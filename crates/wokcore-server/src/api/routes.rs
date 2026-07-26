use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    Json,
    extract::{Extension, Path, State, rejection::JsonRejection},
    http::StatusCode,
};
use secrecy::ExposeSecret;
use uuid::Uuid;
use wokcore_core::id::ClientId;
use wokcore_protocols::IMPLEMENTED_PROVIDER_PROTOCOLS;

use crate::ServerState;

use super::{
    error::ApiError,
    model::{
        AuthorizeRequest, AuthorizeResponse, CapabilitiesResponse, HealthResponse,
        LifecycleResponse, RevokeResponse,
    },
    request_id::RequestId,
};

pub(crate) async fn health(State(state): State<ServerState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        instance_id: state.instance_id.to_string(),
    })
}

const CAPABILITIES: &[&str] = &[
    "client_token.issue",
    "client_token.revoke",
    "discovery.v1",
    "service.drain",
    "service.status",
];

pub(crate) async fn capabilities(State(state): State<ServerState>) -> Json<CapabilitiesResponse> {
    Json(CapabilitiesResponse {
        wokcore_version: env!("CARGO_PKG_VERSION"),
        management_api_major: 1,
        minimum_management_api_major: 1,
        maximum_management_api_major: 1,
        provider_protocols: IMPLEMENTED_PROVIDER_PROTOCOLS,
        capabilities: CAPABILITIES,
        instance_id: state.instance_id.to_string(),
    })
}

pub(crate) async fn service_status(State(state): State<ServerState>) -> Json<LifecycleResponse> {
    let snapshot = state.lifecycle.snapshot();
    Json(lifecycle_response(snapshot))
}

pub(crate) async fn drain(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
) -> Result<Json<LifecycleResponse>, ApiError> {
    state
        .lifecycle
        .begin_drain(Duration::from_secs(30))
        .await
        .map_err(|_| ApiError::lifecycle_conflict(request_id))?;
    Ok(Json(lifecycle_response(state.lifecycle.snapshot())))
}

pub(crate) async fn cancel_drain(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
) -> Result<Json<LifecycleResponse>, ApiError> {
    let snapshot = state
        .lifecycle
        .cancel_drain()
        .map_err(|_| ApiError::lifecycle_conflict(request_id))?;
    Ok(Json(lifecycle_response(snapshot)))
}

pub(crate) async fn stop(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
) -> Result<Json<LifecycleResponse>, ApiError> {
    let snapshot = state
        .lifecycle
        .request_stop()
        .map_err(|_| ApiError::lifecycle_conflict(request_id))?;
    let response = lifecycle_response(snapshot);
    state.request_shutdown();
    Ok(Json(response))
}

pub(crate) async fn authorize(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
    body: Result<Json<AuthorizeRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<AuthorizeResponse>), ApiError> {
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) if rejection.status() == axum::http::StatusCode::PAYLOAD_TOO_LARGE => {
            return Err(ApiError::payload_too_large(request_id));
        }
        Err(_) => return Err(ApiError::invalid_request_body(request_id)),
    };
    let token_id = Uuid::new_v4().to_string();
    let material = state
        .auth
        .issue_client_token(token_id.clone(), request.client_id.clone(), timestamp())
        .await
        .map_err(|_| ApiError::storage_failure(request_id))?;
    let token = material.into_response_value();
    Ok((
        StatusCode::CREATED,
        Json(AuthorizeResponse {
            client_id: request.client_id,
            token_id,
            token: token.expose_secret().to_owned(),
        }),
    ))
}

pub(crate) async fn revoke(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
    Path((client_id, token_id)): Path<(String, String)>,
) -> Result<Json<RevokeResponse>, ApiError> {
    let client_id =
        ClientId::new(client_id).map_err(|_| ApiError::invalid_client_id(request_id))?;
    let revoked = state
        .auth
        .revoke_client_token(client_id, token_id, timestamp())
        .await
        .map_err(|_| ApiError::storage_failure(request_id))?;
    Ok(Json(RevokeResponse { revoked }))
}

fn lifecycle_response(snapshot: crate::lifecycle::LifecycleSnapshot) -> LifecycleResponse {
    LifecycleResponse {
        phase: lifecycle_phase_name(snapshot.phase).to_owned(),
        active_requests: snapshot.active_requests,
    }
}

const fn lifecycle_phase_name(phase: crate::lifecycle::LifecyclePhase) -> &'static str {
    use crate::lifecycle::LifecyclePhase;

    match phase {
        LifecyclePhase::Starting => "starting",
        LifecyclePhase::Running => "running",
        LifecyclePhase::Draining => "draining",
        LifecyclePhase::AwaitingCancellation => "awaiting_cancellation",
        LifecyclePhase::Stopping => "stopping",
    }
}

fn timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

pub(crate) async fn not_found(Extension(request_id): Extension<RequestId>) -> ApiError {
    ApiError::not_found(request_id)
}

pub(crate) async fn method_not_allowed(Extension(request_id): Extension<RequestId>) -> ApiError {
    ApiError::method_not_allowed(request_id)
}

#[cfg(test)]
mod tests {
    use crate::lifecycle::{LifecyclePhase, LifecycleSnapshot};

    use super::lifecycle_response;

    #[test]
    fn lifecycle_phase_names_are_stable_snake_case() {
        let response = lifecycle_response(LifecycleSnapshot {
            phase: LifecyclePhase::AwaitingCancellation,
            active_requests: 1,
        });

        assert_eq!(response.phase, "awaiting_cancellation");
    }
}
