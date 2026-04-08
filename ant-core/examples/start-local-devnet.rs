//! Start a local devnet with 25 nodes and EVM payments.
//!
//! Launches an Autonomi network with an embedded Anvil blockchain,
//! writes a manifest to the shared ant data directory, and waits for Ctrl+C.
//!
//! Any consumer that checks `ant_core::config::data_dir()` for
//! `devnet-manifest.json` (ant-gui, ant-cli, ant-tui) will auto-detect
//! devnet mode on startup.
//!
//! # Usage
//!
//! ```bash
//! cargo run --release --example start-local-devnet
//! ```

use ant_core::data::LocalDevnet;
use ant_node::devnet::DevnetConfig;

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
        let config = DevnetConfig::default();
        println!(
            "Starting local Anvil devnet with {} nodes...",
            config.node_count
        );

        let devnet = LocalDevnet::start(config).await?;

        let path = manifest_path();
        devnet.write_manifest(&path).await?;

        println!();
        println!("=== Local Devnet is running! ===");
        println!();
        println!("Nodes:           {}", devnet.manifest().node_count);
        println!("Bootstrap peers: {:?}", devnet.bootstrap_addrs());
        println!("Wallet key:      {}", devnet.wallet_private_key());
        println!("Manifest:        {}", path.display());
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
