use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

use serde_json::Value;
use wokcore_protocols::{
    ANTHROPIC_KNOWN_BLOCKS_EXTENSION_KEY, AnthropicCodec, AnthropicEncodeContext,
    AnthropicResponseTemplate, AnthropicStopReason, TokenCounter,
    canonical::{CanonicalEvent, PublicModelId, RequestId, Usage},
    stream::SseDecoder,
};

fn fixture_bytes(relative: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/protocols/anthropic")
        .join(relative);
    fs::read(path).unwrap()
}

fn fixture_json(relative: &str) -> Value {
    serde_json::from_slice(&fixture_bytes(relative)).unwrap()
}

fn encode_context() -> AnthropicEncodeContext {
    AnthropicEncodeContext {
        request_id: RequestId::new("req_encode"),
        model: PublicModelId::new("claude-test"),
        initial_usage: Usage {
            input_tokens: 11,
            output_tokens: 1,
            cached_input_tokens: Some(3),
            reasoning_tokens: None,
            extensions: BTreeMap::new(),
        },
        response: AnthropicResponseTemplate {
            stop_reason: AnthropicStopReason::ToolUse,
            stop_sequence: None,
            thinking_signatures: BTreeMap::new(),
            extra: BTreeMap::new(),
        },
    }
}

#[test]
fn tool_use_and_result_round_trip_as_distinct_blocks() {
    let original = fixture_bytes("request/tool_round_trip.json");
    let canonical =
        AnthropicCodec::decode_message(RequestId::new("req_anthropic"), &original).unwrap();
    let encoded = AnthropicCodec::encode_message(&canonical).unwrap();

    assert_eq!(
        encoded,
        fixture_json("request/tool_round_trip.expected.json")
    );
}

#[test]
fn image_document_and_thinking_blocks_keep_their_boundaries() {
    let original = fixture_bytes("request/multimodal_thinking.json");
    let canonical =
        AnthropicCodec::decode_message(RequestId::new("req_multimodal"), &original).unwrap();

    assert_eq!(canonical.input.len(), 3);
    assert!(matches!(
        &canonical.input[0],
        wokcore_protocols::canonical::InputItem::Text { text } if text == "System block."
    ));
    assert!(matches!(
        &canonical.input[1],
        wokcore_protocols::canonical::InputItem::ImageUrl { url, .. }
            if url.as_str() == "data:image/png;base64,iVBORw0KGgo="
    ));
    assert!(matches!(
        &canonical.input[2],
        wokcore_protocols::canonical::InputItem::Text { text } if text == "Inspect both."
    ));
    let retained_blocks = canonical
        .extensions
        .get("anthropic.known_blocks")
        .and_then(Value::as_array)
        .expect("known Anthropic blocks must be visible outside the replay blob");
    assert_eq!(
        retained_blocks
            .iter()
            .map(|entry| entry["block"]["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["thinking", "redacted_thinking", "document"]
    );
    assert_eq!(retained_blocks[0]["message_index"], 0);
    assert_eq!(retained_blocks[0]["block_index"], 0);
    assert_eq!(retained_blocks[0]["role"], "assistant");
    assert_eq!(retained_blocks[2]["message_index"], 1);
    assert_eq!(retained_blocks[2]["block_index"], 1);
    assert_eq!(retained_blocks[2]["role"], "user");
    assert_eq!(
        AnthropicCodec::encode_message(&canonical).unwrap(),
        fixture_json("request/multimodal_thinking.expected.json")
    );
}

#[test]
fn retained_known_blocks_are_tamper_evident() {
    let original = fixture_bytes("request/multimodal_thinking.json");
    let mut canonical =
        AnthropicCodec::decode_message(RequestId::new("req_tamper"), &original).unwrap();
    canonical
        .extensions
        .get_mut("anthropic.known_blocks")
        .unwrap()
        .as_array_mut()
        .unwrap()
        .remove(0);

    assert_eq!(
        AnthropicCodec::encode_message(&canonical)
            .unwrap_err()
            .code(),
        "unsupported_capability"
    );
}

#[test]
fn extension_namespace_collision_round_trips_without_overwrite() {
    let body = serde_json::json!({
        "model": "claude-test",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "safe"}],
        "anthropic.request": {"client_owned": true},
        "anthropic.request.nested": {"client_owned": 2}
    });
    let canonical = AnthropicCodec::decode_message(
        RequestId::new("req_collision"),
        &serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();

    assert_eq!(
        AnthropicCodec::encode_message(&canonical).unwrap(),
        body,
        "the codec namespace must contain, not overwrite, colliding client fields"
    );
}

#[test]
fn role_specific_required_blocks_are_rejected() {
    for body in [
        serde_json::json!({
            "model": "claude-test",
            "max_tokens": 16,
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_1",
                    "content": "not valid in assistant content"
                }]
            }]
        }),
        serde_json::json!({
            "model": "claude-test",
            "max_tokens": 16,
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "read_file",
                    "input": {}
                }]
            }]
        }),
    ] {
        assert_eq!(
            AnthropicCodec::decode_message(
                RequestId::new("req_invalid_role"),
                &serde_json::to_vec(&body).unwrap(),
            )
            .unwrap_err()
            .code(),
            "invalid_request"
        );
    }
}

#[test]
fn unknown_required_content_block_is_a_typed_compatibility_error() {
    let body = serde_json::json!({
        "model": "claude-test",
        "max_tokens": 16,
        "messages": [{
            "role": "user",
            "content": [{
                "type": "future_required_block",
                "payload": {"safe": true}
            }]
        }]
    });
    assert_eq!(
        AnthropicCodec::decode_message(
            RequestId::new("req_unknown"),
            &serde_json::to_vec(&body).unwrap(),
        )
        .unwrap_err()
        .code(),
        "unsupported_capability"
    );
}

#[test]
fn unknown_required_thinking_mode_is_a_typed_compatibility_error() {
    let body = serde_json::json!({
        "model": "claude-test",
        "max_tokens": 2048,
        "messages": [{"role": "user", "content": "safe"}],
        "thinking": {
            "type": "future_required_mode",
            "budget_tokens": 1024
        }
    });
    assert_eq!(
        AnthropicCodec::decode_message(
            RequestId::new("req_unknown_thinking"),
            &serde_json::to_vec(&body).unwrap(),
        )
        .unwrap_err()
        .code(),
        "unsupported_capability"
    );
}

#[test]
fn unsupported_required_server_tool_is_a_typed_compatibility_error() {
    let body = serde_json::json!({
        "model": "claude-test",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "safe"}],
        "tools": [{
            "type": "future_server_tool_20990101",
            "name": "future_server_tool"
        }]
    });
    assert_eq!(
        AnthropicCodec::decode_message(
            RequestId::new("req_unknown_tool"),
            &serde_json::to_vec(&body).unwrap(),
        )
        .unwrap_err()
        .code(),
        "unsupported_capability"
    );
}

#[test]
fn tool_choice_modes_are_explicit_and_malformed_known_modes_are_invalid() {
    for tool_choice in [
        serde_json::json!({"type": "auto"}),
        serde_json::json!({"type": "any", "disable_parallel_tool_use": true}),
        serde_json::json!({"type": "tool", "name": "read_file", "disable_parallel_tool_use": false}),
        serde_json::json!({"type": "none"}),
    ] {
        let body = serde_json::json!({
            "model": "claude-test",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "safe"}],
            "tool_choice": tool_choice,
        });
        let canonical = AnthropicCodec::decode_message(
            RequestId::new("req_tool_choice"),
            &serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
        assert_eq!(AnthropicCodec::encode_message(&canonical).unwrap(), body);
    }

    for tool_choice in [
        serde_json::json!({"type": "tool"}),
        serde_json::json!({"type": "tool", "name": ""}),
        serde_json::json!({"type": "auto", "name": "read_file"}),
        serde_json::json!({"type": "any", "disable_parallel_tool_use": "yes"}),
    ] {
        let body = serde_json::json!({
            "model": "claude-test",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "safe"}],
            "tool_choice": tool_choice,
        });
        assert_eq!(
            AnthropicCodec::decode_message(
                RequestId::new("req_bad_tool_choice"),
                &serde_json::to_vec(&body).unwrap(),
            )
            .unwrap_err()
            .code(),
            "invalid_request"
        );
    }

    let unknown = serde_json::json!({
        "model": "claude-test",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "safe"}],
        "tool_choice": {"type": "future_required_mode"}
    });
    assert_eq!(
        AnthropicCodec::decode_message(
            RequestId::new("req_unknown_tool_choice"),
            &serde_json::to_vec(&unknown).unwrap(),
        )
        .unwrap_err()
        .code(),
        "unsupported_capability"
    );
}

#[test]
fn tool_choice_none_rejects_disable_parallel_tool_use() {
    let body = serde_json::json!({
        "model": "claude-test",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "safe"}],
        "tool_choice": {
            "type": "none",
            "disable_parallel_tool_use": true
        }
    });

    assert_eq!(
        AnthropicCodec::decode_message(
            RequestId::new("req_none_parallel"),
            &serde_json::to_vec(&body).unwrap(),
        )
        .unwrap_err()
        .code(),
        "invalid_request"
    );
}

#[test]
fn malformed_known_variants_are_invalid_but_future_variants_are_unsupported() {
    for body in [
        serde_json::json!({
            "model": "claude-test",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": [{"type": "text"}]}]
        }),
        serde_json::json!({
            "model": "claude-test",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": [{
                "type": "image",
                "source": {"type": "base64", "media_type": "image/png"}
            }]}]
        }),
        serde_json::json!({
            "model": "claude-test",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "safe"}],
            "tools": [{"name": "read_file"}]
        }),
    ] {
        assert_eq!(
            AnthropicCodec::decode_message(
                RequestId::new("req_malformed_known"),
                &serde_json::to_vec(&body).unwrap(),
            )
            .unwrap_err()
            .code(),
            "invalid_request"
        );
    }

    let unknown_source = serde_json::json!({
        "model": "claude-test",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": [{
            "type": "image",
            "source": {"type": "future_source", "reference": "safe"}
        }]}]
    });
    assert_eq!(
        AnthropicCodec::decode_message(
            RequestId::new("req_unknown_source"),
            &serde_json::to_vec(&unknown_source).unwrap(),
        )
        .unwrap_err()
        .code(),
        "unsupported_capability"
    );
}

#[test]
fn opaque_thinking_payloads_are_not_identifier_limited() {
    let opaque = "x".repeat(513);
    let body = serde_json::json!({
        "model": "claude-test",
        "max_tokens": 2048,
        "messages": [{"role": "assistant", "content": [
            {"type": "thinking", "thinking": "summary", "signature": opaque},
            {"type": "redacted_thinking", "data": "y".repeat(513)}
        ]}]
    });
    AnthropicCodec::decode_message(
        RequestId::new("req_opaque"),
        &serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
}

struct SpyCounter {
    calls: AtomicUsize,
}

impl TokenCounter for SpyCounter {
    fn count_tokens(
        &self,
        request: &wokcore_protocols::canonical::CanonicalRequest,
    ) -> Result<u64, wokcore_protocols::canonical::GatewayError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request.request_id.as_str(), "req_count");
        assert_eq!(request.model.as_str(), "mock/test-model");
        assert_eq!(request.input.len(), 2);
        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.tools[0].name, "Read");
        Ok(37)
    }
}

#[test]
fn count_tokens_uses_only_the_injected_counter() {
    let counter = SpyCounter {
        calls: AtomicUsize::new(0),
    };
    let counted = AnthropicCodec::count_tokens_input(
        RequestId::new("req_count"),
        &fixture_bytes("count_tokens/opencodex_v2_7_35.json"),
        &counter,
    )
    .unwrap();

    assert_eq!(counted.input_tokens, 37);
    assert_eq!(counter.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        serde_json::to_value(counted).unwrap(),
        fixture_json("count_tokens/response.json")
    );
}

struct KnownBlocksCounter;

impl TokenCounter for KnownBlocksCounter {
    fn count_tokens(
        &self,
        request: &wokcore_protocols::canonical::CanonicalRequest,
    ) -> Result<u64, wokcore_protocols::canonical::GatewayError> {
        let blocks = request.extensions[ANTHROPIC_KNOWN_BLOCKS_EXTENSION_KEY]
            .as_array()
            .unwrap();
        assert_eq!(
            blocks
                .iter()
                .map(|entry| entry["block"]["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["thinking", "redacted_thinking", "tool_use", "document"]
        );
        Ok(9)
    }
}

#[test]
fn count_tokens_exposes_all_known_non_ir_blocks_to_the_counter() {
    let body = serde_json::json!({
        "model": "claude-test",
        "messages": [
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "summary", "signature": "opaque"},
                {"type": "redacted_thinking", "data": "opaque"},
                {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"path": "a"}}
            ]},
            {"role": "user", "content": [
                {"type": "document", "source": {
                    "type": "base64",
                    "media_type": "text/plain",
                    "data": "c2FmZQ=="
                }}
            ]}
        ]
    });

    assert_eq!(
        AnthropicCodec::count_tokens_input(
            RequestId::new("req_known_blocks"),
            &serde_json::to_vec(&body).unwrap(),
            &KnownBlocksCounter,
        )
        .unwrap()
        .input_tokens,
        9
    );
}

#[test]
fn messages_and_count_tokens_apply_their_distinct_required_fields() {
    let count_body = fixture_bytes("count_tokens/opencodex_v2_7_35.json");
    assert_eq!(
        AnthropicCodec::decode_message(RequestId::new("req_messages"), &count_body)
            .unwrap_err()
            .code(),
        "invalid_request"
    );

    let counter = SpyCounter {
        calls: AtomicUsize::new(0),
    };
    let mut messages_body = fixture_json("count_tokens/opencodex_v2_7_35.json");
    messages_body["max_tokens"] = serde_json::json!(1);
    assert_eq!(
        AnthropicCodec::count_tokens_input(
            RequestId::new("req_count"),
            &serde_json::to_vec(&messages_body).unwrap(),
            &counter,
        )
        .unwrap_err()
        .code(),
        "invalid_request"
    );
    assert_eq!(counter.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn stream_uses_the_formal_anthropic_event_order() {
    let mut codec = AnthropicCodec::new(encode_context());
    let events = [
        CanonicalEvent::Created {
            response_id: "msg_fixture".to_owned(),
        },
        CanonicalEvent::OutputTextDelta {
            item_id: "text_1".to_owned(),
            delta: "Hello".to_owned(),
        },
        CanonicalEvent::ToolCallDelta {
            item_id: "tool_1".to_owned(),
            call_id: "toolu_fixture".to_owned(),
            name: "read_file".to_owned(),
            delta: "{\"path\":\"README.md\"}".to_owned(),
        },
        CanonicalEvent::Usage(Usage {
            input_tokens: 11,
            output_tokens: 17,
            cached_input_tokens: Some(3),
            reasoning_tokens: None,
            extensions: BTreeMap::new(),
        }),
        CanonicalEvent::Completed,
    ];

    let mut decoder = SseDecoder::default();
    let mut frames = Vec::new();
    for event in events {
        let encoded = codec.encode_event(&event).unwrap();
        frames.extend(decoder.push(&encoded).unwrap());
    }

    let event_names = frames
        .iter()
        .map(|frame| frame.event.as_deref().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        event_names,
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );
    for frame in frames {
        assert_eq!(
            frame.event.as_deref(),
            serde_json::from_str::<Value>(&frame.data)
                .unwrap()
                .get("type")
                .and_then(Value::as_str)
        );
    }
}

#[test]
fn thinking_stream_emits_a_trusted_signature_before_block_stop() {
    let mut context = encode_context();
    context.response.stop_reason = AnthropicStopReason::EndTurn;
    context
        .response
        .thinking_signatures
        .insert("thinking_1".to_owned(), "fixture-signature".to_owned());
    let mut codec = AnthropicCodec::new(context);
    let events = [
        CanonicalEvent::Created {
            response_id: "msg_thinking".to_owned(),
        },
        CanonicalEvent::ReasoningDelta {
            item_id: "thinking_1".to_owned(),
            delta: "Reasoning summary.".to_owned(),
        },
        CanonicalEvent::Usage(Usage {
            input_tokens: 8,
            output_tokens: 9,
            cached_input_tokens: None,
            reasoning_tokens: Some(4),
            extensions: BTreeMap::new(),
        }),
        CanonicalEvent::Completed,
    ];
    let mut decoder = SseDecoder::default();
    let mut values = Vec::new();
    for event in events {
        values.extend(
            decoder
                .push(&codec.encode_event(&event).unwrap())
                .unwrap()
                .into_iter()
                .map(|frame| serde_json::from_str::<Value>(&frame.data).unwrap()),
        );
    }

    assert_eq!(
        values,
        fixture_json("stream/thinking_order.json")
            .as_array()
            .unwrap()
            .clone()
    );
}

#[test]
fn missing_thinking_signature_is_a_typed_compatibility_error() {
    let mut codec = AnthropicCodec::new(encode_context());
    codec
        .encode_event(&CanonicalEvent::Created {
            response_id: "msg_thinking".to_owned(),
        })
        .unwrap();
    assert_eq!(
        codec
            .encode_event(&CanonicalEvent::ReasoningDelta {
                item_id: "thinking_1".to_owned(),
                delta: "private reasoning".to_owned(),
            })
            .unwrap_err()
            .code(),
        "unsupported_capability"
    );
}

#[test]
fn stream_error_is_typed_and_does_not_expose_private_diagnostics() {
    let secret = "Authorization: Bearer fixture-secret";
    let mut codec = AnthropicCodec::new(encode_context());
    let encoded = codec
        .encode_event(&CanonicalEvent::Failed(
            wokcore_protocols::canonical::GatewayError::upstream_auth(secret),
        ))
        .unwrap();
    let encoded_text = String::from_utf8(encoded.to_vec()).unwrap();
    let mut decoder = SseDecoder::default();
    let frame = decoder.push(&encoded).unwrap().remove(0);

    assert_eq!(frame.event.as_deref(), Some("error"));
    assert_eq!(
        serde_json::from_str::<Value>(&frame.data).unwrap(),
        fixture_json("error/authentication.json")
    );
    assert!(!encoded_text.contains(secret));
    assert!(!encoded_text.contains("fixture-secret"));
    assert!(!encoded_text.contains("Bearer"));
}

#[test]
fn non_stream_error_uses_the_trusted_request_id() {
    let encoded = AnthropicCodec::encode_response(
        encode_context(),
        &[CanonicalEvent::Failed(
            wokcore_protocols::canonical::GatewayError::unknown_model(),
        )],
    )
    .unwrap();

    assert_eq!(
        encoded,
        serde_json::json!({
            "type": "error",
            "error": {
                "type": "not_found_error",
                "message": "The requested model is not available."
            },
            "request_id": "req_encode"
        })
    );
}

#[test]
fn every_documented_stop_reason_maps_to_its_wire_value() {
    for (reason, expected) in [
        (AnthropicStopReason::EndTurn, "end_turn"),
        (AnthropicStopReason::MaxTokens, "max_tokens"),
        (AnthropicStopReason::StopSequence, "stop_sequence"),
        (AnthropicStopReason::ToolUse, "tool_use"),
        (AnthropicStopReason::PauseTurn, "pause_turn"),
        (AnthropicStopReason::Refusal, "refusal"),
        (
            AnthropicStopReason::ModelContextWindowExceeded,
            "model_context_window_exceeded",
        ),
    ] {
        let mut context = encode_context();
        context.response.stop_reason = reason;
        let mut codec = AnthropicCodec::new(context);
        codec
            .encode_event(&CanonicalEvent::Created {
                response_id: "msg_stop".to_owned(),
            })
            .unwrap();
        let encoded = codec
            .encode_event(&CanonicalEvent::Usage(Usage {
                input_tokens: 1,
                output_tokens: 1,
                cached_input_tokens: None,
                reasoning_tokens: None,
                extensions: BTreeMap::new(),
            }))
            .unwrap();
        let mut decoder = SseDecoder::default();
        let frame = decoder.push(&encoded).unwrap().remove(0);
        let value: Value = serde_json::from_str(&frame.data).unwrap();
        assert_eq!(value["delta"]["stop_reason"], expected);
    }
}

#[test]
fn stream_rejects_missing_repeated_and_late_lifecycle_events() {
    let created = CanonicalEvent::Created {
        response_id: "msg_state".to_owned(),
    };
    for event in [
        CanonicalEvent::OutputTextDelta {
            item_id: "text".to_owned(),
            delta: "x".to_owned(),
        },
        CanonicalEvent::Usage(Usage {
            input_tokens: 1,
            output_tokens: 1,
            cached_input_tokens: None,
            reasoning_tokens: None,
            extensions: BTreeMap::new(),
        }),
        CanonicalEvent::Completed,
    ] {
        assert_eq!(
            AnthropicCodec::new(encode_context())
                .encode_event(&event)
                .unwrap_err()
                .code(),
            "invalid_request"
        );
    }

    let mut repeated_created = AnthropicCodec::new(encode_context());
    repeated_created.encode_event(&created).unwrap();
    assert_eq!(
        repeated_created.encode_event(&created).unwrap_err().code(),
        "invalid_request"
    );

    let mut late_delta = AnthropicCodec::new(encode_context());
    late_delta.encode_event(&created).unwrap();
    late_delta
        .encode_event(&CanonicalEvent::Usage(Usage {
            input_tokens: 1,
            output_tokens: 1,
            cached_input_tokens: None,
            reasoning_tokens: None,
            extensions: BTreeMap::new(),
        }))
        .unwrap();
    assert_eq!(
        late_delta
            .encode_event(&CanonicalEvent::OutputTextDelta {
                item_id: "text".to_owned(),
                delta: "late".to_owned(),
            })
            .unwrap_err()
            .code(),
        "invalid_request"
    );
}

#[test]
fn non_stream_message_uses_trusted_id_model_stop_reason_and_usage() {
    let events = [
        CanonicalEvent::Created {
            response_id: "msg_non_stream".to_owned(),
        },
        CanonicalEvent::OutputTextDelta {
            item_id: "text_1".to_owned(),
            delta: "Hello".to_owned(),
        },
        CanonicalEvent::ToolCallDelta {
            item_id: "tool_1".to_owned(),
            call_id: "toolu_non_stream".to_owned(),
            name: "read_file".to_owned(),
            delta: "{\"path\":\"README.md\"}".to_owned(),
        },
        CanonicalEvent::Usage(Usage {
            input_tokens: 11,
            output_tokens: 17,
            cached_input_tokens: Some(3),
            reasoning_tokens: None,
            extensions: BTreeMap::from([(
                "cache_creation_input_tokens".to_owned(),
                serde_json::json!(2),
            )]),
        }),
        CanonicalEvent::Completed,
    ];

    assert_eq!(
        AnthropicCodec::encode_response(encode_context(), &events).unwrap(),
        fixture_json("response/complete.json")
    );
}

#[test]
fn non_stream_message_rejects_empty_content() {
    let events = [
        CanonicalEvent::Created {
            response_id: "msg_empty".to_owned(),
        },
        CanonicalEvent::Usage(Usage {
            input_tokens: 1,
            output_tokens: 0,
            cached_input_tokens: None,
            reasoning_tokens: None,
            extensions: BTreeMap::new(),
        }),
        CanonicalEvent::Completed,
    ];

    assert_eq!(
        AnthropicCodec::encode_response(encode_context(), &events)
            .unwrap_err()
            .code(),
        "invalid_request"
    );
}

#[test]
fn non_stream_thinking_maps_signature_and_reasoning_usage() {
    let mut context = encode_context();
    let signature = "s".repeat(513);
    context
        .response
        .thinking_signatures
        .insert("thinking_1".to_owned(), signature.clone());
    let encoded = AnthropicCodec::encode_response(
        context,
        &[
            CanonicalEvent::Created {
                response_id: "msg_reasoning".to_owned(),
            },
            CanonicalEvent::ReasoningDelta {
                item_id: "thinking_1".to_owned(),
                delta: "Summary.".to_owned(),
            },
            CanonicalEvent::Usage(Usage {
                input_tokens: 2,
                output_tokens: 3,
                cached_input_tokens: None,
                reasoning_tokens: Some(2),
                extensions: BTreeMap::new(),
            }),
            CanonicalEvent::Completed,
        ],
    )
    .unwrap();

    assert_eq!(encoded["content"][0]["type"], "thinking");
    assert_eq!(encoded["content"][0]["signature"], signature);
    assert_eq!(
        encoded["usage"]["output_tokens_details"]["thinking_tokens"],
        2
    );
}
