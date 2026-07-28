use clap::Parser;
use tokio::runtime::{Builder, Runtime};

#[cfg(target_os = "macos")]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let runtime = build_runtime().expect("wokcore runtime initialization failed");
    let cli = wokcore::cli::Cli::parse();
    std::process::exit(runtime.block_on(wokcore::run_production(cli)).as_i32());
}

fn build_runtime() -> std::io::Result<Runtime> {
    let mut builder = Builder::new_multi_thread();
    builder.enable_all();
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
