use std::sync::Arc;

use wokcore_core::{
    config::{
        AccountAuthConfig, AccountConfig, ModelAlias, ProviderConfig, ProviderInstanceConfig,
        RouteRule, RouteTarget, RoutingConfig,
    },
    id::{AccountId, ClientId, ProviderId},
    secret::SecretRef,
};
use wokcore_engine::{
    catalog::ProviderCatalog,
    routing::{RouteError, RouteOrigin, RouteRequest},
    snapshot::{RuntimeSnapshot, SnapshotBuilder, SnapshotError, SnapshotPublisher},
};

#[test]
fn snapshot_applies_explicit_alias_rule_and_default_precedence() {
    let catalog = ProviderCatalog::bundled().expect("catalog");
    let providers = two_provider_config();
    let routing = RoutingConfig {
        aliases: vec![ModelAlias {
            alias: "fast".to_owned(),
            target: target("backup", "alias-target"),
        }],
        rules: vec![
            RouteRule {
                client_id: None,
                model: Some("code".to_owned()),
                target: target("backup", "general-code"),
            },
            RouteRule {
                client_id: Some(ClientId::new("wokrouter").expect("client")),
                model: Some("code".to_owned()),
                target: target("primary", "client-code"),
            },
        ],
        default: Some(target("primary", "default-model")),
    };
    let snapshot = RuntimeSnapshot::build(&catalog, &providers, &routing).expect("snapshot");

    let explicit = snapshot
        .route(&request(Some("backup"), "caller-model", Some("wokrouter")))
        .expect("explicit route");
    assert_eq!(explicit.provider_id().as_str(), "backup");
    assert_eq!(explicit.model(), "caller-model");
    assert_eq!(explicit.origin(), RouteOrigin::Explicit);

    let alias = snapshot
        .route(&request(None, "fast", Some("wokrouter")))
        .expect("alias route");
    assert_eq!(alias.provider_id().as_str(), "backup");
    assert_eq!(alias.model(), "alias-target");
    assert_eq!(alias.origin(), RouteOrigin::Alias);

    let client_rule = snapshot
        .route(&request(None, "code", Some("wokrouter")))
        .expect("client rule");
    assert_eq!(client_rule.provider_id().as_str(), "primary");
    assert_eq!(client_rule.model(), "client-code");
    assert_eq!(client_rule.origin(), RouteOrigin::Rule);

    let general_rule = snapshot
        .route(&request(None, "code", None))
        .expect("general rule");
    assert_eq!(general_rule.provider_id().as_str(), "backup");
    assert_eq!(general_rule.model(), "general-code");

    let default = snapshot
        .route(&request(None, "unmapped", Some("wokrouter")))
        .expect("default route");
    assert_eq!(default.provider_id().as_str(), "primary");
    assert_eq!(default.model(), "default-model");
    assert_eq!(default.origin(), RouteOrigin::Default);
}

#[test]
fn publisher_preserves_inflight_views_and_failed_reload_keeps_current() {
    let catalog = ProviderCatalog::bundled().expect("catalog");
    let providers = two_provider_config();
    let initial_routing = RoutingConfig {
        aliases: Vec::new(),
        rules: Vec::new(),
        default: Some(target("primary", "old-model")),
    };
    let initial =
        RuntimeSnapshot::build(&catalog, &providers, &initial_routing).expect("initial snapshot");
    let publisher = SnapshotPublisher::new(initial);
    let held = publisher.load();

    let next_routing = RoutingConfig {
        aliases: Vec::new(),
        rules: Vec::new(),
        default: Some(target("primary", "new-model")),
    };
    publisher
        .rebuild_and_publish(&catalog, &providers, &next_routing)
        .expect("publish");

    assert_eq!(
        held.route(&request(None, "anything", None))
            .expect("held route")
            .model(),
        "old-model"
    );
    let current = publisher.load();
    assert_eq!(
        current
            .route(&request(None, "anything", None))
            .expect("current route")
            .model(),
        "new-model"
    );
    assert!(!Arc::ptr_eq(&held, &current));

    let mut invalid = providers.clone();
    invalid.instances[0].endpoint = Some("https://override.example/v1".to_owned());
    let before_failure = publisher.load();
    assert!(
        publisher
            .rebuild_and_publish(&catalog, &invalid, &next_routing)
            .is_err()
    );
    assert!(Arc::ptr_eq(&before_failure, &publisher.load()));
}

#[test]
fn enabled_required_provider_needs_an_enabled_account() {
    let catalog = ProviderCatalog::bundled().expect("catalog");
    let mut providers = two_provider_config();
    providers.accounts[0].enabled = false;
    providers
        .accounts
        .retain(|account| account.provider.as_str() != "backup");
    let routing = RoutingConfig {
        aliases: Vec::new(),
        rules: Vec::new(),
        default: Some(target("primary", "gpt-5.6-sol")),
    };

    let error = RuntimeSnapshot::build(&catalog, &providers, &routing)
        .expect_err("required account must be available");

    assert_eq!(
        error,
        SnapshotError::NoEnabledAccount(ProviderId::new("primary").expect("provider"))
    );
}

#[test]
fn public_models_are_sorted_deduplicated_and_content_free() {
    let catalog = ProviderCatalog::bundled().expect("catalog");
    let providers = two_provider_config();
    let routing = RoutingConfig {
        aliases: vec![ModelAlias {
            alias: "fast".to_owned(),
            target: target("primary", "gpt-5.6-terra"),
        }],
        rules: Vec::new(),
        default: Some(target("primary", "gpt-5.6-sol")),
    };
    let snapshot = RuntimeSnapshot::build(&catalog, &providers, &routing).expect("snapshot");

    let models = snapshot.public_models();
    let ids = models
        .iter()
        .map(|model| model.id.as_str())
        .collect::<Vec<_>>();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(ids, sorted);
    assert!(ids.contains(&"fast"));
    assert!(ids.contains(&"gpt-5.6-sol"));

    let encoded = serde_json::to_string(models).expect("models JSON");
    assert!(!encoded.contains("00000000-0000-4000-8000-000000000001"));
    assert!(!encoded.contains("api.openai.com"));
    assert!(!encoded.contains("endpoint"));
}

#[test]
fn route_decision_maps_supported_reasoning_and_rejects_unsupported_reasoning() {
    let catalog = ProviderCatalog::bundled().expect("catalog");
    let providers = two_provider_config();
    let routing = RoutingConfig {
        aliases: Vec::new(),
        rules: Vec::new(),
        default: Some(target("primary", "gpt-5.6-sol")),
    };
    let snapshot = RuntimeSnapshot::build(&catalog, &providers, &routing).expect("snapshot");
    let decision = snapshot
        .route(&request(None, "anything", None))
        .expect("route");

    assert_eq!(
        decision
            .map_reasoning_effort("medium")
            .expect("reasoning effort"),
        "medium"
    );

    let unsupported = ProviderConfig {
        instances: vec![provider("limited", "parallel")],
        accounts: vec![account("limited-key", "limited")],
    };
    let unsupported_routing = RoutingConfig {
        aliases: Vec::new(),
        rules: Vec::new(),
        default: Some(target("limited", "plain-model")),
    };
    let snapshot =
        RuntimeSnapshot::build(&catalog, &unsupported, &unsupported_routing).expect("snapshot");
    let decision = snapshot
        .route(&request(None, "anything", None))
        .expect("route");
    assert_eq!(
        decision.map_reasoning_effort("medium"),
        Err(RouteError::UnsupportedReasoningEffort)
    );
}

#[test]
fn builder_is_deterministic_and_candidates_borrow_only_enabled_accounts() {
    let catalog = ProviderCatalog::bundled().expect("catalog");
    let mut providers = two_provider_config();
    providers.accounts.push(AccountConfig {
        id: AccountId::new("disabled-key").expect("account"),
        provider: ProviderId::new("primary").expect("provider"),
        enabled: false,
        auth: AccountAuthConfig::ApiKey {
            secret: SecretRef::parse("secret:00000000-0000-4000-8000-000000000002")
                .expect("secret ref"),
        },
    });
    providers.instances.push(ProviderInstanceConfig {
        id: ProviderId::new("disabled-provider").expect("provider"),
        catalog_id: ProviderId::new("openai-apikey").expect("catalog provider"),
        enabled: false,
        endpoint: None,
        allow_private_network: false,
    });
    providers
        .accounts
        .push(account("disabled-provider-key", "disabled-provider"));
    let routing = RoutingConfig {
        aliases: Vec::new(),
        rules: Vec::new(),
        default: Some(target("primary", "gpt-5.6-sol")),
    };

    let first = SnapshotBuilder::new(&catalog, &providers, &routing)
        .build()
        .expect("first snapshot");
    let second = SnapshotBuilder::new(&catalog, &providers, &routing)
        .build()
        .expect("second snapshot");
    assert_eq!(first.public_models(), second.public_models());

    let decision = first
        .route(&request(None, "anything", None))
        .expect("route");
    let candidates = decision.candidates().collect::<Vec<_>>();
    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates[0].account().expect("account").id().as_str(),
        "primary-key"
    );
    assert!(candidates.iter().all(|candidate| {
        candidate.provider().id().as_str() != "disabled-provider"
            && candidate
                .account()
                .is_none_or(|account| account.id().as_str() != "disabled-key")
    }));

    assert_eq!(
        first.route(&request(Some("disabled-provider"), "gpt-5.6-sol", None)),
        Err(RouteError::ProviderUnavailable)
    );
}

fn two_provider_config() -> ProviderConfig {
    ProviderConfig {
        instances: vec![
            provider("primary", "openai-apikey"),
            provider("backup", "openai-apikey"),
        ],
        accounts: vec![
            account("primary-key", "primary"),
            account("backup-key", "backup"),
        ],
    }
}

fn provider(id: &str, catalog_id: &str) -> ProviderInstanceConfig {
    ProviderInstanceConfig {
        id: ProviderId::new(id).expect("provider"),
        catalog_id: ProviderId::new(catalog_id).expect("catalog provider"),
        enabled: true,
        endpoint: None,
        allow_private_network: false,
    }
}

fn account(id: &str, provider: &str) -> AccountConfig {
    AccountConfig {
        id: AccountId::new(id).expect("account"),
        provider: ProviderId::new(provider).expect("provider"),
        enabled: true,
        auth: AccountAuthConfig::ApiKey {
            secret: SecretRef::parse("secret:00000000-0000-4000-8000-000000000001")
                .expect("secret ref"),
        },
    }
}

fn target(provider: &str, model: &str) -> RouteTarget {
    RouteTarget {
        provider: ProviderId::new(provider).expect("provider"),
        model: model.to_owned(),
    }
}

fn request(provider: Option<&str>, model: &str, client: Option<&str>) -> RouteRequest {
    RouteRequest {
        provider: provider.map(|id| ProviderId::new(id).expect("provider")),
        model: model.to_owned(),
        client_id: client.map(|id| ClientId::new(id).expect("client")),
    }
}
