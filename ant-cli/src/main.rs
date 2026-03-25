mod cli;
mod commands;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tracing::info;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use ant_core::data::{
    Client, ClientConfig, CoreNodeConfig, CustomNetwork, DevnetManifest, EvmAddress, EvmNetwork,
    MultiAddr, NodeMode, P2PNode, Wallet, MAX_WIRE_MESSAGE_SIZE,
};
use cli::{Cli, Commands};

/// Force at least 4 worker threads regardless of CPU count.
///
/// On small VMs (1-2 vCPU), the default `num_cpus` gives only 1-2 worker
/// threads.  The NAT traversal poll() function does synchronous work
/// (parking_lot locks, DashMap iteration) that blocks its worker thread.
/// With only 1 worker, this freezes the entire runtime — timers stop,
/// keepalives can't fire, and connections die silently.
#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let code = match run().await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e:?}");
            1
        }
    };

    // Force-exit to avoid hanging on tokio runtime shutdown.
    // Open QUIC connections and pending background tasks (DHT, keep-alive)
    // block the runtime's graceful shutdown indefinitely. All data has been
    // persisted / printed by this point, so there is nothing left to clean up.
    std::process::exit(code);
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Initialize tracing for data commands (node commands handle their own output)
    let needs_tracing = !matches!(cli.command, Commands::Node { .. });
    if needs_tracing {
        let filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cli.log_level));
        tracing_subscriber::registry()
            .with(fmt::layer().with_writer(std::io::stderr))
            .with(filter)
            .init();
    }

    // Separate the command from the rest of the CLI args to avoid partial-move issues.
    let Cli {
        json,
        command,
        bootstrap,
        devnet_manifest,
        allow_loopback,
        timeout_secs,
        log_level: _,
        evm_network,
        chunk_concurrency,
    } = cli;

    // Shared context for data commands that need EVM / bootstrap info.
    let data_ctx = DataCliContext {
        bootstrap,
        devnet_manifest,
        allow_loopback,
        timeout_secs,
        evm_network,
        chunk_concurrency,
    };

    match command {
        Commands::Node { command } => {
            // Delegate to existing node management commands
            match command {
                commands::node::NodeCommand::Add(args) => {
                    args.execute(json).await?;
                }
                commands::node::NodeCommand::Daemon { command } => {
                    command.execute(json).await?;
                }
                commands::node::NodeCommand::Reset(args) => {
                    args.execute(json).await?;
                }
                commands::node::NodeCommand::Start(args) => {
                    args.execute(json).await?;
                }
                commands::node::NodeCommand::Status(args) => {
                    args.execute(json).await?;
                }
                commands::node::NodeCommand::Stop(args) => {
                    args.execute(json).await?;
                }
            }
        }
        Commands::Wallet { action } => {
            // Wallet commands don't need network connection
            let private_key = require_secret_key()?;
            let (network, _) = resolve_evm_network_and_manifest(&data_ctx)?;
            let wallet = create_wallet(&private_key, network)?;
            action.execute(wallet).await?;
        }
        Commands::File { action } => {
            let needs_wallet = matches!(action, commands::data::FileAction::Upload { .. });
            let client = build_data_client(&data_ctx, needs_wallet).await?;
            action.execute(&client).await?;
        }
        Commands::Chunk { action } => {
            let needs_wallet = matches!(action, commands::data::ChunkAction::Put { .. });
            let client = build_data_client(&data_ctx, needs_wallet).await?;
            action.execute(&client).await?;
        }
    }

    Ok(())
}

/// Shared context for data commands extracted from CLI args.
struct DataCliContext {
    bootstrap: Vec<SocketAddr>,
    devnet_manifest: Option<PathBuf>,
    allow_loopback: bool,
    timeout_secs: u64,
    evm_network: String,
    chunk_concurrency: Option<usize>,
}

/// Build a data client with wallet if SECRET_KEY is set.
async fn build_data_client(ctx: &DataCliContext, needs_wallet: bool) -> anyhow::Result<Client> {
    let private_key = std::env::var("SECRET_KEY").ok();

    if needs_wallet && private_key.is_none() {
        anyhow::bail!("SECRET_KEY environment variable required for this operation");
    }

    // Parse manifest once and share it across bootstrap + EVM resolution.
    let manifest = load_manifest(ctx)?;
    let bootstrap = resolve_bootstrap_from(ctx, manifest.as_ref())?;
    let node = create_client_node(bootstrap, ctx.allow_loopback).await?;

    let mut config = ClientConfig {
        timeout_secs: ctx.timeout_secs,
        ..Default::default()
    };
    if let Some(concurrency) = ctx.chunk_concurrency {
        config.chunk_concurrency = concurrency;
    }

    let mut client = Client::from_node(node, config);

    if let Some(ref key) = private_key {
        let network = resolve_evm_network(&ctx.evm_network, manifest.as_ref())?;
        let wallet = create_wallet(key, network)?;
        info!("Wallet configured for EVM payments");
        client = client.with_wallet(wallet);
        client
            .approve_token_spend()
            .await
            .map_err(|e| anyhow::anyhow!("Token approval failed: {e}"))?;
    }

    Ok(client)
}

fn require_secret_key() -> anyhow::Result<String> {
    std::env::var("SECRET_KEY")
        .map_err(|_| anyhow::anyhow!("SECRET_KEY environment variable required"))
}

fn create_wallet(private_key: &str, network: EvmNetwork) -> anyhow::Result<Wallet> {
    Wallet::new_from_private_key(network, private_key)
        .map_err(|e| anyhow::anyhow!("Failed to create wallet: {e}"))
}

/// Load and parse the devnet manifest once (if configured).
fn load_manifest(ctx: &DataCliContext) -> anyhow::Result<Option<DevnetManifest>> {
    if let Some(ref manifest_path) = ctx.devnet_manifest {
        let data = std::fs::read_to_string(manifest_path)?;
        Ok(Some(serde_json::from_str(&data)?))
    } else {
        Ok(None)
    }
}

fn resolve_evm_network_and_manifest(
    ctx: &DataCliContext,
) -> anyhow::Result<(EvmNetwork, Option<DevnetManifest>)> {
    let manifest = load_manifest(ctx)?;
    let network = resolve_evm_network(&ctx.evm_network, manifest.as_ref())?;
    Ok((network, manifest))
}

fn resolve_evm_network(
    evm_network: &str,
    manifest: Option<&DevnetManifest>,
) -> anyhow::Result<EvmNetwork> {
    match evm_network {
        "arbitrum-one" => Ok(EvmNetwork::ArbitrumOne),
        "arbitrum-sepolia" => Ok(EvmNetwork::ArbitrumSepoliaTest),
        "local" => {
            if let Some(m) = manifest {
                if let Some(ref evm) = m.evm {
                    let rpc_url: reqwest::Url = evm
                        .rpc_url
                        .parse()
                        .map_err(|e| anyhow::anyhow!("Invalid RPC URL: {e}"))?;
                    let token_addr: EvmAddress = evm
                        .payment_token_address
                        .parse()
                        .map_err(|e| anyhow::anyhow!("Invalid token address: {e}"))?;
                    let payments_addr: EvmAddress = evm
                        .data_payments_address
                        .parse()
                        .map_err(|e| anyhow::anyhow!("Invalid payments address: {e}"))?;
                    let merkle_addr: Option<EvmAddress> = evm
                        .merkle_payments_address
                        .as_ref()
                        .map(|s| {
                            s.parse().map_err(|e| {
                                anyhow::anyhow!("Invalid merkle payments address: {e}")
                            })
                        })
                        .transpose()?;
                    return Ok(EvmNetwork::Custom(CustomNetwork {
                        rpc_url_http: rpc_url,
                        payment_token_address: token_addr,
                        data_payments_address: payments_addr,
                        merkle_payments_address: merkle_addr,
                    }));
                }
            }
            anyhow::bail!("EVM network 'local' requires --devnet-manifest with EVM info")
        }
        other => {
            anyhow::bail!(
                "Unsupported EVM network: {other}. Use 'arbitrum-one', 'arbitrum-sepolia', or 'local'."
            )
        }
    }
}

/// Resolve bootstrap peers from a pre-loaded manifest.
fn resolve_bootstrap_from(
    ctx: &DataCliContext,
    manifest: Option<&DevnetManifest>,
) -> anyhow::Result<Vec<SocketAddr>> {
    if !ctx.bootstrap.is_empty() {
        return Ok(ctx.bootstrap.clone());
    }

    if let Some(m) = manifest {
        let bootstrap: Vec<SocketAddr> = m
            .bootstrap
            .iter()
            .filter_map(MultiAddr::socket_addr)
            .collect();
        return Ok(bootstrap);
    }

    anyhow::bail!("No bootstrap peers provided. Use --bootstrap or --devnet-manifest.")
}

async fn create_client_node(
    bootstrap: Vec<SocketAddr>,
    allow_loopback: bool,
) -> anyhow::Result<Arc<P2PNode>> {
    let mut core_config = CoreNodeConfig::builder()
        .port(0)
        .ipv6(false)
        .local(allow_loopback)
        .mode(NodeMode::Client)
        .max_message_size(MAX_WIRE_MESSAGE_SIZE)
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to create core config: {e}"))?;

    core_config.bootstrap_peers = bootstrap
        .iter()
        .map(|addr| MultiAddr::quic(*addr))
        .collect();

    let node = P2PNode::new(core_config)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create P2P node: {e}"))?;
    node.start()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to start P2P node: {e}"))?;

    Ok(Arc::new(node))
}
