use serde::{Deserialize, Serialize};
use wokcore_core::id::ClientId;

#[derive(Serialize)]
pub(crate) struct HealthResponse {
    pub status: &'static str,
    pub instance_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuthorizeRequest {
    pub client_id: ClientId,
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
}

#[derive(Serialize)]
pub(crate) struct CapabilitiesResponse {
    pub wokcore_version: &'static str,
    pub management_api_major: u8,
    pub minimum_management_api_major: u8,
    pub maximum_management_api_major: u8,
    pub provider_protocols: &'static [&'static str],
    pub capabilities: &'static [&'static str],
    pub instance_id: String,
}

#[derive(Serialize)]
pub(crate) struct LifecycleResponse {
    pub phase: String,
    pub active_requests: usize,
}

#[derive(Serialize)]
pub(crate) struct AuthorizeResponse {
    pub client_id: ClientId,
    pub token_id: String,
    pub token: String,
    pub scopes: Vec<&'static str>,
}

#[derive(Serialize)]
pub(crate) struct RevokeResponse {
    pub revoked: bool,
}
