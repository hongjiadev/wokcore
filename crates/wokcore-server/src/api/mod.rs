mod cursor;
mod error;
mod export;
mod logs;
mod model;
mod providers;
mod request_id;
mod routes;
mod security;
mod sessions;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware,
    routing::{MethodFilter, delete, get, on, post, put},
};

use crate::ServerState;

use crate::data_plane::{
    IMAGE_MULTIPART_WIRE_LIMIT, JSON_BODY_LIMIT, anthropic, chat, count_tokens, images_edit,
    images_generation, models_endpoint, responses,
};
use export::diagnostics_export;
use logs::logs;
use providers::{
    catalog, commit_configuration, create_secret, delete_secret, models, reload, replace_secret,
    runtime_status, validate_configuration,
};
pub(crate) use request_id::RequestId;
use request_id::apply_response_envelope;
use routes::{
    authorize, cancel_drain, capabilities, drain, health, method_not_allowed, not_found, revoke,
    service_status, stop,
};
use security::enforce_request_security;
use sessions::{list_sessions, session_messages, usage};

pub fn build_router(state: ServerState) -> Router {
    Router::new()
        .route(
            "/wokcore/v1/health",
            on(MethodFilter::GET, health).on(MethodFilter::HEAD, method_not_allowed),
        )
        .route(
            "/wokcore/v1/capabilities",
            on(MethodFilter::GET, capabilities).on(MethodFilter::HEAD, method_not_allowed),
        )
        .route(
            "/wokcore/v1/service/status",
            on(MethodFilter::GET, service_status).on(MethodFilter::HEAD, method_not_allowed),
        )
        .route("/wokcore/v1/service/drain", post(drain))
        .route("/wokcore/v1/service/drain/cancel", post(cancel_drain))
        .route("/wokcore/v1/service/stop", post(stop))
        .route("/wokcore/v1/clients/authorize", post(authorize))
        .route(
            "/wokcore/v1/sessions",
            on(MethodFilter::GET, list_sessions).on(MethodFilter::HEAD, method_not_allowed),
        )
        .route(
            "/wokcore/v1/sessions/{session_key}/messages",
            on(MethodFilter::GET, session_messages).on(MethodFilter::HEAD, method_not_allowed),
        )
        .route(
            "/wokcore/v1/usage",
            on(MethodFilter::GET, usage).on(MethodFilter::HEAD, method_not_allowed),
        )
        .route(
            "/wokcore/v1/logs",
            on(MethodFilter::GET, logs).on(MethodFilter::HEAD, method_not_allowed),
        )
        .route(
            "/wokcore/v1/diagnostics/export",
            on(MethodFilter::GET, diagnostics_export).on(MethodFilter::HEAD, method_not_allowed),
        )
        .route(
            "/wokcore/v1/providers/catalog",
            on(MethodFilter::GET, catalog).on(MethodFilter::HEAD, method_not_allowed),
        )
        .route(
            "/wokcore/v1/providers/runtime",
            on(MethodFilter::GET, runtime_status).on(MethodFilter::HEAD, method_not_allowed),
        )
        .route(
            "/wokcore/v1/providers/models",
            on(MethodFilter::GET, models).on(MethodFilter::HEAD, method_not_allowed),
        )
        .route(
            "/wokcore/v1/providers/config/validate",
            post(validate_configuration),
        )
        .route("/wokcore/v1/providers/config", put(commit_configuration))
        .route("/wokcore/v1/providers/reload", post(reload))
        .route("/wokcore/v1/provider-secrets", post(create_secret))
        .route(
            "/wokcore/v1/provider-secrets/{secret_ref}",
            put(replace_secret).delete(delete_secret),
        )
        .route(
            "/wokcore/v1/clients/{client_id}/tokens/{token_id}",
            delete(revoke),
        )
        .route(
            "/v1/responses",
            post(responses).layer(DefaultBodyLimit::max(JSON_BODY_LIMIT)),
        )
        .route(
            "/v1/chat/completions",
            post(chat).layer(DefaultBodyLimit::max(JSON_BODY_LIMIT)),
        )
        .route(
            "/v1/messages",
            post(anthropic).layer(DefaultBodyLimit::max(JSON_BODY_LIMIT)),
        )
        .route(
            "/v1/messages/count_tokens",
            post(count_tokens).layer(DefaultBodyLimit::max(JSON_BODY_LIMIT)),
        )
        .route("/v1/models", get(models_endpoint))
        .route(
            "/v1/images/generations",
            post(images_generation).layer(DefaultBodyLimit::max(JSON_BODY_LIMIT)),
        )
        .route(
            "/v1/images/edits",
            post(images_edit).layer(DefaultBodyLimit::max(IMAGE_MULTIPART_WIRE_LIMIT)),
        )
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            enforce_request_security,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            apply_response_envelope,
        ))
        .with_state(state)
}
