use wokcore_core::config::{ProviderConfig, RoutingConfig};

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub providers: ProviderConfig,
    pub routing: RoutingConfig,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VersionedConfig {
    pub revision: u64,
    pub config: AppConfig,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub port: u16,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PersistedConfig {
    pub revision: u64,
    pub server: ServerConfig,
    #[serde(default)]
    pub providers: ProviderConfig,
    #[serde(default)]
    pub routing: RoutingConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig { port: 10101 },
            providers: ProviderConfig::default(),
            routing: RoutingConfig::default(),
        }
    }
}
