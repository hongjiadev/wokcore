use axum::response::Response;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use wokcore_protocols::{
    ResponsesCodec, ResponsesEncodeContext, ResponsesResponseTemplate, canonical::GatewayError,
};

use super::{
    ExecutedResponse, UpstreamExecutionOutput, UpstreamFinishReason,
    response::bounded_json_response,
};

pub(crate) fn validate_request(
    request: &wokcore_protocols::canonical::CanonicalRequest,
) -> Result<(), GatewayError> {
    validate_extension(request, "instructions", Value::is_string)?;
    validate_extension(request, "max_output_tokens", |value| {
        value.as_u64().is_some_and(|limit| limit > 0)
    })?;
    validate_extension(request, "metadata", Value::is_object)?;
    validate_extension(request, "parallel_tool_calls", Value::is_boolean)?;
    validate_extension(request, "store", Value::is_boolean)?;
    validate_extension(request, "temperature", |value| {
        value
            .as_f64()
            .is_some_and(|temperature| (0.0..=2.0).contains(&temperature))
    })?;
    validate_extension(request, "text", Value::is_object)?;
    validate_extension(request, "tool_choice", |value| {
        value.is_string() || value.is_object()
    })?;
    validate_extension(request, "top_p", |value| {
        value
            .as_f64()
            .is_some_and(|top_p| (0.0..=1.0).contains(&top_p))
    })?;
    validate_extension(request, "truncation", |value| {
        matches!(value.as_str(), Some("auto" | "disabled"))
    })?;
    validate_extension(request, "user", Value::is_string)
}

pub(crate) fn encode(executed: &ExecutedResponse) -> Result<Response, GatewayError> {
    let UpstreamExecutionOutput::Events(events) = executed.response.output() else {
        return Err(GatewayError::internal("unexpected upstream output"));
    };
    let response = response_template(executed)?;
    let value = ResponsesCodec::encode_response(
        ResponsesEncodeContext {
            model: executed.public_model.clone(),
            created_at: executed.response.created_at(),
            response,
        },
        events,
    )?;
    bounded_json_response(&value, executed.response.upstream_request_id())
}

fn validate_extension(
    request: &wokcore_protocols::canonical::CanonicalRequest,
    key: &str,
    valid: impl FnOnce(&Value) -> bool,
) -> Result<(), GatewayError> {
    if request
        .extensions
        .get(key)
        .is_none_or(|value| value.is_null() || valid(value))
    {
        Ok(())
    } else {
        Err(GatewayError::invalid_request())
    }
}

fn response_template(
    executed: &ExecutedResponse,
) -> Result<ResponsesResponseTemplate, GatewayError> {
    let request = &executed.request;
    let incomplete_details = incomplete_details(executed.response.finish_reason());
    let completed_at = incomplete_details
        .is_none()
        .then_some(executed.response.created_at());
    let reasoning = request.reasoning.as_ref().map_or_else(
        || json!({"effort": null, "summary": null}),
        |reasoning| {
            let mut value = reasoning
                .extensions
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<Map<_, _>>();
            value.insert(
                "effort".to_owned(),
                json!(executed.public_reasoning_effort.as_deref()),
            );
            value.entry("summary".to_owned()).or_insert(Value::Null);
            Value::Object(value)
        },
    );
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            let mut value = tool
                .extensions
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<Map<_, _>>();
            value.insert("type".to_owned(), Value::String("function".to_owned()));
            value.insert("name".to_owned(), Value::String(tool.name.clone()));
            value.insert("description".to_owned(), json!(tool.description));
            value.insert("parameters".to_owned(), tool.input_schema.clone());
            Value::Object(value)
        })
        .collect();

    Ok(ResponsesResponseTemplate {
        completed_at,
        error: None,
        incomplete_details,
        instructions: request.extensions.get("instructions").cloned(),
        max_output_tokens: extension(request, "max_output_tokens")?,
        metadata: extension(request, "metadata")?.unwrap_or_default(),
        parallel_tool_calls: extension(request, "parallel_tool_calls")?.unwrap_or(false),
        previous_response_id: request
            .thread_key
            .as_ref()
            .map(|thread_key| thread_key.as_str().to_owned()),
        reasoning,
        store: extension(request, "store")?.unwrap_or(false),
        temperature: extension(request, "temperature")?,
        text: request
            .extensions
            .get("text")
            .cloned()
            .unwrap_or_else(|| json!({"format":{"type":"text"},"verbosity":"medium"})),
        tool_choice: request
            .extensions
            .get("tool_choice")
            .cloned()
            .unwrap_or_else(|| Value::String("auto".to_owned())),
        tools,
        top_p: extension(request, "top_p")?,
        truncation: request
            .extensions
            .get("truncation")
            .cloned()
            .unwrap_or_else(|| Value::String("disabled".to_owned())),
        user: extension(request, "user")?,
    })
}

fn extension<T>(
    request: &wokcore_protocols::canonical::CanonicalRequest,
    key: &str,
) -> Result<Option<T>, GatewayError>
where
    T: DeserializeOwned,
{
    match request.extensions.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|_| GatewayError::invalid_request()),
    }
}

fn incomplete_details(finish_reason: UpstreamFinishReason) -> Option<Value> {
    match finish_reason {
        UpstreamFinishReason::Length | UpstreamFinishReason::ModelContextWindowExceeded => {
            Some(json!({"reason":"max_output_tokens"}))
        }
        UpstreamFinishReason::ContentFilter => Some(json!({"reason":"content_filter"})),
        UpstreamFinishReason::Stop
        | UpstreamFinishReason::ToolCalls
        | UpstreamFinishReason::FunctionCall
        | UpstreamFinishReason::PauseTurn
        | UpstreamFinishReason::Refusal => None,
    }
}
