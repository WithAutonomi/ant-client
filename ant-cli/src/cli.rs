use clap::{ArgAction, Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;

use crate::commands::data::{ChunkAction, FileAction, WalletAction};
use crate::commands::node::NodeCommand;
use crate::commands::update::UpdateArgs;

#[derive(Parser)]
// NOTE: when reintroducing a multi-line `--version` (e.g. via `long_version`),
// the version number MUST stay on line 1. The self-update parser in 0.1.2–0.1.4
// reads only the last whitespace-separated token of the entire output, so any
// trailing line (like `License: MIT or Apache-2.0`) breaks upgrades for users
// on those versions.
#[command(name = "ant", version, about = "Autonomi network client")]
pub struct Cli {
    /// Output structured JSON instead of human-readable text
    #[arg(long, global = true)]
    pub json: bool,

    /// Bootstrap peer addresses (for data operations).
    /// Comma-separated or repeated: -b 1.2.3.4:10000,5.6.7.8:10000
    #[arg(long, short, value_delimiter = ',')]
    pub bootstrap: Vec<SocketAddr>,

    /// Path to devnet manifest JSON (for data operations).
    #[arg(long)]
    pub devnet_manifest: Option<PathBuf>,

    /// Allow loopback connections (required for devnet/local testing).
    #[arg(long)]
    pub allow_loopback: bool,

    /// Force IPv4-only mode (disable dual-stack).
    /// Use on hosts without working IPv6 to avoid advertising
    /// unreachable addresses to the DHT.
    #[arg(long)]
    pub ipv4_only: bool,

    /// Timeout for lightweight network operations such as quotes and DHT
    /// lookups (seconds).
    #[arg(long, default_value_t = 10)]
    pub quote_timeout_secs: u64,

    /// Timeout for chunk store/retrieve operations (seconds).
    /// Chunk PUTs transfer multi-MB payloads, so this should be longer than
    /// the quote timeout.
    #[arg(long, default_value_t = 60)]
    pub store_timeout_secs: u64,

    /// Maximum number of chunks quoted or downloaded concurrently.
    /// Defaults to 32. Safe to set high — quoting is pure network I/O.
    #[arg(long)]
    pub quote_concurrency: Option<usize>,

    /// Maximum number of chunks stored concurrently during uploads.
    /// Defaults to half the available CPU threads. Lower values are
    /// more reliable when storing to NAT-restricted nodes.
    #[arg(long, alias = "chunk-concurrency")]
    pub store_concurrency: Option<usize>,

    /// Increase verbosity. By default no logs are emitted (privacy by design).
    /// -v: info + warnings, -vv: debug, -vvv: trace.
    #[arg(short, long, action = ArgAction::Count)]
    pub verbose: u8,

    /// EVM network for payment processing (arbitrum-one, arbitrum-sepolia, local).
    #[arg(long, default_value = "arbitrum-one")]
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
