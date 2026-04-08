//! Start a local devnet with 25 nodes using Arbitrum Sepolia for payments.
//!
//! Uses the existing deployed contracts on Arbitrum Sepolia.
//! Nodes verify payments against the real Sepolia PaymentVault.
//!
//! Writes a manifest to the shared ant data directory so any consumer
//! (ant-gui, ant-cli, ant-tui) auto-detects Sepolia mode on startup.
//!
//! Unlike the local Anvil devnet, no wallet key is provided — the user
//! must connect their own funded Sepolia wallet (e.g. via WalletConnect
//! in ant-gui, or SECRET_KEY env var in ant-cli).
//!
//! # Usage
//!
//! ```bash
//! cargo run --release --example start-devnet-sepolia
//! ```

use ant_core::data::EvmNetwork;
use ant_node::devnet::{Devnet, DevnetConfig};

fn manifest_path() -> std::path::PathBuf {
    let data_dir = ant_core::config::data_dir().expect("Could not determine data directory");
    std::fs::create_dir_all(&data_dir).ok();
    data_dir.join("devnet-manifest.json")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?;

    runtime.block_on(async {
        let evm_network = EvmNetwork::ArbitrumSepoliaTest;

        let rpc_url = evm_network.rpc_url().to_string();
        let token_addr = format!("{}", evm_network.payment_token_address());
        let vault_addr = format!("{}", evm_network.payment_vault_address());

        println!("Starting Sepolia devnet...");
        println!("RPC:   {rpc_url}");
        println!("Token: {token_addr}");
        println!("Vault: {vault_addr}");

        let config = DevnetConfig {
            evm_network: Some(evm_network),
            ..DevnetConfig::default()
        };

        println!("Starting {} nodes...", config.node_count);

        let mut devnet = Devnet::new(config).await?;
        devnet.start().await?;

        let bootstrap_addrs: Vec<String> = devnet
            .bootstrap_addrs()
            .iter()
            .map(|a| format!("{a}"))
            .collect();

        let manifest = serde_json::json!({
            "base_port": 0,
            "node_count": devnet.config().node_count,
            "bootstrap": devnet.bootstrap_addrs().iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            "data_dir": devnet.config().data_dir.to_string_lossy(),
            "created_at": "",
            "evm": {
                "rpc_url": rpc_url,
                "wallet_private_key": "",
                "payment_token_address": token_addr,
                "payment_vault_address": vault_addr,
            }
        });

        let path = manifest_path();
        std::fs::write(&path, serde_json::to_string_pretty(&manifest)?)?;

        println!();
        println!("=== Sepolia Devnet is running! ===");
        println!();
        println!("Nodes:           {}", devnet.config().node_count);
        println!("Bootstrap peers: {:?}", bootstrap_addrs);
        println!("Manifest:        {}", path.display());
        println!();
        println!("No wallet key is embedded — connect your own funded Sepolia wallet.");
        println!();
        println!("Press Ctrl+C to stop.");

        tokio::signal::ctrl_c().await?;
        println!("Shutting down...");

        if path.exists() {
            std::fs::remove_file(&path).ok();
        }

        Ok(())
    })
}
