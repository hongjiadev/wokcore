use axum::{
    Extension,
    body::Body,
    extract::State,
    http::{HeaderValue, header},
    response::Response,
};
use wokcore_protocols::{
    InboundLimitsV1,
    canonical::{GatewayError, RequestId as CanonicalRequestId},
    images::ImageGenerationRequest,
};

use crate::{ServerState, api::RequestId, auth::AuthorizedClient};

use super::{
    ClientProtocol, ImageExecutionInput, UpstreamOperation,
    body::{ValidatedImageEditBody, ValidatedJsonBody},
    execute_canonical,
    images::execute_image,
    response::{attach_upstream_request_id, gateway_error_response},
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
        Ok(canonical) => canonical,
        Err(error) => return gateway_error_response(&error, request_id, protocol, None),
    };
    let streaming = canonical.stream;
    drop(bytes);
    match execute_canonical(&state, &authorized, canonical, UpstreamOperation::Text).await {
        Ok(super::Executed::Response(executed)) if !streaming => {
            match super::anthropic::encode_message(&executed, canonical_request_id) {
                Ok(response) => attach_request_observation(response, executed.observation),
                Err(error) => attach_request_observation(
                    gateway_error_response(
                        &error,
                        request_id,
                        protocol,
                        executed.response.upstream_request_id(),
                    ),
                    executed.observation,
                ),
            }
        }
        Ok(super::Executed::Stream(executed)) if streaming => {
            encode_stream(&state, executed, request_id, protocol)
        }
        Ok(_) => gateway_error_response(
            &GatewayError::internal("upstream execution mode mismatch"),
            request_id,
            protocol,
            None,
        ),
        Err(error) => attach_optional_request_observation(
            gateway_error_response(
                &error.error,
                request_id,
                protocol,
                error.upstream_request_id.as_ref(),
            ),
            error.observation,
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
        Ok(super::Executed::Response(executed)) => {
            match super::anthropic::encode_token_count(&executed) {
                Ok(response) => attach_request_observation(response, executed.observation),
                Err(error) => attach_request_observation(
                    gateway_error_response(
                        &error,
                        request_id,
                        protocol,
                        executed.response.upstream_request_id(),
                    ),
                    executed.observation,
                ),
            }
        }
        Ok(super::Executed::Stream(_)) => gateway_error_response(
            &GatewayError::internal("upstream execution mode mismatch"),
            request_id,
            protocol,
            None,
        ),
        Err(error) => attach_optional_request_observation(
            gateway_error_response(
                &error.error,
                request_id,
                protocol,
                error.upstream_request_id.as_ref(),
            ),
            error.observation,
        ),
    }
}

pub(crate) async fn images_edit(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
    Extension(protocol): Extension<ClientProtocol>,
    Extension(authorized): Extension<AuthorizedClient>,
    body: ValidatedImageEditBody,
) -> Response {
    execute_image_request(
        &state,
        &authorized,
        request_id,
        protocol,
        ImageExecutionInput::Edit(body.into_request()),
    )
    .await
}

pub(crate) async fn images_generation(
    State(state): State<ServerState>,
    Extension(request_id): Extension<RequestId>,
    Extension(protocol): Extension<ClientProtocol>,
    Extension(authorized): Extension<AuthorizedClient>,
    body: ValidatedJsonBody,
) -> Response {
    let request = match ImageGenerationRequest::decode(&body.into_bytes()) {
        Ok(request) => request,
        Err(error) => return gateway_error_response(&error, request_id, protocol, None),
    };
    execute_image_request(
        &state,
        &authorized,
        request_id,
        protocol,
        ImageExecutionInput::Generation(request),
    )
    .await
}

async fn execute_image_request(
    state: &ServerState,
    authorized: &AuthorizedClient,
    request_id: RequestId,
    protocol: ClientProtocol,
    input: ImageExecutionInput,
) -> Response {
    match execute_image(state, authorized, &request_id.to_string(), input).await {
        Ok(executed) => {
            let upstream_request_id = executed.response.upstream_request_id().cloned();
            let mut response = Response::new(Body::from(executed.response.into_body()));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            attach_request_observation(
                attach_upstream_request_id(response, upstream_request_id.as_ref()),
                executed.observation,
            )
        }
        Err(error) => attach_optional_request_observation(
            gateway_error_response(
                &error.error,
                request_id,
                protocol,
                error.upstream_request_id.as_ref(),
            ),
            error.observation,
        ),
    }
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
        Ok(canonical) => canonical,
        Err(error) => return gateway_error_response(&error, request_id, protocol, None),
    };
    let streaming = canonical.stream;
    drop(bytes);
    if let Err(error) = validate(&canonical) {
        return gateway_error_response(&error, request_id, protocol, None);
    }
    match execute_canonical(state, authorized, canonical, UpstreamOperation::Text).await {
        Ok(super::Executed::Response(executed)) if !streaming => match encode(&executed) {
            Ok(response) => attach_request_observation(response, executed.observation),
            Err(error) => attach_request_observation(
                gateway_error_response(
                    &error,
                    request_id,
                    protocol,
                    executed.response.upstream_request_id(),
                ),
                executed.observation,
            ),
        },
        Ok(super::Executed::Stream(executed)) if streaming => {
            encode_stream(state, executed, request_id, protocol)
        }
        Ok(_) => gateway_error_response(
            &GatewayError::internal("upstream execution mode mismatch"),
            request_id,
            protocol,
            None,
        ),
        Err(error) => attach_optional_request_observation(
            gateway_error_response(
                &error.error,
                request_id,
                protocol,
                error.upstream_request_id.as_ref(),
            ),
            error.observation,
        ),
    }
}

fn encode_stream(
    state: &ServerState,
    executed: super::ExecutedStream,
    request_id: RequestId,
    protocol: ClientProtocol,
) -> Response {
    let upstream_request_id = executed.stream.upstream().upstream_request_id().cloned();
    let observation = executed.observation.clone();
    match super::stream::encode(executed, protocol, &state.stream_diagnostics) {
        Ok(response) => attach_request_observation(response, observation),
        Err(error) => attach_request_observation(
            gateway_error_response(&error, request_id, protocol, upstream_request_id.as_ref()),
            observation,
        ),
    }
}

fn attach_request_observation(
    mut response: Response,
    observation: super::RequestObservationContext,
) -> Response {
    response.extensions_mut().insert(observation);
    response
}

fn attach_optional_request_observation(
    response: Response,
    observation: Option<Box<super::RequestObservationContext>>,
) -> Response {
    match observation {
        Some(observation) => attach_request_observation(response, *observation),
        None => response,
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
