use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::{CommandOutput, ExitCode, RunDependencies, cli::Logs};

use super::{
    client::{ControlClient, ControlClientError},
    response::read_bounded_with_limit,
    terminal::escape_single_line,
    write_json,
};

const LOG_RESPONSE_MAX_BYTES: usize = 1024 * 1024;

pub(super) async fn run(
    options: Logs,
    dependencies: &RunDependencies,
    output: &mut dyn CommandOutput,
) -> ExitCode {
    let path = logs_path(&options);
    match get_logs(dependencies, &path).await {
        Ok(value) if options.jsonl => render_jsonl(output, &value),
        Ok(value) => render_text(output, &value),
        Err(error) => render_error(error, output, options.jsonl),
    }
}

fn logs_path(options: &Logs) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    if let Some(request_id) = options.request_id.as_deref() {
        query.append_pair("request_id", request_id);
    }
    if let Some(level) = options.level.as_deref() {
        query.append_pair("level_min", level);
    }
    if let Some(component) = options.component.as_deref() {
        query.append_pair("component", component);
    }
    if let Some(since) = options.since.as_deref() {
        query.append_pair("since", since);
    }
    let query = query.finish();
    if query.is_empty() {
        "/wokcore/v1/logs".to_owned()
    } else {
        format!("/wokcore/v1/logs?{query}")
    }
}

async fn get_logs(dependencies: &RunDependencies, path: &str) -> Result<Value, ControlClientError> {
    let client = ControlClient::connect(dependencies).await?;
    let management = client.management_secret(dependencies).await?;
    let mut response = client.get(path, &management).await?;
    let status = response.status();
    let body = read_bounded_with_limit(&mut response, LOG_RESPONSE_MAX_BYTES)
        .await
        .map_err(|_| ControlClientError::Internal)?;
    if status == StatusCode::UNAUTHORIZED {
        return Err(ControlClientError::Authentication);
    }
    if !status.is_success() {
        return Err(ControlClientError::Internal);
    }
    serde_json::from_slice(&body).map_err(|_| ControlClientError::Internal)
}

fn render_jsonl(output: &mut dyn CommandOutput, value: &Value) -> ExitCode {
    let Some(items) = value.get("items").and_then(Value::as_array) else {
        return ExitCode::InternalFailure;
    };
    for item in items {
        let Ok(mut line) = serde_json::to_string(item) else {
            return ExitCode::InternalFailure;
        };
        line.push('\n');
        if output.write_stdout(&line).is_err() {
            return ExitCode::InternalFailure;
        }
    }
    ExitCode::Success
}

fn render_text(output: &mut dyn CommandOutput, value: &Value) -> ExitCode {
    let Some(items) = value.get("items").and_then(Value::as_array) else {
        return ExitCode::InternalFailure;
    };
    for item in items {
        let correlations = item.get("correlations");
        let request_id = correlations
            .and_then(|value| value.get("request_id"))
            .and_then(Value::as_str)
            .unwrap_or("-");
        let line = format!(
            "{}  {}  {}  {}  request={}\n",
            field(item, "occurred_at"),
            field(item, "level"),
            field(item, "component"),
            field(item, "code"),
            escape_single_line(request_id),
        );
        if output.write_stdout(&line).is_err() {
            return ExitCode::InternalFailure;
        }
    }
    ExitCode::Success
}

fn field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(escape_single_line)
        .unwrap_or_else(|| "-".to_owned())
}

fn render_error(
    error: ControlClientError,
    output: &mut dyn CommandOutput,
    json_output: bool,
) -> ExitCode {
    let (exit, code, human) = match error {
        ControlClientError::NotRunning => (
            ExitCode::NotRunning,
            "not_running",
            "WokCore is not running.\n",
        ),
        ControlClientError::InvalidRuntime | ControlClientError::IdentityMismatch => (
            ExitCode::InvalidInput,
            "invalid_runtime",
            "WokCore runtime metadata is invalid.\n",
        ),
        ControlClientError::Authentication => (
            ExitCode::AuthenticationFailure,
            "authentication_failure",
            "WokCore management authentication failed.\n",
        ),
        ControlClientError::StorageCorruption => (
            ExitCode::StorageCorruption,
            "storage_corrupt",
            "WokCore storage is corrupt.\n",
        ),
        ControlClientError::Internal => (
            ExitCode::InternalFailure,
            "internal_error",
            "WokCore diagnostic query failed.\n",
        ),
    };
    let rendered = if json_output {
        write_json(output, &json!({"code": code}))
    } else {
        output.write_stderr(human)
    };
    if rendered.is_ok() {
        exit
    } else {
        ExitCode::InternalFailure
    }
}
