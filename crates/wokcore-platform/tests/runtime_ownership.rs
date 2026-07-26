use std::{
    env, fs,
    path::Path,
    process::Command,
    sync::{Arc, Barrier, mpsc},
    thread,
    time::{Duration, Instant},
};

use tempfile::tempdir;
use wokcore_platform::{AppPaths, PlatformError, RuntimeLease};

#[test]
fn simultaneous_acquisitions_produce_one_owner_and_one_already_running() {
    let directory = tempdir().unwrap();
    let paths = test_paths(directory.path());
    let start = Arc::new(Barrier::new(3));
    let (sender, receiver) = mpsc::channel();
    let mut threads = Vec::new();

    for _ in 0..2 {
        let paths = paths.clone();
        let start = Arc::clone(&start);
        let sender = sender.clone();
        threads.push(thread::spawn(move || {
            start.wait();
            sender.send(RuntimeLease::acquire(&paths)).unwrap();
        }));
    }

    start.wait();
    let results = [receiver.recv().unwrap(), receiver.recv().unwrap()];
    let owners = results.iter().filter(|result| result.is_ok()).count();
    let already_running = results
        .iter()
        .filter(|result| matches!(result, Err(PlatformError::AlreadyRunning)))
        .count();

    assert_eq!(owners, 1);
    assert_eq!(already_running, 1);

    drop(results);
    for thread in threads {
        thread.join().unwrap();
    }
}

#[test]
fn existing_only_acquisition_never_creates_runtime_state() {
    let directory = tempdir().unwrap();
    let paths = test_paths(&directory.path().join("missing"));

    assert!(matches!(
        RuntimeLease::acquire_existing(&paths),
        Err(PlatformError::UnsafeRuntimePath)
    ));
    assert!(!paths.runtime_dir.exists());
    assert!(!paths.instance_lock.exists());
}

#[test]
fn existing_only_acquisition_is_byte_and_mtime_read_only() {
    let directory = tempdir().unwrap();
    let paths = test_paths(directory.path());
    drop(RuntimeLease::acquire(&paths).unwrap());
    let before = tree_snapshot(directory.path());

    drop(RuntimeLease::acquire_existing(&paths).unwrap());

    assert_eq!(tree_snapshot(directory.path()), before);
}

#[test]
fn existing_only_acquisition_observes_the_current_owner() {
    let directory = tempdir().unwrap();
    let paths = test_paths(directory.path());
    let owner = RuntimeLease::acquire(&paths).unwrap();

    assert!(matches!(
        RuntimeLease::acquire_existing(&paths),
        Err(PlatformError::AlreadyRunning)
    ));

    drop(owner);
}

#[cfg(windows)]
#[test]
fn existing_only_lease_prevents_replacing_its_windows_lock_file() {
    let directory = tempdir().unwrap();
    let paths = test_paths(directory.path());
    drop(RuntimeLease::acquire(&paths).unwrap());
    let lease = RuntimeLease::acquire_existing(&paths).unwrap();

    assert!(fs::remove_file(&paths.instance_lock).is_err());

    drop(lease);
    fs::remove_file(&paths.instance_lock).unwrap();
}

#[test]
fn separate_processes_contend_for_the_same_operating_system_lock() {
    let directory = tempdir().unwrap();
    let paths = test_paths(directory.path());
    let ready = directory.path().join("holder-ready");
    let release = directory.path().join("release-holder");
    let mut holder = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "cross_process_lease_holder_helper",
            "--ignored",
            "--nocapture",
        ])
        .env("WOKCORE_TEST_RUNTIME_ROOT", directory.path())
        .env("WOKCORE_TEST_HOLDER_READY", &ready)
        .env("WOKCORE_TEST_HOLDER_RELEASE", &release)
        .spawn()
        .unwrap();

    wait_until_exists(&ready, Duration::from_secs(10));
    let contender = RuntimeLease::acquire(&paths);
    fs::write(&release, b"release").unwrap();
    assert!(holder.wait().unwrap().success());
    assert!(matches!(contender, Err(PlatformError::AlreadyRunning)));
    drop(RuntimeLease::acquire(&paths).unwrap());
}

#[test]
#[ignore = "spawned only by separate_processes_contend_for_the_same_operating_system_lock"]
fn cross_process_lease_holder_helper() {
    let Some(root) = env::var_os("WOKCORE_TEST_RUNTIME_ROOT") else {
        return;
    };
    let ready = env::var_os("WOKCORE_TEST_HOLDER_READY").unwrap();
    let release = env::var_os("WOKCORE_TEST_HOLDER_RELEASE").unwrap();
    let lease = RuntimeLease::acquire(&test_paths(Path::new(&root))).unwrap();
    fs::write(&ready, b"ready").unwrap();
    wait_until_exists(Path::new(&release), Duration::from_secs(10));
    drop(lease);
}

#[cfg(unix)]
#[test]
fn exec_child_does_not_extend_the_runtime_lease_lifetime() {
    let directory = tempdir().unwrap();
    let paths = test_paths(directory.path());
    let owner = RuntimeLease::acquire(&paths).unwrap();
    let ready = directory.path().join("exec-child-ready");
    let release = directory.path().join("release-exec-child");
    let mut child = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "exec_waiting_child_helper",
            "--ignored",
            "--nocapture",
        ])
        .env("WOKCORE_TEST_EXEC_CHILD_READY", &ready)
        .env("WOKCORE_TEST_EXEC_CHILD_RELEASE", &release)
        .spawn()
        .unwrap();

    wait_until_exists(&ready, Duration::from_secs(10));
    drop(owner);
    let replacement = RuntimeLease::acquire(&paths);
    fs::write(&release, b"release").unwrap();
    assert!(child.wait().unwrap().success());
    drop(replacement.expect("an exec child must not inherit and extend the runtime lock"));
}

#[cfg(unix)]
#[test]
fn replacing_the_runtime_directory_path_does_not_create_a_second_lock_domain() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempdir().unwrap();
    let paths = test_paths(directory.path());
    let moved_runtime = directory.path().join("original-runtime");
    let owner = RuntimeLease::acquire(&paths).unwrap();

    fs::rename(&paths.runtime_dir, &moved_runtime).unwrap();
    fs::create_dir(&paths.runtime_dir).unwrap();
    fs::set_permissions(&paths.runtime_dir, fs::Permissions::from_mode(0o700)).unwrap();

    assert!(matches!(
        RuntimeLease::acquire(&paths),
        Err(PlatformError::AlreadyRunning)
    ));
    drop(owner);
}

#[cfg(unix)]
#[test]
fn namespace_lock_is_a_fixed_owner_only_file_in_the_secure_runtime_parent() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempdir().unwrap();
    let paths = test_paths(directory.path());
    let namespace_lock = directory.path().join(".wokcore-runtime-namespace.lock");
    let owner = RuntimeLease::acquire(&paths).unwrap();

    let metadata = fs::symlink_metadata(&namespace_lock).unwrap();
    assert!(metadata.is_file());
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    drop(owner);
    assert!(namespace_lock.is_file());
}

#[cfg(unix)]
#[test]
fn runtime_parent_must_be_private_to_the_current_user() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempdir().unwrap();
    let paths = test_paths(directory.path());
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();

    assert!(matches!(
        RuntimeLease::acquire(&paths),
        Err(PlatformError::UnsafeRuntimePath)
    ));
}

#[cfg(unix)]
#[test]
fn different_runtime_parents_have_independent_lock_domains() {
    let directory = tempdir().unwrap();
    let first = RuntimeLease::acquire(&test_paths(&directory.path().join("first"))).unwrap();
    let second = RuntimeLease::acquire(&test_paths(&directory.path().join("second"))).unwrap();

    drop((first, second));
}

#[cfg(unix)]
#[test]
#[ignore = "spawned only by exec_child_does_not_extend_the_runtime_lease_lifetime"]
fn exec_waiting_child_helper() {
    let ready = env::var_os("WOKCORE_TEST_EXEC_CHILD_READY").unwrap();
    let release = env::var_os("WOKCORE_TEST_EXEC_CHILD_RELEASE").unwrap();
    fs::write(&ready, b"ready").unwrap();
    wait_until_exists(Path::new(&release), Duration::from_secs(10));
}

#[test]
fn dropping_the_owner_releases_the_operating_system_lock() {
    let directory = tempdir().unwrap();
    let paths = test_paths(directory.path());
    let owner = RuntimeLease::acquire(&paths).unwrap();

    assert!(matches!(
        RuntimeLease::acquire(&paths),
        Err(PlatformError::AlreadyRunning)
    ));

    drop(owner);
    let replacement = RuntimeLease::acquire(&paths)
        .expect("the operating-system lock must be released with the lease");
    drop(replacement);
}

#[test]
fn stale_lock_file_contents_do_not_claim_or_grant_ownership() {
    let directory = tempdir().unwrap();
    let paths = test_paths(directory.path());
    drop(RuntimeLease::acquire(&paths).unwrap());
    fs::write(&paths.instance_lock, b"stale-pid-that-is-not-ownership").unwrap();

    let owner =
        RuntimeLease::acquire(&paths).expect("stale text must not prevent acquiring the real lock");
    assert!(matches!(
        RuntimeLease::acquire(&paths),
        Err(PlatformError::AlreadyRunning)
    ));
    drop(owner);
    assert_eq!(
        fs::read(&paths.instance_lock).unwrap(),
        b"stale-pid-that-is-not-ownership"
    );
}

#[test]
fn existing_lock_symlink_or_reparse_target_fails_closed() {
    let directory = tempdir().unwrap();
    let paths = test_paths(directory.path());
    drop(RuntimeLease::acquire(&paths).unwrap());
    let moved_lock = paths.runtime_dir.join("moved-instance.lock");
    fs::rename(&paths.instance_lock, &moved_lock).unwrap();
    create_file_symlink(&moved_lock, &paths.instance_lock);

    assert!(matches!(
        RuntimeLease::acquire(&paths),
        Err(PlatformError::UnsafeRuntimePath)
    ));
}

#[test]
fn runtime_acquisition_rejects_a_non_directory_target() {
    let directory = tempdir().unwrap();
    let paths = test_paths(directory.path());
    fs::write(&paths.runtime_dir, b"not a directory").unwrap();

    assert!(matches!(
        RuntimeLease::acquire(&paths),
        Err(PlatformError::UnsafeRuntimePath)
    ));
}

#[test]
fn runtime_acquisition_rejects_a_symlink_or_reparse_target() {
    let directory = tempdir().unwrap();
    let paths = test_paths(directory.path());
    let target = directory.path().join("runtime-target");
    fs::create_dir(&target).unwrap();
    create_directory_symlink(&target, &paths.runtime_dir);

    assert!(matches!(
        RuntimeLease::acquire(&paths),
        Err(PlatformError::UnsafeRuntimePath)
    ));
}

#[test]
fn runtime_acquisition_touches_only_the_runtime_directory_and_lock() {
    let directory = tempdir().unwrap();
    let paths = test_paths(directory.path());
    let owner = RuntimeLease::acquire(&paths).unwrap();

    assert!(paths.runtime_dir.is_dir());
    assert!(paths.instance_lock.is_file());
    assert!(!paths.config_file.exists());
    assert!(!paths.state_db.exists());
    assert!(!paths.log_dir.exists());
    assert!(!paths.discovery_file.exists());
    #[cfg(not(unix))]
    assert_eq!(
        directory_entries(directory.path()),
        vec![paths.runtime_dir.file_name().unwrap().to_owned()]
    );
    #[cfg(unix)]
    assert_eq!(
        directory_entries(directory.path()),
        vec![
            ".wokcore-runtime-namespace.lock".into(),
            paths.runtime_dir.file_name().unwrap().to_owned(),
        ]
    );
    drop(owner);
}

#[cfg(unix)]
#[test]
fn unix_runtime_and_lock_have_owner_only_modes() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempdir().unwrap();
    let paths = test_paths(directory.path());
    let owner = RuntimeLease::acquire(&paths).unwrap();

    assert_eq!(
        fs::metadata(&paths.runtime_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&paths.instance_lock)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    drop(owner);
}

#[cfg(windows)]
#[test]
fn windows_runtime_and_lock_use_protected_current_user_only_dacls() {
    let directory = tempdir().unwrap();
    let paths = test_paths(directory.path());
    let owner = RuntimeLease::acquire(&paths).unwrap();

    assert_protected_current_user_only_dacl(&paths.runtime_dir);
    assert_protected_current_user_only_dacl(&paths.instance_lock);
    drop(owner);
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

fn directory_entries(path: &Path) -> Vec<std::ffi::OsString> {
    let mut entries = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

fn wait_until_exists(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn tree_snapshot(root: &Path) -> Vec<(String, Option<Vec<u8>>, std::time::SystemTime)> {
    fn visit(
        root: &Path,
        path: &Path,
        entries: &mut Vec<(String, Option<Vec<u8>>, std::time::SystemTime)>,
    ) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let metadata = entry.metadata().unwrap();
            let relative = entry
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if metadata.is_dir() {
                entries.push((relative, None, metadata.modified().unwrap()));
                visit(root, &entry.path(), entries);
            } else {
                entries.push((
                    relative,
                    Some(fs::read(entry.path()).unwrap()),
                    metadata.modified().unwrap(),
                ));
            }
        }
    }

    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries
}

#[cfg(unix)]
fn create_directory_symlink(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(windows)]
fn create_directory_symlink(target: &Path, link: &Path) {
    std::os::windows::fs::symlink_dir(target, link).unwrap();
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
    use std::{ffi::c_void, iter, mem::size_of, os::windows::ffi::OsStrExt, ptr};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree},
        Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
            Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
            DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
            GetSecurityDescriptorControl, GetTokenInformation, OWNER_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        System::{
            SystemServices::ACCESS_ALLOWED_ACE_TYPE,
            Threading::{GetCurrentProcess, OpenProcessToken},
        },
    };

    struct LocalSecurityDescriptor(PSECURITY_DESCRIPTOR);

    impl Drop for LocalSecurityDescriptor {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    LocalFree(self.0.cast());
                }
            }
        }
    }

    struct Token(HANDLE);

    impl Drop for Token {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn current_user_token() -> (Token, Vec<usize>) {
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
        let mut buffer = vec![0_usize; (required as usize).div_ceil(size_of::<usize>())];
        assert_ne!(
            unsafe {
                GetTokenInformation(
                    token.0,
                    TokenUser,
                    buffer.as_mut_ptr().cast::<c_void>(),
                    required,
                    &mut required,
                )
            },
            0
        );
        (token, buffer)
    }

    let wide_path = path
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
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

    let mut control = 0;
    let mut revision = 0;
    assert_ne!(
        unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) },
        0
    );
    assert_ne!(control & SE_DACL_PROTECTED, 0);

    let (_token, token_user_buffer) = current_user_token();
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

    let mut ace: *mut c_void = ptr::null_mut();
    assert_ne!(unsafe { GetAce(dacl, 0, &mut ace) }, 0);
    let header = unsafe { &*ace.cast::<ACE_HEADER>() };
    assert_eq!(header.AceType as u32, ACCESS_ALLOWED_ACE_TYPE);
    let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
    let allowed_sid = (&raw const allowed.SidStart).cast_mut().cast::<c_void>();
    assert_ne!(unsafe { EqualSid(allowed_sid, token_user.User.Sid) }, 0);
}
