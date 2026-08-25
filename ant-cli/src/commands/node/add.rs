use std::path::PathBuf;

use clap::Args;
use colored::Colorize;

use ant_core::node::binary::{NoopProgress, ProgressReporter};
use ant_core::node::daemon::client;
use ant_core::node::types::DaemonConfig;
use ant_core::node::types::{
    AddNodeOpts, AddNodeResult, BinarySource, EvmNetwork, PortRange, UpgradeChannel,
};

#[derive(Args)]
pub struct AddArgs {
    /// Wallet address for node earnings (required)
    #[arg(long)]
    pub rewards_address: String,

    /// Number of nodes to add
    #[arg(long, default_value = "1")]
    pub count: u16,

    /// Port or port range for node(s) (e.g., 12000 or 12000-12004)
    #[arg(long)]
    pub node_port: Option<String>,

    /// Custom data directory prefix
    #[arg(long)]
    pub data_dir_path: Option<PathBuf>,

    /// Custom log directory prefix
    #[arg(long)]
    pub log_dir_path: Option<PathBuf>,

    /// Path to a local node binary
    #[arg(long, conflicts_with_all = &["version", "url"])]
    pub path: Option<PathBuf>,

    /// Download a specific version
    #[arg(long, conflicts_with_all = &["path", "url"])]
    pub version: Option<String>,

    /// Download binary from a URL (zip/tar.gz archive)
    #[arg(long, conflicts_with_all = &["path", "version"])]
    pub url: Option<String>,

    /// Bootstrap peer(s)
    #[arg(long, value_delimiter = ',')]
    pub bootstrap: Vec<String>,

    /// EVM network the node uses for storage payments
    #[arg(long, value_enum, default_value = "arbitrum-one")]
    pub evm_network: EvmNetworkArg,

    /// Release channel the node tracks for automatic upgrades
    #[arg(long, value_enum)]
    pub upgrade_channel: Option<UpgradeChannelArg>,

    /// Environment variables for the node (KEY=VALUE format)
    #[arg(long, value_delimiter = ',')]
    pub env: Vec<String>,
}

/// CLI value for the node's upgrade channel. Mirrors `ant-node`'s accepted values.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum UpgradeChannelArg {
    Stable,
    Beta,
}

impl From<UpgradeChannelArg> for UpgradeChannel {
    fn from(arg: UpgradeChannelArg) -> Self {
        match arg {
            UpgradeChannelArg::Stable => Self::Stable,
            UpgradeChannelArg::Beta => Self::Beta,
        }
    }
}

/// CLI value for the node's EVM network. Mirrors `ant-node`'s `--evm-network` values.
#[derive(Clone, Copy, Default, clap::ValueEnum)]
pub enum EvmNetworkArg {
    /// Arbitrum One (mainnet).
    #[default]
    ArbitrumOne,
    /// Arbitrum Sepolia testnet.
    ArbitrumSepolia,
}

impl From<EvmNetworkArg> for EvmNetwork {
    fn from(arg: EvmNetworkArg) -> Self {
        match arg {
            EvmNetworkArg::ArbitrumOne => Self::ArbitrumOne,
            EvmNetworkArg::ArbitrumSepolia => Self::ArbitrumSepolia,
        }
    }
}

impl AddArgs {
    pub async fn execute(self, json_output: bool) -> anyhow::Result<()> {
        let opts = self.to_add_node_opts()?;

        // Check if daemon is running; if so, POST to API; otherwise call directly
        let config = DaemonConfig::default();
        let result = match client::status(&config).await {
            Ok(status) if status.running => client::add_node(&config, &opts).await?,
            _ => self.add_directly(&config, &opts, json_output).await?,
        };

        if json_output {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            println!(
                "{} Added {} node(s):",
                "✓".green().bold(),
                result.nodes_added.len().to_string().bold()
            );
            println!();
            for node in &result.nodes_added {
                println!(
                    "  {} {}",
                    "●".cyan(),
                    format!("Node {} ({})", node.id, node.service_name).bold()
                );
                println!(
                    "    {} {}",
                    "Data".dimmed(),
                    node.data_dir.display().to_string().white()
                );
                if let Some(ref log_dir) = node.log_dir {
                    println!(
                        "    {} {}",
                        "Logs".dimmed(),
                        log_dir.display().to_string().white()
                    );
                }
                if let Some(port) = node.node_port {
                    println!("    {} {}", "Port".dimmed(), port.to_string().cyan());
                }
                println!(
                    "    {} {}",
                    "Binary".dimmed(),
                    node.binary_path.display().to_string().dimmed()
                );
                println!("    {} {}", "Version".dimmed(), node.version.green());
            }
        }

        Ok(())
    }

    fn to_add_node_opts(&self) -> anyhow::Result<AddNodeOpts> {
        let node_port = self
            .node_port
            .as_deref()
            .map(str::parse::<PortRange>)
            .transpose()?;

        let binary_source = if let Some(ref path) = self.path {
            BinarySource::LocalPath(path.clone())
        } else if let Some(ref version) = self.version {
            BinarySource::Version(version.clone())
        } else if let Some(ref url) = self.url {
            BinarySource::Url(url.clone())
        } else {
            BinarySource::Latest
        };

        let env_variables = AddNodeOpts::parse_env_vars(&self.env)?;

        Ok(AddNodeOpts {
            count: self.count,
            rewards_address: self.rewards_address.clone(),
            node_port,
            data_dir_path: self.data_dir_path.clone(),
            log_dir_path: self.log_dir_path.clone(),
            binary_source,
            bootstrap_peers: self.bootstrap.clone(),
            env_variables,
            upgrade_channel: self.upgrade_channel.map(Into::into),
            evm_network: self.evm_network.into(),
        })
    }

    async fn add_directly(
        &self,
        config: &DaemonConfig,
        opts: &AddNodeOpts,
        json_output: bool,
    ) -> anyhow::Result<AddNodeResult> {
        // Suppress progress in JSON mode so stdout stays parseable.
        let progress: Box<dyn ProgressReporter> = if json_output {
            Box::new(NoopProgress)
        } else {
            Box::new(crate::progress::CliProgress)
        };
        let result =
            ant_core::node::add_nodes(opts.clone(), &config.registry_path, progress.as_ref())
                .await?;
        Ok(result)
    }
}
