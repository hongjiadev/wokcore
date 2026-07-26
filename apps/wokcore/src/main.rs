use clap::Parser;

#[tokio::main]
async fn main() {
    let cli = wokcore::cli::Cli::parse();
    std::process::exit(wokcore::run_production(cli).await.as_i32());
}
