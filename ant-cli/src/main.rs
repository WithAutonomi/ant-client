mod cli;
mod commands;
mod progress;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tracing::info;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use ant_core::data::{
    Client, ClientConfig, CoreNodeConfig, CustomNetwork, DevnetManifest, EvmAddress, EvmNetwork,
    IPDiversityConfig, MultiAddr, NodeMode, P2PNode, Wallet, MAX_WIRE_MESSAGE_SIZE,
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
            eprintln!("Error: {e:#}");
            1
        }
    };

    // Flush stdout before force-exit to ensure all output (especially JSON) is written.
    let _ = std::io::Write::flush(&mut std::io::stdout());

    // Force-exit to avoid hanging on tokio runtime shutdown.
    // Open QUIC connections and pending background tasks (DHT, keep-alive)
    // block the runtime's graceful shutdown indefinitely. All data has been
    // persisted / printed by this point, so there is nothing left to clean up.
    std::process::exit(code);
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Privacy by design: no logs unless the user explicitly opts in with -v
    // or by setting RUST_LOG. A decentralized network client must not emit
    // metadata by default.
    let needs_tracing = !matches!(cli.command, Commands::Node { .. });
    if needs_tracing {
        let filter = match (EnvFilter::try_from_default_env().ok(), cli.verbose) {
            (Some(f), _) => Some(f),
            (None, 0) => None,
            (None, 1) => Some(EnvFilter::new(verbose_filter("info"))),
            (None, 2) => Some(EnvFilter::new(verbose_filter("debug"))),
            (None, _) => Some(EnvFilter::new("trace")),
        };
        if let Some(filter) = filter {
            tracing_subscriber::registry()
                .with(fmt::layer().with_writer(progress::ProgressAwareWriter))
                .with(filter)
                .init();
        }
    }

    // Separate the command from the rest of the CLI args to avoid partial-move issues.
    // `verbose` was consumed by the tracing filter init above.
    let Cli {
        json,
        command,
        bootstrap,
        devnet_manifest,
        allow_loopback,
        ipv4_only,
        quote_timeout_secs,
        store_timeout_secs,
        verbose: _,
        evm_network,
        quote_concurrency,
        store_concurrency,
    } = cli;

    // Shared context for data commands that need EVM / bootstrap info.
    let data_ctx = DataCliContext {
        bootstrap,
        devnet_manifest,
        allow_loopback,
        ipv4_only,
        quote_timeout_secs,
        store_timeout_secs,
        evm_network,
        quote_concurrency,
        store_concurrency,
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
            // Extract per-upload overrides BEFORE building the client
            // so the adaptive controller picks them up at construction.
            let (store_timeout_override, store_concurrency_override) = action.upload_overrides();
            let client = build_data_client(
                &data_ctx,
                needs_wallet,
                json,
                store_timeout_override,
                store_concurrency_override,
            )
            .await?;
            let result = action.execute(&client, json).await;
            // Persist whatever the controller learned this run, even
            // on error — partial signal is still better than cold next
            // time. Drop will also fire as a backstop.
            client.save_adaptive_snapshot();
            result?;
        }
        Commands::Chunk { action } => {
            let needs_wallet = matches!(action, commands::data::ChunkAction::Put { .. });
            let client = build_data_client(&data_ctx, needs_wallet, json, None, None).await?;
            let result = action.execute(&client).await;
            client.save_adaptive_snapshot();
            result?;
        }
        Commands::Update(args) => {
            args.execute(json).await?;
        }
    }

    Ok(())
}

/// Shared context for data commands extracted from CLI args.
struct DataCliContext {
    bootstrap: Vec<SocketAddr>,
    devnet_manifest: Option<PathBuf>,
    allow_loopback: bool,
    ipv4_only: bool,
    quote_timeout_secs: u64,
    store_timeout_secs: u64,
    evm_network: String,
    quote_concurrency: Option<usize>,
    store_concurrency: Option<usize>,
}

/// Build a data client with wallet if SECRET_KEY is set.
///
/// Per-action overrides (`store_timeout_override`,
/// `store_concurrency_override`) are applied BEFORE constructing the
/// adaptive controller, so the controller actually honors them as
/// caps. Mutating `client.config_mut()` after construction would be a
/// no-op for the controller (the controller is built once at
/// `Client::from_node`).
async fn build_data_client(
    ctx: &DataCliContext,
    needs_wallet: bool,
    quiet: bool,
    store_timeout_override: Option<u64>,
    store_concurrency_override: Option<usize>,
) -> anyhow::Result<Client> {
    let private_key = std::env::var("SECRET_KEY")
        .ok()
        .map(|k| k.strip_prefix("0x").unwrap_or(&k).to_string());

    if needs_wallet && private_key.is_none() {
        anyhow::bail!("SECRET_KEY environment variable required for this operation");
    }

    let manifest = load_manifest(ctx)?;
    let bootstrap = resolve_bootstrap_from(ctx, manifest.as_ref())?;

    // Connection phase with animated spinner showing peer discovery in real-time.
    // The spinner is the user-facing UI; tracing::info! provides log-level visibility
    // when `-v` is set.
    info!("Connecting to autonomi network");
    let node = if quiet {
        create_client_node(bootstrap, ctx.allow_loopback, ctx.ipv4_only).await?
    } else {
        let spinner = progress::new_spinner("Connecting to autonomi network...");

        let node = match create_client_node_raw(bootstrap, ctx.allow_loopback, ctx.ipv4_only).await
        {
            Ok(n) => n,
            Err(e) => {
                spinner.finish_and_clear();
                return Err(e);
            }
        };

        // Poll peer count during node.start() to show real-time discovery.
        // The spinner reflects every change; `info!` only fires on each new
        // peer-count milestone to avoid flooding the log.
        let spinner_clone = spinner.clone();
        let node_clone = node.clone();
        let poll_handle = tokio::spawn(async move {
            let mut last_logged = 0usize;
            loop {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let count = node_clone.connected_peers().await.len();
                if count > 0 {
                    spinner_clone.set_message(format!(
                        "Connecting to autonomi network... (found {count} peers)"
                    ));
                }
                if count > last_logged {
                    info!("Discovered {count} peer(s)");
                    last_logged = count;
                }
            }
        });

        let start_result = node.start().await;
        poll_handle.abort();
        spinner.finish_and_clear();

        start_result.map_err(|e| anyhow::anyhow!("Failed to start P2P node: {e}"))?;

        let peers = node.connected_peers().await.len();
        info!("Connected to autonomi network ({peers} peers)");
        eprintln!("Connected to autonomi network (found {peers} peers)");
        node
    };

    let mut config = ClientConfig {
        quote_timeout_secs: ctx.quote_timeout_secs,
        store_timeout_secs: ctx.store_timeout_secs,
        ..Default::default()
    };
    // Legacy default values are treated as "not pinned" by build_controller
    // (so the default ClientConfig doesn't silently lower the new
    // adaptive ceilings). Mirror that here so the deprecation warning
    // doesn't lie when a user passes a value equal to the legacy default.
    const LEGACY_QUOTE_DEFAULT: usize = 32;
    const LEGACY_STORE_DEFAULT: usize = 8;
    if let Some(concurrency) = ctx.quote_concurrency {
        if concurrency == LEGACY_QUOTE_DEFAULT {
            eprintln!(
                "warning: --quote-concurrency={concurrency} matches the legacy \
                 default and is silently ignored by the adaptive controller. \
                 Pass a different value to actually cap the quote channel."
            );
        } else {
            eprintln!(
                "warning: --quote-concurrency is deprecated; the adaptive controller \
                 sizes quote concurrency from observed signals. Your value \
                 ({concurrency}) caps the quote channel only (store and download \
                 are unaffected) and will be removed in a future release."
            );
        }
        config.quote_concurrency = concurrency;
    }
    if let Some(concurrency) = ctx.store_concurrency {
        if concurrency == LEGACY_STORE_DEFAULT {
            eprintln!(
                "warning: --store-concurrency={concurrency} matches the legacy \
                 default and is silently ignored by the adaptive controller. \
                 Pass a different value to actually cap the store channel."
            );
        } else {
            eprintln!(
                "warning: --store-concurrency is deprecated; the adaptive controller \
                 sizes store concurrency from observed signals. Your value \
                 ({concurrency}) caps the store channel only (quote and download \
                 are unaffected) and will be removed in a future release."
            );
        }
        config.store_concurrency = concurrency;
    }
    // Per-action overrides (e.g. `ant file upload --store-concurrency`)
    // applied here, BEFORE Client::from_node so the adaptive
    // controller actually picks them up. Mutating `config_mut()` after
    // construction is a no-op for the controller's per-channel max.
    if let Some(t) = store_timeout_override {
        config.store_timeout_secs = t;
    }
    if let Some(c) = store_concurrency_override {
        eprintln!(
            "warning: --store-concurrency on upload is deprecated; the \
             adaptive controller sizes store concurrency from observed \
             signals. Your value ({c}) caps the store channel only and \
             will be removed in a future release."
        );
        config.store_concurrency = c;
    }

    let mut client = Client::from_node(node, config);

    if needs_wallet {
        let key = private_key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("SECRET_KEY environment variable required"))?;
        let network = resolve_evm_network(&ctx.evm_network, manifest.as_ref())?;
        let wallet = create_wallet(key, network)?;
        info!("Wallet configured for EVM payments");
        client = client.with_wallet(wallet);

        if !quiet {
            info!("Approving token spend");
            let spinner = progress::new_spinner("Approving token spend...");
            let approval = client.approve_token_spend().await;
            spinner.finish_and_clear();
            approval.map_err(|e| anyhow::anyhow!("Token approval failed: {e}"))?;
            info!("Token spend approved");
            eprintln!("Token spend approved");
        } else {
            client
                .approve_token_spend()
                .await
                .map_err(|e| anyhow::anyhow!("Token approval failed: {e}"))?;
        }
    }

    Ok(client)
}

/// Build the EnvFilter directives string for `-v` / `-vv`.
///
/// Targets cover ant-cli/ant-core plus the network-layer crates (ant_node, evmlib,
/// saorsa_core, saorsa_transport, saorsa_pqc) so that peer discovery / connection
/// events are visible at info/debug levels.
fn verbose_filter(level: &str) -> String {
    [
        "ant_cli",
        "ant_core",
        "ant_node",
        "evmlib",
        "saorsa_core",
        "saorsa_transport",
        "saorsa_pqc",
    ]
    .iter()
    .map(|t| format!("{t}={level}"))
    .collect::<Vec<_>>()
    .join(",")
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
                    let vault_addr: EvmAddress = evm
                        .payment_vault_address
                        .parse()
                        .map_err(|e| anyhow::anyhow!("Invalid payment vault address: {e}"))?;
                    return Ok(EvmNetwork::Custom(CustomNetwork {
                        rpc_url_http: rpc_url,
                        payment_token_address: token_addr,
                        payment_vault_address: vault_addr,
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
///
/// Priority: CLI `--bootstrap` > devnet manifest > `bootstrap_peers.toml` config file.
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

    if let Some(peers) = ant_core::config::load_bootstrap_peers()
        .map_err(|e| anyhow::anyhow!("Failed to load bootstrap config: {e}"))?
    {
        info!("Loaded {} bootstrap peer(s) from config file", peers.len());
        return Ok(peers);
    }

    anyhow::bail!(
        "No bootstrap peers provided. Use --bootstrap, --devnet-manifest, \
         or install bootstrap_peers.toml to your config directory."
    )
}

async fn create_client_node(
    bootstrap: Vec<SocketAddr>,
    allow_loopback: bool,
    ipv4_only: bool,
) -> anyhow::Result<Arc<P2PNode>> {
    let node = create_client_node_raw(bootstrap, allow_loopback, ipv4_only).await?;
    node.start()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to start P2P node: {e}"))?;
    Ok(node)
}

/// Create a P2P node without starting it (for spinner polling during start).
async fn create_client_node_raw(
    bootstrap: Vec<SocketAddr>,
    allow_loopback: bool,
    ipv4_only: bool,
) -> anyhow::Result<Arc<P2PNode>> {
    let mut core_config = CoreNodeConfig::builder()
        .port(0)
        .ipv6(!ipv4_only)
        .local(allow_loopback)
        .mode(NodeMode::Client)
        .max_message_size(MAX_WIRE_MESSAGE_SIZE)
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to create core config: {e}"))?;

    // Clients never enforce IP-diversity limits: they don't host data and
    // their routing table exists only to find peers, not to be defended
    // against Sybil clustering. Strict per-IP / per-subnet caps would
    // silently drop legitimate testnet peers that share an IP or /24.
    core_config.diversity_config = Some(IPDiversityConfig::permissive());

    core_config.bootstrap_peers = bootstrap
        .iter()
        .map(|addr| MultiAddr::quic(*addr))
        .collect();

    let node = P2PNode::new(core_config)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create P2P node: {e}"))?;

    Ok(Arc::new(node))
}
