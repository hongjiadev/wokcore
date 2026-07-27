use axum::{Extension, extract::State, response::Response};
use wokcore_protocols::{
    InboundLimitsV1,
    canonical::{GatewayError, RequestId as CanonicalRequestId},
};

use crate::{ServerState, api::RequestId, auth::AuthorizedClient};

use super::{
    ClientProtocol, UpstreamOperation,
    body::{ValidatedImageEditBody, ValidatedJsonBody},
    execute_canonical,
    response::{gateway_error_response, invalid_request, unsupported_capability},
};

pub(crate) async fn responses(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
    Extension(protocol): Extension<ClientProtocol>,
    Extension(authorized): Extension<AuthorizedClient>,
    body: ValidatedJsonBody,
) -> Response {
    execute_text(
        &state,
        &authorized,
        request_id,
        protocol,
        body,
        super::responses::validate_request,
        super::responses::encode,
    )
    .await
}

pub(crate) async fn chat(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
    Extension(protocol): Extension<ClientProtocol>,
    Extension(authorized): Extension<AuthorizedClient>,
    body: ValidatedJsonBody,
) -> Response {
    execute_text(
        &state,
        &authorized,
        request_id,
        protocol,
        body,
        |_| Ok(()),
        super::chat::encode,
    )
    .await
}

pub(crate) async fn anthropic(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
    Extension(protocol): Extension<ClientProtocol>,
    Extension(authorized): Extension<AuthorizedClient>,
    body: ValidatedJsonBody,
) -> Response {
    let canonical_request_id = CanonicalRequestId::new(request_id.to_string());
    let bytes = body.into_bytes();
    let canonical = match decode(protocol, canonical_request_id.clone(), &bytes) {
        Ok(canonical) if !canonical.stream => canonical,
        Ok(_) => {
            return gateway_error_response(
                &GatewayError::unsupported_capability(),
                request_id,
                protocol,
                None,
            );
        }
        Err(error) => return gateway_error_response(&error, request_id, protocol, None),
    };
    drop(bytes);
    match execute_canonical(&state, &authorized, canonical, UpstreamOperation::Text).await {
        Ok(executed) => match super::anthropic::encode_message(&executed, canonical_request_id) {
            Ok(response) => response,
            Err(error) => gateway_error_response(
                &error,
                request_id,
                protocol,
                executed.response.upstream_request_id(),
            ),
        },
        Err(error) => gateway_error_response(
            &error.error,
            request_id,
            protocol,
            error.upstream_request_id.as_ref(),
        ),
    }
}

pub(crate) async fn count_tokens(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
    Extension(protocol): Extension<ClientProtocol>,
    Extension(authorized): Extension<AuthorizedClient>,
    body: ValidatedJsonBody,
) -> Response {
    let bytes = body.into_bytes();
    let canonical = match decode(
        protocol,
        CanonicalRequestId::new(request_id.to_string()),
        &bytes,
    ) {
        Ok(canonical) => canonical,
        Err(error) => return gateway_error_response(&error, request_id, protocol, None),
    };
    drop(bytes);
    match execute_canonical(
        &state,
        &authorized,
        canonical,
        UpstreamOperation::CountTokens,
    )
    .await
    {
        Ok(executed) => match super::anthropic::encode_token_count(&executed) {
            Ok(response) => response,
            Err(error) => gateway_error_response(
                &error,
                request_id,
                protocol,
                executed.response.upstream_request_id(),
            ),
        },
        Err(error) => gateway_error_response(
            &error.error,
            request_id,
            protocol,
            error.upstream_request_id.as_ref(),
        ),
    }
}

pub(crate) async fn unsupported_multipart(
    Extension(request_id): Extension<RequestId>,
    Extension(protocol): Extension<ClientProtocol>,
    _body: ValidatedImageEditBody,
) -> Response {
    unsupported_capability(request_id, protocol)
}

pub(crate) async fn unsupported_json(
    Extension(request_id): Extension<RequestId>,
    Extension(protocol): Extension<ClientProtocol>,
    body: ValidatedJsonBody,
) -> Response {
    if serde_json::from_slice::<serde_json::Value>(&body.into_bytes()).is_err() {
        return invalid_request(request_id, protocol);
    }
    unsupported_capability(request_id, protocol)
}

async fn execute_text(
    state: &ServerState,
    authorized: &AuthorizedClient,
    request_id: RequestId,
    protocol: ClientProtocol,
    body: ValidatedJsonBody,
    validate: fn(&wokcore_protocols::canonical::CanonicalRequest) -> Result<(), GatewayError>,
    encode: fn(&super::ExecutedResponse) -> Result<Response, GatewayError>,
) -> Response {
    let bytes = body.into_bytes();
    let canonical = match decode(
        protocol,
        CanonicalRequestId::new(request_id.to_string()),
        &bytes,
    ) {
        Ok(canonical) if !canonical.stream => canonical,
        Ok(_) => {
            return gateway_error_response(
                &GatewayError::unsupported_capability(),
                request_id,
                protocol,
                None,
            );
        }
        Err(error) => return gateway_error_response(&error, request_id, protocol, None),
    };
    drop(bytes);
    if let Err(error) = validate(&canonical) {
        return gateway_error_response(&error, request_id, protocol, None);
    }
    match execute_canonical(state, authorized, canonical, UpstreamOperation::Text).await {
        Ok(executed) => match encode(&executed) {
            Ok(response) => response,
            Err(error) => gateway_error_response(
                &error,
                request_id,
                protocol,
                executed.response.upstream_request_id(),
            ),
        },
        Err(error) => gateway_error_response(
            &error.error,
            request_id,
            protocol,
            error.upstream_request_id.as_ref(),
        ),
    }
}

fn decode(
    protocol: ClientProtocol,
    request_id: CanonicalRequestId,
    bytes: &[u8],
) -> Result<wokcore_protocols::canonical::CanonicalRequest, GatewayError> {
    protocol
        .inbound_codec()
        .ok_or_else(GatewayError::unsupported_capability)?
        .decode(request_id, bytes, InboundLimitsV1::default())
}
