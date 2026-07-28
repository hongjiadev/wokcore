use clap::Parser;
use tokio::runtime::{Builder, Runtime};

#[cfg(target_os = "macos")]
#[global_allocator]
static GLOBAL_ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(target_os = "macos")]
union MacosAllocatorConfig {
    bytes: &'static u8,
    c_char: &'static libc::c_char,
}

#[cfg(target_os = "macos")]
#[unsafe(export_name = "_rjem_malloc_conf")]
pub static MACOS_MALLOC_CONF: Option<&'static libc::c_char> = Some(unsafe {
    MacosAllocatorConfig {
        bytes: &b"narenas:2,dirty_decay_ms:0,muzzy_decay_ms:0\0"[0],
    }
    .c_char
});

fn main() {
    let runtime = build_runtime().expect("wokcore runtime initialization failed");
    let cli = wokcore::cli::Cli::parse();
    std::process::exit(runtime.block_on(wokcore::run_production(cli)).as_i32());
}

fn build_runtime() -> std::io::Result<Runtime> {
    let mut builder = Builder::new_multi_thread();
    builder.enable_all();
    builder.build()
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    fn read_jemalloc_option<T: Copy>(name: &std::ffi::CStr) -> T {
        let mut value = std::mem::MaybeUninit::<T>::uninit();
        let mut length = std::mem::size_of::<T>();
        let status = unsafe {
            tikv_jemalloc_sys::mallctl(
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        assert_eq!(status, 0);
        assert_eq!(length, std::mem::size_of::<T>());
        unsafe { value.assume_init() }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_allocator_policy_is_active() {
        assert_eq!(
            unsafe { tikv_jemalloc_sys::malloc_conf },
            super::MACOS_MALLOC_CONF
        );
        let configured = unsafe {
            std::ffi::CStr::from_ptr(super::MACOS_MALLOC_CONF.unwrap())
                .to_str()
                .unwrap()
        };

        assert_eq!(configured, "narenas:2,dirty_decay_ms:0,muzzy_decay_ms:0");
        assert_eq!(read_jemalloc_option::<libc::c_uint>(c"opt.narenas"), 2);
        assert_eq!(
            read_jemalloc_option::<libc::ssize_t>(c"opt.dirty_decay_ms"),
            0
        );
        assert_eq!(
            read_jemalloc_option::<libc::ssize_t>(c"opt.muzzy_decay_ms"),
            0
        );
    }

    #[test]
    fn production_runtime_enables_time_and_io() {
        let runtime = super::build_runtime().unwrap();
        runtime.block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                tokio::time::sleep(std::time::Duration::from_millis(1)),
            )
            .await
            .unwrap();
        });
    }
}
