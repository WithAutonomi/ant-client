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

    /// Per-op timeout for quote / DHT-lookup operations (seconds).
    /// Static knob; the adaptive controller does not currently size
    /// timeouts.
    #[arg(long, default_value_t = 10, hide = true)]
    pub quote_timeout_secs: u64,

    /// Per-op timeout for chunk store / retrieve operations (seconds).
    /// Static knob; the adaptive controller does not currently size
    /// timeouts.
    #[arg(long, default_value_t = 60, hide = true)]
    pub store_timeout_secs: u64,

    /// **Deprecated.** Adaptive controller now sizes quote
    /// concurrency from observed network signals. Setting this caps
    /// the controller's max for the quote channel only (does not
    /// affect store or download). Removed in a future release.
    /// Must be > 0.
    #[arg(long, hide = true, value_parser = parse_positive_usize)]
    pub quote_concurrency: Option<usize>,

    /// **Deprecated.** Adaptive controller now sizes store
    /// concurrency from observed network signals. Setting this caps
    /// the controller's max for the store channel only (does not
    /// affect quote or download). Removed in a future release.
    /// Must be > 0.
    #[arg(long, alias = "chunk-concurrency", hide = true, value_parser = parse_positive_usize)]
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

/// clap value parser that rejects 0 for the deprecated concurrency
/// pins (a pin of 0 silently disables itself in the controller, which
/// is confusing — fail fast at parse time instead).
fn parse_positive_usize(s: &str) -> Result<usize, String> {
    let n: usize = s
        .parse()
        .map_err(|e| format!("not a non-negative integer: {e}"))?;
    if n == 0 {
        return Err("must be > 0".to_string());
    }
    Ok(n)
}
