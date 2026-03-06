mod cli;
mod commands;

use clap::Parser;

use cli::{Cli, Commands};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Node { command } => match command {
            commands::node::NodeCommand::Add(args) => {
                args.execute(cli.json).await?;
            }

            commands::node::NodeCommand::Daemon { command } => {
                command.execute(cli.json).await?;
            }

            commands::node::NodeCommand::Reset(args) => {
                args.execute(cli.json).await?;
            }

            commands::node::NodeCommand::Start(args) => {
                args.execute(cli.json).await?;
            }

            commands::node::NodeCommand::Status(args) => {
                args.execute(cli.json).await?;
            }

            commands::node::NodeCommand::Stop(args) => {
                args.execute(cli.json).await?;
            }
        },
    }

    Ok(())
}
