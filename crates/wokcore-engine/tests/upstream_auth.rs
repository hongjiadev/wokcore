use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use wokcore_core::{config::AccountAuthConfig, secret::SecretRef};
use wokcore_engine::{
    auth::{SecretResolutionError, SecretResolver, resolve_outbound_auth},
    catalog::AdapterFamily,
};

const SECRET_CANARY: &str = "auth-secret-canary";

#[derive(Default)]
struct RecordingResolver {
    values: BTreeMap<String, String>,
    reads: Arc<Mutex<Vec<String>>>,
}

impl RecordingResolver {
    fn with(secret_ref: &SecretRef, value: &str) -> Self {
        Self {
            values: BTreeMap::from([(secret_ref.as_str().to_owned(), value.to_owned())]),
            reads: Arc::default(),
        }
    }

    fn reads(&self) -> Vec<String> {
        self.reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl SecretResolver for RecordingResolver {
    async fn resolve(&self, secret_ref: &SecretRef) -> Result<SecretString, SecretResolutionError> {
        self.reads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(secret_ref.as_str().to_owned());
        self.values
            .get(secret_ref.as_str())
            .cloned()
            .map(SecretString::from)
            .ok_or(SecretResolutionError)
    }
}

#[tokio::test]
async fn outbound_auth_resolves_only_the_selected_secret_at_the_last_step() {
    let access = SecretRef::new();
    let refresh = SecretRef::new();
    let resolver = RecordingResolver::with(&access, SECRET_CANARY);
    let auth = AccountAuthConfig::Oauth {
        access: access.clone(),
        refresh: Some(refresh),
    };

    assert!(resolver.reads().is_empty());
    let resolved = resolve_outbound_auth(&auth, AdapterFamily::OpenAiResponses, &resolver)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(resolver.reads(), vec![access.as_str()]);
    assert_eq!(resolved.header_name(), "authorization");
    assert_eq!(
        resolved.expose_header_value_for_request().expose_secret(),
        format!("Bearer {SECRET_CANARY}")
    );
    let rendered = format!("{resolved:?}");
    assert!(!rendered.contains(SECRET_CANARY));
    assert!(rendered.contains("[redacted]"));
}

#[tokio::test]
async fn outbound_auth_maps_account_kind_and_adapter_to_one_header() {
    let secret_ref = SecretRef::new();
    let cases = [
        (
            AccountAuthConfig::Forward {
                credential: secret_ref.clone(),
            },
            AdapterFamily::OpenAiResponses,
            "authorization",
            SECRET_CANARY.to_owned(),
        ),
        (
            AccountAuthConfig::Oauth {
                access: secret_ref.clone(),
                refresh: None,
            },
            AdapterFamily::Anthropic,
            "authorization",
            format!("Bearer {SECRET_CANARY}"),
        ),
        (
            AccountAuthConfig::ApiKey {
                secret: secret_ref.clone(),
            },
            AdapterFamily::OpenAiChat,
            "authorization",
            format!("Bearer {SECRET_CANARY}"),
        ),
        (
            AccountAuthConfig::ApiKey {
                secret: secret_ref.clone(),
            },
            AdapterFamily::Anthropic,
            "x-api-key",
            SECRET_CANARY.to_owned(),
        ),
        (
            AccountAuthConfig::ApiKey {
                secret: secret_ref.clone(),
            },
            AdapterFamily::Google,
            "x-goog-api-key",
            SECRET_CANARY.to_owned(),
        ),
        (
            AccountAuthConfig::ApiKey {
                secret: secret_ref.clone(),
            },
            AdapterFamily::AzureOpenAi,
            "api-key",
            SECRET_CANARY.to_owned(),
        ),
    ];

    for (auth, adapter, expected_name, expected_value) in cases {
        let resolver = RecordingResolver::with(&secret_ref, SECRET_CANARY);
        let resolved = resolve_outbound_auth(&auth, adapter, &resolver)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(resolved.header_name(), expected_name);
        assert_eq!(
            resolved.expose_header_value_for_request().expose_secret(),
            expected_value
        );
        assert_eq!(resolver.reads(), vec![secret_ref.as_str()]);
    }
}

#[tokio::test]
async fn outbound_auth_local_mode_reads_nothing_and_errors_are_opaque() {
    let resolver = RecordingResolver::default();
    assert!(
        resolve_outbound_auth(
            &AccountAuthConfig::Local,
            AdapterFamily::OpenAiChat,
            &resolver,
        )
        .await
        .unwrap()
        .is_none()
    );
    assert!(resolver.reads().is_empty());

    let missing = SecretRef::new();
    let error = resolve_outbound_auth(
        &AccountAuthConfig::ApiKey {
            secret: missing.clone(),
        },
        AdapterFamily::OpenAiResponses,
        &resolver,
    )
    .await
    .unwrap_err();
    let rendered = format!("{error:?}");
    assert!(!rendered.contains(missing.as_str()));
    assert!(!rendered.contains(SECRET_CANARY));
}
