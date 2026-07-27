use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use arc_swap::ArcSwap;
use wokcore_core::{
    config::{ProviderConfig, RouteRule, RouteTarget, RoutingConfig},
    id::{ClientId, ProviderId},
};

use crate::{
    catalog::{AuthKind, ProviderCatalog, ProviderDefinition},
    config::{ProviderConfigError, validate_provider_configuration},
    models::PublicModelMetadata,
    routing::{RouteAccount, RouteDecision, RouteError, RouteOrigin, RouteProvider, RouteRequest},
};

#[derive(Clone)]
struct CompiledRule {
    client_id: Option<ClientId>,
    model: Option<String>,
    target: RouteTarget,
}

impl CompiledRule {
    fn from_config(rule: &RouteRule) -> Self {
        Self {
            client_id: rule.client_id.clone(),
            model: rule.model.clone(),
            target: rule.target.clone(),
        }
    }

    fn specificity(&self) -> u8 {
        u8::from(self.client_id.is_some()) + u8::from(self.model.is_some())
    }

    fn matches(&self, request: &RouteRequest) -> bool {
        self.client_id
            .as_ref()
            .is_none_or(|client_id| request.client_id.as_ref() == Some(client_id))
            && self
                .model
                .as_ref()
                .is_none_or(|model| model == &request.model)
    }
}

#[derive(Clone)]
pub struct RuntimeSnapshot {
    providers: BTreeMap<ProviderId, Arc<RouteProvider>>,
    aliases: BTreeMap<String, RouteTarget>,
    rules: Vec<CompiledRule>,
    default: Option<RouteTarget>,
    public_models: Vec<PublicModelMetadata>,
}

impl RuntimeSnapshot {
    pub fn build(
        catalog: &ProviderCatalog,
        providers: &ProviderConfig,
        routing: &RoutingConfig,
    ) -> Result<Self, SnapshotError> {
        SnapshotBuilder::new(catalog, providers, routing).build()
    }

    fn build_validated(
        catalog: &ProviderCatalog,
        providers: &ProviderConfig,
        routing: &RoutingConfig,
    ) -> Result<Self, SnapshotError> {
        validate_provider_configuration(catalog, providers, routing)
            .map_err(SnapshotError::ProviderConfig)?;

        let runtime_providers = build_runtime_providers(catalog, providers)?;
        let aliases = routing
            .aliases
            .iter()
            .map(|alias| (alias.alias.clone(), alias.target.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut rules = routing
            .rules
            .iter()
            .map(CompiledRule::from_config)
            .collect::<Vec<_>>();
        rules.sort_by_key(|rule| Reverse(rule.specificity()));
        let public_models = build_public_models(catalog, &runtime_providers, routing);

        Ok(Self {
            providers: runtime_providers,
            aliases,
            rules,
            default: routing.default.clone(),
            public_models,
        })
    }

    pub fn route(&self, request: &RouteRequest) -> Result<RouteDecision, RouteError> {
        if let Some(provider) = &request.provider {
            return self.decision(
                &RouteTarget {
                    provider: provider.clone(),
                    model: request.model.clone(),
                },
                RouteOrigin::Explicit,
            );
        }
        if let Some(target) = self.aliases.get(&request.model) {
            return self.decision(target, RouteOrigin::Alias);
        }
        if let Some(rule) = self.rules.iter().find(|rule| rule.matches(request)) {
            return self.decision(&rule.target, RouteOrigin::Rule);
        }
        if let Some(target) = &self.default {
            return self.decision(target, RouteOrigin::Default);
        }
        Err(RouteError::NoRoute)
    }

    pub fn public_models(&self) -> &[PublicModelMetadata] {
        &self.public_models
    }

    fn decision(
        &self,
        target: &RouteTarget,
        origin: RouteOrigin,
    ) -> Result<RouteDecision, RouteError> {
        let provider = self
            .providers
            .get(&target.provider)
            .cloned()
            .ok_or(RouteError::ProviderUnavailable)?;
        Ok(RouteDecision::new(provider, target.model.clone(), origin))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SnapshotBuilder<'a> {
    catalog: &'a ProviderCatalog,
    providers: &'a ProviderConfig,
    routing: &'a RoutingConfig,
}

impl<'a> SnapshotBuilder<'a> {
    pub const fn new(
        catalog: &'a ProviderCatalog,
        providers: &'a ProviderConfig,
        routing: &'a RoutingConfig,
    ) -> Self {
        Self {
            catalog,
            providers,
            routing,
        }
    }

    pub fn build(self) -> Result<RuntimeSnapshot, SnapshotError> {
        RuntimeSnapshot::build_validated(self.catalog, self.providers, self.routing)
    }
}

impl fmt::Debug for RuntimeSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeSnapshot")
            .field("provider_count", &self.providers.len())
            .field("alias_count", &self.aliases.len())
            .field("rule_count", &self.rules.len())
            .field("has_default", &self.default.is_some())
            .field("public_model_count", &self.public_models.len())
            .finish()
    }
}

pub struct SnapshotPublisher {
    current: ArcSwap<RuntimeSnapshot>,
}

impl SnapshotPublisher {
    pub fn new(initial: RuntimeSnapshot) -> Self {
        Self {
            current: ArcSwap::from_pointee(initial),
        }
    }

    pub fn load(&self) -> Arc<RuntimeSnapshot> {
        self.current.load_full()
    }

    pub fn rebuild_and_publish(
        &self,
        catalog: &ProviderCatalog,
        providers: &ProviderConfig,
        routing: &RoutingConfig,
    ) -> Result<Arc<RuntimeSnapshot>, SnapshotError> {
        let snapshot = Arc::new(RuntimeSnapshot::build(catalog, providers, routing)?);
        self.current.store(Arc::clone(&snapshot));
        Ok(snapshot)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SnapshotError {
    #[error("Provider configuration is invalid")]
    ProviderConfig(ProviderConfigError),
    #[error("an enabled Provider has no enabled account")]
    NoEnabledAccount(ProviderId),
    #[error("an enabled Provider endpoint template is unresolved")]
    UnresolvedEndpointTemplate(ProviderId),
}

fn build_runtime_providers(
    catalog: &ProviderCatalog,
    providers: &ProviderConfig,
) -> Result<BTreeMap<ProviderId, Arc<RouteProvider>>, SnapshotError> {
    let mut runtime = BTreeMap::new();
    for instance in providers
        .instances
        .iter()
        .filter(|instance| instance.enabled)
    {
        let definition = catalog
            .provider(instance.catalog_id.as_str())
            .expect("validated catalog Provider");
        let endpoint = instance
            .endpoint
            .as_ref()
            .unwrap_or(&definition.base_url)
            .clone();
        if endpoint.contains(['{', '}']) {
            return Err(SnapshotError::UnresolvedEndpointTemplate(
                instance.id.clone(),
            ));
        }

        let accounts = providers
            .accounts
            .iter()
            .filter(|account| account.enabled && account.provider == instance.id)
            .map(|account| RouteAccount::new(account.id.clone(), account.auth.clone()))
            .collect::<Vec<_>>();
        if accounts.is_empty() && account_is_required(definition) {
            return Err(SnapshotError::NoEnabledAccount(instance.id.clone()));
        }

        let provider = RouteProvider::new(
            instance.id.clone(),
            instance.catalog_id.clone(),
            endpoint,
            definition.adapter,
            definition.auth_kind,
            definition.capabilities.clone(),
            accounts.into(),
            definition.reasoning_efforts.clone().into(),
            definition.reasoning_effort_map.clone(),
        );
        runtime.insert(instance.id.clone(), Arc::new(provider));
    }
    Ok(runtime)
}

fn account_is_required(provider: &ProviderDefinition) -> bool {
    provider.auth_kind != AuthKind::Local && !provider.key_optional
}

fn build_public_models(
    catalog: &ProviderCatalog,
    providers: &BTreeMap<ProviderId, Arc<RouteProvider>>,
    routing: &RoutingConfig,
) -> Vec<PublicModelMetadata> {
    let mut models = BTreeMap::new();
    for provider in providers.values() {
        let definition = catalog
            .provider(provider.catalog_id().as_str())
            .expect("validated catalog Provider");
        for model in &definition.models {
            models
                .entry(model.clone())
                .or_insert_with(|| public_model(model, provider));
        }
        if let Some(default_model) = &definition.default_model {
            models
                .entry(default_model.clone())
                .or_insert_with(|| public_model(default_model, provider));
        }
    }

    for alias in &routing.aliases {
        if let Some(provider) = providers.get(&alias.target.provider) {
            models.insert(alias.alias.clone(), public_model(&alias.alias, provider));
        }
    }
    for rule in &routing.rules {
        if let (Some(model), Some(provider)) = (&rule.model, providers.get(&rule.target.provider)) {
            models
                .entry(model.clone())
                .or_insert_with(|| public_model(model, provider));
        }
    }
    if let Some(target) = &routing.default
        && let Some(provider) = providers.get(&target.provider)
    {
        models
            .entry(target.model.clone())
            .or_insert_with(|| public_model(&target.model, provider));
    }

    models.into_values().collect()
}

fn public_model(id: &str, provider: &RouteProvider) -> PublicModelMetadata {
    PublicModelMetadata {
        id: id.to_owned(),
        owned_by: provider.id().clone(),
        capabilities: provider.capabilities().clone(),
    }
}
