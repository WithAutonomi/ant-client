//! Start a local devnet with 25 nodes using Arbitrum Sepolia for payments.
//!
//! Uses the existing deployed contracts on Arbitrum Sepolia.
//! Nodes verify payments against the real Sepolia PaymentVault.
//!
//! Writes a manifest to the ant-gui config directory so the GUI
//! auto-detects Sepolia mode on startup.
//!
//! # Usage
//!
//! ```bash
//! cargo run --release --example start-devnet-sepolia
//! ```

use ant_core::data::EvmNetwork;
use ant_node::devnet::{Devnet, DevnetConfig};
use std::path::PathBuf;

fn gui_manifest_path() -> PathBuf {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("autonomi")
        .join("ant-gui");
    std::fs::create_dir_all(&config_dir).ok();
    config_dir.join("devnet-manifest.json")
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

        let mut config = DevnetConfig::default(); // 25 nodes
        config.evm_network = Some(evm_network);

        println!("Starting {} nodes...", config.node_count);

        let mut devnet = Devnet::new(config).await?;
        devnet.start().await?;

        let bootstrap_addrs: Vec<String> = devnet
            .bootstrap_addrs()
            .iter()
            .filter_map(|ma| {
                let s = ma.to_string();
                let parts: Vec<&str> = s.split('/').collect();
                let ip = parts.iter().position(|&p| p == "ip4").and_then(|i| parts.get(i + 1))?;
                let port = parts.iter().position(|&p| p == "udp").and_then(|i| parts.get(i + 1))?;
                Some(format!("{ip}:{port}"))
            })
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

        let manifest_path = gui_manifest_path();
        std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;

        println!();
        println!("=== Sepolia Devnet is running! ===");
        println!();
        println!("Nodes:           {}", devnet.config().node_count);
        println!("Bootstrap peers: {:?}", bootstrap_addrs);
        println!("Manifest:        {}", manifest_path.display());
        println!();
        println!("Start ant-gui with: npm run tauri:dev");
        println!("Import wallet key in Settings > Advanced > Direct Wallet.");
        println!();
        println!("Press Ctrl+C to stop.");

        tokio::signal::ctrl_c().await?;
        println!("Shutting down...");

        if manifest_path.exists() {
            std::fs::remove_file(&manifest_path).ok();
        }

        Ok(())
    })
}
