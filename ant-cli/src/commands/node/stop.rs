use clap::Args;
use colored::Colorize;

use ant_core::node::daemon::client;
use ant_core::node::types::DaemonConfig;

#[derive(Args)]
pub struct StopArgs {
    /// Stop a specific node by service name (e.g., node1). If omitted, stops all nodes.
    #[arg(long)]
    pub service_name: Option<String>,
}

impl StopArgs {
    pub async fn execute(self, json_output: bool) -> anyhow::Result<()> {
        let config = DaemonConfig::default();

        // Verify daemon is running
        let status = client::status(&config).await?;
        if !status.running {
            anyhow::bail!("The daemon is not running. Start it first with: ant node daemon start");
        }

        match self.service_name {
            Some(ref name) => self.stop_single(&config, name, json_output).await,
            None => self.stop_all(&config, json_output).await,
        }
    }

    async fn stop_single(
        &self,
        config: &DaemonConfig,
        service_name: &str,
        json_output: bool,
    ) -> anyhow::Result<()> {
        // Resolve the node ID through the daemon API — the daemon owns the
        // registry, so the CLI must not read node_registry.json directly.
        let node_id = client::resolve_node_id_by_name(config, service_name).await?;

        let result = client::stop_node(config, node_id).await?;

        if json_output {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            println!(
                "{} Node {} ({}) stopped",
                "✓".green().bold(),
                result.service_name.bold(),
                result.node_id.to_string().dimmed()
            );
        }

        Ok(())
    }

    async fn stop_all(&self, config: &DaemonConfig, json_output: bool) -> anyhow::Result<()> {
        let result = client::stop_all_nodes(config).await?;

        if json_output {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            if !result.stopped.is_empty() {
                println!(
                    "{} Stopped {} node(s):",
                    "✓".green().bold(),
                    result.stopped.len().to_string().bold()
                );
                for node in &result.stopped {
                    println!(
                        "  {} {} ({})",
                        "●".dimmed(),
                        node.service_name.bold(),
                        node.node_id.to_string().dimmed()
                    );
                }
            }
            if !result.already_stopped.is_empty() {
                println!(
                    "{} Already stopped: {} node(s)",
                    "●".yellow(),
                    result.already_stopped.len().to_string().bold()
                );
                for id in &result.already_stopped {
                    println!("  {} Node {}", "─".dimmed(), id.to_string().dimmed());
                }
            }
            if !result.failed.is_empty() {
                println!(
                    "{} Failed to stop {} node(s):",
                    "✗".red().bold(),
                    result.failed.len().to_string().bold()
                );
                for fail in &result.failed {
                    println!(
                        "  {} {} ({}) — {}",
                        "●".red(),
                        fail.service_name.bold(),
                        fail.node_id.to_string().dimmed(),
                        fail.error.red()
                    );
                }
            }
            if result.stopped.is_empty()
                && result.already_stopped.is_empty()
                && result.failed.is_empty()
            {
                println!(
                    "{} No nodes registered. Add nodes first with: {}",
                    "●".yellow(),
                    "ant node add".cyan()
                );
            }
        }

        Ok(())
    }
}
