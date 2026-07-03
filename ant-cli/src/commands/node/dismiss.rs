use clap::Args;
use colored::Colorize;

use ant_core::node::daemon::client;
use ant_core::node::types::DaemonConfig;

#[derive(Args)]
pub struct DismissArgs {
    /// The ID of the evicted node to dismiss (remove it from the registry/list).
    pub node_id: u32,
}

impl DismissArgs {
    pub async fn execute(self, json_output: bool) -> anyhow::Result<()> {
        let config = DaemonConfig::default();

        // Dual-path: go through the daemon when it's running so its in-memory registry stays in
        // sync; otherwise operate directly on the registry file.
        let status = client::status(&config).await?;
        let result = if status.running {
            client::dismiss_node(&config, self.node_id).await?
        } else {
            ant_core::node::remove_node(self.node_id, &config.registry_path)?
        };

        if json_output {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            println!(
                "{} Dismissed node {} ({})",
                "✓".green().bold(),
                result.removed.id.to_string().bold(),
                result.removed.service_name.dimmed()
            );
        }

        Ok(())
    }
}
