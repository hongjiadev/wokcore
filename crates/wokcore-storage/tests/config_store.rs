use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant, SystemTime},
};

use wokcore_core::{
    config::{AccountAuthConfig, AccountConfig, ProviderInstanceConfig},
    id::{AccountId, ProviderId},
    secret::SecretRef,
};
use wokcore_storage::{AppConfig, ConfigStore, StorageError};

#[test]
fn absent_load_returns_defaults_without_creating_artifacts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let lock_path = directory.path().join("config.toml.lock");

    let loaded = ConfigStore::new(&path).load().unwrap();

    assert_eq!(loaded.revision, 0);
    assert_eq!(loaded.config, AppConfig::default());
    assert_eq!(loaded.config.server.port, 10101);
    assert!(!path.exists());
    assert!(!lock_path.exists());
    assert!(fs::read_dir(directory.path()).unwrap().next().is_none());
}

#[test]
fn commit_creates_revision_one_and_survives_reload() {
    let directory = tempfile::tempdir().unwrap();
    let store = ConfigStore::new(directory.path().join("config.toml"));
    let mut candidate = AppConfig::default();
    candidate.server.port = 10102;

    let committed = store.commit(0, &candidate).unwrap();

    assert_eq!(committed.revision, 1);
    assert_eq!(committed.config, candidate);
    assert_eq!(store.load().unwrap(), committed);
}

#[test]
fn legacy_server_only_config_loads_empty_provider_defaults() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, "revision = 7\n\n[server]\nport = 12001\n").unwrap();

    let loaded = ConfigStore::new(path).load().unwrap();

    assert_eq!(loaded.revision, 7);
    assert_eq!(loaded.config.server.port, 12001);
    assert!(loaded.config.providers.instances.is_empty());
    assert!(loaded.config.providers.accounts.is_empty());
    assert!(loaded.config.routing.aliases.is_empty());
    assert!(loaded.config.routing.rules.is_empty());
    assert!(loaded.config.routing.default.is_none());
}

#[test]
fn provider_configuration_round_trips_with_opaque_secret_references() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let store = ConfigStore::new(&path);
    let mut candidate = AppConfig::default();
    candidate.providers.instances.push(ProviderInstanceConfig {
        id: ProviderId::new("primary").unwrap(),
        catalog_id: ProviderId::new("openai-apikey").unwrap(),
        enabled: true,
        endpoint: None,
        allow_private_network: false,
    });
    candidate.providers.accounts.push(AccountConfig {
        id: AccountId::new("work").unwrap(),
        provider: ProviderId::new("primary").unwrap(),
        enabled: true,
        auth: AccountAuthConfig::ApiKey {
            secret: SecretRef::parse("secret:00000000-0000-4000-8000-000000000001").unwrap(),
        },
    });

    let committed = store.commit(0, &candidate).unwrap();
    let document = fs::read_to_string(&path).unwrap();

    assert_eq!(store.load().unwrap(), committed);
    assert!(document.contains("secret:00000000-0000-4000-8000-000000000001"));
    for forbidden in ["api_key =", "password =", "authorization =", "bearer ="] {
        assert!(
            !document.to_ascii_lowercase().contains(forbidden),
            "serialized config contains raw credential field {forbidden}"
        );
    }
}

#[test]
fn unsafe_endpoint_credentials_are_rejected_before_config_file_creation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let mut candidate = AppConfig::default();
    candidate.providers.instances.push(ProviderInstanceConfig {
        id: ProviderId::new("primary").unwrap(),
        catalog_id: ProviderId::new("qwen-cloud").unwrap(),
        enabled: true,
        endpoint: Some("https://compatible.example/v1?api_key=raw-secret".to_owned()),
        allow_private_network: false,
    });

    let error = ConfigStore::new(&path).commit(0, &candidate).unwrap_err();

    assert!(matches!(error, StorageError::InvalidConfig { .. }));
    assert!(!path.exists());
    assert!(!directory.path().join("config.toml.lock").exists());
}

#[test]
fn stale_revision_conflict_preserves_file_bytes_and_modified_time() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let store = ConfigStore::new(&path);
    store.commit(0, &AppConfig::default()).unwrap();
    let before_bytes = fs::read(&path).unwrap();
    let before_modified = modified_time(&path);
    let mut stale_candidate = AppConfig::default();
    stale_candidate.server.port = 10102;

    let error = store.commit(0, &stale_candidate).unwrap_err();

    assert!(matches!(
        error,
        StorageError::RevisionConflict {
            expected: 0,
            actual: 1
        }
    ));
    assert_eq!(fs::read(&path).unwrap(), before_bytes);
    assert_eq!(modified_time(&path), before_modified);
}

#[test]
fn malformed_toml_commit_preserves_source_and_leaves_no_temporary_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let malformed = b"revision = [\n";
    fs::write(&path, malformed).unwrap();
    let before_modified = modified_time(&path);

    let error = ConfigStore::new(&path)
        .commit(0, &AppConfig::default())
        .unwrap_err();

    assert!(matches!(error, StorageError::InvalidConfig { .. }));
    assert_eq!(fs::read(&path).unwrap(), malformed);
    assert_eq!(modified_time(&path), before_modified);
    assert_eq!(
        directory_entry_names(directory.path()),
        ["config.toml", "config.toml.lock"]
    );
}

#[test]
fn zero_port_is_rejected_before_creating_file_or_lock() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let lock_path = directory.path().join("config.toml.lock");
    let mut candidate = AppConfig::default();
    candidate.server.port = 0;

    let error = ConfigStore::new(&path).commit(0, &candidate).unwrap_err();

    assert!(matches!(error, StorageError::InvalidConfig { .. }));
    assert!(!path.exists());
    assert!(!lock_path.exists());
    assert!(fs::read_dir(directory.path()).unwrap().next().is_none());
}

#[test]
fn serialized_config_has_only_safe_revision_server_provider_and_routing_fields() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    ConfigStore::new(&path)
        .commit(0, &AppConfig::default())
        .unwrap();

    let document = fs::read_to_string(path).unwrap();

    assert!(document.contains("revision = 1"));
    assert!(document.contains("[server]"));
    assert!(document.contains("port = 10101"));
    assert!(document.contains("[providers]"));
    assert!(document.contains("instances = []"));
    assert!(document.contains("accounts = []"));
    assert!(document.contains("[routing]"));
    assert!(document.contains("aliases = []"));
    assert!(document.contains("rules = []"));
    for forbidden in [
        "host",
        "allow_insecure_private_lan",
        "ui",
        "locale",
        "timezone",
    ] {
        assert!(
            !document.contains(forbidden),
            "serialized config contains forbidden field {forbidden}: {document}"
        );
    }
}

#[test]
fn unknown_fields_are_rejected_without_mutating_the_source() {
    for document in [
        "revision = 1\nhost = \"127.0.0.1\"\n\n[server]\nport = 10101\n",
        "revision = 1\narbitrary = true\n\n[server]\nport = 10101\n",
        "revision = 1\n\n[server]\nport = 10101\nhost = \"127.0.0.1\"\n",
        "revision = 1\n\n[server]\nport = 10101\nallow_insecure_private_lan = true\n",
        "revision = 1\nlocale = \"en\"\n\n[server]\nport = 10101\n",
        "revision = 1\ntimezone = \"UTC\"\n\n[server]\nport = 10101\n",
        "revision = 1\n\n[server]\nport = 10101\n\n[ui]\nlocale = \"en\"\ntimezone = \"UTC\"\n",
        "revision = 1\n\n[server]\nport = 10101\n\n[providers]\ninstances = []\naccounts = []\nraw_token = \"forbidden\"\n",
        "revision = 1\n\n[server]\nport = 10101\n\n[providers]\naccounts = []\n\n[[providers.instances]]\nid = \"primary\"\ncatalog_id = \"qwen-cloud\"\nenabled = true\nallow_private_network = false\nunknown = true\n",
        "revision = 1\n\n[server]\nport = 10101\n\n[providers]\ninstances = []\n\n[[providers.accounts]]\nid = \"work\"\nprovider = \"primary\"\nenabled = true\n\n[providers.accounts.auth]\nkind = \"api_key\"\nsecret = \"secret:00000000-0000-4000-8000-000000000001\"\napi_key = \"forbidden\"\n",
        "revision = 1\n\n[server]\nport = 10101\n\n[providers]\ninstances = []\naccounts = []\n\n[routing]\naliases = []\nrules = []\nunknown = true\n",
    ] {
        assert_invalid_document_is_preserved(document);
    }
}

#[test]
fn invalid_config_errors_do_not_echo_rejected_values() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(
        &path,
        "revision = 1\n\n[server]\nport = \"raw-credential-must-not-escape\"\n",
    )
    .unwrap();

    let error = ConfigStore::new(path).load().unwrap_err().to_string();

    assert!(!error.contains("raw-credential-must-not-escape"));
}

#[test]
fn concurrent_commits_from_one_revision_allow_exactly_one_success() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(ConfigStore::new(directory.path().join("config.toml")));
    let barrier = Arc::new(Barrier::new(2));

    let threads = [10102, 10103].map(|port| {
        let barrier = Arc::clone(&barrier);
        let store = Arc::clone(&store);
        thread::spawn(move || {
            let mut candidate = AppConfig::default();
            candidate.server.port = port;
            barrier.wait();
            store.commit(0, &candidate)
        })
    });
    let results = threads.map(|thread| thread.join().unwrap());

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(
                result,
                Err(StorageError::RevisionConflict {
                    expected: 0,
                    actual: 1
                })
            ))
            .count(),
        1
    );
}

#[test]
fn concurrent_process_commits_from_one_revision_allow_exactly_one_success() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let synchronization = directory.path().join("synchronization");
    fs::create_dir(&synchronization).unwrap();
    let executable = env::current_exe().unwrap();

    let mut children = ["first", "second"].map(|identifier| {
        Command::new(&executable)
            .args(["--exact", "process_commit_helper", "--nocapture"])
            .env("WOKCORE_STORAGE_HELPER_CONFIG", &path)
            .env("WOKCORE_STORAGE_HELPER_SYNC", &synchronization)
            .env("WOKCORE_STORAGE_HELPER_ID", identifier)
            .spawn()
            .unwrap()
    });

    wait_for_all(&synchronization, &["first.ready", "second.ready"]);
    fs::write(synchronization.join("start"), []).unwrap();

    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }

    let results = ["first", "second"]
        .map(|identifier| fs::read_to_string(synchronization.join(identifier)).unwrap());
    assert_eq!(
        results.iter().filter(|result| *result == "success").count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| *result == "conflict")
            .count(),
        1
    );
    assert_eq!(ConfigStore::new(path).load().unwrap().revision, 1);
}

#[test]
fn process_commit_helper() {
    let Ok(config_path) = env::var("WOKCORE_STORAGE_HELPER_CONFIG") else {
        return;
    };
    let synchronization = PathBuf::from(env::var("WOKCORE_STORAGE_HELPER_SYNC").unwrap());
    let identifier = env::var("WOKCORE_STORAGE_HELPER_ID").unwrap();
    fs::write(synchronization.join(format!("{identifier}.ready")), []).unwrap();
    wait_for_all(&synchronization, &["start"]);

    let mut candidate = AppConfig::default();
    candidate.server.port = match identifier.as_str() {
        "first" => 10102,
        "second" => 10103,
        _ => panic!("unexpected helper identifier"),
    };
    let outcome = match ConfigStore::new(config_path).commit(0, &candidate) {
        Ok(_) => "success",
        Err(StorageError::RevisionConflict {
            expected: 0,
            actual: 1,
        }) => "conflict",
        Err(error) => panic!("unexpected commit result: {error}"),
    };
    fs::write(synchronization.join(identifier), outcome).unwrap();
}

#[test]
fn revision_overflow_is_rejected_without_mutating_the_config() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(
        &path,
        "revision = 18446744073709551615\n\n[server]\nport = 10101\n",
    )
    .unwrap();
    let before_bytes = fs::read(&path).unwrap();
    let before_modified = modified_time(&path);

    let error = ConfigStore::new(&path)
        .commit(u64::MAX, &AppConfig::default())
        .unwrap_err();

    assert!(matches!(error, StorageError::InvalidConfig { .. }));
    assert_eq!(fs::read(&path).unwrap(), before_bytes);
    assert_eq!(modified_time(&path), before_modified);
}

fn modified_time(path: &std::path::Path) -> SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

fn assert_invalid_document_is_preserved(document: &str) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, document).unwrap();
    let before_bytes = fs::read(&path).unwrap();
    let before_modified = modified_time(&path);
    let store = ConfigStore::new(&path);

    assert!(matches!(
        store.load().unwrap_err(),
        StorageError::InvalidConfig { .. }
    ));
    assert!(matches!(
        store.commit(1, &AppConfig::default()).unwrap_err(),
        StorageError::InvalidConfig { .. }
    ));
    assert_eq!(fs::read(&path).unwrap(), before_bytes);
    assert_eq!(modified_time(&path), before_modified);
    assert_eq!(
        directory_entry_names(directory.path()),
        ["config.toml", "config.toml.lock"]
    );
}

fn wait_for_all(directory: &Path, names: &[&str]) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !names.iter().all(|name| directory.join(name).exists()) {
        assert!(Instant::now() < deadline, "timed out waiting for {names:?}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn directory_entry_names(directory: &Path) -> Vec<String> {
    let mut names = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}
