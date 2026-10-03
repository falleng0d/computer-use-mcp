mod image;

use anyhow::bail;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version = computer_protocol::VERSION, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print the version and the computer image this build uses
    Info,
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Some(Command::Info) => {
            let image = image::from_env();
            println!("version: {}", computer_protocol::VERSION);
            println!("protocol: {}", computer_protocol::PROTOCOL_VERSION);
            println!("image: {}", image.reference);
            println!("pull if missing: {}", image.pull);
            Ok(())
        }
        None => bail!("the MCP server is not implemented yet"),
    }
}
