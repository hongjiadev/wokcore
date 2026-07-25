use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
};

use crate::PlatformError;

const APPLICATION_NAME: &str = "WokCore";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppPaths {
    pub config_file: PathBuf,
    pub state_db: PathBuf,
    pub runtime_dir: PathBuf,
    pub log_dir: PathBuf,
    pub discovery_file: PathBuf,
    pub instance_lock: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self, PlatformError> {
        Self::resolve(EnvironmentSnapshot::current())
    }

    pub fn resolve(environment: EnvironmentSnapshot) -> Result<Self, PlatformError> {
        let config_dir = environment.config_dir()?;
        let state_dir = environment.state_dir()?;
        let runtime_dir = environment.runtime_dir(&state_dir);

        Ok(Self {
            config_file: config_dir.join("config.toml"),
            state_db: state_dir.join("state.sqlite3"),
            log_dir: state_dir.join("logs"),
            discovery_file: runtime_dir.join("discovery.json"),
            instance_lock: runtime_dir.join("instance.lock"),
            runtime_dir,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Windows,
    Macos,
    Linux,
}

#[derive(Clone, Debug)]
pub struct EnvironmentSnapshot {
    platform: Platform,
    values: BTreeMap<String, PathBuf>,
}

impl EnvironmentSnapshot {
    pub fn new<'a>(
        platform: Platform,
        values: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Self {
        Self {
            platform,
            values: values
                .into_iter()
                .map(|(name, value)| (name.to_owned(), PathBuf::from(value)))
                .collect(),
        }
    }

    fn current() -> Self {
        let platform = if cfg!(windows) {
            Platform::Windows
        } else if cfg!(target_os = "macos") {
            Platform::Macos
        } else {
            Platform::Linux
        };
        let names = match platform {
            Platform::Windows => ["APPDATA", "LOCALAPPDATA", "HOME", "USERPROFILE"].as_slice(),
            Platform::Macos => ["HOME", "USERPROFILE"].as_slice(),
            Platform::Linux => [
                "XDG_CONFIG_HOME",
                "XDG_STATE_HOME",
                "XDG_RUNTIME_DIR",
                "HOME",
                "USERPROFILE",
            ]
            .as_slice(),
        };
        let values = names
            .iter()
            .filter_map(|name| {
                env::var_os(name).map(|value| ((*name).to_owned(), PathBuf::from(value)))
            })
            .collect();

        Self { platform, values }
    }

    fn config_dir(&self) -> Result<PathBuf, PlatformError> {
        match self.platform {
            Platform::Windows => Ok(self.windows_data_dir("APPDATA", &["AppData", "Roaming"])?),
            Platform::Macos => Ok(self
                .home_dir()?
                .join("Library")
                .join("Application Support")
                .join(APPLICATION_NAME)),
            Platform::Linux => Ok(self.xdg_directory("XDG_CONFIG_HOME", &[".config"])?),
        }
    }

    fn state_dir(&self) -> Result<PathBuf, PlatformError> {
        match self.platform {
            Platform::Windows => Ok(self.windows_data_dir("LOCALAPPDATA", &["AppData", "Local"])?),
            Platform::Macos => self.config_dir(),
            Platform::Linux => Ok(self.xdg_directory("XDG_STATE_HOME", &[".local", "state"])?),
        }
    }

    fn runtime_dir(&self, state_dir: &Path) -> PathBuf {
        match self.platform {
            Platform::Linux => self
                .environment_path("XDG_RUNTIME_DIR")
                .map(|path| path.join(APPLICATION_NAME))
                .unwrap_or_else(|| state_dir.join("runtime")),
            Platform::Windows | Platform::Macos => state_dir.join("runtime"),
        }
    }

    fn windows_data_dir(
        &self,
        variable: &str,
        fallback: &[&str],
    ) -> Result<PathBuf, PlatformError> {
        self.environment_path(variable)
            .or_else(|| {
                self.home_dir()
                    .ok()
                    .map(|home| append_components(home, fallback))
            })
            .ok_or(PlatformError::MissingPlatformData {
                name: "application data directory",
            })
            .map(|path| path.join(APPLICATION_NAME))
    }

    fn xdg_directory(&self, variable: &str, fallback: &[&str]) -> Result<PathBuf, PlatformError> {
        self.environment_path(variable)
            .or_else(|| {
                self.home_dir()
                    .ok()
                    .map(|home| append_components(home, fallback))
            })
            .ok_or(PlatformError::MissingPlatformData {
                name: "home directory",
            })
            .map(|path| path.join(APPLICATION_NAME))
    }

    fn home_dir(&self) -> Result<PathBuf, PlatformError> {
        self.environment_path("HOME")
            .or_else(|| self.environment_path("USERPROFILE"))
            .ok_or(PlatformError::MissingPlatformData {
                name: "home directory",
            })
    }

    fn environment_path(&self, variable: &str) -> Option<PathBuf> {
        self.values
            .get(variable)
            .filter(|path| is_absolute(self.platform, path))
            .cloned()
    }
}

fn append_components(mut path: PathBuf, components: &[&str]) -> PathBuf {
    for component in components {
        path.push(component);
    }
    path
}

fn is_absolute(platform: Platform, path: &Path) -> bool {
    let value = path.to_string_lossy();

    match platform {
        Platform::Windows => {
            value.starts_with(r"\\")
                || value
                    .as_bytes()
                    .get(1..3)
                    .is_some_and(|prefix| prefix[0] == b':' && matches!(prefix[1], b'\\' | b'/'))
        }
        Platform::Macos | Platform::Linux => value.starts_with('/'),
    }
}
