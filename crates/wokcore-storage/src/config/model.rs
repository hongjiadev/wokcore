#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct AppConfig {
    pub server: ServerConfig,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct VersionedConfig {
    pub revision: u64,
    #[serde(flatten)]
    pub config: AppConfig,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ServerConfig {
    pub port: u16,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig { port: 10101 },
        }
    }
}
