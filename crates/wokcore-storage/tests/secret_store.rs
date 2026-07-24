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
async fn permissioned_file_store_accepts_a_protected_user_dacl_and_rejects_other_principals() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("secret");
    let plaintext = ["file", "secret"].join("-");
    fs::write(&path, &plaintext).unwrap();
    let secret_ref = SecretRef::new();
    let store =
        PermissionedFileSecretStore::from_config(HeadlessSecretStoreConfig::PermissionedFile {
            secret_ref: secret_ref.clone(),
            path: path.clone(),
        })
        .unwrap();

    set_protected_secret_file_dacl(&path, false).unwrap();
    assert_eq!(
        store.get(&secret_ref).await.unwrap().expose_secret(),
        &plaintext
    );

    set_protected_secret_file_dacl(&path, true).unwrap();
    assert!(matches!(
        store.get(&secret_ref).await,
        Err(StorageError::InsecureSecretFilePermissions)
    ));
}

#[cfg(windows)]
fn set_protected_secret_file_dacl(path: &Path, include_world: bool) -> std::io::Result<()> {
    use std::{ffi::c_void, iter, mem::size_of, os::windows::ffi::OsStrExt, ptr};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_SUCCESS, GENERIC_ALL, GENERIC_READ, HANDLE},
        Security::{
            ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAce,
            Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW},
            CreateWellKnownSid, DACL_SECURITY_INFORMATION, GetLengthSid, GetTokenInformation,
            InitializeAcl, PROTECTED_DACL_SECURITY_INFORMATION, PSID, SECURITY_MAX_SID_SIZE,
            TOKEN_QUERY, TOKEN_USER, TokenUser, WinWorldSid,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    struct Token(HANDLE);

    impl Drop for Token {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn current_user_token() -> std::io::Result<(Token, Vec<usize>)> {
        let mut token_handle: HANDLE = ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token_handle) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let token = Token(token_handle);
        let mut required = 0;
        unsafe {
            GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut required);
        }
        if required == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut buffer = vec![0_usize; (required as usize).div_ceil(size_of::<usize>())];
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast::<c_void>(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok((token, buffer))
    }

    fn world_sid() -> std::io::Result<Vec<usize>> {
        let mut required = SECURITY_MAX_SID_SIZE;
        let mut buffer = vec![0_usize; (required as usize).div_ceil(size_of::<usize>())];
        if unsafe {
            CreateWellKnownSid(
                WinWorldSid,
                ptr::null_mut(),
                buffer.as_mut_ptr().cast::<c_void>(),
                &mut required,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(buffer)
    }

    let (_token, user_buffer) = current_user_token()?;
    let token_user = unsafe { &*(user_buffer.as_ptr().cast::<TOKEN_USER>()) };
    let user_sid = token_user.User.Sid;
    let world_buffer = include_world.then(world_sid).transpose()?;
    let world_sid: PSID = world_buffer
        .as_ref()
        .map_or(ptr::null_mut(), |buffer| buffer.as_ptr().cast_mut().cast());

    let ace_size = |sid: PSID| {
        size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>() + unsafe { GetLengthSid(sid) as usize }
    };
    let mut acl_size = size_of::<ACL>() + ace_size(user_sid);
    if !world_sid.is_null() {
        acl_size += ace_size(world_sid);
    }
    let mut acl_buffer = vec![0_usize; acl_size.div_ceil(size_of::<usize>())];
    let acl = acl_buffer.as_mut_ptr().cast::<ACL>();
    if unsafe { InitializeAcl(acl, acl_size as u32, ACL_REVISION) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { AddAccessAllowedAce(acl, ACL_REVISION, GENERIC_ALL, user_sid) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if !world_sid.is_null()
        && unsafe { AddAccessAllowedAce(acl, ACL_REVISION, GENERIC_READ, world_sid) } == 0
    {
        return Err(std::io::Error::last_os_error());
    }

    let wide_path: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect();
    let status = unsafe {
        SetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            acl,
            ptr::null(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(status as i32));
    }
    Ok(())
}
