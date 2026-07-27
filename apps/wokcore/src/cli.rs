use std::{ffi::OsString, path::PathBuf};

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
    /// Read indexed local coding sessions.
    Sessions(Sessions),
    /// Read redacted WokCore diagnostic events.
    Logs(Logs),
    /// Work with bounded diagnostic support packages.
    Diagnostics(Diagnostics),
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
    /// Exact client-token scope; repeat to grant more than one.
    #[arg(long = "scope", value_name = "SCOPE")]
    pub scopes: Vec<String>,
    /// Emit the one-time token in a stable JSON object.
    #[arg(long, required = true)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct Sessions {
    #[command(subcommand)]
    pub command: SessionsCommand,
}

#[derive(Debug, Subcommand)]
pub enum SessionsCommand {
    /// List indexed sessions.
    List(SessionList),
    /// Show messages from one indexed session.
    Show(SessionShow),
}

#[derive(Debug, Args)]
pub struct SessionList {
    /// Exact session source: codex, claude, or gemini.
    #[arg(long)]
    pub source: Option<String>,
    /// Maximum sessions to return.
    #[arg(long)]
    pub limit: Option<usize>,
    /// Emit the stable JSON response.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct SessionShow {
    /// Opaque indexed session key.
    #[arg(value_name = "SESSION_KEY")]
    pub session_key: String,
    /// Opaque continuation cursor.
    #[arg(long)]
    pub cursor: Option<String>,
    /// Maximum messages to return.
    #[arg(long)]
    pub limit: Option<usize>,
    /// Emit the stable JSON response.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct Logs {
    /// Exact request correlation identifier.
    #[arg(long)]
    pub request_id: Option<String>,
    /// Minimum diagnostic level.
    #[arg(long)]
    pub level: Option<String>,
    /// Exact diagnostic component.
    #[arg(long)]
    pub component: Option<String>,
    /// Inclusive canonical UTC start timestamp.
    #[arg(long)]
    pub since: Option<String>,
    /// Emit one JSON event per line.
    #[arg(long)]
    pub jsonl: bool,
}

#[derive(Debug, Args)]
pub struct Diagnostics {
    #[command(subcommand)]
    pub command: DiagnosticsCommand,
}

#[derive(Debug, Subcommand)]
pub enum DiagnosticsCommand {
    /// Export a validated diagnostic support package.
    Export(DiagnosticsExport),
}

#[derive(Debug, Args)]
pub struct DiagnosticsExport {
    /// Create a new ZIP package at this path.
    #[arg(long, value_name = "PATH")]
    pub output: PathBuf,
}

pub fn parse_command<I, T>(arguments: I) -> Result<Command, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    Cli::try_parse_from(arguments).map(|cli| cli.command)
}
