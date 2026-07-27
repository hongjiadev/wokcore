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
    config::{ProviderConfigError, validate_provider_configuration},
};

#[test]
fn valid_provider_configuration_resolves_every_reference() {
    let catalog = ProviderCatalog::bundled().expect("catalog");
    let providers = ProviderConfig {
        instances: vec![provider_instance("primary", "openai-apikey")],
        accounts: vec![api_key_account("work", "primary")],
    };
    let routing = RoutingConfig {
        aliases: vec![ModelAlias {
            alias: "fast".to_owned(),
            target: route_target("primary", "gpt-5.6-terra"),
        }],
        rules: vec![RouteRule {
            client_id: Some(ClientId::new("wokrouter").expect("client ID")),
            model: Some("code".to_owned()),
            target: route_target("primary", "gpt-5.6-sol"),
        }],
        default: Some(route_target("primary", "gpt-5.6-terra")),
    };

    let validated =
        validate_provider_configuration(&catalog, &providers, &routing).expect("valid config");

    assert_eq!(validated.provider_count(), 1);
    assert_eq!(validated.account_count(), 1);
    assert_eq!(validated.route_count(), 3);
}

#[test]
fn endpoint_override_requires_catalog_permission_and_private_opt_in() {
    let catalog = ProviderCatalog::bundled().expect("catalog");
    let routing = RoutingConfig::default();

    let mut disallowed = provider_instance("primary", "openai-apikey");
    disallowed.endpoint = Some("https://compatible.example/v1".to_owned());
    let error = validate_provider_configuration(
        &catalog,
        &ProviderConfig {
            instances: vec![disallowed],
            accounts: vec![api_key_account("work", "primary")],
        },
        &routing,
    )
    .expect_err("OpenAI override must be rejected");
    assert_eq!(error, ProviderConfigError::EndpointOverrideNotAllowed);

    let mut private = provider_instance("primary", "qwen-cloud");
    private.endpoint = Some("http://127.0.0.1:19001/v1".to_owned());
    let private_config = ProviderConfig {
        instances: vec![private.clone()],
        accounts: vec![api_key_account("work", "primary")],
    };
    assert_eq!(
        validate_provider_configuration(&catalog, &private_config, &routing),
        Err(ProviderConfigError::PrivateNetworkOptInRequired)
    );

    let mut mapped_loopback = provider_instance("primary", "qwen-cloud");
    mapped_loopback.endpoint = Some("http://[::ffff:127.0.0.1]:19001/v1".to_owned());
    assert_eq!(
        validate_provider_configuration(
            &catalog,
            &ProviderConfig {
                instances: vec![mapped_loopback],
                accounts: vec![api_key_account("work", "primary")],
            },
            &routing,
        ),
        Err(ProviderConfigError::PrivateNetworkOptInRequired)
    );

    private.allow_private_network = true;
    validate_provider_configuration(
        &catalog,
        &ProviderConfig {
            instances: vec![private],
            accounts: vec![api_key_account("work", "primary")],
        },
        &routing,
    )
    .expect("explicit loopback override");

    let mut insecure_public = provider_instance("primary", "qwen-cloud");
    insecure_public.endpoint = Some("http://public.example/v1".to_owned());
    insecure_public.allow_private_network = true;
    assert_eq!(
        validate_provider_configuration(
            &catalog,
            &ProviderConfig {
                instances: vec![insecure_public],
                accounts: vec![api_key_account("work", "primary")],
            },
            &routing,
        ),
        Err(ProviderConfigError::InsecurePublicEndpoint)
    );

    let mut query_credential = provider_instance("primary", "qwen-cloud");
    query_credential.endpoint = Some("https://compatible.example/v1?api_key=raw-secret".to_owned());
    assert_eq!(
        validate_provider_configuration(
            &catalog,
            &ProviderConfig {
                instances: vec![query_credential],
                accounts: vec![api_key_account("work", "primary")],
            },
            &routing,
        ),
        Err(ProviderConfigError::InvalidEndpoint)
    );
}

#[test]
fn configuration_rejects_auth_mismatch_and_dangling_routes() {
    let catalog = ProviderCatalog::bundled().expect("catalog");
    let mismatched = ProviderConfig {
        instances: vec![provider_instance("primary", "openai-apikey")],
        accounts: vec![AccountConfig {
            id: AccountId::new("work").expect("account ID"),
            provider: ProviderId::new("primary").expect("provider ID"),
            enabled: true,
            auth: AccountAuthConfig::Local,
        }],
    };
    assert_eq!(
        validate_provider_configuration(&catalog, &mismatched, &RoutingConfig::default()),
        Err(ProviderConfigError::AuthenticationMismatch)
    );

    let providers = ProviderConfig {
        instances: vec![provider_instance("primary", "openai-apikey")],
        accounts: vec![api_key_account("work", "primary")],
    };
    let routing = RoutingConfig {
        aliases: Vec::new(),
        rules: Vec::new(),
        default: Some(route_target("missing", "gpt-5.6-sol")),
    };
    assert_eq!(
        validate_provider_configuration(&catalog, &providers, &routing),
        Err(ProviderConfigError::UnknownRouteProvider)
    );
}

#[test]
fn configuration_debug_output_redacts_every_secret_reference() {
    let mut instance = provider_instance("primary", "openai-apikey");
    instance.endpoint =
        Some("https://compatible.example/v1?api_key=raw-endpoint-secret".to_owned());
    let providers = ProviderConfig {
        instances: vec![instance],
        accounts: vec![api_key_account("work", "primary")],
    };

    let debug = format!("{providers:?}");

    assert!(!debug.contains("00000000-0000-4000-8000-000000000001"));
    assert!(!debug.contains("raw-endpoint-secret"));
    assert!(debug.contains("SecretRef([redacted])"));
    assert!(debug.contains("endpoint_present"));
}

fn provider_instance(id: &str, catalog_id: &str) -> ProviderInstanceConfig {
    ProviderInstanceConfig {
        id: ProviderId::new(id).expect("instance ID"),
        catalog_id: ProviderId::new(catalog_id).expect("catalog ID"),
        enabled: true,
        endpoint: None,
        allow_private_network: false,
    }
}

fn api_key_account(id: &str, provider: &str) -> AccountConfig {
    AccountConfig {
        id: AccountId::new(id).expect("account ID"),
        provider: ProviderId::new(provider).expect("provider ID"),
        enabled: true,
        auth: AccountAuthConfig::ApiKey {
            secret: SecretRef::parse("secret:00000000-0000-4000-8000-000000000001")
                .expect("secret reference"),
        },
    }
}

fn route_target(provider: &str, model: &str) -> RouteTarget {
    RouteTarget {
        provider: ProviderId::new(provider).expect("provider ID"),
        model: model.to_owned(),
    }
}
