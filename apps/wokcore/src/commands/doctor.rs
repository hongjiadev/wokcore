use std::{io, net::TcpListener};

use serde_json::json;
use wokcore_platform::{DiscoveryRecord, DiscoveryStore, PlatformError, RuntimeLease};
use wokcore_storage::{ConfigStore, ReadOnlyStateStore, StorageError};

use crate::{CommandOutput, ExitCode, RunDependencies, cli::JsonOutput};

use super::{
    status::{IdentityError, verify_identity},
    write_json,
};

pub(super) async fn run(
    options: JsonOutput,
    dependencies: &RunDependencies,
    output: &mut dyn CommandOutput,
) -> ExitCode {
    match read_discovery(dependencies) {
        Ok(record) => return inspect_online(record, options, dependencies, output).await,
        Err(DoctorProbe::Absent) => {}
        Err(result) => return render(result, options.json, output, None),
    }

    match inspect_offline_state(dependencies) {
        Ok(Some(record)) => return inspect_online(record, options, dependencies, output).await,
        Ok(None) => {}
        Err(error) => return render(error, options.json, output, None),
    }
    if dependencies.paths.config_file.try_exists().unwrap_or(false) {
        let config = match ConfigStore::new(&dependencies.paths.config_file).load() {
            Ok(config) => config,
            Err(StorageError::InvalidConfig { .. }) => {
                return render(
                    DoctorProbe::InvalidConfiguration,
                    options.json,
                    output,
                    None,
                );
            }
            Err(_) => {
                return render(DoctorProbe::Internal, options.json, output, None);
            }
        };
        match TcpListener::bind(("127.0.0.1", config.config.server.port)) {
            Ok(listener) => drop(listener),
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                return render(DoctorProbe::PortOccupied, options.json, output, None);
            }
            Err(_) => return render(DoctorProbe::Internal, options.json, output, None),
        }
    }
    render(DoctorProbe::Absent, options.json, output, None)
}

async fn inspect_online(
    record: DiscoveryRecord,
    options: JsonOutput,
    dependencies: &RunDependencies,
    output: &mut dyn CommandOutput,
) -> ExitCode {
    if !dependencies.process.is_running(record.pid) {
        return render(
            DoctorProbe::PidMismatch,
            options.json,
            output,
            Some(&record),
        );
    }
    let result = match verify_identity(&record).await {
        Ok(()) => DoctorProbe::Healthy,
        Err(IdentityError::Unreachable) => DoctorProbe::Unreachable,
        Err(IdentityError::InstanceMismatch) => DoctorProbe::InstanceMismatch,
        Err(IdentityError::ApiMismatch) => DoctorProbe::ApiMismatch,
        Err(
            IdentityError::InvalidAuthority
            | IdentityError::InvalidResponse
            | IdentityError::Internal,
        ) => DoctorProbe::UnsafeRuntime,
    };
    if result == DoctorProbe::Healthy
        && let Err(error) = inspect_live_state(dependencies)
    {
        return render(error, options.json, output, Some(&record));
    }
    render(result, options.json, output, Some(&record))
}

fn read_discovery(dependencies: &RunDependencies) -> Result<DiscoveryRecord, DoctorProbe> {
    let store = DiscoveryStore::new(&dependencies.paths).map_err(map_discovery_error)?;
    store.read().map_err(map_discovery_error)
}

fn map_discovery_error(error: PlatformError) -> DoctorProbe {
    match error {
        PlatformError::Io { source } if source.kind() == io::ErrorKind::NotFound => {
            DoctorProbe::Absent
        }
        PlatformError::UnsafeRuntimePath
        | PlatformError::InvalidDiscovery
        | PlatformError::DiscoveryTooLarge => DoctorProbe::UnsafeRuntime,
        PlatformError::Io { .. } => DoctorProbe::UnsafeRuntime,
        PlatformError::AlreadyRunning | PlatformError::MissingPlatformData { .. } => {
            DoctorProbe::Internal
        }
    }
}

fn inspect_live_state(dependencies: &RunDependencies) -> Result<(), DoctorProbe> {
    match dependencies.paths.state_db.try_exists() {
        Ok(false) => return Err(DoctorProbe::StorageCorrupt),
        Err(_) => return Err(DoctorProbe::Internal),
        Ok(true) => {}
    }
    let state = ReadOnlyStateStore::open_live(&dependencies.paths.state_db);
    match state {
        Ok(state) => state.health().map(|_| ()).map_err(map_storage_error),
        Err(error) => Err(map_storage_error(error)),
    }
}

fn inspect_offline_state(
    dependencies: &RunDependencies,
) -> Result<Option<DiscoveryRecord>, DoctorProbe> {
    match dependencies.paths.state_db.try_exists() {
        Ok(false) => return Ok(None),
        Err(_) => return Err(DoctorProbe::Internal),
        Ok(true) => {}
    }
    let lease = match RuntimeLease::acquire_existing(&dependencies.paths) {
        Ok(lease) => lease,
        Err(PlatformError::AlreadyRunning) => {
            return match read_discovery(dependencies) {
                Ok(record) => Ok(Some(record)),
                Err(DoctorProbe::Absent) => Err(DoctorProbe::Unreachable),
                Err(error) => Err(error),
            };
        }
        Err(PlatformError::UnsafeRuntimePath) => return Err(DoctorProbe::UnsafeRuntime),
        Err(_) => return Err(DoctorProbe::Internal),
    };
    match read_discovery(dependencies) {
        Ok(record) => return Ok(Some(record)),
        Err(DoctorProbe::Absent) => {}
        Err(error) => return Err(error),
    }
    let state =
        ReadOnlyStateStore::open(&dependencies.paths.state_db).map_err(map_storage_error)?;
    state.health().map_err(map_storage_error)?;
    drop(state);
    drop(lease);
    Ok(None)
}

fn map_storage_error(error: StorageError) -> DoctorProbe {
    match error {
        StorageError::StateDatabaseCorrupt { .. } | StorageError::StateDatabase { .. } => {
            DoctorProbe::StorageCorrupt
        }
        _ => DoctorProbe::Internal,
    }
}

fn render(
    probe: DoctorProbe,
    json_output: bool,
    output: &mut dyn CommandOutput,
    record: Option<&DiscoveryRecord>,
) -> ExitCode {
    let (exit, code, human) = match probe {
        DoctorProbe::Healthy => (ExitCode::Success, "healthy", "WokCore is healthy.\n"),
        DoctorProbe::Absent => (
            ExitCode::NotRunning,
            "absent",
            "WokCore runtime is absent.\n",
        ),
        DoctorProbe::Unreachable => (
            ExitCode::NotRunning,
            "unreachable",
            "WokCore runtime is unreachable.\n",
        ),
        DoctorProbe::PidMismatch => (
            ExitCode::NotRunning,
            "pid_mismatch",
            "WokCore runtime process identity is stale.\n",
        ),
        DoctorProbe::InstanceMismatch => (
            ExitCode::InvalidInput,
            "instance_mismatch",
            "WokCore runtime instance identity is invalid.\n",
        ),
        DoctorProbe::ApiMismatch => (
            ExitCode::InvalidInput,
            "api_mismatch",
            "WokCore management API identity is incompatible.\n",
        ),
        DoctorProbe::UnsafeRuntime => (
            ExitCode::InvalidInput,
            "unsafe_runtime",
            "WokCore runtime object is unsafe.\n",
        ),
        DoctorProbe::PortOccupied => (
            ExitCode::PortOccupied,
            "port_occupied",
            "The configured WokCore port is occupied.\n",
        ),
        DoctorProbe::StorageCorrupt => (
            ExitCode::StorageCorruption,
            "storage_corrupt",
            "WokCore storage is corrupt.\n",
        ),
        DoctorProbe::InvalidConfiguration => (
            ExitCode::InvalidInput,
            "invalid_configuration",
            "WokCore configuration is invalid.\n",
        ),
        DoctorProbe::Internal => (
            ExitCode::InternalFailure,
            "internal_error",
            "WokCore diagnostics failed.\n",
        ),
    };
    let rendered = if json_output {
        let value = if probe == DoctorProbe::Healthy {
            let record = record.expect("healthy diagnostics have discovery identity");
            json!({
                "api_major": record.api_major,
                "code": code,
                "instance_id": record.instance_id,
                "pid": record.pid,
                "wokcore_version": record.wokcore_version,
            })
        } else {
            json!({"code": code})
        };
        write_json(output, &value)
    } else if exit == ExitCode::Success || exit == ExitCode::NotRunning {
        output.write_stdout(human)
    } else {
        output.write_stderr(human)
    };
    if rendered.is_ok() {
        exit
    } else {
        ExitCode::InternalFailure
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DoctorProbe {
    Healthy,
    Absent,
    Unreachable,
    PidMismatch,
    InstanceMismatch,
    ApiMismatch,
    UnsafeRuntime,
    PortOccupied,
    StorageCorrupt,
    InvalidConfiguration,
    Internal,
}
