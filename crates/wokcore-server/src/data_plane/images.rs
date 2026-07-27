use std::{
    fmt, io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{SystemTime, UNIX_EPOCH},
};

use tokio::io::{AsyncRead, ReadBuf};
use wokcore_core::{
    config::AccountAuthConfig,
    id::{AccountId, ProviderId},
};
use wokcore_engine::{
    accounts::{AccountCandidate, AccountObservation},
    catalog::AdapterFamily,
    execution::ExecutionCancellation,
    routing::{EndpointAccess, RouteError, RouteRequest},
};
use wokcore_protocols::{
    canonical::GatewayError,
    images::{ImageEditMetadata, ImageGenerationRequest, validate_image_response},
};

use crate::{ServerState, auth::AuthorizedClient};

use super::{SafeUpstreamRequestId, UpstreamExecutionFailure, UpstreamFailureKind};

const ACCOUNTLESS_ACCOUNT_ID: &str = "accountless";
const IMAGE_RESPONSE_LIMIT: usize = 50 * 1024 * 1024;

pub struct ImageInputFile {
    field_name: String,
    file_name: String,
    content_type: String,
    length: u64,
    path: tempfile::TempPath,
}

impl ImageInputFile {
    pub fn from_named_temp(
        field_name: impl Into<String>,
        file_name: impl Into<String>,
        content_type: impl Into<String>,
        file: tempfile::NamedTempFile,
    ) -> Result<Self, GatewayError> {
        let field_name = field_name.into();
        let file_name = file_name.into();
        let content_type = content_type.into();
        let length = file
            .as_file()
            .metadata()
            .map_err(|_| GatewayError::invalid_request())?
            .len();
        if !matches!(field_name.as_str(), "image" | "mask")
            || file_name.is_empty()
            || file_name.len() > 128
            || file_name
                .chars()
                .any(|character| character.is_control() || matches!(character, '"' | '\\' | '/'))
            || !matches!(
                content_type.as_str(),
                "image/png" | "image/jpeg" | "image/webp" | "application/octet-stream"
            )
            || length > 20 * 1024 * 1024
        {
            return Err(GatewayError::invalid_request());
        }
        let (_, path) = file.into_parts();
        Ok(Self::new(field_name, file_name, content_type, length, path))
    }

    pub(crate) fn new(
        field_name: String,
        file_name: String,
        content_type: String,
        length: u64,
        path: tempfile::TempPath,
    ) -> Self {
        Self {
            field_name,
            file_name,
            content_type,
            length,
            path,
        }
    }

    pub fn field_name(&self) -> &str {
        &self.field_name
    }

    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    pub fn content_type(&self) -> &str {
        &self.content_type
    }

    pub const fn length(&self) -> u64 {
        self.length
    }

    pub async fn open(&self) -> io::Result<tokio::fs::File> {
        tokio::fs::File::open(&self.path).await
    }

    pub async fn into_reader(self) -> io::Result<ImageInputReader> {
        let file = tokio::fs::File::open(&self.path).await?;
        Ok(ImageInputReader {
            file,
            _path: self.path,
        })
    }
}

impl fmt::Debug for ImageInputFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImageInputFile")
            .field("field_name", &self.field_name)
            .field("file_name", &"[redacted]")
            .field("content_type", &self.content_type)
            .field("length", &self.length)
            .field("path", &"[redacted]")
            .finish()
    }
}

pub struct ImageInputReader {
    file: tokio::fs::File,
    _path: tempfile::TempPath,
}

impl AsyncRead for ImageInputReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.file).poll_read(context, buffer)
    }
}

impl fmt::Debug for ImageInputReader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImageInputReader")
            .field("path", &"[redacted]")
            .finish_non_exhaustive()
    }
}

pub struct ImageEditRequest {
    metadata: ImageEditMetadata,
    files: Vec<ImageInputFile>,
}

impl ImageEditRequest {
    pub fn new(
        metadata: ImageEditMetadata,
        files: Vec<ImageInputFile>,
    ) -> Result<Self, GatewayError> {
        let images = files
            .iter()
            .filter(|file| file.field_name() == "image")
            .count();
        let masks = files
            .iter()
            .filter(|file| file.field_name() == "mask")
            .count();
        if images == 0
            || images > 16
            || masks > 1
            || files.len() != images.saturating_add(masks)
            || files.iter().any(|file| {
                !matches!(
                    file.content_type(),
                    "image/png" | "image/jpeg" | "image/webp" | "application/octet-stream"
                )
            })
        {
            return Err(GatewayError::invalid_request());
        }
        Ok(Self { metadata, files })
    }

    pub fn metadata(&self) -> &ImageEditMetadata {
        &self.metadata
    }

    pub fn files(&self) -> &[ImageInputFile] {
        &self.files
    }

    pub fn into_parts(self) -> (ImageEditMetadata, Vec<ImageInputFile>) {
        (self.metadata, self.files)
    }
}

impl fmt::Debug for ImageEditRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImageEditRequest")
            .field("metadata", &self.metadata)
            .field("file_count", &self.files.len())
            .field(
                "file_bytes",
                &self.files.iter().map(ImageInputFile::length).sum::<u64>(),
            )
            .finish()
    }
}

#[derive(Debug)]
pub enum ImageExecutionInput {
    Generation(ImageGenerationRequest),
    Edit(ImageEditRequest),
}

impl ImageExecutionInput {
    pub fn model(&self) -> &str {
        match self {
            Self::Generation(request) => request.model(),
            Self::Edit(request) => request.metadata().model(),
        }
    }
}

pub struct ImageExecutionRequest {
    request_id: Arc<str>,
    provider_id: ProviderId,
    account_id: AccountId,
    adapter: AdapterFamily,
    endpoint: Arc<str>,
    endpoint_access: EndpointAccess,
    model: Arc<str>,
    auth: AccountAuthConfig,
    input: ImageExecutionInput,
}

impl ImageExecutionRequest {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        request_id: impl Into<String>,
        provider_id: ProviderId,
        account_id: AccountId,
        adapter: AdapterFamily,
        endpoint: impl Into<String>,
        endpoint_access: EndpointAccess,
        model: impl Into<String>,
        auth: AccountAuthConfig,
        input: ImageExecutionInput,
    ) -> Result<Self, GatewayError> {
        let request_id = request_id.into();
        let endpoint = endpoint.into();
        let model = model.into();
        if request_id.is_empty()
            || request_id.len() > 256
            || !request_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            || endpoint.is_empty()
            || endpoint.len() > 2_048
            || url::Url::parse(&endpoint).is_err()
            || model.is_empty()
            || model.len() > 256
            || model.chars().any(char::is_control)
        {
            return Err(GatewayError::invalid_request());
        }
        Ok(Self {
            request_id: request_id.into(),
            provider_id,
            account_id,
            adapter,
            endpoint: endpoint.into(),
            endpoint_access,
            model: model.into(),
            auth,
            input,
        })
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }

    pub fn account_id(&self) -> &AccountId {
        &self.account_id
    }

    pub const fn adapter(&self) -> AdapterFamily {
        self.adapter
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub const fn endpoint_access(&self) -> EndpointAccess {
        self.endpoint_access
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn auth(&self) -> &AccountAuthConfig {
        &self.auth
    }

    pub fn input(&self) -> &ImageExecutionInput {
        &self.input
    }

    pub fn into_input(self) -> ImageExecutionInput {
        self.input
    }
}

impl fmt::Debug for ImageExecutionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImageExecutionRequest")
            .field("request_id", &self.request_id)
            .field("provider_id", &self.provider_id)
            .field("account_id", &self.account_id)
            .field("adapter", &self.adapter)
            .field("endpoint", &"[redacted]")
            .field("endpoint_access", &self.endpoint_access)
            .field("model", &self.model)
            .field("input", &self.input)
            .finish()
    }
}

pub struct ImageExecutionResponse {
    body: Vec<u8>,
    upstream_request_id: Option<SafeUpstreamRequestId>,
}

impl ImageExecutionResponse {
    pub fn json(body: Vec<u8>) -> Result<Self, GatewayError> {
        if body.len() > IMAGE_RESPONSE_LIMIT {
            return Err(GatewayError::invalid_request());
        }
        validate_image_response(&body)?;
        Ok(Self {
            body,
            upstream_request_id: None,
        })
    }

    pub fn with_upstream_request_id(mut self, request_id: SafeUpstreamRequestId) -> Self {
        self.upstream_request_id = Some(request_id);
        self
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub fn into_body(self) -> Vec<u8> {
        self.body
    }

    pub fn upstream_request_id(&self) -> Option<&SafeUpstreamRequestId> {
        self.upstream_request_id.as_ref()
    }
}

impl fmt::Debug for ImageExecutionResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImageExecutionResponse")
            .field("body_bytes", &self.body.len())
            .field("upstream_request_id", &self.upstream_request_id)
            .finish()
    }
}

#[derive(Debug)]
pub enum ImageExecutionResult {
    Succeeded(ImageExecutionResponse),
    Failed(UpstreamExecutionFailure),
}

pub(crate) struct ExecutedImage {
    pub(crate) response: ImageExecutionResponse,
}

pub(crate) struct ImageExecutionError {
    pub(crate) error: GatewayError,
    pub(crate) upstream_request_id: Option<SafeUpstreamRequestId>,
}

pub(crate) async fn execute_image(
    state: &ServerState,
    authorized: &AuthorizedClient,
    request_id: &str,
    input: ImageExecutionInput,
) -> Result<ExecutedImage, ImageExecutionError> {
    let providers = state
        .providers
        .as_ref()
        .ok_or_else(|| image_error(GatewayError::no_executor(), None))?;
    let execution = providers.execution_snapshot();
    let route = execution
        .snapshot
        .route(&RouteRequest {
            provider: None,
            model: input.model().to_owned(),
            client_id: Some(authorized.client_id.clone()),
        })
        .map_err(|error| image_error(map_route_error(error), None))?;
    if !route.provider().capabilities().images {
        return Err(image_error(GatewayError::unsupported_capability(), None));
    }
    let executor = state
        .upstream_executor
        .as_ref()
        .cloned()
        .ok_or_else(|| image_error(GatewayError::no_executor(), None))?;
    let now_ms = unix_milliseconds();
    let (account_id, auth, tracked_health) = if route.provider().accounts().is_empty() {
        (
            AccountId::new(ACCOUNTLESS_ACCOUNT_ID)
                .expect("the accountless execution identity is valid"),
            AccountAuthConfig::Local,
            false,
        )
    } else {
        let mut selected = None;
        let mut authentications = Vec::new();
        for account in route.provider().accounts() {
            if !authentications.contains(&account.authentication()) {
                authentications.push(account.authentication());
            }
        }
        for authentication in authentications {
            let candidates = route
                .provider()
                .accounts()
                .iter()
                .filter(|account| account.authentication() == authentication)
                .map(|account| AccountCandidate::new(account.id(), authentication, 1))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| image_error(GatewayError::internal("candidate"), None))?;
            if let Ok(choice) =
                execution
                    .account_health
                    .select(&candidates, authentication, None, now_ms)
            {
                let account = route
                    .provider()
                    .accounts()
                    .iter()
                    .find(|account| account.id() == choice.account_id())
                    .expect("selected accounts originate from the route");
                selected = Some((account.id().clone(), account.auth().clone(), true));
                break;
            }
        }
        selected.ok_or_else(|| image_error(GatewayError::transport("no eligible account"), None))?
    };

    let cancellation = ExecutionCancellation::new();
    let cancel_on_drop = ImageCancelOnDrop(cancellation.clone());
    let request = ImageExecutionRequest::new(
        request_id,
        route.provider_id().clone(),
        account_id.clone(),
        route.provider().adapter(),
        route.provider().endpoint(),
        route.provider().endpoint_access(),
        route.model(),
        auth,
        input,
    )
    .map_err(|error| image_error(error, None))?;
    let result = executor.execute_image(request, cancellation).await;
    drop(cancel_on_drop);
    match result {
        ImageExecutionResult::Succeeded(response) => {
            if tracked_health {
                let _ = execution.account_health.observe(
                    &account_id,
                    AccountObservation::Success,
                    unix_milliseconds(),
                );
            }
            Ok(ExecutedImage { response })
        }
        ImageExecutionResult::Failed(failure) => {
            if tracked_health {
                let _ = execution.account_health.observe(
                    &account_id,
                    observation_for_failure(&failure),
                    unix_milliseconds(),
                );
            }
            Err(image_error(
                gateway_error_for_failure(&failure),
                failure.upstream_request_id().cloned(),
            ))
        }
    }
}

struct ImageCancelOnDrop(ExecutionCancellation);

impl Drop for ImageCancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn map_route_error(error: RouteError) -> GatewayError {
    match error {
        RouteError::NoRoute | RouteError::ProviderUnavailable => GatewayError::unknown_model(),
        RouteError::UnsupportedReasoningEffort => GatewayError::unsupported_capability(),
    }
}

fn observation_for_failure(failure: &UpstreamExecutionFailure) -> AccountObservation {
    match failure.kind() {
        UpstreamFailureKind::RateLimited => AccountObservation::RateLimited {
            retry_after_ms: failure.retry_after_ms(),
        },
        UpstreamFailureKind::Timeout
        | UpstreamFailureKind::Server
        | UpstreamFailureKind::Reset
        | UpstreamFailureKind::Transport => AccountObservation::TemporaryFailure {
            retry_after_ms: failure.retry_after_ms(),
        },
        UpstreamFailureKind::InvalidCredentials => AccountObservation::InvalidCredentials,
        UpstreamFailureKind::InvalidRequest | UpstreamFailureKind::MalformedResponse => {
            AccountObservation::InvalidRequest
        }
        UpstreamFailureKind::Policy | UpstreamFailureKind::Cancelled => {
            AccountObservation::PolicyRejected
        }
    }
}

fn gateway_error_for_failure(failure: &UpstreamExecutionFailure) -> GatewayError {
    match failure.kind() {
        UpstreamFailureKind::RateLimited => {
            GatewayError::rate_limited(failure.retry_after_ms().map(|delay| delay / 1_000))
        }
        UpstreamFailureKind::Server => GatewayError::upstream_5xx(failure.status().unwrap_or(500)),
        UpstreamFailureKind::Timeout
        | UpstreamFailureKind::Reset
        | UpstreamFailureKind::Transport
        | UpstreamFailureKind::Cancelled => GatewayError::transport("upstream transport"),
        UpstreamFailureKind::InvalidCredentials => {
            GatewayError::upstream_auth("upstream authentication")
        }
        UpstreamFailureKind::InvalidRequest => GatewayError::invalid_request(),
        UpstreamFailureKind::Policy => GatewayError::unsupported_capability(),
        UpstreamFailureKind::MalformedResponse => GatewayError::upstream_response(
            failure.status().unwrap_or(502),
            "malformed upstream response",
        ),
    }
}

fn image_error(
    error: GatewayError,
    upstream_request_id: Option<SafeUpstreamRequestId>,
) -> ImageExecutionError {
    ImageExecutionError {
        error,
        upstream_request_id,
    }
}

fn unix_milliseconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}
