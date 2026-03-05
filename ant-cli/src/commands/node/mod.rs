pub mod daemon;

use clap::Subcommand;

use crate::commands::node::daemon::DaemonCommand;

#[derive(Subcommand)]
pub enum NodeCommand {
    /// Manage the node daemon
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
}
