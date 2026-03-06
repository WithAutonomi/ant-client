pub mod add;
pub mod daemon;
pub mod reset;
pub mod start;
pub mod status;
pub mod stop;

use clap::Subcommand;

use crate::commands::node::add::AddArgs;
use crate::commands::node::daemon::DaemonCommand;
use crate::commands::node::reset::ResetArgs;
use crate::commands::node::start::StartArgs;
use crate::commands::node::status::StatusArgs;
use crate::commands::node::stop::StopArgs;

#[derive(Subcommand)]
pub enum NodeCommand {
    /// Add one or more nodes to the registry
    Add(Box<AddArgs>),
    /// Manage the node daemon
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Reset all node state (removes all data, logs, and clears the registry)
    Reset(ResetArgs),
    /// Start node(s). With no arguments starts all nodes; use --service-name for a specific node.
    Start(StartArgs),
    /// Show the status of all registered nodes
    Status(StatusArgs),
    /// Stop node(s). With no arguments stops all nodes; use --service-name for a specific node.
    Stop(StopArgs),
}
