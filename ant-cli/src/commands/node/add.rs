use std::path::PathBuf;

use clap::Args;
use colored::Colorize;

use ant_core::node::binary::ProgressReporter;
use ant_core::node::daemon::client;
use ant_core::node::types::DaemonConfig;
use ant_core::node::types::{AddNodeOpts, AddNodeResult, BinarySource, PortRange};

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

    /// Metrics port or range (e.g., 13000 or 13000-13004)
    #[arg(long)]
    pub metrics_port: Option<String>,

    /// Custom data directory prefix
    #[arg(long)]
    pub data_dir_path: Option<PathBuf>,

    /// Custom log directory prefix
    #[arg(long)]
    pub log_dir_path: Option<PathBuf>,

    /// Network ID (default: 1 for mainnet)
    #[arg(long, default_value = "1")]
    pub network_id: u32,

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

    /// Environment variables for the node (KEY=VALUE format)
    #[arg(long, value_delimiter = ',')]
    pub env: Vec<String>,
}

impl AddArgs {
    pub async fn execute(self, json_output: bool) -> anyhow::Result<()> {
        let opts = self.to_add_node_opts()?;

        // Check if daemon is running; if so, POST to API; otherwise call directly
        let config = DaemonConfig::default();
        let result = match client::status(&config).await {
            Ok(status) if status.running => self.add_via_daemon(&config, &opts).await?,
            _ => self.add_directly(&config, &opts).await?,
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
                if let Some(port) = node.metrics_port {
                    println!("    {} {}", "Metrics".dimmed(), port.to_string().cyan());
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
        let node_port = self.parse_port_range(&self.node_port)?;
        let metrics_port = self.parse_port_range(&self.metrics_port)?;

        let binary_source = if let Some(ref path) = self.path {
            BinarySource::LocalPath(path.clone())
        } else if let Some(ref version) = self.version {
            BinarySource::Version(version.clone())
        } else if let Some(ref url) = self.url {
            BinarySource::Url(url.clone())
        } else {
            BinarySource::Latest
        };

        let env_variables: Vec<(String, String)> = self
            .env
            .iter()
            .map(|e| {
                let parts: Vec<&str> = e.splitn(2, '=').collect();
                if parts.len() == 2 {
                    Ok((parts[0].to_string(), parts[1].to_string()))
                } else {
                    anyhow::bail!("Invalid env variable format: '{e}'. Expected KEY=VALUE")
                }
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        Ok(AddNodeOpts {
            count: self.count,
            rewards_address: self.rewards_address.clone(),
            node_port,
            metrics_port,
            data_dir_path: self.data_dir_path.clone(),
            log_dir_path: self.log_dir_path.clone(),
            network_id: self.network_id,
            binary_source,
            bootstrap_peers: self.bootstrap.clone(),
            env_variables,
        })
    }

    fn parse_port_range(&self, input: &Option<String>) -> anyhow::Result<Option<PortRange>> {
        match input {
            None => Ok(None),
            Some(s) => {
                if let Some((start, end)) = s.split_once('-') {
                    let start: u16 = start
                        .parse()
                        .map_err(|_| anyhow::anyhow!("Invalid port range start: '{start}'"))?;
                    let end: u16 = end
                        .parse()
                        .map_err(|_| anyhow::anyhow!("Invalid port range end: '{end}'"))?;
                    if end < start {
                        anyhow::bail!("Port range end ({end}) must be >= start ({start})");
                    }
                    Ok(Some(PortRange::Range(start, end)))
                } else {
                    let port: u16 = s
                        .parse()
                        .map_err(|_| anyhow::anyhow!("Invalid port: '{s}'"))?;
                    Ok(Some(PortRange::Single(port)))
                }
            }
        }
    }

    async fn add_via_daemon(
        &self,
        config: &DaemonConfig,
        opts: &AddNodeOpts,
    ) -> anyhow::Result<AddNodeResult> {
        let info = client::info(config);
        let api_base = info
            .api_base
            .ok_or_else(|| anyhow::anyhow!("Daemon is running but API base URL not available"))?;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{api_base}/nodes"))
            .json(opts)
            .send()
            .await?;

        if resp.status().is_success() {
            Ok(resp.json().await?)
        } else {
            let body = resp.text().await?;
            anyhow::bail!("Daemon returned error: {body}");
        }
    }

    async fn add_directly(
        &self,
        config: &DaemonConfig,
        opts: &AddNodeOpts,
    ) -> anyhow::Result<AddNodeResult> {
        let progress = CliProgress;
        let result =
            ant_core::node::add_nodes(opts.clone(), &config.registry_path, &progress).await?;
        Ok(result)
    }
}

/// CLI progress reporter that prints to the terminal.
struct CliProgress;

impl ProgressReporter for CliProgress {
    fn report_started(&self, message: &str) {
        println!("{} {message}", "⟳".cyan());
    }

    fn report_progress(&self, bytes: u64, total: u64) {
        if total > 0 {
            let pct = (bytes as f64 / total as f64 * 100.0) as u32;
            let bar_width = 30;
            let filled = (pct as usize * bar_width) / 100;
            let empty = bar_width - filled;
            let bar = format!(
                "{}{}",
                "█".repeat(filled).cyan(),
                "░".repeat(empty).dimmed()
            );
            print!("\r  {} {bar} {pct:>3}%", "Downloading".dimmed());
        }
    }

    fn report_complete(&self, message: &str) {
        println!("\r{} {message}", "✓".green().bold());
    }
}
