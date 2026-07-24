#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub server: ServerConfig,
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
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig { port: 10101 },
        }
    }
}
