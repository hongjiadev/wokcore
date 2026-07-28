#[cfg(windows)]
pub fn is_process_running(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, STILL_ACTIVE},
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };

    if pid == 0 {
        return false;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut exit_code = 0;
    let queried = unsafe { GetExitCodeProcess(handle, &mut exit_code) } != 0;
    unsafe {
        CloseHandle(handle);
    }
    queried && exit_code == STILL_ACTIVE as u32
}

#[cfg(windows)]
pub fn process_matches_executable(pid: u32, expected: &std::path::Path) -> bool {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};

    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
        },
    };

    if pid == 0 || !expected.is_absolute() {
        return false;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let result = (|| {
        let mut buffer = vec![0_u16; 32_768];
        let mut length = buffer.len() as u32;
        if unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &raw mut length) }
            == 0
        {
            return false;
        }
        buffer.truncate(length as usize);
        let actual = std::path::PathBuf::from(OsString::from_wide(&buffer));
        same_windows_path(&actual, expected)
    })();
    unsafe {
        CloseHandle(handle);
    }
    result
}

#[cfg(windows)]
fn same_windows_path(left: &std::path::Path, right: &std::path::Path) -> bool {
    fn normalized(path: &std::path::Path) -> Option<String> {
        Some(
            std::fs::canonicalize(path)
                .ok()?
                .to_string_lossy()
                .replace('/', "\\")
                .trim_start_matches(r"\\?\")
                .to_owned(),
        )
    }

    normalized(left)
        .zip(normalized(right))
        .is_some_and(|(left, right)| left.eq_ignore_ascii_case(&right))
}

#[cfg(unix)]
pub fn is_process_running(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(target_os = "linux")]
pub fn process_matches_executable(pid: u32, expected: &std::path::Path) -> bool {
    if pid == 0 || !expected.is_absolute() {
        return false;
    }
    exact_canonical_path(
        &std::path::PathBuf::from(format!("/proc/{pid}/exe")),
        expected,
    )
}

#[cfg(target_os = "macos")]
pub fn process_matches_executable(pid: u32, expected: &std::path::Path) -> bool {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

    if pid == 0 || pid > i32::MAX as u32 || !expected.is_absolute() {
        return false;
    }
    let mut buffer = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let length = unsafe {
        libc::proc_pidpath(
            pid as i32,
            buffer.as_mut_ptr().cast(),
            u32::try_from(buffer.len()).unwrap_or(u32::MAX),
        )
    };
    if length <= 0 {
        return false;
    }
    buffer.truncate(length as usize);
    exact_canonical_path(std::path::Path::new(OsStr::from_bytes(&buffer)), expected)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub fn process_matches_executable(_pid: u32, _expected: &std::path::Path) -> bool {
    false
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn exact_canonical_path(left: &std::path::Path, right: &std::path::Path) -> bool {
    std::fs::canonicalize(left)
        .ok()
        .zip(std::fs::canonicalize(right).ok())
        .is_some_and(|(left, right)| left == right)
}
