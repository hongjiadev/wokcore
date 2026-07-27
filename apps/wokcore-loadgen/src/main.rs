use std::{
    io::{self, Read},
    time::Duration,
};

use clap::{Parser, ValueEnum};
use wokcore_provider_sim::{
    LoadConfig, LoadPayloadProfile, LoadProtocol, ProtocolWeight, run_load,
};

const MAX_TOKEN_BYTES: usize = 64 * 1024;

#[derive(Debug, Parser)]
#[command(name = "wokcore-loadgen")]
struct Arguments {
    #[arg(long)]
    target: String,
    #[arg(long, default_value_t = 1)]
    concurrency: usize,
    #[arg(long, default_value_t = 0)]
    ramp_ms: u64,
    #[arg(long, default_value_t = 30_000)]
    duration_ms: u64,
    #[arg(long = "protocol", default_value = "responses=1")]
    protocols: Vec<String>,
    #[arg(long, value_enum, default_value_t = PayloadArgument::Standard32k)]
    payload_profile: PayloadArgument,
    #[arg(long, default_value_t = 0)]
    cancellation_permyriad: u16,
    #[arg(long, default_value_t = 0)]
    slow_consumer_ms: u64,
    #[arg(long)]
    token_stdin: bool,
    #[arg(long, default_value_t = 0)]
    max_errors: u64,
    #[arg(long, default_value_t = 0)]
    require_peak_active: usize,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum PayloadArgument {
    Standard32k,
    Body1mib,
    LongTool,
    LongReasoning,
}

impl From<PayloadArgument> for LoadPayloadProfile {
    fn from(value: PayloadArgument) -> Self {
        match value {
            PayloadArgument::Standard32k => Self::Standard32K,
            PayloadArgument::Body1mib => Self::Body1MiB,
            PayloadArgument::LongTool => Self::LongTool,
            PayloadArgument::LongReasoning => Self::LongReasoning,
        }
    }
}

#[tokio::main]
async fn main() {
    match run(Arguments::parse()).await {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(error) => {
            eprintln!("wokcore-loadgen: {error}");
            std::process::exit(1);
        }
    }
}

async fn run(arguments: Arguments) -> Result<i32, Box<dyn std::error::Error>> {
    let mut config = LoadConfig::new(&arguments.target)?
        .with_concurrency(arguments.concurrency)
        .with_ramp(Duration::from_millis(arguments.ramp_ms))
        .with_duration(Duration::from_millis(arguments.duration_ms))
        .with_protocol_mix(parse_protocols(&arguments.protocols)?)
        .with_payload_profile(arguments.payload_profile.into())
        .with_cancellation_permyriad(arguments.cancellation_permyriad)
        .with_slow_consumer_delay(Duration::from_millis(arguments.slow_consumer_ms));
    if arguments.token_stdin {
        config = config.with_bearer_token(read_token()?);
    }
    let report = run_load(config).await?;
    let failed = report.errors() > arguments.max_errors
        || report.peak_active() < arguments.require_peak_active;
    println!("{}", serde_json::to_string(&report)?);
    Ok(if failed { 2 } else { 0 })
}

fn parse_protocols(values: &[String]) -> Result<Vec<ProtocolWeight>, io::Error> {
    values
        .iter()
        .map(|value| {
            let Some((protocol, weight)) = value.split_once('=') else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "protocol must use name=weight",
                ));
            };
            let protocol = match protocol {
                "responses" => LoadProtocol::Responses,
                "chat" => LoadProtocol::Chat,
                "anthropic" => LoadProtocol::Anthropic,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "unsupported load protocol",
                    ));
                }
            };
            let weight = weight
                .parse()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid weight"))?;
            Ok(ProtocolWeight::new(protocol, weight))
        })
        .collect()
}

fn read_token() -> io::Result<String> {
    let mut bytes = Vec::with_capacity(1024);
    io::stdin()
        .take((MAX_TOKEN_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_TOKEN_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "token input exceeds its bound",
        ));
    }
    let token = String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "token must be UTF-8"))?;
    let token = token.trim_end_matches(['\r', '\n']);
    if token.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "token must not be empty",
        ));
    }
    Ok(token.to_owned())
}
