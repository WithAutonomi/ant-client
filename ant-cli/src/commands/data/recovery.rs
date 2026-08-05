// ant-cli: recovery commands.
// List and recover on-chain DataMap backups.

use ant_core::data::client::recovery::{list_recoveries, recover_datamap};
use clap::{Args, Subcommand};

/// Recovery commands.
#[derive(Subcommand, Debug)]
pub enum RecoveryAction {
    /// List recovery backups for a wallet.
    List(RecoveryListArgs),
    /// Recover a DataMap from an on-chain transaction.
    Recover(RecoveryRecoverArgs),
}

#[derive(Args, Debug)]
pub struct RecoveryListArgs {
    /// Wallet address to query recoveries for.
    #[arg(short, long)]
    pub wallet: Option<String>,

    /// Arbitrum RPC URL.
    #[arg(long, default_value = "https://arb1.arbitrum.io/rpc")]
    pub rpc: String,
}

#[derive(Args, Debug)]
pub struct RecoveryRecoverArgs {
    /// Transaction hash of the recovery backup.
    pub tx_hash: String,

    /// Path to the user key file for DataMap decryption.
    #[arg(short, long)]
    pub key: Option<String>,

    /// Arbitrum RPC URL.
    #[arg(long, default_value = "https://arb1.arbitrum.io/rpc")]
    pub rpc: String,
}

impl RecoveryAction {
    pub async fn execute(self) -> anyhow::Result<()> {
        match self {
            RecoveryAction::List(args) => recovery_list(args).await,
            RecoveryAction::Recover(args) => recovery_recover(args).await,
        }
    }
}

pub async fn recovery_list(args: RecoveryListArgs) -> anyhow::Result<()> {
    let wallet = args.wallet.unwrap_or_else(|| "default".into());
    println!("Recovery backups for wallet: {wallet}");
    let entries = list_recoveries(&wallet, &args.rpc).await
        .map_err(|e| anyhow::anyhow!("list: {e}"))?;
    if entries.is_empty() {
        println!("  No recovery backups found.");
        return Ok(());
    }
    for entry in &entries {
        println!("  {} | {} | {} bytes | folder={}",
            entry.tx_hash, entry.timestamp, entry.datamap_size,
            &entry.folder_hash[..16.min(entry.folder_hash.len())]);
    }
    Ok(())
}

pub async fn recovery_recover(args: RecoveryRecoverArgs) -> anyhow::Result<()> {
    println!("Recovering DataMap from tx: {}", args.tx_hash);
    let key = match &args.key {
        Some(path) => std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("read key: {e}"))?,
        None => anyhow::bail!("--key required for DataMap decryption"),
    };
    let datamap_bytes = recover_datamap(&args.tx_hash, &key, &args.rpc).await
        .map_err(|e| anyhow::anyhow!("recover: {e}"))?;
    println!("  Recovered DataMap: {} bytes", datamap_bytes.len());
    println!("  (Phase 2: reconstruct files from DataMap)");
    Ok(())
}
