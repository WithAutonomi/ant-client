pub mod add;
pub mod daemon;

use clap::Subcommand;

use crate::commands::node::add::AddArgs;
use crate::commands::node::daemon::DaemonCommand;

#[derive(Subcommand)]
pub enum NodeCommand {
    /// Add one or more nodes to the registry
    Add(Box<AddArgs>),
    /// Manage the node daemon
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
}
