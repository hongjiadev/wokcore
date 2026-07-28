use clap::Parser;
use tokio::runtime::{Builder, Runtime};

#[cfg(target_os = "macos")]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn mi_thread_set_in_threadpool();
}

fn main() {
    #[cfg(target_os = "macos")]
    configure_macos_allocator();
    let runtime = build_runtime().expect("wokcore runtime initialization failed");
    let cli = wokcore::cli::Cli::parse();
    std::process::exit(runtime.block_on(wokcore::run_production(cli)).as_i32());
}

#[cfg(any(target_os = "macos", test))]
const fn macos_allocator_options() -> [(i32, i64); 5] {
    // libmimalloc-sys intentionally omits unstable option constants. Its
    // bundled mimalloc v3 ABI assigns 5 to purge_decommits, 15 to
    // purge_delay, 26 to disallow_arena_alloc, 36 to page_full_retain, and 42
    // to page_cross_thread_max_reclaim. macOS reports arena pages as resident
    // after a burst even after a forced purge, so direct OS-backed pages are a
    // better fit for this long-lived process. Tokio workers retain neither
    // full pages nor abandoned pages reclaimed from other workers.
    [(5, 1), (15, 0), (26, 1), (36, 0), (42, 0)]
}

#[cfg(target_os = "macos")]
fn configure_macos_allocator() {
    for (option, value) in macos_allocator_options() {
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
    #[cfg(target_os = "macos")]
    builder.on_thread_start(|| {
        // SAFETY: the bundled mimalloc v3 API documents this call for worker
        // threads that can execute arbitrary tasks, which is Tokio's model.
        unsafe {
            mi_thread_set_in_threadpool();
        }
    });
    #[cfg(target_os = "macos")]
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
    fn macos_allocator_options_purge_idle_pages_immediately() {
        assert_eq!(
            super::macos_allocator_options(),
            [(5, 1), (15, 0), (26, 1), (36, 0), (42, 0)]
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
