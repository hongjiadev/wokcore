use std::{collections::BTreeMap, fs, path::PathBuf};

use serde_json::{Value, json};
use wokcore_protocols::{
    ChatCodec, ChatEncodeContext, ChatFinishReason, ChatResponseTemplate,
    canonical::{
        CanonicalEvent, GatewayError, ImageDetail, InputItem, PublicModelId, RequestId, Usage,
    },
    stream::SseDecoder,
};

fn fixture_json(relative: &str) -> Value {
    serde_json::from_slice(&fixture_bytes(relative)).unwrap()
}

fn fixture_bytes(relative: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/protocols/chat")
        .join(relative);
    fs::read(path).unwrap()
}

fn encode_context(finish_reason: ChatFinishReason) -> ChatEncodeContext {
    ChatEncodeContext {
        model: PublicModelId::new("gpt-test"),
        created: 1_723_456_789,
        response: ChatResponseTemplate {
            choice_index: 0,
            finish_reason,
            logprobs: None,
            include_usage: true,
            extra: BTreeMap::from([
                ("choices".to_owned(), json!("must-not-override")),
                ("created".to_owned(), json!(0)),
                ("id".to_owned(), json!("must-not-override")),
                ("model".to_owned(), json!("must-not-override")),
                ("object".to_owned(), json!("must-not-override")),
                ("service_tier".to_owned(), json!("default")),
                ("usage".to_owned(), json!("must-not-override")),
                ("vendor_trace".to_owned(), json!("trace-safe")),
            ]),
        },
    }
}

fn canonical_events() -> Vec<CanonicalEvent> {
    vec![
        CanonicalEvent::Created {
            response_id: "chatcmpl_fixture".to_owned(),
        },
        CanonicalEvent::OutputTextDelta {
            item_id: "message_1".to_owned(),
            delta: "Hello".to_owned(),
        },
        CanonicalEvent::ToolCallDelta {
            item_id: "tool_1".to_owned(),
            call_id: "call_read".to_owned(),
            name: "read_file".to_owned(),
            delta: "{\"path\":\"".to_owned(),
        },
        CanonicalEvent::ToolCallDelta {
            item_id: "tool_2".to_owned(),
            call_id: "call_list".to_owned(),
            name: "list_files".to_owned(),
            delta: "{\"path\":\"src\"}".to_owned(),
        },
        CanonicalEvent::ToolCallDelta {
            item_id: "tool_1".to_owned(),
            call_id: "call_read".to_owned(),
            name: "read_file".to_owned(),
            delta: "README.md\"}".to_owned(),
        },
        CanonicalEvent::Usage(Usage {
            input_tokens: 12,
            output_tokens: 7,
            cached_input_tokens: Some(5),
            reasoning_tokens: Some(2),
            extensions: BTreeMap::from([
                ("prompt_tokens".to_owned(), json!(999)),
                ("total_tokens".to_owned(), json!(999)),
                ("vendor_usage".to_owned(), json!(true)),
            ]),
        }),
        CanonicalEvent::Completed,
    ]
}

#[test]
fn parallel_tool_request_preserves_roles_tools_and_extensions() {
    let request_id = RequestId::new("request-chat-fixture");
    let canonical = ChatCodec::decode_request(
        request_id.clone(),
        &fixture_bytes("request/parallel_tools.json"),
    )
    .unwrap();

    assert_eq!(canonical.request_id, request_id);
    assert_eq!(canonical.model.as_str(), "gpt-test");
    assert!(canonical.stream);
    assert_eq!(canonical.tools.len(), 2);
    assert_ne!(canonical.tools[0].name, canonical.tools[1].name);
    assert_eq!(canonical.tools[0].extensions["strict"], true);
    assert_eq!(
        canonical.tools[1].extensions["chat.wrapper"]["vendor_tool"],
        "kept"
    );
    assert_eq!(canonical.extensions["chat.parallel_tool_calls"], true);
    assert_eq!(
        canonical.extensions["chat.stream_options"],
        json!({"include_usage": true})
    );
    assert_eq!(canonical.extensions["temperature"], 0.2);
    assert_eq!(
        canonical.reasoning.as_ref().unwrap().effort.as_deref(),
        Some("medium")
    );

    let messages = canonical.extensions["chat.messages"].as_array().unwrap();
    assert_eq!(
        messages
            .iter()
            .map(|message| message["role"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["system", "developer", "user", "assistant", "tool", "tool"]
    );
    assert_eq!(messages[1]["name"], "router");
    assert_eq!(
        messages[1]["content_extra"][0]["extra"]["vendor_part"],
        "kept"
    );
    assert_eq!(messages[3]["tool_calls"][0]["id"], "call_read");
    assert_eq!(messages[3]["tool_calls"][1]["id"], "call_list");
    assert_eq!(
        messages[3]["tool_calls"][0]["function"]["arguments"],
        "{\"path\":\"README.md\"}"
    );
    assert_eq!(messages[4]["tool_call_id"], "call_read");
    assert_eq!(messages[5]["tool_call_id"], "call_list");
    assert_eq!(
        canonical
            .input
            .iter()
            .find_map(|item| match item {
                InputItem::ToolResult { call_id, output } if call_id == "call_list" => Some(output),
                _ => None,
            })
            .unwrap(),
        &json!([{"type": "text", "text": "main.rs"}])
    );
    assert_eq!(
        canonical
            .input
            .iter()
            .filter(|item| matches!(item, InputItem::ToolResult { .. }))
            .count(),
        2
    );
}

#[test]
fn multimodal_user_message_maps_text_image_and_nested_metadata() {
    let canonical = ChatCodec::decode_request(
        RequestId::new("request-image"),
        &fixture_bytes("request/multimodal.json"),
    )
    .unwrap();

    assert_eq!(canonical.input.len(), 2);
    assert_eq!(
        canonical.input[0],
        InputItem::Text {
            text: "Describe this image.".to_owned()
        }
    );
    match &canonical.input[1] {
        InputItem::ImageUrl { url, detail } => {
            assert_eq!(url.as_str(), "https://example.com/image.png");
            assert_eq!(*detail, Some(ImageDetail::High));
        }
        other => panic!("expected image URL, got {other:?}"),
    }
    assert_eq!(
        canonical.extensions["chat.messages"][0]["content_extra"][0]["index"],
        1
    );
    assert_eq!(
        canonical.extensions["chat.messages"][0]["content_extra"][0]["extra"]["image_url"]["vendor_image"],
        true
    );
}

#[test]
fn reserved_namespaced_extension_keys_round_trip_without_collision() {
    let request = json!({
        "model": "gpt-test",
        "messages": [{"role": "user", "content": "hello"}],
        "stream": true,
        "parallel_tool_calls": true,
        "stream_options": {"include_usage": true},
        "chat.messages": "client-value",
        "chat.messages.nested": "client-nested",
        "chat.parallel_tool_calls": "client-parallel",
        "chat.stream_options": "client-stream"
    });
    let encoded = serde_json::to_vec(&request).unwrap();
    let canonical =
        ChatCodec::decode_request(RequestId::new("request-collision"), &encoded).unwrap();

    assert_eq!(canonical.extensions["chat.messages"], "client-value");
    assert_eq!(
        canonical.extensions["chat.messages.nested"],
        "client-nested"
    );
    assert_eq!(
        canonical.extensions["chat.messages.nested.2"][0]["role"],
        "user"
    );
    assert_eq!(
        canonical.extensions["chat.parallel_tool_calls"],
        "client-parallel"
    );
    assert_eq!(
        canonical.extensions["chat.parallel_tool_calls.nested"],
        true
    );
    assert_eq!(canonical.extensions["chat.stream_options"], "client-stream");
    assert_eq!(
        canonical.extensions["chat.stream_options.nested"],
        json!({"include_usage": true})
    );
}

#[test]
fn invalid_requests_return_typed_errors() {
    let overlong_name = "a".repeat(65);
    let cases = [
        json!({"messages": [{"role": "user", "content": "missing model"}]}),
        json!({"model": "", "messages": [{"role": "user", "content": "x"}]}),
        json!({"model": "gpt-test"}),
        json!({"model": "gpt-test", "messages": []}),
        json!({"model": "gpt-test", "messages": [{"role": "unknown", "content": "x"}]}),
        json!({"model": "gpt-test", "messages": [{"role": "tool", "tool_call_id": "", "content": "x"}]}),
        json!({"model": "gpt-test", "messages": [{"role": "user", "content": [{"type": "image_url", "image_url": {"url": "file:///secret"}}]}]}),
        json!({"model": "gpt-test", "messages": [{"role": "assistant", "tool_calls": [{"id": "call", "type": "function", "function": {"name": "", "arguments": "{}"}}]}]}),
        json!({"model": "gpt-test", "messages": [{"role": "assistant", "tool_calls": [
            {"id": "call", "type": "function", "function": {"name": "a", "arguments": "{}"}},
            {"id": "call", "type": "function", "function": {"name": "b", "arguments": "{}"}}
        ]}]}),
        json!({"model": "gpt-test", "messages": [{"role": "user", "content": "x"}], "tools": [{"type": "function", "function": {"name": "bad name"}}]}),
        json!({"model": "gpt-test", "messages": [{"role": "user", "content": "x"}], "tools": [{"type": "function", "function": {"name": overlong_name}}]}),
        json!({"model": "gpt-test", "messages": [{"role": "user", "content": "x"}], "tools": [{"type": "custom", "custom": {"name": "x"}}]}),
        json!({"model": "gpt-test", "messages": [{"role": "user", "content": "x"}], "stream_options": {"include_usage": true}}),
    ];

    for request in cases {
        let encoded = serde_json::to_vec(&request).unwrap();
        assert_eq!(
            ChatCodec::decode_request(RequestId::new("request-invalid"), &encoded).unwrap_err(),
            GatewayError::invalid_request(),
            "unexpected result for {request}"
        );
    }
}

#[test]
fn omitted_function_parameters_map_to_an_empty_schema() {
    let request = json!({
        "model": "gpt-test",
        "messages": [{"role": "user", "content": "call it"}],
        "tools": [{"type": "function", "function": {"name": "no_args"}}]
    });
    let canonical = ChatCodec::decode_request(
        RequestId::new("request-no-args"),
        &serde_json::to_vec(&request).unwrap(),
    )
    .unwrap();

    assert_eq!(canonical.tools[0].input_schema, json!({}));
}

#[test]
fn non_stream_response_matches_hand_normalized_golden() {
    let encoded = ChatCodec::encode_response(
        encode_context(ChatFinishReason::ToolCalls),
        &canonical_events(),
    )
    .unwrap();

    assert_eq!(encoded, fixture_json("response/complete.json"));
    assert_eq!(encoded["model"], "gpt-test");
    assert_eq!(encoded["usage"]["prompt_tokens"], 12);
    assert_eq!(encoded["usage"]["total_tokens"], 19);
}

#[test]
fn all_official_finish_reasons_are_encoded_verbatim() {
    for (finish_reason, expected) in [
        (ChatFinishReason::Stop, "stop"),
        (ChatFinishReason::Length, "length"),
        (ChatFinishReason::ToolCalls, "tool_calls"),
        (ChatFinishReason::ContentFilter, "content_filter"),
        (ChatFinishReason::FunctionCall, "function_call"),
    ] {
        let response = ChatCodec::encode_response(
            encode_context(finish_reason),
            &[
                CanonicalEvent::Created {
                    response_id: "chatcmpl_reason".to_owned(),
                },
                CanonicalEvent::Usage(Usage {
                    input_tokens: 1,
                    output_tokens: 0,
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                    extensions: BTreeMap::new(),
                }),
                CanonicalEvent::Completed,
            ],
        )
        .unwrap();
        assert_eq!(response["choices"][0]["finish_reason"], expected);
    }
}

#[test]
fn outbound_function_names_use_the_canonical_ascii_contract() {
    let invalid_names = [
        ("empty", String::new()),
        ("invalid character", "bad name".to_owned()),
        ("non-ASCII", "工具".to_owned()),
        ("overlong", "a".repeat(65)),
    ];

    for (case, name) in invalid_names {
        let tool_delta = CanonicalEvent::ToolCallDelta {
            item_id: "tool_1".to_owned(),
            call_id: "call_1".to_owned(),
            name,
            delta: "{}".to_owned(),
        };
        let events = [
            CanonicalEvent::Created {
                response_id: "chatcmpl_invalid_name".to_owned(),
            },
            tool_delta.clone(),
            CanonicalEvent::Usage(Usage {
                input_tokens: 1,
                output_tokens: 1,
                cached_input_tokens: None,
                reasoning_tokens: None,
                extensions: BTreeMap::new(),
            }),
            CanonicalEvent::Completed,
        ];

        assert_eq!(
            ChatCodec::encode_response(encode_context(ChatFinishReason::ToolCalls), &events)
                .unwrap_err(),
            GatewayError::invalid_request(),
            "non-stream accepted {case} function name"
        );

        let mut codec = ChatCodec::new(encode_context(ChatFinishReason::ToolCalls));
        codec.encode_chunk(&events[0]).unwrap();
        assert_eq!(
            codec.encode_chunk(&tool_delta).unwrap_err(),
            GatewayError::invalid_request(),
            "stream accepted {case} function name"
        );
    }
}

#[test]
fn usage_details_merge_unknown_siblings_without_overriding_canonical_counts() {
    let response = ChatCodec::encode_response(
        encode_context(ChatFinishReason::Stop),
        &[
            CanonicalEvent::Created {
                response_id: "chatcmpl_usage_merge".to_owned(),
            },
            CanonicalEvent::Usage(Usage {
                input_tokens: 11,
                output_tokens: 7,
                cached_input_tokens: Some(5),
                reasoning_tokens: Some(3),
                extensions: BTreeMap::from([
                    ("prompt_tokens".to_owned(), json!(999)),
                    (
                        "prompt_tokens_details".to_owned(),
                        json!({
                            "cached_tokens": 998,
                            "audio_tokens": 2
                        }),
                    ),
                    (
                        "completion_tokens_details".to_owned(),
                        json!({
                            "reasoning_tokens": 997,
                            "accepted_prediction_tokens": 4
                        }),
                    ),
                    ("vendor_usage".to_owned(), json!("preserved")),
                ]),
            }),
            CanonicalEvent::Completed,
        ],
    )
    .unwrap();

    assert_eq!(response["usage"]["prompt_tokens"], 11);
    assert_eq!(
        response["usage"]["prompt_tokens_details"],
        json!({"cached_tokens": 5, "audio_tokens": 2})
    );
    assert_eq!(
        response["usage"]["completion_tokens_details"],
        json!({"reasoning_tokens": 3, "accepted_prediction_tokens": 4})
    );
    assert_eq!(response["usage"]["vendor_usage"], "preserved");
}

#[test]
fn stream_keeps_parallel_tool_argument_deltas_as_unparsed_strings() {
    let expected = fixture_json("stream/ordered.json");
    let mut codec = ChatCodec::new(encode_context(ChatFinishReason::ToolCalls));
    let mut actual = Vec::new();

    for event in canonical_events() {
        let encoded = codec.encode_chunk(&event).unwrap();
        let mut decoder = SseDecoder::default();
        let frames = decoder.push(&encoded).unwrap();
        actual.extend(frames.into_iter().map(|frame| {
            let data = if frame.data == "[DONE]" {
                json!("[DONE]")
            } else {
                serde_json::from_str::<Value>(&frame.data).unwrap()
            };
            json!({"data": data})
        }));
    }

    assert_eq!(Value::Array(actual), expected);
}

#[test]
fn failure_envelopes_are_typed_and_redact_private_diagnostics() {
    let secret = "Authorization: Bearer chat-secret";
    let events = [CanonicalEvent::Failed(GatewayError::upstream_auth(secret))];
    let response =
        ChatCodec::encode_response(encode_context(ChatFinishReason::Stop), &events).unwrap();
    let response_text = serde_json::to_string(&response).unwrap();

    assert_eq!(response, fixture_json("error/failed.json"));
    assert!(!response_text.contains(secret));
    assert!(!response_text.contains("Bearer"));

    let mut codec = ChatCodec::new(encode_context(ChatFinishReason::Stop));
    let stream = codec.encode_chunk(&events[0]).unwrap();
    let stream_text = String::from_utf8(stream.to_vec()).unwrap();
    assert!(!stream_text.contains(secret));
    assert!(stream_text.contains("[DONE]"));
}

#[test]
fn state_machine_identity_conflicts_and_reasoning_are_typed_errors() {
    let mut missing_created = ChatCodec::new(encode_context(ChatFinishReason::Stop));
    assert_eq!(
        missing_created
            .encode_chunk(&CanonicalEvent::OutputTextDelta {
                item_id: "message".to_owned(),
                delta: "late".to_owned(),
            })
            .unwrap_err(),
        GatewayError::invalid_request()
    );

    let mut codec = ChatCodec::new(encode_context(ChatFinishReason::ToolCalls));
    codec
        .encode_chunk(&CanonicalEvent::Created {
            response_id: "chatcmpl_state".to_owned(),
        })
        .unwrap();
    codec
        .encode_chunk(&CanonicalEvent::ToolCallDelta {
            item_id: "tool_1".to_owned(),
            call_id: "call_1".to_owned(),
            name: "read_file".to_owned(),
            delta: "{".to_owned(),
        })
        .unwrap();
    assert_eq!(
        codec
            .encode_chunk(&CanonicalEvent::ToolCallDelta {
                item_id: "tool_1".to_owned(),
                call_id: "call_2".to_owned(),
                name: "read_file".to_owned(),
                delta: "}".to_owned(),
            })
            .unwrap_err(),
        GatewayError::invalid_request()
    );
    assert_eq!(
        codec
            .encode_chunk(&CanonicalEvent::ReasoningDelta {
                item_id: "reasoning".to_owned(),
                delta: "private chain".to_owned(),
            })
            .unwrap_err(),
        GatewayError::unsupported_capability()
    );

    let mut failed = ChatCodec::new(encode_context(ChatFinishReason::Stop));
    failed
        .encode_chunk(&CanonicalEvent::Failed(GatewayError::internal("private")))
        .unwrap();
    assert_eq!(
        failed
            .encode_chunk(&CanonicalEvent::Created {
                response_id: "late".to_owned(),
            })
            .unwrap_err(),
        GatewayError::invalid_request()
    );
}

#[test]
fn completion_requires_created_and_usage_and_rejects_repeated_lifecycle_events() {
    let usage = CanonicalEvent::Usage(Usage {
        input_tokens: 1,
        output_tokens: 1,
        cached_input_tokens: None,
        reasoning_tokens: None,
        extensions: BTreeMap::new(),
    });
    for event in [usage.clone(), CanonicalEvent::Completed] {
        let mut codec = ChatCodec::new(encode_context(ChatFinishReason::Stop));
        assert_eq!(
            codec.encode_chunk(&event).unwrap_err(),
            GatewayError::invalid_request()
        );
    }

    let mut missing_usage = ChatCodec::new(encode_context(ChatFinishReason::Stop));
    let created = CanonicalEvent::Created {
        response_id: "chatcmpl_lifecycle".to_owned(),
    };
    missing_usage.encode_chunk(&created).unwrap();
    assert_eq!(
        missing_usage
            .encode_chunk(&CanonicalEvent::Completed)
            .unwrap_err(),
        GatewayError::invalid_request()
    );

    let mut repeated = ChatCodec::new(encode_context(ChatFinishReason::Stop));
    repeated.encode_chunk(&created).unwrap();
    assert_eq!(
        repeated.encode_chunk(&created).unwrap_err(),
        GatewayError::invalid_request()
    );
    repeated.encode_chunk(&usage).unwrap();
    assert_eq!(
        repeated.encode_chunk(&usage).unwrap_err(),
        GatewayError::invalid_request()
    );
}

#[test]
fn stream_template_controls_whether_usage_chunks_are_emitted() {
    let mut context = encode_context(ChatFinishReason::Stop);
    context.response.include_usage = false;
    let mut codec = ChatCodec::new(context);
    let mut encoded = Vec::new();
    for event in [
        CanonicalEvent::Created {
            response_id: "chatcmpl_no_usage".to_owned(),
        },
        CanonicalEvent::Usage(Usage {
            input_tokens: 1,
            output_tokens: 0,
            cached_input_tokens: None,
            reasoning_tokens: None,
            extensions: BTreeMap::new(),
        }),
        CanonicalEvent::Completed,
    ] {
        encoded.extend_from_slice(&codec.encode_chunk(&event).unwrap());
    }
    let encoded = String::from_utf8(encoded).unwrap();

    assert!(!encoded.contains("\"usage\""));
    assert!(encoded.ends_with("data: [DONE]\n\n"));
}
