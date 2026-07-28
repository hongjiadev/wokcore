use clap::Parser;

#[cfg(target_os = "macos")]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() {
    let cli = wokcore::cli::Cli::parse();
    std::process::exit(wokcore::run_production(cli).await.as_i32());
}
