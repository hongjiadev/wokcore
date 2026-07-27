use std::{
    fs::File,
    io::{self, Read},
    path::PathBuf,
};

use clap::Parser;
use wokcore_provider_sim::{MAX_SCENARIO_BYTES, Scenario, Simulator, validate_loopback_socket};

#[derive(Debug, Parser)]
#[command(name = "wokcore-provider-sim")]
struct Arguments {
    #[arg(long, default_value = "127.0.0.1:40100")]
    bind: String,
    #[arg(long)]
    scenario: PathBuf,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run(Arguments::parse()).await {
        eprintln!("wokcore-provider-sim: {error}");
        std::process::exit(1);
    }
}

async fn run(arguments: Arguments) -> Result<(), Box<dyn std::error::Error>> {
    let address = validate_loopback_socket(&arguments.bind)?;
    let scenario = Scenario::from_toml(&read_bounded(&arguments.scenario)?)?;
    let simulator = Simulator::start(address, scenario).await?;
    println!(
        "{}",
        serde_json::json!({
            "address": simulator.address().to_string(),
            "loopback_only": true
        })
    );
    tokio::signal::ctrl_c().await?;
    simulator.shutdown().await?;
    Ok(())
}

fn read_bounded(path: &PathBuf) -> io::Result<String> {
    let file = File::open(path)?;
    let mut bytes = Vec::with_capacity(MAX_SCENARIO_BYTES.min(8 * 1024));
    file.take((MAX_SCENARIO_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_SCENARIO_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "scenario exceeds its bounded input size",
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "scenario must be UTF-8"))
}
