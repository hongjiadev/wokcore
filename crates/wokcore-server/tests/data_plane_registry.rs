use serde_json::{Value, json};
use wokcore_protocols::{
    InboundCodecV1, InboundLimitsV1,
    canonical::{GatewayError, RequestId},
};
use wokcore_server::data_plane::{
    ClientProtocol, DataPlaneRequestError, ProtocolRegistry, RequestBodyKind,
};

#[test]
fn data_plane_registry_maps_only_the_frozen_paths() {
    for (path, protocol) in [
        ("/v1/responses", ClientProtocol::OpenAiResponses),
        (
            "/v1/chat/completions",
            ClientProtocol::OpenAiChatCompletions,
        ),
        ("/v1/messages", ClientProtocol::AnthropicMessages),
        (
            "/v1/messages/count_tokens",
            ClientProtocol::AnthropicCountTokens,
        ),
        ("/v1/models", ClientProtocol::OpenAiModels),
        (
            "/v1/images/generations",
            ClientProtocol::OpenAiImageGenerations,
        ),
        ("/v1/images/edits", ClientProtocol::OpenAiImageEdits),
    ] {
        assert_eq!(ProtocolRegistry::resolve(path), Some(protocol));
    }

    for path in [
        "/healthz",
        "/v1",
        "/v1/",
        "/v1/unknown",
        "/v1/responses/",
        "/V1/responses",
    ] {
        assert_eq!(ProtocolRegistry::resolve(path), None);
    }

    assert_eq!(
        ClientProtocol::OpenAiResponses.inbound_codec(),
        Some(InboundCodecV1::OpenAiResponses)
    );
    assert_eq!(
        ClientProtocol::OpenAiChatCompletions.inbound_codec(),
        Some(InboundCodecV1::OpenAiChatCompletions)
    );
    assert_eq!(
        ClientProtocol::AnthropicMessages.inbound_codec(),
        Some(InboundCodecV1::AnthropicMessages)
    );
    assert_eq!(
        ClientProtocol::AnthropicCountTokens.inbound_codec(),
        Some(InboundCodecV1::AnthropicCountTokens)
    );
    assert_eq!(ClientProtocol::OpenAiModels.inbound_codec(), None);
    assert!(!ClientProtocol::OpenAiModels.expects_json_body());
    assert_eq!(
        ClientProtocol::OpenAiImageGenerations.request_body_kind(),
        RequestBodyKind::Json
    );
    assert_eq!(
        ClientProtocol::OpenAiImageEdits.request_body_kind(),
        RequestBodyKind::MultipartFormData
    );
    assert!(!ClientProtocol::OpenAiImageEdits.expects_json_body());
}

#[test]
fn data_plane_versioned_codecs_decode_stream_flags_and_content_free_summaries() {
    for (path, body, expected_protocol, expected_stream) in [
        (
            "/v1/responses",
            json!({"model": "model-a", "input": "private-response-body", "stream": true}),
            ClientProtocol::OpenAiResponses,
            true,
        ),
        (
            "/v1/chat/completions",
            json!({
                "model": "model-b",
                "messages": [{"role": "user", "content": "private-chat-body"}],
                "stream": false
            }),
            ClientProtocol::OpenAiChatCompletions,
            false,
        ),
        (
            "/v1/messages",
            json!({
                "model": "model-c",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "private-anthropic-body"}],
                "stream": true
            }),
            ClientProtocol::AnthropicMessages,
            true,
        ),
        (
            "/v1/messages/count_tokens",
            json!({
                "model": "model-d",
                "messages": [{"role": "user", "content": "private-count-body"}]
            }),
            ClientProtocol::AnthropicCountTokens,
            false,
        ),
    ] {
        let encoded = serde_json::to_vec(&body).unwrap();
        let request = ProtocolRegistry::decode_json(
            path,
            RequestId::new("request-stable"),
            Some("application/json; charset=utf-8"),
            &encoded,
        )
        .unwrap();

        assert_eq!(request.protocol(), expected_protocol);
        assert_eq!(request.canonical().stream, expected_stream);
        assert_eq!(request.summary().request_id, "request-stable");
        assert_eq!(request.summary().stream, expected_stream);
        assert_eq!(request.summary().input_items, 1);
        assert_eq!(request.summary().image_items, 0);

        let debug = format!("{request:?}");
        for forbidden in [
            "private-response-body",
            "private-chat-body",
            "private-anthropic-body",
            "private-count-body",
            "authorization",
            "cookie",
        ] {
            assert!(!debug.to_ascii_lowercase().contains(forbidden));
        }
    }
}

#[test]
fn data_plane_content_type_path_and_body_errors_are_stable_and_content_free() {
    let body = br#"{"model":"model","input":"private-invalid-body"}"#;

    for content_type in [
        None,
        Some("text/json"),
        Some("application/problem+json"),
        Some("application/json; charset=latin1"),
    ] {
        let error = ProtocolRegistry::decode_json(
            "/v1/responses",
            RequestId::new("request"),
            content_type,
            body,
        )
        .unwrap_err();
        assert_eq!(error, DataPlaneRequestError::UnsupportedMediaType);
        assert_eq!(error.code(), "unsupported_media_type");
    }

    let unsupported = ProtocolRegistry::decode_json(
        "/v1/unknown",
        RequestId::new("request"),
        Some("application/json"),
        body,
    )
    .unwrap_err();
    assert_eq!(unsupported, DataPlaneRequestError::UnsupportedProtocol);
    assert_eq!(unsupported.code(), "unsupported_protocol");

    let invalid = ProtocolRegistry::decode_json(
        "/v1/responses",
        RequestId::new("request"),
        Some("application/json"),
        b"{private-invalid-body",
    )
    .unwrap_err();
    assert_eq!(invalid, DataPlaneRequestError::InvalidBody);
    assert_eq!(invalid.code(), "invalid_body");
    assert!(!format!("{invalid:?}").contains("private-invalid-body"));
}

#[test]
fn data_plane_count_tokens_rejects_streaming_requests() {
    let error = ProtocolRegistry::decode_json(
        "/v1/messages/count_tokens",
        RequestId::new("request"),
        Some("application/json"),
        &serde_json::to_vec(&json!({
            "model": "model",
            "messages": [{"role": "user", "content": "private-count-body"}],
            "stream": true
        }))
        .unwrap(),
    )
    .unwrap_err();

    assert_eq!(error, DataPlaneRequestError::InvalidBody);
}

#[test]
fn data_plane_retained_protocol_identifiers_obey_the_public_limit() {
    let limits = InboundLimitsV1 {
        max_body_bytes: 4_096,
        max_identifier_bytes: 8,
        max_input_items: 8,
        max_tools: 8,
        max_images: 8,
        max_extension_fields: 128,
        max_retained_value_bytes: 4_096,
    };

    for (codec, body) in [
        (
            InboundCodecV1::OpenAiChatCompletions,
            json!({
                "model": "model",
                "messages": [{
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "short",
                        "type": "function",
                        "function": {"name": "lookup", "arguments": "{}"}
                    }]
                }]
            }),
        ),
        (
            InboundCodecV1::AnthropicMessages,
            json!({
                "model": "model",
                "max_tokens": 64,
                "messages": [{
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "short",
                        "name": "lookup",
                        "input": {}
                    }]
                }]
            }),
        ),
    ] {
        codec
            .decode(
                RequestId::new("request"),
                &serde_json::to_vec(&body).unwrap(),
                limits,
            )
            .unwrap();
    }

    for (codec, body) in [
        (
            InboundCodecV1::OpenAiChatCompletions,
            json!({
                "model": "model",
                "messages": [{
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "123456789",
                        "type": "function",
                        "function": {"name": "lookup", "arguments": "{}"}
                    }]
                }]
            }),
        ),
        (
            InboundCodecV1::AnthropicMessages,
            json!({
                "model": "model",
                "max_tokens": 64,
                "messages": [{
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "123456789",
                        "name": "lookup",
                        "input": {}
                    }]
                }]
            }),
        ),
    ] {
        assert_eq!(
            codec
                .decode(
                    RequestId::new("request"),
                    &serde_json::to_vec(&body).unwrap(),
                    limits,
                )
                .unwrap_err(),
            GatewayError::invalid_request()
        );
    }
}

#[test]
fn data_plane_canonical_limits_bound_ids_inputs_tools_images_extensions_and_values() {
    let limits = InboundLimitsV1 {
        max_body_bytes: 2_048,
        max_identifier_bytes: 8,
        max_input_items: 2,
        max_tools: 1,
        max_images: 1,
        max_extension_fields: 1,
        max_retained_value_bytes: 256,
    };

    for body in [
        json!({"model": "123456789", "input": "x"}),
        json!({"model": "model", "input": [
            {"type": "input_text", "text": "one"},
            {"type": "input_text", "text": "two"},
            {"type": "input_text", "text": "three"}
        ]}),
        json!({"model": "model", "input": "x", "tools": [
            {"type": "function", "name": "one", "parameters": {"type": "object"}},
            {"type": "function", "name": "two", "parameters": {"type": "object"}}
        ]}),
        json!({"model": "model", "input": [
            {"type": "input_image", "image_url": "data:image/png;base64,eA=="},
            {"type": "input_image", "image_url": "data:image/png;base64,eQ=="}
        ]}),
        json!({"model": "model", "input": "x", "extension_a": 1, "extension_b": 2}),
        json!({"model": "model", "input": [{
            "type": "function_call_output",
            "call_id": "call",
            "output": "x".repeat(300)
        }]}),
    ] {
        assert_eq!(
            decode_responses(&body, limits).unwrap_err(),
            GatewayError::invalid_request()
        );
    }

    let oversized_body = vec![b' '; limits.max_body_bytes + 1];
    assert_eq!(
        InboundCodecV1::OpenAiResponses
            .decode(RequestId::new("request"), &oversized_body, limits)
            .unwrap_err(),
        GatewayError::invalid_request()
    );
}

#[test]
fn data_plane_canonical_request_debug_never_renders_retained_content() {
    let request = decode_responses(
        &json!({
            "model": "model",
            "input": "private-prompt",
            "tools": [{
                "type": "function",
                "name": "lookup",
                "parameters": {"private-schema": "private-value"}
            }],
            "private-extension": "private-extension-value"
        }),
        InboundLimitsV1::default(),
    )
    .unwrap();

    let debug = format!("{request:?}");
    for forbidden in [
        "private-prompt",
        "private-schema",
        "private-value",
        "private-extension",
        "private-extension-value",
    ] {
        assert!(!debug.contains(forbidden), "{debug}");
    }
    assert!(debug.contains("input_items"));
    assert!(debug.contains("tools"));
}

#[test]
fn data_plane_migrated_front_door_provenance_is_publicly_recorded() {
    let notice = include_str!("../../../NOTICE.md");
    let migration = include_str!("../../../MIGRATION.md");

    assert!(notice.contains("data-plane protocol registry"));
    assert!(notice.contains("WokRouter commit `226a40e08ad6c783e996ceed77b8e6dfe2640fb4`"));
    assert!(migration.contains("Provider HTTP data plane foundation 1"));
    assert!(migration.contains("crates/wokrouter-daemon/src/data_plane/registry.rs"));
}

fn decode_responses(
    body: &Value,
    limits: InboundLimitsV1,
) -> Result<wokcore_protocols::canonical::CanonicalRequest, GatewayError> {
    InboundCodecV1::OpenAiResponses.decode(
        RequestId::new("request"),
        &serde_json::to_vec(body).unwrap(),
        limits,
    )
}
