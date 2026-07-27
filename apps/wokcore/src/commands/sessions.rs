use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::{
    CommandOutput, ExitCode, RunDependencies,
    cli::{SessionList, SessionShow, Sessions, SessionsCommand},
};

use super::{
    client::{ControlClient, ControlClientError},
    response::read_bounded_with_limit,
    terminal::{escape_message_body, escape_single_line},
    write_json,
};

const SESSION_LIST_MAX_BYTES: usize = 512 * 1024;
const SESSION_MESSAGES_MAX_BYTES: usize = 1024 * 1024;

pub(super) async fn run(
    options: Sessions,
    dependencies: &RunDependencies,
    output: &mut dyn CommandOutput,
) -> ExitCode {
    match options.command {
        SessionsCommand::List(options) => list(options, dependencies, output).await,
        SessionsCommand::Show(options) => show(options, dependencies, output).await,
    }
}

async fn list(
    options: SessionList,
    dependencies: &RunDependencies,
    output: &mut dyn CommandOutput,
) -> ExitCode {
    let path = list_path(&options);
    let result = get_json(dependencies, &path, SESSION_LIST_MAX_BYTES).await;
    match result {
        Ok(value) if options.json => render_json(output, &value),
        Ok(value) => render_list(output, &value),
        Err(error) => render_error(error, output, options.json),
    }
}

fn list_path(options: &SessionList) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    if let Some(source) = options.source.as_deref() {
        query.append_pair("source", source);
    }
    if let Some(limit) = options.limit {
        query.append_pair("limit", &limit.to_string());
    }
    let query = query.finish();
    if query.is_empty() {
        "/wokcore/v1/sessions".to_owned()
    } else {
        format!("/wokcore/v1/sessions?{query}")
    }
}

async fn show(
    options: SessionShow,
    dependencies: &RunDependencies,
    output: &mut dyn CommandOutput,
) -> ExitCode {
    if !valid_session_key(&options.session_key) {
        return render_error(ControlClientError::InvalidRuntime, output, options.json);
    }
    let path = show_path(&options);
    let result = get_json(dependencies, &path, SESSION_MESSAGES_MAX_BYTES).await;
    match result {
        Ok(value) if options.json => render_json(output, &value),
        Ok(value) => render_messages(output, &value),
        Err(error) => render_error(error, output, options.json),
    }
}

fn show_path(options: &SessionShow) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    if let Some(cursor) = options.cursor.as_deref() {
        query.append_pair("after", cursor);
    }
    if let Some(limit) = options.limit {
        query.append_pair("limit", &limit.to_string());
    }
    let query = query.finish();
    let mut path = format!("/wokcore/v1/sessions/{}/messages", options.session_key);
    if !query.is_empty() {
        path.push('?');
        path.push_str(&query);
    }
    path
}

async fn get_json(
    dependencies: &RunDependencies,
    path: &str,
    maximum_bytes: usize,
) -> Result<Value, ControlClientError> {
    let client = ControlClient::connect(dependencies).await?;
    let management = client.management_secret(dependencies).await?;
    let mut response = client.get(path, &management).await?;
    let status = response.status();
    let body = read_bounded_with_limit(&mut response, maximum_bytes)
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

fn render_json(output: &mut dyn CommandOutput, value: &Value) -> ExitCode {
    if write_json(output, value).is_ok() {
        ExitCode::Success
    } else {
        ExitCode::InternalFailure
    }
}

fn render_list(output: &mut dyn CommandOutput, value: &Value) -> ExitCode {
    let Some(items) = value.get("items").and_then(Value::as_array) else {
        return ExitCode::InternalFailure;
    };
    for item in items {
        let line = format!(
            "{}  {}  {}  {}\n",
            safe_field(item, "session_key"),
            safe_field(item, "source"),
            safe_field(item, "last_active_at"),
            safe_field(item, "availability"),
        );
        if output.write_stdout(&line).is_err() {
            return ExitCode::InternalFailure;
        }
        if let Some(title) = item.get("title").and_then(Value::as_str)
            && output
                .write_stdout(&format!("  {}\n", escape_single_line(title)))
                .is_err()
        {
            return ExitCode::InternalFailure;
        }
    }
    ExitCode::Success
}

fn render_messages(output: &mut dyn CommandOutput, value: &Value) -> ExitCode {
    let Some(items) = value.get("items").and_then(Value::as_array) else {
        return ExitCode::InternalFailure;
    };
    for item in items {
        let header = format!(
            "--- {} {} ---\n",
            safe_field(item, "role"),
            safe_field(item, "timestamp")
        );
        let Some(content) = item.get("content").and_then(Value::as_str) else {
            return ExitCode::InternalFailure;
        };
        if output.write_stdout(&header).is_err()
            || output
                .write_stdout(&format!("{}\n", escape_message_body(content)))
                .is_err()
            || output.write_stdout("--- end message ---\n").is_err()
        {
            return ExitCode::InternalFailure;
        }
    }
    ExitCode::Success
}

fn safe_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(escape_single_line)
        .unwrap_or_else(|| "-".to_owned())
}

fn valid_session_key(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
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
        ControlClientError::InvalidInput => (
            ExitCode::InvalidInput,
            "invalid_input",
            "WokCore Session query input is invalid.\n",
        ),
        ControlClientError::Internal => (
            ExitCode::InternalFailure,
            "internal_error",
            "WokCore Session query failed.\n",
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

#[cfg(test)]
mod tests {
    use super::valid_session_key;

    #[test]
    fn session_key_validation_prevents_path_injection() {
        assert!(valid_session_key(&"a".repeat(64)));
        assert!(!valid_session_key("../messages"));
        assert!(!valid_session_key(&"A".repeat(64)));
    }
}
