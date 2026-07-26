use std::ffi::OsString;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "wokcore",
    bin_name = "wokcore",
    version,
    about = "Independent local provider gateway for the Wok product family"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the local WokCore service.
    Serve(JsonOutput),
    /// Report local WokCore service status.
    Status(JsonOutput),
    /// Gracefully stop the local WokCore service.
    Stop(JsonOutput),
    /// Diagnose the local WokCore service.
    Doctor(JsonOutput),
    /// Issue a one-time proxy token for a client.
    Authorize(Authorize),
}

#[derive(Debug, Args)]
pub struct JsonOutput {
    /// Emit stable JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct Authorize {
    /// Client identifier to authorize.
    #[arg(long, value_name = "ID")]
    pub client: String,
    /// Emit the one-time token in a stable JSON object.
    #[arg(long, required = true)]
    pub json: bool,
}

pub fn parse_command<I, T>(arguments: I) -> Result<Command, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    Cli::try_parse_from(arguments).map(|cli| cli.command)
}
