use std::{fs, path::Path};

use secrecy::{ExposeSecret, SecretString};
use wokcore_core::{
    id::{AccountId, ProviderId},
    secret::{SecretPurpose, SecretRef, SecretScope},
};
use wokcore_storage::{
    EnvironmentSecretStore, HeadlessSecretStoreConfig, MemorySecretStore,
    PermissionedFileSecretStore, SecretStore, StorageError,
};

fn scope() -> SecretScope {
    SecretScope {
        provider_id: ProviderId::new("openai").unwrap(),
        account_id: Some(AccountId::new("primary-account").unwrap()),
        purpose: SecretPurpose::ApiKey,
    }
}

#[tokio::test]
async fn memory_store_round_trips_deletes_and_reports_wrong_refs() {
    let store = MemorySecretStore::default();
    let plaintext = ["memory", "secret"].join("-");
    let secret_ref = store
        .put(&scope(), SecretString::from(plaintext.clone()))
        .await
        .unwrap();

    assert_eq!(
        store.get(&secret_ref).await.unwrap().expose_secret(),
        &plaintext
    );
    assert!(matches!(
        store.get(&SecretRef::new()).await,
        Err(StorageError::SecretNotFound)
    ));

    store.delete(&secret_ref).await.unwrap();
    assert!(matches!(
        store.get(&secret_ref).await,
        Err(StorageError::SecretNotFound)
    ));
    store.delete(&secret_ref).await.unwrap();
}

#[tokio::test]
async fn memory_store_debug_output_never_contains_stored_secret() {
    let store = MemorySecretStore::default();
    let plaintext = ["debug", "canary", "secret"].join("-");
    let secret_ref = store
        .put(&scope(), SecretString::from(plaintext.clone()))
        .await
        .unwrap();

    assert!(!format!("{store:?}").contains(&plaintext));
    assert!(!format!("{secret_ref:?}").contains(&plaintext));
}

#[test]
fn headless_stores_reject_the_wrong_explicit_configuration_variant() {
    let file_config = HeadlessSecretStoreConfig::PermissionedFile {
        secret_ref: SecretRef::new(),
        path: "secret.txt".into(),
    };
    let environment_config = HeadlessSecretStoreConfig::Environment {
        secret_ref: SecretRef::new(),
        variable_name: "WOKCORE_SECRET".to_owned(),
    };

    assert!(matches!(
        EnvironmentSecretStore::from_config(file_config),
        Err(StorageError::InvalidHeadlessSecretStoreConfig)
    ));
    assert!(matches!(
        PermissionedFileSecretStore::from_config(environment_config),
        Err(StorageError::InvalidHeadlessSecretStoreConfig)
    ));
}

#[test]
fn invalid_environment_names_and_empty_file_paths_fail_closed() {
    for variable_name in [String::new(), "A=B".to_owned(), "A\0B".to_owned()] {
        assert!(matches!(
            EnvironmentSecretStore::from_config(HeadlessSecretStoreConfig::Environment {
                secret_ref: SecretRef::new(),
                variable_name,
            }),
            Err(StorageError::InvalidHeadlessSecretStoreConfig)
        ));
    }

    assert!(matches!(
        PermissionedFileSecretStore::from_config(HeadlessSecretStoreConfig::PermissionedFile {
            secret_ref: SecretRef::new(),
            path: Path::new("").to_path_buf(),
        }),
        Err(StorageError::InvalidHeadlessSecretStoreConfig)
    ));
}

#[tokio::test]
async fn environment_store_reads_only_its_configured_ref_and_is_read_only() {
    let variable_name = format!("WOKCORE_TEST_SECRET_{}", std::process::id());
    let plaintext = ["configured", "environment", "secret"].join("-");
    let secret_ref = SecretRef::new();
    unsafe {
        std::env::set_var(&variable_name, &plaintext);
    }
    let _cleanup = EnvironmentCleanup(variable_name.clone());
    let store = EnvironmentSecretStore::from_config(HeadlessSecretStoreConfig::Environment {
        secret_ref: secret_ref.clone(),
        variable_name,
    })
    .unwrap();

    assert_eq!(
        store.get(&secret_ref).await.unwrap().expose_secret(),
        &plaintext
    );
    assert!(matches!(
        store.get(&SecretRef::new()).await,
        Err(StorageError::SecretNotFound)
    ));
    assert!(matches!(
        store.put(&scope(), SecretString::from("replacement")).await,
        Err(StorageError::ReadOnlySecretStore)
    ));
    assert!(matches!(
        store.delete(&secret_ref).await,
        Err(StorageError::ReadOnlySecretStore)
    ));
    assert!(matches!(
        store.delete(&SecretRef::new()).await,
        Err(StorageError::SecretNotFound)
    ));
}

struct EnvironmentCleanup(String);

impl Drop for EnvironmentCleanup {
    fn drop(&mut self) {
        unsafe {
            std::env::remove_var(&self.0);
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn environment_store_rejects_values_over_the_named_limit() {
    use wokcore_storage::MAX_HEADLESS_SECRET_BYTES;

    let variable_name = format!("WOKCORE_TEST_OVERSIZED_SECRET_{}", std::process::id());
    let secret_ref = SecretRef::new();
    unsafe {
        std::env::set_var(&variable_name, "x".repeat(MAX_HEADLESS_SECRET_BYTES + 1));
    }
    let _cleanup = EnvironmentCleanup(variable_name.clone());
    let store = EnvironmentSecretStore::from_config(HeadlessSecretStoreConfig::Environment {
        secret_ref: secret_ref.clone(),
        variable_name,
    })
    .unwrap();

    assert!(matches!(
        store.get(&secret_ref).await,
        Err(StorageError::SecretTooLarge)
    ));
}

#[tokio::test]
async fn permissioned_file_store_accepts_only_its_configured_ref_and_is_read_only() {
    let secret_ref = SecretRef::new();
    let store =
        PermissionedFileSecretStore::from_config(HeadlessSecretStoreConfig::PermissionedFile {
            secret_ref: secret_ref.clone(),
            path: "unused-secret-file".into(),
        })
        .unwrap();

    assert!(matches!(
        store.get(&SecretRef::new()).await,
        Err(StorageError::SecretNotFound)
    ));
    assert!(matches!(
        store.put(&scope(), SecretString::from("replacement")).await,
        Err(StorageError::ReadOnlySecretStore)
    ));
    assert!(matches!(
        store.delete(&secret_ref).await,
        Err(StorageError::ReadOnlySecretStore)
    ));
    assert!(matches!(
        store.delete(&SecretRef::new()).await,
        Err(StorageError::SecretNotFound)
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn permissioned_file_store_rejects_modes_broader_than_owner_read_write() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("secret");
    let plaintext = ["file", "secret"].join("-");
    fs::write(&path, &plaintext).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let secret_ref = SecretRef::new();
    let store =
        PermissionedFileSecretStore::from_config(HeadlessSecretStoreConfig::PermissionedFile {
            secret_ref: secret_ref.clone(),
            path: path.clone(),
        })
        .unwrap();

    assert!(matches!(
        store.get(&secret_ref).await,
        Err(StorageError::InsecureSecretFilePermissions)
    ));

    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        store.get(&secret_ref).await.unwrap().expose_secret(),
        &plaintext
    );
}

#[cfg(windows)]
#[tokio::test]
async fn permissioned_file_store_rejects_acls_granting_other_principals() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("secret");
    fs::write(&path, ["file", "secret"].join("-")).unwrap();
    let secret_ref = SecretRef::new();
    let store =
        PermissionedFileSecretStore::from_config(HeadlessSecretStoreConfig::PermissionedFile {
            secret_ref: secret_ref.clone(),
            path,
        })
        .unwrap();

    assert!(matches!(
        store.get(&secret_ref).await,
        Err(StorageError::InsecureSecretFilePermissions)
    ));
}

#[test]
fn native_tests_contain_no_real_credential_operation_calls() {
    let native_source = include_str!("../src/secrets/native.rs");
    let test_source = native_source
        .split_once("#[cfg(test)]")
        .map(|(_, tests)| tests)
        .unwrap_or_default();

    for forbidden_call in [".set_password(", ".get_password(", ".delete_credential("] {
        assert!(
            !test_source.contains(forbidden_call),
            "native unit tests must not call the real OS keyring"
        );
    }
}
