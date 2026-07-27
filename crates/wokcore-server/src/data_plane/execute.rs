use std::{
    collections::BTreeMap,
    fmt,
    io::{self, Write},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use tokio::sync::mpsc;
use wokcore_core::{
    config::AccountAuthConfig,
    id::{AccountId, ProviderId},
};
use wokcore_engine::{
    catalog::AdapterFamily,
    execution::{
        AttemptBoundary, AttemptContext, AttemptFailure, AttemptFailureKind, AttemptResult,
        ExecutionCancellation, ExecutionCoordinator, ExecutionOutcome, ExecutionRequest,
        ExecutionRequestId, ExecutionTerminal, PreparedRequestBody, SystemExecutionClock,
        TokioRetryDelay, UpstreamAttempt,
    },
    retry::RetryPolicy,
    routing::{RouteDecision, RouteError, RouteRequest},
};
use wokcore_protocols::canonical::{
    CanonicalEvent, CanonicalRequest, GatewayError, PublicModelId, Usage,
};

use crate::{ServerState, auth::AuthorizedClient};

use super::JSON_BODY_LIMIT;

const MAX_UPSTREAM_REQUEST_ID_BYTES: usize = 256;
const MAX_UPSTREAM_EVENTS: usize = 4_096;
const MAX_UPSTREAM_METADATA_BYTES: usize = 1024 * 1024;
const MAX_UPSTREAM_IDENTIFIER_BYTES: usize = 512;
const ACCOUNTLESS_ACCOUNT_ID: &str = "accountless";
pub const UPSTREAM_STREAM_CHANNEL_CAPACITY: usize = 2;

#[derive(Clone, Eq, PartialEq)]
pub struct SafeUpstreamRequestId(Arc<str>);

impl SafeUpstreamRequestId {
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidSafeUpstreamRequestId> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_UPSTREAM_REQUEST_ID_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(InvalidSafeUpstreamRequestId);
        }
        Ok(Self(value.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SafeUpstreamRequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SafeUpstreamRequestId")
            .field(&self.0)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("the upstream request identifier is invalid")]
pub struct InvalidSafeUpstreamRequestId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpstreamOperation {
    Text,
    CountTokens,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpstreamFinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    FunctionCall,
    PauseTurn,
    Refusal,
    ModelContextWindowExceeded,
}

#[derive(Clone)]
pub struct UpstreamExecutionRequest {
    request_id: Arc<str>,
    attempt_ordinal: u8,
    operation: UpstreamOperation,
    provider_id: ProviderId,
    account_id: AccountId,
    adapter: AdapterFamily,
    endpoint: Arc<str>,
    model: Arc<str>,
    auth: AccountAuthConfig,
    canonical: Arc<CanonicalRequest>,
}

impl UpstreamExecutionRequest {
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub const fn attempt_ordinal(&self) -> u8 {
        self.attempt_ordinal
    }

    pub const fn operation(&self) -> UpstreamOperation {
        self.operation
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

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn auth(&self) -> &AccountAuthConfig {
        &self.auth
    }

    pub fn canonical(&self) -> &CanonicalRequest {
        &self.canonical
    }
}

impl fmt::Debug for UpstreamExecutionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UpstreamExecutionRequest")
            .field("request_id", &self.request_id)
            .field("attempt_ordinal", &self.attempt_ordinal)
            .field("operation", &self.operation)
            .field("provider_id", &self.provider_id)
            .field("account_id", &self.account_id)
            .field("adapter", &self.adapter)
            .field("endpoint", &"[redacted]")
            .field("model", &self.model)
            .field("canonical", &self.canonical)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum UpstreamExecutionOutput {
    Events(Vec<CanonicalEvent>),
    TokenCount(u64),
}

#[derive(Clone)]
pub struct UpstreamExecutionResponse {
    output: UpstreamExecutionOutput,
    created_at: u64,
    finish_reason: UpstreamFinishReason,
    stop_sequence: Option<String>,
    thinking_signatures: BTreeMap<String, String>,
    upstream_request_id: Option<SafeUpstreamRequestId>,
}

impl UpstreamExecutionResponse {
    pub fn events(
        events: Vec<CanonicalEvent>,
        created_at: u64,
    ) -> Result<Self, InvalidUpstreamExecutionResponse> {
        let created = events
            .iter()
            .filter(|event| matches!(event, CanonicalEvent::Created { .. }))
            .count();
        let usage = events
            .iter()
            .filter(|event| matches!(event, CanonicalEvent::Usage(_)))
            .count();
        let completed = events
            .iter()
            .filter(|event| matches!(event, CanonicalEvent::Completed))
            .count();
        if events.len() > MAX_UPSTREAM_EVENTS
            || !matches!(events.first(), Some(CanonicalEvent::Created { .. }))
            || !matches!(events.last(), Some(CanonicalEvent::Completed))
            || created != 1
            || usage != 1
            || completed != 1
            || events
                .iter()
                .any(|event| matches!(event, CanonicalEvent::Failed(_)))
            || !json_fits(&events, JSON_BODY_LIMIT)
        {
            return Err(InvalidUpstreamExecutionResponse);
        }
        Ok(Self {
            output: UpstreamExecutionOutput::Events(events),
            created_at,
            finish_reason: UpstreamFinishReason::Stop,
            stop_sequence: None,
            thinking_signatures: BTreeMap::new(),
            upstream_request_id: None,
        })
    }

    pub fn token_count(input_tokens: u64) -> Self {
        Self {
            output: UpstreamExecutionOutput::TokenCount(input_tokens),
            created_at: 0,
            finish_reason: UpstreamFinishReason::Stop,
            stop_sequence: None,
            thinking_signatures: BTreeMap::new(),
            upstream_request_id: None,
        }
    }

    pub fn with_upstream_request_id(mut self, request_id: SafeUpstreamRequestId) -> Self {
        self.upstream_request_id = Some(request_id);
        self
    }

    pub fn with_finish_reason(mut self, finish_reason: UpstreamFinishReason) -> Self {
        self.finish_reason = finish_reason;
        self
    }

    pub fn with_stop_sequence(
        mut self,
        stop_sequence: impl Into<String>,
    ) -> Result<Self, InvalidUpstreamExecutionResponse> {
        let stop_sequence = stop_sequence.into();
        if !json_fits(&stop_sequence, MAX_UPSTREAM_METADATA_BYTES) {
            return Err(InvalidUpstreamExecutionResponse);
        }
        self.stop_sequence = Some(stop_sequence);
        Ok(self)
    }

    pub fn with_thinking_signatures(
        mut self,
        thinking_signatures: BTreeMap<String, String>,
    ) -> Result<Self, InvalidUpstreamExecutionResponse> {
        if thinking_signatures.len() > MAX_UPSTREAM_EVENTS
            || !json_fits(&thinking_signatures, MAX_UPSTREAM_METADATA_BYTES)
        {
            return Err(InvalidUpstreamExecutionResponse);
        }
        self.thinking_signatures = thinking_signatures;
        Ok(self)
    }

    pub fn output(&self) -> &UpstreamExecutionOutput {
        &self.output
    }

    pub const fn created_at(&self) -> u64 {
        self.created_at
    }

    pub const fn finish_reason(&self) -> UpstreamFinishReason {
        self.finish_reason
    }

    pub fn stop_sequence(&self) -> Option<&str> {
        self.stop_sequence.as_deref()
    }

    pub fn thinking_signatures(&self) -> &BTreeMap<String, String> {
        &self.thinking_signatures
    }

    pub fn upstream_request_id(&self) -> Option<&SafeUpstreamRequestId> {
        self.upstream_request_id.as_ref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("the upstream execution response exceeds its bounds")]
pub struct InvalidUpstreamExecutionResponse;

impl fmt::Debug for UpstreamExecutionResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (output_kind, output_items) = match &self.output {
            UpstreamExecutionOutput::Events(events) => ("events", events.len()),
            UpstreamExecutionOutput::TokenCount(_) => ("token_count", 1),
        };
        formatter
            .debug_struct("UpstreamExecutionResponse")
            .field("output_kind", &output_kind)
            .field("output_items", &output_items)
            .field("created_at", &self.created_at)
            .field("finish_reason", &self.finish_reason)
            .field("has_stop_sequence", &self.stop_sequence.is_some())
            .field("thinking_signature_count", &self.thinking_signatures.len())
            .field("upstream_request_id", &self.upstream_request_id)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpstreamFailureKind {
    Timeout,
    Cancelled,
    MalformedResponse,
    RateLimited,
    Server,
    Reset,
    InvalidCredentials,
    InvalidRequest,
    Policy,
    Transport,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpstreamExecutionFailure {
    kind: UpstreamFailureKind,
    status: Option<u16>,
    retry_after_ms: Option<u64>,
    upstream_request_id: Option<SafeUpstreamRequestId>,
}

impl UpstreamExecutionFailure {
    pub const fn new(kind: UpstreamFailureKind) -> Self {
        Self {
            kind,
            status: None,
            retry_after_ms: None,
            upstream_request_id: None,
        }
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = (100..=599).contains(&status).then_some(status);
        self
    }

    pub const fn with_retry_after_ms(mut self, retry_after_ms: u64) -> Self {
        self.retry_after_ms = Some(retry_after_ms);
        self
    }

    pub fn with_upstream_request_id(mut self, request_id: SafeUpstreamRequestId) -> Self {
        self.upstream_request_id = Some(request_id);
        self
    }

    pub const fn kind(&self) -> UpstreamFailureKind {
        self.kind
    }

    pub const fn status(&self) -> Option<u16> {
        self.status
    }

    pub const fn retry_after_ms(&self) -> Option<u64> {
        self.retry_after_ms
    }

    pub fn upstream_request_id(&self) -> Option<&SafeUpstreamRequestId> {
        self.upstream_request_id.as_ref()
    }
}

#[derive(Debug)]
pub enum UpstreamExecutionResult {
    Succeeded(UpstreamExecutionResponse),
    Streaming(UpstreamExecutionStream),
    Failed(UpstreamExecutionFailure),
}

#[derive(Clone)]
pub struct UpstreamStreamSender {
    inner: mpsc::Sender<UpstreamStreamItem>,
}

impl UpstreamStreamSender {
    pub async fn send_event(&self, event: CanonicalEvent) -> Result<(), UpstreamStreamSendError> {
        if !stream_event_fits(&event) {
            return Err(UpstreamStreamSendError::InvalidEvent);
        }
        self.inner
            .send(UpstreamStreamItem::Event(event))
            .await
            .map_err(|_| UpstreamStreamSendError::Closed)
    }

    pub async fn send_failure(
        &self,
        failure: UpstreamExecutionFailure,
    ) -> Result<(), UpstreamStreamSendError> {
        self.inner
            .send(UpstreamStreamItem::Failure(failure))
            .await
            .map_err(|_| UpstreamStreamSendError::Closed)
    }

    pub fn max_capacity(&self) -> usize {
        self.inner.max_capacity()
    }

    pub fn remaining_capacity(&self) -> usize {
        self.inner.capacity()
    }

    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }
}

impl fmt::Debug for UpstreamStreamSender {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UpstreamStreamSender")
            .field("max_capacity", &self.max_capacity())
            .field("remaining_capacity", &self.remaining_capacity())
            .field("closed", &self.is_closed())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum UpstreamStreamSendError {
    #[error("the upstream stream event exceeds its bounds")]
    InvalidEvent,
    #[error("the upstream stream receiver is closed")]
    Closed,
}

pub(crate) enum UpstreamStreamItem {
    Event(CanonicalEvent),
    Failure(UpstreamExecutionFailure),
}

pub struct UpstreamExecutionStream {
    receiver: mpsc::Receiver<UpstreamStreamItem>,
    created_at: u64,
    finish_reason: UpstreamFinishReason,
    stop_sequence: Option<String>,
    thinking_signatures: BTreeMap<String, String>,
    initial_usage: Usage,
    upstream_request_id: Option<SafeUpstreamRequestId>,
}

impl UpstreamExecutionStream {
    pub fn channel(created_at: u64) -> (UpstreamStreamSender, Self) {
        let (sender, receiver) = mpsc::channel(UPSTREAM_STREAM_CHANNEL_CAPACITY);
        (
            UpstreamStreamSender { inner: sender },
            Self {
                receiver,
                created_at,
                finish_reason: UpstreamFinishReason::Stop,
                stop_sequence: None,
                thinking_signatures: BTreeMap::new(),
                initial_usage: Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                    extensions: BTreeMap::new(),
                },
                upstream_request_id: None,
            },
        )
    }

    pub fn with_upstream_request_id(mut self, request_id: SafeUpstreamRequestId) -> Self {
        self.upstream_request_id = Some(request_id);
        self
    }

    pub fn with_finish_reason(mut self, finish_reason: UpstreamFinishReason) -> Self {
        self.finish_reason = finish_reason;
        self
    }

    pub fn with_stop_sequence(
        mut self,
        stop_sequence: impl Into<String>,
    ) -> Result<Self, InvalidUpstreamExecutionStream> {
        let stop_sequence = stop_sequence.into();
        if !valid_stream_identifier(&stop_sequence) {
            return Err(InvalidUpstreamExecutionStream);
        }
        self.stop_sequence = Some(stop_sequence);
        Ok(self)
    }

    pub fn with_thinking_signatures(
        mut self,
        thinking_signatures: BTreeMap<String, String>,
    ) -> Result<Self, InvalidUpstreamExecutionStream> {
        if thinking_signatures.len() > MAX_UPSTREAM_EVENTS
            || thinking_signatures.iter().any(|(item_id, signature)| {
                !valid_stream_identifier(item_id)
                    || signature.is_empty()
                    || signature.len() > MAX_UPSTREAM_METADATA_BYTES
            })
            || !json_fits(&thinking_signatures, MAX_UPSTREAM_METADATA_BYTES)
        {
            return Err(InvalidUpstreamExecutionStream);
        }
        self.thinking_signatures = thinking_signatures;
        Ok(self)
    }

    pub fn with_initial_usage(
        mut self,
        initial_usage: Usage,
    ) -> Result<Self, InvalidUpstreamExecutionStream> {
        if !json_fits(&initial_usage, MAX_UPSTREAM_METADATA_BYTES) {
            return Err(InvalidUpstreamExecutionStream);
        }
        self.initial_usage = initial_usage;
        Ok(self)
    }

    pub const fn created_at(&self) -> u64 {
        self.created_at
    }

    pub const fn finish_reason(&self) -> UpstreamFinishReason {
        self.finish_reason
    }

    pub fn stop_sequence(&self) -> Option<&str> {
        self.stop_sequence.as_deref()
    }

    pub fn thinking_signatures(&self) -> &BTreeMap<String, String> {
        &self.thinking_signatures
    }

    pub fn initial_usage(&self) -> &Usage {
        &self.initial_usage
    }

    pub fn upstream_request_id(&self) -> Option<&SafeUpstreamRequestId> {
        self.upstream_request_id.as_ref()
    }

    async fn receive(
        &mut self,
        cancellation: &ExecutionCancellation,
    ) -> Option<UpstreamStreamItem> {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => None,
            item = self.receiver.recv() => item,
        }
    }

    pub(crate) fn poll_receive(
        &mut self,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<UpstreamStreamItem>> {
        self.receiver.poll_recv(context)
    }
}

impl fmt::Debug for UpstreamExecutionStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UpstreamExecutionStream")
            .field("created_at", &self.created_at)
            .field("finish_reason", &self.finish_reason)
            .field("has_stop_sequence", &self.stop_sequence.is_some())
            .field("thinking_signature_count", &self.thinking_signatures.len())
            .field("upstream_request_id", &self.upstream_request_id)
            .field("channel_capacity", &UPSTREAM_STREAM_CHANNEL_CAPACITY)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("the upstream execution stream metadata exceeds its bounds")]
pub struct InvalidUpstreamExecutionStream;

fn stream_event_fits(event: &CanonicalEvent) -> bool {
    let identifiers_fit = match event {
        CanonicalEvent::Created { response_id } => valid_stream_identifier(response_id),
        CanonicalEvent::OutputTextDelta { item_id, .. }
        | CanonicalEvent::ReasoningDelta { item_id, .. } => valid_stream_identifier(item_id),
        CanonicalEvent::ToolCallDelta {
            item_id,
            call_id,
            name,
            ..
        } => {
            valid_stream_identifier(item_id)
                && valid_stream_identifier(call_id)
                && valid_stream_identifier(name)
        }
        CanonicalEvent::Usage(_) | CanonicalEvent::Completed => true,
        CanonicalEvent::Failed(_) => false,
    };
    identifiers_fit && json_fits(event, MAX_UPSTREAM_METADATA_BYTES)
}

fn valid_stream_identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_UPSTREAM_IDENTIFIER_BYTES
}

#[async_trait]
pub trait UpstreamExecutor: Send + Sync + 'static {
    async fn execute(
        &self,
        request: UpstreamExecutionRequest,
        cancellation: ExecutionCancellation,
    ) -> UpstreamExecutionResult;
}

enum AttemptExecutionOutput {
    Complete(UpstreamExecutionResponse),
    Stream(StartedUpstreamStream),
}

pub(crate) enum Executed {
    Response(ExecutedResponse),
    Stream(ExecutedStream),
}

pub(crate) struct ExecutedResponse {
    pub(crate) response: UpstreamExecutionResponse,
    pub(crate) public_model: PublicModelId,
    pub(crate) request: Arc<CanonicalRequest>,
    pub(crate) public_reasoning_effort: Option<String>,
}

pub(crate) struct ExecutedStream {
    pub(crate) stream: StartedUpstreamStream,
    pub(crate) public_model: PublicModelId,
    pub(crate) request: Arc<CanonicalRequest>,
    pub(crate) public_reasoning_effort: Option<String>,
    pub(crate) cancellation: CancelOnDrop,
}

pub(crate) struct StartedUpstreamStream {
    first_event: Option<CanonicalEvent>,
    upstream: UpstreamExecutionStream,
}

impl StartedUpstreamStream {
    pub(crate) fn take_first_event(&mut self) -> Option<CanonicalEvent> {
        self.first_event.take()
    }

    pub(crate) fn upstream(&self) -> &UpstreamExecutionStream {
        &self.upstream
    }

    pub(crate) fn upstream_mut(&mut self) -> &mut UpstreamExecutionStream {
        &mut self.upstream
    }
}

pub(crate) struct DataPlaneExecutionError {
    pub(crate) error: GatewayError,
    pub(crate) upstream_request_id: Option<SafeUpstreamRequestId>,
}

struct RequestAttempt {
    executor: Arc<dyn UpstreamExecutor>,
    operation: UpstreamOperation,
    route: RouteDecision,
    canonical: Arc<CanonicalRequest>,
    last_upstream_request_id: Arc<Mutex<Option<SafeUpstreamRequestId>>>,
}

#[async_trait]
impl UpstreamAttempt for RequestAttempt {
    type Output = AttemptExecutionOutput;

    async fn execute(
        &self,
        context: AttemptContext<'_>,
        _body: &[u8],
        cancellation: &ExecutionCancellation,
    ) -> AttemptResult<Self::Output> {
        let account = self
            .route
            .provider()
            .accounts()
            .iter()
            .find(|account| account.id() == context.account_id());
        let auth = if let Some(account) = account {
            account.auth().clone()
        } else if self.route.provider().accounts().is_empty()
            && context.account_id().as_str() == ACCOUNTLESS_ACCOUNT_ID
        {
            AccountAuthConfig::Local
        } else {
            return AttemptResult::Failed {
                failure: AttemptFailure::new(AttemptFailureKind::Other, None, None),
                boundary: AttemptBoundary::BeforeVisible,
            };
        };
        let request = UpstreamExecutionRequest {
            request_id: Arc::from(context.request_id().as_str()),
            attempt_ordinal: context.attempt_id().ordinal(),
            operation: self.operation,
            provider_id: context.provider_id().clone(),
            account_id: context.account_id().clone(),
            adapter: self.route.provider().adapter(),
            endpoint: Arc::from(self.route.provider().endpoint()),
            model: Arc::from(context.model()),
            auth,
            canonical: Arc::clone(&self.canonical),
        };
        match self.executor.execute(request, cancellation.clone()).await {
            UpstreamExecutionResult::Succeeded(response) => {
                *self
                    .last_upstream_request_id
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    response.upstream_request_id().cloned();
                if self.canonical.stream {
                    failed_attempt(UpstreamExecutionFailure::new(
                        UpstreamFailureKind::MalformedResponse,
                    ))
                } else {
                    AttemptResult::Succeeded {
                        output: AttemptExecutionOutput::Complete(response),
                        boundary: AttemptBoundary::BeforeVisible,
                    }
                }
            }
            UpstreamExecutionResult::Streaming(mut stream) => {
                *self
                    .last_upstream_request_id
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    stream.upstream_request_id().cloned();
                if self.operation != UpstreamOperation::Text || !self.canonical.stream {
                    return failed_attempt(UpstreamExecutionFailure::new(
                        UpstreamFailureKind::MalformedResponse,
                    ));
                }
                match stream.receive(cancellation).await {
                    Some(UpstreamStreamItem::Event(
                        first_event @ CanonicalEvent::Created { .. },
                    )) => AttemptResult::Succeeded {
                        output: AttemptExecutionOutput::Stream(StartedUpstreamStream {
                            first_event: Some(first_event),
                            upstream: stream,
                        }),
                        boundary: AttemptBoundary::Visible,
                    },
                    Some(UpstreamStreamItem::Event(_)) => failed_attempt(
                        UpstreamExecutionFailure::new(UpstreamFailureKind::MalformedResponse),
                    ),
                    Some(UpstreamStreamItem::Failure(failure)) => {
                        *self
                            .last_upstream_request_id
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            failure.upstream_request_id().cloned();
                        failed_attempt(failure)
                    }
                    None if cancellation.is_cancelled() => failed_attempt(
                        UpstreamExecutionFailure::new(UpstreamFailureKind::Cancelled),
                    ),
                    None => failed_attempt(UpstreamExecutionFailure::new(
                        UpstreamFailureKind::MalformedResponse,
                    )),
                }
            }
            UpstreamExecutionResult::Failed(failure) => {
                *self
                    .last_upstream_request_id
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    failure.upstream_request_id().cloned();
                failed_attempt(failure)
            }
        }
    }
}

fn failed_attempt(failure: UpstreamExecutionFailure) -> AttemptResult<AttemptExecutionOutput> {
    AttemptResult::Failed {
        failure: AttemptFailure::new(
            map_failure_kind(failure.kind()),
            failure.status(),
            failure.retry_after_ms(),
        ),
        boundary: AttemptBoundary::BeforeVisible,
    }
}

pub(crate) async fn execute_canonical(
    state: &ServerState,
    authorized: &AuthorizedClient,
    mut canonical: CanonicalRequest,
    operation: UpstreamOperation,
) -> Result<Executed, DataPlaneExecutionError> {
    let providers = state
        .providers
        .as_ref()
        .ok_or_else(|| execution_error(GatewayError::no_executor(), None))?;
    let public_model = canonical.model.clone();
    let public_reasoning_effort = canonical
        .reasoning
        .as_ref()
        .and_then(|reasoning| reasoning.effort.clone());
    let execution_request_id = ExecutionRequestId::new(canonical.request_id.as_str().to_owned())
        .map_err(|_| execution_error(GatewayError::invalid_request(), None))?;
    let execution = providers.execution_snapshot();
    let route = execution
        .snapshot
        .route(&RouteRequest {
            provider: None,
            model: canonical.model.as_str().to_owned(),
            client_id: Some(authorized.client_id.clone()),
        })
        .map_err(|error| execution_error(map_route_error(error), None))?;
    if !route.provider().capabilities().text {
        return Err(execution_error(
            GatewayError::unsupported_capability(),
            None,
        ));
    }
    if operation == UpstreamOperation::CountTokens && !route.provider().capabilities().count_tokens
    {
        let input_tokens =
            estimate_local_tokens(&canonical).map_err(|error| execution_error(error, None))?;
        return Ok(Executed::Response(ExecutedResponse {
            response: UpstreamExecutionResponse::token_count(input_tokens),
            public_model,
            request: Arc::new(canonical),
            public_reasoning_effort,
        }));
    }
    let executor = state
        .upstream_executor
        .as_ref()
        .cloned()
        .ok_or_else(|| execution_error(GatewayError::no_executor(), None))?;
    let health = Arc::clone(&execution.account_health);
    canonical.model = PublicModelId::new(route.model());
    if let Some(effort) = canonical
        .reasoning
        .as_ref()
        .and_then(|reasoning| reasoning.effort.as_deref())
    {
        let mapped = route
            .map_reasoning_effort(effort)
            .map_err(|error| execution_error(map_route_error(error), None))?;
        if let Some(reasoning) = canonical.reasoning.as_mut() {
            reasoning.effort = Some(mapped);
        }
    }

    let mut authentications = Vec::new();
    for account in route.provider().accounts() {
        let authentication = account.authentication();
        if !authentications.contains(&authentication) {
            authentications.push(authentication);
        }
    }
    let cancellation = ExecutionCancellation::new();
    let cancel_on_drop = CancelOnDrop(cancellation.clone());
    let last_upstream_request_id = Arc::new(Mutex::new(None));
    let canonical = Arc::new(canonical);
    let attempt = Arc::new(RequestAttempt {
        executor,
        operation,
        route: route.clone(),
        canonical: Arc::clone(&canonical),
        last_upstream_request_id: Arc::clone(&last_upstream_request_id),
    });
    let coordinator = ExecutionCoordinator::new(
        attempt,
        Arc::new(TokioRetryDelay),
        Arc::new(SystemExecutionClock),
        RetryPolicy::new(1, 60_000, JSON_BODY_LIMIT)
            .expect("data-plane retry policy is compile-time valid"),
    );
    if authentications.is_empty() {
        let account_id = AccountId::new(ACCOUNTLESS_ACCOUNT_ID)
            .expect("the accountless execution identity is compile-time valid");
        let request = ExecutionRequest::new_accountless(
            execution_request_id,
            route.provider_id(),
            route.model(),
            &account_id,
            PreparedRequestBody::new(Vec::new()),
        );
        return match coordinator
            .execute(request, &health, cancellation.clone())
            .await
        {
            ExecutionOutcome::Succeeded(success) => executed_from_attempt(
                success.into_output(),
                public_model,
                canonical,
                public_reasoning_effort,
                cancel_on_drop,
            ),
            ExecutionOutcome::Failed(failure) => {
                let request_id = last_upstream_request_id
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                Err(execution_error(
                    map_terminal(failure.terminal()),
                    request_id,
                ))
            }
        };
    }
    for authentication in authentications {
        let candidates = route
            .provider()
            .accounts()
            .iter()
            .filter(|account| account.authentication() == authentication)
            .map(|account| {
                wokcore_engine::execution::ExecutionCandidate::new(
                    account.id().clone(),
                    account.authentication(),
                    1,
                )
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| execution_error(GatewayError::internal("candidate"), None))?;
        let request = ExecutionRequest::new(
            execution_request_id.clone(),
            route.provider_id(),
            route.model(),
            &candidates,
            authentication,
            None,
            PreparedRequestBody::new(Vec::new()),
        );
        match coordinator
            .execute(request, &health, cancellation.clone())
            .await
        {
            ExecutionOutcome::Succeeded(success) => {
                return executed_from_attempt(
                    success.into_output(),
                    public_model,
                    Arc::clone(&canonical),
                    public_reasoning_effort,
                    cancel_on_drop,
                );
            }
            ExecutionOutcome::Failed(failure)
                if matches!(failure.terminal(), ExecutionTerminal::NoEligibleAccount)
                    && failure.history().is_empty() =>
            {
                continue;
            }
            ExecutionOutcome::Failed(failure) => {
                let request_id = last_upstream_request_id
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                return Err(execution_error(
                    map_terminal(failure.terminal()),
                    request_id,
                ));
            }
        }
    }
    Err(execution_error(
        GatewayError::transport("no eligible account"),
        None,
    ))
}

fn executed_from_attempt(
    output: AttemptExecutionOutput,
    public_model: PublicModelId,
    request: Arc<CanonicalRequest>,
    public_reasoning_effort: Option<String>,
    cancellation: CancelOnDrop,
) -> Result<Executed, DataPlaneExecutionError> {
    match (request.stream, output) {
        (false, AttemptExecutionOutput::Complete(response)) => {
            Ok(Executed::Response(ExecutedResponse {
                response,
                public_model,
                request,
                public_reasoning_effort,
            }))
        }
        (true, AttemptExecutionOutput::Stream(stream)) => Ok(Executed::Stream(ExecutedStream {
            stream,
            public_model,
            request,
            public_reasoning_effort,
            cancellation,
        })),
        (false, AttemptExecutionOutput::Stream(_))
        | (true, AttemptExecutionOutput::Complete(_)) => Err(execution_error(
            GatewayError::upstream_response(502, "upstream execution mode mismatch"),
            None,
        )),
    }
}

fn map_failure_kind(kind: UpstreamFailureKind) -> AttemptFailureKind {
    match kind {
        UpstreamFailureKind::Timeout => AttemptFailureKind::Timeout,
        UpstreamFailureKind::Cancelled | UpstreamFailureKind::Transport => {
            AttemptFailureKind::Reset
        }
        UpstreamFailureKind::MalformedResponse => AttemptFailureKind::Other,
        UpstreamFailureKind::RateLimited => AttemptFailureKind::RateLimited,
        UpstreamFailureKind::Server => AttemptFailureKind::Server,
        UpstreamFailureKind::Reset => AttemptFailureKind::Reset,
        UpstreamFailureKind::InvalidCredentials => AttemptFailureKind::InvalidCredentials,
        UpstreamFailureKind::InvalidRequest => AttemptFailureKind::InvalidRequest,
        UpstreamFailureKind::Policy => AttemptFailureKind::Policy,
    }
}

fn estimate_local_tokens(request: &CanonicalRequest) -> Result<u64, GatewayError> {
    let mut counter = BoundedByteCounter::new(JSON_BODY_LIMIT);
    serde_json::to_writer(&mut counter, request).map_err(|_| GatewayError::invalid_request())?;
    let estimated = counter.length().div_ceil(4).max(1);
    u64::try_from(estimated).map_err(|_| GatewayError::invalid_request())
}

fn json_fits<T>(value: &T, limit: usize) -> bool
where
    T: serde::Serialize + ?Sized,
{
    let mut counter = BoundedByteCounter::new(limit);
    serde_json::to_writer(&mut counter, value).is_ok()
}

struct BoundedByteCounter {
    length: usize,
    limit: usize,
}

impl BoundedByteCounter {
    const fn new(limit: usize) -> Self {
        Self { length: 0, limit }
    }

    const fn length(&self) -> usize {
        self.length
    }
}

impl Write for BoundedByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let length = self
            .length
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("token estimate overflow"))?;
        if length > self.limit {
            return Err(io::Error::other("token estimate limit"));
        }
        self.length = length;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn map_route_error(error: RouteError) -> GatewayError {
    match error {
        RouteError::NoRoute | RouteError::ProviderUnavailable => GatewayError::unknown_model(),
        RouteError::UnsupportedReasoningEffort => GatewayError::unsupported_capability(),
    }
}

fn map_terminal(terminal: &ExecutionTerminal) -> GatewayError {
    match terminal {
        ExecutionTerminal::Cancelled => GatewayError::transport("cancelled"),
        ExecutionTerminal::RequestBodyTooLarge => GatewayError::invalid_request(),
        ExecutionTerminal::NoEligibleAccount => GatewayError::transport("no eligible account"),
        ExecutionTerminal::Upstream { failure, .. } => match failure.kind() {
            AttemptFailureKind::RateLimited => {
                GatewayError::rate_limited(failure.retry_after_ms().map(|delay| delay / 1_000))
            }
            AttemptFailureKind::Server => GatewayError::upstream_5xx(
                failure
                    .status_code()
                    .filter(|status| (500..=599).contains(status))
                    .unwrap_or(500),
            ),
            AttemptFailureKind::Timeout | AttemptFailureKind::Reset => {
                GatewayError::transport("upstream transport")
            }
            AttemptFailureKind::InvalidCredentials => {
                GatewayError::upstream_auth("upstream authentication")
            }
            AttemptFailureKind::InvalidRequest => GatewayError::invalid_request(),
            AttemptFailureKind::Policy => GatewayError::unsupported_capability(),
            AttemptFailureKind::Other => GatewayError::upstream_response(
                failure.status_code().unwrap_or(502),
                "malformed upstream response",
            ),
        },
    }
}

fn execution_error(
    error: GatewayError,
    upstream_request_id: Option<SafeUpstreamRequestId>,
) -> DataPlaneExecutionError {
    DataPlaneExecutionError {
        error,
        upstream_request_id,
    }
}

pub(crate) struct CancelOnDrop(ExecutionCancellation);

impl CancelOnDrop {
    pub(crate) fn cancellation(&self) -> &ExecutionCancellation {
        &self.0
    }

    pub(crate) fn cancel(&self) {
        self.0.cancel();
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
