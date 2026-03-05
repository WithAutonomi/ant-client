use clap::{Parser, Subcommand};

use crate::commands::node::NodeCommand;

#[derive(Parser)]
#[command(name = "ant", about = "Autonomi network client")]
pub struct Cli {
    /// Output structured JSON instead of human-readable text
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Manage nodes
    Node {
        #[command(subcommand)]
        command: NodeCommand,
    },
}
