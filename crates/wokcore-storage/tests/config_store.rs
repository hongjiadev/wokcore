use std::{
    fs,
    sync::{Arc, Barrier},
    thread,
    time::SystemTime,
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
fn serialized_config_has_only_revision_and_server_port() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    ConfigStore::new(&path)
        .commit(0, &AppConfig::default())
        .unwrap();

    let document = fs::read_to_string(path).unwrap();

    assert!(document.contains("revision = 1"));
    assert!(document.contains("[server]"));
    assert!(document.contains("port = 10101"));
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
