use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

const REDACTED: &str = "[redacted]";

#[derive(Clone)]
pub struct Redacted<T>(T);

impl<T> Redacted<T> {
    fn new(value: T) -> Self {
        Self(value)
    }
}

impl<T> fmt::Debug for Redacted<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

impl<T> fmt::Display for Redacted<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

impl<T> Serialize for Redacted<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(REDACTED)
    }
}

impl<'de> Deserialize<'de> for Redacted<String> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryClass {
    Never,
    RefreshCredentials,
    AfterDelay,
    BeforeFirstEvent,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ErrorKind {
    InvalidRequest,
    ModelNotFound,
    UnsupportedCapability,
    NoExecutor,
    UpstreamAuth,
    RateLimited { retry_after_seconds: Option<u64> },
    UpstreamError { status: u16 },
    UpstreamUnavailable,
    InternalError,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct GatewayError {
    #[serde(flatten)]
    kind: ErrorKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    diagnostic: Option<Redacted<String>>,
}

impl GatewayError {
    pub fn invalid_request() -> Self {
        Self::new(ErrorKind::InvalidRequest)
    }

    pub fn unknown_model() -> Self {
        Self::new(ErrorKind::ModelNotFound)
    }

    pub fn unsupported_capability() -> Self {
        Self::new(ErrorKind::UnsupportedCapability)
    }

    pub fn no_executor() -> Self {
        Self::new(ErrorKind::NoExecutor)
    }

    pub fn upstream_auth(diagnostic: impl Into<String>) -> Self {
        Self::with_diagnostic(ErrorKind::UpstreamAuth, diagnostic)
    }

    pub fn rate_limited(retry_after_seconds: Option<u64>) -> Self {
        Self::new(ErrorKind::RateLimited {
            retry_after_seconds,
        })
    }

    pub fn upstream_5xx(status: u16) -> Self {
        Self::new(ErrorKind::UpstreamError { status })
    }

    pub fn upstream_response(status: u16, snippet: impl Into<String>) -> Self {
        Self::with_diagnostic(ErrorKind::UpstreamError { status }, snippet)
    }

    pub fn transport(diagnostic: impl Into<String>) -> Self {
        Self::with_diagnostic(ErrorKind::UpstreamUnavailable, diagnostic)
    }

    pub fn internal(diagnostic: impl Into<String>) -> Self {
        Self::with_diagnostic(ErrorKind::InternalError, diagnostic)
    }

    pub fn code(&self) -> &'static str {
        match self.kind {
            ErrorKind::InvalidRequest => "invalid_request",
            ErrorKind::ModelNotFound => "model_not_found",
            ErrorKind::UnsupportedCapability => "unsupported_capability",
            ErrorKind::NoExecutor => "no_executor",
            ErrorKind::UpstreamAuth => "upstream_auth",
            ErrorKind::RateLimited { .. } => "rate_limited",
            ErrorKind::UpstreamError { .. } => "upstream_error",
            ErrorKind::UpstreamUnavailable => "upstream_unavailable",
            ErrorKind::InternalError => "internal_error",
        }
    }

    pub fn http_status(&self) -> u16 {
        match self.kind {
            ErrorKind::InvalidRequest => 400,
            ErrorKind::ModelNotFound => 404,
            ErrorKind::UnsupportedCapability => 422,
            ErrorKind::NoExecutor => 501,
            ErrorKind::RateLimited { .. } => 429,
            ErrorKind::InternalError => 500,
            ErrorKind::UpstreamAuth
            | ErrorKind::UpstreamError { .. }
            | ErrorKind::UpstreamUnavailable => 502,
        }
    }

    pub fn retry_class(&self) -> RetryClass {
        match self.kind {
            ErrorKind::UpstreamAuth => RetryClass::RefreshCredentials,
            ErrorKind::RateLimited { .. } => RetryClass::AfterDelay,
            ErrorKind::UpstreamError { .. } | ErrorKind::UpstreamUnavailable => {
                RetryClass::BeforeFirstEvent
            }
            ErrorKind::InvalidRequest
            | ErrorKind::ModelNotFound
            | ErrorKind::UnsupportedCapability
            | ErrorKind::NoExecutor
            | ErrorKind::InternalError => RetryClass::Never,
        }
    }

    pub fn public_message(&self) -> &'static str {
        match self.kind {
            ErrorKind::InvalidRequest => "The request is invalid.",
            ErrorKind::ModelNotFound => "The requested model is not available.",
            ErrorKind::UnsupportedCapability => "The requested capability is not supported.",
            ErrorKind::NoExecutor => "No executor is configured for the requested operation.",
            ErrorKind::UpstreamAuth => "The upstream account needs to be authenticated again.",
            ErrorKind::RateLimited { .. } => "The request was rate limited.",
            ErrorKind::UpstreamError { .. } => "The upstream service failed.",
            ErrorKind::UpstreamUnavailable => "The upstream service is unavailable.",
            ErrorKind::InternalError => "An internal gateway error occurred.",
        }
    }

    fn new(kind: ErrorKind) -> Self {
        Self {
            kind,
            diagnostic: None,
        }
    }

    fn with_diagnostic(kind: ErrorKind, diagnostic: impl Into<String>) -> Self {
        Self {
            kind,
            diagnostic: Some(Redacted::new(diagnostic.into())),
        }
    }
}

impl fmt::Debug for GatewayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayError")
            .field("kind", &self.kind)
            .field("diagnostic", &self.diagnostic)
            .finish()
    }
}

impl fmt::Display for GatewayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.public_message())
    }
}

impl std::error::Error for GatewayError {}

impl PartialEq for GatewayError {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.diagnostic.is_some() == other.diagnostic.is_some()
    }
}
