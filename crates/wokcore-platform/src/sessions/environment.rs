use std::{
    collections::BTreeMap,
    env, fmt,
    path::{Path, PathBuf},
};

use crate::system::paths::Platform;

use super::SessionError;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SessionSourceKind {
    Codex,
    ClaudeCode,
    GeminiCli,
}

impl SessionSourceKind {
    pub const ALL: [Self; 3] = [Self::Codex, Self::ClaudeCode, Self::GeminiCli];

    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude_code",
            Self::GeminiCli => "gemini_cli",
        }
    }

    fn environment_name(self) -> &'static str {
        match self {
            Self::Codex => "CODEX_HOME",
            Self::ClaudeCode => "CLAUDE_CONFIG_DIR",
            Self::GeminiCli => "GEMINI_CLI_HOME",
        }
    }

    fn home_component(self) -> &'static str {
        match self {
            Self::Codex => ".codex",
            Self::ClaudeCode => ".claude",
            Self::GeminiCli => ".gemini",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SessionEnvironment {
    platform: Platform,
    values: BTreeMap<String, PathBuf>,
}

impl SessionEnvironment {
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
        let values = [
            "HOME",
            "USERPROFILE",
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
            "GEMINI_CLI_HOME",
        ]
        .into_iter()
        .filter_map(|name| env::var_os(name).map(|value| (name.to_owned(), PathBuf::from(value))))
        .collect();
        Self { platform, values }
    }

    fn absolute_value(&self, name: &str) -> Option<PathBuf> {
        self.values
            .get(name)
            .filter(|path| is_absolute(self.platform, path))
            .cloned()
    }

    fn home(&self) -> Result<PathBuf, SessionError> {
        self.absolute_value("HOME")
            .or_else(|| self.absolute_value("USERPROFILE"))
            .ok_or(SessionError::MissingPlatformData {
                name: "home directory",
            })
    }
}

#[derive(Clone, Debug, Default)]
pub struct SessionRootOverrides {
    roots: BTreeMap<SessionSourceKind, PathBuf>,
}

impl SessionRootOverrides {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_root(mut self, kind: SessionSourceKind, path: impl Into<PathBuf>) -> Self {
        self.roots.insert(kind, path.into());
        self
    }
}

pub struct SessionRoots {
    roots: BTreeMap<SessionSourceKind, PathBuf>,
}

impl SessionRoots {
    pub fn discover(overrides: SessionRootOverrides) -> Result<Self, SessionError> {
        Self::resolve(SessionEnvironment::current(), overrides)
    }

    pub fn resolve(
        environment: SessionEnvironment,
        overrides: SessionRootOverrides,
    ) -> Result<Self, SessionError> {
        let mut roots = BTreeMap::new();
        for kind in SessionSourceKind::ALL {
            let path = match overrides.roots.get(&kind) {
                Some(path) if is_absolute(environment.platform, path) => path.clone(),
                Some(_) => return Err(SessionError::UnsafePath),
                None => match environment.absolute_value(kind.environment_name()) {
                    Some(path) => path,
                    None => append_component(
                        environment.platform,
                        environment.home()?,
                        kind.home_component(),
                    )
                    .ok_or(SessionError::UnsafePath)?,
                },
            };
            roots.insert(kind, path);
        }
        Ok(Self { roots })
    }

    pub fn path(&self, kind: SessionSourceKind) -> &Path {
        self.roots
            .get(&kind)
            .expect("every Session source has one resolved root")
    }
}

impl fmt::Debug for SessionRoots {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionRoots")
            .field(
                "sources",
                &SessionSourceKind::ALL.map(SessionSourceKind::label),
            )
            .finish()
    }
}

fn append_component(platform: Platform, mut path: PathBuf, component: &str) -> Option<PathBuf> {
    if uses_native_paths(platform) {
        path.push(component);
        return Some(path);
    }

    let separator = match platform {
        Platform::Windows => '\\',
        Platform::Macos | Platform::Linux => '/',
    };
    let mut value = path.to_str()?.to_owned();
    if !value.ends_with(separator) {
        value.push(separator);
    }
    value.push_str(component);
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

fn uses_native_paths(platform: Platform) -> bool {
    match platform {
        Platform::Windows => cfg!(windows),
        Platform::Macos => cfg!(target_os = "macos"),
        Platform::Linux => cfg!(target_os = "linux"),
    }
}
