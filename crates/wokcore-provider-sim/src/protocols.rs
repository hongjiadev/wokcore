use serde_json::json;

use crate::scenario::{FrameMode, PayloadProfile, Protocol, Scenario};

pub(crate) fn render(scenario: &Scenario) -> Vec<Vec<u8>> {
    if scenario.status() >= 400 {
        return vec![
            format!(
                "{{\"error\":{{\"type\":\"synthetic_error\",\"status\":{}}}}}",
                scenario.status()
            )
            .into_bytes(),
        ];
    }
    if !scenario.stream() {
        return vec![non_stream(scenario.protocol()).into_bytes()];
    }

    match scenario.protocol() {
        Protocol::OpenAiResponses => responses_events(scenario),
        Protocol::OpenAiChat | Protocol::AzureOpenAi => chat_events(scenario),
        Protocol::Anthropic => anthropic_events(scenario),
        Protocol::Gemini => gemini_events(scenario),
    }
}

pub(crate) fn frame(mut frames: Vec<Vec<u8>>, mode: FrameMode, chunk_bytes: usize) -> Vec<Vec<u8>> {
    if mode == FrameMode::Malformed && !frames.is_empty() {
        let midpoint = frames.len() / 2;
        frames[midpoint] = b"data: {malformed\n\n".to_vec();
    }

    let chunks = match mode {
        FrameMode::Coalesced => coalesce(frames, chunk_bytes),
        FrameMode::Partial => frames.into_iter().flat_map(split_half).collect::<Vec<_>>(),
        FrameMode::Utf8Split => split_utf8(frames),
        FrameMode::Normal | FrameMode::Malformed => frames,
    };
    chunks
        .into_iter()
        .flat_map(|chunk| split_at_limit(chunk, chunk_bytes))
        .collect()
}

fn non_stream(protocol: Protocol) -> String {
    match protocol {
        Protocol::OpenAiResponses => json!({
            "id": "resp_synthetic",
            "object": "response",
            "status": "completed",
            "output": [{"type":"message","id":"msg_synthetic","role":"assistant","content":[{"type":"output_text","text":"synthetic"}]}],
            "usage": {"input_tokens":8,"output_tokens":4,"total_tokens":12}
        })
        .to_string(),
        Protocol::OpenAiChat | Protocol::AzureOpenAi => json!({
            "id": "chatcmpl_synthetic",
            "object": "chat.completion",
            "created": 1,
            "model": "synthetic",
            "choices": [{"index":0,"message":{"role":"assistant","content":"synthetic"},"finish_reason":"stop"}],
            "usage": {"prompt_tokens":8,"completion_tokens":4,"total_tokens":12}
        })
        .to_string(),
        Protocol::Anthropic => json!({
            "id": "msg_synthetic",
            "type": "message",
            "role": "assistant",
            "model": "synthetic",
            "content": [{"type":"text","text":"synthetic"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens":8,"output_tokens":4}
        })
        .to_string(),
        Protocol::Gemini => json!({
            "candidates": [{"content":{"role":"model","parts":[{"text":"synthetic"}]},"finishReason":"STOP"}],
            "usageMetadata": {"promptTokenCount":8,"candidatesTokenCount":4,"totalTokenCount":12}
        })
        .to_string(),
    }
}

fn responses_events(scenario: &Scenario) -> Vec<Vec<u8>> {
    let mut events = vec![sse(
        "response.created",
        json!({"type":"response.created","response":{"id":"resp_synthetic","status":"in_progress"}}),
    )];
    if scenario.payload_profile() == PayloadProfile::Tool {
        events.push(sse(
            "response.output_item.added",
            json!({"type":"response.output_item.added","item":{"type":"function_call","id":"tool_synthetic","call_id":"call_synthetic","name":"synthetic_tool"}}),
        ));
    }
    for index in 0..scenario.event_count() {
        let (kind, item_id) = match scenario.payload_profile() {
            PayloadProfile::Standard => ("response.output_text.delta", "msg_synthetic"),
            PayloadProfile::Reasoning => (
                "response.reasoning_summary_text.delta",
                "reasoning_synthetic",
            ),
            PayloadProfile::Tool => ("response.function_call_arguments.delta", "tool_synthetic"),
        };
        events.push(sse(
            kind,
            json!({"type":kind,"item_id":item_id,"output_index":0,"content_index":0,"delta":scenario.content(index)}),
        ));
    }
    if scenario.terminal() {
        events.push(sse(
            "response.completed",
            json!({"type":"response.completed","response":{"id":"resp_synthetic","status":"completed","usage":{"input_tokens":8,"output_tokens":4,"total_tokens":12}}}),
        ));
    }
    events
}

fn chat_events(scenario: &Scenario) -> Vec<Vec<u8>> {
    let mut events = Vec::with_capacity(scenario.event_count().saturating_add(2));
    for index in 0..scenario.event_count() {
        let delta = match scenario.payload_profile() {
            PayloadProfile::Standard if index == 0 => {
                json!({"role":"assistant","content":scenario.content(index)})
            }
            PayloadProfile::Standard => json!({"content":scenario.content(index)}),
            PayloadProfile::Reasoning => {
                json!({"reasoning_content":scenario.content(index)})
            }
            PayloadProfile::Tool => json!({"tool_calls":[{
                "index":0,
                "id":"call_synthetic",
                "type":"function",
                "function":{"name":"synthetic_tool","arguments":scenario.content(index)}
            }]}),
        };
        events.push(
            format!(
                "data: {}\n\n",
                json!({
                    "id":"chatcmpl_synthetic",
                    "object":"chat.completion.chunk",
                    "created":1,
                    "model":"synthetic",
                    "choices":[{"index":0,"delta":delta,"finish_reason":null}]
                })
            )
            .into_bytes(),
        );
    }
    if scenario.terminal() {
        events.push(
            format!(
                "data: {}\n\n",
                json!({"id":"chatcmpl_synthetic","object":"chat.completion.chunk","created":1,"model":"synthetic","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
            )
            .into_bytes(),
        );
        events.push(b"data: [DONE]\n\n".to_vec());
    }
    events
}

fn anthropic_events(scenario: &Scenario) -> Vec<Vec<u8>> {
    let mut events = vec![sse(
        "message_start",
        json!({"type":"message_start","message":{"id":"msg_synthetic","type":"message","role":"assistant","model":"synthetic","content":[],"stop_reason":null,"usage":{"input_tokens":8,"output_tokens":0}}}),
    )];
    let content_block = match scenario.payload_profile() {
        PayloadProfile::Standard => json!({"type":"text","text":""}),
        PayloadProfile::Reasoning => json!({"type":"thinking","thinking":""}),
        PayloadProfile::Tool => {
            json!({"type":"tool_use","id":"toolu_synthetic","name":"synthetic_tool","input":{}})
        }
    };
    events.push(sse(
        "content_block_start",
        json!({"type":"content_block_start","index":0,"content_block":content_block}),
    ));
    for index in 0..scenario.event_count() {
        let delta = match scenario.payload_profile() {
            PayloadProfile::Standard => {
                json!({"type":"text_delta","text":scenario.content(index)})
            }
            PayloadProfile::Reasoning => {
                json!({"type":"thinking_delta","thinking":scenario.content(index)})
            }
            PayloadProfile::Tool => {
                json!({"type":"input_json_delta","partial_json":scenario.content(index)})
            }
        };
        events.push(sse(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":delta}),
        ));
    }
    if scenario.terminal() {
        if scenario.payload_profile() == PayloadProfile::Reasoning {
            events.push(sse(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"synthetic_signature"}}),
            ));
        }
        events.push(sse(
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        ));
        events.push(sse(
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":4}}),
        ));
        events.push(sse("message_stop", json!({"type":"message_stop"})));
    }
    events
}

fn gemini_events(scenario: &Scenario) -> Vec<Vec<u8>> {
    let mut events = Vec::with_capacity(scenario.event_count().saturating_add(1));
    for index in 0..scenario.event_count() {
        let part = match scenario.payload_profile() {
            PayloadProfile::Standard => json!({"text":scenario.content(index)}),
            PayloadProfile::Reasoning => {
                json!({"text":scenario.content(index),"thought":true})
            }
            PayloadProfile::Tool => json!({"functionCall":{
                "name":"synthetic_tool",
                "args":{"synthetic":scenario.content(index)}
            }}),
        };
        events.push(
            format!(
                "data: {}\n\n",
                json!({"candidates":[{"content":{"role":"model","parts":[part]},"finishReason":null}]})
            )
            .into_bytes(),
        );
    }
    if scenario.terminal() {
        events.push(
            format!(
                "data: {}\n\n",
                json!({"candidates":[{"content":{"role":"model","parts":[]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":8,"candidatesTokenCount":4,"totalTokenCount":12}})
            )
            .into_bytes(),
        );
    }
    events
}

fn sse(event: &str, value: serde_json::Value) -> Vec<u8> {
    format!("event: {event}\ndata: {value}\n\n").into_bytes()
}

fn split_half(bytes: Vec<u8>) -> Vec<Vec<u8>> {
    if bytes.len() < 2 {
        return vec![bytes];
    }
    let midpoint = bytes.len() / 2;
    vec![bytes[..midpoint].to_vec(), bytes[midpoint..].to_vec()]
}

fn split_utf8(frames: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut result = Vec::new();
    let mut split = false;
    for frame in frames {
        if !split
            && let Some(index) = frame
                .windows(3)
                .position(|window| window == "烧".as_bytes())
        {
            result.push(frame[..index + 1].to_vec());
            result.push(frame[index + 1..].to_vec());
            split = true;
        } else {
            result.push(frame);
        }
    }
    result
}

fn coalesce(frames: Vec<Vec<u8>>, limit: usize) -> Vec<Vec<u8>> {
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    for frame in frames {
        if !current.is_empty() && current.len().saturating_add(frame.len()) > limit {
            chunks.push(std::mem::take(&mut current));
        }
        current.extend_from_slice(&frame);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn split_at_limit(bytes: Vec<u8>, limit: usize) -> Vec<Vec<u8>> {
    bytes.chunks(limit).map(<[u8]>::to_vec).collect()
}
