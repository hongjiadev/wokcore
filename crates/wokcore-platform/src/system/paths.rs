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
        let runtime_dir = environment.runtime_dir(&state_dir)?;
        let config_file = append_components(environment.platform, config_dir, &["config.toml"])
            .ok_or(PlatformError::MissingPlatformData {
                name: "configuration directory",
            })?;
        let state_db =
            append_components(environment.platform, state_dir.clone(), &["state.sqlite3"]).ok_or(
                PlatformError::MissingPlatformData {
                    name: "state directory",
                },
            )?;
        let log_dir = append_components(environment.platform, state_dir, &["logs"]).ok_or(
            PlatformError::MissingPlatformData {
                name: "state directory",
            },
        )?;
        let discovery_file = append_components(
            environment.platform,
            runtime_dir.clone(),
            &["discovery.json"],
        )
        .ok_or(PlatformError::MissingPlatformData {
            name: "runtime directory",
        })?;
        let instance_lock = append_components(
            environment.platform,
            runtime_dir.clone(),
            &["instance.lock"],
        )
        .ok_or(PlatformError::MissingPlatformData {
            name: "runtime directory",
        })?;

        Ok(Self {
            config_file,
            state_db,
            log_dir,
            discovery_file,
            instance_lock,
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
            Platform::Windows => [
                "WOKCORE_HOME",
                "APPDATA",
                "LOCALAPPDATA",
                "HOME",
                "USERPROFILE",
            ]
            .as_slice(),
            Platform::Macos => ["WOKCORE_HOME", "HOME", "USERPROFILE"].as_slice(),
            Platform::Linux => [
                "WOKCORE_HOME",
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
        if let Some(directory) = self.environment_path("WOKCORE_HOME") {
            return Ok(directory);
        }
        match self.platform {
            Platform::Windows => Ok(self.windows_data_dir("APPDATA", &["AppData", "Roaming"])?),
            Platform::Macos => append_components(
                self.platform,
                self.home_dir()?,
                &["Library", "Application Support", APPLICATION_NAME],
            )
            .ok_or(PlatformError::MissingPlatformData {
                name: "home directory",
            }),
            Platform::Linux => Ok(self.xdg_directory("XDG_CONFIG_HOME", &[".config"])?),
        }
    }

    fn state_dir(&self) -> Result<PathBuf, PlatformError> {
        if let Some(directory) = self.environment_path("WOKCORE_HOME") {
            return Ok(directory);
        }
        match self.platform {
            Platform::Windows => Ok(self.windows_data_dir("LOCALAPPDATA", &["AppData", "Local"])?),
            Platform::Macos => self.config_dir(),
            Platform::Linux => Ok(self.xdg_directory("XDG_STATE_HOME", &[".local", "state"])?),
        }
    }

    fn runtime_dir(&self, state_dir: &Path) -> Result<PathBuf, PlatformError> {
        let (runtime_root, component) = match self.platform {
            Platform::Linux => match self.environment_path("XDG_RUNTIME_DIR") {
                Some(runtime_root) => (runtime_root, APPLICATION_NAME),
                None => (state_dir.to_path_buf(), "runtime"),
            },
            Platform::Windows | Platform::Macos => (state_dir.to_path_buf(), "runtime"),
        };

        append_components(self.platform, runtime_root, &[component]).ok_or(
            PlatformError::MissingPlatformData {
                name: "runtime directory",
            },
        )
    }

    fn windows_data_dir(
        &self,
        variable: &str,
        fallback: &[&str],
    ) -> Result<PathBuf, PlatformError> {
        let directory = self
            .environment_path(variable)
            .or_else(|| {
                self.home_dir()
                    .ok()
                    .and_then(|home| append_components(self.platform, home, fallback))
            })
            .ok_or(PlatformError::MissingPlatformData {
                name: "application data directory",
            })?;

        append_components(self.platform, directory, &[APPLICATION_NAME]).ok_or(
            PlatformError::MissingPlatformData {
                name: "application data directory",
            },
        )
    }

    fn xdg_directory(&self, variable: &str, fallback: &[&str]) -> Result<PathBuf, PlatformError> {
        let directory = self
            .environment_path(variable)
            .or_else(|| {
                self.home_dir()
                    .ok()
                    .and_then(|home| append_components(self.platform, home, fallback))
            })
            .ok_or(PlatformError::MissingPlatformData {
                name: "home directory",
            })?;

        append_components(self.platform, directory, &[APPLICATION_NAME]).ok_or(
            PlatformError::MissingPlatformData {
                name: "home directory",
            },
        )
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
            .filter(|path| self.platform.uses_native_paths() || path.to_str().is_some())
            .cloned()
    }
}

fn append_components(
    platform: Platform,
    mut path: PathBuf,
    components: &[&str],
) -> Option<PathBuf> {
    if platform.uses_native_paths() {
        for component in components {
            path.push(component);
        }
        return Some(path);
    }

    let separator = platform.separator();
    let mut value = path.to_str()?.to_owned();
    if matches!(platform, Platform::Windows) {
        value = value.replace('/', "\\");
    }
    for component in components {
        if !value.ends_with(separator) {
            value.push(separator);
        }
        value.push_str(component);
    }
    Some(PathBuf::from(value))
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

impl Platform {
    fn uses_native_paths(self) -> bool {
        match self {
            Platform::Windows => cfg!(windows),
            Platform::Macos => cfg!(target_os = "macos"),
            Platform::Linux => cfg!(target_os = "linux"),
        }
    }

    fn separator(self) -> char {
        match self {
            Platform::Windows => '\\',
            Platform::Macos | Platform::Linux => '/',
        }
    }
}
