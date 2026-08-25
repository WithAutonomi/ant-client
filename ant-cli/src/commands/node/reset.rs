use std::io::{self, Write};

use clap::Args;
use colored::Colorize;

use ant_core::node::daemon::client;
use ant_core::node::types::{DaemonConfig, ResetResult};

#[derive(Args)]
pub struct ResetArgs {
    /// Skip confirmation prompt
    #[arg(long)]
    pub force: bool,
}

impl ResetArgs {
    pub async fn execute(self, json_output: bool) -> anyhow::Result<()> {
        let config = DaemonConfig::default();

        // Check if daemon is running and if any nodes are running
        let daemon_running = match client::status(&config).await {
            Ok(status) if status.running => {
                if status.nodes_running > 0 {
                    anyhow::bail!(
                        "Cannot reset while nodes are running ({} node(s) still running). \
                         Stop all nodes first.",
                        status.nodes_running
                    );
                }
                true
            }
            _ => false,
        };

        // Prompt for confirmation unless --force
        if !self.force && !json_output {
            print!(
                "{} This will remove all node data, logs, and clear the registry.\n  Are you sure? [y/N] ",
                "⚠".yellow().bold()
            );
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            if !input.trim().eq_ignore_ascii_case("y") {
                println!("{} Reset cancelled.", "─".dimmed());
                return Ok(());
            }
        }

        let result = if daemon_running {
            client::reset(&config).await?
        } else {
            self.reset_directly(&config)?
        };

        if json_output {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else if result.nodes_cleared == 0 {
            println!(
                "{} No nodes to reset — registry is already empty.",
                "●".yellow()
            );
        } else {
            println!(
                "{} Reset complete — {} node(s) cleared",
                "✓".green().bold(),
                result.nodes_cleared.to_string().bold()
            );
            for dir in &result.data_dirs_removed {
                println!(
                    "  {} Removed data: {}",
                    "─".dimmed(),
                    dir.display().to_string().dimmed()
                );
            }
            for dir in &result.log_dirs_removed {
                println!(
                    "  {} Removed logs: {}",
                    "─".dimmed(),
                    dir.display().to_string().dimmed()
                );
            }
        }

        Ok(())
    }

    fn reset_directly(&self, config: &DaemonConfig) -> anyhow::Result<ResetResult> {
        let result = ant_core::node::reset(&config.registry_path)?;
        Ok(result)
    }
}
