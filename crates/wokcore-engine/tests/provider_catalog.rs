use std::collections::BTreeSet;

use wokcore_engine::catalog::{
    AdapterFamily, AuthKind, EndpointPolicy, ModelSourceKind, ProviderCatalog,
};

const FROZEN_BASELINE: &str = "97e7326f89bcfbb29a2c73250cb25eb801d066b6";

#[test]
fn bundled_catalog_matches_the_frozen_provider_baseline() {
    let catalog = ProviderCatalog::bundled().expect("bundled catalog");
    let actual = catalog
        .providers()
        .iter()
        .map(|provider| provider.id.as_str())
        .collect::<Vec<_>>();
    let expected = vec![
        "openai",
        "cursor",
        "xai",
        "anthropic",
        "anthropic-apikey",
        "kimi",
        "kiro",
        "openai-apikey",
        "umans",
        "opencode-go",
        "neuralwatt",
        "openrouter",
        "orcarouter",
        "groq",
        "google",
        "google-vertex",
        "google-antigravity",
        "azure-openai",
        "ollama",
        "vllm",
        "lm-studio",
        "deepseek",
        "cerebras",
        "together",
        "fireworks",
        "firepass",
        "moonshot",
        "huggingface",
        "nvidia",
        "venice",
        "zai",
        "nanogpt",
        "synthetic",
        "siliconflow",
        "qwen-cloud",
        "tencent-coding-plan",
        "qianfan",
        "alibaba",
        "alibaba-token-plan",
        "alibaba-token-plan-intl",
        "parallel",
        "zenmux",
        "litellm",
        "ollama-cloud",
        "mistral",
        "minimax",
        "minimax-cn",
        "kimi-code",
        "opencode-zen",
        "vercel-ai-gateway",
        "opencode-free",
        "xiaomi",
        "kilo",
        "mimo-free",
        "cloudflare-ai-gateway",
        "cloudflare-workers-ai",
        "github-copilot",
        "gitlab-duo",
    ];

    assert_eq!(catalog.baseline_commit(), FROZEN_BASELINE);
    assert_eq!(actual, expected);
    assert_eq!(actual.iter().copied().collect::<BTreeSet<_>>().len(), 58);
}

#[test]
fn bundled_entries_have_explicit_runtime_metadata() {
    let catalog = ProviderCatalog::bundled().expect("bundled catalog");

    for provider in catalog.providers() {
        assert!(!provider.label.trim().is_empty(), "{} label", provider.id);
        assert!(!provider.base_url.trim().is_empty(), "{} URL", provider.id);
        assert!(
            provider.capabilities.text,
            "{} must declare text support",
            provider.id
        );
        assert!(
            provider.capabilities.streaming,
            "{} must declare streaming support",
            provider.id
        );
        assert!(
            !matches!(provider.model_source, ModelSourceKind::Static)
                || !provider.models.is_empty(),
            "{} has a static model source without models",
            provider.id
        );
    }

    let ollama = catalog
        .providers()
        .iter()
        .find(|provider| provider.id == "ollama")
        .expect("ollama");
    assert_eq!(ollama.adapter, AdapterFamily::OpenAiChat);
    assert_eq!(ollama.auth_kind, AuthKind::Local);
    assert_eq!(ollama.endpoint_policy, EndpointPolicy::LoopbackHttp);

    let azure = catalog
        .providers()
        .iter()
        .find(|provider| provider.id == "azure-openai")
        .expect("azure-openai");
    assert_eq!(azure.adapter, AdapterFamily::AzureOpenAi);
    assert_eq!(azure.endpoint_policy, EndpointPolicy::HttpsTemplate);
}

#[test]
fn catalog_rejects_duplicate_ids_and_alias_shadowing() {
    let duplicate = catalog_fixture(
        r#"
[[providers]]
id = "first"
label = "Duplicate"
adapter = "open_ai_chat"
base_url = "https://duplicate.example/v1"
auth_kind = "key"
endpoint_policy = "public_https"
model_source = "none"
aliases = []
models = []
capabilities = { text = true, streaming = true, tools = true, vision = false, images = false, reasoning = false }
"#,
    );
    assert!(ProviderCatalog::parse(&duplicate).is_err());

    let alias_shadow = catalog_fixture(
        r#"
[[providers]]
id = "second"
label = "Alias shadow"
adapter = "open_ai_chat"
base_url = "https://second.example/v1"
auth_kind = "key"
endpoint_policy = "public_https"
model_source = "none"
aliases = ["first"]
models = []
capabilities = { text = true, streaming = true, tools = true, vision = false, images = false, reasoning = false }
"#,
    );
    assert!(ProviderCatalog::parse(&alias_shadow).is_err());
}

#[test]
fn catalog_rejects_secret_fields_and_unsafe_public_endpoints() {
    let secret_field = catalog_fixture(
        r#"
api_key = "must-not-be-accepted"
"#,
    );
    assert!(ProviderCatalog::parse(&secret_field).is_err());

    let public_http =
        catalog_fixture("").replace("https://first.example/v1", "http://first.example/v1");
    assert!(ProviderCatalog::parse(&public_http).is_err());

    let remote_loopback_policy = catalog_fixture("")
        .replace("https://first.example/v1", "http://192.0.2.10:11434/v1")
        .replace("public_https", "loopback_http");
    assert!(ProviderCatalog::parse(&remote_loopback_policy).is_err());
}

#[test]
fn canonical_json_is_stable_across_equivalent_loads() {
    let source = catalog_fixture("");
    let first = ProviderCatalog::parse(&source).expect("first parse");
    let second = ProviderCatalog::parse(&source).expect("second parse");

    let first_json = first.canonical_json().expect("first JSON");
    let second_json = second.canonical_json().expect("second JSON");
    assert_eq!(first_json, second_json);
    assert!(serde_json::from_slice::<serde_json::Value>(&first_json).is_ok());
}

fn catalog_fixture(extra_provider_fields: &str) -> String {
    format!(
        r#"
schema_version = 1
baseline_commit = "fixture"

[[providers]]
id = "first"
label = "First"
adapter = "open_ai_chat"
base_url = "https://first.example/v1"
auth_kind = "key"
endpoint_policy = "public_https"
model_source = "none"
aliases = []
models = []
capabilities = {{ text = true, streaming = true, tools = true, vision = false, images = false, reasoning = false }}
{extra_provider_fields}
"#
    )
}
