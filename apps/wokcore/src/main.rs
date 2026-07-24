use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "wokcore",
    version,
    about = "Independent local provider gateway for the Wok product family"
)]
struct Cli {}

fn main() {
    Cli::parse();
}
