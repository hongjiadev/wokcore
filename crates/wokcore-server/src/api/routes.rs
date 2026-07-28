use std::{str::FromStr, time::Duration};

use axum::{
    Json,
    extract::{
        Extension, Path, State,
        rejection::{JsonRejection, PathRejection},
    },
    http::StatusCode,
};
use secrecy::ExposeSecret;
use wokcore_core::id::ClientId;
use wokcore_diagnostics::event::StateTransition;
use wokcore_protocols::IMPLEMENTED_PROVIDER_PROTOCOLS;
use wokcore_storage::ClientTokenScope;

use crate::ServerState;

use super::{
    error::ApiError,
    model::{
        AuthorizeRequest, AuthorizeResponse, CapabilitiesResponse, ClientTokenStatusResponse,
        HealthResponse, LifecycleResponse, RevokeResponse,
    },
    request_id::{RequestId, record_lifecycle_diagnostic},
};

pub(crate) async fn health(State(state): State<ServerState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        instance_id: state.instance_id.to_string(),
    })
}

const CAPABILITIES: &[&str] = &[
    "client_token.issue",
    "client_token.inspect",
    "client_token.revoke",
    "diagnostics.events.v1",
    "diagnostics.export.v1",
    "discovery.v1",
    "provider.catalog.v1",
    "provider.config.v1",
    "provider.models.v1",
    "provider.secrets.v1",
    "service.drain",
    "service.status",
    "sessions.index.v1",
    "sessions.messages.v1",
    "usage.session.v1",
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
        installation_id: state.auth.installation_id(),
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
    let before = state.lifecycle.snapshot().phase;
    state
        .lifecycle
        .begin_drain(Duration::from_secs(30))
        .await
        .map_err(|_| ApiError::lifecycle_conflict(request_id))?;
    let after = state.lifecycle.snapshot().phase;
    if before == crate::lifecycle::LifecyclePhase::Running {
        let transition = if after == crate::lifecycle::LifecyclePhase::AwaitingCancellation {
            StateTransition::DrainingToAwaitingCancellation
        } else {
            StateTransition::ReadyToDraining
        };
        record_lifecycle_diagnostic(&state, request_id, transition);
    }
    Ok(Json(lifecycle_response(state.lifecycle.snapshot())))
}

pub(crate) async fn cancel_drain(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
) -> Result<Json<LifecycleResponse>, ApiError> {
    let before = state.lifecycle.snapshot().phase;
    let snapshot = state
        .lifecycle
        .cancel_drain()
        .map_err(|_| ApiError::lifecycle_conflict(request_id))?;
    if matches!(
        before,
        crate::lifecycle::LifecyclePhase::Draining
            | crate::lifecycle::LifecyclePhase::AwaitingCancellation
    ) {
        record_lifecycle_diagnostic(&state, request_id, StateTransition::DrainingToReady);
    }
    Ok(Json(lifecycle_response(snapshot)))
}

pub(crate) async fn stop(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
) -> Result<Json<LifecycleResponse>, ApiError> {
    let before = state.lifecycle.snapshot().phase;
    let snapshot = state
        .lifecycle
        .request_stop()
        .map_err(|_| ApiError::lifecycle_conflict(request_id))?;
    if before != crate::lifecycle::LifecyclePhase::Stopping {
        record_lifecycle_diagnostic(&state, request_id, StateTransition::ReadyToStopping);
    }
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
    let scopes = parse_authorize_scopes(request.scopes)
        .ok_or_else(|| ApiError::invalid_request_body(request_id))?;
    let token_id = match request.token_id {
        Some(token_id) => token_id.to_string(),
        None => state
            .token_metadata
            .new_token_id()
            .map_err(|_| ApiError::internal_failure(request_id))?,
    };
    let issued_at = state
        .token_metadata
        .now()
        .map_err(|_| ApiError::internal_failure(request_id))?;
    let material = state
        .auth
        .issue_client_token_with_scopes(
            token_id.clone(),
            request.client_id.clone(),
            issued_at,
            scopes.clone(),
        )
        .await
        .map_err(|_| ApiError::storage_failure(request_id))?;
    let token = material.into_response_value();
    Ok((
        StatusCode::CREATED,
        Json(AuthorizeResponse {
            client_id: request.client_id,
            token_id,
            token: token.expose_secret().to_owned(),
            scopes: scopes.into_iter().map(ClientTokenScope::as_str).collect(),
        }),
    ))
}

fn parse_authorize_scopes(scopes: Option<Vec<String>>) -> Option<Vec<ClientTokenScope>> {
    let scopes = scopes.unwrap_or_else(|| vec![ClientTokenScope::ProxyUse.as_str().to_owned()]);
    if scopes.is_empty() {
        return None;
    }
    let mut parsed = scopes
        .iter()
        .map(|scope| ClientTokenScope::from_str(scope))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let original_len = parsed.len();
    parsed.sort_unstable();
    parsed.dedup();
    (parsed.len() == original_len).then_some(parsed)
}

pub(crate) async fn client_token_status(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<Json<ClientTokenStatusResponse>, ApiError> {
    let Path((client_id, token_id)) =
        path.map_err(|_| ApiError::invalid_path_parameters(request_id))?;
    let client_id =
        ClientId::new(client_id).map_err(|_| ApiError::invalid_client_id(request_id))?;
    Ok(Json(ClientTokenStatusResponse {
        active: state.auth.client_token_active(&client_id, &token_id).await,
    }))
}

pub(crate) async fn revoke(
    Extension(request_id): Extension<RequestId>,
    State(state): State<ServerState>,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<Json<RevokeResponse>, ApiError> {
    let Path((client_id, token_id)) =
        path.map_err(|_| ApiError::invalid_path_parameters(request_id))?;
    let client_id =
        ClientId::new(client_id).map_err(|_| ApiError::invalid_client_id(request_id))?;
    let revoked_at = state
        .token_metadata
        .now()
        .map_err(|_| ApiError::internal_failure(request_id))?;
    let revoked = state
        .auth
        .revoke_client_token(client_id, token_id, revoked_at)
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

pub(crate) async fn not_found(Extension(request_id): Extension<RequestId>) -> ApiError {
    ApiError::not_found(request_id)
}

pub(crate) async fn method_not_allowed(Extension(request_id): Extension<RequestId>) -> ApiError {
    ApiError::method_not_allowed(request_id)
}

#[cfg(test)]
mod tests {
    use crate::lifecycle::{LifecyclePhase, LifecycleSnapshot};
    use wokcore_storage::ClientTokenScope;

    use super::{lifecycle_response, parse_authorize_scopes};

    #[test]
    fn lifecycle_phase_names_are_stable_snake_case() {
        let response = lifecycle_response(LifecycleSnapshot {
            phase: LifecyclePhase::AwaitingCancellation,
            active_requests: 1,
        });

        assert_eq!(response.phase, "awaiting_cancellation");
    }

    #[test]
    fn authorization_scope_parser_accepts_every_stable_scope_in_wire_order() {
        let scopes = [
            "clients.manage",
            "providers.write",
            "service.control",
            "diagnostics.export",
            "providers.read",
            "service.read",
            "usage.read",
            "sessions.read",
            "diagnostics.read",
            "proxy.use",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();

        assert_eq!(
            parse_authorize_scopes(Some(scopes)),
            Some(vec![
                ClientTokenScope::ProxyUse,
                ClientTokenScope::SessionsRead,
                ClientTokenScope::UsageRead,
                ClientTokenScope::DiagnosticsRead,
                ClientTokenScope::DiagnosticsExport,
                ClientTokenScope::ServiceRead,
                ClientTokenScope::ServiceControl,
                ClientTokenScope::ProvidersRead,
                ClientTokenScope::ProvidersWrite,
                ClientTokenScope::ClientsManage,
            ])
        );
    }

    #[test]
    fn authorization_scope_parser_defaults_and_rejects_invalid_lists() {
        assert_eq!(
            parse_authorize_scopes(None),
            Some(vec![ClientTokenScope::ProxyUse])
        );
        assert_eq!(parse_authorize_scopes(Some(Vec::new())), None);
        assert_eq!(
            parse_authorize_scopes(Some(vec!["unknown".to_owned()])),
            None
        );
        assert_eq!(
            parse_authorize_scopes(Some(vec![
                "service.read".to_owned(),
                "service.read".to_owned(),
            ])),
            None
        );
    }
}
