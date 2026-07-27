use axum::{extract::State, response::Response};
use serde::Serialize;

use crate::ServerState;
use crate::{
    api::RequestId,
    data_plane::{
        ClientProtocol,
        body::ValidatedEmptyBody,
        response::{bounded_json_response, gateway_error_response},
    },
};
use wokcore_protocols::canonical::GatewayError;

#[derive(Serialize)]
struct ModelsResponse {
    object: &'static str,
    data: Vec<ModelResponse>,
}

#[derive(Serialize)]
struct ModelResponse {
    id: String,
    object: &'static str,
    created: u64,
    owned_by: String,
}

pub(crate) async fn models(
    State(state): State<ServerState>,
    axum::Extension(request_id): axum::Extension<RequestId>,
    axum::Extension(protocol): axum::Extension<ClientProtocol>,
    _body: ValidatedEmptyBody,
) -> Response {
    let Some(providers) = state.providers.as_ref() else {
        return gateway_error_response(&GatewayError::no_executor(), request_id, protocol, None);
    };
    let snapshot = providers.snapshot();
    let data = snapshot
        .public_models()
        .iter()
        .map(|model| ModelResponse {
            id: model.id.clone(),
            object: "model",
            created: 0,
            owned_by: model.owned_by.as_str().to_owned(),
        })
        .collect();
    let response = ModelsResponse {
        object: "list",
        data,
    };
    bounded_json_response(&response, None)
        .unwrap_or_else(|error| gateway_error_response(&error, request_id, protocol, None))
}
