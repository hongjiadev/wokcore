use std::{
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use secrecy::SecretString;
use tokio::{io::AsyncReadExt, sync::mpsc};
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
    images::ImageEditMetadata,
    upstream::{
        UpstreamAdapter, UpstreamOperation as ProtocolOperation, UpstreamProtocol,
        UpstreamStreamDecoder,
    },
};
use wokcore_server::data_plane::{
    ImageExecutionInput, ImageExecutionRequest, ImageExecutionResponse, ImageExecutionResult,
    ImageInputFile, SafeUpstreamRequestId, UpstreamExecutionFailure, UpstreamExecutionRequest,
    UpstreamExecutionResponse, UpstreamExecutionResult, UpstreamExecutionStream, UpstreamExecutor,
    UpstreamFailureKind, UpstreamOperation,
};
use wokcore_storage::{SecretStore, StorageError};

const IMAGE_MULTIPART_WIRE_LIMIT: usize = 51 * 1024 * 1024;
const IMAGE_RESPONSE_LIMIT: usize = 50 * 1024 * 1024;
const MULTIPART_CHANNEL_CAPACITY: usize = 1;
const MULTIPART_FILE_CHUNK_BYTES: usize = 64 * 1024;

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

    async fn execute_image(
        &self,
        request: ImageExecutionRequest,
        cancellation: ExecutionCancellation,
    ) -> ImageExecutionResult {
        self.execute_image_inner(request, cancellation)
            .await
            .unwrap_or_else(ImageExecutionResult::Failed)
    }
}

impl ProductionUpstreamExecutor {
    async fn execute_image_inner(
        &self,
        request: ImageExecutionRequest,
        cancellation: ExecutionCancellation,
    ) -> Result<ImageExecutionResult, UpstreamExecutionFailure> {
        let operation = match request.input() {
            ImageExecutionInput::Generation(_) => ImageOperation::Generation,
            ImageExecutionInput::Edit(_) => ImageOperation::Edit,
        };
        let endpoint = image_endpoint(
            request.endpoint(),
            request.adapter(),
            operation,
            request.model(),
        )?;
        let authorization =
            resolve_outbound_auth(request.auth(), request.adapter(), self.secrets.as_ref())
                .await
                .map_err(|_| failure(UpstreamFailureKind::InvalidCredentials))?;
        let network_policy = network_policy(request.endpoint_access());
        let request_id = request.request_id().to_owned();
        let model = request.model().to_owned();
        let mut transport_request = match request.into_input() {
            ImageExecutionInput::Generation(input) => {
                let body = input.encode_with_model(&model).map_err(map_request_error)?;
                TransportRequest::post(endpoint, body, false, network_policy)
                    .with_header("content-type", "application/json")
                    .map_err(map_transport_error)?
            }
            ImageExecutionInput::Edit(input) => {
                let upload = multipart_upload(
                    input.into_parts(),
                    &model,
                    &request_id,
                    cancellation.clone(),
                )?;
                TransportRequest::post_stream(
                    endpoint,
                    upload.stream,
                    upload.content_length,
                    false,
                    network_policy,
                )
                .map_err(map_transport_error)?
                .with_header("content-type", &upload.content_type)
                .map_err(map_transport_error)?
            }
        }
        .with_header("accept", "application/json")
        .map_err(map_transport_error)?
        .with_body_limits(IMAGE_MULTIPART_WIRE_LIMIT, IMAGE_RESPONSE_LIMIT)
        .map_err(map_transport_error)?;
        if let Some(authorization) = authorization {
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
        let TransportResponse::Complete(response) = response else {
            return Err(failure(UpstreamFailureKind::MalformedResponse));
        };
        let upstream_request_id = upstream_request_id(response.head());
        if !(200..=299).contains(&response.head().status()) {
            return Err(http_failure(response.head(), upstream_request_id));
        }
        let mut response = ImageExecutionResponse::json(response.into_body())
            .map_err(|_| failure(UpstreamFailureKind::MalformedResponse))?;
        if let Some(request_id) = upstream_request_id {
            response = response.with_upstream_request_id(request_id);
        }
        Ok(ImageExecutionResult::Succeeded(response))
    }

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

#[derive(Clone, Copy)]
enum ImageOperation {
    Generation,
    Edit,
}

fn image_endpoint(
    endpoint: &str,
    adapter: AdapterFamily,
    operation: ImageOperation,
    model: &str,
) -> Result<Url, UpstreamExecutionFailure> {
    let mut endpoint = Url::parse(endpoint).map_err(|_| failure(UpstreamFailureKind::Policy))?;
    if !matches!(endpoint.scheme(), "http" | "https")
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(failure(UpstreamFailureKind::Policy));
    }
    if !endpoint.path().ends_with('/') {
        endpoint.set_path(&format!("{}/", endpoint.path()));
    }
    let operation = match operation {
        ImageOperation::Generation => "generations",
        ImageOperation::Edit => "edits",
    };
    match adapter {
        AdapterFamily::OpenAiResponses | AdapterFamily::OpenAiChat => endpoint
            .join(&format!("images/{operation}"))
            .map_err(|_| failure(UpstreamFailureKind::Policy)),
        AdapterFamily::AzureOpenAi => {
            let ends_with_openai = endpoint.path().trim_end_matches('/').ends_with("/openai");
            {
                let mut segments = endpoint
                    .path_segments_mut()
                    .map_err(|_| failure(UpstreamFailureKind::Policy))?;
                segments.pop_if_empty();
                if !ends_with_openai {
                    segments.push("openai");
                }
                segments
                    .push("deployments")
                    .push(model)
                    .push("images")
                    .push(operation);
            }
            endpoint
                .query_pairs_mut()
                .append_pair("api-version", "2024-10-21");
            Ok(endpoint)
        }
        AdapterFamily::Anthropic
        | AdapterFamily::Google
        | AdapterFamily::Cursor
        | AdapterFamily::Kiro
        | AdapterFamily::MimoFree => Err(failure(UpstreamFailureKind::Policy)),
    }
}

struct MultipartUpload {
    stream: MultipartStream,
    content_length: usize,
    content_type: String,
}

enum MultipartSegment {
    Bytes(Bytes),
    File(ImageInputFile),
}

fn multipart_upload(
    (metadata, files): (ImageEditMetadata, Vec<ImageInputFile>),
    routed_model: &str,
    request_id: &str,
    cancellation: ExecutionCancellation,
) -> Result<MultipartUpload, UpstreamExecutionFailure> {
    let boundary = format!("wokcore-{request_id}");
    let mut segments = Vec::new();
    let mut content_length = 0_usize;
    for (name, value) in metadata.fields() {
        let value = if name == "model" { routed_model } else { value };
        let bytes = Bytes::from(format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        ));
        add_multipart_length(&mut content_length, bytes.len())?;
        segments.push(MultipartSegment::Bytes(bytes));
    }
    for file in files {
        let header = Bytes::from(format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",
            file.field_name(),
            file.file_name(),
            file.content_type(),
        ));
        add_multipart_length(&mut content_length, header.len())?;
        add_multipart_length(
            &mut content_length,
            usize::try_from(file.length())
                .map_err(|_| failure(UpstreamFailureKind::InvalidRequest))?,
        )?;
        add_multipart_length(&mut content_length, 2)?;
        segments.push(MultipartSegment::Bytes(header));
        segments.push(MultipartSegment::File(file));
        segments.push(MultipartSegment::Bytes(Bytes::from_static(b"\r\n")));
    }
    let closing = Bytes::from(format!("--{boundary}--\r\n"));
    add_multipart_length(&mut content_length, closing.len())?;
    segments.push(MultipartSegment::Bytes(closing));
    if content_length > IMAGE_MULTIPART_WIRE_LIMIT {
        return Err(failure(UpstreamFailureKind::InvalidRequest));
    }
    let (sender, receiver) = mpsc::channel(MULTIPART_CHANNEL_CAPACITY);
    tokio::spawn(pump_multipart(segments, sender, cancellation));
    Ok(MultipartUpload {
        stream: MultipartStream { receiver },
        content_length,
        content_type: format!("multipart/form-data; boundary={boundary}"),
    })
}

fn add_multipart_length(total: &mut usize, length: usize) -> Result<(), UpstreamExecutionFailure> {
    *total = total
        .checked_add(length)
        .ok_or_else(|| failure(UpstreamFailureKind::InvalidRequest))?;
    Ok(())
}

struct MultipartStream {
    receiver: mpsc::Receiver<Result<Bytes, io::Error>>,
}

impl Stream for MultipartStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(context)
    }
}

async fn pump_multipart(
    segments: Vec<MultipartSegment>,
    sender: mpsc::Sender<Result<Bytes, io::Error>>,
    cancellation: ExecutionCancellation,
) {
    for segment in segments {
        match segment {
            MultipartSegment::Bytes(bytes) => {
                if !send_multipart(&sender, Ok(bytes), &cancellation).await {
                    return;
                }
            }
            MultipartSegment::File(file) => {
                let mut reader = match file.into_reader().await {
                    Ok(reader) => reader,
                    Err(error) => {
                        let _ = send_multipart(&sender, Err(error), &cancellation).await;
                        return;
                    }
                };
                let mut buffer = vec![0_u8; MULTIPART_FILE_CHUNK_BYTES];
                loop {
                    let read = match reader.read(&mut buffer).await {
                        Ok(read) => read,
                        Err(error) => {
                            let _ = send_multipart(&sender, Err(error), &cancellation).await;
                            return;
                        }
                    };
                    if read == 0 {
                        break;
                    }
                    if !send_multipart(
                        &sender,
                        Ok(Bytes::copy_from_slice(&buffer[..read])),
                        &cancellation,
                    )
                    .await
                    {
                        return;
                    }
                }
            }
        }
    }
}

async fn send_multipart(
    sender: &mpsc::Sender<Result<Bytes, io::Error>>,
    item: Result<Bytes, io::Error>,
    cancellation: &ExecutionCancellation,
) -> bool {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => false,
        result = sender.send(item) => result.is_ok(),
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
    let transport = PooledTransport::new(
        TransportTimeouts::default(),
        TransportLimits {
            max_request_body_bytes: IMAGE_MULTIPART_WIRE_LIMIT,
            max_response_body_bytes: IMAGE_RESPONSE_LIMIT,
            ..TransportLimits::default()
        },
    )
    .map_err(|_| ProductionUpstreamBuildError)?;
    Ok(Arc::new(ProductionUpstreamExecutor::new(
        transport,
        Arc::new(StorageSecretResolver { store }),
    )))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("the production upstream runtime could not be initialized")]
pub struct ProductionUpstreamBuildError;
