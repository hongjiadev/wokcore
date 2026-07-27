use std::{
    convert::Infallible,
    pin::Pin,
    task::{Context, Poll},
};

use axum::{
    body::{Body, Bytes},
    http::{HeaderName, HeaderValue, header},
    response::Response,
};
use futures_core::Stream;
use wokcore_diagnostics::runtime::{
    StreamRuntimeDiagnostics, StreamRuntimeObservation, StreamRuntimeOutcome,
};
use wokcore_protocols::{
    AnthropicCodec, ChatCodec, ResponsesCodec,
    canonical::{CanonicalEvent, GatewayError},
};

use super::{
    ClientProtocol, ExecutedStream, UpstreamExecutionFailure, UpstreamFailureKind,
    execute::{CancelOnDrop, UpstreamStreamItem},
    response::attach_upstream_request_id,
};

const ACCEL_BUFFERING_HEADER: HeaderName = HeaderName::from_static("x-accel-buffering");

pub(crate) fn encode(
    mut executed: ExecutedStream,
    protocol: ClientProtocol,
    diagnostics: &StreamRuntimeDiagnostics,
) -> Result<Response, GatewayError> {
    let codec = match protocol {
        ClientProtocol::OpenAiResponses => {
            ClientStreamCodec::Responses(super::responses::stream_codec(&executed)?)
        }
        ClientProtocol::OpenAiChatCompletions => {
            ClientStreamCodec::Chat(super::chat::stream_codec(&executed))
        }
        ClientProtocol::AnthropicMessages => {
            ClientStreamCodec::Anthropic(super::anthropic::stream_codec(&executed))
        }
        ClientProtocol::AnthropicCountTokens
        | ClientProtocol::OpenAiModels
        | ClientProtocol::OpenAiImageGenerations
        | ClientProtocol::OpenAiImageEdits => {
            return Err(GatewayError::unsupported_capability());
        }
    };
    let upstream_request_id = executed.stream.upstream().upstream_request_id().cloned();
    let first_event = executed
        .stream
        .take_first_event()
        .ok_or_else(|| GatewayError::upstream_response(502, "missing initial stream event"))?;
    let body = CanonicalSseStream {
        first_event: Some(first_event),
        upstream: executed.stream,
        codec,
        cancellation: executed.cancellation,
        observation: diagnostics.start(),
        finished: false,
        protocol_error_sent: false,
    };
    let mut response = Response::new(Body::from_stream(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert(ACCEL_BUFFERING_HEADER, HeaderValue::from_static("no"));
    Ok(attach_upstream_request_id(
        response,
        upstream_request_id.as_ref(),
    ))
}

enum ClientStreamCodec {
    Responses(ResponsesCodec),
    Chat(ChatCodec),
    Anthropic(AnthropicCodec),
}

impl ClientStreamCodec {
    fn encode(&mut self, event: &CanonicalEvent) -> Result<Bytes, GatewayError> {
        match self {
            Self::Responses(codec) => codec.encode_event(event),
            Self::Chat(codec) => codec.encode_chunk(event),
            Self::Anthropic(codec) => codec.encode_event(event),
        }
    }
}

struct CanonicalSseStream {
    first_event: Option<CanonicalEvent>,
    upstream: super::execute::StartedUpstreamStream,
    codec: ClientStreamCodec,
    cancellation: CancelOnDrop,
    observation: StreamRuntimeObservation,
    finished: bool,
    protocol_error_sent: bool,
}

impl CanonicalSseStream {
    fn encode_event(&mut self, event: CanonicalEvent) -> Option<Result<Bytes, Infallible>> {
        let outcome = match &event {
            CanonicalEvent::Completed => Some(StreamRuntimeOutcome::Completed),
            CanonicalEvent::Failed(_) => Some(StreamRuntimeOutcome::UpstreamError),
            CanonicalEvent::Created { .. }
            | CanonicalEvent::OutputTextDelta { .. }
            | CanonicalEvent::ReasoningDelta { .. }
            | CanonicalEvent::ToolCallDelta { .. }
            | CanonicalEvent::Usage(_) => None,
        };
        match self.codec.encode(&event) {
            Ok(bytes) => {
                if !bytes.is_empty() {
                    self.observation.observe_frame(bytes.len());
                }
                if let Some(outcome) = outcome {
                    self.finish(outcome);
                }
                (!bytes.is_empty()).then_some(Ok(bytes))
            }
            Err(_) => self.emit_protocol_error(),
        }
    }

    fn emit_upstream_failure(
        &mut self,
        failure: UpstreamExecutionFailure,
    ) -> Option<Result<Bytes, Infallible>> {
        let outcome = if failure.kind() == UpstreamFailureKind::MalformedResponse {
            StreamRuntimeOutcome::ProtocolError
        } else if failure.kind() == UpstreamFailureKind::Cancelled {
            StreamRuntimeOutcome::Cancelled
        } else {
            StreamRuntimeOutcome::UpstreamError
        };
        self.emit_error(map_stream_failure(&failure), outcome)
    }

    fn emit_protocol_error(&mut self) -> Option<Result<Bytes, Infallible>> {
        self.emit_error(
            GatewayError::upstream_response(502, "malformed upstream stream"),
            StreamRuntimeOutcome::ProtocolError,
        )
    }

    fn emit_error(
        &mut self,
        error: GatewayError,
        outcome: StreamRuntimeOutcome,
    ) -> Option<Result<Bytes, Infallible>> {
        if self.finished || self.protocol_error_sent {
            return None;
        }
        self.protocol_error_sent = true;
        let encoded = self.codec.encode(&CanonicalEvent::Failed(error)).ok();
        if let Some(bytes) = encoded.as_ref().filter(|bytes| !bytes.is_empty()) {
            self.observation.observe_frame(bytes.len());
        }
        self.finish(outcome);
        encoded
            .filter(|bytes| !bytes.is_empty())
            .map(Ok::<_, Infallible>)
    }

    fn finish(&mut self, outcome: StreamRuntimeOutcome) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.cancellation.cancel();
        self.observation.finish(outcome);
    }
}

impl Stream for CanonicalSseStream {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        if this.finished {
            return Poll::Ready(None);
        }
        if this.cancellation.cancellation().is_cancelled() {
            this.finish(StreamRuntimeOutcome::Cancelled);
            return Poll::Ready(None);
        }

        loop {
            let item = if let Some(first_event) = this.first_event.take() {
                Some(UpstreamStreamItem::Event(first_event))
            } else {
                match this.upstream.upstream_mut().poll_receive(context) {
                    Poll::Ready(item) => item,
                    Poll::Pending => return Poll::Pending,
                }
            };
            let encoded = match item {
                Some(UpstreamStreamItem::Event(event)) => this.encode_event(event),
                Some(UpstreamStreamItem::Failure(failure)) => this.emit_upstream_failure(failure),
                None => this.emit_protocol_error(),
            };
            if this.finished || encoded.is_some() {
                return Poll::Ready(encoded);
            }
        }
    }
}

fn map_stream_failure(failure: &UpstreamExecutionFailure) -> GatewayError {
    match failure.kind() {
        UpstreamFailureKind::Timeout
        | UpstreamFailureKind::Cancelled
        | UpstreamFailureKind::Reset
        | UpstreamFailureKind::Transport => GatewayError::transport("upstream stream transport"),
        UpstreamFailureKind::MalformedResponse => {
            GatewayError::upstream_response(failure.status().unwrap_or(502), "malformed stream")
        }
        UpstreamFailureKind::RateLimited => {
            GatewayError::rate_limited(failure.retry_after_ms().map(|delay| delay / 1_000))
        }
        UpstreamFailureKind::Server => GatewayError::upstream_5xx(
            failure
                .status()
                .filter(|status| (500..=599).contains(status))
                .unwrap_or(500),
        ),
        UpstreamFailureKind::InvalidCredentials => {
            GatewayError::upstream_auth("upstream authentication")
        }
        UpstreamFailureKind::InvalidRequest => GatewayError::invalid_request(),
        UpstreamFailureKind::Policy => GatewayError::unsupported_capability(),
    }
}
