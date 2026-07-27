use std::collections::BTreeMap;

use url::Url;
use wokcore_core::{
    config::{
        AccountAuthConfig, ConfigShapeError, ProviderConfig, ProviderInstanceConfig, RoutingConfig,
        validate_provider_configuration_shape,
    },
    id::ProviderId,
};

use crate::catalog::{AuthKind, ProviderCatalog, ProviderDefinition, is_private_host};

const MAX_ENDPOINT_BYTES: usize = 2_048;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValidatedProviderConfig {
    provider_count: usize,
    account_count: usize,
    route_count: usize,
}

impl ValidatedProviderConfig {
    pub const fn provider_count(self) -> usize {
        self.provider_count
    }

    pub const fn account_count(self) -> usize {
        self.account_count
    }

    pub const fn route_count(self) -> usize {
        self.route_count
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ProviderConfigError {
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
    #[error("a Provider catalog identifier is unknown")]
    UnknownCatalogProvider,
    #[error("an account identifier is duplicated")]
    DuplicateAccount,
    #[error("an account references an unknown Provider instance")]
    UnknownAccountProvider,
    #[error("the Provider does not allow endpoint overrides")]
    EndpointOverrideNotAllowed,
    #[error("the endpoint is invalid")]
    InvalidEndpoint,
    #[error("a private endpoint requires explicit opt-in")]
    PrivateNetworkOptInRequired,
    #[error("public endpoints require HTTPS")]
    InsecurePublicEndpoint,
    #[error("the account authentication kind does not match the Provider")]
    AuthenticationMismatch,
    #[error("a model alias is duplicated")]
    DuplicateAlias,
    #[error("a route model identifier is invalid")]
    InvalidModel,
    #[error("a route references an unknown Provider")]
    UnknownRouteProvider,
}

pub fn validate_provider_configuration(
    catalog: &ProviderCatalog,
    providers: &ProviderConfig,
    routing: &RoutingConfig,
) -> Result<ValidatedProviderConfig, ProviderConfigError> {
    validate_provider_configuration_shape(providers, routing).map_err(map_shape_error)?;
    let instances = validate_instances(catalog, &providers.instances)?;
    validate_accounts(providers, &instances)?;

    Ok(ValidatedProviderConfig {
        provider_count: providers.instances.len(),
        account_count: providers.accounts.len(),
        route_count: routing.aliases.len()
            + routing.rules.len()
            + usize::from(routing.default.is_some()),
    })
}

fn validate_instances<'a>(
    catalog: &'a ProviderCatalog,
    instances: &'a [ProviderInstanceConfig],
) -> Result<BTreeMap<&'a ProviderId, &'a ProviderDefinition>, ProviderConfigError> {
    let mut validated = BTreeMap::new();
    for instance in instances {
        let definition = catalog
            .provider(instance.catalog_id.as_str())
            .ok_or(ProviderConfigError::UnknownCatalogProvider)?;
        if let Some(endpoint) = &instance.endpoint {
            if !definition.allow_endpoint_override {
                return Err(ProviderConfigError::EndpointOverrideNotAllowed);
            }
            match classify_endpoint_override(endpoint)? {
                EndpointClass::Public => {}
                EndpointClass::Private if instance.allow_private_network => {}
                EndpointClass::Private => {
                    return Err(ProviderConfigError::PrivateNetworkOptInRequired);
                }
            }
        }
        validated.insert(&instance.id, definition);
    }
    Ok(validated)
}

fn validate_accounts(
    providers: &ProviderConfig,
    instances: &BTreeMap<&ProviderId, &ProviderDefinition>,
) -> Result<(), ProviderConfigError> {
    for account in &providers.accounts {
        let definition = instances
            .get(&account.provider)
            .copied()
            .ok_or(ProviderConfigError::UnknownAccountProvider)?;
        if !authentication_matches(definition, &account.auth) {
            return Err(ProviderConfigError::AuthenticationMismatch);
        }
    }
    Ok(())
}

fn authentication_matches(provider: &ProviderDefinition, account: &AccountAuthConfig) -> bool {
    match (provider.auth_kind, account) {
        (AuthKind::Forward, AccountAuthConfig::Forward { .. })
        | (AuthKind::Oauth, AccountAuthConfig::Oauth { .. })
        | (AuthKind::Key, AccountAuthConfig::ApiKey { .. })
        | (AuthKind::Local, AccountAuthConfig::Local) => true,
        (AuthKind::Oauth, AccountAuthConfig::ApiKey { .. }) => provider.allow_key_auth_override,
        (AuthKind::Key, AccountAuthConfig::Local) => provider.key_optional,
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EndpointClass {
    Public,
    Private,
}

fn classify_endpoint_override(endpoint: &str) -> Result<EndpointClass, ProviderConfigError> {
    if endpoint.is_empty() || endpoint.len() > MAX_ENDPOINT_BYTES || endpoint.contains(['{', '}']) {
        return Err(ProviderConfigError::InvalidEndpoint);
    }
    let url = Url::parse(endpoint).map_err(|_| ProviderConfigError::InvalidEndpoint)?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ProviderConfigError::InvalidEndpoint);
    }
    let host = url.host().ok_or(ProviderConfigError::InvalidEndpoint)?;
    if is_private_host(host) {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ProviderConfigError::InvalidEndpoint);
        }
        return Ok(EndpointClass::Private);
    }
    if url.scheme() != "https" {
        return Err(ProviderConfigError::InsecurePublicEndpoint);
    }
    Ok(EndpointClass::Public)
}

fn map_shape_error(error: ConfigShapeError) -> ProviderConfigError {
    match error {
        ConfigShapeError::ProviderLimitExceeded => ProviderConfigError::ProviderLimitExceeded,
        ConfigShapeError::AccountLimitExceeded => ProviderConfigError::AccountLimitExceeded,
        ConfigShapeError::AliasLimitExceeded => ProviderConfigError::AliasLimitExceeded,
        ConfigShapeError::RouteRuleLimitExceeded => ProviderConfigError::RouteRuleLimitExceeded,
        ConfigShapeError::DuplicateProvider => ProviderConfigError::DuplicateProvider,
        ConfigShapeError::DuplicateAccount => ProviderConfigError::DuplicateAccount,
        ConfigShapeError::UnknownAccountProvider => ProviderConfigError::UnknownAccountProvider,
        ConfigShapeError::InvalidEndpoint => ProviderConfigError::InvalidEndpoint,
        ConfigShapeError::DuplicateAlias => ProviderConfigError::DuplicateAlias,
        ConfigShapeError::InvalidModel => ProviderConfigError::InvalidModel,
        ConfigShapeError::UnknownRouteProvider => ProviderConfigError::UnknownRouteProvider,
    }
}
