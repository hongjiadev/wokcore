use clap::Parser;
use tokio::runtime::{Builder, Runtime};

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(any(target_os = "linux", target_os = "macos"))]
unsafe extern "C" {
    fn mi_thread_set_in_threadpool();
}

fn main() {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    configure_portable_allocator();
    let runtime = build_runtime().expect("wokcore runtime initialization failed");
    let cli = wokcore::cli::Cli::parse();
    std::process::exit(runtime.block_on(wokcore::run_production(cli)).as_i32());
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
const fn portable_allocator_options() -> [(i32, i64); 6] {
    // libmimalloc-sys intentionally omits unstable option constants. Its
    // bundled mimalloc v3 ABI assigns 5 to purge_decommits, 15 to
    // purge_delay, 26 to disallow_arena_alloc, 35 to page_reclaim_on_free, 36
    // to page_full_retain, and 42 to page_cross_thread_max_reclaim. Direct
    // OS-backed pages and an immediate purge keep burst memory reclaimable;
    // page_reclaim_on_free lets a Tokio worker reclaim pages freed remotely.
    [(5, 1), (15, 0), (26, 1), (35, 1), (36, 0), (42, -1)]
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn configure_portable_allocator() {
    for (option, value) in portable_allocator_options() {
        // SAFETY: this runs on the initial process thread before Tokio starts;
        // mimalloc documents option mutation as safe before concurrent use.
        unsafe {
            libmimalloc_sys::mi_option_set(option, value);
        }
    }
}

fn build_runtime() -> std::io::Result<Runtime> {
    let mut builder = Builder::new_multi_thread();
    builder.enable_all();
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    builder.on_thread_start(|| {
        // SAFETY: the bundled mimalloc v3 API documents this call for worker
        // threads that can execute arbitrary tasks, which is Tokio's model.
        unsafe {
            mi_thread_set_in_threadpool();
        }
    });
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    builder.on_thread_park(|| {
        // Requests can be allocated and released on different Tokio workers.
        // Collecting from the owning worker when it becomes idle lets mimalloc
        // promptly decommit those remote frees without limiting concurrency.
        unsafe {
            libmimalloc_sys::mi_collect(true);
        }
    });
    builder.build()
}

#[cfg(test)]
mod tests {
    #[test]
    fn portable_allocator_options_reclaim_remote_frees_immediately() {
        assert_eq!(
            super::portable_allocator_options(),
            [(5, 1), (15, 0), (26, 1), (35, 1), (36, 0), (42, -1)]
        );
    }

    #[test]
    fn portable_allocator_cfg_excludes_windows() {
        let source = include_str!("main.rs");
        assert!(source.contains("#[cfg(any(target_os = \"linux\", target_os = \"macos\"))]"));
        assert!(!source.contains("#[cfg(target_os = \"windows\")]\n#[global_allocator]"));
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
