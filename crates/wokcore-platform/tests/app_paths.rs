use std::{fs, path::Path};

use tempfile::tempdir;
use wokcore_platform::{AppPaths, EnvironmentSnapshot, Platform};

#[test]
fn windows_uses_wokcore_roots_and_runtime_artifact_names() {
    let paths = AppPaths::resolve(EnvironmentSnapshot::new(
        Platform::Windows,
        [
            ("APPDATA", r"C:\Users\Ada\AppData\Roaming"),
            ("LOCALAPPDATA", r"C:\Users\Ada\AppData\Local"),
        ],
    ))
    .expect("Windows paths resolve from absolute environment roots");

    assert_path(
        &paths.config_file,
        r"C:\Users\Ada\AppData\Roaming\WokCore\config.toml",
    );
    assert_path(
        &paths.state_db,
        r"C:\Users\Ada\AppData\Local\WokCore\state.sqlite3",
    );
    assert_path(
        &paths.runtime_dir,
        r"C:\Users\Ada\AppData\Local\WokCore\runtime",
    );
    assert_path(&paths.log_dir, r"C:\Users\Ada\AppData\Local\WokCore\logs");
    assert_path(
        &paths.discovery_file,
        r"C:\Users\Ada\AppData\Local\WokCore\runtime\discovery.json",
    );
    assert_path(
        &paths.instance_lock,
        r"C:\Users\Ada\AppData\Local\WokCore\runtime\instance.lock",
    );
}

#[test]
fn macos_uses_wokcore_application_support_directory() {
    let paths = AppPaths::resolve(EnvironmentSnapshot::new(
        Platform::Macos,
        [("HOME", "/Users/ada")],
    ))
    .expect("macOS paths resolve from HOME");

    assert_path(
        &paths.config_file,
        "/Users/ada/Library/Application Support/WokCore/config.toml",
    );
    assert_path(
        &paths.state_db,
        "/Users/ada/Library/Application Support/WokCore/state.sqlite3",
    );
    assert_path(
        &paths.runtime_dir,
        "/Users/ada/Library/Application Support/WokCore/runtime",
    );
}

#[test]
fn linux_uses_absolute_xdg_roots_and_wokcore_runtime_directory() {
    let paths = AppPaths::resolve(EnvironmentSnapshot::new(
        Platform::Linux,
        [
            ("XDG_CONFIG_HOME", "/etc/xdg"),
            ("XDG_STATE_HOME", "/var/lib/xdg"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ],
    ))
    .expect("Linux paths resolve from absolute XDG roots");

    assert_path(&paths.config_file, "/etc/xdg/WokCore/config.toml");
    assert_path(&paths.state_db, "/var/lib/xdg/WokCore/state.sqlite3");
    assert_path(&paths.runtime_dir, "/run/user/1000/WokCore");
    assert_path(&paths.log_dir, "/var/lib/xdg/WokCore/logs");
    assert_path(
        &paths.discovery_file,
        "/run/user/1000/WokCore/discovery.json",
    );
    assert_path(&paths.instance_lock, "/run/user/1000/WokCore/instance.lock");
}

#[test]
fn relative_linux_environment_paths_fall_back_to_home_owned_directories() {
    let paths = AppPaths::resolve(EnvironmentSnapshot::new(
        Platform::Linux,
        [
            ("HOME", "/home/ada"),
            ("XDG_CONFIG_HOME", "relative-config"),
            ("XDG_STATE_HOME", "relative-state"),
            ("XDG_RUNTIME_DIR", "relative-runtime"),
        ],
    ))
    .expect("relative XDG paths fail closed to the documented HOME fallbacks");

    assert_path(&paths.config_file, "/home/ada/.config/WokCore/config.toml");
    assert_path(
        &paths.state_db,
        "/home/ada/.local/state/WokCore/state.sqlite3",
    );
    assert_path(&paths.runtime_dir, "/home/ada/.local/state/WokCore/runtime");
}

#[test]
fn missing_required_environment_and_home_fails_closed() {
    let error = AppPaths::resolve(EnvironmentSnapshot::new(Platform::Linux, []))
        .expect_err("missing XDG roots without HOME must not resolve relative paths");

    assert!(error.to_string().contains("home directory"));
}

#[test]
fn resolving_paths_creates_no_directories_or_files() {
    let temporary_root = tempdir().expect("test temporary directory");
    let app_data = temporary_root.path().join("absent-app-data");
    let local_app_data = temporary_root.path().join("absent-local-app-data");
    let before = directory_entries(temporary_root.path());
    let app_data_value = app_data.to_string_lossy().into_owned();
    let local_app_data_value = local_app_data.to_string_lossy().into_owned();
    let paths = AppPaths::resolve(EnvironmentSnapshot::new(
        Platform::Windows,
        [
            ("APPDATA", app_data_value.as_str()),
            ("LOCALAPPDATA", local_app_data_value.as_str()),
        ],
    ))
    .expect("paths resolve without materializing the supplied roots");

    assert!(!app_data.exists());
    assert!(!local_app_data.exists());
    assert_eq!(directory_entries(temporary_root.path()), before);
    assert!(!paths.config_file.exists());
    assert!(!paths.state_db.exists());
    assert!(!paths.runtime_dir.exists());
    assert!(!paths.log_dir.exists());
    assert!(!paths.discovery_file.exists());
    assert!(!paths.instance_lock.exists());
}

fn assert_path(path: &Path, expected: &str) {
    assert_eq!(path.to_string_lossy(), expected);
}

fn directory_entries(root: &Path) -> Vec<String> {
    let mut entries = fs::read_dir(root)
        .expect("read test temporary root")
        .map(|entry| {
            entry
                .expect("read test temporary-root entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    entries.sort();
    entries
}
