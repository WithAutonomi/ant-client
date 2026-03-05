use std::io::{self, Write};

use clap::Args;

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
                "This will remove all node data directories, log directories, and clear the \
                 registry.\nAre you sure? [y/N] "
            );
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            if !input.trim().eq_ignore_ascii_case("y") {
                println!("Reset cancelled.");
                return Ok(());
            }
        }

        let result = if daemon_running {
            self.reset_via_daemon(&config).await?
        } else {
            self.reset_directly(&config)?
        };

        if json_output {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else if result.nodes_cleared == 0 {
            println!("No nodes to reset. Registry is already empty.");
        } else {
            println!("Reset complete:");
            println!("  Nodes cleared: {}", result.nodes_cleared);
            for dir in &result.data_dirs_removed {
                println!("  Removed data dir: {}", dir.display());
            }
            for dir in &result.log_dirs_removed {
                println!("  Removed log dir:  {}", dir.display());
            }
        }

        Ok(())
    }

    async fn reset_via_daemon(&self, config: &DaemonConfig) -> anyhow::Result<ResetResult> {
        let info = client::info(config);
        let api_base = info
            .api_base
            .ok_or_else(|| anyhow::anyhow!("Daemon is running but API base URL not available"))?;

        let client = reqwest::Client::new();
        let resp = client.post(format!("{api_base}/reset")).send().await?;

        if resp.status().is_success() {
            Ok(resp.json().await?)
        } else {
            let body = resp.text().await?;
            anyhow::bail!("Daemon returned error: {body}");
        }
    }

    fn reset_directly(&self, config: &DaemonConfig) -> anyhow::Result<ResetResult> {
        let result = ant_core::node::reset(&config.registry_path)?;
        Ok(result)
    }
}
