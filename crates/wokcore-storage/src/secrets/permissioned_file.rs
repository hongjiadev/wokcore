use std::{fs::File, io::Read, path::PathBuf};

use secrecy::SecretString;
use wokcore_core::secret::{SecretRef, SecretScope};
use zeroize::Zeroize;

use crate::{HeadlessSecretStoreConfig, MAX_HEADLESS_SECRET_BYTES, SecretStore, StorageError};

#[derive(Clone, Debug)]
pub struct PermissionedFileSecretStore {
    secret_ref: SecretRef,
    path: PathBuf,
}

impl PermissionedFileSecretStore {
    pub fn from_config(config: HeadlessSecretStoreConfig) -> Result<Self, StorageError> {
        let HeadlessSecretStoreConfig::PermissionedFile { secret_ref, path } = config else {
            return Err(StorageError::InvalidHeadlessSecretStoreConfig);
        };
        if path.as_os_str().is_empty() {
            return Err(StorageError::InvalidHeadlessSecretStoreConfig);
        }
        Ok(Self { secret_ref, path })
    }
}

#[async_trait::async_trait]
impl SecretStore for PermissionedFileSecretStore {
    async fn put(
        &self,
        _scope: &SecretScope,
        _value: SecretString,
    ) -> Result<SecretRef, StorageError> {
        Err(StorageError::ReadOnlySecretStore)
    }

    async fn get(&self, secret_ref: &SecretRef) -> Result<SecretString, StorageError> {
        if secret_ref != &self.secret_ref {
            return Err(StorageError::SecretNotFound);
        }
        let mut file = File::open(&self.path).map_err(|source| StorageError::Io { source })?;
        verify_permissions(&file)?;
        let value = read_file_contents(&mut file)?;
        Ok(SecretString::from(value))
    }

    async fn delete(&self, secret_ref: &SecretRef) -> Result<(), StorageError> {
        if secret_ref != &self.secret_ref {
            return Err(StorageError::SecretNotFound);
        }
        Err(StorageError::ReadOnlySecretStore)
    }
}

fn read_file_contents(file: &mut File) -> Result<String, StorageError> {
    let metadata = file
        .metadata()
        .map_err(|source| StorageError::Io { source })?;
    if metadata.len() > MAX_HEADLESS_SECRET_BYTES as u64 {
        return Err(StorageError::SecretTooLarge);
    }
    read_bounded_secret(file, metadata.len() as usize)
}

fn read_bounded_secret(reader: impl Read, capacity_hint: usize) -> Result<String, StorageError> {
    let mut bytes = Vec::with_capacity(capacity_hint.min(MAX_HEADLESS_SECRET_BYTES));
    let mut bounded = reader.take((MAX_HEADLESS_SECRET_BYTES + 1) as u64);
    if let Err(source) = bounded.read_to_end(&mut bytes) {
        bytes.zeroize();
        return Err(StorageError::Io { source });
    }
    if bytes.len() > MAX_HEADLESS_SECRET_BYTES {
        bytes.zeroize();
        return Err(StorageError::SecretTooLarge);
    }
    match String::from_utf8(bytes) {
        Ok(value) => Ok(value),
        Err(error) => {
            let mut bytes = error.into_bytes();
            zeroize_invalid_secret_bytes(&mut bytes);
            Err(StorageError::InvalidSecretEncoding)
        }
    }
}

fn zeroize_invalid_secret_bytes(bytes: &mut [u8]) {
    bytes.zeroize();
    #[cfg(test)]
    INVALID_UTF8_ZEROIZE_OBSERVER.with(|observer| {
        if let Some(observer) = observer.borrow().as_ref() {
            observer(bytes);
        }
    });
}

#[cfg(test)]
type InvalidUtf8ZeroizeObserver = Box<dyn Fn(&[u8])>;

#[cfg(test)]
thread_local! {
    static INVALID_UTF8_ZEROIZE_OBSERVER:
        std::cell::RefCell<Option<InvalidUtf8ZeroizeObserver>> =
            const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn with_invalid_utf8_zeroize_observer<T>(
    observer: impl Fn(&[u8]) + 'static,
    operation: impl FnOnce() -> T,
) -> T {
    INVALID_UTF8_ZEROIZE_OBSERVER.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(Box::new(observer));
    });

    struct ResetObserver;

    impl Drop for ResetObserver {
        fn drop(&mut self) {
            INVALID_UTF8_ZEROIZE_OBSERVER.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }

    let _reset = ResetObserver;
    operation()
}

#[cfg(unix)]
fn verify_permissions(file: &File) -> Result<(), StorageError> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = file
        .metadata()
        .map_err(|source| StorageError::Io { source })?;
    if metadata.permissions().mode() & 0o7177 != 0 {
        return Err(StorageError::InsecureSecretFilePermissions);
    }
    Ok(())
}

#[cfg(windows)]
fn verify_permissions(file: &File) -> Result<(), StorageError> {
    use std::{ffi::c_void, mem::size_of, os::windows::io::AsRawHandle, ptr};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree},
        Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
            Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
            DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation, GetTokenInformation,
            OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY, TOKEN_USER,
            TokenUser,
        },
        System::{
            SystemServices::ACCESS_ALLOWED_ACE_TYPE,
            Threading::{GetCurrentProcess, OpenProcessToken},
        },
    };

    let mut owner: PSID = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    let descriptor = SecurityDescriptor(descriptor);
    if status != ERROR_SUCCESS {
        return Err(StorageError::Io {
            source: std::io::Error::from_raw_os_error(status as i32),
        });
    }
    if owner.is_null() || dacl.is_null() {
        return Err(StorageError::InsecureSecretFilePermissions);
    }

    let (token, token_user_buffer) = current_user_token()?;
    let token_user = unsafe { &*(token_user_buffer.as_ptr().cast::<TOKEN_USER>()) };
    if unsafe { EqualSid(owner, token_user.User.Sid) } == 0 {
        return Err(StorageError::InsecureSecretFilePermissions);
    }

    let mut acl_info = ACL_SIZE_INFORMATION::default();
    let acl_info_ok = unsafe {
        GetAclInformation(
            dacl,
            (&mut acl_info as *mut ACL_SIZE_INFORMATION).cast::<c_void>(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if acl_info_ok == 0 {
        return Err(StorageError::Io {
            source: std::io::Error::last_os_error(),
        });
    }

    for index in 0..acl_info.AceCount {
        let mut ace: *mut c_void = ptr::null_mut();
        if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
            return Err(StorageError::Io {
                source: std::io::Error::last_os_error(),
            });
        }
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        match header.AceType as u32 {
            ACCESS_ALLOWED_ACE_TYPE => {
                let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
                let sid = (&raw const allowed.SidStart).cast_mut().cast::<c_void>();
                if unsafe { EqualSid(sid, token_user.User.Sid) } == 0 {
                    return Err(StorageError::InsecureSecretFilePermissions);
                }
            }
            ace_type if ace_type_is_non_granting(ace_type) => {}
            _ => return Err(StorageError::InsecureSecretFilePermissions),
        }
    }

    struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

    impl Drop for SecurityDescriptor {
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

    fn current_user_token() -> Result<(Token, Vec<usize>), StorageError> {
        let mut token_handle: HANDLE = ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token_handle) } == 0 {
            return Err(StorageError::Io {
                source: std::io::Error::last_os_error(),
            });
        }
        let token = Token(token_handle);
        let mut required = 0;
        unsafe {
            GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut required);
        }
        if required == 0 {
            return Err(StorageError::Io {
                source: std::io::Error::last_os_error(),
            });
        }
        let word_count = (required as usize).div_ceil(size_of::<usize>());
        let mut buffer = vec![0_usize; word_count];
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
            return Err(StorageError::Io {
                source: std::io::Error::last_os_error(),
            });
        }
        Ok((token, buffer))
    }

    drop(token);
    drop(descriptor);
    Ok(())
}

#[cfg(windows)]
fn ace_type_is_non_granting(ace_type: u32) -> bool {
    use windows_sys::Win32::System::SystemServices::{
        ACCESS_DENIED_ACE_TYPE, ACCESS_DENIED_CALLBACK_ACE_TYPE,
        ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE, ACCESS_DENIED_OBJECT_ACE_TYPE,
        SYSTEM_ACCESS_FILTER_ACE_TYPE, SYSTEM_ALARM_ACE_TYPE, SYSTEM_ALARM_CALLBACK_ACE_TYPE,
        SYSTEM_ALARM_CALLBACK_OBJECT_ACE_TYPE, SYSTEM_ALARM_OBJECT_ACE_TYPE, SYSTEM_AUDIT_ACE_TYPE,
        SYSTEM_AUDIT_CALLBACK_ACE_TYPE, SYSTEM_AUDIT_CALLBACK_OBJECT_ACE_TYPE,
        SYSTEM_AUDIT_OBJECT_ACE_TYPE, SYSTEM_MANDATORY_LABEL_ACE_TYPE,
        SYSTEM_PROCESS_TRUST_LABEL_ACE_TYPE, SYSTEM_RESOURCE_ATTRIBUTE_ACE_TYPE,
        SYSTEM_SCOPED_POLICY_ID_ACE_TYPE,
    };

    matches!(
        ace_type,
        ACCESS_DENIED_ACE_TYPE
            | ACCESS_DENIED_CALLBACK_ACE_TYPE
            | ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE
            | ACCESS_DENIED_OBJECT_ACE_TYPE
            | SYSTEM_ACCESS_FILTER_ACE_TYPE
            | SYSTEM_ALARM_ACE_TYPE
            | SYSTEM_ALARM_CALLBACK_ACE_TYPE
            | SYSTEM_ALARM_CALLBACK_OBJECT_ACE_TYPE
            | SYSTEM_ALARM_OBJECT_ACE_TYPE
            | SYSTEM_AUDIT_ACE_TYPE
            | SYSTEM_AUDIT_CALLBACK_ACE_TYPE
            | SYSTEM_AUDIT_CALLBACK_OBJECT_ACE_TYPE
            | SYSTEM_AUDIT_OBJECT_ACE_TYPE
            | SYSTEM_MANDATORY_LABEL_ACE_TYPE
            | SYSTEM_PROCESS_TRUST_LABEL_ACE_TYPE
            | SYSTEM_RESOURCE_ATTRIBUTE_ACE_TYPE
            | SYSTEM_SCOPED_POLICY_ID_ACE_TYPE
    )
}

#[cfg(not(any(unix, windows)))]
fn verify_permissions(_file: &File) -> Result<(), StorageError> {
    Err(StorageError::InsecureSecretFilePermissions)
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        fs,
        fs::File,
        io::{Cursor, Seek},
        rc::Rc,
    };

    use super::{read_bounded_secret, read_file_contents, with_invalid_utf8_zeroize_observer};
    use crate::{MAX_HEADLESS_SECRET_BYTES, StorageError};

    #[test]
    fn bounded_reader_accepts_the_exact_limit_and_rejects_one_more_byte() {
        let mut accepted = Cursor::new(vec![b'x'; MAX_HEADLESS_SECRET_BYTES]);
        let value = read_bounded_secret(&mut accepted, 0).unwrap();
        let mut rejected = Cursor::new(vec![b'x'; MAX_HEADLESS_SECRET_BYTES + 1]);

        assert_eq!(value.len(), MAX_HEADLESS_SECRET_BYTES);
        assert!(matches!(
            read_bounded_secret(&mut rejected, 0),
            Err(StorageError::SecretTooLarge)
        ));
    }

    #[test]
    fn bounded_reader_stops_after_limit_plus_one_byte() {
        let mut reader = Cursor::new(vec![b'x'; MAX_HEADLESS_SECRET_BYTES + 2]);

        assert!(matches!(
            read_bounded_secret(&mut reader, 0),
            Err(StorageError::SecretTooLarge)
        ));
        assert_eq!(
            reader.stream_position().unwrap(),
            (MAX_HEADLESS_SECRET_BYTES + 1) as u64
        );
    }

    #[test]
    fn metadata_rejects_oversized_file_before_reading() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("secret");
        fs::write(&path, vec![b'x'; MAX_HEADLESS_SECRET_BYTES + 1]).unwrap();
        let mut file = File::open(path).unwrap();

        assert!(matches!(
            read_file_contents(&mut file),
            Err(StorageError::SecretTooLarge)
        ));
    }

    #[test]
    fn invalid_utf8_read_error_exposes_only_a_zeroized_buffer_to_the_test_observer() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("secret");
        fs::write(&path, [0xff]).unwrap();
        let mut file = File::open(path).unwrap();
        let observed_zeroized = Rc::new(Cell::new(false));
        let observer_result = Rc::clone(&observed_zeroized);

        let result = with_invalid_utf8_zeroize_observer(
            move |bytes| observer_result.set(bytes.iter().all(|byte| *byte == 0)),
            || read_file_contents(&mut file),
        );

        assert!(matches!(result, Err(StorageError::InvalidSecretEncoding)));
        assert!(observed_zeroized.get());
    }

    #[test]
    fn already_open_secret_file_is_not_replaced_by_a_path_swap() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("secret");
        let original = ["original", "value"].join("-");
        fs::write(&path, &original).unwrap();
        let mut file = File::open(&path).unwrap();
        fs::rename(&path, directory.path().join("original")).unwrap();
        fs::write(&path, ["replacement", "value"].join("-")).unwrap();

        assert_eq!(read_file_contents(&mut file).unwrap(), original);
    }

    #[cfg(windows)]
    #[test]
    fn unknown_or_compound_ace_types_fail_closed() {
        use windows_sys::Win32::System::SystemServices::{
            ACCESS_ALLOWED_COMPOUND_ACE_TYPE, ACCESS_DENIED_ACE_TYPE,
        };

        assert!(super::ace_type_is_non_granting(ACCESS_DENIED_ACE_TYPE));
        assert!(!super::ace_type_is_non_granting(
            ACCESS_ALLOWED_COMPOUND_ACE_TYPE
        ));
        assert!(!super::ace_type_is_non_granting(u8::MAX as u32));
    }
}
