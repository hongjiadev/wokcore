use std::{collections::BTreeMap, fs, path::PathBuf};

use url::Url;
use wokcore_protocols::{
    AzureAdapter, AzureConfig, CursorAdapter, CursorConfig, GeminiAdapter, GeminiConfig,
    UpstreamLimits,
    canonical::{
        CanonicalEvent, CanonicalRequest, InputItem, PublicModelId, ReasoningOptions, RequestId,
        ToolDefinition,
    },
};

fn fixture_bytes(relative: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/protocols")
        .join(relative);
    fs::read(path).unwrap()
}

fn fixture_hex(relative: &str) -> Vec<u8> {
    let source = String::from_utf8(fixture_bytes(relative)).unwrap();
    source
        .lines()
        .filter_map(|line| line.split('#').next())
        .flat_map(str::split_whitespace)
        .map(|byte| u8::from_str_radix(byte, 16).unwrap())
        .collect()
}

fn connect_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 5);
    frame.push(0);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

fn split_connect_frames(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let payload_len =
            u32::from_be_bytes(bytes[offset + 1..offset + 5].try_into().unwrap()) as usize;
        let frame_end = offset + 5 + payload_len;
        frames.push(bytes[offset..frame_end].to_vec());
        offset = frame_end;
    }
    frames
}

fn request(model: &str, stream: bool) -> CanonicalRequest {
    CanonicalRequest {
        request_id: RequestId::new("req_fixture"),
        model: PublicModelId::new(model),
        thread_key: None,
        input: vec![InputItem::Text {
            text: "Use the weather tool.".to_owned(),
        }],
        tools: vec![ToolDefinition {
            name: "weather".to_owned(),
            description: Some("Get weather".to_owned()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }),
            extensions: BTreeMap::new(),
        }],
        stream,
        reasoning: None,
        extensions: BTreeMap::new(),
    }
}

#[test]
fn gemini_tool_call_stream_becomes_canonical_deltas() {
    let adapter = GeminiAdapter::new(
        GeminiConfig::new(
            Url::parse("https://generativelanguage.googleapis.com/").unwrap(),
            "fixture-secret",
        )
        .unwrap(),
        UpstreamLimits::default(),
    );
    let outbound = adapter
        .build_request(&request("gemini-2.5-flash", true))
        .unwrap();
    assert_eq!(
        outbound.url.as_str(),
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
    );
    assert!(!outbound.url.as_str().contains("fixture-secret"));

    let mut decoder = adapter.stream_decoder(RequestId::new("req_fixture"));
    let body = fixture_bytes("gemini/tool_stream.sse");
    let split = body.len() / 2;
    let mut events = decoder.push(&body[..split]).unwrap();
    events.extend(decoder.push(&body[split..]).unwrap());
    events.extend(decoder.finish().unwrap());

    assert!(events.iter().any(|event| matches!(
        event,
        CanonicalEvent::ToolCallDelta { call_id, name, delta, .. }
            if call_id == "call_weather"
                && name == "weather"
                && delta == "{\"city\":\"Paris\"}"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        CanonicalEvent::Usage(usage)
            if usage.input_tokens == 9 && usage.output_tokens == 4
    )));
    assert_eq!(events.last(), Some(&CanonicalEvent::Completed));
}

#[test]
fn gemini_rejects_endpoint_escape_and_bounds_stream_frames() {
    assert_eq!(
        GeminiConfig::new(Url::parse("file:///tmp/provider/").unwrap(), "secret")
            .unwrap_err()
            .code(),
        "invalid_request"
    );

    let adapter = GeminiAdapter::new(
        GeminiConfig::new(Url::parse("https://example.test/root/").unwrap(), "secret").unwrap(),
        UpstreamLimits {
            max_stream_frame_bytes: 32,
            ..UpstreamLimits::default()
        },
    );
    assert_eq!(
        adapter
            .stream_decoder(RequestId::new("req_bound"))
            .push(
                b"data: {\"text\":\"this frame is deliberately larger than thirty-two bytes\"}\n\n"
            )
            .unwrap_err()
            .code(),
        "invalid_request"
    );
}

#[test]
fn gemini_usage_wire_preserves_extensions_and_rejects_malformed_known_fields() {
    let adapter = GeminiAdapter::new(
        GeminiConfig::new(Url::parse("https://example.test/").unwrap(), "secret").unwrap(),
        UpstreamLimits::default(),
    );
    let events = adapter
        .decode_response(
            RequestId::new("req_gemini_usage"),
            br#"{
                "candidates": [],
                "usageMetadata": {
                    "promptTokenCount": 11,
                    "candidatesTokenCount": 7,
                    "cachedContentTokenCount": 3,
                    "thoughtsTokenCount": 2,
                    "vendorMetric": {"tier": "warm"},
                    "input_tokens": 999
                }
            }"#,
        )
        .unwrap();
    let usage = events
        .iter()
        .find_map(|event| match event {
            CanonicalEvent::Usage(usage) => Some(usage),
            _ => None,
        })
        .unwrap();
    assert_eq!(usage.input_tokens, 11);
    assert_eq!(usage.output_tokens, 7);
    assert_eq!(usage.cached_input_tokens, Some(3));
    assert_eq!(usage.reasoning_tokens, Some(2));
    assert_eq!(
        usage.extensions,
        BTreeMap::from([
            ("input_tokens".to_owned(), serde_json::json!(999)),
            (
                "vendorMetric".to_owned(),
                serde_json::json!({"tier": "warm"})
            ),
        ])
    );

    for usage in [
        serde_json::json!({"promptTokenCount": "11"}),
        serde_json::json!({"candidatesTokenCount": "7"}),
        serde_json::json!({"cachedContentTokenCount": "3"}),
        serde_json::json!({"thoughtsTokenCount": "2"}),
        serde_json::json!({"promptTokenCount": null}),
        serde_json::json!({"candidatesTokenCount": null}),
        serde_json::json!({"cachedContentTokenCount": null}),
        serde_json::json!({"thoughtsTokenCount": null}),
    ] {
        let body = serde_json::to_vec(&serde_json::json!({
            "candidates": [],
            "usageMetadata": usage,
        }))
        .unwrap();
        assert_eq!(
            adapter
                .decode_response(RequestId::new("req_gemini_bad_usage"), &body)
                .unwrap_err()
                .code(),
            "invalid_request"
        );
    }

    let absent = adapter
        .decode_response(
            RequestId::new("req_gemini_absent_usage"),
            br#"{"candidates":[],"usageMetadata":{}}"#,
        )
        .unwrap();
    assert!(absent.iter().any(|event| matches!(
        event,
        CanonicalEvent::Usage(usage)
            if usage.input_tokens == 0
                && usage.output_tokens == 0
                && usage.cached_input_tokens.is_none()
                && usage.reasoning_tokens.is_none()
                && usage.extensions.is_empty()
    )));
}

#[test]
fn azure_request_response_and_url_validation_are_canonical() {
    let adapter = AzureAdapter::new(
        AzureConfig::new(
            Url::parse("https://fixture.openai.azure.com/").unwrap(),
            "deployment-a",
            "2024-10-21",
            "fixture-azure-key",
        )
        .unwrap(),
        UpstreamLimits::default(),
    );
    let outbound = adapter
        .build_request(&request("public-alias", false))
        .unwrap();
    assert_eq!(
        outbound.url.as_str(),
        "https://fixture.openai.azure.com/openai/deployments/deployment-a/chat/completions?api-version=2024-10-21"
    );
    assert_eq!(
        outbound.headers.get("api-key").map(String::as_str),
        Some("fixture-azure-key")
    );
    assert!(!outbound.url.as_str().contains("fixture-azure-key"));

    let events = adapter
        .decode_response(
            RequestId::new("req_azure"),
            &fixture_bytes("azure/response/tool.json"),
        )
        .unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        CanonicalEvent::ToolCallDelta { call_id, name, delta, .. }
            if call_id == "call_azure" && name == "weather" && delta == "{\"city\":\"Paris\"}"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        CanonicalEvent::Usage(usage)
            if usage.input_tokens == 7 && usage.output_tokens == 3
    )));

    for deployment in ["", "../escape", "bad/name", "bad?query"] {
        assert_eq!(
            AzureConfig::new(
                Url::parse("https://fixture.openai.azure.com/").unwrap(),
                deployment,
                "2024-10-21",
                "secret",
            )
            .unwrap_err()
            .code(),
            "invalid_request"
        );
    }
    for deployment in [".", ".."] {
        assert_eq!(
            AzureConfig::new(
                Url::parse("https://fixture.openai.azure.com/").unwrap(),
                deployment,
                "2024-10-21",
                "secret",
            )
            .unwrap_err()
            .code(),
            "invalid_request"
        );
    }
    for api_version in ["", "v1", "2024-10-21?other=true", "2024-13-01"] {
        assert_eq!(
            AzureConfig::new(
                Url::parse("https://fixture.openai.azure.com/").unwrap(),
                "safe-deployment",
                api_version,
                "secret",
            )
            .unwrap_err()
            .code(),
            "invalid_request"
        );
    }
}

#[test]
fn azure_usage_wire_preserves_extensions_and_rejects_malformed_known_fields() {
    let adapter = AzureAdapter::new(
        AzureConfig::new(
            Url::parse("https://fixture.openai.azure.com/").unwrap(),
            "deployment-a",
            "2024-10-21",
            "secret",
        )
        .unwrap(),
        UpstreamLimits::default(),
    );
    let events = adapter
        .decode_response(
            RequestId::new("req_azure_usage"),
            br#"{
                "choices": [],
                "usage": {
                    "prompt_tokens": 13,
                    "completion_tokens": 8,
                    "prompt_tokens_details": {"cached_tokens": 5},
                    "completion_tokens_details": {"reasoning_tokens": 3},
                    "vendor_metric": ["warm"],
                    "input_tokens": 999
                }
            }"#,
        )
        .unwrap();
    let usage = events
        .iter()
        .find_map(|event| match event {
            CanonicalEvent::Usage(usage) => Some(usage),
            _ => None,
        })
        .unwrap();
    assert_eq!(usage.input_tokens, 13);
    assert_eq!(usage.output_tokens, 8);
    assert_eq!(usage.cached_input_tokens, Some(5));
    assert_eq!(usage.reasoning_tokens, Some(3));
    assert_eq!(
        usage.extensions,
        BTreeMap::from([
            ("input_tokens".to_owned(), serde_json::json!(999)),
            ("vendor_metric".to_owned(), serde_json::json!(["warm"])),
        ])
    );

    for usage in [
        serde_json::json!({"prompt_tokens": "13"}),
        serde_json::json!({"completion_tokens": "8"}),
        serde_json::json!({"prompt_tokens_details": []}),
        serde_json::json!({"completion_tokens_details": []}),
        serde_json::json!({"prompt_tokens_details": {"cached_tokens": "5"}}),
        serde_json::json!({"completion_tokens_details": {"reasoning_tokens": "3"}}),
        serde_json::json!({"prompt_tokens": null}),
        serde_json::json!({"completion_tokens": null}),
        serde_json::json!({"prompt_tokens_details": null}),
        serde_json::json!({"completion_tokens_details": null}),
        serde_json::json!({"prompt_tokens_details": {"cached_tokens": null}}),
        serde_json::json!({"completion_tokens_details": {"reasoning_tokens": null}}),
    ] {
        let body = serde_json::to_vec(&serde_json::json!({
            "choices": [],
            "usage": usage,
        }))
        .unwrap();
        assert_eq!(
            adapter
                .decode_response(RequestId::new("req_azure_bad_usage"), &body)
                .unwrap_err()
                .code(),
            "invalid_request"
        );
    }

    let absent = adapter
        .decode_response(
            RequestId::new("req_azure_absent_usage"),
            br#"{"choices":[],"usage":{}}"#,
        )
        .unwrap();
    assert!(absent.iter().any(|event| matches!(
        event,
        CanonicalEvent::Usage(usage)
            if usage.input_tokens == 0
                && usage.output_tokens == 0
                && usage.cached_input_tokens.is_none()
                && usage.reasoning_tokens.is_none()
                && usage.extensions.is_empty()
    )));
}

#[test]
fn gemini_usage_top_level_extensions_have_an_independent_collection_limit() {
    let limits = UpstreamLimits {
        max_collection_items: 1,
        ..UpstreamLimits::default()
    };
    let gemini = GeminiAdapter::new(
        GeminiConfig::new(Url::parse("https://example.test/").unwrap(), "secret").unwrap(),
        limits,
    );
    assert_eq!(
        gemini
            .decode_response(
                RequestId::new("req_gemini_usage_limit"),
                br#"{
                    "candidates": [],
                    "usageMetadata": {"vendorA": 1, "vendorB": 2}
                }"#,
            )
            .unwrap_err()
            .code(),
        "invalid_request"
    );
}

fn assert_azure_usage_collection_limit(usage: serde_json::Value) {
    let limits = UpstreamLimits {
        max_collection_items: 1,
        ..UpstreamLimits::default()
    };
    let azure = AzureAdapter::new(
        AzureConfig::new(
            Url::parse("https://example.test/").unwrap(),
            "deployment",
            "2024-10-21",
            "secret",
        )
        .unwrap(),
        limits,
    );
    let body = serde_json::to_vec(&serde_json::json!({
        "choices": [],
        "usage": usage,
    }))
    .unwrap();
    assert_eq!(
        azure
            .decode_response(RequestId::new("req_azure_usage_limit"), &body)
            .unwrap_err()
            .code(),
        "invalid_request"
    );
}

#[test]
fn azure_usage_top_level_extensions_have_an_independent_collection_limit() {
    assert_azure_usage_collection_limit(serde_json::json!({"vendor_a": 1, "vendor_b": 2}));
}

#[test]
fn azure_prompt_token_detail_extensions_have_an_independent_collection_limit() {
    assert_azure_usage_collection_limit(serde_json::json!({
        "prompt_tokens_details": {"vendor_a": 1, "vendor_b": 2}
    }));
}

#[test]
fn azure_completion_token_detail_extensions_have_an_independent_collection_limit() {
    assert_azure_usage_collection_limit(serde_json::json!({
        "completion_tokens_details": {"vendor_a": 1, "vendor_b": 2}
    }));
}

#[test]
fn azure_stream_keeps_partial_tool_arguments_opaque_until_wire_delta() {
    let adapter = AzureAdapter::new(
        AzureConfig::new(
            Url::parse("https://fixture.openai.azure.com/").unwrap(),
            "deployment-a",
            "2024-10-21",
            "secret",
        )
        .unwrap(),
        UpstreamLimits::default(),
    );
    let mut decoder = adapter.stream_decoder(RequestId::new("req_azure_stream"));
    let events = decoder
        .push(&fixture_bytes("azure/stream/tool.sse"))
        .unwrap();
    let tool_deltas = events
        .iter()
        .filter_map(|event| match event {
            CanonicalEvent::ToolCallDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tool_deltas, ["{\"city\":", "\"Paris\"}"]);
    assert_eq!(
        decoder.finish().unwrap().last(),
        Some(&CanonicalEvent::Completed)
    );
}

#[test]
fn cursor_is_opt_in_maps_events_and_never_enables_native_execution() {
    let disabled = CursorAdapter::new(
        CursorConfig::new(Url::parse("https://api2.cursor.sh/").unwrap(), false).unwrap(),
        UpstreamLimits::default(),
    );
    assert_eq!(
        disabled
            .build_request(&request("cursor/composer-2.5", true))
            .unwrap_err()
            .code(),
        "unsupported_capability"
    );

    let enabled = CursorAdapter::new(
        CursorConfig::new(Url::parse("https://api2.cursor.sh/").unwrap(), true).unwrap(),
        UpstreamLimits::default(),
    );
    let outbound = enabled
        .build_request(&request("cursor/composer-2.5", true))
        .unwrap();
    assert_eq!(
        outbound.headers.get("content-type").map(String::as_str),
        Some("application/connect+proto")
    );
    assert_eq!(outbound.body[0], 0);
    assert_eq!(
        u32::from_be_bytes(outbound.body[1..5].try_into().unwrap()) as usize,
        outbound.body.len() - 5
    );
    assert!(!outbound.body.starts_with(b"{"));
    assert!(
        outbound
            .body
            .windows("composer-2.5".len())
            .any(|window| window == b"composer-2.5")
    );

    let mut decoder = enabled.stream_decoder(RequestId::new("req_cursor"));
    let fixture = fixture_hex("cursor/stream/tool.connect.hex");
    let mut events = decoder.push(&fixture[..7]).unwrap();
    events.extend(decoder.push(&fixture[7..]).unwrap());
    events.extend(decoder.finish().unwrap());
    assert!(events.iter().any(|event| matches!(
        event,
        CanonicalEvent::ReasoningDelta { delta, .. } if delta == "Checking."
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        CanonicalEvent::ToolCallDelta { call_id, name, delta, .. }
            if call_id == "call_cursor" && name == "weather" && delta == "{\"city\":\"Paris\"}"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        CanonicalEvent::Usage(usage) if usage.output_tokens == 3
    )));
    assert_eq!(events.last(), Some(&CanonicalEvent::Completed));
}

#[test]
fn cursor_request_matches_independently_encoded_protobuf_fixture() {
    let adapter = CursorAdapter::new(
        CursorConfig::new(Url::parse("https://api2.cursor.sh/").unwrap(), true).unwrap(),
        UpstreamLimits::default(),
    );
    let request = CanonicalRequest {
        request_id: RequestId::new("req_wire"),
        model: PublicModelId::new("cursor/composer-2.5"),
        thread_key: Some(wokcore_protocols::canonical::ThreadKey::new("thread_wire")),
        input: vec![InputItem::Text {
            text: "hi".to_owned(),
        }],
        tools: Vec::new(),
        stream: true,
        reasoning: None,
        extensions: BTreeMap::new(),
    };

    assert_eq!(
        adapter.build_request(&request).unwrap().body,
        fixture_hex("cursor/request/run.connect.hex")
    );
}

#[test]
fn cursor_rejects_unmapped_reasoning_options() {
    let adapter = CursorAdapter::new(
        CursorConfig::new(Url::parse("https://api2.cursor.sh/").unwrap(), true).unwrap(),
        UpstreamLimits::default(),
    );
    let mut request = request("cursor/composer-2.5", true);
    request.reasoning = Some(ReasoningOptions {
        effort: Some("high".to_owned()),
        extensions: BTreeMap::new(),
    });

    assert_eq!(
        adapter.build_request(&request).unwrap_err().code(),
        "unsupported_capability"
    );
}

#[test]
fn cursor_rejects_missing_and_unknown_required_oneofs() {
    let adapter = CursorAdapter::new(
        CursorConfig::new(Url::parse("https://api2.cursor.sh/").unwrap(), true).unwrap(),
        UpstreamLimits::default(),
    );
    for payload in [
        Vec::new(),
        vec![0x32, 0x00],
        vec![0x0a, 0x00],
        vec![0x0a, 0x03, 0x92, 0x01, 0x00],
    ] {
        let mut decoder = adapter.stream_decoder(RequestId::new("req_unknown_oneof"));
        assert_eq!(
            decoder.push(&connect_frame(&payload)).unwrap_err().code(),
            "unsupported_capability"
        );
        assert_eq!(
            decoder.push(&connect_frame(&[])).unwrap_err().code(),
            "invalid_request"
        );
    }
}

#[test]
fn cursor_cumulative_tool_arguments_must_equal_or_prefix_extend() {
    let adapter = CursorAdapter::new(
        CursorConfig::new(Url::parse("https://api2.cursor.sh/").unwrap(), true).unwrap(),
        UpstreamLimits::default(),
    );
    let frames = split_connect_frames(&fixture_hex("cursor/stream/tool.connect.hex"));

    let mut equal = adapter.stream_decoder(RequestId::new("req_equal"));
    for frame in &frames[..4] {
        equal.push(frame).unwrap();
    }
    equal.push(&frames[3]).unwrap();
    for frame in &frames[4..] {
        equal.push(frame).unwrap();
    }
    equal.finish().unwrap();

    let mut valid_extension = adapter.stream_decoder(RequestId::new("req_extension"));
    for frame in &frames {
        valid_extension.push(frame).unwrap();
    }
    valid_extension.finish().unwrap();

    let mut shorter = adapter.stream_decoder(RequestId::new("req_shorter"));
    for frame in &frames[..5] {
        shorter.push(frame).unwrap();
    }
    assert_eq!(
        shorter.push(&frames[3]).unwrap_err().code(),
        "invalid_request"
    );

    let mut divergent_frame = frames[3].clone();
    let city = divergent_frame
        .windows(4)
        .position(|window| window == b"city")
        .unwrap();
    divergent_frame[city..city + 4].copy_from_slice(b"town");
    let mut divergent = adapter.stream_decoder(RequestId::new("req_divergent"));
    for frame in &frames[..4] {
        divergent.push(frame).unwrap();
    }
    assert_eq!(
        divergent.push(&divergent_frame).unwrap_err().code(),
        "invalid_request"
    );
}

#[test]
fn cursor_executor_hook_without_executor_is_typed_and_redacted() {
    let adapter = CursorAdapter::new(
        CursorConfig::new(
            Url::parse("https://internal.example.test/secret-base/").unwrap(),
            true,
        )
        .unwrap(),
        UpstreamLimits::default(),
    );
    let mut decoder = adapter.stream_decoder(RequestId::new("req_cursor_exec"));
    let error = decoder
        .push(&fixture_hex("cursor/stream/exec.connect.hex"))
        .unwrap_err();
    assert_eq!(error.code(), "no_executor");
    let serialized = serde_json::to_string(&error).unwrap();
    assert!(!serialized.contains("internal.example.test"));
}

#[test]
fn generated_response_and_cursor_conversation_ids_are_individually_bounded() {
    let gemini = GeminiAdapter::new(
        GeminiConfig::new(Url::parse("https://example.test/").unwrap(), "secret").unwrap(),
        UpstreamLimits {
            max_identifier_bytes: 16,
            ..UpstreamLimits::default()
        },
    );
    assert_eq!(
        gemini
            .decode_response(RequestId::new("x".repeat(32)), br#"{"candidates":[]}"#)
            .unwrap_err()
            .code(),
        "invalid_request"
    );

    let cursor = CursorAdapter::new(
        CursorConfig::new(Url::parse("https://example.test/").unwrap(), true).unwrap(),
        UpstreamLimits {
            max_identifier_bytes: 16,
            ..UpstreamLimits::default()
        },
    );
    let mut request = request("cursor/auto", true);
    request.thread_key = Some(wokcore_protocols::canonical::ThreadKey::new(
        "conversation-key-that-is-too-long",
    ));
    assert_eq!(
        cursor.build_request(&request).unwrap_err().code(),
        "invalid_request"
    );
}

#[test]
fn cursor_connect_decoder_enforces_frame_aggregate_event_and_tool_limits() {
    let fixture = fixture_hex("cursor/stream/tool.connect.hex");
    for limits in [
        UpstreamLimits {
            max_stream_frame_bytes: 8,
            ..UpstreamLimits::default()
        },
        UpstreamLimits {
            max_response_body_bytes: fixture.len() - 1,
            ..UpstreamLimits::default()
        },
        UpstreamLimits {
            max_events: 1,
            ..UpstreamLimits::default()
        },
        UpstreamLimits {
            max_tool_argument_bytes: 8,
            ..UpstreamLimits::default()
        },
    ] {
        let adapter = CursorAdapter::new(
            CursorConfig::new(Url::parse("https://api2.cursor.sh/").unwrap(), true).unwrap(),
            limits,
        );
        let mut decoder = adapter.stream_decoder(RequestId::new("req_limits"));
        assert_eq!(
            decoder.push(&fixture).unwrap_err().code(),
            "invalid_request"
        );
    }
}

#[test]
fn stream_event_limits_are_aggregate_across_push_and_finish_calls() {
    let limits = UpstreamLimits {
        max_events: 2,
        ..UpstreamLimits::default()
    };
    let gemini = GeminiAdapter::new(
        GeminiConfig::new(Url::parse("https://example.test/").unwrap(), "secret").unwrap(),
        limits,
    );
    let mut gemini_decoder = gemini.stream_decoder(RequestId::new("req_events"));
    assert_eq!(
        gemini_decoder
            .push(b"data: {\"candidates\":[]}\n\n")
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        gemini_decoder
            .push(b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"x\"}]},\"finishReason\":\"STOP\"}]}\n\n")
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        gemini_decoder.finish().unwrap_err().code(),
        "invalid_request"
    );

    let azure = AzureAdapter::new(
        AzureConfig::new(
            Url::parse("https://example.test/").unwrap(),
            "deployment",
            "2024-10-21",
            "secret",
        )
        .unwrap(),
        limits,
    );
    let mut azure_decoder = azure.stream_decoder(RequestId::new("req_events"));
    assert_eq!(
        azure_decoder
            .push(b"data: {\"id\":\"az\",\"choices\":[]}\n\n")
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        azure_decoder
            .push(b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n")
            .unwrap()
            .len(),
        1
    );
    assert!(azure_decoder.push(b"data: [DONE]\n\n").unwrap().is_empty());
    assert_eq!(
        azure_decoder.finish().unwrap_err().code(),
        "invalid_request"
    );

    let cursor = CursorAdapter::new(
        CursorConfig::new(Url::parse("https://api2.cursor.sh/").unwrap(), true).unwrap(),
        limits,
    );
    let fixture = fixture_hex("cursor/stream/tool.connect.hex");
    let first_frame_end = 16;
    let second_frame_end = first_frame_end + 20;
    let mut cursor_decoder = cursor.stream_decoder(RequestId::new("req_events"));
    assert_eq!(
        cursor_decoder
            .push(&fixture[..first_frame_end])
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        cursor_decoder
            .push(&fixture[first_frame_end..second_frame_end])
            .unwrap_err()
            .code(),
        "invalid_request"
    );
}

#[test]
fn gemini_and_azure_streams_require_clean_eof_and_formal_terminals() {
    let gemini = GeminiAdapter::new(
        GeminiConfig::new(Url::parse("https://example.test/").unwrap(), "secret").unwrap(),
        UpstreamLimits::default(),
    );
    let mut gemini_missing_terminal = gemini.stream_decoder(RequestId::new("req_gemini_missing"));
    gemini_missing_terminal
        .push(b"data: {\"candidates\":[]}\n\n")
        .unwrap();
    assert_eq!(
        gemini_missing_terminal.finish().unwrap_err().code(),
        "invalid_request"
    );
    assert_eq!(
        gemini_missing_terminal
            .push(b"data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n")
            .unwrap_err()
            .code(),
        "invalid_request"
    );

    let mut gemini_truncated = gemini.stream_decoder(RequestId::new("req_gemini_truncated"));
    gemini_truncated
        .push(b"data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\ndata: {")
        .unwrap();
    assert_eq!(
        gemini_truncated.finish().unwrap_err().code(),
        "invalid_request"
    );

    let mut gemini_complete = gemini.stream_decoder(RequestId::new("req_gemini_complete"));
    gemini_complete
        .push(b"data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n")
        .unwrap();
    assert_eq!(
        gemini_complete.finish().unwrap().last(),
        Some(&CanonicalEvent::Completed)
    );
    assert_eq!(
        gemini_complete.finish().unwrap_err().code(),
        "invalid_request"
    );
    assert_eq!(
        gemini_complete.push(b"data: {}\n\n").unwrap_err().code(),
        "invalid_request"
    );

    let azure = AzureAdapter::new(
        AzureConfig::new(
            Url::parse("https://example.test/").unwrap(),
            "deployment",
            "2024-10-21",
            "secret",
        )
        .unwrap(),
        UpstreamLimits::default(),
    );
    let mut azure_missing_done = azure.stream_decoder(RequestId::new("req_azure_missing"));
    azure_missing_done
        .push(b"data: {\"id\":\"az\",\"choices\":[]}\n\n")
        .unwrap();
    assert_eq!(
        azure_missing_done.finish().unwrap_err().code(),
        "invalid_request"
    );
    assert_eq!(
        azure_missing_done
            .push(b"data: [DONE]\n\n")
            .unwrap_err()
            .code(),
        "invalid_request"
    );

    let mut azure_truncated = azure.stream_decoder(RequestId::new("req_azure_truncated"));
    azure_truncated
        .push(b"data: {\"id\":\"az\",\"choices\":[]}\n\ndata: [DONE]")
        .unwrap();
    assert_eq!(
        azure_truncated.finish().unwrap_err().code(),
        "invalid_request"
    );

    let mut azure_complete = azure.stream_decoder(RequestId::new("req_azure_complete"));
    azure_complete
        .push(b"data: {\"id\":\"az\",\"choices\":[]}\n\ndata: [DONE]\n\n")
        .unwrap();
    assert_eq!(
        azure_complete.finish().unwrap().last(),
        Some(&CanonicalEvent::Completed)
    );
    assert_eq!(
        azure_complete.finish().unwrap_err().code(),
        "invalid_request"
    );
    assert_eq!(
        azure_complete.push(b"data: {}\n\n").unwrap_err().code(),
        "invalid_request"
    );
}

#[test]
fn cursor_connect_terminal_states_fail_closed() {
    let adapter = CursorAdapter::new(
        CursorConfig::new(Url::parse("https://api2.cursor.sh/").unwrap(), true).unwrap(),
        UpstreamLimits::default(),
    );
    let fixture = fixture_hex("cursor/stream/tool.connect.hex");
    let first_frame_end = 16;

    let mut truncated = adapter.stream_decoder(RequestId::new("req_truncated"));
    truncated.push(&fixture[..first_frame_end]).unwrap();
    assert_eq!(truncated.finish().unwrap_err().code(), "invalid_request");

    let mut ended = adapter.stream_decoder(RequestId::new("req_ended"));
    ended.push(&fixture).unwrap();
    assert_eq!(
        ended.push(&fixture[..first_frame_end]).unwrap_err().code(),
        "invalid_request"
    );

    let mut rejected_exec = adapter.stream_decoder(RequestId::new("req_exec"));
    assert_eq!(
        rejected_exec
            .push(&fixture_hex("cursor/stream/exec.connect.hex"))
            .unwrap_err()
            .code(),
        "no_executor"
    );
    assert_eq!(
        rejected_exec
            .push(&fixture[..first_frame_end])
            .unwrap_err()
            .code(),
        "invalid_request"
    );
    assert_eq!(
        rejected_exec.finish().unwrap_err().code(),
        "invalid_request"
    );
}

#[test]
fn base_urls_must_have_explicit_directory_semantics() {
    for base in [
        "https://example.test/root",
        "https://example.test/root?query=true",
        "https://user@example.test/",
    ] {
        assert_eq!(
            GeminiConfig::new(Url::parse(base).unwrap(), "secret")
                .unwrap_err()
                .code(),
            "invalid_request"
        );
    }
}

#[test]
fn response_body_limit_fails_without_retaining_the_body() {
    let adapter = AzureAdapter::new(
        AzureConfig::new(
            Url::parse("https://fixture.openai.azure.com/").unwrap(),
            "deployment-a",
            "2024-10-21",
            "secret",
        )
        .unwrap(),
        UpstreamLimits {
            max_response_body_bytes: 16,
            ..UpstreamLimits::default()
        },
    );
    let secret_body = br#"{"secret":"must-not-be-retained"}"#;
    let error = adapter
        .decode_response(RequestId::new("req_bound"), secret_body)
        .unwrap_err();
    assert_eq!(error.code(), "invalid_request");
    assert!(!format!("{error:?}").contains("must-not-be-retained"));
}

#[test]
fn upstream_http_errors_are_typed_without_body_or_endpoint_diagnostics() {
    let gemini = GeminiAdapter::new(
        GeminiConfig::new(
            Url::parse("https://internal-gemini.example.test/").unwrap(),
            "gemini-secret",
        )
        .unwrap(),
        UpstreamLimits::default(),
    );
    let azure = AzureAdapter::new(
        AzureConfig::new(
            Url::parse("https://internal-azure.example.test/").unwrap(),
            "private-deployment",
            "2024-10-21",
            "azure-secret",
        )
        .unwrap(),
        UpstreamLimits::default(),
    );
    let cursor = CursorAdapter::new(
        CursorConfig::new(
            Url::parse("https://internal-cursor.example.test/").unwrap(),
            true,
        )
        .unwrap(),
        UpstreamLimits::default(),
    );

    assert_eq!(gemini.decode_http_error(401, None).code(), "upstream_auth");
    assert_eq!(
        azure.decode_http_error(429, Some("12")).code(),
        "rate_limited"
    );
    assert_eq!(cursor.decode_http_error(503, None).code(), "upstream_error");
    for error in [
        gemini.decode_http_error(401, None),
        azure.decode_http_error(400, None),
        cursor.decode_http_error(503, None),
    ] {
        let debug = format!("{error:?}");
        for private in [
            "gemini-secret",
            "azure-secret",
            "private-deployment",
            "internal-gemini",
            "internal-azure",
            "internal-cursor",
        ] {
            assert!(!debug.contains(private));
        }
    }
}
