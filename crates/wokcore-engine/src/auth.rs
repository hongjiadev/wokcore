use std::fmt;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use wokcore_core::{config::AccountAuthConfig, secret::SecretRef};

use crate::catalog::AdapterFamily;

#[async_trait]
pub trait SecretResolver: Send + Sync {
    async fn resolve(&self, secret_ref: &SecretRef) -> Result<SecretString, SecretResolutionError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("the requested Provider credential is unavailable")]
pub struct SecretResolutionError;

pub struct ResolvedAuthorization {
    header_name: &'static str,
    header_value: SecretString,
}

impl ResolvedAuthorization {
    pub const fn header_name(&self) -> &'static str {
        self.header_name
    }

    pub fn expose_header_value_for_request(&self) -> &SecretString {
        &self.header_value
    }

    pub fn into_parts(self) -> (&'static str, SecretString) {
        (self.header_name, self.header_value)
    }
}

impl fmt::Debug for ResolvedAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedAuthorization")
            .field("header_name", &self.header_name)
            .field("header_value", &"[redacted]")
            .finish()
    }
}

pub async fn resolve_outbound_auth(
    auth: &AccountAuthConfig,
    adapter: AdapterFamily,
    resolver: &dyn SecretResolver,
) -> Result<Option<ResolvedAuthorization>, SecretResolutionError> {
    let (secret_ref, header_name, bearer) = match auth {
        AccountAuthConfig::Forward { credential } => (credential, "authorization", false),
        AccountAuthConfig::Oauth { access, .. } => (access, "authorization", true),
        AccountAuthConfig::ApiKey { secret } => {
            let (header_name, bearer) = api_key_header(adapter);
            (secret, header_name, bearer)
        }
        AccountAuthConfig::Local => return Ok(None),
    };
    let secret = resolver.resolve(secret_ref).await?;
    if secret.expose_secret().is_empty() {
        return Err(SecretResolutionError);
    }
    let header_value = if bearer {
        SecretString::from(format!("Bearer {}", secret.expose_secret()))
    } else {
        secret
    };
    Ok(Some(ResolvedAuthorization {
        header_name,
        header_value,
    }))
}

const fn api_key_header(adapter: AdapterFamily) -> (&'static str, bool) {
    match adapter {
        AdapterFamily::Anthropic => ("x-api-key", false),
        AdapterFamily::Google => ("x-goog-api-key", false),
        AdapterFamily::AzureOpenAi => ("api-key", false),
        AdapterFamily::OpenAiResponses
        | AdapterFamily::OpenAiChat
        | AdapterFamily::Cursor
        | AdapterFamily::Kiro
        | AdapterFamily::MimoFree => ("authorization", true),
    }
}
