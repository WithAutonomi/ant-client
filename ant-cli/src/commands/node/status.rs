use clap::Args;
use colored::Colorize;

use ant_core::node::daemon::client;
use ant_core::node::types::{DaemonConfig, NodeStatus};

#[derive(Args)]
pub struct StatusArgs {}

impl StatusArgs {
    pub async fn execute(self, json_output: bool) -> anyhow::Result<()> {
        let config = DaemonConfig::default();

        let daemon_status = client::status(&config).await?;
        let result = if daemon_status.running {
            client::node_status(&config).await?
        } else {
            ant_core::node::node_status_offline(&config.registry_path)?
        };

        if json_output {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            if result.nodes.is_empty() {
                println!(
                    "{} No nodes registered. Add nodes first with: {}",
                    "●".yellow(),
                    "ant node add".cyan()
                );
                return Ok(());
            }

            // Table header
            println!(
                "  {:<4} {:<14} {:<10} {}",
                "ID".dimmed(),
                "Name".dimmed(),
                "Version".dimmed(),
                "Status".dimmed()
            );
            println!("  {}", "─".repeat(44).dimmed());

            for node in &result.nodes {
                let status_display = match node.status {
                    NodeStatus::Running => format!("{} {}", "●".green(), "Running".green()),
                    NodeStatus::Stopped => format!("{} {}", "●".dimmed(), "Stopped".dimmed()),
                    NodeStatus::Starting => format!("{} {}", "●".yellow(), "Starting".yellow()),
                    NodeStatus::Stopping => format!("{} {}", "●".yellow(), "Stopping".yellow()),
                    NodeStatus::Errored => format!("{} {}", "●".red(), "Errored".red()),
                    NodeStatus::UpgradeScheduled => {
                        format!("{} {}", "●".cyan(), "Upgrade scheduled".cyan())
                    }
                };
                let version_display = match &node.pending_version {
                    Some(pending) => format!("{} → {}", node.version, pending),
                    None => node.version.clone(),
                };
                println!(
                    "  {:<4} {:<14} {:<18} {}",
                    node.node_id.to_string().bold(),
                    node.name,
                    version_display.dimmed(),
                    status_display
                );
            }

            if !daemon_status.running {
                println!();
                println!(
                    "  {} Daemon is not running — all nodes shown as stopped.",
                    "!".yellow().bold()
                );
                println!("  Start it with: {}", "ant node daemon start".cyan());
            }
        }

        Ok(())
    }
}
