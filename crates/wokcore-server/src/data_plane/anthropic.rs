use std::collections::BTreeMap;

use axum::response::Response;
use serde_json::json;
use wokcore_protocols::{
    AnthropicCodec, AnthropicEncodeContext, AnthropicResponseTemplate, AnthropicStopReason,
    canonical::{GatewayError, Usage},
};

use super::{
    ExecutedResponse, UpstreamExecutionOutput, UpstreamFinishReason,
    response::bounded_json_response,
};

pub(crate) fn encode_message(
    executed: &ExecutedResponse,
    request_id: wokcore_protocols::canonical::RequestId,
) -> Result<Response, GatewayError> {
    let UpstreamExecutionOutput::Events(events) = executed.response.output() else {
        return Err(GatewayError::internal("unexpected upstream output"));
    };
    let value = AnthropicCodec::encode_response(
        AnthropicEncodeContext {
            request_id,
            model: executed.public_model.clone(),
            initial_usage: initial_usage(events),
            response: AnthropicResponseTemplate {
                stop_reason: stop_reason(executed.response.finish_reason()),
                stop_sequence: executed.response.stop_sequence().map(str::to_owned),
                thinking_signatures: executed.response.thinking_signatures().clone(),
                extra: BTreeMap::new(),
            },
        },
        events,
    )?;
    bounded_json_response(&value, executed.response.upstream_request_id())
}

pub(crate) fn encode_token_count(executed: &ExecutedResponse) -> Result<Response, GatewayError> {
    let UpstreamExecutionOutput::TokenCount(input_tokens) = executed.response.output() else {
        return Err(GatewayError::internal("unexpected upstream output"));
    };
    bounded_json_response(
        &json!({"input_tokens": input_tokens}),
        executed.response.upstream_request_id(),
    )
}

fn initial_usage(events: &[wokcore_protocols::canonical::CanonicalEvent]) -> Usage {
    let usage = events.iter().find_map(|event| match event {
        wokcore_protocols::canonical::CanonicalEvent::Usage(usage) => Some(usage),
        _ => None,
    });
    Usage {
        input_tokens: usage.map_or(0, |usage| usage.input_tokens),
        output_tokens: 0,
        cached_input_tokens: usage.and_then(|usage| usage.cached_input_tokens),
        reasoning_tokens: None,
        extensions: BTreeMap::new(),
    }
}

fn stop_reason(reason: UpstreamFinishReason) -> AnthropicStopReason {
    match reason {
        UpstreamFinishReason::Stop
        | UpstreamFinishReason::ContentFilter
        | UpstreamFinishReason::FunctionCall => AnthropicStopReason::EndTurn,
        UpstreamFinishReason::Length => AnthropicStopReason::MaxTokens,
        UpstreamFinishReason::ToolCalls => AnthropicStopReason::ToolUse,
        UpstreamFinishReason::PauseTurn => AnthropicStopReason::PauseTurn,
        UpstreamFinishReason::Refusal => AnthropicStopReason::Refusal,
        UpstreamFinishReason::ModelContextWindowExceeded => {
            AnthropicStopReason::ModelContextWindowExceeded
        }
    }
}
