//! Start a local devnet with EVM payments.
//!
//! Launches a minimal Autonomi network (5 nodes) with an embedded Anvil
//! blockchain, writes a manifest to `/tmp/ant-devnet-manifest.json`,
//! and waits for Ctrl+C.
//!
//! # Usage
//!
//! ```bash
//! cargo run --example start-local-devnet
//! ```

use ant_core::data::LocalDevnet;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    // Start a minimal local devnet with EVM payments
    let devnet = LocalDevnet::start_minimal().await?;

    // Write manifest so the CLI example can use it
    let manifest_path = PathBuf::from("/tmp/ant-devnet-manifest.json");
    devnet.write_manifest(&manifest_path).await?;

    println!();
    println!("=== Devnet is running! ===");
    println!();
    println!("Bootstrap peers: {:?}", devnet.bootstrap_addrs());
    println!("Wallet key:      {}", devnet.wallet_private_key());
    println!("Manifest:        {}", manifest_path.display());
    println!();
    println!("# Upload a file:");
    println!(
        "cargo run --example cli -- --devnet-manifest {} upload --file <YOUR_FILE>",
        manifest_path.display()
    );
    println!();
    println!("# Download it back:");
    println!(
        "cargo run --example cli -- --devnet-manifest {} download --datamap <HEX> --output <OUTPUT_PATH>",
        manifest_path.display()
    );
    println!();
    println!("Press Ctrl+C to stop.");

    tokio::signal::ctrl_c().await?;
    println!("Shutting down...");

    Ok(())
}
