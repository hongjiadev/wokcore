use std::collections::BTreeMap;

use serde_json::{Value, json};
use url::Url;
use wokcore_protocols::{
    UpstreamLimits,
    canonical::{
        CanonicalEvent, CanonicalRequest, InputItem, PublicModelId, ReasoningOptions, RequestId,
        ToolDefinition,
    },
    upstream::{UpstreamAdapter, UpstreamOperation, UpstreamProtocol},
};

fn request(model: &str, stream: bool) -> CanonicalRequest {
    CanonicalRequest {
        request_id: RequestId::new("req_unified"),
        model: PublicModelId::new(model),
        thread_key: None,
        input: vec![InputItem::Text {
            text: "Use the weather tool.".to_owned(),
        }],
        tools: vec![ToolDefinition {
            name: "weather".to_owned(),
            description: Some("Get weather".to_owned()),
            input_schema: json!({
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }),
            extensions: BTreeMap::new(),
        }],
        stream,
        reasoning: Some(ReasoningOptions {
            effort: Some("medium".to_owned()),
            extensions: BTreeMap::new(),
        }),
        extensions: BTreeMap::new(),
    }
}

fn adapter(protocol: UpstreamProtocol, base_url: &str) -> UpstreamAdapter {
    UpstreamAdapter::new(
        protocol,
        Url::parse(base_url).unwrap(),
        UpstreamLimits::default(),
    )
    .unwrap()
}

#[test]
fn unified_upstream_builds_five_secret_free_request_shapes() {
    let cases = [
        (
            UpstreamProtocol::OpenAiResponses,
            "http://127.0.0.1:31001/v1",
            "model-a",
            "http://127.0.0.1:31001/v1/responses",
            "model",
        ),
        (
            UpstreamProtocol::OpenAiChat,
            "http://127.0.0.1:31002/v1/",
            "model-b",
            "http://127.0.0.1:31002/v1/chat/completions",
            "model",
        ),
        (
            UpstreamProtocol::Anthropic,
            "http://127.0.0.1:31003",
            "model-c",
            "http://127.0.0.1:31003/v1/messages",
            "model",
        ),
        (
            UpstreamProtocol::Gemini,
            "http://127.0.0.1:31004",
            "gemini-test",
            "http://127.0.0.1:31004/v1beta/models/gemini-test:streamGenerateContent?alt=sse",
            "contents",
        ),
        (
            UpstreamProtocol::AzureOpenAi,
            "http://127.0.0.1:31005/openai",
            "deployment-a",
            "http://127.0.0.1:31005/openai/deployments/deployment-a/chat/completions?api-version=2024-10-21",
            "messages",
        ),
    ];

    for (protocol, base_url, model, expected_url, required_body_field) in cases {
        let outbound = adapter(protocol, base_url)
            .build_request(&request(model, true), UpstreamOperation::Text)
            .unwrap();
        assert_eq!(outbound.url.as_str(), expected_url);
        assert!(outbound.stream);
        assert!(outbound.headers.contains_key("content-type"));
        for forbidden in ["authorization", "api-key", "x-api-key", "x-goog-api-key"] {
            assert!(!outbound.headers.contains_key(forbidden));
        }
        let body: Value = serde_json::from_slice(&outbound.body).unwrap();
        assert!(body.get(required_body_field).is_some());
        assert!(!String::from_utf8_lossy(&outbound.body).contains("secret"));
    }
}

#[test]
fn unified_upstream_decodes_five_complete_response_shapes() {
    let cases = [
        (
            UpstreamProtocol::OpenAiResponses,
            "http://127.0.0.1:31101/v1",
            json!({
                "id": "resp_complete",
                "output": [{
                    "id": "message_0",
                    "type": "message",
                    "content": [{"type": "output_text", "text": "hello"}]
                }],
                "usage": {"input_tokens": 3, "output_tokens": 2}
            }),
        ),
        (
            UpstreamProtocol::OpenAiChat,
            "http://127.0.0.1:31102/v1",
            json!({
                "id": "chat_complete",
                "choices": [{
                    "message": {"role": "assistant", "content": "hello"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 3, "completion_tokens": 2}
            }),
        ),
        (
            UpstreamProtocol::Anthropic,
            "http://127.0.0.1:31103",
            json!({
                "id": "msg_complete",
                "type": "message",
                "content": [{"type": "text", "text": "hello"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 3, "output_tokens": 2}
            }),
        ),
        (
            UpstreamProtocol::Gemini,
            "http://127.0.0.1:31104",
            json!({
                "responseId": "gemini_complete",
                "candidates": [{
                    "content": {"parts": [{"text": "hello"}]},
                    "finishReason": "STOP"
                }],
                "usageMetadata": {
                    "promptTokenCount": 3,
                    "candidatesTokenCount": 2
                }
            }),
        ),
        (
            UpstreamProtocol::AzureOpenAi,
            "http://127.0.0.1:31105/openai",
            json!({
                "id": "azure_complete",
                "choices": [{
                    "message": {"role": "assistant", "content": "hello"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 3, "completion_tokens": 2}
            }),
        ),
    ];

    for (protocol, base_url, response) in cases {
        let events = adapter(protocol, base_url)
            .decode_response(
                RequestId::new("req_complete"),
                &serde_json::to_vec(&response).unwrap(),
            )
            .unwrap();
        assert!(matches!(
            events.first(),
            Some(CanonicalEvent::Created { .. })
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            CanonicalEvent::OutputTextDelta { delta, .. } if delta == "hello"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            CanonicalEvent::Usage(usage)
                if usage.input_tokens == 3 && usage.output_tokens == 2
        )));
        assert_eq!(events.last(), Some(&CanonicalEvent::Completed));
    }
}

#[test]
fn unified_upstream_decodes_fragmented_streams_for_five_protocols() {
    let cases = [
        (
            UpstreamProtocol::OpenAiResponses,
            "http://127.0.0.1:31201/v1",
            concat!(
                "event: response.created\n",
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_stream\"}}\n\n",
                "event: response.output_text.delta\n",
                "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"message_0\",\"delta\":\"hello\"}\n\n",
                "event: response.completed\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n"
            ),
        ),
        (
            UpstreamProtocol::OpenAiChat,
            "http://127.0.0.1:31202/v1",
            concat!(
                "data: {\"id\":\"chat_stream\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hello\"},\"finish_reason\":null}]}\n\n",
                "data: {\"id\":\"chat_stream\",\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n",
                "data: [DONE]\n\n"
            ),
        ),
        (
            UpstreamProtocol::Anthropic,
            "http://127.0.0.1:31203",
            concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_stream\",\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
                "event: content_block_start\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
                "event: message_delta\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
                "event: message_stop\n",
                "data: {\"type\":\"message_stop\"}\n\n"
            ),
        ),
        (
            UpstreamProtocol::Gemini,
            "http://127.0.0.1:31204",
            "data: {\"responseId\":\"gemini_stream\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hello\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":2}}\n\n",
        ),
        (
            UpstreamProtocol::AzureOpenAi,
            "http://127.0.0.1:31205/openai",
            concat!(
                "data: {\"id\":\"azure_stream\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hello\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: {\"id\":\"azure_stream\",\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n",
                "data: [DONE]\n\n"
            ),
        ),
    ];

    for (protocol, base_url, body) in cases {
        let mut decoder = adapter(protocol, base_url).stream_decoder(RequestId::new("req_stream"));
        let bytes = body.as_bytes();
        let first = bytes.len() / 3;
        let second = first * 2;
        let mut events = decoder.push(&bytes[..first]).unwrap();
        events.extend(decoder.push(&bytes[first..second]).unwrap());
        events.extend(decoder.push(&bytes[second..]).unwrap());
        events.extend(decoder.finish().unwrap());

        assert!(matches!(
            events.first(),
            Some(CanonicalEvent::Created { .. })
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            CanonicalEvent::OutputTextDelta { delta, .. } if delta == "hello"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            CanonicalEvent::Usage(usage)
                if usage.input_tokens == 3 && usage.output_tokens == 2
        )));
        assert_eq!(events.last(), Some(&CanonicalEvent::Completed));
    }
}

#[test]
fn unified_upstream_builds_anthropic_count_tokens_without_credentials() {
    let outbound = adapter(UpstreamProtocol::Anthropic, "http://127.0.0.1:31301")
        .build_request(
            &request("claude-test", false),
            UpstreamOperation::CountTokens,
        )
        .unwrap();

    assert_eq!(
        outbound.url.as_str(),
        "http://127.0.0.1:31301/v1/messages/count_tokens"
    );
    assert!(!outbound.stream);
    assert!(outbound.headers.contains_key("anthropic-version"));
    assert_eq!(
        outbound
            .headers
            .keys()
            .filter(|name| {
                matches!(
                    name.as_str(),
                    "authorization" | "api-key" | "x-api-key" | "x-goog-api-key"
                )
            })
            .count(),
        0
    );
}
