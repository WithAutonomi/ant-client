use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;

use crate::commands::data::{ChunkAction, FileAction, WalletAction};
use crate::commands::node::NodeCommand;
use crate::commands::update::UpdateArgs;

fn long_version() -> &'static str {
    concat!(
        env!("CARGO_PKG_VERSION"),
        "\n",
        "Autonomi network client: file operations and node management for the Autonomi decentralised network\n",
        "\n",
        "Repository: https://github.com/WithAutonomi/ant-client\n",
        "License:    MIT or Apache-2.0",
    )
}

#[derive(Parser)]
#[command(
    name = "ant",
    version,
    long_version = long_version(),
    about = "Autonomi network client"
)]
pub struct Cli {
    /// Output structured JSON instead of human-readable text
    #[arg(long, global = true)]
    pub json: bool,

    /// Bootstrap peer addresses (for data operations).
    #[arg(long, short)]
    pub bootstrap: Vec<SocketAddr>,

    /// Path to devnet manifest JSON (for data operations).
    #[arg(long)]
    pub devnet_manifest: Option<PathBuf>,

    /// Allow loopback connections (required for devnet/local testing).
    #[arg(long)]
    pub allow_loopback: bool,

    /// Timeout for network operations (seconds).
    #[arg(long, default_value_t = 60)]
    pub timeout_secs: u64,

    /// Maximum number of chunks processed concurrently during uploads.
    /// Defaults to half the available CPU threads.
    #[arg(long)]
    pub chunk_concurrency: Option<usize>,

    /// Log level.
    #[arg(long, default_value = "info")]
    pub log_level: String,

    /// EVM network for payment processing.
    #[arg(long, default_value = "local")]
    pub evm_network: String,

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
    /// Wallet operations
    Wallet {
        #[command(subcommand)]
        action: WalletAction,
    },
    /// File operations (multi-chunk upload/download with EVM payment)
    File {
        #[command(subcommand)]
        action: FileAction,
    },
    /// Single-chunk operations (low-level put/get without file splitting)
    Chunk {
        #[command(subcommand)]
        action: ChunkAction,
    },
    /// Update the ant binary to the latest version
    Update(UpdateArgs),
}
