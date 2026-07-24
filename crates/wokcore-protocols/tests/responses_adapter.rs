use std::{collections::BTreeMap, fs, path::PathBuf};

use serde_json::{Value, json};
use wokcore_protocols::{
    ResponsesCodec, ResponsesEncodeContext, ResponsesResponseTemplate,
    canonical::{
        CanonicalEvent, GatewayError, ImageDetail, InputItem, PublicModelId, RequestId, Usage,
    },
    stream::SseDecoder,
};

fn fixture_bytes(path: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/fixtures/protocols/responses")
        .join(path);
    fs::read(path).unwrap()
}

fn fixture_json(path: &str) -> Value {
    serde_json::from_slice(&fixture_bytes(path)).unwrap()
}

const FORMAL_RESPONSE_KEYS: [&str; 24] = [
    "completed_at",
    "created_at",
    "error",
    "id",
    "incomplete_details",
    "instructions",
    "max_output_tokens",
    "metadata",
    "model",
    "object",
    "output",
    "parallel_tool_calls",
    "previous_response_id",
    "reasoning",
    "status",
    "store",
    "temperature",
    "text",
    "tool_choice",
    "tools",
    "top_p",
    "truncation",
    "usage",
    "user",
];

fn assert_formal_response_fields(response: &Value) {
    let response = response.as_object().unwrap();
    assert_eq!(
        response.len(),
        FORMAL_RESPONSE_KEYS.len(),
        "unexpected Response fields: {:?}",
        response.keys().collect::<Vec<_>>()
    );
    for key in FORMAL_RESPONSE_KEYS {
        assert!(response.contains_key(key), "missing Response field {key}");
    }
}

fn encode_context() -> ResponsesEncodeContext {
    ResponsesEncodeContext {
        model: PublicModelId::new("gpt-test"),
        created_at: 1_723_456_789,
        response: ResponsesResponseTemplate {
            completed_at: Some(1_723_456_790),
            error: None,
            incomplete_details: None,
            instructions: Some(json!("trusted instructions")),
            max_output_tokens: Some(512),
            metadata: BTreeMap::from([("fixture".to_owned(), json!("trusted"))]),
            parallel_tool_calls: false,
            previous_response_id: Some("resp_previous".to_owned()),
            reasoning: json!({"effort": "high", "summary": "auto"}),
            store: false,
            temperature: Some(0.25),
            text: json!({
                "format": {"type": "text"},
                "verbosity": "low"
            }),
            tool_choice: json!({
                "type": "function",
                "name": "read_file"
            }),
            tools: vec![json!({
                "type": "function",
                "name": "read_file",
                "parameters": {"type": "object"}
            })],
            top_p: Some(0.9),
            truncation: json!("auto"),
            user: None,
        },
    }
}

fn canonical_events() -> Vec<CanonicalEvent> {
    vec![
        CanonicalEvent::Created {
            response_id: "resp_fixture".to_owned(),
        },
        CanonicalEvent::OutputTextDelta {
            item_id: "msg_1".to_owned(),
            delta: "你好".to_owned(),
        },
        CanonicalEvent::ReasoningDelta {
            item_id: "reasoning_1".to_owned(),
            delta: "先检查".to_owned(),
        },
        CanonicalEvent::ToolCallDelta {
            item_id: "tool_1".to_owned(),
            call_id: "call_1".to_owned(),
            name: "read_file".to_owned(),
            delta: "{\"path\":\"配置.toml\"}".to_owned(),
        },
        CanonicalEvent::Usage(Usage {
            input_tokens: 10,
            output_tokens: 5,
            cached_input_tokens: Some(3),
            reasoning_tokens: Some(2),
            extensions: BTreeMap::from([("vendor_usage".to_owned(), json!("kept"))]),
        }),
        CanonicalEvent::Completed,
    ]
}

#[test]
fn decodes_tool_reasoning_multimodal_and_result_fixture() {
    let request = fixture_bytes("request/tool_reasoning.json");
    let canonical =
        ResponsesCodec::decode_request(RequestId::new("request-fixture"), &request).unwrap();

    assert_eq!(canonical.request_id.as_str(), "request-fixture");
    assert_eq!(canonical.model.as_str(), "gpt-test");
    assert_eq!(
        canonical.thread_key.as_ref().unwrap().as_str(),
        "resp_previous"
    );
    assert!(canonical.stream);
    assert_eq!(
        canonical.input,
        [
            InputItem::Text {
                text: "读取 配置.toml".to_owned(),
            },
            InputItem::ImageUrl {
                url: "https://example.test/image.png".parse().unwrap(),
                detail: Some(ImageDetail::High),
            },
            InputItem::ToolResult {
                call_id: "call_1".to_owned(),
                output: json!({"content": "done"}),
            },
        ]
    );
    assert_eq!(canonical.tools[0].name, "read_file");
    assert_eq!(
        canonical.tools[0].input_schema,
        json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        })
    );
    assert_eq!(
        canonical.tools[0].extensions,
        BTreeMap::from([("strict".to_owned(), json!(true))])
    );
    let reasoning = canonical.reasoning.unwrap();
    assert_eq!(reasoning.effort.as_deref(), Some("high"));
    assert_eq!(
        reasoning.extensions,
        BTreeMap::from([("summary".to_owned(), json!("auto"))])
    );
    assert_eq!(
        canonical.extensions["metadata"],
        json!({"tenant": "fixture"})
    );
    assert_eq!(
        canonical.extensions["responses.input_extensions"],
        json!([
            {
                "index": 0,
                "role": "user",
                "message": {"vendor_message": true},
                "content": [
                    {"index": 0, "extra": {"vendor_text": "kept"}},
                    {"index": 1, "extra": {"vendor_image": 7}}
                ]
            },
            {
                "index": 1,
                "extra": {"vendor_result": "kept"}
            }
        ])
    );
}

#[test]
fn decodes_string_input_and_allows_front_door_request_id_injection() {
    let request = fixture_bytes("request/string_input.json");
    let assigned =
        ResponsesCodec::decode_request(RequestId::new("request-front-door"), &request).unwrap();

    assert_eq!(assigned.request_id.as_str(), "request-front-door");
    assert_eq!(
        assigned.input,
        [InputItem::Text {
            text: "你好，world".to_owned(),
        }]
    );
}

#[test]
fn accepts_only_explicitly_allowed_image_url_schemes() {
    for url in [
        "http://example.test/image.png",
        "https://example.test/image.png",
        "data:image/png;base64,AA==",
        "data:image/jpeg;base64,AA==",
        "data:image/webp;base64,AA==",
        "data:image/gif;base64,AA==",
    ] {
        let request = json!({
            "model": "gpt-test",
            "input": [{
                "role": "user",
                "content": [{"type": "input_image", "image_url": url}]
            }]
        });
        let encoded = serde_json::to_vec(&request).unwrap();
        assert!(
            ResponsesCodec::decode_request(RequestId::new("request-image"), &encoded).is_ok(),
            "{url} should be accepted"
        );
    }

    for url in [
        "file:///etc/passwd",
        "javascript:alert(1)",
        "ftp://example.test/image.png",
        "data:text/html,<script>alert(1)</script>",
        "data:image/svg+xml;base64,PHN2Zz48L3N2Zz4=",
        "data:image/png,AA==",
        "data:image/png;base64,not-base64",
        "data:image/png;base64,",
    ] {
        let request = json!({
            "model": "gpt-test",
            "input": [{
                "role": "user",
                "content": [{"type": "input_image", "image_url": url}]
            }]
        });
        let encoded = serde_json::to_vec(&request).unwrap();
        assert_eq!(
            ResponsesCodec::decode_request(RequestId::new("request-image"), &encoded).unwrap_err(),
            GatewayError::invalid_request(),
            "{url} should be rejected"
        );
    }
}

#[test]
fn message_role_uses_the_official_allow_list() {
    for role in ["user", "assistant", "system", "developer"] {
        let request = json!({
            "model": "gpt-test",
            "input": [{
                "type": "message",
                "role": role,
                "content": "hello"
            }]
        });
        let encoded = serde_json::to_vec(&request).unwrap();
        assert!(
            ResponsesCodec::decode_request(RequestId::new("request-role"), &encoded).is_ok(),
            "{role} should be accepted"
        );
    }

    for role in ["tool", "future_role"] {
        let request = json!({
            "model": "gpt-test",
            "input": [{
                "type": "message",
                "role": role,
                "content": "hello"
            }]
        });
        let encoded = serde_json::to_vec(&request).unwrap();
        assert_eq!(
            ResponsesCodec::decode_request(RequestId::new("request-role"), &encoded).unwrap_err(),
            GatewayError::invalid_request(),
            "{role} should be rejected"
        );
    }
}

#[test]
fn reserved_nested_extension_key_does_not_overwrite_top_level_extension() {
    let request = json!({
        "model": "gpt-test",
        "input": [{
            "type": "input_text",
            "text": "hello",
            "vendor_part": true
        }],
        "responses.input_extensions": "client-value"
    });
    let encoded = serde_json::to_vec(&request).unwrap();
    let canonical =
        ResponsesCodec::decode_request(RequestId::new("request-extensions"), &encoded).unwrap();

    assert_eq!(
        canonical.extensions["responses.input_extensions"],
        "client-value"
    );
    assert_eq!(
        canonical.extensions["responses.input_extensions.nested"],
        json!([{"index": 0, "extra": {"vendor_part": true}}])
    );
}

#[test]
fn rejects_missing_or_mistyped_required_fields_and_tools() {
    let cases = [
        json!({"input": "missing model"}),
        json!({"model": 7, "input": "bad model"}),
        json!({"model": "gpt-test"}),
        json!({"model": "gpt-test", "input": 7}),
        json!({
            "model": "gpt-test",
            "input": "x",
            "tools": [{"type": "function", "parameters": {"type": "object"}}]
        }),
        json!({
            "model": "gpt-test",
            "input": "x",
            "tools": [{"type": "function", "name": "", "parameters": {"type": "object"}}]
        }),
        json!({
            "model": "gpt-test",
            "input": "x",
            "tools": [{"type": "function", "name": "read_file"}]
        }),
        json!({
            "model": "gpt-test",
            "input": "x",
            "tools": [{"type": "function", "name": "read_file", "parameters": "not-schema"}]
        }),
        json!({
            "model": "gpt-test",
            "input": [{
                "type": "function_call_output",
                "call_id": "",
                "output": "x"
            }]
        }),
        json!({
            "model": "gpt-test",
            "input": [{
                "role": "user",
                "content": [{"type": "input_image", "image_url": "not a URL"}]
            }]
        }),
    ];

    for request in cases {
        let encoded = serde_json::to_vec(&request).unwrap();
        assert_eq!(
            ResponsesCodec::decode_request(RequestId::new("request-invalid"), &encoded)
                .unwrap_err(),
            GatewayError::invalid_request(),
            "unexpected result for {request}"
        );
    }
}

#[test]
fn non_stream_failure_uses_redacted_openai_error_envelope() {
    let secret = "Authorization: Bearer non-stream-secret";
    let encoded = ResponsesCodec::encode_response(
        encode_context(),
        &[CanonicalEvent::Failed(GatewayError::upstream_auth(secret))],
    )
    .unwrap();
    let serialized = serde_json::to_string(&encoded).unwrap();

    assert_eq!(encoded, fixture_json("error/non_stream.json"));
    assert!(!serialized.contains(secret));
    assert!(!serialized.contains("non-stream-secret"));
    assert!(!serialized.contains("Bearer"));
}

#[test]
fn non_stream_response_matches_golden_and_preserves_safe_usage_extensions() {
    assert_eq!(
        ResponsesCodec::encode_response(encode_context(), &canonical_events()).unwrap(),
        fixture_json("response/complete.json")
    );
}

#[test]
fn non_stream_response_has_the_formal_official_field_set() {
    let response = ResponsesCodec::encode_response(encode_context(), &canonical_events()).unwrap();

    assert_formal_response_fields(&response);
    assert_eq!(response["completed_at"], 1_723_456_790_u64);
    assert_eq!(response["error"], Value::Null);
    assert_eq!(response["incomplete_details"], Value::Null);
    assert_eq!(response["instructions"], "trusted instructions");
    assert_eq!(response["max_output_tokens"], 512);
    assert_eq!(response["metadata"], json!({"fixture": "trusted"}));
    assert_eq!(response["parallel_tool_calls"], false);
    assert_eq!(response["previous_response_id"], "resp_previous");
    assert_eq!(
        response["reasoning"],
        json!({"effort": "high", "summary": "auto"})
    );
    assert_eq!(response["store"], false);
    assert_eq!(response["temperature"], 0.25);
    assert_eq!(
        response["text"],
        json!({"format": {"type": "text"}, "verbosity": "low"})
    );
    assert_eq!(
        response["tool_choice"],
        json!({"type": "function", "name": "read_file"})
    );
    assert_eq!(
        response["tools"],
        json!([{
            "type": "function",
            "name": "read_file",
            "parameters": {"type": "object"}
        }])
    );
    assert_eq!(response["top_p"], 0.9);
    assert_eq!(response["truncation"], "auto");
    assert_eq!(response["user"], Value::Null);
}

#[test]
fn reasoning_items_have_the_official_returned_status() {
    let mut codec = ResponsesCodec::new(encode_context());
    codec
        .encode_event(&CanonicalEvent::Created {
            response_id: "resp_reasoning".to_owned(),
        })
        .unwrap();
    let encoded = codec
        .encode_event(&CanonicalEvent::ReasoningDelta {
            item_id: "reasoning_1".to_owned(),
            delta: "inspect".to_owned(),
        })
        .unwrap();
    let mut decoder = SseDecoder::default();
    let frames = decoder.push(&encoded).unwrap();
    let added: Value = serde_json::from_str(&frames[0].data).unwrap();
    assert_eq!(added["item"]["status"], "in_progress");

    let response = ResponsesCodec::encode_response(encode_context(), &canonical_events()).unwrap();
    assert_eq!(response["output"][1]["status"], "completed");
}

#[test]
fn response_extensions_cannot_override_standard_usage_fields() {
    let events = vec![
        CanonicalEvent::Created {
            response_id: "resp_1".to_owned(),
        },
        CanonicalEvent::Usage(Usage {
            input_tokens: 1,
            output_tokens: 2,
            cached_input_tokens: None,
            reasoning_tokens: None,
            extensions: BTreeMap::from([
                ("input_tokens".to_owned(), json!(999)),
                ("total_tokens".to_owned(), json!(999)),
                ("vendor".to_owned(), json!(true)),
            ]),
        }),
        CanonicalEvent::Completed,
    ];

    let encoded = ResponsesCodec::encode_response(encode_context(), &events).unwrap();
    assert_eq!(encoded["usage"]["input_tokens"], 1);
    assert_eq!(encoded["usage"]["total_tokens"], 3);
    assert_eq!(encoded["usage"]["vendor"], true);
}

#[test]
fn stream_events_cover_every_variant_and_match_golden_order() {
    let expected = fixture_json("stream/ordered.json");
    let mut codec = ResponsesCodec::new(encode_context());
    let mut actual = Vec::new();

    for event in canonical_events() {
        let encoded = codec.encode_event(&event).unwrap();
        let mut decoder = SseDecoder::default();
        let frames = decoder.push(&encoded).unwrap();
        actual.extend(frames.into_iter().map(|frame| {
            json!({
                "event": frame.event,
                "data": serde_json::from_str::<Value>(&frame.data).unwrap()
            })
        }));
    }

    assert_eq!(Value::Array(actual), expected);
}

#[test]
fn stream_encoder_rejects_events_after_completion() {
    let mut codec = ResponsesCodec::new(encode_context());
    codec
        .encode_event(&CanonicalEvent::Created {
            response_id: "resp_1".to_owned(),
        })
        .unwrap();
    codec
        .encode_event(&CanonicalEvent::Usage(Usage {
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: None,
            reasoning_tokens: None,
            extensions: BTreeMap::new(),
        }))
        .unwrap();
    codec.encode_event(&CanonicalEvent::Completed).unwrap();

    assert_eq!(
        codec
            .encode_event(&CanonicalEvent::OutputTextDelta {
                item_id: "msg_1".to_owned(),
                delta: "late".to_owned(),
            })
            .unwrap_err(),
        GatewayError::invalid_request()
    );
}

#[test]
fn stream_encoder_rejects_payload_beyond_the_default_retained_state_limit() {
    let mut codec = ResponsesCodec::new(encode_context());
    codec
        .encode_event(&CanonicalEvent::Created {
            response_id: "resp_1".to_owned(),
        })
        .unwrap();

    assert_eq!(
        codec
            .encode_event(&CanonicalEvent::OutputTextDelta {
                item_id: "msg_1".to_owned(),
                delta: "x".repeat(16 * 1024 * 1024 + 1),
            })
            .unwrap_err(),
        GatewayError::invalid_request()
    );
}

#[test]
fn usage_is_cached_without_a_wire_event_and_is_required_for_completion() {
    let mut codec = ResponsesCodec::new(encode_context());
    codec
        .encode_event(&CanonicalEvent::Created {
            response_id: "resp_1".to_owned(),
        })
        .unwrap();
    let usage = Usage {
        input_tokens: 1,
        output_tokens: 2,
        cached_input_tokens: None,
        reasoning_tokens: None,
        extensions: BTreeMap::new(),
    };

    assert!(
        codec
            .encode_event(&CanonicalEvent::Usage(usage))
            .unwrap()
            .is_empty()
    );
    assert!(
        !codec
            .encode_event(&CanonicalEvent::Completed)
            .unwrap()
            .is_empty()
    );

    let mut missing_usage = ResponsesCodec::new(encode_context());
    missing_usage
        .encode_event(&CanonicalEvent::Created {
            response_id: "resp_2".to_owned(),
        })
        .unwrap();
    assert_eq!(
        missing_usage
            .encode_event(&CanonicalEvent::Completed)
            .unwrap_err(),
        GatewayError::invalid_request()
    );
}

#[test]
fn stream_state_machine_rejects_missing_repeated_and_late_events() {
    let usage = CanonicalEvent::Usage(Usage {
        input_tokens: 1,
        output_tokens: 1,
        cached_input_tokens: None,
        reasoning_tokens: None,
        extensions: BTreeMap::new(),
    });
    for event in [
        CanonicalEvent::OutputTextDelta {
            item_id: "msg_1".to_owned(),
            delta: "x".to_owned(),
        },
        usage.clone(),
        CanonicalEvent::Completed,
    ] {
        let mut codec = ResponsesCodec::new(encode_context());
        assert_eq!(
            codec.encode_event(&event).unwrap_err(),
            GatewayError::invalid_request()
        );
    }

    let mut repeated_created = ResponsesCodec::new(encode_context());
    let created = CanonicalEvent::Created {
        response_id: "resp_1".to_owned(),
    };
    repeated_created.encode_event(&created).unwrap();
    assert_eq!(
        repeated_created.encode_event(&created).unwrap_err(),
        GatewayError::invalid_request()
    );

    let mut repeated_usage = ResponsesCodec::new(encode_context());
    repeated_usage.encode_event(&created).unwrap();
    repeated_usage.encode_event(&usage).unwrap();
    assert_eq!(
        repeated_usage.encode_event(&usage).unwrap_err(),
        GatewayError::invalid_request()
    );
    assert_eq!(
        repeated_usage
            .encode_event(&CanonicalEvent::ReasoningDelta {
                item_id: "reasoning_1".to_owned(),
                delta: "late".to_owned(),
            })
            .unwrap_err(),
        GatewayError::invalid_request()
    );

    let mut failed = ResponsesCodec::new(encode_context());
    failed
        .encode_event(&CanonicalEvent::Failed(GatewayError::internal("private")))
        .unwrap();
    assert_eq!(
        failed.encode_event(&created).unwrap_err(),
        GatewayError::invalid_request()
    );
}

#[test]
fn item_registry_rejects_empty_and_conflicting_identities() {
    let invalid_first_events = [
        CanonicalEvent::OutputTextDelta {
            item_id: String::new(),
            delta: "x".to_owned(),
        },
        CanonicalEvent::ReasoningDelta {
            item_id: String::new(),
            delta: "x".to_owned(),
        },
        CanonicalEvent::ToolCallDelta {
            item_id: String::new(),
            call_id: "call_1".to_owned(),
            name: "read_file".to_owned(),
            delta: "{}".to_owned(),
        },
        CanonicalEvent::ToolCallDelta {
            item_id: "tool_1".to_owned(),
            call_id: String::new(),
            name: "read_file".to_owned(),
            delta: "{}".to_owned(),
        },
        CanonicalEvent::ToolCallDelta {
            item_id: "tool_1".to_owned(),
            call_id: "call_1".to_owned(),
            name: String::new(),
            delta: "{}".to_owned(),
        },
    ];
    for event in invalid_first_events {
        let mut codec = ResponsesCodec::new(encode_context());
        codec
            .encode_event(&CanonicalEvent::Created {
                response_id: "resp_1".to_owned(),
            })
            .unwrap();
        assert_eq!(
            codec.encode_event(&event).unwrap_err(),
            GatewayError::invalid_request()
        );
    }

    let mut kind_conflict = ResponsesCodec::new(encode_context());
    kind_conflict
        .encode_event(&CanonicalEvent::Created {
            response_id: "resp_1".to_owned(),
        })
        .unwrap();
    kind_conflict
        .encode_event(&CanonicalEvent::OutputTextDelta {
            item_id: "shared".to_owned(),
            delta: "x".to_owned(),
        })
        .unwrap();
    assert_eq!(
        kind_conflict
            .encode_event(&CanonicalEvent::ReasoningDelta {
                item_id: "shared".to_owned(),
                delta: "x".to_owned(),
            })
            .unwrap_err(),
        GatewayError::invalid_request()
    );

    let mut tool_conflict = ResponsesCodec::new(encode_context());
    tool_conflict
        .encode_event(&CanonicalEvent::Created {
            response_id: "resp_1".to_owned(),
        })
        .unwrap();
    let first = CanonicalEvent::ToolCallDelta {
        item_id: "tool_1".to_owned(),
        call_id: "call_1".to_owned(),
        name: "read_file".to_owned(),
        delta: "{".to_owned(),
    };
    tool_conflict.encode_event(&first).unwrap();
    for event in [
        CanonicalEvent::ToolCallDelta {
            item_id: "tool_1".to_owned(),
            call_id: "call_2".to_owned(),
            name: "read_file".to_owned(),
            delta: "}".to_owned(),
        },
        CanonicalEvent::ToolCallDelta {
            item_id: "tool_1".to_owned(),
            call_id: "call_1".to_owned(),
            name: "write_file".to_owned(),
            delta: "}".to_owned(),
        },
        CanonicalEvent::ToolCallDelta {
            item_id: "tool_2".to_owned(),
            call_id: "call_1".to_owned(),
            name: "read_file".to_owned(),
            delta: "}".to_owned(),
        },
    ] {
        assert_eq!(
            tool_conflict.encode_event(&event).unwrap_err(),
            GatewayError::invalid_request()
        );
    }
}

#[test]
fn failed_event_is_redacted_and_matches_golden() {
    let secret = "Authorization: Bearer fixture-secret";
    let mut codec = ResponsesCodec::new(encode_context());
    let encoded = codec
        .encode_event(&CanonicalEvent::Failed(GatewayError::upstream_auth(secret)))
        .unwrap();
    let encoded_text = String::from_utf8(encoded.to_vec()).unwrap();
    let mut decoder = SseDecoder::default();
    let frame = decoder.push(encoded_text.as_bytes()).unwrap().remove(0);

    assert_eq!(frame.event.as_deref(), Some("error"));
    assert_eq!(
        serde_json::from_str::<Value>(&frame.data).unwrap(),
        fixture_json("error/failed.json")
    );
    assert!(!encoded_text.contains(secret));
    assert!(!encoded_text.contains("fixture-secret"));
    assert!(!encoded_text.contains("Bearer"));
}

#[test]
fn non_stream_encoder_rejects_invalid_event_order() {
    let invalid_sequences = [
        vec![CanonicalEvent::Completed],
        vec![
            CanonicalEvent::Created {
                response_id: "resp_1".to_owned(),
            },
            CanonicalEvent::Completed,
            CanonicalEvent::Usage(Usage {
                input_tokens: 1,
                output_tokens: 1,
                cached_input_tokens: None,
                reasoning_tokens: None,
                extensions: BTreeMap::new(),
            }),
        ],
        vec![
            CanonicalEvent::Created {
                response_id: "resp_1".to_owned(),
            },
            CanonicalEvent::Failed(GatewayError::internal("secret")),
            CanonicalEvent::Completed,
        ],
    ];

    for events in invalid_sequences {
        assert_eq!(
            ResponsesCodec::encode_response(encode_context(), &events).unwrap_err(),
            GatewayError::invalid_request()
        );
    }
}
