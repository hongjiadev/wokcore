use serde::Serialize;
use thiserror::Error;

#[derive(Clone, Copy, Debug)]
pub struct ProcessMetricValues {
    pub pid: u32,
    pub identity_token: u64,
    pub observed_ms: u64,
    pub private_working_set_bytes: u64,
    pub peak_private_bytes: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub handle_count: u32,
    pub thread_count: u32,
    pub lifetime_ms: u64,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct ProcessMetricSample {
    pid: u32,
    #[serde(skip)]
    identity_token: u64,
    observed_ms: u64,
    private_working_set_bytes: u64,
    peak_private_bytes: u64,
    read_bytes: u64,
    write_bytes: u64,
    handle_count: u32,
    thread_count: u32,
    lifetime_ms: u64,
}

impl ProcessMetricSample {
    #[must_use]
    pub const fn from_values(values: ProcessMetricValues) -> Self {
        Self {
            pid: values.pid,
            identity_token: values.identity_token,
            observed_ms: values.observed_ms,
            private_working_set_bytes: values.private_working_set_bytes,
            peak_private_bytes: values.peak_private_bytes,
            read_bytes: values.read_bytes,
            write_bytes: values.write_bytes,
            handle_count: values.handle_count,
            thread_count: values.thread_count,
            lifetime_ms: values.lifetime_ms,
        }
    }

    #[must_use]
    pub const fn pid(self) -> u32 {
        self.pid
    }

    #[must_use]
    pub const fn identity_token(self) -> u64 {
        self.identity_token
    }

    #[must_use]
    pub const fn observed_ms(self) -> u64 {
        self.observed_ms
    }

    #[must_use]
    pub const fn private_working_set_bytes(self) -> u64 {
        self.private_working_set_bytes
    }

    #[must_use]
    pub const fn peak_private_bytes(self) -> u64 {
        self.peak_private_bytes
    }

    #[must_use]
    pub const fn read_bytes(self) -> u64 {
        self.read_bytes
    }

    #[must_use]
    pub const fn write_bytes(self) -> u64 {
        self.write_bytes
    }

    #[must_use]
    pub const fn handle_count(self) -> u32 {
        self.handle_count
    }

    #[must_use]
    pub const fn thread_count(self) -> u32 {
        self.thread_count
    }

    #[must_use]
    pub const fn lifetime_ms(self) -> u64 {
        self.lifetime_ms
    }
}

#[derive(Debug)]
pub struct ProcessMetricValidator {
    pid: u32,
    identity_token: u64,
    previous: Option<ProcessMetricSample>,
}

impl ProcessMetricValidator {
    #[must_use]
    pub const fn new(pid: u32, identity_token: u64) -> Self {
        Self {
            pid,
            identity_token,
            previous: None,
        }
    }

    pub fn push(&mut self, sample: &ProcessMetricSample) -> Result<(), ProcessMetricError> {
        if sample.pid != self.pid || sample.identity_token != self.identity_token {
            return Err(ProcessMetricError::IdentityChanged);
        }
        if let Some(previous) = self.previous {
            if sample.observed_ms < previous.observed_ms
                || sample.lifetime_ms < previous.lifetime_ms
            {
                return Err(ProcessMetricError::TimeReversed);
            }
            if sample.read_bytes < previous.read_bytes || sample.write_bytes < previous.write_bytes
            {
                return Err(ProcessMetricError::CounterRollback);
            }
        }
        self.previous = Some(*sample);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ProcessMetricError {
    #[error("process identity changed while sampling")]
    IdentityChanged,
    #[error("process counters moved backwards")]
    CounterRollback,
    #[error("process sample time moved backwards")]
    TimeReversed,
    #[error("process executable does not match the exact expected path")]
    ExecutableMismatch,
    #[error("process metrics are unavailable")]
    Unavailable,
}

#[cfg(windows)]
mod windows {
    use std::{
        ffi::OsString,
        mem::size_of,
        os::windows::ffi::OsStringExt,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX2},
            Threading::{
                GetProcessHandleCount, GetProcessIoCounters, GetProcessTimes, IO_COUNTERS,
                OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
                QueryFullProcessImageNameW,
            },
        },
    };

    use super::{ProcessMetricError, ProcessMetricSample, ProcessMetricValues};

    const FILETIME_UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    const FILETIME_TICKS_PER_MILLISECOND: u64 = 10_000;
    const MAX_IMAGE_PATH_UNITS: usize = 32_768;

    pub struct WindowsProcessSampler {
        handle: HANDLE,
        pid: u32,
        identity_token: u64,
        creation_unix_ms: u64,
    }

    impl std::fmt::Debug for WindowsProcessSampler {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("WindowsProcessSampler")
                .field("pid", &self.pid)
                .field("identity_token", &self.identity_token)
                .finish()
        }
    }

    impl WindowsProcessSampler {
        pub fn open(pid: u32, expected_executable: &Path) -> Result<Self, ProcessMetricError> {
            if pid == 0 || !expected_executable.is_absolute() {
                return Err(ProcessMetricError::Unavailable);
            }
            let handle =
                unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid) };
            if handle.is_null() {
                return Err(ProcessMetricError::Unavailable);
            }
            let result = (|| {
                let actual = query_image_path(handle)?;
                if !same_windows_path(&actual, expected_executable) {
                    return Err(ProcessMetricError::ExecutableMismatch);
                }
                let identity_token = creation_ticks(handle)?;
                let creation_unix_ms = filetime_to_unix_ms(identity_token)?;
                Ok(Self {
                    handle,
                    pid,
                    identity_token,
                    creation_unix_ms,
                })
            })();
            if result.is_err() {
                unsafe {
                    CloseHandle(handle);
                }
            }
            result
        }

        pub fn sample(&self) -> Result<ProcessMetricSample, ProcessMetricError> {
            if creation_ticks(self.handle)? != self.identity_token {
                return Err(ProcessMetricError::IdentityChanged);
            }
            let mut memory = PROCESS_MEMORY_COUNTERS_EX2 {
                cb: size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32,
                ..PROCESS_MEMORY_COUNTERS_EX2::default()
            };
            if unsafe {
                K32GetProcessMemoryInfo(
                    self.handle,
                    (&raw mut memory).cast(),
                    size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32,
                )
            } == 0
            {
                return Err(ProcessMetricError::Unavailable);
            }
            let mut io = IO_COUNTERS::default();
            if unsafe { GetProcessIoCounters(self.handle, &raw mut io) } == 0 {
                return Err(ProcessMetricError::Unavailable);
            }
            let mut handle_count = 0_u32;
            if unsafe { GetProcessHandleCount(self.handle, &raw mut handle_count) } == 0 {
                return Err(ProcessMetricError::Unavailable);
            }
            let observed_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| ProcessMetricError::Unavailable)?
                .as_millis()
                .min(u128::from(u64::MAX)) as u64;
            Ok(ProcessMetricSample::from_values(ProcessMetricValues {
                pid: self.pid,
                identity_token: self.identity_token,
                observed_ms,
                private_working_set_bytes: memory.PrivateWorkingSetSize as u64,
                peak_private_bytes: memory.PeakPagefileUsage as u64,
                read_bytes: io.ReadTransferCount,
                write_bytes: io.WriteTransferCount,
                handle_count,
                thread_count: thread_count(self.pid)?,
                lifetime_ms: observed_ms.saturating_sub(self.creation_unix_ms),
            }))
        }
    }

    impl Drop for WindowsProcessSampler {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }

    fn query_image_path(handle: HANDLE) -> Result<PathBuf, ProcessMetricError> {
        let mut buffer = vec![0_u16; MAX_IMAGE_PATH_UNITS];
        let mut length = buffer.len() as u32;
        if unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &raw mut length) }
            == 0
        {
            return Err(ProcessMetricError::Unavailable);
        }
        buffer.truncate(length as usize);
        Ok(PathBuf::from(OsString::from_wide(&buffer)))
    }

    fn creation_ticks(handle: HANDLE) -> Result<u64, ProcessMetricError> {
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        if unsafe {
            GetProcessTimes(
                handle,
                &raw mut creation,
                &raw mut exit,
                &raw mut kernel,
                &raw mut user,
            )
        } == 0
        {
            return Err(ProcessMetricError::Unavailable);
        }
        Ok(filetime_ticks(creation))
    }

    fn filetime_ticks(value: FILETIME) -> u64 {
        (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
    }

    fn filetime_to_unix_ms(value: u64) -> Result<u64, ProcessMetricError> {
        value
            .checked_sub(FILETIME_UNIX_EPOCH_TICKS)
            .map(|ticks| ticks / FILETIME_TICKS_PER_MILLISECOND)
            .ok_or(ProcessMetricError::Unavailable)
    }

    fn thread_count(pid: u32) -> Result<u32, ProcessMetricError> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(ProcessMetricError::Unavailable);
        }
        let result = (|| {
            let mut entry = THREADENTRY32 {
                dwSize: size_of::<THREADENTRY32>() as u32,
                ..THREADENTRY32::default()
            };
            if unsafe { Thread32First(snapshot, &raw mut entry) } == 0 {
                return Err(ProcessMetricError::Unavailable);
            }
            let mut count = 0_u32;
            loop {
                if entry.th32OwnerProcessID == pid {
                    count = count.saturating_add(1);
                }
                if unsafe { Thread32Next(snapshot, &raw mut entry) } == 0 {
                    break;
                }
            }
            if count == 0 {
                return Err(ProcessMetricError::Unavailable);
            }
            Ok(count)
        })();
        unsafe {
            CloseHandle(snapshot);
        }
        result
    }

    fn same_windows_path(left: &Path, right: &Path) -> bool {
        normalized_path(left).eq_ignore_ascii_case(&normalized_path(right))
    }

    fn normalized_path(path: &Path) -> String {
        std::fs::canonicalize(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .replace('/', "\\")
            .trim_start_matches(r"\\?\")
            .to_owned()
    }
}

#[cfg(windows)]
pub use windows::WindowsProcessSampler;
