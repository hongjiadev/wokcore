use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use secrecy::SecretString;
use url::Url;
use wokcore_engine::{
    auth::{SecretResolutionError, SecretResolver, resolve_outbound_auth},
    catalog::AdapterFamily,
    execution::ExecutionCancellation,
    routing::EndpointAccess,
    transport::{
        NetworkPolicy, PooledTransport, TransportError, TransportErrorKind, TransportLimits,
        TransportRequest, TransportResponse, TransportResponseHead, TransportTimeouts,
    },
};
use wokcore_protocols::{
    UpstreamLimits,
    canonical::{GatewayError, RetryClass},
    upstream::{
        UpstreamAdapter, UpstreamOperation as ProtocolOperation, UpstreamProtocol,
        UpstreamStreamDecoder,
    },
};
use wokcore_server::data_plane::{
    SafeUpstreamRequestId, UpstreamExecutionFailure, UpstreamExecutionRequest,
    UpstreamExecutionResponse, UpstreamExecutionResult, UpstreamExecutionStream, UpstreamExecutor,
    UpstreamFailureKind, UpstreamOperation,
};
use wokcore_storage::{SecretStore, StorageError};

#[derive(Clone)]
pub struct ProductionUpstreamExecutor {
    transport: PooledTransport,
    secrets: Arc<dyn SecretResolver>,
}

impl ProductionUpstreamExecutor {
    pub fn new(transport: PooledTransport, secrets: Arc<dyn SecretResolver>) -> Self {
        Self { transport, secrets }
    }
}

impl std::fmt::Debug for ProductionUpstreamExecutor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProductionUpstreamExecutor")
            .field("transport", &self.transport)
            .field("secrets", &"[redacted]")
            .finish()
    }
}

#[async_trait]
impl UpstreamExecutor for ProductionUpstreamExecutor {
    async fn execute(
        &self,
        request: UpstreamExecutionRequest,
        cancellation: ExecutionCancellation,
    ) -> UpstreamExecutionResult {
        self.execute_inner(request, cancellation)
            .await
            .unwrap_or_else(UpstreamExecutionResult::Failed)
    }
}

impl ProductionUpstreamExecutor {
    async fn execute_inner(
        &self,
        request: UpstreamExecutionRequest,
        cancellation: ExecutionCancellation,
    ) -> Result<UpstreamExecutionResult, UpstreamExecutionFailure> {
        let protocol =
            protocol_for(request.adapter()).ok_or_else(|| failure(UpstreamFailureKind::Policy))?;
        let endpoint =
            Url::parse(request.endpoint()).map_err(|_| failure(UpstreamFailureKind::Policy))?;
        let adapter = UpstreamAdapter::new(protocol, endpoint, UpstreamLimits::default())
            .map_err(|_| failure(UpstreamFailureKind::Policy))?;
        let operation = match request.operation() {
            UpstreamOperation::Text => ProtocolOperation::Text,
            UpstreamOperation::CountTokens => ProtocolOperation::CountTokens,
        };
        let outbound = adapter
            .build_request(request.canonical(), operation)
            .map_err(map_request_error)?;
        let mut transport_request = TransportRequest::post(
            outbound.url,
            outbound.body,
            outbound.stream,
            network_policy(request.endpoint_access()),
        );
        for (name, value) in outbound.headers {
            transport_request = transport_request
                .with_header(&name, &value)
                .map_err(map_transport_error)?;
        }
        if let Some(authorization) =
            resolve_outbound_auth(request.auth(), request.adapter(), self.secrets.as_ref())
                .await
                .map_err(|_| failure(UpstreamFailureKind::InvalidCredentials))?
        {
            let (name, value) = authorization.into_parts();
            transport_request = transport_request
                .with_sensitive_header(name, value)
                .map_err(map_transport_error)?;
        }

        let response = self
            .transport
            .execute(transport_request, &cancellation)
            .await
            .map_err(map_transport_error)?;
        match response {
            TransportResponse::Complete(response) => {
                let upstream_request_id = upstream_request_id(response.head());
                if !(200..=299).contains(&response.head().status()) {
                    return Err(http_failure(response.head(), upstream_request_id));
                }
                match request.operation() {
                    UpstreamOperation::CountTokens => {
                        let count = adapter
                            .decode_token_count(response.body())
                            .map_err(map_gateway_error)?;
                        let response = with_response_request_id(
                            UpstreamExecutionResponse::token_count(count),
                            upstream_request_id,
                        );
                        Ok(UpstreamExecutionResult::Succeeded(response))
                    }
                    UpstreamOperation::Text => {
                        let events = adapter
                            .decode_response(
                                request.canonical().request_id.clone(),
                                response.body(),
                            )
                            .map_err(map_gateway_error)?;
                        let response = UpstreamExecutionResponse::events(events, unix_seconds())
                            .map_err(|_| failure(UpstreamFailureKind::MalformedResponse))?;
                        let response = with_response_request_id(response, upstream_request_id);
                        Ok(UpstreamExecutionResult::Succeeded(response))
                    }
                }
            }
            TransportResponse::Streaming(response) => {
                let upstream_request_id = upstream_request_id(response.head());
                if !(200..=299).contains(&response.head().status()) {
                    return Err(http_failure(response.head(), upstream_request_id));
                }
                if request.operation() != UpstreamOperation::Text {
                    return Err(failure(UpstreamFailureKind::MalformedResponse));
                }
                let (sender, stream) = UpstreamExecutionStream::channel(unix_seconds());
                let stream = with_stream_request_id(stream, upstream_request_id);
                let decoder = adapter.stream_decoder(request.canonical().request_id.clone());
                tokio::spawn(pump_stream(response, decoder, sender, cancellation));
                Ok(UpstreamExecutionResult::Streaming(stream))
            }
        }
    }
}

async fn pump_stream(
    mut response: wokcore_engine::transport::StreamingTransportResponse,
    mut decoder: UpstreamStreamDecoder,
    sender: wokcore_server::data_plane::UpstreamStreamSender,
    cancellation: ExecutionCancellation,
) {
    loop {
        match response.next_chunk(&cancellation).await {
            Ok(Some(chunk)) => match decoder.push(&chunk) {
                Ok(events) => {
                    for event in events {
                        if sender.send_event(event).await.is_err() {
                            return;
                        }
                    }
                }
                Err(error) => {
                    let _ = sender.send_failure(map_gateway_error(error)).await;
                    return;
                }
            },
            Ok(None) => {
                match decoder.finish() {
                    Ok(events) => {
                        for event in events {
                            if sender.send_event(event).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        let _ = sender.send_failure(map_gateway_error(error)).await;
                    }
                }
                return;
            }
            Err(error) => {
                let _ = sender.send_failure(map_transport_error(error)).await;
                return;
            }
        }
    }
}

fn protocol_for(adapter: AdapterFamily) -> Option<UpstreamProtocol> {
    match adapter {
        AdapterFamily::OpenAiResponses => Some(UpstreamProtocol::OpenAiResponses),
        AdapterFamily::OpenAiChat => Some(UpstreamProtocol::OpenAiChat),
        AdapterFamily::Anthropic => Some(UpstreamProtocol::Anthropic),
        AdapterFamily::Google => Some(UpstreamProtocol::Gemini),
        AdapterFamily::AzureOpenAi => Some(UpstreamProtocol::AzureOpenAi),
        AdapterFamily::Cursor | AdapterFamily::Kiro | AdapterFamily::MimoFree => None,
    }
}

const fn network_policy(access: EndpointAccess) -> NetworkPolicy {
    match access {
        EndpointAccess::PublicOnly => NetworkPolicy::PublicOnly,
        EndpointAccess::PrivateAllowed => NetworkPolicy::PrivateAllowed,
        EndpointAccess::LoopbackOnly => NetworkPolicy::LoopbackOnly,
    }
}

fn http_failure(
    head: &TransportResponseHead,
    upstream_request_id: Option<SafeUpstreamRequestId>,
) -> UpstreamExecutionFailure {
    let status = head.status();
    let kind = match status {
        401 | 403 => UpstreamFailureKind::InvalidCredentials,
        408 | 504 => UpstreamFailureKind::Timeout,
        429 => UpstreamFailureKind::RateLimited,
        500..=599 => UpstreamFailureKind::Server,
        _ => UpstreamFailureKind::InvalidRequest,
    };
    let mut failure = failure(kind).with_status(status);
    if status == 429
        && let Some(seconds) = head
            .header("retry-after")
            .and_then(|value| value.trim().parse::<u64>().ok())
    {
        failure = failure.with_retry_after_ms(seconds.saturating_mul(1_000));
    }
    with_failure_request_id(failure, upstream_request_id)
}

fn map_transport_error(error: TransportError) -> UpstreamExecutionFailure {
    let kind = match error.kind() {
        TransportErrorKind::Cancelled => UpstreamFailureKind::Cancelled,
        TransportErrorKind::ConnectTimeout
        | TransportErrorKind::HeaderTimeout
        | TransportErrorKind::IdleTimeout
        | TransportErrorKind::TotalTimeout => UpstreamFailureKind::Timeout,
        TransportErrorKind::ResponseTooLarge | TransportErrorKind::InvalidResponse => {
            UpstreamFailureKind::MalformedResponse
        }
        TransportErrorKind::InvalidRequest => UpstreamFailureKind::InvalidRequest,
        TransportErrorKind::Policy => UpstreamFailureKind::Policy,
        TransportErrorKind::Transport => UpstreamFailureKind::Transport,
    };
    failure(kind)
}

fn map_gateway_error(error: GatewayError) -> UpstreamExecutionFailure {
    let kind = match error.code() {
        "upstream_auth" => UpstreamFailureKind::InvalidCredentials,
        "rate_limited" => UpstreamFailureKind::RateLimited,
        "upstream_error" => UpstreamFailureKind::Server,
        "upstream_unavailable" => UpstreamFailureKind::Transport,
        "invalid_request" => UpstreamFailureKind::MalformedResponse,
        "unsupported_capability" | "model_not_found" | "no_executor" => UpstreamFailureKind::Policy,
        _ => match error.retry_class() {
            RetryClass::RefreshCredentials => UpstreamFailureKind::InvalidCredentials,
            RetryClass::AfterDelay => UpstreamFailureKind::RateLimited,
            RetryClass::BeforeFirstEvent => UpstreamFailureKind::Transport,
            RetryClass::Never => UpstreamFailureKind::MalformedResponse,
        },
    };
    failure(kind)
}

fn map_request_error(error: GatewayError) -> UpstreamExecutionFailure {
    let kind = match error.code() {
        "invalid_request" => UpstreamFailureKind::InvalidRequest,
        "unsupported_capability" | "model_not_found" | "no_executor" => UpstreamFailureKind::Policy,
        _ => match error.retry_class() {
            RetryClass::RefreshCredentials => UpstreamFailureKind::InvalidCredentials,
            RetryClass::AfterDelay => UpstreamFailureKind::RateLimited,
            RetryClass::BeforeFirstEvent => UpstreamFailureKind::Transport,
            RetryClass::Never => UpstreamFailureKind::InvalidRequest,
        },
    };
    failure(kind)
}

const fn failure(kind: UpstreamFailureKind) -> UpstreamExecutionFailure {
    UpstreamExecutionFailure::new(kind)
}

fn upstream_request_id(head: &TransportResponseHead) -> Option<SafeUpstreamRequestId> {
    [
        "x-request-id",
        "request-id",
        "apim-request-id",
        "x-goog-request-id",
    ]
    .into_iter()
    .find_map(|name| head.header(name))
    .and_then(|value| SafeUpstreamRequestId::new(value.to_owned()).ok())
}

fn with_response_request_id(
    response: UpstreamExecutionResponse,
    request_id: Option<SafeUpstreamRequestId>,
) -> UpstreamExecutionResponse {
    if let Some(request_id) = request_id {
        response.with_upstream_request_id(request_id)
    } else {
        response
    }
}

fn with_stream_request_id(
    stream: UpstreamExecutionStream,
    request_id: Option<SafeUpstreamRequestId>,
) -> UpstreamExecutionStream {
    if let Some(request_id) = request_id {
        stream.with_upstream_request_id(request_id)
    } else {
        stream
    }
}

fn with_failure_request_id(
    failure: UpstreamExecutionFailure,
    request_id: Option<SafeUpstreamRequestId>,
) -> UpstreamExecutionFailure {
    if let Some(request_id) = request_id {
        failure.with_upstream_request_id(request_id)
    } else {
        failure
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[derive(Clone)]
struct StorageSecretResolver {
    store: Arc<dyn SecretStore>,
}

#[async_trait]
impl SecretResolver for StorageSecretResolver {
    async fn resolve(
        &self,
        secret_ref: &wokcore_core::secret::SecretRef,
    ) -> Result<SecretString, SecretResolutionError> {
        self.store
            .get(secret_ref)
            .await
            .map_err(map_storage_secret_error)
    }
}

fn map_storage_secret_error(_: StorageError) -> SecretResolutionError {
    SecretResolutionError
}

pub fn production_upstream_executor(
    store: Arc<dyn SecretStore>,
) -> Result<Arc<dyn UpstreamExecutor>, ProductionUpstreamBuildError> {
    let transport = PooledTransport::new(TransportTimeouts::default(), TransportLimits::default())
        .map_err(|_| ProductionUpstreamBuildError)?;
    Ok(Arc::new(ProductionUpstreamExecutor::new(
        transport,
        Arc::new(StorageSecretResolver { store }),
    )))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("the production upstream runtime could not be initialized")]
pub struct ProductionUpstreamBuildError;
