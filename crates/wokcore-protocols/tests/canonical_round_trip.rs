use std::collections::BTreeMap;

use serde_json::{Value, json};
use wokcore_protocols::canonical::{
    AdapterKind, CanonicalEvent, CanonicalRequest, GatewayError, ImageDetail, InputItem,
    PublicModelId, ReasoningOptions, RequestId, RetryClass, ThreadKey, ToolDefinition, Usage,
};

fn fixture_request() -> CanonicalRequest {
    CanonicalRequest {
        request_id: RequestId::new("req_test"),
        model: PublicModelId::new("openai/gpt-test"),
        thread_key: Some(ThreadKey::new("thread_test")),
        input: vec![
            InputItem::Text {
                text: "hello".to_owned(),
            },
            InputItem::ImageUrl {
                url: "https://example.com/image.png".parse().unwrap(),
                detail: Some(ImageDetail::High),
            },
            InputItem::ToolResult {
                call_id: "call_1".to_owned(),
                output: json!({"ok": true}),
            },
        ],
        tools: vec![
            ToolDefinition {
                name: "read_file".to_owned(),
                description: Some("Read a file".to_owned()),
                input_schema: json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                }),
                extensions: BTreeMap::new(),
            }
            .with_extension("strict", json!(true)),
        ],
        stream: true,
        reasoning: Some(ReasoningOptions {
            effort: Some("high".to_owned()),
            extensions: BTreeMap::from([("summary".to_owned(), json!("auto"))]),
        }),
        extensions: BTreeMap::new(),
    }
}

#[test]
fn canonical_request_round_trips_without_losing_extensions() {
    let request = fixture_request().with_extension("vendor_field", serde_json::json!({"x": 1}));

    let encoded = serde_json::to_vec(&request).unwrap();
    let decoded: CanonicalRequest = serde_json::from_slice(&encoded).unwrap();

    assert_eq!(decoded, request);
}

#[test]
fn canonical_request_serializes_only_the_stable_roadmap_fields() {
    let encoded = serde_json::to_value(fixture_request()).unwrap();
    let keys = encoded
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();

    assert_eq!(
        keys,
        [
            "extensions",
            "input",
            "model",
            "reasoning",
            "request_id",
            "stream",
            "thread_key",
            "tools",
        ]
    );
}

#[test]
fn canonical_newtypes_are_transparent_and_minimally_accessible() {
    let request_id = RequestId::new("req_123");
    let model = PublicModelId::new("provider/model");
    let thread = ThreadKey::new("thread:123");

    assert_eq!(request_id.as_str(), "req_123");
    assert_eq!(model.as_str(), "provider/model");
    assert_eq!(thread.as_str(), "thread:123");
    assert_eq!(request_id.to_string(), "req_123");
    assert_eq!(model.to_string(), "provider/model");
    assert_eq!(thread.to_string(), "thread:123");
    assert_eq!(serde_json::to_value(&request_id).unwrap(), json!("req_123"));
    assert_eq!(
        serde_json::from_value::<RequestId>(json!("req_123")).unwrap(),
        request_id
    );
}

#[test]
fn adapter_kind_has_one_stable_wire_name_for_every_adapter() {
    let cases = [
        (AdapterKind::OpenAiResponses, "open_ai_responses"),
        (AdapterKind::OpenAiChat, "open_ai_chat"),
        (AdapterKind::Anthropic, "anthropic"),
        (AdapterKind::Gemini, "gemini"),
        (AdapterKind::AzureOpenAi, "azure_open_ai"),
        (AdapterKind::Cursor, "cursor"),
    ];

    for (adapter, wire_name) in cases {
        let encoded = serde_json::to_value(adapter).unwrap();
        assert_eq!(encoded, json!(wire_name));
        assert_eq!(
            serde_json::from_value::<AdapterKind>(encoded).unwrap(),
            adapter
        );
    }
}

#[test]
fn input_items_have_stable_tags_and_round_trip() {
    let cases = [
        (
            InputItem::Text {
                text: "hello".to_owned(),
            },
            "text",
        ),
        (
            InputItem::ImageUrl {
                url: "https://example.com/image.png".parse().unwrap(),
                detail: Some(ImageDetail::Auto),
            },
            "image_url",
        ),
        (
            InputItem::ToolResult {
                call_id: "call_1".to_owned(),
                output: json!({"content": "done"}),
            },
            "tool_result",
        ),
    ];

    for (item, tag) in cases {
        let encoded = serde_json::to_value(&item).unwrap();
        assert_eq!(encoded["type"], tag);
        assert_eq!(serde_json::from_value::<InputItem>(encoded).unwrap(), item);
    }
}

#[test]
fn image_detail_has_stable_wire_names() {
    for (detail, wire_name) in [
        (ImageDetail::Auto, "auto"),
        (ImageDetail::Low, "low"),
        (ImageDetail::High, "high"),
    ] {
        let encoded = serde_json::to_value(detail).unwrap();
        assert_eq!(encoded, json!(wire_name));
        assert_eq!(
            serde_json::from_value::<ImageDetail>(encoded).unwrap(),
            detail
        );
    }
}

#[test]
fn canonical_events_have_stable_tags_and_round_trip() {
    let usage = Usage {
        input_tokens: 10,
        output_tokens: 4,
        cached_input_tokens: Some(3),
        reasoning_tokens: Some(2),
        extensions: BTreeMap::from([("vendor_usage".to_owned(), json!({"x": 1}))]),
    };
    let cases = [
        (
            CanonicalEvent::Created {
                response_id: "response_1".to_owned(),
            },
            "created",
        ),
        (
            CanonicalEvent::OutputTextDelta {
                item_id: "item_1".to_owned(),
                delta: "hello".to_owned(),
            },
            "output_text_delta",
        ),
        (
            CanonicalEvent::ReasoningDelta {
                item_id: "item_2".to_owned(),
                delta: "thinking".to_owned(),
            },
            "reasoning_delta",
        ),
        (
            CanonicalEvent::ToolCallDelta {
                item_id: "item_3".to_owned(),
                call_id: "call_1".to_owned(),
                name: "lookup_weather".to_owned(),
                delta: "{\"path\":".to_owned(),
            },
            "tool_call_delta",
        ),
        (CanonicalEvent::Usage(usage), "usage"),
        (CanonicalEvent::Completed, "completed"),
        (
            CanonicalEvent::Failed(GatewayError::upstream_5xx(503)),
            "failed",
        ),
    ];

    for (event, tag) in cases {
        let encoded = serde_json::to_value(&event).unwrap();
        assert_eq!(encoded["type"], tag);
        assert_eq!(
            serde_json::from_value::<CanonicalEvent>(encoded).unwrap(),
            event
        );
    }
}

#[test]
fn gateway_error_categories_have_stable_public_contracts() {
    let cases = [
        (
            GatewayError::invalid_request(),
            "invalid_request",
            400,
            RetryClass::Never,
            "The request is invalid.",
        ),
        (
            GatewayError::unknown_model(),
            "model_not_found",
            404,
            RetryClass::Never,
            "The requested model is not available.",
        ),
        (
            GatewayError::unsupported_capability(),
            "unsupported_capability",
            422,
            RetryClass::Never,
            "The requested capability is not supported.",
        ),
        (
            GatewayError::no_executor(),
            "no_executor",
            501,
            RetryClass::Never,
            "No executor is configured for the requested operation.",
        ),
        (
            GatewayError::upstream_auth("Authorization: Bearer secret-token"),
            "upstream_auth",
            502,
            RetryClass::RefreshCredentials,
            "The upstream account needs to be authenticated again.",
        ),
        (
            GatewayError::rate_limited(Some(12)),
            "rate_limited",
            429,
            RetryClass::AfterDelay,
            "The request was rate limited.",
        ),
        (
            GatewayError::upstream_5xx(503),
            "upstream_error",
            502,
            RetryClass::BeforeFirstEvent,
            "The upstream service failed.",
        ),
        (
            GatewayError::transport("token=network-secret"),
            "upstream_unavailable",
            502,
            RetryClass::BeforeFirstEvent,
            "The upstream service is unavailable.",
        ),
        (
            GatewayError::internal("Bearer internal-secret"),
            "internal_error",
            500,
            RetryClass::Never,
            "An internal gateway error occurred.",
        ),
    ];

    for (error, code, status, retry_class, message) in cases {
        assert_eq!(error.code(), code);
        assert_eq!(error.http_status(), status);
        assert_eq!(error.retry_class(), retry_class);
        assert_eq!(error.public_message(), message);

        let encoded = serde_json::to_value(&error).unwrap();
        assert_eq!(encoded["type"], code);
        let decoded: GatewayError = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, error);
    }
}

#[test]
fn retry_classes_have_stable_wire_names() {
    for (retry_class, wire_name) in [
        (RetryClass::Never, "never"),
        (RetryClass::RefreshCredentials, "refresh_credentials"),
        (RetryClass::AfterDelay, "after_delay"),
        (RetryClass::BeforeFirstEvent, "before_first_event"),
    ] {
        let encoded = serde_json::to_value(retry_class).unwrap();
        assert_eq!(encoded, json!(wire_name));
        assert_eq!(
            serde_json::from_value::<RetryClass>(encoded).unwrap(),
            retry_class
        );
    }
}

fn assert_error_outputs_are_redacted(error: &GatewayError, secrets: &[&str]) {
    let cloned = error.clone();
    let outputs = [
        format!("{error:?}"),
        error.to_string(),
        serde_json::to_string(error).unwrap(),
        format!("{cloned:?}"),
        cloned.to_string(),
        serde_json::to_string(&cloned).unwrap(),
    ];

    for output in outputs {
        let normalized = output.to_ascii_lowercase();
        for secret in secrets {
            assert!(
                !normalized.contains(&secret.to_ascii_lowercase()),
                "sensitive text leaked through protocol error output: {output}"
            );
        }
    }
}

#[test]
fn gateway_error_debug_display_and_serde_never_reveal_sensitive_details() {
    let secrets = [
        "Authorization: Basic dXNlcjpwYXNz",
        "Bearer secret-token",
        "access_token=provider-secret",
        "token=internal-secret",
    ];
    let probes = [
        secrets[0],
        secrets[1],
        secrets[2],
        secrets[3],
        "authorization",
        "bearer",
        "access_token",
        "token",
        "provider-secret",
        "internal-secret",
    ];
    let errors = [
        GatewayError::invalid_request(),
        GatewayError::unknown_model(),
        GatewayError::unsupported_capability(),
        GatewayError::upstream_auth(secrets[0]),
        GatewayError::rate_limited(Some(12)),
        GatewayError::upstream_5xx(503),
        GatewayError::upstream_response(502, secrets[1]),
        GatewayError::transport(secrets[2]),
        GatewayError::internal(secrets[3]),
    ];

    for error in errors {
        assert_error_outputs_are_redacted(&error, &probes);
    }

    for error in [
        GatewayError::upstream_auth(secrets[0]),
        GatewayError::upstream_response(502, secrets[1]),
        GatewayError::transport(secrets[2]),
        GatewayError::internal(secrets[3]),
    ] {
        assert!(format!("{error:?}").contains("[redacted]"));
        assert_eq!(
            serde_json::to_value(error).unwrap()["diagnostic"],
            json!("[redacted]")
        );
    }
}

#[test]
fn gateway_error_ignores_secret_bearing_fields_for_safe_categories() {
    let secret = "Authorization: Bearer malicious-json-token";
    for (encoded, expected_type) in [
        (
            json!({"type": "model_not_found", "model": secret}),
            "model_not_found",
        ),
        (
            json!({"type": "unsupported_capability", "capability": secret}),
            "unsupported_capability",
        ),
    ] {
        let error: GatewayError = serde_json::from_value(encoded).unwrap();

        assert_error_outputs_are_redacted(&error, &[secret, "authorization", "bearer", "token"]);
        assert_eq!(
            serde_json::to_value(error).unwrap(),
            json!({"type": expected_type})
        );
    }
}

#[test]
fn tool_reasoning_and_usage_extension_maps_round_trip() {
    let request = fixture_request();
    let encoded = serde_json::to_value(&request).unwrap();

    assert_eq!(
        encoded["tools"][0]["extensions"]["strict"],
        Value::Bool(true)
    );
    assert_eq!(encoded["reasoning"]["extensions"]["summary"], json!("auto"));

    let decoded: CanonicalRequest = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded, request);
}

#[test]
fn nested_extension_maps_preserve_keys_that_collide_with_canonical_fields() {
    let tool = ToolDefinition {
        name: "canonical_name".to_owned(),
        description: None,
        input_schema: json!({"type": "object"}),
        extensions: BTreeMap::from([("name".to_owned(), json!("vendor_name"))]),
    };
    let reasoning = ReasoningOptions {
        effort: Some("high".to_owned()),
        extensions: BTreeMap::from([("effort".to_owned(), json!("vendor_effort"))]),
    };
    let usage = Usage {
        input_tokens: 10,
        output_tokens: 4,
        cached_input_tokens: None,
        reasoning_tokens: None,
        extensions: BTreeMap::from([("input_tokens".to_owned(), json!(99))]),
    };

    let encoded_tool = serde_json::to_value(&tool).unwrap();
    assert_eq!(encoded_tool["name"], json!("canonical_name"));
    assert_eq!(encoded_tool["extensions"]["name"], json!("vendor_name"));
    assert_eq!(
        serde_json::from_value::<ToolDefinition>(encoded_tool).unwrap(),
        tool
    );

    let encoded_reasoning = serde_json::to_value(&reasoning).unwrap();
    assert_eq!(encoded_reasoning["effort"], json!("high"));
    assert_eq!(
        encoded_reasoning["extensions"]["effort"],
        json!("vendor_effort")
    );
    assert_eq!(
        serde_json::from_value::<ReasoningOptions>(encoded_reasoning).unwrap(),
        reasoning
    );

    let encoded_usage = serde_json::to_value(&usage).unwrap();
    assert_eq!(encoded_usage["input_tokens"], json!(10));
    assert_eq!(encoded_usage["extensions"]["input_tokens"], json!(99));
    assert_eq!(
        serde_json::from_value::<Usage>(encoded_usage).unwrap(),
        usage
    );
}

#[test]
fn unknown_top_level_fields_are_ignored_in_nested_extension_types() {
    let tool: ToolDefinition = serde_json::from_value(json!({
        "name": "read_file",
        "description": null,
        "input_schema": {"type": "object"},
        "extensions": {"vendor_tool": true},
        "unknown_top_level": "ignored",
    }))
    .unwrap();
    assert_eq!(
        tool.extensions,
        BTreeMap::from([("vendor_tool".to_owned(), json!(true))])
    );
    let encoded_tool = serde_json::to_value(tool).unwrap();
    assert!(encoded_tool.get("unknown_top_level").is_none());

    let reasoning: ReasoningOptions = serde_json::from_value(json!({
        "effort": "high",
        "extensions": {"vendor_reasoning": 1},
        "unknown_top_level": "ignored",
    }))
    .unwrap();
    assert_eq!(
        reasoning.extensions,
        BTreeMap::from([("vendor_reasoning".to_owned(), json!(1))])
    );
    let encoded_reasoning = serde_json::to_value(reasoning).unwrap();
    assert!(encoded_reasoning.get("unknown_top_level").is_none());

    let usage: Usage = serde_json::from_value(json!({
        "input_tokens": 10,
        "output_tokens": 4,
        "cached_input_tokens": null,
        "reasoning_tokens": null,
        "extensions": {"vendor_usage": 2},
        "unknown_top_level": "ignored",
    }))
    .unwrap();
    assert_eq!(
        usage.extensions,
        BTreeMap::from([("vendor_usage".to_owned(), json!(2))])
    );
    let encoded_usage = serde_json::to_value(usage).unwrap();
    assert!(encoded_usage.get("unknown_top_level").is_none());
}
