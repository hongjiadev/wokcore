use std::{fs, path::Path, time::SystemTime};

use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};
use uuid::Uuid;
use wokcore_platform::{
    AppPaths, DiscoveryRecord, DiscoveryStore, MAX_DISCOVERY_BYTES, PlatformError, RuntimeLease,
};

#[test]
fn discovery_round_trips_exactly_the_five_public_fields() {
    let fixture = Fixture::new();
    let record = sample_record();

    fixture.store.publish(&record).unwrap();

    assert_eq!(fixture.store.read().unwrap(), record);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&fixture.paths.discovery_file).unwrap()).unwrap(),
        json!({
            "base_url": "http://127.0.0.1:10101",
            "pid": 4242,
            "instance_id": "6f4f9b7d-b5ea-4e5e-9afb-6153ad5db5db",
            "wokcore_version": "0.1.0",
            "api_major": 1
        })
    );
}

#[test]
fn unknown_and_token_like_fields_fail_closed() {
    for field in [
        "unknown",
        "token",
        "admin_token",
        "proxy_token",
        "credential",
        "authorization",
        "headers",
    ] {
        let fixture = Fixture::new();
        let mut document = serde_json::to_value(sample_record()).unwrap();
        document
            .as_object_mut()
            .unwrap()
            .insert(field.to_owned(), json!("canary-secret"));
        fixture.write_raw(&serde_json::to_vec(&document).unwrap());

        assert!(
            matches!(fixture.store.read(), Err(PlatformError::InvalidDiscovery)),
            "field {field:?} must not be accepted"
        );
    }
}

#[test]
fn bounded_read_accepts_exactly_sixteen_kib_and_rejects_one_more_byte() {
    let accepted = Fixture::new();
    let mut exact = serde_json::to_vec(&sample_record()).unwrap();
    exact.resize(MAX_DISCOVERY_BYTES, b' ');
    accepted.write_raw(&exact);
    assert_eq!(accepted.store.read().unwrap(), sample_record());

    let rejected = Fixture::new();
    let mut oversized = serde_json::to_vec(&sample_record()).unwrap();
    oversized.resize(MAX_DISCOVERY_BYTES + 1, b' ');
    rejected.write_raw(&oversized);
    assert!(matches!(
        rejected.store.read(),
        Err(PlatformError::DiscoveryTooLarge)
    ));
}

#[test]
fn invalid_discovery_field_values_fail_closed() {
    let invalid_values = [
        ("pid", json!(0)),
        ("pid", json!(-1)),
        ("api_major", json!(0)),
        ("instance_id", json!("not-a-uuid")),
        ("wokcore_version", json!("not-semver")),
        ("wokcore_version", json!("")),
    ];

    for (field, invalid) in invalid_values {
        let fixture = Fixture::new();
        let mut document = serde_json::to_value(sample_record()).unwrap();
        document
            .as_object_mut()
            .unwrap()
            .insert(field.to_owned(), invalid.clone());
        fixture.write_raw(&serde_json::to_vec(&document).unwrap());

        assert!(
            matches!(fixture.store.read(), Err(PlatformError::InvalidDiscovery)),
            "invalid {field} value {invalid} must not be accepted"
        );
    }
}

#[test]
fn only_explicit_nonzero_ipv4_loopback_http_base_urls_are_accepted() {
    let invalid_urls = [
        "https://127.0.0.1:10101",
        "http://localhost:10101",
        "http://0.0.0.0:10101",
        "http://127.0.0.2:10101",
        "http://[::1]:10101",
        "http://user@127.0.0.1:10101",
        "http://127.0.0.1",
        "http://127.0.0.1:0",
        "http://127.0.0.1:10101/",
        "http://127.0.0.1:10101/wokcore",
        "http://127.0.0.1:10101?query=yes",
        "http://127.0.0.1:10101#fragment",
    ];

    for base_url in invalid_urls {
        let fixture = Fixture::new();
        let mut document = serde_json::to_value(sample_record()).unwrap();
        document
            .as_object_mut()
            .unwrap()
            .insert("base_url".to_owned(), json!(base_url));
        fixture.write_raw(&serde_json::to_vec(&document).unwrap());

        assert!(
            matches!(fixture.store.read(), Err(PlatformError::InvalidDiscovery)),
            "base URL {base_url:?} must not be accepted"
        );
    }
}

#[test]
fn invalid_records_are_rejected_before_publish_mutates_discovery() {
    let fixture = Fixture::new();
    let original = sample_record();
    fixture.store.publish(&original).unwrap();
    let before = snapshot(&fixture.paths.discovery_file);
    let mut invalid = original;
    invalid.pid = 0;

    assert!(matches!(
        fixture.store.publish(&invalid),
        Err(PlatformError::InvalidDiscovery)
    ));
    assert_eq!(snapshot(&fixture.paths.discovery_file), before);
}

#[test]
fn publish_replaces_atomically_without_leaving_temporary_files() {
    let fixture = Fixture::new();
    let first = sample_record();
    let mut replacement = sample_record();
    replacement.pid = 4343;
    replacement.instance_id = Uuid::parse_str("f241dab9-0f79-4124-bb95-2e9d626c99f8").unwrap();

    fixture.store.publish(&first).unwrap();
    fixture.store.publish(&replacement).unwrap();

    assert_eq!(fixture.store.read().unwrap(), replacement);
    assert_eq!(
        directory_entries(&fixture.paths.runtime_dir),
        vec![
            fixture.paths.discovery_file.file_name().unwrap().to_owned(),
            fixture.paths.instance_lock.file_name().unwrap().to_owned(),
        ]
    );
}

#[test]
fn failed_publish_cleans_up_its_same_directory_temporary_file() {
    let fixture = Fixture::new();
    fs::create_dir(&fixture.paths.discovery_file).unwrap();

    assert!(fixture.store.publish(&sample_record()).is_err());
    assert_eq!(
        directory_entries(&fixture.paths.runtime_dir),
        vec![
            fixture.paths.discovery_file.file_name().unwrap().to_owned(),
            fixture.paths.instance_lock.file_name().unwrap().to_owned(),
        ]
    );
}

#[test]
fn reads_and_failed_ownership_checks_do_not_mutate_bytes_or_mtime() {
    let fixture = Fixture::new();
    fixture.store.publish(&sample_record()).unwrap();
    let before = snapshot(&fixture.paths.discovery_file);

    assert_eq!(fixture.store.read().unwrap(), sample_record());
    assert!(!fixture.store.remove_if_owned(Uuid::nil()).unwrap());
    assert_eq!(snapshot(&fixture.paths.discovery_file), before);
}

#[test]
fn owned_removal_deletes_only_the_matching_instance() {
    let fixture = Fixture::new();
    let record = sample_record();
    fixture.store.publish(&record).unwrap();

    assert!(fixture.store.remove_if_owned(record.instance_id).unwrap());
    assert!(!fixture.paths.discovery_file.exists());
}

#[test]
fn owned_removal_preserves_a_replacement_instance() {
    let fixture = Fixture::new();
    let original = sample_record();
    let mut replacement = sample_record();
    replacement.instance_id = Uuid::parse_str("61be0bde-4787-4535-9d1e-bc787813004c").unwrap();
    fixture.store.publish(&original).unwrap();
    fixture.store.publish(&replacement).unwrap();

    assert!(!fixture.store.remove_if_owned(original.instance_id).unwrap());
    assert_eq!(fixture.store.read().unwrap(), replacement);
}

#[test]
fn unsafe_discovery_file_types_and_symlink_replacements_fail_closed() {
    let directory_fixture = Fixture::new();
    fs::create_dir(&directory_fixture.paths.discovery_file).unwrap();
    assert!(matches!(
        directory_fixture.store.read(),
        Err(PlatformError::UnsafeRuntimePath)
    ));

    let symlink_fixture = Fixture::new();
    symlink_fixture.store.publish(&sample_record()).unwrap();
    let moved = symlink_fixture.paths.runtime_dir.join("moved-discovery");
    fs::rename(&symlink_fixture.paths.discovery_file, &moved).unwrap();
    create_file_symlink(&moved, &symlink_fixture.paths.discovery_file);
    assert!(matches!(
        symlink_fixture.store.read(),
        Err(PlatformError::UnsafeRuntimePath)
    ));
}

#[cfg(unix)]
#[test]
fn unix_discovery_uses_owner_only_mode_and_rejects_broader_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    fixture.store.publish(&sample_record()).unwrap();
    assert_eq!(
        fs::metadata(&fixture.paths.discovery_file)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    fs::set_permissions(
        &fixture.paths.discovery_file,
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(matches!(
        fixture.store.read(),
        Err(PlatformError::UnsafeRuntimePath)
    ));
}

#[cfg(windows)]
#[test]
fn windows_discovery_uses_a_protected_current_user_only_dacl() {
    let fixture = Fixture::new();
    fixture.store.publish(&sample_record()).unwrap();

    assert_protected_current_user_only_dacl(&fixture.paths.discovery_file);
    add_world_read_ace(&fixture.paths.discovery_file);
    assert!(matches!(
        fixture.store.read(),
        Err(PlatformError::UnsafeRuntimePath)
    ));
}

struct Fixture {
    _directory: TempDir,
    paths: AppPaths,
    _lease: RuntimeLease,
    store: DiscoveryStore,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempdir().unwrap();
        let paths = test_paths(directory.path());
        let lease = RuntimeLease::acquire(&paths).unwrap();
        let store = DiscoveryStore::new(&paths).unwrap();
        Self {
            _directory: directory,
            paths,
            _lease: lease,
            store,
        }
    }

    fn write_raw(&self, bytes: &[u8]) {
        self.store.publish(&sample_record()).unwrap();
        fs::write(&self.paths.discovery_file, bytes).unwrap();
    }
}

fn sample_record() -> DiscoveryRecord {
    DiscoveryRecord {
        base_url: "http://127.0.0.1:10101".to_owned(),
        pid: 4242,
        instance_id: Uuid::parse_str("6f4f9b7d-b5ea-4e5e-9afb-6153ad5db5db").unwrap(),
        wokcore_version: "0.1.0".to_owned(),
        api_major: 1,
    }
}

fn test_paths(root: &Path) -> AppPaths {
    let runtime_dir = root.join("runtime");
    AppPaths {
        config_file: root.join("config").join("config.toml"),
        state_db: root.join("state").join("state.sqlite3"),
        log_dir: root.join("logs"),
        discovery_file: runtime_dir.join("discovery.json"),
        instance_lock: runtime_dir.join("instance.lock"),
        runtime_dir,
    }
}

fn snapshot(path: &Path) -> (Vec<u8>, SystemTime) {
    (
        fs::read(path).unwrap(),
        fs::metadata(path).unwrap().modified().unwrap(),
    )
}

fn directory_entries(path: &Path) -> Vec<std::ffi::OsString> {
    let mut entries = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

#[cfg(unix)]
fn create_file_symlink(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(windows)]
fn create_file_symlink(target: &Path, link: &Path) {
    std::os::windows::fs::symlink_file(target, link).unwrap();
}

#[cfg(windows)]
fn assert_protected_current_user_only_dacl(path: &Path) {
    let (owner, dacl, descriptor, token_user_buffer) = security_information(path);
    use std::{ffi::c_void, mem::size_of};

    use windows_sys::Win32::{
        Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER, ACL_SIZE_INFORMATION, AclSizeInformation, EqualSid,
            GetAce, GetAclInformation, GetSecurityDescriptorControl, SE_DACL_PROTECTED, TOKEN_USER,
        },
        System::SystemServices::ACCESS_ALLOWED_ACE_TYPE,
    };

    let mut control = 0;
    let mut revision = 0;
    assert_ne!(
        unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) },
        0
    );
    assert_ne!(control & SE_DACL_PROTECTED, 0);

    let token_user = unsafe { &*(token_user_buffer.as_ptr().cast::<TOKEN_USER>()) };
    assert_ne!(unsafe { EqualSid(owner, token_user.User.Sid) }, 0);

    let mut acl_info = ACL_SIZE_INFORMATION::default();
    assert_ne!(
        unsafe {
            GetAclInformation(
                dacl,
                (&mut acl_info as *mut ACL_SIZE_INFORMATION).cast::<c_void>(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        },
        0
    );
    assert_eq!(acl_info.AceCount, 1);
    let mut ace: *mut c_void = std::ptr::null_mut();
    assert_ne!(unsafe { GetAce(dacl, 0, &mut ace) }, 0);
    let header = unsafe { &*ace.cast::<ACE_HEADER>() };
    assert_eq!(header.AceType as u32, ACCESS_ALLOWED_ACE_TYPE);
    let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
    let allowed_sid = (&raw const allowed.SidStart).cast_mut().cast::<c_void>();
    assert_ne!(unsafe { EqualSid(allowed_sid, token_user.User.Sid) }, 0);
}

#[cfg(windows)]
fn add_world_read_ace(path: &Path) {
    use std::{ffi::c_void, mem::size_of, ptr};

    use windows_sys::Win32::{
        Foundation::{ERROR_SUCCESS, GENERIC_READ},
        Security::{
            ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAce,
            Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW},
            CreateWellKnownSid, DACL_SECURITY_INFORMATION, EqualSid, GetLengthSid, InitializeAcl,
            PROTECTED_DACL_SECURITY_INFORMATION, SECURITY_MAX_SID_SIZE, TOKEN_USER, WinWorldSid,
        },
    };

    let (owner, _, _descriptor, token_user_buffer) = security_information(path);
    let token_user = unsafe { &*(token_user_buffer.as_ptr().cast::<TOKEN_USER>()) };
    assert_ne!(unsafe { EqualSid(owner, token_user.User.Sid) }, 0);
    let mut world_size = SECURITY_MAX_SID_SIZE;
    let mut world_buffer = vec![0_usize; (world_size as usize).div_ceil(size_of::<usize>())];
    assert_ne!(
        unsafe {
            CreateWellKnownSid(
                WinWorldSid,
                ptr::null_mut(),
                world_buffer.as_mut_ptr().cast::<c_void>(),
                &mut world_size,
            )
        },
        0
    );
    let world_sid = world_buffer.as_mut_ptr().cast::<c_void>();
    let ace_size = |sid| {
        size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>() + unsafe { GetLengthSid(sid) as usize }
    };
    let acl_size = size_of::<ACL>() + ace_size(token_user.User.Sid) + ace_size(world_sid);
    let mut acl_buffer = vec![0_usize; acl_size.div_ceil(size_of::<usize>())];
    let acl = acl_buffer.as_mut_ptr().cast::<ACL>();
    assert_ne!(
        unsafe { InitializeAcl(acl, acl_size as u32, ACL_REVISION) },
        0
    );
    assert_ne!(
        unsafe { AddAccessAllowedAce(acl, ACL_REVISION, GENERIC_READ, token_user.User.Sid) },
        0
    );
    assert_ne!(
        unsafe { AddAccessAllowedAce(acl, ACL_REVISION, GENERIC_READ, world_sid) },
        0
    );
    let mut wide_path = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let status = unsafe {
        SetNamedSecurityInfoW(
            wide_path.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            acl,
            ptr::null(),
        )
    };
    assert_eq!(status, ERROR_SUCCESS);
}

#[cfg(windows)]
fn security_information(
    path: &Path,
) -> (
    windows_sys::Win32::Security::PSID,
    *mut windows_sys::Win32::Security::ACL,
    LocalSecurityDescriptor,
    Vec<usize>,
) {
    use std::{ffi::c_void, mem::size_of, os::windows::ffi::OsStrExt, ptr};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE},
        Security::{
            ACL,
            Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
            DACL_SECURITY_INFORMATION, GetTokenInformation, OWNER_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY, TokenUser,
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

    let wide_path = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut owner: PSID = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    assert_eq!(status, ERROR_SUCCESS);
    let descriptor = LocalSecurityDescriptor(descriptor);
    assert!(!owner.is_null());
    assert!(!dacl.is_null());

    let mut token_handle: HANDLE = ptr::null_mut();
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token_handle) },
        0
    );
    let token = Token(token_handle);
    let mut required = 0;
    unsafe {
        GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut required);
    }
    assert_ne!(required, 0);
    let mut token_user_buffer = vec![0_usize; (required as usize).div_ceil(size_of::<usize>())];
    assert_ne!(
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                token_user_buffer.as_mut_ptr().cast::<c_void>(),
                required,
                &mut required,
            )
        },
        0
    );
    (owner, dacl, descriptor, token_user_buffer)
}

#[cfg(windows)]
struct LocalSecurityDescriptor(windows_sys::Win32::Security::PSECURITY_DESCRIPTOR);

#[cfg(windows)]
impl Drop for LocalSecurityDescriptor {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::LocalFree;

        if !self.0.is_null() {
            unsafe {
                LocalFree(self.0.cast());
            }
        }
    }
}

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
