use std::{
    fmt,
    time::{Duration, Instant},
};

use axum::{
    extract::{Request, State},
    http::{
        HeaderValue, StatusCode,
        header::{CACHE_CONTROL, HeaderName},
    },
    middleware::Next,
    response::{IntoResponse, Response},
};
use uuid::Uuid;
use wokcore_diagnostics::event::{
    BuildIdentity, CapabilityVersion, Correlations, DiagnosticBuildError, DiagnosticComponent,
    DiagnosticDecision, DiagnosticEventCode, DiagnosticEventDraft, DiagnosticLevel, EventId,
    FailoverDecision, GitCommit, Measurements, RequestId as DiagnosticRequestId, RetryDecision,
    StageCode, StateTransition, TokenCounts, UtcTimestamp, WokcoreVersion,
};

use crate::{ServerState, runtime::generate_uuid_v4};

use super::error::{ApiError, ApiErrorComponent};

const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");
const CONTENT_TYPE_OPTIONS_HEADER: HeaderName = HeaderName::from_static("x-content-type-options");

#[derive(Clone, Copy)]
pub(crate) struct RequestId(Uuid);

impl RequestId {
    fn entropy_failure() -> Self {
        Self(Uuid::nil())
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

pub(crate) async fn apply_response_envelope(
    State(state): State<ServerState>,
    mut request: Request,
    next: Next,
) -> Response {
    let request_id = match generate_uuid_v4(state.request_id_entropy.as_ref()) {
        Ok(uuid) => RequestId(uuid),
        Err(_) => {
            return apply_headers(
                ApiError::internal_failure(RequestId::entropy_failure()).into_response(),
                RequestId::entropy_failure(),
            );
        }
    };
    request.headers_mut().remove(&REQUEST_ID_HEADER);
    request.extensions_mut().insert(request_id);
    let route_component = request_component(request.uri().path());
    let started_at = Instant::now();
    let response = next.run(request).await;
    let component = response
        .extensions()
        .get::<ApiErrorComponent>()
        .map_or(route_component, |component| component.0);
    record_request_diagnostic(
        &state,
        request_id,
        response.status(),
        component,
        started_at.elapsed(),
    );
    apply_headers(response, request_id)
}

fn apply_headers(mut response: Response, request_id: RequestId) -> Response {
    response.headers_mut().insert(
        REQUEST_ID_HEADER,
        HeaderValue::from_str(&request_id.to_string()).expect("UUID is a valid header value"),
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        CONTENT_TYPE_OPTIONS_HEADER,
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn request_component(path: &str) -> DiagnosticComponent {
    if path == "/wokcore/v1/logs" || path == "/wokcore/v1/diagnostics/export" {
        DiagnosticComponent::Diagnostics
    } else if path == "/wokcore/v1/sessions"
        || path.starts_with("/wokcore/v1/sessions/")
        || path == "/wokcore/v1/usage"
    {
        DiagnosticComponent::Sessions
    } else {
        DiagnosticComponent::Core
    }
}

fn record_request_diagnostic(
    state: &ServerState,
    request_id: RequestId,
    status: StatusCode,
    component: DiagnosticComponent,
    elapsed: Duration,
) {
    let Some(writer) = state.diagnostics.as_ref() else {
        return;
    };
    let occurred_at = state.token_metadata.now();
    let request_id = request_id.to_string();
    let duration_micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
    let draft = request_diagnostic_draft(
        &request_id,
        occurred_at.as_deref().unwrap_or_default(),
        status,
        component,
        duration_micros,
    );
    let _ = writer.recorder().try_record(draft);
}

fn request_diagnostic_draft(
    request_id: &str,
    occurred_at: &str,
    status: StatusCode,
    component: DiagnosticComponent,
    duration_micros: u64,
) -> Result<DiagnosticEventDraft, DiagnosticBuildError> {
    let level = if status == StatusCode::INTERNAL_SERVER_ERROR {
        DiagnosticLevel::Warn
    } else {
        DiagnosticLevel::Info
    };
    let code = if status.is_success() {
        DiagnosticEventCode::RequestCompleted
    } else {
        DiagnosticEventCode::RequestFailed
    };
    Ok(DiagnosticEventDraft::new(
        EventId::parse(request_id)?,
        UtcTimestamp::parse(occurred_at)?,
        level,
        component,
        code,
        build_identity()?,
    )
    .with_correlations(Correlations::new(
        Some(DiagnosticRequestId::parse(request_id)?),
        None,
        None,
        None,
        None,
        None,
    ))
    .with_measurements(Measurements::new(
        StageCode::Response,
        duration_micros,
        0,
        0,
        TokenCounts::new(0, 0, 0, 0),
    )))
}

pub(crate) fn record_lifecycle_diagnostic(
    state: &ServerState,
    request_id: RequestId,
    transition: StateTransition,
) {
    let Some(writer) = state.diagnostics.as_ref() else {
        return;
    };
    let request_id_text = request_id.to_string();
    let event_id = derived_event_id(request_id, lifecycle_transition_discriminator(transition));
    let draft = state
        .token_metadata
        .now()
        .map_err(|_| DiagnosticBuildError::InvalidValue)
        .and_then(|occurred_at| {
            Ok(DiagnosticEventDraft::new(
                EventId::parse(&event_id)?,
                UtcTimestamp::parse(&occurred_at)?,
                DiagnosticLevel::Info,
                DiagnosticComponent::Core,
                DiagnosticEventCode::LifecycleTransition,
                build_identity()?,
            )
            .with_correlations(Correlations::new(
                Some(DiagnosticRequestId::parse(&request_id_text)?),
                None,
                None,
                None,
                None,
                None,
            ))
            .with_decision(DiagnosticDecision::new(
                transition,
                RetryDecision::NotApplicable,
                FailoverDecision::NotApplicable,
            )))
        });
    let _ = writer.recorder().try_record(draft);
}

pub(crate) fn record_export_stream_failure(state: &ServerState, request_id: RequestId) {
    let Some(writer) = state.diagnostics.as_ref() else {
        return;
    };
    let occurred_at = state.token_metadata.now();
    let request_id_text = request_id.to_string();
    let event_id = derived_event_id(request_id, 0xf0);
    let draft = occurred_at
        .map_err(|_| DiagnosticBuildError::InvalidValue)
        .and_then(|occurred_at| {
            correlated_failure_draft(
                &event_id,
                &request_id_text,
                &occurred_at,
                DiagnosticComponent::Diagnostics,
            )
        });
    let _ = writer.recorder().try_record(draft);
}

fn correlated_failure_draft(
    event_id: &str,
    request_id: &str,
    occurred_at: &str,
    component: DiagnosticComponent,
) -> Result<DiagnosticEventDraft, DiagnosticBuildError> {
    Ok(DiagnosticEventDraft::new(
        EventId::parse(event_id)?,
        UtcTimestamp::parse(occurred_at)?,
        DiagnosticLevel::Warn,
        component,
        DiagnosticEventCode::RequestFailed,
        build_identity()?,
    )
    .with_correlations(Correlations::new(
        Some(DiagnosticRequestId::parse(request_id)?),
        None,
        None,
        None,
        None,
        None,
    ))
    .with_measurements(Measurements::new(
        StageCode::Response,
        0,
        0,
        0,
        TokenCounts::new(0, 0, 0, 0),
    )))
}

fn derived_event_id(request_id: RequestId, discriminator: u8) -> String {
    let mut bytes = *request_id.0.as_bytes();
    bytes[15] ^= discriminator;
    Uuid::from_bytes(bytes).hyphenated().to_string()
}

const fn lifecycle_transition_discriminator(transition: StateTransition) -> u8 {
    match transition {
        StateTransition::StartingToReady => 1,
        StateTransition::ReadyToDegraded => 2,
        StateTransition::DegradedToReady => 3,
        StateTransition::ReadyToDraining => 4,
        StateTransition::DrainingToAwaitingCancellation => 5,
        StateTransition::DrainingToReady => 6,
        StateTransition::ReadyToStopping => 7,
        StateTransition::StoppingToStopped => 8,
    }
}

fn build_identity() -> Result<BuildIdentity, DiagnosticBuildError> {
    Ok(BuildIdentity::new(
        WokcoreVersion::parse(env!("CARGO_PKG_VERSION"))?,
        GitCommit::parse(
            option_env!("WOKCORE_GIT_COMMIT").unwrap_or("0000000000000000000000000000000000000000"),
        )?,
        1,
        CapabilityVersion::new(1),
    ))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::Value;
    use wokcore_diagnostics::{
        event::{DiagnosticComponent, DiagnosticEvent, DiagnosticEventCode, DiagnosticLevel},
        recorder::{DiagnosticRecorder, RecordOutcome},
        ring::{PageDirection, PageRequest},
    };

    use super::{request_component, request_diagnostic_draft};

    #[test]
    fn request_paths_map_to_stable_diagnostic_components() {
        assert_eq!(
            request_component("/wokcore/v1/sessions/example/messages"),
            DiagnosticComponent::Sessions
        );
        assert_eq!(
            request_component("/wokcore/v1/diagnostics/export"),
            DiagnosticComponent::Diagnostics
        );
        assert_eq!(
            request_component("/wokcore/v1/clients/authorize"),
            DiagnosticComponent::Core
        );
    }

    #[tokio::test]
    async fn request_events_are_typed_correlated_and_bounded() {
        let (recorder, owner) = DiagnosticRecorder::new();
        let owner_task = tokio::spawn(owner.run());
        let request_id = "019844f0-4de0-7000-8000-000000000031";
        assert_eq!(
            recorder.try_record(request_diagnostic_draft(
                request_id,
                "2026-07-27T12:00:00Z",
                StatusCode::SERVICE_UNAVAILABLE,
                DiagnosticComponent::Sessions,
                12_345,
            )),
            RecordOutcome::Accepted
        );
        recorder.try_barrier().unwrap().wait().await.unwrap();
        let page = recorder
            .try_query(PageRequest::default_for(PageDirection::Ascending))
            .unwrap()
            .wait()
            .await
            .unwrap();
        assert_eq!(page.events().len(), 1);
        let decoded = DiagnosticEvent::decode(page.events()[0].encoded()).unwrap();
        assert_eq!(decoded.code(), DiagnosticEventCode::RequestFailed);
        assert_eq!(decoded.level(), DiagnosticLevel::Info);
        let wire: Value = serde_json::from_slice(page.events()[0].encoded()).unwrap();
        assert_eq!(wire["component"], "sessions");
        assert_eq!(wire["correlations"]["request_id"], request_id);
        assert_eq!(wire["measurements"]["duration_micros"], 12_345);
        drop(recorder);
        owner_task.abort();
    }

    #[test]
    fn internal_failures_are_durable_warning_candidates() {
        let draft = request_diagnostic_draft(
            "019844f0-4de0-7000-8000-000000000032",
            "2026-07-27T12:00:00Z",
            StatusCode::INTERNAL_SERVER_ERROR,
            DiagnosticComponent::Storage,
            1,
        );
        assert!(draft.is_ok());
    }
}
