use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv6Addr};

use serde::{Deserialize, Serialize};
use url::{Host, Url};
use wokcore_core::id::ProviderId;

const CATALOG_SCHEMA_VERSION: u32 = 1;
const MAX_PROVIDERS: usize = 256;
const MAX_ALIASES_PER_PROVIDER: usize = 32;
const MAX_MODELS_PER_PROVIDER: usize = 512;
const MAX_LABEL_BYTES: usize = 256;
const MAX_MODEL_ID_BYTES: usize = 256;
const MAX_ENDPOINT_BYTES: usize = 2_048;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterFamily {
    OpenAiResponses,
    OpenAiChat,
    Anthropic,
    Google,
    AzureOpenAi,
    Cursor,
    Kiro,
    MimoFree,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    Forward,
    Oauth,
    Key,
    Local,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointPolicy {
    PublicHttps,
    HttpsTemplate,
    LoopbackHttp,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSourceKind {
    None,
    Static,
    Live,
    Hybrid,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCapabilities {
    pub text: bool,
    pub streaming: bool,
    pub tools: bool,
    pub vision: bool,
    pub images: bool,
    pub reasoning: bool,
    #[serde(default)]
    pub count_tokens: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDefinition {
    pub id: String,
    pub label: String,
    pub adapter: AdapterFamily,
    pub base_url: String,
    pub auth_kind: AuthKind,
    pub endpoint_policy: EndpointPolicy,
    pub model_source: ModelSourceKind,
    pub aliases: Vec<String>,
    pub models: Vec<String>,
    pub default_model: Option<String>,
    #[serde(default)]
    pub allow_endpoint_override: bool,
    #[serde(default)]
    pub key_optional: bool,
    #[serde(default)]
    pub allow_key_auth_override: bool,
    #[serde(default)]
    pub reasoning_efforts: Vec<String>,
    #[serde(default)]
    pub reasoning_effort_map: BTreeMap<String, String>,
    pub capabilities: ProviderCapabilities,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogFile {
    schema_version: u32,
    baseline_commit: String,
    providers: Vec<ProviderDefinition>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProviderCatalog {
    schema_version: u32,
    baseline_commit: String,
    providers: Vec<ProviderDefinition>,
}

impl ProviderCatalog {
    pub fn bundled() -> Result<Self, CatalogError> {
        Self::parse(include_str!("../provider-catalog/providers.toml"))
    }

    pub fn parse(source: &str) -> Result<Self, CatalogError> {
        let file = toml_edit::de::from_str::<CatalogFile>(source)
            .map_err(|_| CatalogError::new("catalog_format"))?;
        Self::validate(file)
    }

    pub fn baseline_commit(&self) -> &str {
        &self.baseline_commit
    }

    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn providers(&self) -> &[ProviderDefinition] {
        &self.providers
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderDefinition> {
        self.providers.iter().find(|provider| provider.id == id)
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, CatalogError> {
        serde_json::to_vec(self).map_err(|_| CatalogError::new("catalog_serialization"))
    }

    fn validate(file: CatalogFile) -> Result<Self, CatalogError> {
        if file.schema_version != CATALOG_SCHEMA_VERSION {
            return Err(CatalogError::new("unsupported_schema"));
        }
        if file.baseline_commit.is_empty()
            || file.baseline_commit.len() > 64
            || !file
                .baseline_commit
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(CatalogError::new("invalid_baseline"));
        }
        if file.providers.is_empty() || file.providers.len() > MAX_PROVIDERS {
            return Err(CatalogError::new("invalid_provider_count"));
        }

        let mut canonical_ids = BTreeSet::new();
        for provider in &file.providers {
            ProviderId::new(provider.id.clone())
                .map_err(|_| CatalogError::new("invalid_provider_id"))?;
            if !canonical_ids.insert(provider.id.as_str()) {
                return Err(CatalogError::new("duplicate_provider_id"));
            }
        }

        let mut aliases = BTreeSet::new();
        for provider in &file.providers {
            validate_provider(provider, &canonical_ids, &mut aliases)?;
        }

        Ok(Self {
            schema_version: file.schema_version,
            baseline_commit: file.baseline_commit,
            providers: file.providers,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("invalid Provider catalog ({code})")]
pub struct CatalogError {
    code: &'static str,
}

impl CatalogError {
    const fn new(code: &'static str) -> Self {
        Self { code }
    }

    pub const fn code(self) -> &'static str {
        self.code
    }
}

fn validate_provider<'a>(
    provider: &'a ProviderDefinition,
    canonical_ids: &BTreeSet<&'a str>,
    aliases: &mut BTreeSet<&'a str>,
) -> Result<(), CatalogError> {
    if provider.label.trim().is_empty() || provider.label.len() > MAX_LABEL_BYTES {
        return Err(CatalogError::new("invalid_label"));
    }
    if provider.aliases.len() > MAX_ALIASES_PER_PROVIDER {
        return Err(CatalogError::new("too_many_aliases"));
    }
    for alias in &provider.aliases {
        ProviderId::new(alias.clone()).map_err(|_| CatalogError::new("invalid_alias"))?;
        if canonical_ids.contains(alias.as_str()) || !aliases.insert(alias) {
            return Err(CatalogError::new("alias_collision"));
        }
    }

    validate_models(provider)?;
    validate_endpoint(&provider.base_url, provider.endpoint_policy)?;
    if !provider.capabilities.text || !provider.capabilities.streaming {
        return Err(CatalogError::new("invalid_capabilities"));
    }
    validate_reasoning_metadata(provider)?;
    Ok(())
}

fn validate_models(provider: &ProviderDefinition) -> Result<(), CatalogError> {
    if provider.models.len() > MAX_MODELS_PER_PROVIDER {
        return Err(CatalogError::new("too_many_models"));
    }
    if matches!(
        provider.model_source,
        ModelSourceKind::Static | ModelSourceKind::Hybrid
    ) && provider.models.is_empty()
    {
        return Err(CatalogError::new("missing_static_models"));
    }
    if provider.model_source == ModelSourceKind::None && !provider.models.is_empty() {
        return Err(CatalogError::new("unexpected_models"));
    }

    let mut models = BTreeSet::new();
    for model in &provider.models {
        if model.trim().is_empty()
            || model.len() > MAX_MODEL_ID_BYTES
            || model.chars().any(char::is_control)
            || !models.insert(model)
        {
            return Err(CatalogError::new("invalid_model"));
        }
    }
    if let Some(default_model) = &provider.default_model
        && (!models.contains(default_model) && provider.model_source != ModelSourceKind::Live)
    {
        return Err(CatalogError::new("invalid_default_model"));
    }
    Ok(())
}

fn validate_reasoning_metadata(provider: &ProviderDefinition) -> Result<(), CatalogError> {
    if !provider.capabilities.reasoning
        && (!provider.reasoning_efforts.is_empty() || !provider.reasoning_effort_map.is_empty())
    {
        return Err(CatalogError::new("unexpected_reasoning_metadata"));
    }
    if provider.reasoning_efforts.len() > 16 || provider.reasoning_effort_map.len() > 32 {
        return Err(CatalogError::new("too_much_reasoning_metadata"));
    }

    let mut efforts = BTreeSet::new();
    for effort in &provider.reasoning_efforts {
        if !is_safe_reasoning_value(effort) || !efforts.insert(effort) {
            return Err(CatalogError::new("invalid_reasoning_effort"));
        }
    }
    for (input, wire) in &provider.reasoning_effort_map {
        if !is_safe_reasoning_value(input) || !is_safe_reasoning_value(wire) {
            return Err(CatalogError::new("invalid_reasoning_effort_map"));
        }
    }
    Ok(())
}

fn is_safe_reasoning_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn validate_endpoint(endpoint: &str, policy: EndpointPolicy) -> Result<(), CatalogError> {
    if endpoint.is_empty() || endpoint.len() > MAX_ENDPOINT_BYTES {
        return Err(CatalogError::new("invalid_endpoint"));
    }
    match policy {
        EndpointPolicy::PublicHttps => validate_public_url(endpoint),
        EndpointPolicy::HttpsTemplate => {
            let expanded = expand_endpoint_template(endpoint)?;
            validate_public_url(&expanded)
        }
        EndpointPolicy::LoopbackHttp => validate_loopback_url(endpoint),
    }
}

fn validate_public_url(endpoint: &str) -> Result<(), CatalogError> {
    let url = Url::parse(endpoint).map_err(|_| CatalogError::new("invalid_endpoint"))?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(CatalogError::new("unsafe_public_endpoint"));
    }
    let host = url
        .host()
        .ok_or_else(|| CatalogError::new("missing_endpoint_host"))?;
    if is_private_host(host) {
        return Err(CatalogError::new("private_public_endpoint"));
    }
    Ok(())
}

fn validate_loopback_url(endpoint: &str) -> Result<(), CatalogError> {
    let url = Url::parse(endpoint).map_err(|_| CatalogError::new("invalid_endpoint"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(CatalogError::new("unsafe_loopback_endpoint"));
    }
    let is_loopback = match url
        .host()
        .ok_or_else(|| CatalogError::new("missing_endpoint_host"))?
    {
        Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(address) => address.is_loopback(),
        Host::Ipv6(address) => address.is_loopback(),
    };
    if !is_loopback {
        return Err(CatalogError::new("non_loopback_endpoint"));
    }
    Ok(())
}

fn expand_endpoint_template(endpoint: &str) -> Result<String, CatalogError> {
    if !endpoint.starts_with("https://") {
        return Err(CatalogError::new("unsafe_template_endpoint"));
    }
    let mut expanded = String::with_capacity(endpoint.len());
    let mut characters = endpoint.chars();
    while let Some(character) = characters.next() {
        match character {
            '{' => {
                let mut placeholder = String::new();
                loop {
                    match characters.next() {
                        Some('}') => break,
                        Some(value)
                            if value.is_ascii_alphanumeric() || matches!(value, '_' | '-') =>
                        {
                            placeholder.push(value);
                        }
                        _ => return Err(CatalogError::new("invalid_endpoint_template")),
                    }
                }
                if placeholder.is_empty() {
                    return Err(CatalogError::new("invalid_endpoint_template"));
                }
                expanded.push_str("placeholder");
            }
            '}' => return Err(CatalogError::new("invalid_endpoint_template")),
            value => expanded.push(value),
        }
    }
    Ok(expanded)
}

pub(crate) fn is_private_host(host: Host<&str>) -> bool {
    match host {
        Host::Domain(domain) => {
            let domain = domain.to_ascii_lowercase();
            domain == "localhost"
                || domain.ends_with(".localhost")
                || domain.ends_with(".local")
                || domain.ends_with(".internal")
        }
        Host::Ipv4(address) => is_private_ip(IpAddr::V4(address)),
        Host::Ipv6(address) => is_private_ip(IpAddr::V6(address)),
    }
}

fn is_private_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_broadcast()
                || address.is_documentation()
                || address.is_unspecified()
                || address.is_multicast()
                || address.octets()[0] == 0
        }
        IpAddr::V6(address) => {
            address
                .to_ipv4_mapped()
                .is_some_and(|mapped| is_private_ip(IpAddr::V4(mapped)))
                || address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || is_ipv6_unique_local(address)
                || is_ipv6_link_local(address)
        }
    }
}

fn is_ipv6_unique_local(address: Ipv6Addr) -> bool {
    address.octets()[0] & 0xfe == 0xfc
}

fn is_ipv6_link_local(address: Ipv6Addr) -> bool {
    let octets = address.octets();
    octets[0] == 0xfe && octets[1] & 0xc0 == 0x80
}
