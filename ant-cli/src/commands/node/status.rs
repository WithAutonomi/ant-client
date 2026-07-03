use clap::Args;
use colored::Colorize;

use ant_core::node::daemon::client;
use ant_core::node::daemon::health::{FleetHealth, HealthLevel};
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

        // Fleet health is only meaningful while the daemon is running (it owns the disk monitor).
        let health = if daemon_status.running {
            client::fleet_health(&config).await.ok()
        } else {
            None
        };

        if json_output {
            // Fold health into the JSON payload alongside node status.
            let payload = serde_json::json!({
                "nodes": result.nodes,
                "total_running": result.total_running,
                "total_stopped": result.total_stopped,
                "health": health,
            });
            println!("{}", serde_json::to_string_pretty(&payload)?);
            return Ok(());
        } else {
            if let Some(health) = &health {
                print_fleet_health(health);
            }
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
                "  {:<4} {:<14} {:<18} {}",
                "ID".dimmed(),
                "Name".dimmed(),
                "Version".dimmed(),
                "Status".dimmed()
            );
            println!("  {}", "─".repeat(52).dimmed());

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
                    NodeStatus::Evicted => format!("{} {}", "●".magenta(), "Evicted".magenta()),
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
                // Supplementary text explaining an eviction, plus how to clear it.
                if let Some(eviction) = &node.eviction {
                    println!("       {}", eviction.reason.dimmed());
                    println!(
                        "       {} {}",
                        "dismiss with:".dimmed(),
                        format!("ant node dismiss {}", node.node_id).cyan()
                    );
                }
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

/// Print the fleet health summary: an overall line, plus a detail line per non-green check.
fn print_fleet_health(health: &FleetHealth) {
    let (dot, label) = match health.overall {
        HealthLevel::Green => ("●".green(), "Healthy".green()),
        HealthLevel::Warning => ("●".yellow(), "Warning".yellow()),
        HealthLevel::Critical => ("●".red(), "Critical".red()),
    };
    println!("  {} Fleet health: {}", dot, label);

    // Surface the reason(s) whenever the fleet is not fully healthy.
    for check in health
        .checks
        .iter()
        .filter(|c| c.level != HealthLevel::Green)
    {
        println!("    {} {}", "→".dimmed(), check.summary.dimmed());
    }
    println!();
}
