use std::{fs::File, io::Read, path::PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AppPaths, PlatformError};

use super::permissions::{
    open_existing_runtime_directory, open_existing_secure_file, publish_secure_file,
    remove_open_secure_file, sync_parent_directory,
};

pub const MAX_DISCOVERY_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryRecord {
    pub base_url: String,
    pub pid: u32,
    pub instance_id: Uuid,
    pub wokcore_version: String,
    pub api_major: u32,
}

#[derive(Clone, Debug)]
pub struct DiscoveryStore {
    runtime_dir: PathBuf,
    path: PathBuf,
}

impl DiscoveryStore {
    pub fn new(paths: &AppPaths) -> Result<Self, PlatformError> {
        if paths.discovery_file.parent() != Some(paths.runtime_dir.as_path()) {
            return Err(PlatformError::UnsafeRuntimePath);
        }
        let runtime_dir = open_existing_runtime_directory(&paths.runtime_dir)?;
        drop(runtime_dir);
        Ok(Self {
            runtime_dir: paths.runtime_dir.clone(),
            path: paths.discovery_file.clone(),
        })
    }

    pub fn read(&self) -> Result<DiscoveryRecord, PlatformError> {
        let runtime_dir = open_existing_runtime_directory(&self.runtime_dir)?;
        let file = open_existing_secure_file(&runtime_dir, &self.path)?;
        let record = read_record(file)?;
        Ok(record)
    }

    pub fn publish(&self, record: &DiscoveryRecord) -> Result<(), PlatformError> {
        validate_record(record)?;
        let document = serde_json::to_vec(record).map_err(|_| PlatformError::InvalidDiscovery)?;
        if document.len() > MAX_DISCOVERY_BYTES {
            return Err(PlatformError::DiscoveryTooLarge);
        }

        let runtime_dir = open_existing_runtime_directory(&self.runtime_dir)?;
        match open_existing_secure_file(&runtime_dir, &self.path) {
            Ok(existing) => drop(existing),
            Err(PlatformError::Io { source }) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        publish_secure_file(&runtime_dir, &self.path, &document)?;
        sync_parent_directory(&runtime_dir)?;
        Ok(())
    }

    pub fn remove_if_owned(&self, instance_id: Uuid) -> Result<bool, PlatformError> {
        let runtime_dir = open_existing_runtime_directory(&self.runtime_dir)?;
        let file = match open_existing_secure_file(&runtime_dir, &self.path) {
            Ok(file) => file,
            Err(PlatformError::Io { source }) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        let record = read_record(file.try_clone()?)?;
        if record.instance_id != instance_id {
            return Ok(false);
        }

        remove_open_secure_file(&runtime_dir, file, &self.path)?;
        sync_parent_directory(&runtime_dir)?;
        Ok(true)
    }
}

fn read_record(file: File) -> Result<DiscoveryRecord, PlatformError> {
    let metadata = file.metadata()?;
    if metadata.len() > MAX_DISCOVERY_BYTES as u64 {
        return Err(PlatformError::DiscoveryTooLarge);
    }

    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(MAX_DISCOVERY_BYTES));
    file.take((MAX_DISCOVERY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_DISCOVERY_BYTES {
        return Err(PlatformError::DiscoveryTooLarge);
    }
    let record = serde_json::from_slice::<DiscoveryRecord>(&bytes)
        .map_err(|_| PlatformError::InvalidDiscovery)?;
    validate_record(&record)?;
    Ok(record)
}

fn validate_record(record: &DiscoveryRecord) -> Result<(), PlatformError> {
    if record.pid == 0
        || record.api_major == 0
        || !valid_semver(&record.wokcore_version)
        || !valid_base_url(&record.base_url)
    {
        return Err(PlatformError::InvalidDiscovery);
    }
    Ok(())
}

fn valid_base_url(value: &str) -> bool {
    value
        .strip_prefix("http://127.0.0.1:")
        .filter(|port| !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|port| port.parse::<u16>().ok())
        .is_some_and(|port| port != 0)
}

fn valid_semver(value: &str) -> bool {
    let Some((core_and_pre, build)) = split_optional_once(value, '+') else {
        return false;
    };
    if build.is_some_and(|identifiers| !valid_semver_identifiers(identifiers, false)) {
        return false;
    }
    let (core, pre_release) = match core_and_pre.split_once('-') {
        Some((_core, "")) => return false,
        Some((core, pre_release)) => (core, Some(pre_release)),
        None => (core_and_pre, None),
    };
    if pre_release.is_some_and(|identifiers| !valid_semver_identifiers(identifiers, true)) {
        return false;
    }

    let mut core_identifiers = core.split('.');
    let valid_core = core_identifiers
        .by_ref()
        .take(3)
        .all(valid_numeric_identifier);
    valid_core && core_identifiers.next().is_none() && core.split('.').count() == 3
}

fn split_optional_once(value: &str, delimiter: char) -> Option<(&str, Option<&str>)> {
    let mut parts = value.split(delimiter);
    let before = parts.next()?;
    let after = parts.next();
    if parts.next().is_some() || after.is_some_and(str::is_empty) {
        return None;
    }
    Some((before, after))
}

fn valid_numeric_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'))
}

fn valid_semver_identifiers(value: &str, reject_numeric_leading_zero: bool) -> bool {
    value.split('.').all(|identifier| {
        !identifier.is_empty()
            && identifier
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && (!reject_numeric_leading_zero
                || !identifier.bytes().all(|byte| byte.is_ascii_digit())
                || valid_numeric_identifier(identifier))
    })
}
