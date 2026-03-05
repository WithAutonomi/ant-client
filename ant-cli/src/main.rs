mod cli;
mod commands;

use clap::Parser;

use cli::{Cli, Commands};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Node { command } => match command {
            commands::node::NodeCommand::Daemon { command } => {
                command.execute(cli.json).await?;
            }
        },
    }

    Ok(())
}
