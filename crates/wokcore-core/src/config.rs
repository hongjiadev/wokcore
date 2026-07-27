use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
    id::{AccountId, ClientId, ProviderId},
    secret::SecretRef,
};

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub instances: Vec<ProviderInstanceConfig>,
    pub accounts: Vec<AccountConfig>,
}

const MAX_PROVIDER_INSTANCES: usize = 64;
const MAX_ACCOUNTS: usize = 256;
const MAX_MODEL_ALIASES: usize = 1_024;
const MAX_ROUTE_RULES: usize = 1_024;
const MAX_MODEL_ID_BYTES: usize = 256;
const MAX_ENDPOINT_BYTES: usize = 2_048;

#[derive(Clone, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderInstanceConfig {
    pub id: ProviderId,
    pub catalog_id: ProviderId,
    pub enabled: bool,
    pub endpoint: Option<String>,
    pub allow_private_network: bool,
}

impl fmt::Debug for ProviderInstanceConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderInstanceConfig")
            .field("id", &self.id)
            .field("catalog_id", &self.catalog_id)
            .field("enabled", &self.enabled)
            .field("endpoint_present", &self.endpoint.is_some())
            .field("allow_private_network", &self.allow_private_network)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    pub id: AccountId,
    pub provider: ProviderId,
    pub enabled: bool,
    pub auth: AccountAuthConfig,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AccountAuthConfig {
    Forward {
        credential: SecretRef,
    },
    Oauth {
        access: SecretRef,
        refresh: Option<SecretRef>,
    },
    ApiKey {
        secret: SecretRef,
    },
    Local,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingConfig {
    pub aliases: Vec<ModelAlias>,
    pub rules: Vec<RouteRule>,
    pub default: Option<RouteTarget>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelAlias {
    pub alias: String,
    pub target: RouteTarget,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RouteRule {
    pub client_id: Option<ClientId>,
    pub model: Option<String>,
    pub target: RouteTarget,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RouteTarget {
    pub provider: ProviderId,
    pub model: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ConfigShapeError {
    #[error("the Provider instance count exceeds its limit")]
    ProviderLimitExceeded,
    #[error("the account count exceeds its limit")]
    AccountLimitExceeded,
    #[error("the model alias count exceeds its limit")]
    AliasLimitExceeded,
    #[error("the route rule count exceeds its limit")]
    RouteRuleLimitExceeded,
    #[error("a Provider instance identifier is duplicated")]
    DuplicateProvider,
    #[error("an account identifier is duplicated")]
    DuplicateAccount,
    #[error("an account references an unknown Provider instance")]
    UnknownAccountProvider,
    #[error("a configured endpoint has an unsafe shape")]
    InvalidEndpoint,
    #[error("a model alias is duplicated")]
    DuplicateAlias,
    #[error("a model identifier is invalid")]
    InvalidModel,
    #[error("a route references an unknown Provider instance")]
    UnknownRouteProvider,
}

pub fn validate_provider_configuration_shape(
    providers: &ProviderConfig,
    routing: &RoutingConfig,
) -> Result<(), ConfigShapeError> {
    if providers.instances.len() > MAX_PROVIDER_INSTANCES {
        return Err(ConfigShapeError::ProviderLimitExceeded);
    }
    if providers.accounts.len() > MAX_ACCOUNTS {
        return Err(ConfigShapeError::AccountLimitExceeded);
    }
    if routing.aliases.len() > MAX_MODEL_ALIASES {
        return Err(ConfigShapeError::AliasLimitExceeded);
    }
    if routing.rules.len() > MAX_ROUTE_RULES {
        return Err(ConfigShapeError::RouteRuleLimitExceeded);
    }

    let mut provider_ids = BTreeSet::new();
    for provider in &providers.instances {
        if !provider_ids.insert(&provider.id) {
            return Err(ConfigShapeError::DuplicateProvider);
        }
        if let Some(endpoint) = &provider.endpoint {
            validate_endpoint_shape(endpoint)?;
        }
    }

    let mut account_ids = BTreeSet::new();
    for account in &providers.accounts {
        if !account_ids.insert(&account.id) {
            return Err(ConfigShapeError::DuplicateAccount);
        }
        if !provider_ids.contains(&account.provider) {
            return Err(ConfigShapeError::UnknownAccountProvider);
        }
    }

    let mut aliases = BTreeSet::new();
    for alias in &routing.aliases {
        validate_model(&alias.alias)?;
        if !aliases.insert(alias.alias.as_str()) {
            return Err(ConfigShapeError::DuplicateAlias);
        }
        validate_target(&alias.target, &provider_ids)?;
    }
    for rule in &routing.rules {
        if let Some(model) = &rule.model {
            validate_model(model)?;
        }
        validate_target(&rule.target, &provider_ids)?;
    }
    if let Some(target) = &routing.default {
        validate_target(target, &provider_ids)?;
    }

    Ok(())
}

fn validate_endpoint_shape(endpoint: &str) -> Result<(), ConfigShapeError> {
    if endpoint.is_empty()
        || endpoint.len() > MAX_ENDPOINT_BYTES
        || endpoint.contains('{')
        || endpoint.contains('}')
    {
        return Err(ConfigShapeError::InvalidEndpoint);
    }
    let url = Url::parse(endpoint).map_err(|_| ConfigShapeError::InvalidEndpoint)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host().is_none()
    {
        return Err(ConfigShapeError::InvalidEndpoint);
    }
    Ok(())
}

fn validate_target(
    target: &RouteTarget,
    provider_ids: &BTreeSet<&ProviderId>,
) -> Result<(), ConfigShapeError> {
    if !provider_ids.contains(&target.provider) {
        return Err(ConfigShapeError::UnknownRouteProvider);
    }
    validate_model(&target.model)
}

fn validate_model(model: &str) -> Result<(), ConfigShapeError> {
    if model.trim().is_empty()
        || model.len() > MAX_MODEL_ID_BYTES
        || model.chars().any(char::is_control)
    {
        return Err(ConfigShapeError::InvalidModel);
    }
    Ok(())
}
