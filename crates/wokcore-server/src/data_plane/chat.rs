use std::collections::BTreeMap;

use axum::response::Response;
use wokcore_protocols::{
    ChatCodec, ChatEncodeContext, ChatFinishReason, ChatResponseTemplate, canonical::GatewayError,
};

use super::{
    ExecutedResponse, ExecutedStream, UpstreamExecutionOutput, UpstreamFinishReason,
    response::bounded_json_response,
};

pub(crate) fn encode(executed: &ExecutedResponse) -> Result<Response, GatewayError> {
    let UpstreamExecutionOutput::Events(events) = executed.response.output() else {
        return Err(GatewayError::internal("unexpected upstream output"));
    };
    let value = ChatCodec::encode_response(
        ChatEncodeContext {
            model: executed.public_model.clone(),
            created: executed.response.created_at(),
            response: ChatResponseTemplate {
                choice_index: 0,
                finish_reason: finish_reason(executed.response.finish_reason()),
                logprobs: None,
                include_usage: true,
                extra: BTreeMap::new(),
            },
        },
        events,
    )?;
    bounded_json_response(&value, executed.response.upstream_request_id())
}

pub(crate) fn stream_codec(executed: &ExecutedStream) -> ChatCodec {
    ChatCodec::new(ChatEncodeContext {
        model: executed.public_model.clone(),
        created: executed.stream.upstream().created_at(),
        response: ChatResponseTemplate {
            choice_index: 0,
            finish_reason: finish_reason(executed.stream.upstream().finish_reason()),
            logprobs: None,
            include_usage: true,
            extra: BTreeMap::new(),
        },
    })
}

fn finish_reason(reason: UpstreamFinishReason) -> ChatFinishReason {
    match reason {
        UpstreamFinishReason::Stop
        | UpstreamFinishReason::PauseTurn
        | UpstreamFinishReason::Refusal => ChatFinishReason::Stop,
        UpstreamFinishReason::Length | UpstreamFinishReason::ModelContextWindowExceeded => {
            ChatFinishReason::Length
        }
        UpstreamFinishReason::ToolCalls => ChatFinishReason::ToolCalls,
        UpstreamFinishReason::ContentFilter => ChatFinishReason::ContentFilter,
        UpstreamFinishReason::FunctionCall => ChatFinishReason::FunctionCall,
    }
}
