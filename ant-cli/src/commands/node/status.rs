use clap::Args;

use ant_core::node::daemon::client;
use ant_core::node::types::DaemonConfig;

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
                println!("No nodes registered. Add nodes first with: ant node add");
                return Ok(());
            }

            println!("{:<4} {:<12} {:<10} Status", "ID", "Name", "Version");
            for node in &result.nodes {
                let status_str = serde_json::to_value(node.status)?;
                println!(
                    "{:<4} {:<12} {:<10} {}",
                    node.node_id,
                    node.name,
                    node.version,
                    status_str.as_str().unwrap_or("unknown"),
                );
            }

            if !daemon_status.running {
                println!();
                println!("Note: daemon is not running. All nodes shown as stopped.");
                println!("Start the daemon with: ant node daemon start");
            }
        }

        Ok(())
    }
}
