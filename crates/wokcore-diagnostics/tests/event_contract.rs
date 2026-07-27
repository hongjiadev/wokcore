use std::{ffi::OsStr, path::Path};

use sha2::{Digest, Sha256};
use wokcore_diagnostics::{
    event::{
        BuildIdentity, CapabilityVersion, Correlations, DiagnosticComponent, DiagnosticDecision,
        DiagnosticError, DiagnosticEvent, DiagnosticEventCode, DiagnosticEventDraft,
        DiagnosticLevel, ErrorCode, ErrorSourceCode, EventId, FailoverDecision, GitCommit,
        MAX_PREPARED_EVENT_BYTES, MAX_SAFE_SUMMARY_BYTES, Measurements, ModelId, OpaqueAccountId,
        OpaqueSessionId, PlatformCategory, PreparedDiagnosticEvent, ProviderContext,
        ProviderProtocol, RequestId, RetryDecision, RouteId, StageCode, StateTransition,
        TokenCounts, TraceId, UtcTimestamp, WokcoreVersion,
    },
    recorder::{DiagnosticRecorder, RecordOutcome},
    redaction::{
        RedactedSummaries, RedactedSummary, SensitiveValue, SensitiveValues, StructuralObservation,
        StructuralObservations, StructuralSummaryInput, build_structural_summary,
    },
    ring::{PageDirection, PageRequest},
};

fn event_id(identity: u64) -> EventId {
    EventId::parse(&format!("018f47a2-4c1d-7a8f-9b2d-{identity:012x}")).unwrap()
}

fn build_identity() -> BuildIdentity {
    BuildIdentity::new(
        WokcoreVersion::parse("0.1.0").unwrap(),
        GitCommit::parse("0123456789abcdef0123456789abcdef01234567").unwrap(),
        1,
        CapabilityVersion::new(3),
    )
}

fn sensitive_corpus() -> SensitiveValues<'static> {
    SensitiveValues::new()
        .push(SensitiveValue::authorization(
            "Bearer authorization_秘密_🧪",
        ))
        .unwrap()
        .push(SensitiveValue::cookie("sid=cookie_秘密"))
        .unwrap()
        .push(SensitiveValue::body(b"prompt body token credential"))
        .unwrap()
        .push(SensitiveValue::path(OsStr::new(
            r"C:\Users\Alice\.config\secret",
        )))
        .unwrap()
        .push(SensitiveValue::token("sk-token-canary"))
        .unwrap()
        .push(SensitiveValue::credential("credential-password-canary"))
        .unwrap()
}

fn observations(values: &[StructuralObservation]) -> StructuralObservations {
    values
        .iter()
        .copied()
        .try_fold(StructuralObservations::new(), |current, value| {
            current.push(value)
        })
        .unwrap()
}

fn full_summary() -> RedactedSummary {
    build_structural_summary(
        StructuralSummaryInput::new(
            ProviderProtocol::OpenAiResponses,
            StageCode::Upstream,
            RetryDecision::Scheduled,
            FailoverDecision::Selected,
            true,
        )
        .with_platform(PlatformCategory::Network)
        .with_model(ModelId::parse("gpt-5").unwrap())
        .with_observations(observations(&[StructuralObservation::RouteSelected])),
        sensitive_corpus(),
    )
    .unwrap()
}

fn full_draft(identity: u64, summaries: RedactedSummaries) -> DiagnosticEventDraft {
    DiagnosticEventDraft::new(
        event_id(identity),
        UtcTimestamp::parse("2026-07-26T12:30:00Z").unwrap(),
        DiagnosticLevel::Warn,
        DiagnosticComponent::Router,
        DiagnosticEventCode::RequestFailed,
        build_identity(),
    )
    .with_correlations(Correlations::new(
        Some(RequestId::parse("req_01J3TEST").unwrap()),
        Some(TraceId::parse("trace_01J3TEST").unwrap()),
        None,
        None,
        None,
        Some(OpaqueSessionId::parse("session_opaque_01").unwrap()),
    ))
    .with_provider(
        ProviderContext::new(ProviderProtocol::OpenAiResponses)
            .with_model(ModelId::parse("gpt-5").unwrap())
            .with_route(RouteId::parse("primary").unwrap())
            .with_opaque_account(OpaqueAccountId::parse("account_opaque_01").unwrap()),
    )
    .with_decision(DiagnosticDecision::new(
        StateTransition::ReadyToDegraded,
        RetryDecision::Scheduled,
        FailoverDecision::Selected,
    ))
    .with_measurements(Measurements::new(
        StageCode::Upstream,
        12_345,
        1_024,
        2_048,
        TokenCounts::new(11, 22, 3, 4),
    ))
    .with_error(
        DiagnosticError::new(
            ErrorCode::UpstreamTimeout,
            [ErrorSourceCode::Router, ErrorSourceCode::Provider],
            PlatformCategory::Network,
        )
        .unwrap(),
    )
    .with_redacted_summaries(summaries)
}

fn one_summary(summary: RedactedSummary) -> RedactedSummaries {
    RedactedSummaries::new().push(summary).unwrap()
}

async fn record_one(draft: DiagnosticEventDraft) -> PreparedDiagnosticEvent {
    let (recorder, owner) = DiagnosticRecorder::new();
    assert_eq!(recorder.try_record(Ok(draft)), RecordOutcome::Accepted);
    let pending = recorder
        .try_query(PageRequest::default_for(PageDirection::Ascending))
        .unwrap();
    let owner_task = tokio::spawn(owner.run());
    let page = pending.wait().await.unwrap();
    owner_task.abort();
    assert_eq!(page.events().len(), 1);
    page.events()[0].clone()
}

#[tokio::test]
async fn schema_v1_full_event_serializes_to_the_exact_allow_list() {
    let prepared = record_one(full_draft(1, one_summary(full_summary()))).await;
    let summary_text = "protocol=open_ai_responses;stage=upstream;retry=scheduled;\
        failover=selected;streaming=true;platform=network;model=gpt-5;\
        observations=route_selected";
    let expected = serde_json::json!({
        "schema_version": 1,
        "sequence": "00000000000000000001",
        "event_id": "018f47a2-4c1d-7a8f-9b2d-000000000001",
        "occurred_at": "2026-07-26T12:30:00Z",
        "level": "warn",
        "component": "router",
        "code": "request_failed",
        "correlations": {
            "request_id": "req_01J3TEST",
            "trace_id": "trace_01J3TEST",
            "attempt_id": null,
            "client_id": null,
            "parent_event_id": null,
            "opaque_session_id": "session_opaque_01"
        },
        "build": {
            "wokcore_version": "0.1.0",
            "git_commit": "0123456789abcdef0123456789abcdef01234567",
            "api_major": 1,
            "capability_version": 3
        },
        "provider": {
            "protocol": "open_ai_responses",
            "model": "gpt-5",
            "route": "primary",
            "opaque_account_id": "account_opaque_01"
        },
        "decision": {
            "state_transition": "ready_to_degraded",
            "retry": "scheduled",
            "failover": "selected"
        },
        "measurements": {
            "stage": "upstream",
            "duration_micros": 12345,
            "request_bytes": 1024,
            "response_bytes": 2048,
            "tokens": {"input": 11, "output": 22, "cached": 3, "reasoning": 4}
        },
        "error": {
            "code": "upstream_timeout",
            "source_chain": ["router", "provider"],
            "platform": "network"
        },
        "summaries": [{
            "text": summary_text,
            "truncated": false,
            "original_safe_utf8_bytes": summary_text.len(),
            "full_safe_sha256": format!("{:x}", Sha256::digest(summary_text.as_bytes()))
        }],
        "redaction_counts": {
            "authorization_values_removed": 1,
            "cookie_values_removed": 1,
            "body_values_removed": 1,
            "path_values_removed": 1,
            "token_values_removed": 1,
            "credential_values_removed": 1
        }
    });
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(prepared.encoded()).unwrap(),
        expected
    );
    assert_eq!(prepared.encoded_len(), prepared.encoded().len());
    assert!(!prepared.encoded().contains(&b'\n'));

    let decoded = DiagnosticEvent::decode(prepared.encoded()).unwrap();
    assert_eq!(decoded.sequence(), 1);
    assert_eq!(decoded.event_id(), event_id(1));
    assert_eq!(serde_json::to_vec(&decoded).unwrap(), prepared.encoded());
}

#[test]
fn persistent_event_graph_has_no_free_form_or_forbidden_escape_hatch() {
    let event_source =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/event.rs"))
            .unwrap();
    let ring_source =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ring.rs")).unwrap();
    let redaction_source =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/redaction.rs"))
            .unwrap();

    for forbidden in [
        "pub message:",
        "pub details:",
        "pub context:",
        "pub metadata:",
        "pub payload:",
        "pub body:",
        "pub path:",
        "pub headers:",
        "pub authorization:",
        "pub credential:",
        "#[serde(flatten)]",
        "serde_json::Value",
        "HashMap",
        "BTreeMap",
        "Box<dyn",
    ] {
        assert!(!event_source.contains(forbidden), "{forbidden}");
    }
    assert!(!event_source.contains("impl<'de> Deserialize<'de> for DiagnosticEvent"));
    assert!(!event_source.contains("pub fn prepare(self)"));
    assert!(!event_source.contains("pub fn with_summaries"));
    assert!(!event_source.contains("pub fn with_redaction_counts"));
    assert!(!event_source.contains("pub struct RedactionCounts"));
    assert!(!ring_source.contains("pub struct DiagnosticRing"));
    assert!(!ring_source.contains("pub fn insert(&mut self"));
    assert!(!redaction_source.contains("SafeSummaryGlyph"));
    assert!(!redaction_source.contains("padding_units"));
    assert!(!redaction_source.contains("pub fn into_parts"));
    assert!(event_source.contains("request_bytes: u64"));
    assert!(event_source.contains("response_bytes: u64"));
    assert!(event_source.contains("tokens: TokenCounts"));
}

#[test]
fn safe_summary_truncates_after_redaction_and_hashes_only_the_safe_full_form() {
    let mut typed = StructuralObservations::new();
    for _ in 0..256 {
        typed = typed.push(StructuralObservation::JsonShape).unwrap();
    }
    let redacted = build_structural_summary(
        StructuralSummaryInput::new(
            ProviderProtocol::OpenAiResponses,
            StageCode::Upstream,
            RetryDecision::Scheduled,
            FailoverDecision::Selected,
            true,
        )
        .with_observations(typed),
        sensitive_corpus(),
    )
    .unwrap();
    let value = serde_json::to_value(redacted.summary()).unwrap();
    let observation = r#"shape={"event":"provider_response","fields":["status","duration","tokens"],"escaped":"\\\""}"#;
    let full = format!(
        "protocol=open_ai_responses;stage=upstream;retry=scheduled;failover=selected;\
         streaming=true;platform=none;model=unavailable;observations={}",
        std::iter::repeat_n(observation, 256)
            .collect::<Vec<_>>()
            .join("|")
    );
    assert!(full.len() > MAX_SAFE_SUMMARY_BYTES);
    let retained = value["text"].as_str().unwrap();
    assert_eq!(retained.len(), MAX_SAFE_SUMMARY_BYTES);
    assert!(retained.is_char_boundary(retained.len()));
    assert_eq!(value["truncated"], true);
    assert_eq!(value["original_safe_utf8_bytes"], full.len());
    assert_eq!(
        value["full_safe_sha256"],
        format!("{:x}", Sha256::digest(full.as_bytes()))
    );
    for canary in [
        "authorization_秘密_🧪",
        "cookie_秘密",
        "prompt body token credential",
        r"C:\Users\Alice\.config\secret",
        "sk-token-canary",
        "credential-password-canary",
    ] {
        assert!(!retained.contains(canary));
    }
}

#[tokio::test]
async fn truncated_structural_summary_remains_preparable_inside_a_full_envelope() {
    let mut typed = StructuralObservations::new();
    for _ in 0..256 {
        typed = typed.push(StructuralObservation::JsonShape).unwrap();
    }
    let redacted = build_structural_summary(
        StructuralSummaryInput::new(
            ProviderProtocol::OpenAiResponses,
            StageCode::Upstream,
            RetryDecision::Scheduled,
            FailoverDecision::Selected,
            true,
        )
        .with_platform(PlatformCategory::Network)
        .with_model(ModelId::parse("gpt-5").unwrap())
        .with_observations(typed),
        sensitive_corpus(),
    )
    .unwrap();
    let observation = r#"shape={"event":"provider_response","fields":["status","duration","tokens"],"escaped":"\\\""}"#;
    let full = format!(
        "protocol=open_ai_responses;stage=upstream;retry=scheduled;failover=selected;\
         streaming=true;platform=network;model=gpt-5;observations={}",
        std::iter::repeat_n(observation, 256)
            .collect::<Vec<_>>()
            .join("|")
    );
    assert!(full.len() > MAX_SAFE_SUMMARY_BYTES);
    assert_eq!(
        serde_json::to_value(redacted.summary()).unwrap()["truncated"],
        true
    );

    let (recorder, owner) = DiagnosticRecorder::new();
    let owner_task = tokio::spawn(owner.run());
    assert_eq!(
        recorder.try_record(Ok(full_draft(9, one_summary(redacted)))),
        RecordOutcome::Accepted
    );
    let page = recorder
        .try_query(PageRequest::default_for(PageDirection::Ascending))
        .unwrap()
        .wait()
        .await
        .unwrap();
    owner_task.abort();
    let prepared = &page.events()[0];
    let value = serde_json::from_slice::<serde_json::Value>(prepared.encoded()).unwrap();
    assert!(prepared.encoded_len() <= MAX_PREPARED_EVENT_BYTES);
    assert!(!prepared.encoded().contains(&b'\n'));
    assert_eq!(value["summaries"][0]["truncated"], true);
    assert_eq!(
        value["summaries"][0]["text"].as_str().unwrap().len(),
        MAX_SAFE_SUMMARY_BYTES
    );
    assert_eq!(
        value["summaries"][0]["original_safe_utf8_bytes"],
        full.len()
    );
    assert_eq!(
        value["summaries"][0]["full_safe_sha256"],
        format!("{:x}", Sha256::digest(full.as_bytes()))
    );
    for canary in [
        "authorization_秘密_🧪",
        "cookie_秘密",
        "prompt body token credential",
        r"C:\Users\Alice\.config\secret",
        "sk-token-canary",
        "credential-password-canary",
    ] {
        assert!(
            !prepared
                .encoded()
                .windows(canary.len())
                .any(|window| window == canary.as_bytes())
        );
    }
    assert_eq!(
        DiagnosticEvent::decode(prepared.encoded())
            .unwrap_err()
            .to_string(),
        "invalid diagnostic event"
    );
}

fn cap_summary(json_shapes: usize, cache_hits: usize, cache_misses: usize) -> RedactedSummary {
    let mut typed = StructuralObservations::new()
        .push(StructuralObservation::LocalizedCategory)
        .unwrap()
        .push(StructuralObservation::EmojiCategory)
        .unwrap();
    for _ in 0..json_shapes {
        typed = typed.push(StructuralObservation::JsonShape).unwrap();
    }
    for _ in 0..cache_hits {
        typed = typed.push(StructuralObservation::CacheHit).unwrap();
    }
    for _ in 0..cache_misses {
        typed = typed.push(StructuralObservation::CacheMiss).unwrap();
    }
    build_structural_summary(
        StructuralSummaryInput::new(
            ProviderProtocol::OpenAiResponses,
            StageCode::Upstream,
            RetryDecision::Scheduled,
            FailoverDecision::Selected,
            true,
        )
        .with_platform(PlatformCategory::Network)
        .with_model(ModelId::parse("gpt-5").unwrap())
        .with_observations(typed),
        sensitive_corpus(),
    )
    .unwrap()
}

fn cap_summaries(
    first_json_shapes: usize,
    second_json_shapes: usize,
    second_cache_hits: usize,
    second_cache_misses: usize,
) -> RedactedSummaries {
    RedactedSummaries::new()
        .push(cap_summary(first_json_shapes, 0, 0))
        .unwrap()
        .push(cap_summary(
            second_json_shapes,
            second_cache_hits,
            second_cache_misses,
        ))
        .unwrap()
}

fn find_combined_cap_shapes(
    base_event_len: usize,
    base_summary_len: usize,
    target: usize,
) -> (usize, usize, usize, usize) {
    const PREFIX: &str = "protocol=open_ai_responses;stage=upstream;retry=scheduled;\
        failover=selected;streaming=true;platform=network;model=gpt-5;observations=";
    const JSON_SHAPE: &str = r#"shape={"event":"provider_response","fields":["status","duration","tokens"],"escaped":"\\\""}"#;
    const LOCALIZED: &str = "category=结构化诊断";
    const EMOJI: &str = "category=stream_🧪_👩‍💻";
    const CACHE_HIT: &str = "cache_hit";
    const CACHE_MISS: &str = "cache_miss";

    fn encoded_content_len(value: &str) -> usize {
        serde_json::to_vec(value).unwrap().len() - 2
    }

    fn decimal_digits(value: usize) -> usize {
        value.to_string().len()
    }

    let base_text = format!("{PREFIX}{LOCALIZED}|{EMOJI}");
    let wire_overhead =
        base_summary_len - encoded_content_len(&base_text) - decimal_digits(base_text.len());
    let target_summaries_len = target - base_event_len + 2 * base_summary_len;
    let base_raw = base_text.len();
    let base_encoded = encoded_content_len(&base_text);
    let json_raw_delta = 1 + JSON_SHAPE.len();
    let json_encoded_delta = 1 + encoded_content_len(JSON_SHAPE);
    let hit_raw_delta = 1 + CACHE_HIT.len();
    let hit_encoded_delta = 1 + encoded_content_len(CACHE_HIT);
    let miss_raw_delta = 1 + CACHE_MISS.len();
    let miss_encoded_delta = 1 + encoded_content_len(CACHE_MISS);

    let summary_len = |json_shapes: usize, cache_hits: usize, cache_misses: usize| {
        let raw_len = base_raw
            + json_shapes * json_raw_delta
            + cache_hits * hit_raw_delta
            + cache_misses * miss_raw_delta;
        (raw_len <= MAX_SAFE_SUMMARY_BYTES).then(|| {
            let encoded_len = base_encoded
                + json_shapes * json_encoded_delta
                + cache_hits * hit_encoded_delta
                + cache_misses * miss_encoded_delta;
            wire_overhead + encoded_len + decimal_digits(raw_len)
        })
    };

    for first_json_shapes in 0..=254 {
        let Some(first_len) = summary_len(first_json_shapes, 0, 0) else {
            break;
        };
        for second_json_shapes in 0..=254 {
            let Some(second_base_len) = summary_len(second_json_shapes, 0, 0) else {
                break;
            };
            if first_len + second_base_len > target_summaries_len {
                break;
            }
            for short_count in 0..=(254 - second_json_shapes) {
                let Some(all_hits_len) = summary_len(second_json_shapes, short_count, 0) else {
                    break;
                };
                let current = first_len + all_hits_len;
                if current > target_summaries_len {
                    break;
                }
                let needed = target_summaries_len - current;
                for cache_misses in needed.saturating_sub(1)..=needed.saturating_add(1) {
                    if cache_misses <= short_count {
                        let cache_hits = short_count - cache_misses;
                        if summary_len(second_json_shapes, cache_hits, cache_misses).is_some_and(
                            |second_len| first_len + second_len == target_summaries_len,
                        ) {
                            return (
                                first_json_shapes,
                                second_json_shapes,
                                cache_hits,
                                cache_misses,
                            );
                        }
                    }
                }
            }
        }
    }
    panic!("closed structural observations could not form exact {target}-byte event");
}

#[tokio::test]
async fn serialized_event_cap_accepts_exactly_16k_and_rejects_one_over() {
    let base_summary_len = serde_json::to_vec(cap_summary(0, 0, 0).summary())
        .unwrap()
        .len();
    let base = record_one(full_draft(10, cap_summaries(0, 0, 0, 0))).await;
    let exact_shape = find_combined_cap_shapes(
        base.encoded_len(),
        base_summary_len,
        MAX_PREPARED_EVENT_BYTES,
    );
    let over_shape = find_combined_cap_shapes(
        base.encoded_len(),
        base_summary_len,
        MAX_PREPARED_EVENT_BYTES + 1,
    );

    let (recorder, owner) = DiagnosticRecorder::new();
    let owner_task = tokio::spawn(owner.run());
    assert_eq!(
        recorder.try_record(Ok(full_draft(
            11,
            cap_summaries(exact_shape.0, exact_shape.1, exact_shape.2, exact_shape.3,),
        ))),
        RecordOutcome::Accepted
    );
    let exact_page = recorder
        .try_query(PageRequest::with_limits(PageDirection::Descending, None, 1, 1_048_576).unwrap())
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(
        exact_page.events()[0].encoded_len(),
        MAX_PREPARED_EVENT_BYTES
    );
    let exact_value =
        serde_json::from_slice::<serde_json::Value>(exact_page.events()[0].encoded()).unwrap();
    assert_eq!(exact_value["summaries"].as_array().unwrap().len(), 2);
    assert!(
        exact_value["summaries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|summary| summary["truncated"] == false)
    );
    assert_eq!(
        recorder.try_record(Ok(full_draft(
            12,
            cap_summaries(over_shape.0, over_shape.1, over_shape.2, over_shape.3),
        ))),
        RecordOutcome::DroppedOversized
    );
    let after = recorder
        .try_query(PageRequest::with_limits(PageDirection::Descending, None, 1, 1_048_576).unwrap())
        .unwrap()
        .wait()
        .await
        .unwrap();
    owner_task.abort();
    assert_eq!(after.events()[0].event_id(), event_id(11));
    assert_eq!(recorder.metrics().oversized(), 1);
}

#[tokio::test]
async fn debug_and_errors_never_render_variable_or_sensitive_canaries() {
    let prepared = record_one(full_draft(20, one_summary(full_summary()))).await;
    let event = DiagnosticEvent::decode(prepared.encoded()).unwrap();
    for canary in [
        "Authorization: Basic YQ==",
        "Cookie: sid=秘密",
        "令牌-🧪",
        r"C:\Users\Alice\.secrets",
        "/home/alice/.config/key",
        "credential_backend_password",
    ] {
        assert!(!format!("{event:?}").contains(canary));
        assert!(!format!("{prepared:?}").contains(canary));
        assert!(
            !UtcTimestamp::parse(canary)
                .unwrap_err()
                .to_string()
                .contains(canary)
        );
    }

    let canary = "unknown_令牌_🧪_C:\\Users\\Alice";
    let mut value = serde_json::to_value(event).unwrap();
    value["code"] = serde_json::Value::String(canary.into());
    value[canary] = serde_json::Value::String("Cookie: sid=秘密".into());
    let error = DiagnosticEvent::decode(&serde_json::to_vec(&value).unwrap())
        .unwrap_err()
        .to_string();
    assert_eq!(error, "invalid diagnostic event");
    assert!(!error.contains(canary));
}

#[tokio::test]
async fn untrusted_decode_is_bounded_static_and_cannot_create_prepared_privacy_bypasses() {
    let prepared = record_one(full_draft(21, one_summary(full_summary()))).await;
    let mut value =
        serde_json::to_value(DiagnosticEvent::decode(prepared.encoded()).unwrap()).unwrap();
    for canary in [
        "Authorization: Bearer 秘密🧪",
        "Cookie: sid=credential",
        r"C:\Users\Alice\.secrets",
        "/home/alice/.config/token",
    ] {
        value["summaries"][0]["text"] = serde_json::Value::String(canary.into());
        value["summaries"][0]["truncated"] = serde_json::Value::Bool(true);
        value["summaries"][0]["original_safe_utf8_bytes"] =
            serde_json::Value::Number((canary.len() as u64 + 1).into());
        value["summaries"][0]["full_safe_sha256"] = serde_json::Value::String("00".repeat(32));
        assert_eq!(
            DiagnosticEvent::decode(&serde_json::to_vec(&value).unwrap())
                .unwrap_err()
                .to_string(),
            "invalid diagnostic event"
        );

        value["summaries"][0]["truncated"] = serde_json::Value::Bool(false);
        value["summaries"][0]["original_safe_utf8_bytes"] =
            serde_json::Value::Number((canary.len() as u64).into());
        value["summaries"][0]["full_safe_sha256"] =
            serde_json::Value::String(format!("{:x}", Sha256::digest(canary.as_bytes())));
        assert!(DiagnosticEvent::decode(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    let oversized = vec![b'x'; MAX_PREPARED_EVENT_BYTES + 1];
    assert_eq!(
        DiagnosticEvent::decode(&oversized).unwrap_err().to_string(),
        "invalid diagnostic event"
    );

    let mut bounded =
        serde_json::to_value(DiagnosticEvent::decode(prepared.encoded()).unwrap()).unwrap();
    bounded["error"]["source_chain"] =
        serde_json::json!(["router", "provider", "protocol", "platform", "storage"]);
    assert_eq!(
        DiagnosticEvent::decode(&serde_json::to_vec(&bounded).unwrap())
            .unwrap_err()
            .to_string(),
        "invalid diagnostic event"
    );
    let fifth = bounded["summaries"][0].clone();
    bounded["error"]["source_chain"] = serde_json::json!(["router"]);
    bounded["summaries"] = serde_json::json!([
        fifth.clone(),
        fifth.clone(),
        fifth.clone(),
        fifth.clone(),
        fifth
    ]);
    assert_eq!(
        DiagnosticEvent::decode(&serde_json::to_vec(&bounded).unwrap())
            .unwrap_err()
            .to_string(),
        "invalid diagnostic event"
    );
}

#[test]
fn provider_context_represents_each_optional_identifier_when_available() {
    assert_eq!(
        serde_json::to_value(ProviderContext::new(ProviderProtocol::Gemini)).unwrap(),
        serde_json::json!({
            "protocol": "gemini",
            "model": null,
            "route": null,
            "opaque_account_id": null
        })
    );
    let complete = ProviderContext::new(ProviderProtocol::OpenAiResponses)
        .with_model(ModelId::parse("gpt-5").unwrap())
        .with_route(RouteId::parse("primary").unwrap())
        .with_opaque_account(OpaqueAccountId::parse("opaque-1").unwrap());
    assert_eq!(serde_json::to_value(complete).unwrap()["model"], "gpt-5");
}

#[tokio::test]
async fn closed_codes_and_validated_identifiers_reject_unknown_or_unsafe_values() {
    let prepared = record_one(full_draft(22, one_summary(full_summary()))).await;
    let event = DiagnosticEvent::decode(prepared.encoded()).unwrap();
    let mut value = serde_json::to_value(&event).unwrap();
    value["code"] = serde_json::Value::String("future_code".into());
    assert!(DiagnosticEvent::decode(&serde_json::to_vec(&value).unwrap()).is_err());

    let mut value = serde_json::to_value(&event).unwrap();
    value["build"]["unknown"] = serde_json::Value::Bool(true);
    assert!(DiagnosticEvent::decode(&serde_json::to_vec(&value).unwrap()).is_err());
    for invalid in [
        "",
        "contains space",
        "slash/value",
        "colon:value",
        "line\nbreak",
        "escape\u{1b}",
        "bidi\u{202e}value",
    ] {
        assert!(RequestId::parse(invalid).is_err(), "{invalid:?}");
        assert!(OpaqueAccountId::parse(invalid).is_err(), "{invalid:?}");
    }
    assert!(RequestId::parse(&"r".repeat(64)).is_ok());
    assert!(RequestId::parse(&"r".repeat(65)).is_err());
    assert!(ModelId::parse("模型-安全").is_ok());
    assert!(EventId::parse("018f47a24c1d7a8f9b2d3e4f50617283").is_err());
    assert!(GitCommit::parse("0123456789ABCDEF0123456789ABCDEF01234567").is_err());
    assert!(WokcoreVersion::parse("1..0").is_err());
    assert!(UtcTimestamp::parse("2026-02-31T12:30:00Z").is_err());
    assert!(
        DiagnosticError::new(
            ErrorCode::InternalInvariant,
            [
                ErrorSourceCode::Router,
                ErrorSourceCode::Provider,
                ErrorSourceCode::Protocol,
                ErrorSourceCode::Platform,
                ErrorSourceCode::Storage,
            ],
            PlatformCategory::None,
        )
        .is_err()
    );

    let summary = build_structural_summary(
        StructuralSummaryInput::new(
            ProviderProtocol::Gemini,
            StageCode::Routing,
            RetryDecision::NotApplicable,
            FailoverDecision::NotApplicable,
            false,
        ),
        SensitiveValues::new(),
    )
    .unwrap();
    let summaries = RedactedSummaries::new()
        .push(summary.clone())
        .unwrap()
        .push(summary.clone())
        .unwrap()
        .push(summary.clone())
        .unwrap()
        .push(summary.clone())
        .unwrap();
    assert!(summaries.push(summary).is_err());
}
