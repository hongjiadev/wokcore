mod error;
mod model;
mod request_id;
mod routes;
mod security;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware,
    routing::{delete, get, post},
};

use crate::ServerState;

use request_id::apply_response_envelope;
use routes::{
    authorize, cancel_drain, capabilities, drain, health, method_not_allowed, not_found, revoke,
    service_status, stop,
};
use security::enforce_request_security;

pub fn build_router(state: ServerState) -> Router {
    Router::new()
        .route("/wokcore/v1/health", get(health))
        .route("/wokcore/v1/capabilities", get(capabilities))
        .route("/wokcore/v1/service/status", get(service_status))
        .route("/wokcore/v1/service/drain", post(drain))
        .route("/wokcore/v1/service/drain/cancel", post(cancel_drain))
        .route("/wokcore/v1/service/stop", post(stop))
        .route("/wokcore/v1/clients/authorize", post(authorize))
        .route(
            "/wokcore/v1/clients/{client_id}/tokens/{token_id}",
            delete(revoke),
        )
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            enforce_request_security,
        ))
        .layer(middleware::from_fn(apply_response_envelope))
        .with_state(state)
}
